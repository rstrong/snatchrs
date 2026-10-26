//! Explicit project-name selection without resolving versions or dependencies.
use std::{collections::HashSet, io::BufRead};

use anyhow::{Context, Result, ensure};

use crate::normalize_name;

/// Normalize and deduplicate project names, preserving first-seen order.
pub fn normalize_names(names: impl IntoIterator<Item = String>) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut selected = Vec::new();
    for name in names {
        let normalized = normalize_name(&name)?;
        if seen.insert(normalized.clone()) {
            selected.push(normalized);
        }
    }
    Ok(selected)
}

/// Read leading project names from requirements-like lines. Extras, versions,
/// markers, and direct URLs select the named project, never dependencies.
pub fn names_from_reader(reader: impl BufRead) -> Result<Vec<String>> {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for (position, line) in reader.lines().enumerate() {
        let number = position + 1;
        let line = line.with_context(|| format!("Cannot read line {number}"))?;
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        ensure!(
            !line.starts_with('-'),
            "Pip options are unsupported on line {number}: {line}"
        );
        let prefix = line
            .split(|c: char| {
                c.is_whitespace()
                    || matches!(c, '[' | '<' | '>' | '=' | '!' | '~' | ';' | '@' | ',')
            })
            .next()
            .unwrap_or_default();
        let name = normalize_name(prefix)
            .with_context(|| format!("Invalid project name on line {number}"))?;
        if seen.insert(name.clone()) {
            selected.push(name);
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_requirements_names_and_first_seen_normalization() {
        let input = b"# comment\n\nrequests>=2.0 # trailing comment\nFoo[bar]==1; python_version<\"3.9\"\nname @ https://example.org/wheel.whl\nSome_Pkg!=1\nsome.pkg~=2\nFOO\nREQUESTS < 3\nlast, ignored\n";
        assert_eq!(
            names_from_reader(&input[..]).unwrap(),
            ["requests", "foo", "name", "some-pkg", "last"]
        );
        assert_eq!(
            normalize_names([
                "Foo.Bar".into(),
                "foo_bar".into(),
                "Another".into(),
                "FOO-BAR".into()
            ])
            .unwrap(),
            ["foo-bar", "another"]
        );
    }

    #[test]
    fn invalid_lines_report_their_line_numbers() {
        for (input, number) in [
            ("# heading\n-r other.txt\n", 2),
            ("good\n\n???\n", 3),
            ("  -e editable\n", 1),
            ("good\n[extra]\n", 2),
            ("good\nname/bad\n", 2),
            ("good\nname$bad\n", 2),
        ] {
            let error = format!("{:#}", names_from_reader(input.as_bytes()).unwrap_err());
            assert!(error.contains(&format!("line {number}")), "{error}");
        }
        assert!(
            names_from_reader(b"# only a comment\n\n".as_slice())
                .unwrap()
                .is_empty()
        );
    }
}
