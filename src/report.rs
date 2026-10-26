//! Run-journal views; the live inspection and download types stay unchanged.
use serde::{Deserialize, Serialize};
use snatchrs::{
    download::DownloadOutcome,
    mirror::{CheckedFile, CheckedPackage, FileState},
};
use std::path::{Path, PathBuf};

#[derive(Serialize)]
pub struct CheckedPackageLine<'a> {
    name: &'a str,
    last_serial: Option<u64>,
    metadata_path: &'a Path,
    release_count: usize,
    files: usize,
    missing: usize,
    size_mismatch: usize,
    matching_size: usize,
    blocked: usize,
    errors: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    problems: Vec<&'a CheckedFile>,
}
impl<'a> From<&'a CheckedPackage> for CheckedPackageLine<'a> {
    fn from(package: &'a CheckedPackage) -> Self {
        let mut record = Self {
            name: &package.metadata.package.normalized_name,
            last_serial: package
                .metadata
                .last_serial
                .or(package.metadata.package.last_serial),
            metadata_path: &package.metadata.metadata_path,
            release_count: package.metadata.releases.len(),
            files: package.files.len(),
            missing: 0,
            size_mismatch: 0,
            matching_size: 0,
            blocked: 0,
            errors: 0,
            problems: Vec::new(),
        };
        for file in &package.files {
            match file.state {
                FileState::Missing => record.missing += 1,
                FileState::SizeMismatch { .. } => record.size_mismatch += 1,
                FileState::MatchingSize => {
                    record.matching_size += 1;
                    continue;
                }
                FileState::NotRegularFile => record.blocked += 1,
                FileState::Error { .. } => record.errors += 1,
            }
            record.problems.push(file);
        }
        record
    }
}

// Readers only need counts and a pointer into metadata/, not per-file details.
#[derive(Deserialize)]
pub struct PackageReference {
    pub name: String,
    pub metadata_path: PathBuf,
    pub release_count: usize,
    pub files: usize,
    pub missing: usize,
    pub size_mismatch: usize,
}

#[derive(Serialize)]
pub struct DownloadOutcomeLine<'a> {
    package: &'a str,
    filename: &'a str,
    local_path: &'a Path,
    size: u64,
    attempts: u32,
    downloaded: bool,
    sha256_verified: bool,
    error: &'a Option<String>,
}
impl<'a> From<&'a DownloadOutcome> for DownloadOutcomeLine<'a> {
    fn from(outcome: &'a DownloadOutcome) -> Self {
        Self {
            package: &outcome.task.package,
            filename: &outcome.task.filename,
            local_path: &outcome.task.local_path,
            size: outcome.task.size,
            attempts: outcome.attempts,
            downloaded: outcome.downloaded,
            sha256_verified: outcome.sha256_verified,
            error: &outcome.error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use snatchrs::{Package, download::DownloadTask, mirror::PackageMetadata};

    fn checked(states: Vec<FileState>) -> CheckedPackage {
        let root = tempfile::tempdir().unwrap();
        let layout = snatchrs::mirror::MirrorLayout::create(root.path()).unwrap();
        let source: url::Url = "https://example.org/simple/project/".parse().unwrap();
        let input = json!({"name":"project", "meta":{"api-version":"1.4", "_last-serial":42}, "versions":["1.0"], "files": states.iter().enumerate().map(|(i, _)| json!({"filename":format!("project-1.0-{i}-py3-none-any.whl"), "url":format!("https://example.org/{i}"), "size":10, "hashes":{"sha256":"private-metadata-hash"}})).collect::<Vec<_>>()});
        let metadata = PackageMetadata::from_reader(
            input.to_string().as_bytes(),
            Package {
                name: "project".into(),
                normalized_name: "project".into(),
                index_url: source.clone(),
                last_serial: None,
            },
            source,
            &layout,
        )
        .unwrap();
        let files = states
            .into_iter()
            .enumerate()
            .map(|(i, state)| CheckedFile {
                version: "1.0".into(),
                filename: format!("project-1.0-{i}-py3-none-any.whl"),
                local_path: format!("packages/project/project-1.0-{i}-py3-none-any.whl").into(),
                state,
            })
            .collect();
        CheckedPackage { metadata, files }
    }
    fn assert_no_copied_metadata(value: &Value) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    assert!(
                        !["metadata", "releases", "download_url", "sha256"].contains(&key.as_str()),
                        "{key}"
                    );
                    assert_no_copied_metadata(value);
                }
            }
            Value::Array(values) => values.iter().for_each(assert_no_copied_metadata),
            _ => {}
        }
    }
    #[test]
    fn checked_lines_keep_only_counts_references_and_problem_files() {
        let package = checked(vec![
            FileState::MatchingSize,
            FileState::Missing,
            FileState::SizeMismatch { actual_size: 3 },
            FileState::NotRegularFile,
            FileState::Error {
                message: "permission denied".into(),
            },
        ]);
        let value = serde_json::to_value(CheckedPackageLine::from(&package)).unwrap();
        assert_no_copied_metadata(&value);
        assert_eq!(value["name"], "project");
        assert_eq!(value["last_serial"], 42);
        assert_eq!(value["metadata_path"], "metadata/project.json");
        assert_eq!(value["release_count"], 1);
        assert_eq!(value["files"], 5);
        for state in [
            "matching_size",
            "missing",
            "size_mismatch",
            "blocked",
            "errors",
        ] {
            assert_eq!(value[state], 1);
        }
        let problems = value["problems"].as_array().unwrap();
        assert_eq!(problems.len(), 4);
        assert_eq!(problems[0]["status"], "missing");
        assert_eq!(problems[1]["status"], "size_mismatch");
        assert_eq!(problems[1]["actual_size"], 3);
        assert_eq!(problems[2]["status"], "not_regular_file");
        assert_eq!(problems[3]["status"], "error");
        assert_eq!(problems[3]["message"], "permission denied");
        assert!(
            problems
                .iter()
                .all(|file| file["status"] != "matching_size")
        );
        for (record, original) in problems.iter().zip(&package.files[1..]) {
            assert_eq!(record["filename"], original.filename);
            assert_eq!(record["version"], original.version);
            assert_eq!(record["local_path"], original.local_path.to_str().unwrap());
        }
    }
    #[test]
    fn healthy_projects_have_no_problem_array() {
        let package = checked(vec![FileState::MatchingSize]);
        let value = serde_json::to_value(CheckedPackageLine::from(&package)).unwrap();
        assert_eq!(value["matching_size"], 1);
        assert!(value.get("problems").is_none());
        assert_no_copied_metadata(&value);
    }
    #[test]
    fn outcome_view_is_flat_without_changing_the_download_type() {
        let outcome = DownloadOutcome {
            task: DownloadTask {
                package: "project".into(),
                filename: "project-1.0.tar.gz".into(),
                download_url: "https://example.org/project.tar.gz".parse().unwrap(),
                local_path: "packages/project/project-1.0.tar.gz".into(),
                size: 10,
                sha256: Some("hash".into()),
            },
            attempts: 2,
            downloaded: false,
            sha256_verified: false,
            error: Some("disk full".into()),
        };
        let value = serde_json::to_value(DownloadOutcomeLine::from(&outcome)).unwrap();
        assert_eq!(
            value,
            json!({"package":"project", "filename":"project-1.0.tar.gz", "local_path":"packages/project/project-1.0.tar.gz", "size":10, "attempts":2, "downloaded":false, "sha256_verified":false, "error":"disk full"})
        );
        assert_no_copied_metadata(&value);
        let original = serde_json::to_value(&outcome).unwrap();
        assert_eq!(original["task"]["sha256"], "hash");
        assert_eq!(
            original["task"]["download_url"],
            "https://example.org/project.tar.gz"
        );
    }
}
