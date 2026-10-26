use sha2::{Digest, Sha256};
use snatchrs::{
    IndexClient, Package,
    download::{DownloadOptions, DownloadTask, WorkerState, download_all},
    mirror::MirrorLayout,
};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};
use tempfile::tempdir;

const METADATA: &str =
    r#"{"name":"retry-fixture","meta":{"api-version":"1.4"},"versions":[],"files":[]}"#;

#[derive(Clone)]
struct Reply {
    status: u16,
    body: Vec<u8>,
    retry_after: Option<String>,
    declared_length: Option<usize>,
    disconnect: bool,
    body_delay: Duration,
    chunk_delay: Duration,
}
impl Reply {
    fn new(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            body: body.to_vec(),
            retry_after: None,
            declared_length: None,
            disconnect: false,
            body_delay: Duration::ZERO,
            chunk_delay: Duration::ZERO,
        }
    }
}

struct Request {
    text: String,
    at: Instant,
    wall_time: SystemTime,
}
struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Server {
    fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let records = requests.clone();
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            thread::scope(|scope| {
                while !stopping.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let replies = &replies;
                            let records = &records;
                            scope.spawn(move || {
                            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                            let mut bytes = Vec::new();
                            loop {
                                let mut buffer = [0; 1024];
                                let Ok(size) = stream.read(&mut buffer) else { return; };
                                if size == 0 { return; }
                                bytes.extend_from_slice(&buffer[..size]);
                                if bytes.windows(4).any(|chunk| chunk == b"\r\n\r\n") { break; }
                            }
                            let mut requests = records.lock().unwrap();
                            let reply = replies.get(requests.len()).unwrap_or_else(|| replies.last().unwrap()).clone();
                            requests.push(Request { text: String::from_utf8(bytes).unwrap(), at: Instant::now(), wall_time: SystemTime::now() });
                            drop(requests);
                            if reply.disconnect { return; }
                            let extra = reply.retry_after.map(|value| format!("Retry-After: {value}\r\n")).unwrap_or_default();
                            let header = format!("HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n", reply.status, reply.declared_length.unwrap_or(reply.body.len()));
                            if stream.write_all(header.as_bytes()).is_err() { return; }
                            thread::sleep(reply.body_delay);
                            for chunk in reply.body.chunks(1024) {
                                if stream.write_all(chunk).is_err() { return; }
                                thread::sleep(reply.chunk_delay);
                            }
                        });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1))
                        }
                        Err(error) => panic!("{error}"),
                    }
                }
            })
        });
        Self {
            url,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn metadata(&self, attempts: u32) -> anyhow::Result<snatchrs::mirror::PackageMetadata> {
        let root = tempdir().unwrap();
        let layout = MirrorLayout::create(root.path()).unwrap();
        IndexClient::with_attempts(attempts)?.fetch_metadata(
            Package {
                name: "retry-fixture".into(),
                normalized_name: "retry-fixture".into(),
                index_url: format!("{}/simple/retry-fixture/", self.url)
                    .parse()
                    .unwrap(),
                last_serial: None,
            },
            format!("{}/simple/", self.url).parse().unwrap(),
            &layout,
        )
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn task(server: &Server, root: &std::path::Path, body: &[u8]) -> DownloadTask {
    DownloadTask {
        package: "retry-fixture".into(),
        filename: "artifact.whl".into(),
        local_path: root.join("artifact.whl"),
        download_url: format!("{}/artifact", server.url).parse().unwrap(),
        size: body.len() as u64,
        sha256: Some(format!("{:x}", Sha256::digest(body))),
    }
}

fn options(attempts: u32) -> DownloadOptions {
    DownloadOptions {
        concurrency: 1,
        attempts,
        read_timeout: Duration::from_secs(3),
    }
}

#[test]
fn metadata_429_honors_retry_after_seconds_and_then_succeeds() {
    let mut limited = Reply::new(429, b"slow down");
    limited.retry_after = Some("1".into());
    let server = Server::start(vec![limited, Reply::new(200, METADATA.as_bytes())]);
    let metadata = server.metadata(3).unwrap();
    assert!(metadata.releases.is_empty());
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].at.duration_since(requests[0].at) >= Duration::from_secs(1));
    assert!(requests.iter().all(|request| {
        request
            .text
            .starts_with("GET /simple/retry-fixture/ HTTP/1.1\r\n")
    }));
}

#[test]
fn metadata_recovers_from_disconnects_and_truncated_bodies() {
    let mut disconnected = Reply::new(200, b"");
    disconnected.disconnect = true;
    let mut truncated = Reply::new(200, b"{\"meta\":");
    truncated.declared_length = Some(100);
    for failure in [disconnected, truncated, Reply::new(503, b"temporary")] {
        let server = Server::start(vec![failure, Reply::new(200, METADATA.as_bytes())]);
        assert!(server.metadata(2).is_ok());
        assert_eq!(server.count(), 2);
    }
}

#[test]
fn permanent_metadata_statuses_and_invalid_json_do_not_retry() {
    for reply in [
        Reply::new(400, b"bad request"),
        Reply::new(403, b"denied"),
        Reply::new(404, b"missing"),
        Reply::new(501, b"not implemented"),
        Reply::new(200, b"not JSON"),
        Reply::new(200, b"{\"name\":\"broken\"}"),
    ] {
        let server = Server::start(vec![reply]);
        assert!(server.metadata(5).is_err());
        assert_eq!(server.count(), 1);
    }
}

#[test]
fn index_retries_transient_statuses() {
    let server = Server::start(vec![
        Reply::new(502, b"temporary"),
        Reply::new(200, br#"{"meta":{"api-version":"1.4"},"projects":[]}"#),
    ]);
    let index = IndexClient::with_attempts(2)
        .unwrap()
        .fetch(format!("{}/simple/", server.url).parse().unwrap())
        .unwrap();
    assert!(index.packages.is_empty());
    assert_eq!(server.count(), 2);
}

#[test]
fn check_cli_attempt_budget_applies_to_metadata_and_is_exhausted() {
    let server = Server::start(vec![Reply::new(503, b"temporary")]);
    let root = tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_snatchrs"));
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        command.env_remove(key);
    }
    let output = command
        .args([
            "check",
            "--attempts",
            "2",
            "--package",
            "retry-fixture",
            "--metadata-url",
            &format!("{}/simple/", server.url),
            "--json",
        ])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(server.count(), 2);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["error_count"], 1);
    let errors =
        fs::read_to_string(root.path().join(report["errors_path"].as_str().unwrap())).unwrap();
    let error: serde_json::Value = serde_json::from_str(errors.lines().next().unwrap()).unwrap();
    assert!(error["error"].as_str().unwrap().contains("2 attempt(s)"));
}

#[test]
fn download_retry_after_http_date_is_respected_and_visible_in_progress() {
    let deadline = SystemTime::now() + Duration::from_secs(2);
    let date = httpdate::fmt_http_date(deadline);
    let advertised = httpdate::parse_http_date(&date).unwrap();
    let mut unavailable = Reply::new(503, b"temporary");
    unavailable.retry_after = Some(date);
    let server = Server::start(vec![unavailable, Reply::new(200, b"x")]);
    let root = tempdir().unwrap();
    let mut saw_retry = false;
    let report = download_all(
        root.path(),
        &[task(&server, root.path(), b"x")],
        options(3),
        |progress| {
            saw_retry |= progress.workers.iter().any(|worker| {
                worker.state == WorkerState::Retrying && worker.retry_delay_seconds.is_some()
            });
        },
    )
    .unwrap();
    assert_eq!(report.progress.downloaded, 1);
    assert_eq!(report.outcomes[0].attempts, 2);
    assert!(saw_retry);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].wall_time >= advertised);
    assert_eq!(fs::read(root.path().join("artifact.whl")).unwrap(), b"x");
}

#[test]
fn downloads_do_not_retry_404_integrity_or_local_path_failures() {
    for reply in [
        Reply::new(404, b"missing"),
        Reply::new(403, b"denied"),
        Reply::new(200, b"z"),
        Reply::new(200, b"too large"),
    ] {
        let server = Server::start(vec![reply]);
        let root = tempdir().unwrap();
        let report = download_all(
            root.path(),
            &[task(&server, root.path(), b"x")],
            options(5),
            |_| {},
        )
        .unwrap();
        assert_eq!(report.progress.failed, 1);
        assert_eq!(report.outcomes[0].attempts, 1);
        assert_eq!(server.count(), 1);
        assert!(!root.path().join("artifact.whl").exists());
    }
    let server = Server::start(vec![Reply::new(200, b"x")]);
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("artifact.whl")).unwrap();
    let report = download_all(
        root.path(),
        &[task(&server, root.path(), b"x")],
        options(5),
        |_| {},
    )
    .unwrap();
    assert_eq!(report.outcomes[0].attempts, 1);
    assert_eq!(report.progress.failed, 1);
    assert_eq!(server.count(), 0);
}

#[test]
fn downloads_retry_truncated_bodies_and_read_timeouts() {
    let mut truncated = Reply::new(200, b"he");
    truncated.declared_length = Some(5);
    let mut slow = Reply::new(200, b"hello");
    slow.body_delay = Duration::from_millis(750);
    for (failure, timeout) in [
        (truncated, Duration::from_secs(3)),
        (slow, Duration::from_millis(250)),
    ] {
        let server = Server::start(vec![failure, Reply::new(200, b"hello")]);
        let root = tempdir().unwrap();
        let report = download_all(
            root.path(),
            &[task(&server, root.path(), b"hello")],
            DownloadOptions {
                read_timeout: timeout,
                ..options(2)
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(report.progress.downloaded, 1, "{:?}", report.outcomes);
        assert_eq!(report.outcomes[0].attempts, 2);
        assert_eq!(server.count(), 2);
        assert!(report.outcomes[0].sha256_verified);
    }
}

#[test]
fn active_download_can_outlast_read_timeout() {
    let body = vec![42; 16 * 1024];
    let mut reply = Reply::new(200, &body);
    reply.chunk_delay = Duration::from_millis(100);
    let server = Server::start(vec![reply]);
    let root = tempdir().unwrap();
    let read_timeout = Duration::from_millis(500);
    let started = Instant::now();
    let report = download_all(
        root.path(),
        &[task(&server, root.path(), &body)],
        DownloadOptions {
            read_timeout,
            ..options(1)
        },
        |_| {},
    )
    .unwrap();
    assert!(started.elapsed() > read_timeout * 2);
    assert_eq!(report.progress.downloaded, 1, "{:?}", report.outcomes);
    assert_eq!(report.outcomes[0].attempts, 1);
    assert!(report.outcomes[0].sha256_verified);
    assert_eq!(fs::read(root.path().join("artifact.whl")).unwrap(), body);
    assert_eq!(server.count(), 1);
}

#[test]
fn project_404_is_not_retried_and_later_projects_are_still_checked() {
    for mode in ["check", "mirror"] {
        let server = Server::start(vec![
            Reply::new(404, b"deleted"),
            Reply::new(200, METADATA.as_bytes()),
        ]);
        let root = tempdir().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
            .args([
                mode,
                "--package",
                "gone",
                "--package",
                "retry-fixture",
                "--attempts",
                "5",
                "--metadata-concurrency",
                "1",
                "--metadata-url",
                &format!("{}/simple/", server.url),
                "--json",
                "--quiet",
            ])
            .arg(root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(server.count(), 2);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let inspection = if mode == "mirror" {
            &report["inspection"]
        } else {
            &report
        };
        assert_eq!(inspection["unavailable_count"], 1);
        assert_eq!(inspection["error_count"], 0);
        assert_eq!(inspection["summary"]["packages"], 1);
        if mode == "mirror" {
            assert_eq!(
                report["downloads"]["progress"]["discovery"]["checked_packages"],
                2
            );
            assert_eq!(
                report["downloads"]["progress"]["discovery"]["unavailable"],
                1
            );
            assert_eq!(report["final_check"]["packages"], 1);
        }
    }
}

#[test]
fn worker_states_serialize_as_progress_file_strings() {
    for (state, text) in [
        (WorkerState::Waiting, "waiting"),
        (WorkerState::Downloading, "downloading"),
        (WorkerState::Retrying, "retrying"),
        (WorkerState::Done, "done"),
        (WorkerState::Failed, "failed"),
    ] {
        assert_eq!(serde_json::to_value(state).unwrap(), text);
        assert_eq!(state.to_string(), text);
    }
}
