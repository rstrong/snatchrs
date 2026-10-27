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

struct Server {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Server {
    fn start(files_per_project: usize, conflicting: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let recorded = requests.clone();
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
                let mut bytes = Vec::new();
                while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                    let mut buffer = [0; 1024];
                    let size = stream.read(&mut buffer).unwrap();
                    assert!(size > 0);
                    bytes.extend_from_slice(&buffer[..size]);
                }
                let request = String::from_utf8(bytes).unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                recorded.lock().unwrap().push(path.to_owned());
                let body = if path.starts_with("/simple/") {
                    let name = path.split('/').nth(2).unwrap();
                    if files_per_project > 1 && name != "p0" {
                        thread::sleep(Duration::from_millis(25));
                    }
                    let mut files: Vec<_> = (0..files_per_project).map(|number| serde_json::json!({
                        "filename":format!("{name}-1.0-{number}-py3-none-any.whl"), "size":1,
                        "url":format!("{origin}/artifact/{name}/{number}"), "hashes":{"sha256":format!("{:x}",Sha256::digest(b"x"))}
                    })).collect();
                    if conflicting && name == "bad" {
                        let mut conflict = files[0].clone();
                        conflict["hashes"]["sha256"] = "ab".repeat(32).into();
                        files.push(conflict);
                    }
                    serde_json::json!({"name":name,"meta":{"api-version":"1.4"},"versions":["1.0"],"files":files}).to_string().into_bytes()
                } else {
                    assert!(path.starts_with("/artifact/"));
                    b"x".to_vec()
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                let _ = stream.write_all(&body);
            }
        });
        Self {
            base,
            requests,
            stop,
            thread: Some(thread),
        }
    }
    fn mirror(&self, root: &Path, names: &[String], extra: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_snatchrs"));
        command.arg("mirror").arg(root);
        if !extra.contains(&"--min-free") {
            command.args(["--min-free", "0"]);
        }
        for name in names {
            command.args(["--package", name]);
        }
        command
            .args([
                "--metadata-url",
                &format!("{}/simple/", self.base),
                "--metadata-concurrency",
                "1",
                "--concurrency",
                "2",
                "--attempts",
                "1",
                "--json",
                "--quiet",
            ])
            .args(extra)
            .env("NO_PROXY", "*")
            .env("no_proxy", "*")
            .output()
            .unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}
fn assert_partition(report: &serde_json::Value, total: u64) {
    let inspection = &report["inspection"];
    assert_eq!(
        inspection["checked"].as_u64().unwrap()
            + inspection["error_count"].as_u64().unwrap()
            + inspection["unavailable_count"].as_u64().unwrap()
            + inspection["not_inspected"].as_u64().unwrap(),
        total
    );
}
fn assert_no_partial_files(path: &Path) {
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        assert!(
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".snatchrs-")
        );
        if entry.file_type().unwrap().is_dir() {
            assert_no_partial_files(&entry.path());
        }
    }
}

#[test]
fn storage_stop_saves_a_report_and_does_not_fail_every_remaining_job() {
    let root = tempdir().unwrap();
    let server = Server::start(1000, false);
    let names: Vec<_> = (0..20).map(|number| format!("p{number}")).collect();
    let output = server.mirror(root.path(), &names, &["--min-free", "1000T"]);
    assert!(!output.status.success());
    let report = json(&output);
    assert!(root.path().join("mirror-report.json").is_file());
    let progress = &report["downloads"]["progress"];
    assert!(
        progress["stopped"]
            .as_str()
            .unwrap()
            .contains("Not enough disk space")
    );
    let failed = progress["failed"].as_u64().unwrap();
    assert!((1..=2).contains(&failed));
    assert_eq!(progress["downloaded"], 0);
    assert_eq!(
        progress["total_files"].as_u64().unwrap(),
        failed + progress["not_attempted"].as_u64().unwrap()
    );
    assert!(report["inspection"]["not_inspected"].as_u64().unwrap() > 0);
    assert_partition(&report, names.len() as u64);
    assert_eq!(report["downloads"]["min_free"], 1000 * 1024u64.pow(4));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("disk space"), "{stderr}");
    assert!(stderr.contains("--min-free"));
    assert!(stderr.contains("Free space and rerun"));
    let outcomes = fs::read_to_string(
        root.path()
            .join(report["downloads"]["outcomes_path"].as_str().unwrap()),
    )
    .unwrap();
    assert_eq!(outcomes.lines().count(), failed as usize);
    let queue_errors = fs::read_to_string(
        root.path()
            .join(report["inspection"]["errors_path"].as_str().unwrap()),
    )
    .unwrap();
    assert_eq!(
        queue_errors
            .lines()
            .filter(
                |line| serde_json::from_str::<serde_json::Value>(line).unwrap()["name"]
                    == "<queue>"
            )
            .count(),
        1
    );
    assert_eq!(report["inspection"]["error_count"], 0);
    for line in outcomes.lines() {
        let outcome: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(outcome["attempts"], 1);
        assert_eq!(outcome["downloaded"], false);
    }
    assert_no_partial_files(&root.path().join("packages"));
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| !request.starts_with("/artifact/"))
    );
    let snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("mirror-progress.json")).unwrap())
            .unwrap();
    assert_eq!(snapshot["not_attempted"], progress["not_attempted"]);
    assert_eq!(snapshot["stopped"], progress["stopped"]);
}

#[test]
fn package_queue_errors_do_not_stop_later_projects() {
    let root = tempdir().unwrap();
    let server = Server::start(1, true);
    let output = server.mirror(root.path(), &["bad".into(), "good".into()], &[]);
    assert!(!output.status.success());
    let report = json(&output);
    assert_eq!(report["inspection"]["error_count"], 1);
    assert_eq!(report["inspection"]["not_inspected"], 0);
    assert!(report["inspection"]["queue_error"].is_null());
    assert!(report["downloads"]["progress"]["stopped"].is_null());
    assert_eq!(report["downloads"]["progress"]["downloaded"], 1);
    assert_partition(&report, 2);
    let errors = fs::read_to_string(
        root.path()
            .join(report["inspection"]["errors_path"].as_str().unwrap()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = root
            .path()
            .join(report["inspection"]["errors_path"].as_str().unwrap());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
    assert!(errors.contains("Conflicting metadata"));
    assert!(errors.contains("bad"));
    assert_eq!(
        fs::read(
            root.path()
                .join("packages/good/good-1.0-0-py3-none-any.whl")
        )
        .unwrap(),
        b"x"
    );
}

#[test]
fn project_page_errors_keep_other_pages_and_the_final_report() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("simple")).unwrap();
    fs::write(root.path().join("simple/bad"), b"not a directory").unwrap();
    let server = Server::start(1, false);
    let output = server.mirror(root.path(), &["bad".into(), "good".into()], &[]);
    assert!(!output.status.success());
    let report = json(&output);
    assert_eq!(report["simple_index"]["page_errors"], 1);
    assert_eq!(
        report["simple_index"]["errors"].as_array().unwrap().len(),
        1
    );
    assert_eq!(report["simple_index"]["errors"][0]["name"], "bad");
    assert!(root.path().join("simple/good/index.html").is_file());
    let index = fs::read_to_string(root.path().join("simple/index.html")).unwrap();
    assert!(index.contains("href=\"good/\""));
    assert!(!index.contains("href=\"bad/\""));
    assert_eq!(report["downloads"]["progress"]["downloaded"], 2);
    assert_partition(&report, 2);
}

#[test]
fn standalone_simple_keeps_corrupt_projects_and_removes_only_absent_metadata() {
    let root = tempdir().unwrap();
    let server = Server::start(1, false);
    let output = server.mirror(
        root.path(),
        &["old".into(), "bad".into(), "good".into()],
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let old_bad_page = fs::read(root.path().join("simple/bad/index.html")).unwrap();
    fs::write(root.path().join("metadata/bad.json"), b"{corrupt").unwrap();
    fs::remove_file(root.path().join("metadata/old.json")).unwrap();
    fs::remove_file(root.path().join("simple/good/index.html")).unwrap();
    let requests_before = server.requests.lock().unwrap().len();
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .arg("simple")
        .arg(root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bad"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 projects removed"));
    assert!(root.path().join("simple/good/index.html").is_file());
    assert_eq!(
        fs::read(root.path().join("simple/bad/index.html")).unwrap(),
        old_bad_page
    );
    assert!(!root.path().join("simple/old").exists());
    assert_eq!(
        fs::read(root.path().join("packages/old/old-1.0-0-py3-none-any.whl")).unwrap(),
        b"x"
    );
    let index = fs::read_to_string(root.path().join("simple/index.html")).unwrap();
    assert!(index.contains("href=\"bad/\""));
    assert!(index.contains("href=\"good/\""));
    assert!(!index.contains("href=\"old/\""));
    assert_eq!(server.requests.lock().unwrap().len(), requests_before);
}

#[test]
fn root_page_failure_still_saves_the_mirror_report() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("simple/index.html")).unwrap();
    let server = Server::start(1, false);
    let output = server.mirror(root.path(), &["good".into()], &[]);
    assert!(!output.status.success());
    let report = json(&output);
    assert_eq!(report["simple_index"]["page_errors"], 1);
    assert_eq!(report["simple_index"]["errors"][0]["name"], "<root>");
    assert!(root.path().join("simple/good/index.html").is_file());
    assert!(root.path().join("mirror-report.json").is_file());
}
