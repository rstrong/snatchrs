use std::{
    collections::BTreeMap,
    fs,
    io::{BufReader, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rand::{SeedableRng, rngs::StdRng, seq::IteratorRandom};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    IndexClient, JSON_MEDIA_TYPE, Package, PackageIndex,
    filename::{ReleaseNames, package_type},
    normalize_index_url, normalize_name,
};

pub const DEFAULT_METADATA_URL: &str = crate::DEFAULT_INDEX_URL;

/// A release can contain many wheels and source distributions, or no files.
#[derive(Debug, Serialize)]
pub struct Release {
    pub version: String,
    pub files: Vec<DistributionFile>,
}

#[derive(Debug, Serialize)]
pub struct DistributionFile {
    pub filename: String,
    pub download_url: Url,
    pub local_path: PathBuf,
    pub size: u64,
    pub sha256: Option<String>,
    pub package_type: Option<String>,
    pub requires_python: Option<String>,
    pub yanked: bool,
}

#[derive(Debug, Serialize)]
pub struct PackageMetadata {
    pub package: Package,
    pub metadata_url: Url,
    pub metadata_path: PathBuf,
    pub package_path: PathBuf,
    pub last_serial: Option<u64>,
    /// Ordered lexically by version, not by Python version precedence.
    pub releases: Vec<Release>,
}

impl PackageMetadata {
    pub fn from_reader(
        reader: impl std::io::Read,
        package: Package,
        metadata_url: Url,
        layout: &MirrorLayout,
    ) -> Result<Self> {
        let raw: RawMetadata = serde_json::from_reader(BufReader::new(reader))
            .map_err(crate::http::json_error)
            .context("Invalid per-project Simple Index JSON metadata")?;
        ensure!(
            raw.meta.api_version.split('.').next() == Some("1"),
            "Unsupported Simple Index API version: {}",
            raw.meta.api_version
        );
        ensure!(
            normalize_name(&raw.name)? == package.normalized_name,
            "Metadata package name does not match {}",
            package.name
        );
        let package_path = layout.package_path(&package.normalized_name)?;
        let names = ReleaseNames::new(raw.versions)?;
        let mut releases: BTreeMap<String, Vec<DistributionFile>> = names
            .names()
            .map(|name| (name.clone(), Vec::new()))
            .collect();
        for file in raw.files {
            validate_filename(&file.filename)?;
            let url = metadata_url
                .join(&file.url)
                .context("Invalid distribution URL")?;
            ensure!(
                matches!(url.scheme(), "https" | "http"),
                "File URL must use HTTP or HTTPS"
            );
            let version = names.version(&file.filename, &package.normalized_name)?;
            let package_type = package_type(&file.filename);
            releases.entry(version).or_default().push(DistributionFile {
                local_path: package_path.join(&file.filename),
                filename: file.filename,
                download_url: url,
                size: file.size,
                sha256: file.hashes.get("sha256").cloned(),
                package_type,
                requires_python: file.requires_python,
                yanked: file.yanked.is_yanked(),
            });
        }
        Ok(Self {
            metadata_path: layout
                .root
                .join("metadata")
                .join(format!("{}.json", package.normalized_name)),
            package_path,
            package,
            metadata_url,
            last_serial: raw.meta.last_serial,
            releases: releases
                .into_iter()
                .map(|(version, files)| Release { version, files })
                .collect(),
        })
    }
}

impl IndexClient {
    /// One per-project Simple JSON request covers all advertised files/versions.
    pub fn fetch_metadata(
        &self,
        package: Package,
        metadata_base: Url,
        layout: &MirrorLayout,
    ) -> Result<PackageMetadata> {
        let base = normalize_index_url(metadata_base)?;
        let url = base.join(&format!("{}/", normalize_name(&package.name)?))?;
        crate::http::get(
            &self.client,
            &url,
            JSON_MEDIA_TYPE,
            self.attempts,
            |response| {
                let effective_url = response.url().clone();
                PackageMetadata::from_reader(response, package.clone(), effective_url, layout)
            },
        )
        .context("Failed to fetch project metadata")
    }
}

/// The mirror stores distributions under packages/<normalized-name>/<filename>.
pub struct MirrorLayout {
    pub root: PathBuf,
}

impl MirrorLayout {
    /// Keep metadata writers and downloaders from overlapping in the same root.
    pub fn lock(&self) -> Result<fs::File> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(".snatchrs.lock"))?;
        fs2::FileExt::try_lock_exclusive(&file)
            .context("Another snatchrs command is using this mirror")?;
        Ok(file)
    }
    pub fn create(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("Cannot create mirror {}", root.display()))?;
        let root = fs::canonicalize(root)?;
        let layout = Self { root };
        create_directory(&layout.root.join("packages"))?;
        create_directory(&layout.root.join("metadata"))?;
        Ok(layout)
    }

    pub fn package_path(&self, name: &str) -> Result<PathBuf> {
        Ok(self.root.join("packages").join(normalize_name(name)?))
    }

    pub fn save_metadata(&self, metadata: &PackageMetadata) -> Result<()> {
        create_directory(&metadata.package_path)?;
        write_json(&metadata.metadata_path, metadata)
    }
}

fn create_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                fs::symlink_metadata(path)?.file_type().is_dir(),
                "Expected a real directory: {}",
                path.display()
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Write through a temporary file, preserving an existing report on failure.
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .context("JSON output has no parent directory")?;
    let mut file = tempfile::Builder::new()
        .prefix(".snatchrs-json-")
        .tempfile_in(parent)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    writeln!(file)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FileState {
    Missing,
    SizeMismatch { actual_size: u64 },
    MatchingSize,
    NotRegularFile,
    Error { message: String },
}

impl FileState {
    pub fn needs_download(&self) -> bool {
        matches!(self, Self::Missing | Self::SizeMismatch { .. })
    }
}

pub fn inspect_file(file: &DistributionFile) -> FileState {
    match fs::symlink_metadata(&file.local_path) {
        Ok(metadata) if !metadata.file_type().is_file() => FileState::NotRegularFile,
        Ok(metadata) if metadata.len() != file.size => FileState::SizeMismatch {
            actual_size: metadata.len(),
        },
        Ok(_) => FileState::MatchingSize,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileState::Missing,
        Err(error) => FileState::Error {
            message: error.to_string(),
        },
    }
}

#[derive(Debug, Serialize)]
pub struct CheckedFile {
    pub version: String,
    pub filename: String,
    pub local_path: PathBuf,
    #[serde(flatten)]
    pub state: FileState,
}

#[derive(Debug, Serialize)]
pub struct CheckedPackage {
    pub metadata: PackageMetadata,
    pub files: Vec<CheckedFile>,
}

#[derive(Debug, Default, Serialize)]
pub struct CheckSummary {
    pub packages: usize,
    pub releases: usize,
    pub files: usize,
    pub expected_bytes: u64,
    pub missing: usize,
    pub size_mismatch: usize,
    pub matching_size: usize,
    pub blocked: usize,
}

/// Placeholder only: count candidates without storing jobs or downloading files.
#[derive(Debug, Default, Serialize)]
pub struct DownloadQueueStub {
    pub candidates_seen: usize,
}

impl DownloadQueueStub {
    pub fn consider(&mut self, _file: &DistributionFile, state: &FileState) {
        if state.needs_download() {
            self.candidates_seen += 1;
        }
    }
}

pub fn check_package(
    metadata: PackageMetadata,
    summary: &mut CheckSummary,
    queue: &mut DownloadQueueStub,
) -> CheckedPackage {
    summary.packages += 1;
    summary.releases += metadata.releases.len();
    let mut files = Vec::new();
    for release in &metadata.releases {
        for file in &release.files {
            let state = inspect_file(file);
            summary.files += 1;
            summary.expected_bytes += file.size;
            match &state {
                FileState::Missing => summary.missing += 1,
                FileState::SizeMismatch { .. } => summary.size_mismatch += 1,
                FileState::MatchingSize => summary.matching_size += 1,
                FileState::NotRegularFile | FileState::Error { .. } => summary.blocked += 1,
            }
            queue.consider(file, &state);
            files.push(CheckedFile {
                version: release.version.clone(),
                filename: file.filename.clone(),
                local_path: file.local_path.clone(),
                state,
            });
        }
    }
    CheckedPackage { metadata, files }
}

pub fn sample_packages(index: &PackageIndex, count: usize, seed: u64) -> Vec<Package> {
    let mut rng = StdRng::seed_from_u64(seed);
    index
        .iter()
        .choose_multiple(&mut rng, count)
        .into_iter()
        .cloned()
        .collect()
}

#[derive(Deserialize)]
struct RawMetadata {
    name: String,
    meta: RawMeta,
    versions: Option<Vec<String>>,
    files: Vec<RawFile>,
}

#[derive(Deserialize)]
struct RawMeta {
    #[serde(rename = "api-version")]
    api_version: String,
    #[serde(rename = "_last-serial")]
    last_serial: Option<u64>,
}

#[derive(Deserialize)]
struct RawFile {
    filename: String,
    url: String,
    size: u64,
    hashes: BTreeMap<String, String>,
    #[serde(rename = "requires-python")]
    requires_python: Option<String>,
    #[serde(default)]
    yanked: Yanked,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Yanked {
    Flag(bool),
    Reason(String),
}
impl Default for Yanked {
    fn default() -> Self {
        Self::Flag(false)
    }
}
impl Yanked {
    fn is_yanked(&self) -> bool {
        match self {
            Self::Flag(flag) => *flag,
            Self::Reason(reason) => !reason.is_empty(),
        }
    }
}

fn validate_filename(filename: &str) -> Result<()> {
    ensure!(
        !filename.is_empty()
            && filename != "."
            && filename != ".."
            && !filename
                .chars()
                .any(|c| matches!(c, '/' | '\\' | ':') || c.is_control()),
        "Invalid distribution filename: {filename:?}"
    );
    Ok(())
}
