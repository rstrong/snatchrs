use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tempfile::tempdir;

struct State {
    root_serial: Option<u64>,
    project_serial: Option<u64>,
    header_serial: Option<u64>,
    metadata_status: u16,
    index_status: u16,
    artifact_status: u16,
    artifact_requests: usize,
    metadata_requests: usize,
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
            root_serial: Some(100),
            project_serial: Some(100),
            header_serial: None,
            metadata_status: 200,
            index_status: 200,
            artifact_status: 200,
            artifact_requests: 0,
            metadata_requests: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let serving = state.clone();
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
                let mut header = String::new();
                let (status, body) = if path == "/simple/" || path == "/other-simple/" {
                    let mut project = serde_json::json!({"name": "cache-fixture"});
                    if let Some(serial) = state.root_serial {
                        project["_last-serial"] = serial.into();
                    }
                    (
                        state.index_status,
                        serde_json::json!({"meta": {"api-version": "1.4"}, "projects": [project]})
                            .to_string(),
                    )
                } else if path == "/artifact" {
                    state.artifact_requests += 1;
                    (state.artifact_status, "hello".to_owned())
                } else {
                    assert!(path.ends_with("/cache-fixture/"), "{path}");
                    state.metadata_requests += 1;
                    if let Some(serial) = state.header_serial {
                        header = format!("X-PyPI-Last-Serial: {serial}\r\n");
                    }
                    let mut meta = serde_json::json!({"api-version": "1.4"});
                    if let Some(serial) = state.project_serial {
                        meta["_last-serial"] = serial.into();
                    }
                    (
                        state.metadata_status,
                        serde_json::json!({
                            "name": "cache-fixture", "meta": meta, "versions": ["1.0"],
                            "files": [{"filename": "cache_fixture-1.0.tar.gz", "size": 5,
                                "url": format!("{origin}/artifact"),
                                "hashes": {"sha256": format!("{:x}", Sha256::digest(b"hello"))}}]
                        })
                        .to_string(),
                    )
                };
                drop(state);
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{header}\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            base,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> usize {
        self.state.lock().unwrap().metadata_requests
    }

    fn command(&self, root: &Path, mode: &str, extra: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_snatchrs"));
        command.args([mode]).arg(root);
        if !extra.contains(&"--package") {
            command.args(["--sample", "1"]);
        }
        if !extra.contains(&"--index-url") {
            command.args(["--index-url", &format!("{}/simple/", self.base)]);
        }
        command
            .args(["--json", "--quiet", "--attempts", "1"])
            .args(extra)
            .env("NO_PROXY", "*")
            .env("no_proxy", "*")
            .output()
            .unwrap()
    }

    fn run(&self, root: &Path, mode: &str, extra: &[&str], fetches: usize) -> serde_json::Value {
        let before = self.requests();
        let output = self.command(root, mode, extra);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(self.requests() - before, fetches);
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
fn unchanged_serial_reuses_metadata_but_still_repairs_local_files() {
    let server = Server::start();
    let root = tempdir().unwrap();
    server.run(root.path(), "check", &[], 1);
    let cached = root.path().join("metadata/cache-fixture.json");
    let original = fs::read(&cached).unwrap();
    let report = server.run(root.path(), "mirror", &[], 0);
    assert_eq!(report["downloads"]["progress"]["downloaded"], 1);
    assert_eq!(fs::read(&cached).unwrap(), original);
    let artifact = root
        .path()
        .join("packages/cache-fixture/cache_fixture-1.0.tar.gz");
    fs::write(&artifact, b"bad").unwrap();
    let report = server.run(root.path(), "mirror", &[], 0);
    assert_eq!(report["inspection"]["summary"]["size_mismatch"], 1);
    assert_eq!(report["downloads"]["progress"]["downloaded"], 1);
    assert_eq!(fs::read(&artifact).unwrap(), b"hello");
    fs::remove_file(&artifact).unwrap();
    let report = server.run(root.path(), "mirror", &[], 0);
    assert_eq!(report["inspection"]["summary"]["missing"], 1);
    assert_eq!(report["final_check"]["matching_size"], 1);
    server.run(root.path(), "mirror", &["--refresh"], 1);
    server.run(root.path(), "check", &["--refresh"], 1);
    server.run(root.path(), "check", &[], 0);
    let mut state = server.state.lock().unwrap();
    state.root_serial = Some(101);
    state.project_serial = Some(101);
    drop(state);
    server.run(root.path(), "mirror", &[], 1);
    server.run(root.path(), "check", &[], 0);
}

#[test]
fn missing_or_mismatched_serials_never_skip_metadata_fetches() {
    let server = Server::start();
    let root = tempdir().unwrap();
    for (root_serial, project_serial) in
        [(None, Some(100)), (Some(100), None), (Some(100), Some(99))]
    {
        let mut state = server.state.lock().unwrap();
        state.root_serial = root_serial;
        state.project_serial = project_serial;
        drop(state);
        server.run(root.path(), "check", &["--refresh"], 1);
        server.run(root.path(), "check", &[], 1);
    }
    server.state.lock().unwrap().header_serial = Some(100);
    server.state.lock().unwrap().project_serial = None;
    server.run(root.path(), "check", &["--refresh"], 1);
    server.run(root.path(), "check", &[], 0);
    // Explicit selection has no fresh root-listing serial, so always fetch.
    server.run(root.path(), "check", &["--package", "cache-fixture"], 1);
    server.run(root.path(), "check", &["--package", "cache-fixture"], 1);
}

#[test]
fn invalid_caches_and_repository_changes_fetch_fresh_metadata() {
    let server = Server::start();
    let root = tempdir().unwrap();
    server.run(root.path(), "check", &[], 1);
    let cached = root.path().join("metadata/cache-fixture.json");
    let original: serde_json::Value = serde_json::from_slice(&fs::read(&cached).unwrap()).unwrap();
    fs::write(&cached, b"{truncated").unwrap();
    server.run(root.path(), "check", &[], 1);
    for field in ["metadata_source_url", "package_path", "metadata_path"] {
        let mut invalid = original.clone();
        invalid.as_object_mut().unwrap().remove(field);
        fs::write(&cached, invalid.to_string()).unwrap();
        server.run(root.path(), "check", &[], 1);
    }
    for field in ["local_path", "filename", "download_url"] {
        let mut invalid = original.clone();
        invalid["releases"][0]["files"][0][field] = match field {
            "download_url" => "file:///untrusted".into(),
            "filename" => "../escape".into(),
            _ => "/untrusted/file".into(),
        };
        fs::write(&cached, invalid.to_string()).unwrap();
        server.run(root.path(), "mirror", &[], 1);
    }
    let other_metadata = format!("{}/alternative/", server.base);
    server.run(
        root.path(),
        "check",
        &["--metadata-url", &other_metadata],
        1,
    );
    server.run(
        root.path(),
        "mirror",
        &["--metadata-url", &other_metadata],
        0,
    );
    let other_index = format!("{}/other-simple/", server.base);
    server.run(
        root.path(),
        "check",
        &[
            "--metadata-url",
            &other_metadata,
            "--index-url",
            &other_index,
        ],
        1,
    );
}

#[test]
fn failed_refresh_does_not_overwrite_or_silently_reuse_the_cache() {
    let server = Server::start();
    let root = tempdir().unwrap();
    server.run(root.path(), "check", &[], 1);
    let cached = root.path().join("metadata/cache-fixture.json");
    let original = fs::read(&cached).unwrap();
    server.state.lock().unwrap().metadata_status = 503;
    let before = server.requests();
    let output = server.command(root.path(), "mirror", &["--refresh"]);
    assert!(!output.status.success());
    assert_eq!(server.requests(), before + 1);
    assert_eq!(fs::read(&cached).unwrap(), original);
    server.state.lock().unwrap().root_serial = Some(101);
    let output = server.command(root.path(), "check", &[]);
    assert!(!output.status.success());
    assert_eq!(server.requests(), before + 2);
    assert_eq!(fs::read(&cached).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn cached_metadata_does_not_bypass_symlink_directory_checks() {
    let server = Server::start();
    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    server.run(root.path(), "check", &[], 1);
    let package_path = root.path().join("packages/cache-fixture");
    fs::remove_dir(&package_path).unwrap();
    std::os::unix::fs::symlink(outside.path(), &package_path).unwrap();
    let before = server.requests();
    let output = server.command(root.path(), "mirror", &[]);
    assert!(!output.status.success());
    assert_eq!(server.requests(), before + 1);
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn project_404_is_an_outcome_and_invalidates_only_metadata_cache() {
    let server = Server::start();
    let root = tempdir().unwrap();
    server.run(root.path(), "mirror", &[], 1);
    let artifact = root
        .path()
        .join("packages/cache-fixture/cache_fixture-1.0.tar.gz");
    let cached = root.path().join("metadata/cache-fixture.json");
    server.state.lock().unwrap().metadata_status = 404;
    // Force observation of a deletion even if a stale root index still reports
    // the old serial. Subsequent runs must not reuse the now-invalid cache.
    for (mode, extra) in [
        ("mirror", vec!["--refresh"]),
        ("check", vec![]),
        ("mirror", vec![]),
    ] {
        let report = server.run(root.path(), mode, &extra, 1);
        let inspection = if mode == "mirror" {
            &report["inspection"]
        } else {
            &report
        };
        assert_eq!(inspection["error_count"], 0);
        assert_eq!(inspection["unavailable_count"], 1);
        assert_eq!(inspection["summary"]["packages"], 0);
        assert!(
            fs::read_to_string(
                root.path()
                    .join(inspection["errors_path"].as_str().unwrap())
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            fs::read_to_string(
                root.path()
                    .join(inspection["packages_path"].as_str().unwrap())
            )
            .unwrap()
            .is_empty()
        );
        let records = fs::read_to_string(
            root.path()
                .join(inspection["unavailable_path"].as_str().unwrap()),
        )
        .unwrap();
        assert_eq!(records.lines().count(), 1);
        let record: serde_json::Value =
            serde_json::from_str(records.lines().next().unwrap()).unwrap();
        assert_eq!(record["name"], "cache-fixture");
        assert_eq!(record["status"], "not_found");
        assert_eq!(record["http_status"], 404);
        assert_eq!(
            record["metadata_url"],
            format!("{}/simple/cache-fixture/", server.base)
        );
        if mode == "mirror" {
            assert_eq!(report["downloads"]["progress"]["failed"], 0);
            assert_eq!(report["downloads"]["progress"]["total_files"], 0);
            assert_eq!(
                report["downloads"]["progress"]["discovery"]["checked_packages"],
                1
            );
            assert_eq!(
                report["downloads"]["progress"]["discovery"]["unavailable"],
                1
            );
            assert_eq!(report["downloads"]["progress"]["discovery"]["errors"], 0);
        }
        assert!(!cached.exists());
        assert_eq!(fs::read(&artifact).unwrap(), b"hello");
    }
    server.state.lock().unwrap().metadata_status = 200;
    let report = server.run(root.path(), "mirror", &[], 1);
    assert_eq!(report["inspection"]["unavailable_count"], 0);
    assert_eq!(report["final_check"]["matching_size"], 1);
    assert!(cached.is_file());
}

#[test]
fn other_metadata_errors_and_non_project_404s_still_fail() {
    let server = Server::start();
    let root = tempdir().unwrap();
    for status in [403, 410, 500] {
        server.state.lock().unwrap().metadata_status = status;
        for mode in ["check", "mirror"] {
            let before = server.requests();
            let output = server.command(root.path(), mode, &[]);
            assert!(!output.status.success());
            assert_eq!(server.requests(), before + 1);
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let inspection = if mode == "mirror" {
                &report["inspection"]
            } else {
                &report
            };
            assert_eq!(inspection["unavailable_count"], 0);
            assert_eq!(inspection["error_count"], 1);
        }
    }
    server.state.lock().unwrap().metadata_status = 200;
    server.state.lock().unwrap().artifact_status = 404;
    let output = server.command(root.path(), "mirror", &[]);
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["inspection"]["error_count"], 0);
    assert_eq!(report["inspection"]["unavailable_count"], 0);
    assert_eq!(report["downloads"]["progress"]["failed"], 1);
    assert_eq!(server.state.lock().unwrap().artifact_requests, 1);
    server.state.lock().unwrap().index_status = 404;
    let before = server.requests();
    let output = server.command(root.path(), "mirror", &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("404"));
    assert!(output.stdout.is_empty());
    assert_eq!(server.requests(), before);
}

fn assert_relative_paths(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if key.ends_with("_path") || key == "mirror_root" {
                    let path = Path::new(value.as_str().unwrap());
                    assert!(!path.is_absolute(), "{key}: {path:?}");
                    assert!(
                        !path
                            .components()
                            .any(|part| matches!(part, std::path::Component::ParentDir))
                    );
                }
                assert_relative_paths(value);
            }
        }
        serde_json::Value::Array(values) => values.iter().for_each(assert_relative_paths),
        _ => {}
    }
}

#[test]
fn moving_a_mirror_preserves_reports_cache_hits_and_download_repairs() {
    let server = Server::start();
    let parent = tempdir().unwrap();
    let original_root = parent.path().join("original");
    let moved_root = parent.path().join("moved");
    let report = server.run(&original_root, "mirror", &[], 1);
    assert_eq!(report["inspection"]["mirror_root"], ".");
    assert_relative_paths(&report);
    fs::rename(&original_root, &moved_root).unwrap();
    // The earlier summary and JSONL links remain valid without rewriting them.
    for field in ["packages_path", "errors_path", "unavailable_path"] {
        assert!(
            moved_root
                .join(report["inspection"][field].as_str().unwrap())
                .is_file()
        );
    }
    let packages = fs::read_to_string(
        moved_root.join(report["inspection"]["packages_path"].as_str().unwrap()),
    )
    .unwrap();
    assert_relative_paths(&serde_json::from_str(packages.lines().next().unwrap()).unwrap());
    let outcomes =
        fs::read_to_string(moved_root.join(report["downloads"]["outcomes_path"].as_str().unwrap()))
            .unwrap();
    let outcome: serde_json::Value =
        serde_json::from_str(outcomes.lines().next().unwrap()).unwrap();
    assert_relative_paths(&outcome);
    assert_eq!(
        outcome["task"]["local_path"],
        "packages/cache-fixture/cache_fixture-1.0.tar.gz"
    );
    let cached = moved_root.join("metadata/cache-fixture.json");
    let cache_bytes = fs::read(&cached).unwrap();
    assert_relative_paths(&serde_json::from_slice(&cache_bytes).unwrap());
    let report = server.run(&moved_root, "mirror", &[], 0);
    assert_eq!(report["final_check"]["matching_size"], 1);
    assert_eq!(report["downloads"]["progress"]["downloaded"], 0);
    assert_eq!(fs::read(&cached).unwrap(), cache_bytes);
    fs::remove_file(moved_root.join("packages/cache-fixture/cache_fixture-1.0.tar.gz")).unwrap();
    let report = server.run(&moved_root, "mirror", &[], 0);
    assert_eq!(report["downloads"]["progress"]["downloaded"], 1);
    assert_eq!(report["final_check"]["matching_size"], 1);
    assert!(!original_root.exists());

    // Old absolute-path caches are refreshed, never used to write into the old
    // location, and existing distributions in the current mirror are retained.
    let mut legacy: serde_json::Value = serde_json::from_slice(&cache_bytes).unwrap();
    for field in ["metadata_path", "package_path"] {
        legacy[field] = original_root
            .join(legacy[field].as_str().unwrap())
            .to_str()
            .unwrap()
            .into();
    }
    for release in legacy["releases"].as_array_mut().unwrap() {
        for file in release["files"].as_array_mut().unwrap() {
            file["local_path"] = original_root
                .join(file["local_path"].as_str().unwrap())
                .to_str()
                .unwrap()
                .into();
        }
    }
    fs::write(&cached, legacy.to_string()).unwrap();
    let report = server.run(&moved_root, "mirror", &[], 1);
    assert_eq!(report["downloads"]["progress"]["downloaded"], 0);
    assert_relative_paths(&serde_json::from_slice(&fs::read(cached).unwrap()).unwrap());
    assert!(!original_root.exists());
}
