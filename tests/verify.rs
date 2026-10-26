use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use snatchrs::{
    Package,
    mirror::{DistributionFile, MirrorLayout, PackageMetadata, Release},
    simple,
};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::tempdir;

type Entry<'a> = (&'a str, Option<&'a [u8]>, Option<&'a [u8]>);

fn metadata(root: &Path, name: &str, entries: &[Entry<'_>]) -> PackageMetadata {
    let layout = MirrorLayout::create(root).unwrap();
    let url: url::Url = "https://offline.invalid/simple/".parse().unwrap();
    let files = entries
        .iter()
        .map(|(filename, actual, expected)| {
            let path = Path::new("packages").join(name).join(filename);
            fs::create_dir_all(root.join("packages").join(name)).unwrap();
            if let Some(body) = actual {
                fs::write(root.join(&path), body).unwrap();
            }
            DistributionFile {
                filename: filename.to_string(),
                download_url: "https://offline.invalid/file".parse().unwrap(),
                local_path: path,
                size: expected.or(*actual).map_or(4, |v| v.len()) as u64,
                sha256: expected.map(|body| format!("{:X}", Sha256::digest(body))),
                package_type: None,
                requires_python: None,
                yanked: false,
            }
        })
        .collect();
    let metadata = PackageMetadata {
        package: Package {
            name: name.into(),
            normalized_name: name.into(),
            index_url: url.clone(),
            last_serial: None,
        },
        metadata_url: url,
        metadata_source_url: None,
        metadata_path: format!("metadata/{name}.json").into(),
        package_path: format!("packages/{name}").into(),
        last_serial: None,
        releases: vec![Release {
            version: "1.0".into(),
            files,
        }],
    };
    layout.save_metadata(&metadata).unwrap();
    metadata
}
fn run(root: &Path, extra: &[&str]) -> (Output, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .arg("verify")
        .arg(root)
        .args(["--json", "--no-progress"])
        .args(extra)
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .output()
        .unwrap();
    let report = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)));
    (output, report)
}
#[test]
#[cfg(unix)]
fn states_selection_and_safe_removal() {
    use std::os::unix::fs::symlink;
    let temp = tempdir().unwrap();
    let root = temp.path().join("mirror");
    let meta = metadata(
        &root,
        "example",
        &[
            ("ok.whl", Some(b"good"), Some(b"good")),
            ("nohash.whl", Some(b"good"), None),
            ("missing.whl", None, Some(b"good")),
            ("size.whl", Some(b"x"), Some(b"good")),
            ("bad.whl", Some(b"evil"), Some(b"good")),
            ("link.whl", None, Some(b"good")),
        ],
    );
    fs::write(temp.path().join("target"), b"good").unwrap();
    symlink(
        temp.path().join("target"),
        root.join("packages/example/link.whl"),
    )
    .unwrap();
    fs::write(root.join("metadata/corrupt.json"), b"{").unwrap();
    let (output, report) = run(&root, &[]);
    assert!(!output.status.success());
    for state in [
        "ok",
        "no_hash",
        "missing",
        "size_mismatch",
        "hash_mismatch",
        "blocked",
        "project_errors",
    ] {
        assert_eq!(report[state], 1, "{state}");
    }
    assert_eq!(report["projects"], 2);
    assert_eq!(report["files"], 6);
    assert_eq!(report["bytes_hashed"], 8);
    let problems =
        fs::read_to_string(root.join(report["problems_path"].as_str().unwrap())).unwrap();
    let problems: Vec<Value> = problems
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(problems.len(), 5);
    for problem in problems {
        assert!(!["ok", "no_hash"].contains(&problem["state"].as_str().unwrap()));
        assert!(problem["detail"].is_string());
        assert!(!Path::new(problem["local_path"].as_str().unwrap()).is_absolute());
    }
    simple::write_project_page(&root, &meta).unwrap();
    simple::write_root_index(&root).unwrap();
    fs::write(root.join("packages/example/.snatchrs-stale"), b"keep").unwrap();
    let (output, report) = run(
        &root,
        &[
            "--package",
            "EXAMPLE",
            "--package",
            "example",
            "--remove-bad",
        ],
    );
    assert!(!output.status.success());
    assert_eq!(report["projects"], 1);
    assert_eq!(report["project_errors"], 0);
    assert_eq!(report["removed"], 1);
    assert_eq!(report["hash_mismatch"], 1);
    assert!(!root.join("packages/example/bad.whl").exists());
    assert_eq!(
        fs::read(root.join("packages/example/size.whl")).unwrap(),
        b"x"
    );
    assert_eq!(fs::read(temp.path().join("target")).unwrap(), b"good");
    assert!(root.join("packages/example/.snatchrs-stale").exists());
    assert!(
        !fs::read_to_string(root.join("simple/example/index.html"))
            .unwrap()
            .contains("bad.whl")
    );
    metadata(
        &root,
        "clean-pkg",
        &[
            ("ok.whl", Some(b"good"), Some(b"good")),
            ("nohash.whl", Some(b"good"), None),
        ],
    );
    let (output, report) = run(&root, &["--package", "Clean_Pkg", "--package", "clean.pkg"]);
    assert!(output.status.success());
    assert_eq!(report["projects"], 1);
    assert_eq!(report["ok"], 1);
    assert_eq!(report["no_hash"], 1);
    assert_eq!(
        fs::read_to_string(root.join(report["problems_path"].as_str().unwrap())).unwrap(),
        ""
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(root.join("verify-report.json")).unwrap())
            .unwrap(),
        report
    );
}
#[test]
fn lock_and_cli_validation() {
    let temp = tempdir().unwrap();
    let layout = MirrorLayout::create(temp.path()).unwrap();
    let lock = layout.lock().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .arg("verify")
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Another snatchrs"));
    drop(lock);
    for count in ["0", "65"] {
        assert!(
            !Command::new(env!("CARGO_BIN_EXE_snatchrs"))
                .arg("verify")
                .arg(temp.path())
                .args(["-j", count])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}
#[test]
#[cfg(unix)]
fn unsafe_paths_and_read_errors() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = tempdir().unwrap();
    let root = temp.path().join("mirror");
    let mut meta = metadata(
        &root,
        "example",
        &[("unreadable.whl", Some(b"good"), Some(b"good"))],
    );
    let path = root.join("packages/example/unreadable.whl");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
    if fs::File::open(&path).is_err() {
        let (out, report) = run(&root, &[]);
        assert!(!out.status.success());
        assert_eq!(report["error"], 1);
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    meta.releases[0].files[0].local_path = "../outside".into();
    MirrorLayout { root: root.clone() }
        .save_metadata(&meta)
        .unwrap();
    let (_, report) = run(&root, &["--remove-bad"]);
    assert_eq!(report["blocked"], 1);
    assert!(path.exists());
    meta.releases[0].files[0].local_path = "packages/example/unreadable.whl".into();
    MirrorLayout { root: root.clone() }
        .save_metadata(&meta)
        .unwrap();
    fs::rename(root.join("packages/example"), temp.path().join("external")).unwrap();
    symlink(temp.path().join("external"), root.join("packages/example")).unwrap();
    let (_, report) = run(&root, &["--remove-bad"]);
    assert_eq!(report["blocked"], 1);
    assert!(temp.path().join("external/unreadable.whl").exists());
}
#[test]
fn remove_then_mirror_repairs() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let origin = base.clone();
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|s| s == b"\r\n\r\n") {
                let mut buf = [0; 1024];
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
            }
            let body = if String::from_utf8_lossy(&request).starts_with("GET /simple/") {
                serde_json::to_vec(&json!({"name":"example", "meta":{"api-version":"1.4"}, "versions":["1.0"], "files":[{"filename":"example-1.0.tar.gz", "url":format!("{origin}/artifact"), "size":4, "hashes":{"sha256":format!("{:x}",Sha256::digest(b"good"))}}]})).unwrap()
            } else {
                b"good".to_vec()
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    let temp = tempdir().unwrap();
    let meta = metadata(
        temp.path(),
        "example",
        &[("example-1.0.tar.gz", Some(b"evil"), Some(b"good"))],
    );
    simple::write_project_page(temp.path(), &meta).unwrap();
    let (output, report) = run(temp.path(), &["--remove-bad"]);
    assert!(!output.status.success());
    assert_eq!(report["removed"], 1);
    assert!(
        !fs::read_to_string(temp.path().join("simple/example/index.html"))
            .unwrap()
            .contains("example-1.0.tar.gz")
    );
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .arg("mirror")
        .arg(temp.path())
        .args([
            "--package",
            "example",
            "--metadata-url",
            &format!("{base}/simple/"),
            "--no-progress",
            "--min-free",
            "0",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let (output, report) = run(temp.path(), &[]);
    assert!(output.status.success());
    assert_eq!(report["ok"], 1);
}

#[test]
fn streaming_hash_plain_summary_and_missing_metadata() {
    let temp = tempdir().unwrap();
    let body = vec![42; 2 * 1024 * 1024 + 7];
    metadata(
        temp.path(),
        "large",
        &[("large.whl", Some(&body), Some(&body))],
    );
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .arg("verify")
        .arg(temp.path())
        .args(["-j", "1", "--quiet"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
    let report: Value =
        serde_json::from_slice(&fs::read(temp.path().join("verify-report.json")).unwrap()).unwrap();
    assert_eq!(report["bytes_hashed"], body.len());
    assert_eq!(report["ok"], 1);
    let (output, report) = run(temp.path(), &["--package", "absent"]);
    assert!(!output.status.success());
    assert_eq!(report["project_errors"], 1);
    assert_eq!(report["files"], 0);
}
