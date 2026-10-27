use sha2::{Digest, Sha256};
use snatchrs::mirror::MirrorLayout;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tempfile::tempdir;
use url::Url;

const FILENAMES: [&str; 2] = [
    "example_pkg-1.0+local-py3-none-any.whl",
    "example_pkg-1.0+local.tar.gz",
];
const BODY: &[u8] = b"local artifact bytes";

struct Server {
    base: String,
    unavailable: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Server {
    fn start() -> Self {
        Self::with_files(&["1.0+local"], &FILENAMES)
    }
    fn with_files(versions: &[&str], filenames: &[&str]) -> Self {
        let versions: Vec<String> = versions.iter().map(|name| (*name).into()).collect();
        let filenames: Vec<String> = filenames.iter().map(|name| (*name).into()).collect();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let unavailable = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let missing = unavailable.clone();
        let hits = requests.clone();
        let stopping = stop.clone();
        let origin = base.clone();
        let thread = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                // Accepted sockets inherit O_NONBLOCK from the listener on macOS/BSD.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut buffer = [0; 1024];
                    let size = stream.read(&mut buffer).unwrap();
                    assert!(size > 0);
                    request.extend_from_slice(&buffer[..size]);
                }
                hits.fetch_add(1, Ordering::AcqRel);
                let request = String::from_utf8(request).unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                let (status, body) = if path == "/simple/example-pkg/" {
                    if missing.load(Ordering::Acquire) {
                        (404, b"deleted".to_vec())
                    } else {
                        (200, serde_json::json!({"name":"Example_Pkg", "meta":{"api-version":"1.4"}, "versions":versions,
                            "files": filenames.iter().enumerate().map(|(i, filename)| serde_json::json!({
                                "filename": filename, "url":format!("{origin}/artifact/{i}"), "size":BODY.len(),
                                "hashes":{"sha256":format!("{:x}", Sha256::digest(BODY))}, "requires-python": ">=3.8"
                            })).collect::<Vec<_>>()}).to_string().into_bytes())
                    }
                } else {
                    assert!(path.starts_with("/artifact/"), "{path}");
                    (200, BODY.to_vec())
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Self {
            base,
            unavailable,
            requests,
            stop,
            thread: Some(thread),
        }
    }
    fn run(&self, root: &Path, mode: &str) -> Output {
        self.run_with(root, mode, &[])
    }
    fn run_with(&self, root: &Path, mode: &str, extra: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
            .args([mode])
            .args(if mode == "mirror" {
                &["--min-free", "0"][..]
            } else {
                &[]
            })
            .arg(root)
            .args([
                "--package",
                "Example_Pkg",
                "--metadata-url",
                &format!("{}/simple/", self.base),
                "--json",
                "--quiet",
            ])
            .args(extra)
            .env("NO_PROXY", "*")
            .env("no_proxy", "*")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn hrefs(html: &str) -> Vec<&str> {
    html.split("href=\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap())
        .collect()
}

#[test]
fn mirrored_links_resolve_locally_and_match_their_hashes_then_regenerate_and_remove() {
    let server = Server::start();
    let root = tempdir().unwrap();
    server.run(root.path(), "check");
    assert!(
        !root.path().join("simple").exists(),
        "check must not write pages"
    );
    let output = server.run(root.path(), "mirror");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["simple_index"],
        serde_json::json!({"projects":1,"files_linked":2,"files_skipped":0,"projects_removed":0,"page_errors":0,"errors":[],"stale_temp_files_removed":0})
    );
    let root_page = fs::read_to_string(root.path().join("simple/index.html")).unwrap();
    let project_page =
        fs::read_to_string(root.path().join("simple/example-pkg/index.html")).unwrap();
    assert_eq!(hrefs(&root_page), vec!["example-pkg/"]);
    let root_url = Url::from_directory_path(root.path().join("simple")).unwrap();
    for href in hrefs(&root_page) {
        assert!(
            root_url
                .join(href)
                .unwrap()
                .to_file_path()
                .unwrap()
                .join("index.html")
                .is_file()
        );
    }
    let base = root_url.join("example-pkg/").unwrap();
    let links = hrefs(&project_page);
    assert_eq!(links.len(), 2);
    assert!(links[0].contains("%2Blocal"));
    for href in links {
        assert!(href.starts_with("../../packages/example-pkg/"));
        let url = base.join(href).unwrap();
        let path = url.to_file_path().unwrap();
        assert!(path.starts_with(root.path()));
        assert!(path.is_file());
        assert_eq!(
            url.fragment().unwrap(),
            format!("sha256={:x}", Sha256::digest(fs::read(path).unwrap()))
        );
    }
    // Standalone regeneration needs no network and filters manual cleanup.
    fs::remove_file(root.path().join("packages/example-pkg").join(FILENAMES[0])).unwrap();
    fs::write(
        root.path().join("packages/example-pkg").join(FILENAMES[1]),
        b"wrong",
    )
    .unwrap();
    let before = server.requests.load(Ordering::Acquire);
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .args(["simple"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = String::from_utf8(output.stdout).unwrap();
    assert!(summary.contains("1 projects written"));
    assert!(summary.contains("0 files linked"));
    assert!(summary.contains("2 files skipped"));
    assert_eq!(server.requests.load(Ordering::Acquire), before);
    assert!(
        hrefs(&fs::read_to_string(root.path().join("simple/example-pkg/index.html")).unwrap())
            .is_empty()
    );
    server.run(root.path(), "mirror");
    let before = fs::read(root.path().join("simple/example-pkg/index.html")).unwrap();
    server.unavailable.store(true, Ordering::Release);
    server.run(root.path(), "check");
    assert_eq!(
        fs::read(root.path().join("simple/example-pkg/index.html")).unwrap(),
        before
    );
    let output = server.run(root.path(), "mirror");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["inspection"]["unavailable_count"], 1);
    assert_eq!(report["simple_index"]["projects"], 0);
    assert!(!root.path().join("simple/example-pkg").exists());
    assert!(hrefs(&fs::read_to_string(root.path().join("simple/index.html")).unwrap()).is_empty());
    for filename in FILENAMES {
        assert_eq!(
            fs::read(root.path().join("packages/example-pkg").join(filename)).unwrap(),
            BODY
        );
    }
}

#[test]
fn simple_takes_the_mirror_lock_and_can_generate_an_empty_index() {
    let root = tempdir().unwrap();
    let layout = MirrorLayout::create(root.path()).unwrap();
    let lock = layout.lock().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .args(["simple"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Another snatchrs command"));
    assert!(!root.path().join("simple").exists());
    drop(lock);
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .args(["simple"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("0 projects written"));
    assert!(root.path().join("simple/index.html").is_file());
}

#[test]
fn odd_release_filenames_are_checked_downloaded_published_and_verified() {
    let filenames = [
        "example_pkg-1.0.1.dev3460.win32-py2.4.exe",
        "example_pkg-9.0-py3-none-any.whl",
        "unrecognized.blob",
    ];
    let server = Server::with_files(&["1.0", "1.0.1"], &filenames);
    let root = tempdir().unwrap();
    let checked: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "check").stdout).unwrap();
    assert_eq!(checked["error_count"], 0);
    assert_eq!(checked["summary"]["files"], 3);
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("metadata/example-pkg.json")).unwrap())
            .unwrap();
    for (filename, version) in filenames.iter().zip(["1.0.1", "9.0", "unknown"]) {
        let release = metadata["releases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|release| release["version"] == version)
            .unwrap();
        assert_eq!(release["files"][0]["filename"], *filename);
    }
    let mirrored: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "mirror").stdout).unwrap();
    assert_eq!(mirrored["inspection"]["error_count"], 0);
    assert_eq!(mirrored["downloads"]["progress"]["downloaded"], 3);
    let page = fs::read_to_string(root.path().join("simple/example-pkg/index.html")).unwrap();
    assert_eq!(hrefs(&page).len(), 3);
    for filename in filenames {
        assert!(page.contains(filename));
        assert_eq!(
            fs::read(root.path().join("packages/example-pkg").join(filename)).unwrap(),
            BODY
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .arg("verify")
        .arg(root.path())
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let verified: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(verified["ok"], 3);
    assert_eq!(verified["project_errors"], 0);
}

#[test]
fn spaced_and_local_version_filenames_are_mirrored_with_encoded_links() {
    let filenames = ["Car Crash Detection-0.0.1.tar.gz", "weboob-1.3+1.tar.gz"];
    let server = Server::with_files(&["8.0", "9.0"], &filenames);
    let root = tempdir().unwrap();
    let checked: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "check").stdout).unwrap();
    assert_eq!(checked["error_count"], 0);
    assert_eq!(checked["summary"]["files"], 2);
    let mirrored: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "mirror").stdout).unwrap();
    assert_eq!(mirrored["inspection"]["error_count"], 0);
    assert_eq!(mirrored["downloads"]["progress"]["downloaded"], 2);
    let project = root.path().join("simple/example-pkg");
    let page = fs::read_to_string(project.join("index.html")).unwrap();
    let links = hrefs(&page);
    assert_eq!(links.len(), 2);
    assert!(
        links
            .iter()
            .any(|link| link.contains("Car%20Crash%20Detection-0.0.1.tar.gz"))
    );
    assert!(
        links
            .iter()
            .any(|link| link.contains("weboob-1.3%2B1.tar.gz"))
    );
    let base = Url::from_directory_path(project).unwrap();
    for link in links {
        let path = base.join(link).unwrap().to_file_path().unwrap();
        assert_eq!(fs::read(path).unwrap(), BODY);
    }
    for filename in filenames {
        assert_eq!(
            fs::read(root.path().join("packages/example-pkg").join(filename)).unwrap(),
            BODY
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .arg("verify")
        .arg(root.path())
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let verified: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(verified["ok"], 2);
    assert_eq!(verified["project_errors"], 0);
}

#[test]
fn clean_journals_are_compact_and_empty_error_journals_are_not_created() {
    let server = Server::start();
    let root = tempdir().unwrap();
    let mirrored: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "mirror").stdout).unwrap();
    let outcomes = fs::read_to_string(
        root.path()
            .join(mirrored["downloads"]["outcomes_path"].as_str().unwrap()),
    )
    .unwrap();
    for line in outcomes.lines() {
        let outcome: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(outcome.get("task").is_none());
        assert!(outcome.get("sha256").is_none());
        assert!(outcome.get("download_url").is_none());
        assert_eq!(outcome["package"], "example-pkg");
        assert_eq!(outcome["downloaded"], true);
    }
    let checked: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "check").stdout).unwrap();
    for summary in [&mirrored["inspection"], &checked] {
        assert!(summary.get("errors_path").is_none());
        assert!(summary.get("unavailable_path").is_none());
        let run = root.path().join(summary["packages_path"].as_str().unwrap());
        assert!(!run.parent().unwrap().join("package-errors.jsonl").exists());
        assert!(
            !run.parent()
                .unwrap()
                .join("unavailable-projects.jsonl")
                .exists()
        );
    }
    let journal = root.path().join(checked["packages_path"].as_str().unwrap());
    let records = fs::read_to_string(&journal).unwrap();
    assert_eq!(records.lines().count(), 1);
    let record: serde_json::Value = serde_json::from_str(records.lines().next().unwrap()).unwrap();
    assert_eq!(record["matching_size"], 2);
    assert_eq!(record["files"], 2);
    assert!(record.get("problems").is_none());
    assert!(record.get("metadata").is_none());
    assert!(record.get("releases").is_none());
    assert_eq!(record["metadata_path"], "metadata/example-pkg.json");
    fs::write(
        root.path().join("packages/example-pkg").join(FILENAMES[0]),
        b"x",
    )
    .unwrap();
    let checked: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "check").stdout).unwrap();
    let records =
        fs::read_to_string(root.path().join(checked["packages_path"].as_str().unwrap())).unwrap();
    let record: serde_json::Value = serde_json::from_str(records.lines().next().unwrap()).unwrap();
    assert_eq!(record["matching_size"], 1);
    assert_eq!(record["size_mismatch"], 1);
    let problems = record["problems"].as_array().unwrap();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0]["filename"], FILENAMES[0]);
    assert_eq!(problems[0]["status"], "size_mismatch");
    assert_eq!(problems[0]["actual_size"], 1);
}

#[test]
#[cfg(unix)]
fn metadata_reports_journals_and_run_directories_are_readable_by_other_users() {
    use std::os::unix::fs::PermissionsExt;
    fn mode(path: &Path, expected: u32) {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            expected,
            "{}",
            path.display()
        );
    }
    let root = tempdir().unwrap();
    let server = Server::start();
    let mirrored: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "mirror").stdout).unwrap();
    for path in [
        "metadata/example-pkg.json",
        "check-report.json",
        "mirror-report.json",
        "mirror-progress.json",
    ] {
        mode(&root.path().join(path), 0o644);
    }
    for path in [
        mirrored["inspection"]["packages_path"].as_str().unwrap(),
        mirrored["downloads"]["outcomes_path"].as_str().unwrap(),
    ] {
        mode(&root.path().join(path), 0o644);
        mode(root.path().join(path).parent().unwrap(), 0o755);
    }
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .arg("verify")
        .arg(root.path())
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let verified: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let path = root
        .path()
        .join(verified["problems_path"].as_str().unwrap());
    mode(&path, 0o644);
    mode(path.parent().unwrap(), 0o755);
    mode(&root.path().join("verify-report.json"), 0o644);
    server.unavailable.store(true, Ordering::Release);
    let checked: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "check").stdout).unwrap();
    mode(
        &root
            .path()
            .join(checked["unavailable_path"].as_str().unwrap()),
        0o644,
    );
}

#[test]
fn report_retention_flags_counts_and_current_references_work_across_commands() {
    fn plant(root: &Path, prefix: &str, years: &[u16]) {
        for year in years {
            let path = root.join(format!("reports/{prefix}{year}0101T000000Z-old"));
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("details.jsonl"), b"{}\n").unwrap();
        }
    }
    fn verify(root: &Path, keep: &str) -> serde_json::Value {
        let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
            .arg("verify")
            .arg(root)
            .args(["--keep-reports", keep, "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    let root = tempdir().unwrap();
    let server = Server::start();
    server.run(root.path(), "mirror");
    for prefix in ["run-", "verify-"] {
        plant(root.path(), prefix, &[1990, 1991, 1992]);
    }
    let checked: serde_json::Value = serde_json::from_slice(
        &server
            .run_with(root.path(), "check", &["--keep-reports", "1"])
            .stdout,
    )
    .unwrap();
    assert_eq!(checked["reports_pruned"], 5);
    let checked_path = root.path().join(checked["packages_path"].as_str().unwrap());
    assert!(checked_path.file_name().is_some());
    assert!(
        checked_path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("run-20")
    );
    plant(root.path(), "verify-", &[1980, 1981, 1982]);
    let verified = verify(root.path(), "1");
    assert_eq!(verified["reports_pruned"], 3);
    let mirrored: serde_json::Value = serde_json::from_slice(
        &server
            .run_with(root.path(), "mirror", &["--keep-reports", "1"])
            .stdout,
    )
    .unwrap();
    assert_eq!(mirrored["inspection"]["reports_pruned"], 1);
    for path in [
        checked["packages_path"].as_str().unwrap(),
        verified["problems_path"].as_str().unwrap(),
        mirrored["inspection"]["packages_path"].as_str().unwrap(),
    ] {
        assert!(root.path().join(path).is_file(), "{path}");
    }
    plant(root.path(), "run-", &[1971, 1972]);
    plant(root.path(), "verify-", &[1971, 1972]);
    let before: Vec<_> = fs::read_dir(root.path().join("reports"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(verify(root.path(), "0")["reports_pruned"], 0);
    assert!(before.iter().all(|path| path.is_dir()));
    let plain = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .arg("check")
        .arg(root.path())
        .args([
            "--package",
            "Example_Pkg",
            "--metadata-url",
            &format!("{}/simple/", server.base),
            "--keep-reports",
            "1",
            "--no-progress",
        ])
        .output()
        .unwrap();
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(String::from_utf8_lossy(&plain.stdout).contains("old report directories pruned"));
    assert!(String::from_utf8_lossy(&plain.stderr).contains("Pruned"));
}

#[test]
fn check_reports_full_download_sizes_only_for_missing_and_wrong_sized_files() {
    let server = Server::start();
    let root = tempdir().unwrap();
    let selected = (BODY.len() * 2) as u64;
    let empty: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "check").stdout).unwrap();
    assert_eq!(empty["summary"]["expected_bytes"], selected);
    assert_eq!(empty["summary"]["download_bytes"], selected);
    let first = root.path().join("packages/example-pkg").join(FILENAMES[0]);
    fs::write(&first, BODY).unwrap();
    let matching: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "check").stdout).unwrap();
    assert_eq!(matching["summary"]["expected_bytes"], selected);
    assert_eq!(matching["summary"]["download_bytes"], BODY.len());
    assert_eq!(matching["summary"]["matching_size"], 1);
    fs::write(&first, b"x").unwrap();
    let wrong: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "check").stdout).unwrap();
    assert_eq!(wrong["summary"]["download_bytes"], selected);
    assert_eq!(wrong["summary"]["size_mismatch"], 1);
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("check-report.json")).unwrap()).unwrap();
    assert_eq!(saved["summary"]["download_bytes"], selected);
    let plain = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .arg("check")
        .arg(root.path())
        .args([
            "--package",
            "Example_Pkg",
            "--metadata-url",
            &format!("{}/simple/", server.base),
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&plain.stdout)
            .lines()
            .last()
            .unwrap(),
        format!(
            "2 files ({selected} bytes of {selected} bytes selected) need downloading; no downloads performed"
        )
    );
    let mirrored: serde_json::Value =
        serde_json::from_slice(&server.run(root.path(), "mirror").stdout).unwrap();
    assert_eq!(
        mirrored["inspection"]["summary"]["download_bytes"],
        selected
    );
    assert_eq!(mirrored["final_check"]["download_bytes"], 0);
    assert_eq!(mirrored["final_check"]["expected_bytes"], selected);
}
