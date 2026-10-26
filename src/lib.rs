//! Discover PyPI projects, load release metadata, and check local mirror files.

pub mod download;
mod filename;
mod http;
pub mod mirror;
pub mod selection;
pub mod simple;

pub mod cleanup;
pub mod interrupt;

use std::{
    io::{BufReader, Read},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use url::Url;

pub const DEFAULT_INDEX_URL: &str = "https://pypi.org/simple/";
const JSON_MEDIA_TYPE: &str = "application/vnd.pypi.simple.v1+json";

/// Progress from actual reads and package construction, independent of any UI.
#[derive(Debug)]
pub enum IndexProgress<'a> {
    Reading {
        bytes: u64,
    },
    Packages {
        processed: usize,
        total: usize,
        name: &'a str,
    },
}

/// Knowledge available about a package from the root Simple Index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    pub normalized_name: String,
    pub index_url: Url,
    /// PyPI extension; other indexes may omit it.
    pub last_serial: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct PackageIndex {
    pub index_url: Url,
    pub api_version: String,
    pub last_serial: Option<u64>,
    pub packages: Vec<Package>,
}

impl PackageIndex {
    /// Parse a saved response or an HTTP response using the same code path.
    pub fn from_reader(reader: impl Read, index_url: Url) -> Result<Self> {
        Self::from_reader_with_progress(reader, index_url, |_| {})
    }

    pub fn from_reader_with_progress(
        reader: impl Read,
        index_url: Url,
        mut progress: impl FnMut(IndexProgress<'_>),
    ) -> Result<Self> {
        let index_url = normalize_index_url(index_url)?;
        let reader = ProgressReader {
            reader,
            bytes: 0,
            progress: &mut progress,
        };
        let raw: RawIndex = serde_json::from_reader(BufReader::new(reader))
            .map_err(http::json_error)
            .context("Failed to parse the Simple Index JSON response")?;
        ensure!(
            raw.meta.api_version.split('.').next() == Some("1"),
            "Unsupported Simple Index API version: {}",
            raw.meta.api_version
        );
        let total = raw.projects.len();
        progress(IndexProgress::Packages {
            processed: 0,
            total,
            name: "",
        });
        let packages = raw
            .projects
            .into_iter()
            .enumerate()
            .map(|(position, project)| {
                let normalized_name = normalize_name(&project.name)?;
                let package_url = index_url.join(&format!("{normalized_name}/"))?;
                progress(IndexProgress::Packages {
                    processed: position + 1,
                    total,
                    name: &project.name,
                });
                Ok(Package {
                    name: project.name,
                    normalized_name,
                    index_url: package_url,
                    last_serial: project.last_serial,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            index_url,
            api_version: raw.meta.api_version,
            last_serial: raw.meta.last_serial,
            packages,
        })
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Package> {
        self.packages.iter()
    }
}

impl<'a> IntoIterator for &'a PackageIndex {
    type Item = &'a Package;
    type IntoIter = std::slice::Iter<'a, Package>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Reuse one HTTP client across future discovery and metadata operations.
pub struct IndexClient {
    client: Client,
    attempts: u32,
    interrupt: interrupt::StopToken,
}

impl IndexClient {
    pub fn new() -> Result<Self> {
        Self::with_attempts(http::DEFAULT_ATTEMPTS)
    }

    pub fn with_attempts(attempts: u32) -> Result<Self> {
        ensure!(
            (1..=10).contains(&attempts),
            "Request attempts must be between 1 and 10"
        );
        Ok(Self {
            client: http::client_builder()
                .timeout(Duration::from_secs(180))
                .gzip(true)
                .build()?,
            attempts,
            interrupt: interrupt::StopToken::default(),
        })
    }

    pub fn with_interrupt(mut self, interrupt: interrupt::StopToken) -> Self {
        self.interrupt = interrupt;
        self
    }

    pub fn fetch(&self, index_url: Url) -> Result<PackageIndex> {
        self.fetch_with_progress(index_url, |_| {})
    }

    pub fn fetch_with_progress(
        &self,
        index_url: Url,
        mut progress: impl FnMut(IndexProgress<'_>),
    ) -> Result<PackageIndex> {
        let index_url = normalize_index_url(index_url)?;
        http::get(
            &self.client,
            &index_url,
            JSON_MEDIA_TYPE,
            self.attempts,
            &self.interrupt,
            |response| {
                let effective_url = response.url().clone();
                PackageIndex::from_reader_with_progress(response, effective_url, &mut progress)
            },
        )
        .context("Failed to fetch the package index")
    }
}

struct ProgressReader<'a, R, F> {
    reader: R,
    bytes: u64,
    progress: &'a mut F,
}

impl<R: Read, F: FnMut(IndexProgress<'_>)> Read for ProgressReader<'_, R, F> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let size = self.reader.read(buffer)?;
        self.bytes += size as u64;
        (self.progress)(IndexProgress::Reading { bytes: self.bytes });
        Ok(size)
    }
}

/// Apply the PyPA package-name validation and normalization rules.
pub fn normalize_name(name: &str) -> Result<String> {
    let bytes = name.as_bytes();
    ensure!(
        bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
        "Invalid package name: {name:?}"
    );
    let mut normalized = String::with_capacity(name.len());
    let mut separator = false;
    for byte in bytes {
        if matches!(byte, b'.' | b'_' | b'-') {
            if !separator {
                normalized.push('-');
            }
            separator = true;
        } else {
            normalized.push(byte.to_ascii_lowercase() as char);
            separator = false;
        }
    }
    Ok(normalized)
}

pub fn normalize_index_url(mut url: Url) -> Result<Url> {
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "Index URL must use HTTP or HTTPS"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "Index URL must not contain a query or fragment"
    );
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

#[derive(Deserialize)]
struct RawIndex {
    meta: RawMeta,
    projects: Vec<RawProject>,
}

#[derive(Deserialize)]
struct RawMeta {
    #[serde(rename = "api-version")]
    api_version: String,
    #[serde(rename = "_last-serial")]
    last_serial: Option<u64>,
}

#[derive(Deserialize)]
struct RawProject {
    name: String,
    #[serde(rename = "_last-serial")]
    last_serial: Option<u64>,
}
