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
