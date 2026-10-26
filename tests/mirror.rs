use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
};

use snatchrs::{
    IndexClient, Package, PackageIndex,
    download::build_tasks,
    mirror::{
        CheckSummary, FileState, MetadataOutcome, MirrorLayout, PackageMetadata, check_package,
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
        Url::parse("https://pypi.org/simple/example-pkg/").unwrap(),
        layout,
    )
}

#[test]
fn includes_all_versions_files_urls_sizes_and_paths() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let metadata = parse(FIXTURE, &layout).unwrap();
    assert_eq!(metadata.releases.len(), 3);
    assert_eq!(metadata.last_serial, Some(12345));
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
        std::path::Path::new("packages/example-pkg/example_pkg-1.0-py3-none-any.whl")
    );
    assert_eq!(file.sha256.as_deref(), Some("abc123"));
    assert_eq!(file.requires_python.as_deref(), Some(">=3.9"));
    assert!(metadata.releases[2].files[0].yanked);
    layout.save_metadata(&metadata).unwrap();
    assert!(layout.root.join(&metadata.package_path).is_dir());
    assert!(layout.root.join(&metadata.metadata_path).is_file());
    assert_eq!(
        fs::read_dir(layout.root.join(&metadata.package_path))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn resolves_relative_urls_and_accepts_yanked_reasons() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    value["files"][0]["url"] = "../../files/a.whl#sha256=abc123".into();
    value["files"][1]["url"] = "/files/b.tar.gz".into();
    value["files"][2]["url"] = "//cdn.example.org/c.whl".into();
    value["files"][0]["yanked"] = "Broken release".into();
    value["files"][1]["yanked"] = "".into();
    let metadata = parse(&value.to_string(), &layout).unwrap();
    assert_eq!(
        metadata.releases[1].files[0].download_url.as_str(),
        "https://pypi.org/files/a.whl#sha256=abc123"
    );
    assert_eq!(
        metadata.releases[1].files[1].download_url.as_str(),
        "https://pypi.org/files/b.tar.gz"
    );
    assert_eq!(
        metadata.releases[2].files[0].download_url.as_str(),
        "https://cdn.example.org/c.whl"
    );
    assert!(metadata.releases[1].files[0].yanked);
    assert!(!metadata.releases[1].files[1].yanked);
    assert!(metadata.releases[2].files[0].yanked);
}

fn simple(versions: &[&str], filenames: &[&str]) -> serde_json::Value {
    serde_json::json!({"name": "Example_Pkg", "meta": {"api-version": "1.4"}, "versions": versions,
        "files": filenames.iter().map(|name| serde_json::json!({"filename": name, "url": name, "size": 1, "hashes": {}})).collect::<Vec<_>>()})
}

#[test]
fn groups_normalized_versions_build_wheels_and_legacy_distributions() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let value = simple(
        &["0.6rc11", "1.0-1", "2.0", "empty", "legacy-foo"],
        &[
            "example_pkg-1.0.post1-py3-none-any.whl",
            "example_pkg-2.0-3-py3-none-any.whl",
            "Example-Pkg-legacy-foo.tar.gz",
            "example_pkg-2.0-py2.7.egg",
            "example_pkg-2.0.win32.exe",
            "example_pkg-1.0.post1-py2.7.egg",
            "example_pkg-1.0.post1.win32.exe",
            "example_pkg-0.6c11-py2.7.egg",
        ],
    );
    let metadata = parse(&value.to_string(), &layout).unwrap();
    let release = |name: &str| {
        metadata
            .releases
            .iter()
            .find(|release| release.version == name)
            .unwrap()
    };
    assert_eq!(release("1.0-1").files.len(), 3);
    assert_eq!(release("0.6rc11").files.len(), 1);
    assert_eq!(release("2.0").files.len(), 3);
    assert_eq!(release("legacy-foo").files.len(), 1);
    assert!(release("empty").files.is_empty());
    assert_eq!(
        release("2.0").files[0].package_type.as_deref(),
        Some("bdist_wheel")
    );
    assert_eq!(
        release("2.0").files[1].package_type.as_deref(),
        Some("bdist_egg")
    );
}

#[test]
fn infers_versions_when_older_simple_response_omits_versions() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let mut value = simple(
        &[],
        &["example_pkg-1.0.tar.gz", "example_pkg-2.0-py3-none-any.whl"],
    );
    value.as_object_mut().unwrap().remove("versions");
    value["meta"]["api-version"] = "1.0".into();
    let metadata = parse(&value.to_string(), &layout).unwrap();
    assert_eq!(
        metadata
            .releases
            .iter()
            .map(|release| release.version.as_str())
            .collect::<Vec<_>>(),
        ["1.0", "2.0"]
    );
    assert_eq!(metadata.last_serial, None);
}

#[test]
fn rejects_ambiguous_unlisted_and_malformed_simple_metadata() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    for value in [
        simple(&["1.0", "1.0.0"], &["example_pkg-1.0.00-py3-none-any.whl"]),
        simple(&["1.0", "2.0"], &["example_pkg-3.0.tar.gz"]),
        simple(&["1.0", "2.0"], &["opaque.blob"]),
        simple(&[], &["example_pkg-1.0.tar.gz"]),
        simple(&[""], &[]),
        serde_json::json!({"info": {"name": "Example_Pkg"}, "releases": {}}),
    ] {
        assert!(
            parse(&value.to_string(), &layout).is_err(),
            "accepted {value}"
        );
    }
    let mut value = simple(&["1.0"], &["example_pkg-1.0.tar.gz"]);
    value["meta"]["api-version"] = "2.0".into();
    assert!(parse(&value.to_string(), &layout).is_err());
    value["meta"]["api-version"] = "1.4".into();
    value["files"][0].as_object_mut().unwrap().remove("hashes");
    assert!(parse(&value.to_string(), &layout).is_err());
    let empty = parse(&simple(&[], &[]).to_string(), &layout).unwrap();
    assert!(empty.releases.is_empty());
}

#[test]
fn fetches_simple_json_and_resolves_files_against_redirected_page() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for position in 0..2 {
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
            requests.push(String::from_utf8(request).unwrap());
            if position == 0 {
                write!(stream, "HTTP/1.1 302 Found\r\nLocation: /repository/project.json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            } else {
                let mut response = simple(&["1.0"], &["example_pkg-1.0.tar.gz"]);
                response["meta"]["_last-serial"] = 12345.into();
                response["files"][0]["url"] = "../files/example_pkg-1.0.tar.gz".into();
                let body = response.to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.pypi.simple.v1+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        }
        requests
    });
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let metadata = IndexClient::new()
        .unwrap()
        .fetch_metadata(
            package(),
            format!("{base}/simple/").parse().unwrap(),
            &layout,
        )
        .unwrap();
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("GET /simple/example-pkg/ HTTP/1.1\r\n"));
    assert!(requests[1].starts_with("GET /repository/project.json HTTP/1.1\r\n"));
    assert!(requests.iter().all(|request| {
        request
            .to_ascii_lowercase()
            .contains("accept: application/vnd.pypi.simple.v1+json\r\n")
    }));
    assert_eq!(
        metadata.metadata_url.as_str(),
        format!("{base}/repository/project.json")
    );
    assert_eq!(
        metadata.releases[0].files[0].download_url.as_str(),
        format!("{base}/files/example_pkg-1.0.tar.gz")
    );
    // The cache is keyed by the requested endpoint, retaining redirected URLs
    // for resolving files. The server has stopped, so another fetch would fail.
    layout.save_metadata(&metadata).unwrap();
    let mut current = package();
    current.last_serial = Some(12345);
    let cached = IndexClient::with_attempts(1)
        .unwrap()
        .load_metadata(
            current,
            format!("{base}/simple/").parse().unwrap(),
            &layout,
            false,
        )
        .unwrap();
    let MetadataOutcome::Available(cached) = cached else {
        panic!("cached project unavailable")
    };
    assert_eq!(cached.metadata_url, metadata.metadata_url);
    assert_eq!(
        cached.releases[0].files[0].download_url,
        metadata.releases[0].files[0].download_url
    );
}

#[test]
fn checks_missing_matching_wrong_sizes_and_non_files_without_downloading() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let metadata = parse(FIXTURE, &layout).unwrap();
    layout.save_metadata(&metadata).unwrap();
    fs::write(
        layout.root.join(&metadata.releases[1].files[0].local_path),
        b"1234",
    )
    .unwrap();
    fs::write(
        layout.root.join(&metadata.releases[1].files[1].local_path),
        b"x",
    )
    .unwrap();
    fs::create_dir(layout.root.join(&metadata.releases[2].files[1].local_path)).unwrap();
    let mut summary = CheckSummary::default();
    let checked = check_package(&layout.root, metadata, &mut summary);
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
    assert!(!layout.root.join(&checked.files[2].local_path).exists());
    assert_eq!(
        fs::read(layout.root.join(&checked.files[1].local_path)).unwrap(),
        b"x"
    );
}

#[test]
fn build_tasks_rejects_check_results_misaligned_with_metadata() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let metadata = parse(FIXTURE, &layout).unwrap();
    layout.save_metadata(&metadata).unwrap();
    let mut checked = check_package(&layout.root, metadata, &mut CheckSummary::default());
    assert_eq!(
        build_tasks(std::slice::from_ref(&checked)).unwrap().len(),
        4
    );

    checked.files.swap(0, 1);
    let error = build_tasks(std::slice::from_ref(&checked)).unwrap_err();
    assert!(error.to_string().contains("does not match"), "{error:#}");

    checked.files.swap(0, 1);
    checked.files.pop();
    let error = build_tasks(std::slice::from_ref(&checked)).unwrap_err();
    assert!(error.to_string().contains("cover 3 of 4"), "{error:#}");
}

#[test]
fn existing_zero_byte_file_matches_zero_expected_size() {
    let directory = tempdir().unwrap();
    let layout = MirrorLayout::create(directory.path()).unwrap();
    let metadata = parse(FIXTURE, &layout).unwrap();
    layout.save_metadata(&metadata).unwrap();
    let file = &metadata.releases[2].files[1];
    fs::write(layout.root.join(&file.local_path), b"").unwrap();
    assert_eq!(inspect_file(&layout.root, file), FileState::MatchingSize);
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
        value["files"][0]["filename"] = filename.into();
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
    symlink(outside.path(), layout.root.join(&metadata.package_path)).unwrap();
    assert!(layout.save_metadata(&metadata).is_err());
    fs::remove_file(layout.root.join(&metadata.package_path)).unwrap();
    layout.save_metadata(&metadata).unwrap();
    let file = &metadata.releases[1].files[0];
    symlink(
        outside.path().join("missing"),
        layout.root.join(&file.local_path),
    )
    .unwrap();
    assert_eq!(inspect_file(&layout.root, file), FileState::NotRegularFile);
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
fn cli_saves_metadata_and_report_and_continues_after_unavailable_project() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/simple/", listener.local_addr().unwrap());
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
            let (status, body) = if request.starts_with("GET /simple/example-pkg/ ") {
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
    assert_eq!(report["summary"]["packages"], 1);
    assert_eq!(report["summary"]["releases"], 3);
    assert_eq!(report["summary"]["missing"], 4);
    assert!(report.get("queue_stub").is_none());
    assert_eq!(report["error_count"], 0);
    assert_eq!(report["unavailable_count"], 1);
    assert!(report.get("packages").is_none());
    let errors = fs::read_to_string(
        directory
            .path()
            .join(report["errors_path"].as_str().unwrap()),
    )
    .unwrap();
    assert!(errors.is_empty());
    let unavailable = fs::read_to_string(
        directory
            .path()
            .join(report["unavailable_path"].as_str().unwrap()),
    )
    .unwrap();
    let outcome: serde_json::Value =
        serde_json::from_str(unavailable.lines().next().unwrap()).unwrap();
    assert_eq!(outcome["name"], "gone");
    assert_eq!(outcome["status"], "not_found");
    assert_eq!(outcome["http_status"], 404);
    let packages = fs::read_to_string(
        directory
            .path()
            .join(report["packages_path"].as_str().unwrap()),
    )
    .unwrap();
    let checked: snatchrs::mirror::CheckedPackage =
        serde_json::from_str(packages.lines().next().unwrap()).unwrap();
    assert_eq!(checked.files.len(), 4);
    assert_eq!(packages.lines().count(), 1);
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
        ])
        .arg(directory.path())
        .output()
        .unwrap();
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1]
            .to_lowercase()
            .contains("accept: application/vnd.pypi.simple.v1+json\r\n")
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
    assert_eq!(
        fs::read(package_path.join("example_pkg-1.0.tar.gz")).unwrap(),
        b"x"
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join("check-report.json")).unwrap())
            .unwrap();
    assert_eq!(saved, report);
}
