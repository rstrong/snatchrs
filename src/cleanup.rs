//! Best-effort cleanup under the mirror lock; never recurse or follow symlinks.
use std::{fs, io, path::Path};

fn warning(path: &Path, error: io::Error) {
    if error.kind() != io::ErrorKind::NotFound {
        eprintln!(
            "warning: cannot clean stale temporary files at {}: {error}",
            path.display()
        );
    }
}
fn real_directory(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(meta) => meta.file_type().is_dir(),
        Err(error) => {
            warning(path, error);
            false
        }
    }
}
pub fn sweep_directory(path: &Path) -> usize {
    if !real_directory(path) {
        return 0;
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            warning(path, error);
            return 0;
        }
    };
    let mut removed = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warning(path, error);
                continue;
            }
        };
        if !entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(b".snatchrs-")
        {
            continue;
        }
        let path = entry.path();
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => match fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(error) => warning(&path, error),
            },
            Ok(_) => {}
            Err(error) => warning(&path, error),
        }
    }
    removed
}
pub fn sweep_project(root: &Path, name: &str) -> usize {
    if !crate::normalize_name(name).is_ok_and(|normalized| normalized == name) {
        return 0;
    }
    ["packages", "simple"]
        .into_iter()
        .map(|parent| {
            let parent = root.join(parent);
            if real_directory(&parent) {
                sweep_directory(&parent.join(name))
            } else {
                0
            }
        })
        .sum()
}
pub fn announce(count: usize, quiet: bool) {
    if count > 0 && !quiet {
        eprintln!("Removed {count} stale temporary files from an interrupted run");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sweeps_only_regular_prefixed_files_and_only_selected_projects() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            ".snatchrs-abc",
            ".snatchrs-json-x",
            ".snatchrs-queue-y",
            ".snatchrs.lock",
            "keep.whl",
            ".snatchrsfoo",
        ] {
            fs::write(dir.path().join(name), b"keep").unwrap();
        }
        fs::create_dir(dir.path().join(".snatchrs-dir")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            dir.path().join("keep.whl"),
            dir.path().join(".snatchrs-link"),
        )
        .unwrap();
        assert_eq!(sweep_directory(dir.path()), 3);
        for name in [
            ".snatchrs.lock",
            "keep.whl",
            ".snatchrsfoo",
            ".snatchrs-dir",
        ] {
            assert!(dir.path().join(name).exists());
        }
        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(dir.path().join(".snatchrs-link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        for parent in ["packages", "simple"] {
            for project in ["one", "two"] {
                let path = dir.path().join(parent).join(project);
                fs::create_dir_all(&path).unwrap();
                fs::write(path.join(".snatchrs-stale"), b"stale").unwrap();
            }
        }
        assert_eq!(sweep_project(dir.path(), "one"), 2);
        for parent in ["packages", "simple"] {
            assert!(dir.path().join(parent).join("two/.snatchrs-stale").exists());
        }
    }
}

/// Reuse the existing UTC HTTP-date formatter without another date dependency.
pub(crate) fn report_timestamp(time: std::time::SystemTime) -> String {
    let date = httpdate::fmt_http_date(time);
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|month| *month == &date[8..11])
    .expect("HTTP-date month")
        + 1;
    format!(
        "{}{:02}{}T{}Z",
        &date[12..16],
        month,
        &date[5..7],
        date[17..25].replace(':', "")
    )
}

fn timestamp_in_name(name: &str, prefix: &str) -> Option<String> {
    let (stamp, random) = name.strip_prefix(prefix)?.split_once('-')?;
    let bytes = stamp.as_bytes();
    if random.is_empty()
        || bytes.len() != 16
        || bytes[8] != b'T'
        || bytes[15] != b'Z'
        || !bytes[..8]
            .iter()
            .chain(&bytes[9..15])
            .all(u8::is_ascii_digit)
    {
        return None;
    }
    let year: u16 = stamp[..4].parse().ok()?;
    let month: usize = stamp[4..6].parse().ok()?;
    let day: u8 = stamp[6..8].parse().ok()?;
    let hour: u8 = stamp[9..11].parse().ok()?;
    let minute: u8 = stamp[11..13].parse().ok()?;
    let second: u8 = stamp[13..15].parse().ok()?;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if year < 1970
        || !(1..=12).contains(&month)
        || day == 0
        || day > days[month - 1]
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    Some(stamp.to_owned())
}

fn retention_warning(path: &Path, error: impl std::fmt::Display) {
    eprintln!(
        "warning: report retention skipped {}: {error}",
        path.display()
    );
}

fn normalized_relative(path: &str) -> Option<std::path::PathBuf> {
    use std::path::{Component, PathBuf};
    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(normalized)
}

fn reference_directory(path: &str) -> Option<std::path::PathBuf> {
    let normalized = normalized_relative(path)?;
    let mut parts = normalized.components();
    if parts.next()?.as_os_str() != "reports" {
        return None;
    }
    let name = parts.next()?.as_os_str();
    Some(Path::new("reports").join(name))
}

fn collect_references(
    value: &serde_json::Value,
    protected: &mut std::collections::BTreeSet<std::path::PathBuf>,
) -> Option<()> {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if key.ends_with("_path") && !value.is_null() {
                    let path = value.as_str()?;
                    normalized_relative(path)?;
                    if let Some(directory) = reference_directory(path) {
                        protected.insert(directory);
                    }
                } else {
                    collect_references(value, protected)?;
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_references(value, protected)?;
            }
        }
        _ => {}
    }
    Some(())
}

fn protected_reports(root: &Path) -> (std::collections::BTreeSet<std::path::PathBuf>, [bool; 2]) {
    let mut protected = std::collections::BTreeSet::new();
    let mut blocked = [false; 2];
    for (filename, group, required) in [
        ("check-report.json", 0, &["/packages_path"][..]),
        (
            "mirror-report.json",
            0,
            &["/inspection/packages_path", "/downloads/outcomes_path"][..],
        ),
        ("verify-report.json", 1, &["/problems_path"][..]),
    ] {
        let path = root.join(filename);
        let parsed = (|| -> anyhow::Result<()> {
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
                Ok(meta) => {
                    anyhow::ensure!(meta.file_type().is_file(), "Expected a regular report file")
                }
            }
            let value: serde_json::Value =
                serde_json::from_reader(std::io::BufReader::new(fs::File::open(&path)?))?;
            for pointer in required {
                let directory = value
                    .pointer(pointer)
                    .and_then(|value| value.as_str())
                    .and_then(reference_directory);
                anyhow::ensure!(
                    directory
                        .as_ref()
                        .and_then(|path| path.file_name())
                        .is_some_and(|name| name.as_encoded_bytes().starts_with(if group == 0 {
                            b"run-"
                        } else {
                            b"verify-"
                        })),
                    "Invalid or missing {pointer}"
                );
            }
            anyhow::ensure!(
                collect_references(&value, &mut protected).is_some(),
                "Invalid relative report path"
            );
            Ok(())
        })();
        if let Err(error) = parsed {
            blocked[group] = true;
            retention_warning(&path, error);
        }
    }
    (protected, blocked)
}

fn remove_report_directory(path: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_dir() {
        return Err(io::Error::other(
            "No longer a real directory; keeping entry",
        ));
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        if !fs::symlink_metadata(&path)?.file_type().is_file() {
            return Err(io::Error::other(
                "Contains a non-regular entry; keeping directory",
            ));
        }
        files.push(path);
    }
    // Inspect the entire directory before removing any of its files. Never
    // recurse: directories, links, and special files make the whole run exempt.
    for file in files {
        if !fs::symlink_metadata(path)?.file_type().is_dir()
            || !fs::symlink_metadata(&file)?.file_type().is_file()
        {
            return Err(io::Error::other(
                "Directory contents changed; keeping directory",
            ));
        }
        fs::remove_file(file)?;
    }
    fs::remove_dir(path)
}

/// Best-effort, shallow retention under the mirror lock. Zero disables all work.
pub fn prune_reports(root: &Path, keep: usize) -> usize {
    if keep == 0 {
        return 0;
    }
    let reports = root.join("reports");
    if !real_directory(&reports) {
        return 0;
    }
    let (protected, blocked) = protected_reports(root);
    let entries = match fs::read_dir(&reports) {
        Ok(entries) => entries,
        Err(error) => {
            retention_warning(&reports, error);
            return 0;
        }
    };
    let mut groups = [Vec::new(), Vec::new()];
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                retention_warning(&reports, error);
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let group = if name.starts_with("run-") {
            0
        } else if name.starts_with("verify-") {
            1
        } else {
            continue;
        };
        if blocked[group] {
            continue;
        }
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(meta) if meta.file_type().is_dir() => meta,
            Ok(_) => continue,
            Err(error) => {
                retention_warning(&entry.path(), error);
                continue;
            }
        };
        let stamp = timestamp_in_name(name, if group == 0 { "run-" } else { "verify-" });
        let modified = if stamp.is_none() {
            match metadata.modified() {
                Ok(time) => Some(time),
                Err(error) => {
                    retention_warning(&entry.path(), error);
                    continue;
                }
            }
        } else {
            None
        };
        groups[group].push((stamp, modified, name.to_owned()));
    }
    let mut pruned = 0;
    for mut group in groups {
        group.sort_by(|a, b| b.cmp(a));
        for (_, _, name) in group.into_iter().skip(keep) {
            let relative = Path::new("reports").join(name);
            if protected.contains(&relative) {
                continue;
            }
            let path = root.join(relative);
            match remove_report_directory(&path) {
                Ok(()) => pruned += 1,
                Err(error) => retention_warning(&path, error),
            }
        }
    }
    pruned
}

pub fn retention_summary(count: usize) -> String {
    if count > 0 {
        format!(" · {count} old report directories pruned")
    } else {
        String::new()
    }
}

pub fn announce_pruned(count: usize, quiet: bool) {
    if count > 0 && !quiet {
        eprintln!("Pruned {count} old report directories");
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    use serde_json::json;
    use std::{
        path::PathBuf,
        time::{Duration, SystemTime},
    };

    fn directory(root: &Path, name: &str) -> PathBuf {
        let path = root.join("reports").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("detail.jsonl"), b"{}\n").unwrap();
        path
    }
    fn report(root: &Path, name: &str, value: serde_json::Value) {
        fs::write(root.join(name), serde_json::to_vec(&value).unwrap()).unwrap();
    }
    #[test]
    fn utc_names_are_sortable_and_still_use_unique_tempfile_suffixes() {
        assert_eq!(report_timestamp(SystemTime::UNIX_EPOCH), "19700101T000000Z");
        let leap = httpdate::parse_http_date("Thu, 29 Feb 2024 23:59:59 GMT").unwrap();
        assert_eq!(report_timestamp(leap), "20240229T235959Z");
        assert_eq!(
            report_timestamp(leap + Duration::from_secs(1)),
            "20240301T000000Z"
        );
        let root = tempfile::tempdir().unwrap();
        for prefix in ["run-", "verify-"] {
            let first = crate::mirror::create_report_directory(root.path(), prefix).unwrap();
            let second = crate::mirror::create_report_directory(root.path(), prefix).unwrap();
            assert_ne!(first, second);
            assert!(
                timestamp_in_name(first.file_name().unwrap().to_str().unwrap(), prefix).is_some()
            );
        }
        for name in [
            "run-XbiKar",
            "run-20240230T000000Z-x",
            "run-20240101T240000Z-x",
            "run-20240101T000000Z-",
            "run-🦀",
        ] {
            assert!(timestamp_in_name(name, "run-").is_none(), "{name}");
        }
    }
    #[test]
    fn keeps_newest_per_group_independently_and_prunes_legacy_first() {
        let root = tempfile::tempdir().unwrap();
        for prefix in ["run-", "verify-"] {
            directory(root.path(), &format!("{prefix}legacy"));
            directory(root.path(), &format!("{prefix}20230101T000000Z-old"));
            directory(root.path(), &format!("{prefix}20240101T000000Z-new"));
        }
        assert_eq!(prune_reports(root.path(), 2), 2);
        for prefix in ["run-", "verify-"] {
            assert!(!root.path().join(format!("reports/{prefix}legacy")).exists());
            assert!(
                root.path()
                    .join(format!("reports/{prefix}20230101T000000Z-old"))
                    .exists()
            );
        }
        assert_eq!(prune_reports(root.path(), 1), 2);
        assert_eq!(
            fs::read_dir(root.path().join("reports")).unwrap().count(),
            2
        );
    }
    #[test]
    #[cfg(unix)]
    fn legacy_directory_order_uses_mtime_and_is_older_than_any_timestamp() {
        let root = tempfile::tempdir().unwrap();
        let older = directory(root.path(), "run-z-older");
        let newer = directory(root.path(), "run-a-newer");
        fs::File::open(&older)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(100))
            .unwrap();
        fs::File::open(&newer)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(200))
            .unwrap();
        assert_eq!(prune_reports(root.path(), 1), 1);
        assert!(!older.exists());
        assert!(newer.exists());
        let stamped = directory(root.path(), "run-19700101T000000Z-stamped");
        assert_eq!(prune_reports(root.path(), 1), 1);
        assert!(!newer.exists());
        assert!(stamped.exists());
    }
    #[test]
    fn all_current_reports_protect_normalized_references_even_when_oldest() {
        let root = tempfile::tempdir().unwrap();
        let legacy = directory(root.path(), "run-legacy");
        for prefix in ["run-", "verify-"] {
            for year in [2022, 2023, 2024] {
                directory(root.path(), &format!("{prefix}{year}0101T000000Z-x"));
            }
        }
        report(
            root.path(),
            "check-report.json",
            json!({"packages_path":"reports/placeholder/../run-20220101T000000Z-x/./checked-packages.jsonl"}),
        );
        report(
            root.path(),
            "mirror-report.json",
            json!({"inspection":{"packages_path":"./reports/run-20230101T000000Z-x/checked-packages.jsonl"}, "downloads":{"outcomes_path":"reports/run-20230101T000000Z-x/download-outcomes.jsonl"}}),
        );
        report(
            root.path(),
            "verify-report.json",
            json!({"problems_path":"reports/verify-20220101T000000Z-x/problems.jsonl"}),
        );
        assert_eq!(prune_reports(root.path(), 1), 2);
        assert!(!legacy.exists());
        for name in [
            "run-20220101T000000Z-x",
            "run-20230101T000000Z-x",
            "run-20240101T000000Z-x",
            "verify-20220101T000000Z-x",
            "verify-20240101T000000Z-x",
        ] {
            assert!(root.path().join("reports").join(name).is_dir(), "{name}");
        }
    }
    #[test]
    fn malformed_reports_disable_only_their_group_and_zero_disables_everything() {
        for (filename, blocked_prefix, other_prefix) in [
            ("check-report.json", "run-", "verify-"),
            ("mirror-report.json", "run-", "verify-"),
            ("verify-report.json", "verify-", "run-"),
        ] {
            for contents in ["{", "{}", "[]", r#"{"packages_path":"../escape"}"#] {
                let root = tempfile::tempdir().unwrap();
                for prefix in [blocked_prefix, other_prefix] {
                    for year in [2023, 2024] {
                        directory(root.path(), &format!("{prefix}{year}0101T000000Z-x"));
                    }
                }
                fs::write(root.path().join(filename), contents).unwrap();
                assert_eq!(prune_reports(root.path(), 0), 0);
                assert_eq!(
                    fs::read_dir(root.path().join("reports")).unwrap().count(),
                    4
                );
                assert_eq!(prune_reports(root.path(), 1), 1);
                assert!(
                    root.path()
                        .join(format!("reports/{blocked_prefix}20230101T000000Z-x"))
                        .exists()
                );
                assert!(
                    !root
                        .path()
                        .join(format!("reports/{other_prefix}20230101T000000Z-x"))
                        .exists()
                );
            }
        }
    }
    #[test]
    #[cfg(unix)]
    fn never_follows_links_or_removes_directories_with_nonregular_contents() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), b"keep").unwrap();
        directory(root.path(), "run-20240101T000000Z-new");
        let safe_old = directory(root.path(), "run-20200101T000000Z-old");
        let nested = directory(root.path(), "run-20210101T000000Z-nested");
        fs::create_dir(nested.join("subdir")).unwrap();
        let linked = directory(root.path(), "run-20220101T000000Z-linked");
        symlink(outside.path().join("keep"), linked.join("link")).unwrap();
        symlink(
            outside.path(),
            root.path().join("reports/run-linked-directory"),
        )
        .unwrap();
        let unrelated = directory(root.path(), "other-directory");
        let special = directory(root.path(), "run-20230101T000000Z-special");
        let _socket = std::os::unix::net::UnixListener::bind(special.join("socket")).unwrap();
        let prefixed_file = root.path().join("reports/run-not-a-directory");
        fs::write(&prefixed_file, b"keep").unwrap();
        assert_eq!(prune_reports(root.path(), 1), 1);
        assert!(prefixed_file.is_file());
        assert!(!safe_old.exists());
        for path in [nested, linked, unrelated, special] {
            assert!(path.join("detail.jsonl").exists());
        }
        assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"keep");
        assert!(
            fs::symlink_metadata(root.path().join("reports/run-linked-directory"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let second = tempfile::tempdir().unwrap();
        symlink(root.path().join("reports"), second.path().join("reports")).unwrap();
        assert_eq!(prune_reports(second.path(), 1), 0);
    }
    #[test]
    #[cfg(unix)]
    fn a_symlinked_top_level_report_prevents_pruning_its_group() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("check-report.json")).unwrap();
        for year in [2023, 2024] {
            directory(root.path(), &format!("run-{year}0101T000000Z-x"));
        }
        assert_eq!(prune_reports(root.path(), 1), 0);
    }
}
