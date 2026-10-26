#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tempfile::tempdir;

struct Server {
    base: String,
    stop: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    later_started: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}
impl Server {
    fn new(slow: bool, metadata_delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/simple/", listener.local_addr().unwrap());
        let origin = base.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let started = Arc::new(AtomicBool::new(false));
        let starting = started.clone();
        let later_started = Arc::new(AtomicBool::new(false));
        let later = later_started.clone();
        let handle = thread::spawn(move || {
            let mut handlers = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                let stopping = stopping.clone();
                let starting = starting.clone();
                let later = later.clone();
                let origin = origin.clone();
                handlers.push(thread::spawn(move || {
                    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    let mut request = Vec::new(); while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") { let mut buf = [0;1024]; let Ok(n) = stream.read(&mut buf) else {return;}; if n==0 {return;} request.extend_from_slice(&buf[..n]); }
                    let request = String::from_utf8_lossy(&request); let path = request.split_whitespace().nth(1).unwrap();
                    let body = if path == "/simple/p/" {
                        serde_json::to_vec(&json!({"name":"p", "meta":{"api-version":"1.4"}, "versions":["1.0"], "files": (1..=6).map(|n| json!({"filename":format!("p-1.0-{n}-py3-none-any.whl"), "url":format!("{origin}artifact/{n}"), "size":if slow && n>1 {1048576} else {4}, "hashes":{}})).collect::<Vec<_>>() })).unwrap()
                    } else if path.contains("/artifact/") {
                        if slow && !path.ends_with("/1") {
                            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n"); let _ = stream.write_all(&[7;1024]); let _ = stream.flush(); starting.store(true, Ordering::Release);
                            while !stopping.load(Ordering::Acquire) { thread::sleep(Duration::from_millis(10)); } return;
                        }
                        b"good".to_vec()
                    } else {
                        later.store(true, Ordering::Release); let until = Instant::now()+metadata_delay;
                        while Instant::now()<until && !stopping.load(Ordering::Acquire) {thread::sleep(Duration::from_millis(5));}
                        let name = path.trim_matches('/').strip_prefix("simple/").unwrap();
                        serde_json::to_vec(&json!({"name":name, "meta":{"api-version":"1.4"}, "files":[], "versions":[]})).unwrap()
                    };
                    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()); let _ = stream.write_all(&body);
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        Self {
            base,
            stop,
            started,
            later_started,
            handle: Some(handle),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap();
    }
}
fn command(root: &Path, server: &Server, name: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_snatchrs"));
    cmd.arg(name).arg(root).args([
        "--package",
        "p",
        "--metadata-url",
        &server.base,
        "--json",
        "--no-progress",
    ]);
    if name == "mirror" {
        cmd.args(["-j", "1", "--min-free", "0"]);
    }
    cmd
}
fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)))
}
fn plant(root: &Path) -> Vec<PathBuf> {
    [
        ".snatchrs-root",
        "metadata/.snatchrs-json-x",
        "simple/.snatchrs-html-x",
        "reports/.snatchrs-report",
        "packages/p/.snatchrs-download",
        "simple/p/.snatchrs-html-y",
    ]
    .into_iter()
    .map(|name| {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"stale").unwrap();
        path
    })
    .collect()
}
#[test]
fn all_mutating_commands_sweep_and_count_stale_files() {
    let temp = tempdir().unwrap();
    let server = Server::new(false, Duration::ZERO);
    let planted = plant(temp.path());
    fs::create_dir_all(temp.path().join("packages/other")).unwrap();
    fs::write(temp.path().join("packages/other/.snatchrs-keep"), b"keep").unwrap();
    let output = command(temp.path(), &server, "mirror").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report(&output)["inspection"]["stale_temp_files_removed"], 6);
    for path in planted {
        assert!(!path.exists());
    }
    assert!(temp.path().join("packages/other/.snatchrs-keep").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Removed 6 stale temporary files"));
    for name in ["check", "simple", "verify"] {
        let planted = plant(temp.path());
        let output = if name == "check" {
            command(temp.path(), &server, name).output().unwrap()
        } else {
            let mut command = Command::new(env!("CARGO_BIN_EXE_snatchrs"));
            command.arg(name).arg(temp.path());
            if name == "verify" {
                command.arg("--json");
            }
            command.output().unwrap()
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        for path in planted {
            assert!(!path.exists(), "{}", path.display());
        }
        if name == "simple" {
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("6 stale temporary files removed")
            );
        } else {
            assert_eq!(report(&output)["stale_temp_files_removed"], 6);
        }
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Removed 6 stale temporary files")
        );
    }
}
fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for fixture");
        thread::sleep(Duration::from_millis(10));
    }
}
fn signal(child: &Child, signal: &str) {
    assert!(
        Command::new("kill")
            .args([signal, &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}
fn start_mirror(root: &Path, server: &Server) -> Child {
    let mut cmd = command(root, server, "mirror");
    cmd.args(["--metadata-concurrency", "1"]);
    for n in 0..25 {
        cmd.args(["--package", &format!("later{n}")]);
    }
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}
fn temps(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                found.extend(temps(&entry.path()));
            } else if entry
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".snatchrs-")
            {
                found.push(entry.path());
            }
        }
    }
    found
}
#[test]
fn interrupt_aborts_stalled_download_saves_report_and_keeps_completed_files() {
    let temp = tempdir().unwrap();
    let server = Server::new(true, Duration::from_millis(150));
    let mut child = start_mirror(temp.path(), &server);
    wait_until(|| {
        server.started.load(Ordering::Acquire)
            && fs::read(temp.path().join("mirror-progress.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .is_some_and(|r| r["total_files"].as_u64().unwrap_or(0) >= 6)
    });
    let started = Instant::now();
    signal(&child, "-INT");
    wait_until(|| child.try_wait().unwrap().is_some());
    assert!(started.elapsed() < Duration::from_secs(5));
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(temp.path().join("mirror-report.json")).unwrap()).unwrap();
    assert_eq!(report["downloads"]["progress"]["stopped"], "Interrupted");
    assert!(
        report["downloads"]["progress"]["not_attempted"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(report["inspection"]["not_inspected"].as_u64().unwrap() > 0);
    assert!(temps(temp.path()).is_empty());
    let page = fs::read_to_string(temp.path().join("simple/p/index.html")).unwrap();
    assert!(page.contains("p-1.0-1-py3-none-any.whl"));
    assert!(!page.contains("p-1.0-2-py3-none-any.whl"));
    assert_eq!(
        fs::read(temp.path().join("packages/p/p-1.0-1-py3-none-any.whl")).unwrap(),
        b"good"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Completed files are kept; rerun to continue.")
    );
    let interrupted_journal = temp
        .path()
        .join(report["inspection"]["packages_path"].as_str().unwrap());
    for year in [2000, 2001] {
        let old = temp
            .path()
            .join(format!("reports/run-{year}0101T000000Z-old"));
        fs::create_dir(&old).unwrap();
        fs::write(old.join("partial.jsonl"), b"{").unwrap();
    }
    let next = command(temp.path(), &server, "check")
        .args(["--keep-reports", "1"])
        .output()
        .unwrap();
    assert!(
        next.status.success(),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
    let next = serde_json::from_slice::<Value>(&next.stdout).unwrap();
    assert_eq!(next["reports_pruned"], 2);
    assert_eq!(next["error_count"], 0);
    assert!(
        interrupted_journal.exists(),
        "The current mirror report still references this interrupted run"
    );
}
#[test]
fn second_signal_exits_immediately_and_next_run_sweeps_leftovers() {
    let temp = tempdir().unwrap();
    let server = Server::new(true, Duration::from_secs(10));
    let mut child = start_mirror(temp.path(), &server);
    wait_until(|| {
        server.started.load(Ordering::Acquire) && server.later_started.load(Ordering::Acquire)
    });
    signal(&child, "-INT");
    thread::sleep(Duration::from_millis(30));
    signal(&child, "-INT");
    let started = Instant::now();
    wait_until(|| child.try_wait().unwrap().is_some());
    assert!(started.elapsed() < Duration::from_secs(2));
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Interrupted again; exiting immediately")
    );
    let leftovers = temps(temp.path());
    assert!(!leftovers.is_empty());
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .arg("simple")
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(temps(temp.path()).is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains(&format!(
        "Removed {} stale temporary files",
        leftovers.len()
    )));
}

#[test]
fn verify_term_and_hup_stop_hashing_and_save_partial_reports() {
    for signal_name in ["-TERM", "-HUP"] {
        let temp = tempdir().unwrap();
        let layout = snatchrs::mirror::MirrorLayout::create(temp.path()).unwrap();
        fs::create_dir_all(temp.path().join("packages/huge")).unwrap();
        let artifact = temp.path().join("packages/huge/huge.whl");
        let size = 16 * 1024 * 1024 * 1024u64;
        fs::File::create(&artifact).unwrap().set_len(size).unwrap();
        let metadata: snatchrs::mirror::PackageMetadata = serde_json::from_value(json!({
            "package":{"name":"huge", "normalized_name":"huge", "index_url":"https://offline.invalid/simple/huge/", "last_serial":null},
            "metadata_url":"https://offline.invalid/simple/huge/", "metadata_path":"metadata/huge.json", "package_path":"packages/huge", "last_serial":null,
            "releases":[{"version":"1.0", "files":[{"filename":"huge.whl", "download_url":"https://offline.invalid/huge.whl", "local_path":"packages/huge/huge.whl", "size":size, "sha256":"00", "package_type":null, "requires_python":null, "yanked":false}]}]
        })).unwrap();
        layout.save_metadata(&metadata).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
            .arg("verify")
            .arg(temp.path())
            .args(["-j", "1", "--remove-bad", "--quiet"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        wait_until(|| {
            fs::read_dir(temp.path().join("reports"))
                .is_ok_and(|mut entries| entries.next().is_some())
        });
        thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        signal(&child, signal_name);
        wait_until(|| child.try_wait().unwrap().is_some());
        assert!(start.elapsed() < Duration::from_secs(5));
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(130));
        let report: Value =
            serde_json::from_slice(&fs::read(temp.path().join("verify-report.json")).unwrap())
                .unwrap();
        assert_eq!(report["stopped"], "Interrupted");
        assert_eq!(report["removed"], 0);
        assert_eq!(report["error"], 0);
        assert!(report["bytes_hashed"].as_u64().unwrap() < size);
        assert!(artifact.exists());
        assert!(temps(temp.path()).is_empty());
    }
}

#[test]
fn discovery_and_metadata_retry_waits_wake_and_save_partial_reports() {
    for root_listing in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/simple/", listener.local_addr().unwrap());
        let ready = Arc::new(AtomicBool::new(false));
        let served = ready.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|s| s == b"\r\n\r\n") {
                let mut buf = [0; 1024];
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
            }
            stream.write_all(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 60\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            served.store(true, Ordering::Release);
        });
        let temp = tempdir().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_snatchrs"));
        if root_listing {
            fs::create_dir_all(temp.path().join("simple/retained")).unwrap();
            fs::write(temp.path().join("simple/retained/index.html"), b"retained").unwrap();
            command
                .arg("mirror")
                .args(["--min-free", "0", "--all", "--index-url", &base]);
        } else {
            command
                .arg("check")
                .args(["--package", "p", "--metadata-url", &base]);
        }
        let mut child = command
            .arg(temp.path())
            .args(["--json", "--quiet"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        wait_until(|| ready.load(Ordering::Acquire));
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        signal(&child, "-INT");
        wait_until(|| child.try_wait().unwrap().is_some());
        assert!(started.elapsed() < Duration::from_secs(3));
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(130));
        let report: Value =
            serde_json::from_slice(&fs::read(temp.path().join("check-report.json")).unwrap())
                .unwrap();
        assert_eq!(report["stopped"], "Interrupted");
        assert_eq!(report["not_inspected"], if root_listing { 0 } else { 1 });
        assert_eq!(report["error_count"], 0);
        if root_listing {
            let mirror: Value =
                serde_json::from_slice(&fs::read(temp.path().join("mirror-report.json")).unwrap())
                    .unwrap();
            assert_eq!(mirror["downloads"]["progress"]["stopped"], "Interrupted");
            assert!(temp.path().join("simple/retained/index.html").exists());
        } else {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("1 projects were not inspected")
            );
        }
        server.join().unwrap();
    }
}
