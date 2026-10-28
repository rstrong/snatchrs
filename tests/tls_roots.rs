//! Exercise native trust configuration in child processes, without mutating
//! this test process's environment or relying on the host's installed roots.
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    path::Path,
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
use tempfile::tempdir;

struct Server {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start() -> (Self, String) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let key = KeyPair::generate().unwrap();
        let params = CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
        let cert = params.signed_by(&key, &ca, &ca_key).unwrap();
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.der().clone()],
                    PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
                )
                .unwrap();
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // macOS/BSD accepted sockets inherit O_NONBLOCK.
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(3)))
                            .unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_secs(3)))
                            .unwrap();
                        let connection = ServerConnection::new(config.clone()).unwrap();
                        let mut tls = StreamOwned::new(connection, stream);
                        let mut request = Vec::new();
                        let mut buffer = [0; 1024];
                        while request.len() < 65536 {
                            match tls.read(&mut buffer) {
                                Ok(0) | Err(_) => break, // Untrusted CA: expected handshake failure.
                                Ok(size) => request.extend_from_slice(&buffer[..size]),
                            }
                            if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                                assert!(request.starts_with(b"GET /simple/ HTTP/1.1\r\n"));
                                let body = include_str!("fixtures/index.json");
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.pypi.simple.v1+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                                let _ = tls.write_all(response.as_bytes());
                                break;
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        });
        (
            Self {
                address,
                stop,
                thread: Some(thread),
            },
            ca.pem(),
        )
    }

    fn command(&self, file: &Path, directory: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_snatchrs"));
        command.args([
            "--attempts",
            "1",
            "index",
            "--index-url",
            &format!("https://localhost:{}/simple/", self.address.port()),
            "--json",
            "--no-progress",
        ]);
        command
            .env("NO_PROXY", "*")
            .env("no_proxy", "*")
            .env("SSL_CERT_FILE", file)
            .env("SSL_CERT_DIR", directory);
        command
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn assert_trusted(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let index: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<_> = index["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| package["normalized_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["friendly-bard", "requests", "zope-interface"]);
}

#[test]
fn private_ca_requires_explicit_native_trust_configuration() {
    let (server, ca) = Server::start();
    let root = tempdir().unwrap();
    let empty_file = root.path().join("empty.pem");
    std::fs::write(&empty_file, "").unwrap();
    let empty_dir = root.path().join("empty");
    std::fs::create_dir(&empty_dir).unwrap();
    // Empty roots and native loading errors both leave client construction
    // working, but neither may trust this private server certificate.
    for file in [&empty_file, &root.path().join("absent.pem")] {
        let output = server.command(file, &empty_dir).output().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.to_ascii_lowercase().contains("certificate"),
            "{stderr}"
        );
    }

    let ca_file = root.path().join("ca.pem");
    std::fs::write(&ca_file, &ca).unwrap();
    // Only SSL_CERT_FILE is set: this must override the platform store on macOS too.
    assert_trusted(
        server
            .command(&ca_file, &empty_dir)
            .env_remove("SSL_CERT_DIR")
            .output()
            .unwrap(),
    );

    let ca_dir = root.path().join("certs");
    std::fs::create_dir(&ca_dir).unwrap();
    std::fs::write(ca_dir.join("private.pem"), ca).unwrap();
    // Only SSL_CERT_DIR is set: the file is discovered without a hash symlink.
    assert_trusted(
        server
            .command(&empty_file, &ca_dir)
            .env_remove("SSL_CERT_FILE")
            .output()
            .unwrap(),
    );
}
