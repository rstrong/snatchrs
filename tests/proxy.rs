//! Proxy configuration is tested in child processes: no process-global env
//! mutation, external DNS, or Internet access is needed for these regressions.
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use sha2::{Digest, Sha256};
use tempfile::tempdir;

const ARTIFACT: &[u8] = b"proxied artifact bytes";
const PROXY_ENV: &[&str] = &[
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "REQUEST_METHOD",
];

#[derive(Clone, Copy)]
enum Mode {
    Pipeline { proxy: bool, secure_artifacts: bool },
    Reject,
}

struct Server {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let recorded = requests.clone();
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            thread::scope(|scope| {
                while !stopping.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let recorded = &recorded;
                            scope.spawn(move || {
                            let Some(request) = read_request(&mut stream) else { return; };
                            recorded.lock().unwrap().push(request.clone());
                            let (status, body) = reply(mode, address, &request);
                            let header = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                            let _ = stream.write_all(header.as_bytes()).and_then(|()| stream.write_all(&body));
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
            address,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn read_request(stream: &mut TcpStream) -> Option<String> {
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let mut request = Vec::new();
    while request.len() < 65536 {
        let mut buffer = [0; 1024];
        let size = stream.read(&mut buffer).ok()?;
        if size == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..size]);
        if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            return String::from_utf8(request).ok();
        }
    }
    None
}

fn reply(mode: Mode, address: SocketAddr, request: &str) -> (&'static str, Vec<u8>) {
    let Mode::Pipeline {
        proxy,
        secure_artifacts,
    } = mode
    else {
        return ("502 Bad Gateway", Vec::new());
    };
    let target = request.split_whitespace().nth(1).unwrap();
    let path = if target.starts_with("http://") {
        url::Url::parse(target).unwrap().path().to_owned()
    } else {
        target.to_owned()
    };
    let value = match path.as_str() {
        "/simple/" => {
            serde_json::json!({"meta": {"api-version": "1.0"}, "projects": [{"name": "proxy-fixture"}]})
        }
        "/simple/proxy-fixture/" => {
            let download = if secure_artifacts {
                "https://files.invalid/artifact.whl".into()
            } else if proxy {
                "http://files.invalid/artifact.whl".into()
            } else {
                format!("http://{address}/artifact.whl")
            };
            serde_json::json!({"name": "proxy-fixture", "meta": {"api-version": "1.4"}, "versions": ["1.0"], "files": [{
                "filename": "artifact.whl", "url": download, "size": ARTIFACT.len(),
                "hashes": {"sha256": format!("{:x}", Sha256::digest(ARTIFACT))}
            }]})
        }
        "/artifact.whl" => return ("200 OK", ARTIFACT.to_vec()),
        _ => return ("404 Not Found", Vec::new()),
    };
    ("200 OK", value.to_string().into_bytes())
}

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_snatchrs"));
    command.args(["--attempts", "1"]);
    for name in PROXY_ENV {
        command.env_remove(name);
    }
    command
}

fn mirror(root: &Path, base: &str, metadata: &str) -> Command {
    let mut command = command();
    command
        .args([
            "mirror",
            "--sample",
            "1",
            "--seed",
            "42",
            "--concurrency",
            "2",
            "--json",
            "--index-url",
            base,
            "--metadata-url",
            metadata,
            "--mirror",
        ])
        .arg(root);
    command
}

fn assert_downloaded(output: &Output, root: &Path) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["downloads"]["progress"]["downloaded"], 1);
    assert_eq!(report["final_check"]["matching_size"], 1);
    assert_eq!(
        std::fs::read(root.join("packages/proxy-fixture/artifact.whl")).unwrap(),
        ARTIFACT
    );
}

#[test]
fn http_and_all_proxy_variables_route_index_metadata_and_downloads() {
    for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        let proxy = Server::start(Mode::Pipeline {
            proxy: true,
            secure_artifacts: false,
        });
        let root = tempdir().unwrap();
        let mut command = mirror(
            root.path(),
            "http://index.invalid/simple/",
            "http://metadata.invalid/simple/",
        );
        if name.eq_ignore_ascii_case("http_proxy") {
            // Protocol-specific settings must take priority over ALL_PROXY.
            command.env("ALL_PROXY", "http://127.0.0.1:1");
        }
        let output = command.env(name, proxy.url()).output().unwrap();
        assert_downloaded(&output, root.path());
        let requests = proxy.requests();
        assert_eq!(requests.len(), 3, "{name}");
        for (request, target) in requests.iter().zip([
            "http://index.invalid/simple/",
            "http://metadata.invalid/simple/proxy-fixture/",
            "http://files.invalid/artifact.whl",
        ]) {
            assert!(
                request.starts_with(&format!("GET {target} HTTP/1.1\r\n")),
                "{request}"
            );
        }
    }
}

#[test]
fn no_proxy_variables_bypass_proxy_for_all_request_types() {
    for name in ["NO_PROXY", "no_proxy"] {
        let origin = Server::start(Mode::Pipeline {
            proxy: false,
            secure_artifacts: false,
        });
        let proxy = Server::start(Mode::Reject);
        let root = tempdir().unwrap();
        let output = mirror(
            root.path(),
            &format!("{}/simple/", origin.url()),
            &format!("{}/simple/", origin.url()),
        )
        .env("HTTP_PROXY", proxy.url())
        .env("ALL_PROXY", proxy.url())
        .env(name, "localhost,127.0.0.1")
        .output()
        .unwrap();
        assert_downloaded(&output, root.path());
        assert!(proxy.requests().is_empty());
        assert_eq!(origin.requests().len(), 3);
        assert!(origin.requests()[0].starts_with("GET /simple/ HTTP/1.1\r\n"));
    }
}

#[test]
fn https_proxy_variables_use_connect_for_index_requests() {
    for name in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        let proxy = Server::start(Mode::Reject);
        let mut command = command();
        command.args(["index", "--index-url", "https://index.invalid/simple/"]);
        if name.eq_ignore_ascii_case("https_proxy") {
            command.env("ALL_PROXY", "http://127.0.0.1:1");
        }
        let output = command
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env(name, proxy.url())
            .output()
            .unwrap();
        // Deliberately reject the tunnel: prove routing without disabling TLS
        // validation or needing an Internet endpoint in the test suite.
        assert!(!output.status.success());
        let requests = proxy.requests();
        assert_eq!(
            requests.len(),
            1,
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(requests[0].starts_with("CONNECT index.invalid:443 HTTP/1.1\r\n"));
    }
}

#[test]
fn https_proxy_is_used_by_download_workers() {
    for name in ["HTTPS_PROXY", "https_proxy"] {
        let origin = Server::start(Mode::Pipeline {
            proxy: false,
            secure_artifacts: true,
        });
        let proxy = Server::start(Mode::Reject);
        let root = tempdir().unwrap();
        let output = mirror(
            root.path(),
            &format!("{}/simple/", origin.url()),
            &format!("{}/simple/", origin.url()),
        )
        .env(name, proxy.url())
        .output()
        .unwrap();
        assert!(!output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            report["inspection"]["errors"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(report["downloads"]["progress"]["failed"], 1);
        let requests = proxy.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("CONNECT files.invalid:443 HTTP/1.1\r\n"));
    }
}

#[test]
fn proxy_url_credentials_are_used_for_http_requests_and_connect() {
    let proxy = Server::start(Mode::Pipeline {
        proxy: true,
        secure_artifacts: false,
    });
    let root = tempdir().unwrap();
    let output = mirror(
        root.path(),
        "http://index.invalid/simple/",
        "http://metadata.invalid/simple/",
    )
    .env("HTTP_PROXY", format!("http://user:pass@{}", proxy.address))
    .output()
    .unwrap();
    assert_downloaded(&output, root.path());
    for request in proxy.requests() {
        assert!(
            request
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic dxnlcjpwyxnz\r\n")
        );
    }
    let tunnel = Server::start(Mode::Reject);
    let output = command()
        .args(["index", "--index-url", "https://index.invalid/simple/"])
        .env(
            "HTTPS_PROXY",
            format!("http://user:pass@{}", tunnel.address),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        tunnel.requests()[0]
            .to_ascii_lowercase()
            .contains("proxy-authorization: basic dxnlcjpwyxnz\r\n")
    );
}

#[test]
fn failed_proxy_does_not_fall_back_to_a_direct_connection() {
    let origin = Server::start(Mode::Pipeline {
        proxy: false,
        secure_artifacts: false,
    });
    let proxy = Server::start(Mode::Reject);
    let output = command()
        .args(["index", "--index-url", &format!("{}/simple/", origin.url())])
        .env("HTTP_PROXY", proxy.url())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(proxy.requests().len(), 1);
    assert!(origin.requests().is_empty());
}
