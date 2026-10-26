use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tempfile::tempdir;

struct State {
    names: Vec<String>,
    requests: Vec<String>,
    failed_project: Option<String>,
    failed_download: Option<String>,
}
struct Server {
    base: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            names: vec!["zulu".into(), "beta".into(), "alpha".into()],
            requests: Vec::new(),
            failed_project: None,
            failed_download: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let serving = state.clone();
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
                let request = String::from_utf8(request).unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                let mut state = serving.lock().unwrap();
                state.requests.push(path.to_owned());
                let (status, body) = if path == "/simple/" {
                    (200, serde_json::json!({"meta":{"api-version":"1.4"}, "projects":state.names.iter().map(|name| serde_json::json!({"name":name})).collect::<Vec<_>>()} ).to_string().into_bytes())
                } else if path.starts_with("/simple/") {
                    let name = path.split('/').nth(2).unwrap();
                    if state.failed_project.as_deref() == Some(name) {
                        (503, b"failure".to_vec())
                    } else {
                        (200, serde_json::json!({"name":name,"meta":{"api-version":"1.4"},"versions":["1.0"],"files":[{
                        "filename":format!("{name}-1.0.tar.gz"),"url":format!("{origin}/files/{name}"),"size":1,
                        "hashes":{"sha256":format!("{:x}",Sha256::digest(b"x"))}
                    }]}).to_string().into_bytes())
                    }
                } else {
                    assert!(path.starts_with("/files/"));
                    if state.failed_download.as_deref() == path.strip_prefix("/files/") {
                        (500, b"download failure".to_vec())
                    } else {
                        (200, b"x".to_vec())
                    }
                };
                drop(state);
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
            state,
            stop,
            thread: Some(thread),
        }
    }
    fn command(&self, root: &Path, mode: &str, selection: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_snatchrs"))
            .arg(mode)
            .args(if mode == "mirror" {
                &["--min-free", "0"][..]
            } else {
                &[]
            })
            .arg(root)
            .args(selection)
            .args([
                "--index-url",
                &format!("{}/simple/", self.base),
                "--metadata-concurrency",
                "1",
                "--attempts",
                "1",
                "--json",
                "--quiet",
            ])
            .env("NO_PROXY", "*")
            .env("no_proxy", "*")
            .output()
            .unwrap()
    }
    fn run(&self, root: &Path, mode: &str, selection: &[&str]) -> serde_json::Value {
        let output = self.command(root, mode, selection);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn all_mirrors_in_index_order_without_names_in_report_and_prunes_deleted_pages_only() {
    let server = Server::start();
    let root = tempdir().unwrap();
    let report = server.run(root.path(), "mirror", &["--all"]);
    assert_eq!(report["inspection"]["selection"], "all");
    assert!(report["inspection"].get("selected_packages").is_none());
    assert!(report["inspection"]["seed"].is_null());
    assert_eq!(report["final_check"]["packages"], 3);
    let records = fs::read_to_string(
        root.path()
            .join(report["inspection"]["packages_path"].as_str().unwrap()),
    )
    .unwrap();
    let names: Vec<String> = records
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["name"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(names, ["zulu", "beta", "alpha"]);
    let root_page = fs::read_to_string(root.path().join("simple/index.html")).unwrap();
    for name in &names {
        assert!(root_page.contains(&format!("href=\"{name}/\"")));
        assert!(
            root.path()
                .join("simple")
                .join(name)
                .join("index.html")
                .is_file()
        );
    }
    server
        .state
        .lock()
        .unwrap()
        .names
        .retain(|name| name != "beta");
    // Check remains read-only with respect to public pages, even for --all.
    let checked = server.run(root.path(), "check", &["--all"]);
    assert_eq!(checked["selection"], "all");
    assert!(checked.get("selected_packages").is_none());
    assert!(root.path().join("simple/beta/index.html").is_file());
    for selection in [
        vec!["--package", "alpha"],
        vec!["--sample", "1", "--seed", "42"],
    ] {
        let report = server.run(root.path(), "mirror", &selection);
        assert_eq!(report["simple_index"]["projects_removed"], 0);
        assert!(root.path().join("simple/beta/index.html").is_file());
    }
    // The complete root listing remains authoritative despite metadata errors.
    server.state.lock().unwrap().failed_project = Some("alpha".into());
    let output = server.command(root.path(), "mirror", &["--all"]);
    assert!(!output.status.success());
    let failed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(failed["simple_index"]["projects_removed"], 1);
    assert_eq!(failed["inspection"]["error_count"], 1);
    assert!(!root.path().join("simple/beta/index.html").exists());
    assert!(!root.path().join("metadata/beta.json").exists());
    assert!(root.path().join("simple/alpha/index.html").is_file());
    assert_eq!(
        fs::read(root.path().join("packages/beta/beta-1.0.tar.gz")).unwrap(),
        b"x"
    );
    server.state.lock().unwrap().failed_project = None;
    let report = server.run(root.path(), "mirror", &["--all"]);
    assert_eq!(report["simple_index"]["projects_removed"], 0);
    assert!(!root.path().join("simple/beta/index.html").exists());
    assert!(!root.path().join("metadata/beta.json").exists());
    assert_eq!(
        fs::read(root.path().join("packages/beta/beta-1.0.tar.gz")).unwrap(),
        b"x"
    );
    let root_page = fs::read_to_string(root.path().join("simple/index.html")).unwrap();
    assert!(!root_page.contains("href=\"beta/\""));
    assert!(root_page.contains("href=\"alpha/\""));
    server.state.lock().unwrap().names.clear();
    let report = server.run(root.path(), "mirror", &["--all"]);
    assert_eq!(report["simple_index"]["projects_removed"], 2);
    assert_eq!(
        fs::read(root.path().join("packages/alpha/alpha-1.0.tar.gz")).unwrap(),
        b"x"
    );
}

#[test]
fn packages_file_and_explicit_packages_deduplicate_without_root_index_requests() {
    let server = Server::start();
    let root = tempdir().unwrap();
    let file = root.path().join("requirements.txt");
    fs::write(&file, "# selected names\n\nZulu>=1\nALPHA[extra]==1; python_version<\"3.9\"\nzulu # duplicate\nalpha @ https://elsewhere.invalid/file\n").unwrap();
    let report = server.run(
        root.path(),
        "mirror",
        &["--packages-file", file.to_str().unwrap()],
    );
    assert_eq!(report["inspection"]["selection"], "packages_file");
    assert_eq!(
        report["inspection"]["packages_file"],
        file.to_str().unwrap()
    );
    assert_eq!(
        report["inspection"]["selected_packages"],
        serde_json::json!(["zulu", "alpha"])
    );
    assert_eq!(report["final_check"]["packages"], 2);
    assert!(
        server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|path| path != "/simple/")
    );
    assert!(!root.path().join("packages/beta").exists());
    let report = server.run(
        root.path(),
        "mirror",
        &[
            "--package",
            "Some_Pkg",
            "--package",
            "some.pkg",
            "--package",
            "SOME-PKG",
            "--package",
            "zulu",
        ],
    );
    assert_eq!(report["inspection"]["selection"], "packages");
    assert_eq!(
        report["inspection"]["selected_packages"],
        serde_json::json!(["some-pkg", "zulu"])
    );
    assert_eq!(report["simple_index"]["projects_removed"], 0);
    assert!(root.path().join("simple/alpha/index.html").is_file());
    assert!(
        server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|path| path != "/simple/")
    );
    let metadata_requests: Vec<_> = server
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|path| path.starts_with("/simple/"))
        .cloned()
        .collect();
    assert_eq!(
        metadata_requests,
        [
            "/simple/zulu/",
            "/simple/alpha/",
            "/simple/some-pkg/",
            "/simple/zulu/"
        ]
    );
}

#[test]
fn all_prunes_delisted_projects_even_when_a_remaining_download_fails() {
    let server = Server::start();
    let root = tempdir().unwrap();
    server.run(root.path(), "mirror", &["--all"]);
    {
        let mut state = server.state.lock().unwrap();
        state.names.retain(|name| name != "beta");
        state.failed_download = Some("zulu".into());
    }
    fs::remove_file(root.path().join("packages/zulu/zulu-1.0.tar.gz")).unwrap();
    let output = server.command(root.path(), "mirror", &["--all"]);
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["inspection"]["error_count"], 0);
    assert_eq!(report["downloads"]["progress"]["failed"], 1);
    assert_eq!(report["simple_index"]["projects_removed"], 1);
    assert!(!root.path().join("simple/beta/index.html").exists());
    assert!(!root.path().join("metadata/beta.json").exists());
    for name in ["alpha", "zulu"] {
        assert!(
            root.path()
                .join("simple")
                .join(name)
                .join("index.html")
                .is_file()
        );
        assert!(
            root.path()
                .join("metadata")
                .join(format!("{name}.json"))
                .is_file()
        );
    }
    assert_eq!(
        fs::read(root.path().join("packages/beta/beta-1.0.tar.gz")).unwrap(),
        b"x"
    );
    assert_eq!(
        fs::read(root.path().join("packages/alpha/alpha-1.0.tar.gz")).unwrap(),
        b"x"
    );
    let index = fs::read_to_string(root.path().join("simple/index.html")).unwrap();
    assert!(!index.contains("href=\"beta/\""));
    assert!(index.contains("href=\"zulu/\""));
    assert!(index.contains("href=\"alpha/\""));
}

#[test]
fn empty_packages_files_fail_before_reports_or_network_requests() {
    let server = Server::start();
    for contents in ["", "  \n# a comment\n \t # another comment\n"] {
        for mode in ["mirror", "check"] {
            let parent = tempdir().unwrap();
            let root = parent.path().join("mirror");
            let file = parent.path().join("packages.txt");
            fs::write(&file, contents).unwrap();
            let before = server.state.lock().unwrap().requests.len();
            let output = server.command(&root, mode, &["--packages-file", file.to_str().unwrap()]);
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains(&format!("No project names found in {}", file.display()))
            );
            assert!(!root.join("reports").exists());
            assert!(!root.join("mirror-report.json").exists());
            assert!(!root.join("check-report.json").exists());
            assert_eq!(server.state.lock().unwrap().requests.len(), before);
        }
    }
}
