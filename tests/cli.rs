use std::process::Command;

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
