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
