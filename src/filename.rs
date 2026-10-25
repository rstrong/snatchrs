//! Associate flat Simple API files with the versions advertised by the index.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail, ensure};
use pep440_rs::Version;

use crate::normalize_name;

pub(crate) struct ReleaseNames {
    advertised: bool,
    names: BTreeSet<String>,
    normalized: BTreeMap<Version, Vec<String>>,
    aliases: Vec<(String, String)>,
}

impl ReleaseNames {
    pub fn new(versions: Option<Vec<String>>) -> Result<Self> {
        let advertised = versions.is_some();
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
            advertised,
            names,
            normalized,
            aliases,
        })
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.names.iter()
    }

    pub fn version(&self, filename: &str, project: &str) -> Result<String> {
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
        };
        if let Some(candidate) = candidate {
            ensure!(
                !candidate.is_empty(),
                "Empty filename version in {filename}"
            );
            if self.names.contains(candidate) {
                return Ok(candidate.to_owned());
            }
            if let Ok(version) = candidate.parse::<Version>() {
                if let Some(names) = self.normalized.get(&version) {
                    ensure!(
                        names.len() == 1,
                        "Ambiguous release for {filename}: {names:?}"
                    );
                    return Ok(names[0].clone());
                }
                if self.advertised {
                    bail!(
                        "File {filename} has a version absent from the Simple Index versions list"
                    );
                }
            }
            if !self.advertised {
                return Ok(candidate.to_owned());
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
                    return Ok(original.clone());
                }
            }
        }
        // The API guarantees every file belongs to a listed version. A single
        // advertised version is unambiguous even for unusual legacy filenames.
        if self.names.len() == 1 {
            return Ok(self.names.first().unwrap().clone());
        }
        bail!("Cannot associate {filename} with a release from the Simple Index")
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
