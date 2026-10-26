use std::process::Command;

fn command(mode: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_snatchrs"));
    command.arg(mode);
    if mode == "mirror" {
        command.args(["--min-free", "0"]);
    }
    command
}

#[test]
fn redirected_output_stays_plain_and_quiet_suppresses_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .args([
            "--quiet",
            "index",
            "--from-file",
            "tests/fixtures/index.json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "friendly-bard\nrequests\nzope-interface\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_output_is_machine_readable() {
    let output = Command::new(env!("CARGO_BIN_EXE_snatchrs"))
        .args([
            "index",
            "--from-file",
            "tests/fixtures/index.json",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["packages"].as_array().unwrap().len(), 3);
    assert_eq!(value["packages"][0]["normalized_name"], "friendly-bard");
}

#[test]
fn mirror_check_and_simple_require_a_directory() {
    for arguments in [
        vec!["mirror", "--package", "pkg"],
        vec!["check", "--all"],
        vec!["simple"],
        vec!["mirror"],
    ] {
        let output = command(arguments[0])
            .args(&arguments[1..])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("<DIR>"));
    }
}

#[test]
fn selection_is_explicit_exclusive_and_seed_requires_sample() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("not-created");
    for mode in ["mirror", "check"] {
        let output = command(mode)
            .arg(&destination)
            .args([
                "--index-url",
                "http://127.0.0.1:1/simple/",
                "--attempts",
                "1",
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8_lossy(&output.stderr);
        for option in ["--all", "--package", "--packages-file", "--sample"] {
            assert!(error.contains(option), "{error}");
        }
        let selections = [
            vec!["--all"],
            vec!["--package", "pkg"],
            vec!["--packages-file", "unused.txt"],
            vec!["--sample", "1"],
        ];
        for (i, first) in selections.iter().enumerate() {
            for second in selections.iter().skip(i + 1) {
                let output = command(mode)
                    .arg(&destination)
                    .args(first)
                    .args(second)
                    .args([
                        "--index-url",
                        "http://127.0.0.1:1/simple/",
                        "--attempts",
                        "1",
                    ])
                    .output()
                    .unwrap();
                assert_eq!(output.status.code(), Some(2), "{first:?}, {second:?}");
            }
        }
        for selection in [&["--all"][..], &["--package", "pkg"][..], &[][..]] {
            let output = command(mode)
                .arg(&destination)
                .args(selection)
                .args(["--seed", "42"])
                .args([
                    "--index-url",
                    "http://127.0.0.1:1/simple/",
                    "--attempts",
                    "1",
                ])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2));
            assert!(String::from_utf8_lossy(&output.stderr).contains("--sample"));
        }
    }
    assert!(!destination.exists());
}

#[test]
fn removed_flags_are_not_retained_as_aliases() {
    let root = tempfile::tempdir().unwrap();
    for flag in ["--mirror", "--threads"] {
        let output = command("mirror")
            .arg(root.path())
            .args(["--package", "pkg", flag, "unused"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));
    }
}
