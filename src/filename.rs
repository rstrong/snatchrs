//! Associate flat Simple API files with the versions advertised by the index.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use pep440_rs::Version;

use crate::normalize_name;

pub(crate) struct ReleaseNames {
    names: BTreeSet<String>,
    normalized: BTreeMap<Version, Vec<String>>,
    aliases: Vec<(String, String)>,
}

impl ReleaseNames {
    pub fn new(versions: Option<Vec<String>>) -> Result<Self> {
        let names: BTreeSet<_> = versions.unwrap_or_default().into_iter().collect();
        let mut normalized: BTreeMap<Version, Vec<String>> = BTreeMap::new();
        let mut aliases = Vec::new();
        for name in &names {
            ensure!(!name.is_empty(), "Simple Index contains an empty version");
            aliases.push((name.to_ascii_lowercase(), name.clone()));
            if let Ok(version) = name.parse::<Version>() {
                aliases.push((version.to_string(), name.clone()));
                normalized.entry(version).or_default().push(name.clone());
            }
        }
        aliases.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.cmp(b)));
        Ok(Self {
            names,
            normalized,
            aliases,
        })
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.names.iter()
    }

    pub fn version(&self, filename: &str, project: &str) -> String {
        let candidate = if let Some(stem) = filename.strip_suffix(".whl") {
            let parts: Vec<_> = stem.split('-').collect();
            if matches!(parts.len(), 5 | 6)
                && normalize_name(parts[0]).is_ok_and(|name| name == project)
            {
                Some(parts[1])
            } else {
                None
            }
        } else {
            archive_stem(filename)
                .and_then(|stem| project_suffix(stem, project))
                .or_else(|| {
                    filename
                        .strip_suffix(".egg")
                        .and_then(|stem| project_suffix(stem, project))
                        .map(|suffix| {
                            suffix
                                .split_once("-py")
                                .map_or(suffix, |(version, _)| version)
                        })
                })
                .or_else(|| {
                    filename
                        .strip_suffix(".exe")
                        .or_else(|| filename.strip_suffix(".msi"))
                        .and_then(|stem| project_suffix(stem, project))
                        .map(|suffix| {
                            suffix
                                .split_once(".win")
                                .map_or(suffix, |(version, _)| version)
                        })
                })
        }
        .filter(|candidate| !candidate.is_empty());
        if let Some(candidate) = candidate {
            if self.names.contains(candidate) {
                return candidate.to_owned();
            }
            if let Ok(version) = candidate.parse::<Version>()
                && let Some(names) = self.normalized.get(&version)
            {
                // Equivalent advertised spellings are sorted by name.
                // The label is informational, so choose deterministically.
                return names[0].clone();
            }
        }
        // Legacy eggs/installers may append platform or Python tags. Prefer
        // the longest advertised version, avoiding 1.0 versus 1.0.1 collisions.
        if let Some(suffix) = project_suffix(filename, project) {
            let suffix = suffix.to_ascii_lowercase();
            for (alias, original) in &self.aliases {
                if suffix
                    .strip_prefix(alias)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(['-', '.', '_']))
                {
                    return original.clone();
                }
            }
        }
        // A single advertised version is a useful label even for an unusual
        // filename. Association never determines whether a file is mirrored.
        if self.names.len() == 1 {
            return self.names.first().unwrap().clone();
        }
        // Preserve every file even when the versions list and filenames disagree.
        candidate.unwrap_or("unknown").to_owned()
    }
}

fn project_suffix<'a>(filename: &'a str, project: &str) -> Option<&'a str> {
    filename.match_indices('-').find_map(|(position, _)| {
        normalize_name(&filename[..position])
            .ok()
            .filter(|name| name == project)
            .map(|_| &filename[position + 1..])
    })
}

fn archive_stem(filename: &str) -> Option<&str> {
    [
        ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.Z", ".tgz", ".zip", ".tar",
    ]
    .into_iter()
    .find_map(|suffix| filename.strip_suffix(suffix))
}

pub(crate) fn package_type(filename: &str) -> Option<String> {
    let kind = if filename.ends_with(".whl") {
        "bdist_wheel"
    } else if archive_stem(filename).is_some() {
        "sdist"
    } else if filename.ends_with(".egg") {
        "bdist_egg"
    } else if filename.ends_with(".exe") {
        "bdist_wininst"
    } else if filename.ends_with(".msi") {
        "bdist_msi"
    } else if filename.ends_with(".rpm") {
        "bdist_rpm"
    } else {
        return None;
    };
    Some(kind.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(versions: &[&str]) -> ReleaseNames {
        ReleaseNames::new(Some(
            versions.iter().map(|name| (*name).to_owned()).collect(),
        ))
        .unwrap()
    }

    #[test]
    fn legacy_numpy_installer_uses_the_longest_advertised_prefix() {
        let filename = "numpy-1.0.1.dev3460.win32-py2.4.exe";
        assert_eq!(names(&["1.0", "1.0.1"]).version(filename, "numpy"), "1.0.1");
        assert_eq!(names(&["1.0"]).version(filename, "numpy"), "1.0");
    }

    #[test]
    fn unadvertised_wheel_keeps_its_filename_version() {
        assert_eq!(
            names(&["1.0", "2.0"]).version("numpy-3.0-py3-none-any.whl", "numpy"),
            "3.0"
        );
    }

    #[test]
    fn unmatched_names_use_unknown_or_the_single_advertised_release() {
        assert_eq!(
            names(&["1.0", "2.0"]).version("opaque.blob", "numpy"),
            "unknown"
        );
        assert_eq!(names(&["1.0"]).version("opaque.blob", "numpy"), "1.0");
        assert_eq!(names(&[]).version("numpy-.tar.gz", "numpy"), "unknown");
        assert_eq!(
            ReleaseNames::new(None)
                .unwrap()
                .version("opaque.blob", "numpy"),
            "unknown"
        );
    }

    #[test]
    fn equivalent_advertised_spellings_choose_the_first_sorted_name() {
        assert_eq!(
            names(&["1.0.0", "1.0"]).version("numpy-1.0.00-py3-none-any.whl", "numpy"),
            "1.0"
        );
        assert_eq!(
            names(&["1.0.0", "1.0"]).version("numpy-1.0.0-py3-none-any.whl", "numpy"),
            "1.0.0"
        );
    }

    #[test]
    fn real_pypi_legacy_filename_shapes_always_get_a_label() {
        // Deliberately unrelated advertised releases exercise the final fallback
        // rather than letting the single-version or prefix fallback hide a bug.
        let releases = names(&["8.0", "9.0"]);
        for (project, filename, expected) in [
            ("pytest-pycharm", "pytest-pycharm-0.2.0.tar.gz", "0.2.0"),
            ("piptui", "PipTUI-0.3.1dev1.tar.gz", "0.3.1dev1"),
            ("weboob", "weboob-1.3+1.tar.gz", "1.3+1"),
            ("ppb-vector", "ppb_vector-0.2-py3-none-any.whl", "0.2"),
            (
                "dap-plugins-grads",
                "dap.plugins.grads-0.1dev_r201-py2.4.egg",
                "0.1dev_r201",
            ),
            // Spaces are valid in a safe filename, but not in a normalized
            // project name. Keep the file with an informational unknown label.
            (
                "car-crash-detection",
                "Car Crash Detection-0.0.1.tar.gz",
                "unknown",
            ),
        ] {
            assert_eq!(releases.version(filename, project), expected, "{filename}");
        }
    }

    #[test]
    fn empty_advertised_versions_are_still_corrupt_metadata() {
        assert!(ReleaseNames::new(Some(vec![String::new()])).is_err());
    }
}
