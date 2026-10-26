use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;

// The first metadata response is held until a later project's result has been
// checked (and its artifact published in mirror mode). This detects both serial
// fetching and collectors that wait for input order despite parallel requests.
#[test]
fn metadata_limit_is_respected_and_fast_results_pass_a_blocked_first_project() {
    for mode in ["check", "mirror"] {
        let root = tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let slow_started = Arc::new(AtomicBool::new(false));
        let release_slow = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let server_stop = stop.clone();
        let server_observed = observed.clone();
        let server_peak = peak.clone();
        let server_requests = requests.clone();
        let mirror_root = root.path().to_path_buf();
        let origin = base.clone();
        let server = thread::spawn(move || {
            thread::scope(|scope| {
                while !server_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let slow_started = &slow_started;
                            let release_slow = &release_slow;
                            let observed = &server_observed;
                            let active = &active;
                            let peak = &server_peak;
                            let requests = &server_requests;
                            let root = &mirror_root;
                            let origin = &origin;
                            scope.spawn(move || {
                                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                                let mut request = Vec::new();
                                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                                    let mut buffer = [0; 1024];
                                    let size = stream.read(&mut buffer).unwrap();
                                    assert!(size > 0);
                                    request.extend_from_slice(&buffer[..size]);
                                }
                                let request = String::from_utf8(request).unwrap();
                                let path = request.split_whitespace().nth(1).unwrap();
                                let body = if path == "/artifact" {
                                    b"x".to_vec()
                                } else {
                                    let count = active.fetch_add(1, Ordering::AcqRel) + 1;
                                    peak.fetch_max(count, Ordering::AcqRel);
                                    requests.fetch_add(1, Ordering::AcqRel);
                                    let name = path.split('/').nth(2).unwrap();
                                    let deadline = Instant::now() + Duration::from_secs(4);
                                    if name == "slow" {
                                        slow_started.store(true, Ordering::Release);
                                        while !release_slow.load(Ordering::Acquire) && Instant::now() < deadline {
                                            thread::sleep(Duration::from_millis(2));
                                        }
                                    } else if name == "fast" {
                                        while !slow_started.load(Ordering::Acquire) && Instant::now() < deadline {
                                            thread::sleep(Duration::from_millis(2));
                                        }
                                    } else {
                                        assert_eq!(name, "tail");
                                        while Instant::now() < deadline {
                                            let ready = if mode == "mirror" {
                                                root.join("packages/fast/fast-1.0.tar.gz").is_file()
                                            } else {
                                                fs::read_dir(root.join("reports")).unwrap().any(|run| {
                                                    fs::read_to_string(run.unwrap().path().join("checked-packages.jsonl"))
                                                        .is_ok_and(|records| records.lines().any(|line| {
                                                            serde_json::from_str::<serde_json::Value>(line).is_ok_and(|p| p["metadata"]["package"]["name"] == "fast")
                                                        }))
                                                })
                                            };
                                            if ready && slow_started.load(Ordering::Acquire) && !release_slow.load(Ordering::Acquire) {
                                                observed.store(true, Ordering::Release);
                                                break;
                                            }
                                            thread::sleep(Duration::from_millis(2));
                                        }
                                        release_slow.store(true, Ordering::Release);
                                    }
                                    let files = if name == "fast" {
                                        vec![serde_json::json!({"filename":"fast-1.0.tar.gz", "url":format!("{origin}/artifact"), "size":1,
                                            "hashes":{"sha256":format!("{:x}", Sha256::digest(b"x"))}})]
                                    } else { vec![] };
                                    let body = serde_json::json!({"name":name,"meta":{"api-version":"1.4"},"versions":["1.0"],"files":files}).to_string().into_bytes();
                                    active.fetch_sub(1, Ordering::AcqRel);
                                    body
                                };
                                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                                stream.write_all(&body).unwrap();
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1))
                        }
                        Err(error) => panic!("{error}"),
                    }
                }
            });
        });
        let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
            .args([
                mode,
                "--package",
                "slow",
                "--package",
                "fast",
                "--package",
                "tail",
                "--metadata-concurrency",
                "2",
                "--attempts",
                "1",
                "--metadata-url",
                &format!("{base}/simple/"),
                "--json",
                "--quiet",
            ])
            .arg(root.path())
            .env("NO_PROXY", "*")
            .env("no_proxy", "*")
            .output()
            .unwrap();
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            observed.load(Ordering::Acquire),
            "fast result blocked behind the slow first project in {mode}"
        );
        assert_eq!(peak.load(Ordering::Acquire), 2);
        assert_eq!(requests.load(Ordering::Acquire), 3);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let inspection = if mode == "mirror" {
            &report["inspection"]
        } else {
            &report
        };
        assert_eq!(inspection["metadata_concurrency"], 2);
        assert_eq!(inspection["summary"]["packages"], 3);
        assert_eq!(inspection["error_count"], 0);
        let records = fs::read_to_string(
            root.path()
                .join(inspection["packages_path"].as_str().unwrap()),
        )
        .unwrap();
        assert_eq!(records.lines().count(), 3);
        let first: serde_json::Value =
            serde_json::from_str(records.lines().next().unwrap()).unwrap();
        assert_eq!(first["metadata"]["package"]["name"], "fast");
        if mode == "mirror" {
            assert_eq!(report["downloads"]["progress"]["downloaded"], 1);
            assert_eq!(report["final_check"]["matching_size"], 1);
        }
    }
}
