use sha2::{Digest, Sha256};
use snatchrs::{
    download::{DownloadOptions, DownloadTask, download_all},
    mirror::MirrorLayout,
};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;

fn request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut data = Vec::new();
    loop {
        let mut buffer = [0; 1024];
        let size = stream.read(&mut buffer).unwrap();
        assert!(size > 0);
        data.extend_from_slice(&buffer[..size]);
        if data.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(data).unwrap()
}

fn respond(stream: &mut TcpStream, status: &str, body: &[u8], headers: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
}

fn task(root: &Path, base: &str, filename: &str, body: &[u8]) -> DownloadTask {
    DownloadTask {
        package: "test".into(),
        filename: filename.into(),
        download_url: format!("{base}/{filename}").parse().unwrap(),
        local_path: root.join(filename),
        size: body.len() as u64,
        sha256: Some(format!("{:x}", Sha256::digest(body))),
    }
}

fn options(concurrency: usize, attempts: u32) -> DownloadOptions {
    DownloadOptions {
        concurrency,
        attempts,
        min_free: 0,
        read_timeout: Duration::from_secs(5),
    }
}

#[test]
fn bounded_workers_download_raw_bytes_and_verify_hashes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let body = vec![42u8; 524288];
    let server_body = body.clone();
    let server_active = active.clone();
    let server_peak = peak.clone();
    let server = thread::spawn(move || {
        thread::scope(|scope| {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let body = &server_body;
                let active = &server_active;
                let peak = &server_peak;
                scope.spawn(move || {
                    let request = request(&mut stream);
                    assert!(
                        request
                            .to_lowercase()
                            .contains("accept-encoding: identity\r\n")
                    );
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(count, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(150));
                    // A gzip header must not cause artifact decompression.
                    respond(&mut stream, "200 OK", body, "Content-Encoding: gzip\r\n");
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
    });
    let directory = tempdir().unwrap();
    let tasks: Vec<_> = (0..4)
        .map(|n| task(directory.path(), &base, &format!("file-{n}.tar.gz"), &body))
        .collect();
    let mut heartbeats = 0;
    let report = download_all(directory.path(), &tasks, options(2, 1), |_| {
        heartbeats += 1;
    })
    .unwrap();
    server.join().unwrap();
    assert!(heartbeats >= 3);
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    assert_eq!(report.progress.downloaded, 4);
    assert_eq!(report.progress.failed, 0);
    assert_eq!(report.progress.verified_bytes, body.len() as u64 * 4);
    assert_eq!(report.progress.transferred_bytes, body.len() as u64 * 4);
    assert!(
        report
            .outcomes
            .iter()
            .all(|outcome| outcome.sha256_verified)
    );
    for task in tasks {
        assert_eq!(fs::read(task.local_path).unwrap(), body);
    }
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 4);
}

#[test]
fn bad_hash_and_wrong_size_preserve_existing_files_and_clean_temporary_files() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = request(&mut stream);
            let body: &[u8] = if request.contains("bad-hash") {
                b"wrong!"
            } else {
                b"short"
            };
            respond(&mut stream, "200 OK", body, "");
        }
    });
    let directory = tempdir().unwrap();
    let tasks = [
        task(directory.path(), &base, "bad-hash.whl", b"right!"),
        task(directory.path(), &base, "bad-size.whl", b"longer"),
    ];
    for task in &tasks {
        fs::write(&task.local_path, b"old").unwrap();
    }
    let report = download_all(directory.path(), &tasks, options(1, 1), |_| {}).unwrap();
    server.join().unwrap();
    assert_eq!(report.progress.failed, 2);
    assert_eq!(report.progress.verified_bytes, 0);
    assert!(
        report.outcomes[0]
            .error
            .as_ref()
            .unwrap()
            .contains("SHA256 mismatch")
    );
    assert!(
        report.outcomes[1]
            .error
            .as_ref()
            .unwrap()
            .contains("Content-Length")
    );
    for task in tasks {
        assert_eq!(fs::read(task.local_path).unwrap(), b"old");
    }
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[test]
fn retries_http_failure_and_handles_empty_files() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for status in ["503 Service Unavailable", "200 OK"] {
            let (mut stream, _) = listener.accept().unwrap();
            request(&mut stream);
            respond(&mut stream, status, b"", "");
        }
    });
    let directory = tempdir().unwrap();
    let task = task(directory.path(), &base, "empty.whl", b"");
    let report = download_all(directory.path(), &[task], options(1, 2), |_| {}).unwrap();
    server.join().unwrap();
    assert_eq!(report.progress.downloaded, 1);
    assert_eq!(report.outcomes[0].attempts, 2);
    assert_eq!(
        fs::metadata(directory.path().join("empty.whl"))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn empty_queue_limits_and_mirror_lock() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let lock = layout.lock().unwrap();
    assert!(layout.lock().is_err());
    drop(lock);
    assert!(layout.lock().is_ok());
    let report = download_all(directory.path(), &[], options(8, 1), |_| {}).unwrap();
    assert_eq!(report.progress.total_files, 0);
    assert!(report.progress.workers.is_empty());
    assert!(download_all(directory.path(), &[], options(0, 1), |_| {}).is_err());
    assert!(download_all(directory.path(), &[], options(65, 1), |_| {}).is_err());
    let mut huge = task(directory.path(), "http://127.0.0.1:1", "huge.whl", b"");
    huge.size = u64::MAX;
    let report = download_all(directory.path(), &[huge], options(1, 1), |_| {}).unwrap();
    assert_eq!(report.progress.failed, 1);
    assert!(
        report
            .progress
            .stopped
            .as_deref()
            .unwrap()
            .contains("Not enough disk space")
    );
    let outside = tempdir().unwrap();
    let escaped = task(outside.path(), "http://127.0.0.1:1", "outside.whl", b"");
    assert!(
        download_all(directory.path(), &[escaped], options(1, 1), |_| {})
            .unwrap_err()
            .to_string()
            .contains("escapes mirror root")
    );
}

#[test]
fn truncated_http_response_is_never_published() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        request(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nshort")
            .unwrap();
    });
    let directory = tempdir().unwrap();
    let task = task(directory.path(), &base, "truncated.whl", b"expected!!");
    let report = download_all(directory.path(), &[task], options(1, 1), |_| {}).unwrap();
    server.join().unwrap();
    assert_eq!(report.progress.failed, 1);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn cli_downloads_then_skips_existing_file_on_repeat() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let body = b"sample artifact bytes";
    let metadata = serde_json::json!({
        "name": "test", "meta": {"api-version": "1.4"}, "versions": ["1.0"], "files": [{
            "filename": "test-1.0.tar.gz", "url": format!("{base}/artifact"), "size": body.len(),
            "hashes": {"sha256": format!("{:x}", Sha256::digest(body))}, "yanked": true
        }]
    })
    .to_string();
    let server = thread::spawn(move || {
        let mut artifacts = 0;
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = request(&mut stream);
            if request.starts_with("GET /artifact ") {
                artifacts += 1;
                respond(&mut stream, "200 OK", body, "");
            } else {
                respond(&mut stream, "200 OK", metadata.as_bytes(), "");
            }
        }
        artifacts
    });
    let directory = tempdir().unwrap();
    for expected_downloads in [1, 0] {
        let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
            .args([
                "mirror",
                "--min-free",
                "0",
                "--package",
                "test",
                "--metadata-url",
                &format!("{base}/simple/"),
                "--concurrency",
                "2",
                "--json",
            ])
            .arg(directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["downloads"]["progress"]["downloaded"],
            expected_downloads
        );
        assert_eq!(report["final_check"]["matching_size"], 1);
        assert_eq!(report["final_check"]["missing"], 0);
    }
    assert_eq!(server.join().unwrap(), 1);
    assert_eq!(
        fs::read(directory.path().join("packages/test/test-1.0.tar.gz")).unwrap(),
        body
    );
    assert!(directory.path().join("mirror-report.json").is_file());
    assert!(directory.path().join("mirror-progress.json").is_file());
}

#[test]
fn mirror_respects_samples_above_one_hundred_and_smaller_indexes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let projects: Vec<_> = (0..115)
        .map(|n| serde_json::json!({"name": format!("sample-{n}")}))
        .collect();
    let index = Arc::new(
        serde_json::json!({"meta": {"api-version": "1.0"}, "projects": projects}).to_string(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let server_base = base.clone();
    let server = thread::spawn(move || {
        thread::scope(|scope| {
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let base = &server_base;
                        let index = &index;
                        scope.spawn(move || {
                            let request = request(&mut stream);
                            let path = request.split_whitespace().nth(1).unwrap();
                            if path == "/simple/" {
                                respond(&mut stream, "200 OK", index.as_bytes(), "");
                            } else if path.starts_with("/simple/") {
                                let name = path.split('/').nth(2).unwrap();
                                let metadata = serde_json::json!({
                                    "name": name, "meta": {"api-version": "1.4"}, "versions": ["1.0"], "files": [{
                                        "filename": format!("{name}-1.0.tar.gz"),
                                        "url": format!("{base}/files/{name}-1.0.tar.gz"),
                                        "size": 1, "hashes": {"sha256": format!("{:x}", Sha256::digest(b"x"))}
                                    }]
                                }).to_string();
                                respond(&mut stream, "200 OK", metadata.as_bytes(), "");
                            } else {
                                assert!(path.starts_with("/files/"));
                                respond(&mut stream, "200 OK", b"x", "");
                            }
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
    let directory = tempdir().unwrap();
    let outputs: Vec<_> = [101, 10000]
        .into_iter()
        .map(|count| {
            Command::new(env!("CARGO_BIN_EXE_snatchrs"))
                .args([
                    "mirror",
                    "--min-free",
                    "0",
                    "--sample",
                    &count.to_string(),
                    "--seed",
                    "42",
                    "--concurrency",
                    "4",
                    "--json",
                    "--index-url",
                    &format!("{base}/simple/"),
                ])
                .arg(directory.path())
                .output()
                .unwrap()
        })
        .collect();
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    for (output, expected, downloaded) in [
        (outputs.first().unwrap(), 101, 101),
        (outputs.last().unwrap(), 115, 14),
    ] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["inspection"]["selected_packages"]
                .as_array()
                .unwrap()
                .len(),
            expected
        );
        assert_eq!(report["final_check"]["matching_size"], expected);
        assert_eq!(report["downloads"]["progress"]["downloaded"], downloaded);
        assert_eq!(report["downloads"]["progress"]["failed"], 0);
        assert!(report["inspection"].get("packages").is_none());
        assert!(report["downloads"].get("outcomes").is_none());
        let checked = fs::read_to_string(
            directory
                .path()
                .join(report["inspection"]["packages_path"].as_str().unwrap()),
        )
        .unwrap();
        assert_eq!(checked.lines().count(), expected);
        for line in checked.lines() {
            let package: snatchrs::mirror::CheckedPackage = serde_json::from_str(line).unwrap();
            assert_eq!(package.files.len(), 1);
        }
        let outcomes = fs::read_to_string(
            directory
                .path()
                .join(report["downloads"]["outcomes_path"].as_str().unwrap()),
        )
        .unwrap();
        assert_eq!(outcomes.lines().count(), downloaded as usize);
        for line in outcomes.lines() {
            let outcome: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(outcome["downloaded"], true);
            assert_eq!(outcome["sha256_verified"], true);
        }
        // Detailed file metadata stays out of the summary and JSON stdout.
        assert!(output.stdout.len() < 16_384);
    }
    assert_eq!(
        fs::read_dir(directory.path().join("packages"))
            .unwrap()
            .count(),
        115
    );
}

#[test]
fn downloads_and_progress_start_before_later_metadata_finishes() {
    let directory = tempdir().unwrap();
    let target = directory.path().join("packages/early/early-1.0.whl");
    let progress_path = directory.path().join("mirror-progress.json");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let body = b"overlapped artifact";
    let early = serde_json::json!({"name":"early","meta":{"api-version":"1.4"},"versions":["1.0"],"files":[{
        "filename":"early-1.0.whl","url":format!("{base}/artifact"),"size":body.len(),
        "hashes":{"sha256":format!("{:x}",Sha256::digest(body))}
    }]})
    .to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let overlapped = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let server_overlap = overlapped.clone();
    let server = thread::spawn(move || {
        thread::scope(|scope| {
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let early = &early;
                        let target = &target;
                        let progress_path = &progress_path;
                        let overlapped = &server_overlap;
                        scope.spawn(move || {
                            let request = request(&mut stream);
                            if request.starts_with("GET /simple/early/ ") {
                                respond(&mut stream, "200 OK", early.as_bytes(), "");
                            } else if request.starts_with("GET /artifact ") {
                                respond(&mut stream, "200 OK", body, "");
                            } else {
                                assert!(request.starts_with("GET /simple/later/ "));
                                let deadline = Instant::now() + Duration::from_secs(5);
                                // Keep metadata unavailable until the first artifact is
                                // fully published AND its live progress has been saved.
                                while Instant::now() < deadline {
                                    let snapshot = fs::read(progress_path).ok().and_then(|bytes| {
                                        serde_json::from_slice::<serde_json::Value>(&bytes).ok()
                                    });
                                    if target.is_file()
                                        && snapshot.is_some_and(|p| {
                                            p["downloaded"] == 1
                                                && p["discovery"]["checked_packages"] == 1
                                                && p["discovery"]["finished"] == false
                                        })
                                    {
                                        overlapped.store(true, Ordering::Release);
                                        break;
                                    }
                                    thread::sleep(Duration::from_millis(10));
                                }
                                respond(
                                    &mut stream,
                                    "200 OK",
                                    br#"{"name":"later","meta":{"api-version":"1.4"},"versions":[],"files":[]}"#,
                                    "",
                                );
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
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .args([
            "mirror",
            "--min-free",
            "0",
            "--package",
            "early",
            "--package",
            "later",
            "--metadata-url",
            &format!("{base}/simple/"),
            "--concurrency",
            "2",
            "--json",
        ])
        .arg(directory.path())
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
        overlapped.load(Ordering::Acquire),
        "first download and live progress must precede later metadata"
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["downloads"]["progress"]["discovery"]["checked_packages"],
        2
    );
    assert_eq!(
        report["downloads"]["progress"]["discovery"]["finished"],
        true
    );
    assert_eq!(report["final_check"]["matching_size"], 1);
}

#[test]
fn streaming_queue_deduplicates_rejects_conflicts_and_drains_after_producer_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        request(&mut stream);
        respond(&mut stream, "200 OK", b"x", "");
    });
    let directory = tempdir().unwrap();
    let item = task(directory.path(), &base, "one.whl", b"x");
    let result: anyhow::Result<((), _)> = snatchrs::download::download_stream(
        directory.path(),
        options(1, 1),
        |queue| {
            queue.enqueue(item.clone())?;
            queue.enqueue(item.clone())?;
            let mut conflict = item;
            conflict.size = 99;
            assert!(queue.enqueue(conflict).is_err());
            anyhow::bail!("intentional producer failure")
        },
        |_| {},
    );
    server.join().unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("intentional producer failure")
    );
    assert_eq!(fs::read(directory.path().join("one.whl")).unwrap(), b"x");
}

#[test]
fn checking_can_enqueue_every_job_while_download_workers_are_blocked() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let released = Arc::new(AtomicBool::new(false));
    let server_release = released.clone();
    let server = thread::spawn(move || {
        for _ in 0..20 {
            let (mut stream, _) = listener.accept().unwrap();
            request(&mut stream);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !server_release.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            respond(&mut stream, "200 OK", b"x", "");
        }
    });
    let directory = tempdir().unwrap();
    let accepted = AtomicUsize::new(0);
    let mut checker_finished_before_download = false;
    let (_, report) = snatchrs::download::download_stream(
        directory.path(),
        options(1, 1),
        |queue| {
            for index in 0..20 {
                queue.enqueue(task(
                    directory.path(),
                    &base,
                    &format!("file-{index}.whl"),
                    b"x",
                ))?;
                accepted.fetch_add(1, Ordering::Release);
            }
            Ok(())
        },
        |progress| {
            if !released.load(Ordering::Acquire) && progress.discovery.finished {
                checker_finished_before_download = accepted.load(Ordering::Acquire) == 20
                    && progress.downloaded == 0
                    && progress.total_files == 20;
                released.store(true, Ordering::Release);
            } else if progress.elapsed_seconds > 2.0 {
                // Let a regression finish instead of hanging on its full queue.
                released.store(true, Ordering::Release);
            }
        },
    )
    .unwrap();
    server.join().unwrap();
    assert!(
        checker_finished_before_download,
        "checking must not wait for a download slot to become free"
    );
    assert_eq!(report.progress.downloaded, 20);
    assert!(report.progress.discovery.finished);
}

#[test]
fn oversized_transfer_stops_without_retrying_or_leaving_partial_files() {
    let directory = tempdir().unwrap();
    let mut huge = task(directory.path(), "http://127.0.0.1:1", "huge.whl", b"");
    huge.size = u64::MAX;
    let (checked, report) = snatchrs::download::download_stream(
        directory.path(),
        options(1, 5),
        |queue| {
            queue.enqueue(huge)?;
            Ok("producer finished")
        },
        |_| {},
    )
    .unwrap();
    assert_eq!(checked, "producer finished");
    assert!(report.progress.stopped.is_some());
    assert_eq!(report.progress.failed, 1);
    assert_eq!(report.outcomes[0].attempts, 1);
    assert!(
        report.outcomes[0]
            .error
            .as_ref()
            .unwrap()
            .contains("Not enough disk space")
    );
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn streamed_outcomes_are_not_retained_and_sink_errors_drain_workers() {
    for fail_sink in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for _ in 0..32 {
                let (mut stream, _) = listener.accept().unwrap();
                request(&mut stream);
                respond(&mut stream, "200 OK", b"x", "");
            }
        });
        let directory = tempdir().unwrap();
        let mut recorded = 0;
        let result = snatchrs::download::download_stream_to(
            directory.path(),
            options(4, 1),
            |queue| {
                for i in 0..32 {
                    queue.enqueue(task(directory.path(), &base, &format!("{i}.whl"), b"x"))?;
                }
                Ok(32)
            },
            |_| {},
            |outcome| {
                recorded += 1;
                assert!(!outcome.task.local_path.is_absolute());
                assert!(outcome.downloaded);
                assert!(outcome.sha256_verified);
                if fail_sink {
                    anyhow::bail!("simulated report disk failure");
                }
                Ok(())
            },
        );
        server.join().unwrap();
        for i in 0..32 {
            assert_eq!(
                fs::read(directory.path().join(format!("{i}.whl"))).unwrap(),
                b"x"
            );
        }
        if fail_sink {
            assert_eq!(recorded, 1);
            assert!(format!("{:#}", result.unwrap_err()).contains("simulated report disk failure"));
        } else {
            let (produced, report) = result.unwrap();
            assert_eq!(produced, 32);
            assert_eq!(recorded, 32);
            assert_eq!(report.progress.downloaded, 32);
            assert!(report.outcomes.is_empty());
        }
    }
}

#[test]
fn relative_download_paths_reject_parent_traversal_and_symlink_escape() {
    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    for path in ["../escape.whl", "nested/../../escape.whl", ""] {
        let mut item = task(root.path(), "http://127.0.0.1:1", "escape.whl", b"x");
        item.local_path = path.into();
        let result = snatchrs::download::download_stream_to(
            root.path(),
            options(1, 1),
            |queue| queue.enqueue(item),
            |_| {},
            |_| Ok(()),
        );
        assert!(result.is_err(), "accepted {path}");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        let mut item = task(root.path(), "http://127.0.0.1:1", "escape.whl", b"x");
        item.local_path = "linked/escape.whl".into();
        let result = snatchrs::download::download_stream_to(
            root.path(),
            options(1, 1),
            |queue| queue.enqueue(item),
            |_| {},
            |_| Ok(()),
        );
        assert!(result.is_err());
    }
    assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
}
