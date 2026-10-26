//! Static HTML Simple API pages for nginx, pip, and uv.
use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::{
    mirror::{
        FileState, PackageMetadata, create_directory, inspect_file, resolve_path, validate_filename,
    },
    normalize_name,
};

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PageError {
    pub name: String,
    pub error: String,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct SimpleSummary {
    pub stale_temp_files_removed: usize,
    pub projects: usize,
    pub files_linked: usize,
    pub files_skipped: usize,
    pub projects_removed: usize,
    pub page_errors: usize,
    pub errors: Vec<PageError>,
}
impl SimpleSummary {
    pub fn record_error(&mut self, name: impl Into<String>, error: anyhow::Error) {
        self.page_errors += 1;
        if self.errors.len() < 100 {
            self.errors.push(PageError {
                name: name.into(),
                error: format!("{error:#}"),
            });
        }
    }
    pub fn failure_message(&self) -> String {
        format!(
            "{} index/page errors (showing {}): {}",
            self.page_errors,
            self.errors.len(),
            self.errors
                .iter()
                .map(|error| format!("{}: {}", error.name, error.error))
                .collect::<Vec<_>>()
                .join("; ")
        )
    }
    pub fn add(&mut self, page: Self) {
        self.stale_temp_files_removed += page.stale_temp_files_removed;
        self.projects += page.projects;
        self.files_linked += page.files_linked;
        self.files_skipped += page.files_skipped;
        self.projects_removed += page.projects_removed;
        self.page_errors += page.page_errors;
        self.errors.extend(
            page.errors
                .into_iter()
                .take(100usize.saturating_sub(self.errors.len())),
        );
    }
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

// Encode UTF-8 bytes, allowing only RFC 3986 unreserved characters. In
// particular, '+' is a literal filename character, not a URL form-space.
fn encode_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    encoded
}

fn real_directory_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_dir(),
                "Expected a real directory: {}",
                path.display()
            );
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn atomic_html(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<&mut File>) -> Result<()>,
) -> Result<()> {
    let parent = path
        .parent()
        .context("HTML output has no parent directory")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".snatchrs-html-")
        .tempfile_in(parent)?;
    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        write(&mut writer)?;
        writer.flush()?;
    }
    // NamedTempFile is private by default; static HTML must be readable by nginx.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn header(writer: &mut impl Write, title: &str) -> Result<()> {
    let title = escape_html(title);
    writeln!(
        writer,
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"pypi:repository-version\" content=\"1.0\">\n<title>{title}</title>\n</head>\n<body>\n<h1>{title}</h1>"
    )?;
    Ok(())
}

/// Publish a project's available distributions in metadata order. No artifact
/// is installed, extracted, fetched, or deleted while generating HTML.
pub fn write_project_page(root: &Path, metadata: &PackageMetadata) -> Result<SimpleSummary> {
    let name = normalize_name(&metadata.package.name)?;
    ensure!(
        name == metadata.package.normalized_name,
        "Inconsistent normalized project name"
    );
    let stale_temp_files_removed = crate::cleanup::sweep_project(root, &name);
    let package_path = PathBuf::from("packages").join(&name);
    create_directory(&resolve_path(root, Path::new("packages"))?)?;
    real_directory_exists(&resolve_path(root, &package_path)?)?;
    let simple = resolve_path(root, Path::new("simple"))?;
    create_directory(&simple)?;
    let project = simple.join(&name);
    create_directory(&project)?;
    let mut summary = SimpleSummary {
        stale_temp_files_removed,
        projects: 1,
        ..Default::default()
    };
    atomic_html(&project.join("index.html"), |writer| {
        header(writer, &metadata.package.name)?;
        for file in metadata.releases.iter().flat_map(|release| &release.files) {
            validate_filename(&file.filename)?;
            ensure!(
                file.local_path == package_path.join(&file.filename),
                "Unexpected local distribution path: {}",
                file.local_path.display()
            );
            if inspect_file(root, file) != FileState::MatchingSize {
                summary.files_skipped += 1;
                continue;
            }
            let mut href = format!("../../packages/{name}/{}", encode_segment(&file.filename));
            if let Some(hash) = &file.sha256 {
                href.push_str("#sha256=");
                href.push_str(&encode_segment(hash));
            }
            write!(writer, "<a href=\"{}\"", escape_html(&href))?;
            if let Some(requires) = &file.requires_python {
                write!(
                    writer,
                    " data-requires-python=\"{}\"",
                    escape_html(requires)
                )?;
            }
            if file.yanked {
                write!(writer, " data-yanked=\"\"")?;
            }
            writeln!(writer, ">{}</a><br>", escape_html(&file.filename))?;
            summary.files_linked += 1;
        }
        writeln!(writer, "</body>\n</html>")?;
        Ok(())
    })?;
    Ok(summary)
}

/// Index all existing project pages, including projects from earlier samples.
/// Only project directory names are retained for sorting; metadata is not read.
pub fn project_names(root: &Path) -> Result<Vec<String>> {
    let simple = resolve_path(root, Path::new("simple"))?;
    if !real_directory_exists(&simple)? {
        return Ok(Vec::new());
    }
    let mut projects = Vec::new();
    for entry in fs::read_dir(&simple)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        // A broken/symlinked project must not prevent indexing healthy pages.
        // Never traverse links or include them in the public root index.
        if !kind.is_dir() {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Non-UTF-8 Simple Index project directory"))?;
        if !normalize_name(&name).is_ok_and(|normalized| normalized == name) {
            continue;
        }
        match fs::symlink_metadata(entry.path().join("index.html")) {
            Ok(metadata) => {
                if metadata.file_type().is_file() {
                    projects.push(name);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    projects.sort();
    Ok(projects)
}

/// Atomically rebuild the root page from existing project pages.
pub fn write_root_index(root: &Path) -> Result<usize> {
    let simple = resolve_path(root, Path::new("simple"))?;
    create_directory(&simple)?;
    let projects = project_names(root)?;
    atomic_html(&simple.join("index.html"), |writer| {
        header(writer, "Package index")?;
        for name in &projects {
            writeln!(writer, "<a href=\"{name}/\">{name}</a><br>")?;
        }
        writeln!(writer, "</body>\n</html>")?;
        Ok(())
    })?;
    Ok(projects.len())
}

/// Remove a project's HTML page and its directory if empty; retain other files.
pub fn remove_project_page(root: &Path, name: &str) -> Result<()> {
    let name = normalize_name(name)?;
    let simple = resolve_path(root, Path::new("simple"))?;
    if !real_directory_exists(&simple)? {
        return Ok(());
    }
    let project = simple.join(name);
    if !real_directory_exists(&project)? {
        return Ok(());
    }
    match fs::remove_file(project.join("index.html")) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match fs::remove_dir(project) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Regenerate project pages from one enriched metadata file at a time. The
/// caller must hold the mirror lock, as the CLI does for all mirror mutations.
pub fn regenerate(root: &Path) -> Result<SimpleSummary> {
    regenerate_interruptible(root, &crate::interrupt::StopToken::default())
}

pub fn regenerate_interruptible(
    root: &Path,
    interrupt: &crate::interrupt::StopToken,
) -> Result<SimpleSummary> {
    let metadata_dir = resolve_path(root, Path::new("metadata"))?;
    create_directory(&metadata_dir)?;
    let mut summary = SimpleSummary::default();
    for entry in fs::read_dir(&metadata_dir)? {
        if interrupt.is_interrupted() {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                summary.record_error("<metadata>", error.into());
                continue;
            }
        };
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let name = entry
            .path()
            .file_stem()
            .context("Metadata has no project name")?
            .to_string_lossy()
            .into_owned();
        let page = (|| -> Result<SimpleSummary> {
            ensure!(
                entry.file_type()?.is_file(),
                "Expected a regular metadata file: {}",
                entry.path().display()
            );
            let metadata: PackageMetadata = serde_json::from_reader(BufReader::new(File::open(
                entry.path(),
            )?))
            .with_context(|| format!("Invalid project metadata in {}", entry.path().display()))?;
            ensure!(
                entry.file_name() == format!("{}.json", metadata.package.normalized_name).as_str(),
                "Metadata filename does not match project name"
            );
            write_project_page(root, &metadata)
        })();
        match page {
            Ok(page) => summary.add(page),
            Err(error) => summary.record_error(name, error),
        }
    }
    if !interrupt.is_interrupted() {
        match project_names(root) {
            Ok(names) => {
                for name in names {
                    match fs::symlink_metadata(metadata_dir.join(format!("{name}.json"))) {
                        Ok(_) => {} // Existing but corrupt/unreadable metadata keeps its old page.
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            match remove_project_page(root, &name) {
                                Ok(()) => summary.projects_removed += 1,
                                Err(error) => summary.record_error(name, error),
                            }
                        }
                        Err(error) => summary.record_error(name, error.into()),
                    }
                }
            }
            Err(error) => summary.record_error("<root>", error),
        }
    }
    if let Err(error) = write_root_index(root) {
        summary.record_error("<root>", error);
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Package,
        mirror::{DistributionFile, MirrorLayout, Release},
    };
    use tempfile::tempdir;

    fn metadata(root: &Path, filenames: &[&str]) -> PackageMetadata {
        fs::create_dir_all(root.join("packages/pkg")).unwrap();
        PackageMetadata {
            package: Package {
                name: "Pkg".into(),
                normalized_name: "pkg".into(),
                index_url: "https://example.org/simple/pkg/".parse().unwrap(),
                last_serial: None,
            },
            metadata_url: "https://example.org/simple/pkg/".parse().unwrap(),
            metadata_source_url: None,
            metadata_path: "metadata/pkg.json".into(),
            package_path: "packages/pkg".into(),
            last_serial: None,
            releases: vec![Release {
                version: "1.0".into(),
                files: filenames
                    .iter()
                    .map(|name| DistributionFile {
                        filename: (*name).into(),
                        download_url: "https://example.org/file".parse().unwrap(),
                        local_path: Path::new("packages/pkg").join(name),
                        size: 4,
                        sha256: None,
                        package_type: None,
                        requires_python: None,
                        yanked: false,
                    })
                    .collect(),
            }],
        }
    }

    #[test]
    fn page_errors_are_counted_exactly_with_bounded_details_and_output() {
        let mut summary = SimpleSummary::default();
        for number in 0..150 {
            summary.record_error(format!("pkg-{number}"), anyhow::anyhow!("page failure"));
        }
        assert_eq!(summary.page_errors, 150);
        assert_eq!(summary.errors.len(), 100);
        let message = summary.failure_message();
        assert!(message.contains("150 index/page errors"));
        assert!(message.contains("pkg-99:"));
        assert!(!message.contains("pkg-100:"));
    }

    #[test]
    fn escapes_html_and_encodes_single_utf8_url_segments() {
        assert_eq!(escape_html("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
        assert_eq!(
            encode_segment("pkg-1.0+local-py3-none-any.whl"),
            "pkg-1.0%2Blocal-py3-none-any.whl"
        );
        assert_eq!(
            encode_segment("a+% #?&\"<>/café"),
            "a%2B%25%20%23%3F%26%22%3C%3E%2Fcaf%C3%A9"
        );
        assert_eq!(encode_segment("AZaz09-._~"), "AZaz09-._~");
    }

    #[test]
    fn links_only_matching_files_in_order_with_escaped_attributes() {
        let root = tempdir().unwrap();
        let mut data = metadata(
            root.path(),
            &[
                "pkg-1.0+local-py3-none-any.whl",
                "pkg&plain.tar.gz",
                "missing.whl",
                "wrong.whl",
            ],
        );
        let files = &mut data.releases[0].files;
        files[0].sha256 = Some("ab".repeat(32));
        files[0].requires_python = Some(">=3.8 & <4.0 \"quoted\"".into());
        files[0].yanked = true;
        for file in &files[..2] {
            fs::write(root.path().join(&file.local_path), b"data").unwrap();
        }
        fs::write(root.path().join(&files[3].local_path), b"x").unwrap();
        let summary = write_project_page(root.path(), &data).unwrap();
        assert_eq!(
            summary,
            SimpleSummary {
                projects: 1,
                files_linked: 2,
                files_skipped: 2,
                projects_removed: 0,
                ..Default::default()
            }
        );
        let html = fs::read_to_string(root.path().join("simple/pkg/index.html")).unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<meta name=\"pypi:repository-version\" content=\"1.0\">"));
        assert!(html.contains("<title>Pkg</title>"));
        assert!(html.contains("<h1>Pkg</h1>"));
        let links: Vec<_> = html
            .lines()
            .filter(|line| line.starts_with("<a "))
            .collect();
        assert_eq!(links.len(), 2);
        assert!(links[0].contains(&format!(
            "href=\"../../packages/pkg/pkg-1.0%2Blocal-py3-none-any.whl#sha256={}\"",
            "ab".repeat(32)
        )));
        assert!(
            links[0].contains("data-requires-python=\"&gt;=3.8 &amp; &lt;4.0 &quot;quoted&quot;\"")
        );
        assert!(links[0].contains("data-yanked=\"\""));
        assert!(links[0].ends_with(">pkg-1.0+local-py3-none-any.whl</a><br>"));
        assert!(links[1].contains("href=\"../../packages/pkg/pkg%26plain.tar.gz\""));
        assert!(links[1].ends_with(">pkg&amp;plain.tar.gz</a><br>"));
        assert!(!links[1].contains("#sha256"));
        assert!(!links[1].contains("data-requires-python"));
        assert!(!links[1].contains("data-yanked"));
        assert!(!html.contains("missing.whl"));
        assert!(!html.contains("wrong.whl"));
        assert_eq!(write_project_page(root.path(), &data).unwrap(), summary);
        assert_eq!(
            fs::read_to_string(root.path().join("simple/pkg/index.html")).unwrap(),
            html
        );
    }

    #[test]
    fn root_index_sorts_existing_pages_without_loading_metadata() {
        let root = tempdir().unwrap();
        for project in ["zulu", "empty", "alpha"] {
            fs::create_dir_all(root.path().join("simple").join(project)).unwrap();
            if project != "empty" {
                fs::write(
                    root.path().join("simple").join(project).join("index.html"),
                    b"page",
                )
                .unwrap();
            }
        }
        fs::write(root.path().join("simple/notes.txt"), b"notes").unwrap();
        assert_eq!(write_root_index(root.path()).unwrap(), 2);
        let html = fs::read_to_string(root.path().join("simple/index.html")).unwrap();
        assert!(html.find("href=\"alpha/\"").unwrap() < html.find("href=\"zulu/\"").unwrap());
        assert!(!html.contains("empty/"));
        assert!(!html.contains("notes.txt"));
        assert_eq!(write_root_index(root.path()).unwrap(), 2);
        assert_eq!(
            fs::read_to_string(root.path().join("simple/index.html")).unwrap(),
            html
        );
    }

    #[test]
    fn removing_a_page_preserves_distributions_and_is_idempotent() {
        let root = tempdir().unwrap();
        let data = metadata(root.path(), &["pkg.whl"]);
        let artifact = root.path().join("packages/pkg/pkg.whl");
        fs::write(&artifact, b"data").unwrap();
        write_project_page(root.path(), &data).unwrap();
        assert_eq!(write_root_index(root.path()).unwrap(), 1);
        remove_project_page(root.path(), "Pkg").unwrap();
        remove_project_page(root.path(), "pkg").unwrap();
        assert_eq!(write_root_index(root.path()).unwrap(), 0);
        assert_eq!(fs::read(artifact).unwrap(), b"data");
        assert!(!root.path().join("simple/pkg").exists());
        assert!(
            !fs::read_to_string(root.path().join("simple/index.html"))
                .unwrap()
                .contains("href=\"pkg/\"")
        );
    }

    #[test]
    fn removing_a_page_retains_nonempty_project_directories() {
        let root = tempdir().unwrap();
        let data = metadata(root.path(), &[]);
        write_project_page(root.path(), &data).unwrap();
        let extra = root.path().join("simple/pkg/notes.txt");
        fs::write(&extra, b"keep").unwrap();
        remove_project_page(root.path(), "pkg").unwrap();
        assert!(!root.path().join("simple/pkg/index.html").exists());
        assert_eq!(fs::read(&extra).unwrap(), b"keep");
        fs::remove_file(extra).unwrap();
        remove_project_page(root.path(), "pkg").unwrap();
        assert!(!root.path().join("simple/pkg").exists());
    }

    #[test]
    fn invalid_project_page_preserves_the_previous_atomic_output() {
        let root = tempdir().unwrap();
        let mut data = metadata(root.path(), &["pkg.whl"]);
        write_project_page(root.path(), &data).unwrap();
        let page = root.path().join("simple/pkg/index.html");
        let original = fs::read(&page).unwrap();
        data.releases[0].files[0].local_path = "../outside".into();
        assert!(write_project_page(root.path(), &data).is_err());
        assert_eq!(fs::read(page).unwrap(), original);
        assert_eq!(
            fs::read_dir(root.path().join("simple/pkg"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn regeneration_skips_nonmatching_files_after_cleanup() {
        let root = tempdir().unwrap();
        let layout = MirrorLayout::create(root.path()).unwrap();
        let data = metadata(root.path(), &["pkg.whl"]);
        layout.save_metadata(&data).unwrap();
        fs::write(root.path().join("packages/pkg/pkg.whl"), b"data").unwrap();
        assert_eq!(
            regenerate(root.path()).unwrap(),
            SimpleSummary {
                projects: 1,
                files_linked: 1,
                files_skipped: 0,
                projects_removed: 0,
                ..Default::default()
            }
        );
        fs::remove_file(root.path().join("packages/pkg/pkg.whl")).unwrap();
        assert_eq!(
            regenerate(root.path()).unwrap(),
            SimpleSummary {
                projects: 1,
                files_linked: 0,
                files_skipped: 1,
                projects_removed: 0,
                ..Default::default()
            }
        );
        assert!(
            !fs::read_to_string(root.path().join("simple/pkg/index.html"))
                .unwrap()
                .contains("<a ")
        );
    }

    #[cfg(unix)]
    #[test]
    fn html_is_readable_by_the_static_server() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir().unwrap();
        let data = metadata(root.path(), &[]);
        write_project_page(root.path(), &data).unwrap();
        write_root_index(root.path()).unwrap();
        for page in ["simple/index.html", "simple/pkg/index.html"] {
            assert_eq!(
                fs::metadata(root.path().join(page))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o644
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_simple_and_package_directories_are_rejected() {
        use std::os::unix::fs::symlink;
        let outside = tempdir().unwrap();
        let root = tempdir().unwrap();
        let data = metadata(root.path(), &[]);
        symlink(outside.path(), root.path().join("simple")).unwrap();
        assert!(write_project_page(root.path(), &data).is_err());
        assert!(write_root_index(root.path()).is_err());
        assert!(remove_project_page(root.path(), "pkg").is_err());
        fs::remove_file(root.path().join("simple")).unwrap();
        fs::create_dir(root.path().join("simple")).unwrap();
        symlink(outside.path(), root.path().join("simple/pkg")).unwrap();
        assert!(write_project_page(root.path(), &data).is_err());
        assert_eq!(write_root_index(root.path()).unwrap(), 0);
        assert!(remove_project_page(root.path(), "pkg").is_err());
        fs::remove_file(root.path().join("simple/pkg")).unwrap();
        fs::remove_dir(root.path().join("packages/pkg")).unwrap();
        symlink(outside.path(), root.path().join("packages/pkg")).unwrap();
        assert!(write_project_page(root.path(), &data).is_err());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }
}
