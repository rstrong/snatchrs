use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
};

use snatchrs::{
    Package, PackageIndex,
    mirror::{
        CheckSummary, DownloadQueueStub, FileState, MirrorLayout, PackageMetadata, check_package,
        inspect_file, sample_packages,
    },
};
use tempfile::tempdir;
use url::Url;

const FIXTURE: &str = include_str!("fixtures/metadata.json");

fn package() -> Package {
    Package {
        name: "Example_Pkg".into(),
        normalized_name: "example-pkg".into(),
        index_url: Url::parse("https://pypi.org/simple/example-pkg/").unwrap(),
        last_serial: Some(123),
    }
}

fn parse(input: &str, layout: &MirrorLayout) -> anyhow::Result<PackageMetadata> {
    PackageMetadata::from_reader(
        input.as_bytes(),
        package(),
        Url::parse("https://pypi.org/pypi/example-pkg/json").unwrap(),
        layout,
    )
}

#[test]
fn includes_all_versions_files_urls_sizes_and_paths() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let metadata = parse(FIXTURE, &layout).unwrap();
    assert_eq!(metadata.releases.len(), 3);
    assert!(metadata.releases[0].files.is_empty());
    let file = &metadata.releases[1].files[0];
    assert_eq!(metadata.releases[1].version, "1.0");
    assert_eq!(file.size, 4);
    assert_eq!(
        file.download_url.as_str(),
        "https://files.pythonhosted.org/packages/ab/example_pkg-1.0-py3-none-any.whl"
    );
    assert_eq!(
        file.local_path,
        layout
            .root
            .join("packages/example-pkg/example_pkg-1.0-py3-none-any.whl")
    );
    assert_eq!(file.sha256.as_deref(), Some("abc123"));
    assert_eq!(file.requires_python.as_deref(), Some(">=3.9"));
    assert!(metadata.releases[2].files[0].yanked);
    layout.save_metadata(&metadata).unwrap();
    assert!(metadata.package_path.is_dir());
    assert!(metadata.metadata_path.is_file());
    assert_eq!(fs::read_dir(&metadata.package_path).unwrap().count(), 0);
}

#[test]
fn checks_missing_matching_wrong_sizes_and_non_files_without_downloading() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let metadata = parse(FIXTURE, &layout).unwrap();
    layout.save_metadata(&metadata).unwrap();
    fs::write(&metadata.releases[1].files[0].local_path, b"1234").unwrap();
    fs::write(&metadata.releases[1].files[1].local_path, b"x").unwrap();
    fs::create_dir(&metadata.releases[2].files[1].local_path).unwrap();
    let mut summary = CheckSummary::default();
    let mut queue = DownloadQueueStub::default();
    let checked = check_package(metadata, &mut summary, &mut queue);
    assert_eq!(checked.files[0].state, FileState::MatchingSize);
    assert_eq!(
        checked.files[1].state,
        FileState::SizeMismatch { actual_size: 1 }
    );
    assert_eq!(checked.files[2].state, FileState::Missing);
    assert_eq!(checked.files[3].state, FileState::NotRegularFile);
    assert_eq!(
        (
            summary.packages,
            summary.releases,
            summary.files,
            summary.expected_bytes
        ),
        (1, 3, 4, 15)
    );
    assert_eq!(
        (
            summary.missing,
            summary.size_mismatch,
            summary.matching_size,
            summary.blocked
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(queue.candidates_seen, 2);
    assert!(!checked.files[2].local_path.exists());
    assert_eq!(fs::read(&checked.files[1].local_path).unwrap(), b"x");
}

#[test]
fn existing_zero_byte_file_matches_zero_expected_size() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let metadata = parse(FIXTURE, &layout).unwrap();
    layout.save_metadata(&metadata).unwrap();
    let file = &metadata.releases[2].files[1];
    fs::write(&file.local_path, b"").unwrap();
    assert_eq!(inspect_file(file), FileState::MatchingSize);
}

#[test]
fn rejects_unsafe_filenames_wrong_project_and_missing_sizes() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    for filename in [
        "../escape.whl",
        "/tmp/escape.whl",
        "..",
        "a\\b.whl",
        "C:escape.whl",
        "",
    ] {
        let mut value: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        value["releases"]["1.0"][0]["filename"] = filename.into();
        assert!(
            parse(&value.to_string(), &layout).is_err(),
            "accepted {filename}"
        );
    }
    assert!(parse(&FIXTURE.replace("Example_Pkg", "other"), &layout).is_err());
    assert!(parse(&FIXTURE.replace("\"size\": 4,", ""), &layout).is_err());
    assert!(parse(&FIXTURE.replace("\"size\": 4", "\"size\": -1"), &layout).is_err());
    assert!(
        parse(
            &FIXTURE.replace(
                "https://files.pythonhosted.org",
                "file://files.pythonhosted.org"
            ),
            &layout
        )
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn rejects_symlink_directories_and_does_not_follow_file_symlinks() {
    use std::os::unix::fs::symlink;
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let metadata = parse(FIXTURE, &layout).unwrap();
    let outside = tempdir().unwrap();
    symlink(outside.path(), &metadata.package_path).unwrap();
    assert!(layout.save_metadata(&metadata).is_err());
    fs::remove_file(&metadata.package_path).unwrap();
    layout.save_metadata(&metadata).unwrap();
    let file = &metadata.releases[1].files[0];
    symlink(outside.path().join("missing"), &file.local_path).unwrap();
    assert_eq!(inspect_file(file), FileState::NotRegularFile);
}

#[test]
fn random_sample_is_unique_and_reproducible() {
    let projects: Vec<_> = (0..30)
        .map(|n| serde_json::json!({"name": format!("package-{n}")}))
        .collect();
    let input =
        serde_json::json!({"meta": {"api-version": "1.0"}, "projects": projects}).to_string();
    let index = PackageIndex::from_reader(
        input.as_bytes(),
        Url::parse("https://pypi.org/simple/").unwrap(),
    )
    .unwrap();
    let names = |seed| {
        sample_packages(&index, 10, seed)
            .into_iter()
            .map(|p| p.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(names(42), names(42));
    assert_ne!(names(42), names(43));
    let mut unique = names(42);
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 10);
    assert_eq!(sample_packages(&index, 50, 42).len(), 30);
}

// Exercise the real CLI against a local HTTP server, with no artifact endpoint.
#[test]
fn cli_saves_metadata_and_report_and_continues_after_package_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/pypi/", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 1024];
                let size = stream.read(&mut bytes).unwrap();
                assert!(size > 0);
                request.extend_from_slice(&bytes[..size]);
                if request.windows(4).any(|chunk| chunk == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap();
            let (status, body) = if request.starts_with("GET /pypi/example-pkg/json ") {
                ("200 OK", FIXTURE)
            } else {
                ("404 Not Found", "missing")
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            requests.push(request);
        }
        requests
    });
    let directory = tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .args([
            "check",
            "--package",
            "gone",
            "--package",
            "Example_Pkg",
            "--metadata-url",
            &base,
            "--json",
            "--mirror",
        ])
        .arg(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["packages"], 1);
    assert_eq!(report["summary"]["releases"], 3);
    assert_eq!(report["summary"]["missing"], 4);
    assert_eq!(report["queue_stub"]["candidates_seen"], 4);
    assert_eq!(report["errors"][0]["name"], "gone");
    assert!(directory.path().join("metadata/example-pkg.json").is_file());
    assert!(directory.path().join("check-report.json").is_file());
    assert_eq!(
        fs::read_dir(directory.path().join("packages/example-pkg"))
            .unwrap()
            .count(),
        0
    );
    // Recheck the same shell after simulating one correct and one partial file.
    let package_path = directory.path().join("packages/example-pkg");
    fs::write(
        package_path.join("example_pkg-1.0-py3-none-any.whl"),
        b"1234",
    )
    .unwrap();
    fs::write(package_path.join("example_pkg-1.0.tar.gz"), b"x").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .args([
            "--quiet",
            "check",
            "--package",
            "Example_Pkg",
            "--metadata-url",
            &base,
            "--json",
            "--mirror",
        ])
        .arg(directory.path())
        .output()
        .unwrap();
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1]
            .to_lowercase()
            .contains("accept: application/json\r\n")
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["matching_size"], 1);
    assert_eq!(report["summary"]["size_mismatch"], 1);
    assert_eq!(report["summary"]["missing"], 2);
    assert_eq!(report["queue_stub"]["candidates_seen"], 3);
    assert_eq!(
        fs::read(package_path.join("example_pkg-1.0.tar.gz")).unwrap(),
        b"x"
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join("check-report.json")).unwrap())
            .unwrap();
    assert_eq!(saved, report);
}
