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

/// Read leading project names from requirements-like or hash-pinned lockfiles.
/// Continuations are joined before comments are stripped. Only the leading name
/// is selected; extras, versions, markers, direct URLs, and hashes are ignored.
/// Dependencies are not resolved; downloads use the upstream index's own hashes.
pub fn names_from_reader(reader: impl BufRead) -> Result<Vec<String>> {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    let mut logical = String::new();
    let mut first_line = None;
    for (position, line) in reader.lines().enumerate() {
        let number = *first_line.get_or_insert(position + 1);
        let line = line.with_context(|| format!("Cannot read line {number}"))?;
        let comment_only = line.trim_start().starts_with('#');
        if line.ends_with('\\') && !comment_only {
            logical.push_str(&line[..line.len() - 1]);
            continue;
        }
        // Like pip, a comment-only physical line ends a pending logical line,
        // even when the comment itself ends in a backslash. Keep its comment
        // boundary separate from the preceding text before stripping comments.
        if comment_only {
            logical.push(' ');
        }
        logical.push_str(&line);
        select_logical_line(&logical, number, &mut selected, &mut seen)?;
        logical.clear();
        first_line = None;
    }
    if let Some(number) = first_line {
        select_logical_line(&logical, number, &mut selected, &mut seen)?;
    }
    Ok(selected)
}

fn without_comment(line: &str) -> &str {
    let mut previous_whitespace = true;
    for (position, character) in line.char_indices() {
        if character == '#' && previous_whitespace {
            return &line[..position];
        }
        previous_whitespace = character.is_whitespace();
    }
    line
}

fn hashes_only(line: &str) -> bool {
    let mut tokens = line.split_whitespace().peekable();
    let mut found_hash = false;
    while let Some(token) = tokens.next() {
        if token == "--hash" {
            found_hash = true;
            if tokens.peek().is_some_and(|value| !value.starts_with('-')) {
                tokens.next();
            }
        } else if token.starts_with("--hash=") {
            found_hash = true;
        } else {
            return false;
        }
    }
    found_hash
}

fn without_requirement_options(line: &str) -> &str {
    let mut previous_whitespace = false;
    for (position, character) in line.char_indices() {
        if previous_whitespace && line[position..].starts_with("--") {
            return &line[..position];
        }
        previous_whitespace = character.is_whitespace();
    }
    line
}

fn select_logical_line(
    line: &str,
    number: usize,
    selected: &mut Vec<String>,
    seen: &mut HashSet<String>,
) -> Result<()> {
    let line = without_comment(line).trim();
    if line.is_empty() || hashes_only(line) {
        return Ok(());
    }
    ensure!(
        !line.starts_with('-'),
        "Pip options are unsupported on line {number}: {line}"
    );
    let prefix = without_requirement_options(line)
        .split(|c: char| {
            c.is_whitespace() || matches!(c, '[' | '<' | '>' | '=' | '!' | '~' | ';' | '@' | ',')
        })
        .next()
        .unwrap_or_default();
    let name =
        normalize_name(prefix).with_context(|| format!("Invalid project name on line {number}"))?;
    if seen.insert(name.clone()) {
        selected.push(name);
    }
    Ok(())
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

    #[test]
    fn accepts_uv_compile_hash_pins_with_headers_annotations_and_markers() {
        let input = r#"# This file was autogenerated by uv via the following command:
#    uv pip compile --generate-hashes requirements.in
Foo_Bar==1.0 \
    --hash=sha256:aaa \
    --hash=sha256:bbb
    # via example
second==2.0 ; python_version >= "3.8" \
    --hash=sha256:ccc
"#;
        assert_eq!(
            names_from_reader(input.as_bytes()).unwrap(),
            ["foo-bar", "second"]
        );
    }

    #[test]
    fn accepts_pip_compile_hash_pins_and_via_blocks() {
        let input = r#"# This file is autogenerated by pip-compile
first==1.0 \
    --hash=sha256:aaa \
    --hash sha256:bbb
    # via
    #   requirements.in
    #   another-project
second==2.0 \
    --hash=sha256:ccc
"#;
        assert_eq!(
            names_from_reader(input.as_bytes()).unwrap(),
            ["first", "second"]
        );
    }

    #[test]
    fn accepts_poetry_export_markers_and_hashes_on_one_line() {
        let input = "first==1.0 ; python_version >= \"3.8\" --hash=sha256:aaa --hash sha256:bbb\nsecond[extra]==2.0 ; sys_platform == \"linux\" --hash=sha256:ccc\n";
        assert_eq!(
            names_from_reader(input.as_bytes()).unwrap(),
            ["first", "second"]
        );
        assert_eq!(
            without_requirement_options("first==1.0 --hash=sha256:aaa"),
            "first==1.0 "
        );
    }

    #[test]
    fn comment_only_physical_lines_end_continuations_like_pip() {
        // Tested against pip's req_file.preprocess: a comment flushes the
        // preceding logical line, even if the comment ends in a backslash.
        // The next project remains independent, with physical line number 3.
        for comment in ["# via example", "# via example \\"] {
            let input = format!("first\\\n{comment}\nsecond==2.0\n");
            assert_eq!(
                names_from_reader(input.as_bytes()).unwrap(),
                ["first", "second"]
            );
        }
        let input = "first==1.0 \\\n    # via example\n    --hash=sha256:aaa\nsecond==2.0\n";
        assert_eq!(
            names_from_reader(input.as_bytes()).unwrap(),
            ["first", "second"]
        );
    }

    #[test]
    fn joins_before_stripping_inline_comments_and_keeps_url_fragments() {
        let input = "first==1.0 # comment \\\nsecond==2.0\nthird==3.0\n";
        // pip joins the second physical line into the first line's comment.
        assert_eq!(
            names_from_reader(input.as_bytes()).unwrap(),
            ["first", "third"]
        );
        let url = "pkg @ https://x/y.whl#sha256=abc # trailing comment";
        assert_eq!(without_comment(url), "pkg @ https://x/y.whl#sha256=abc ");
        assert_eq!(names_from_reader(url.as_bytes()).unwrap(), ["pkg"]);
        assert!(names_from_reader(b"pkg#not-a-comment".as_slice()).is_err());
    }

    #[test]
    fn continuations_preserve_first_line_numbers_and_allow_a_dangling_backslash() {
        for (input, number) in [
            ("# header\n??? \\\n    --hash=sha256:aaa\n", 2),
            ("good\n-r \\\n    other.txt\n", 2),
            ("# header\n\\\n???\n", 2),
        ] {
            let error = format!("{:#}", names_from_reader(input.as_bytes()).unwrap_err());
            assert!(error.contains(&format!("line {number}")), "{error}");
        }
        for input in ["first==1.0 \\", "first==1.0 \\\n    --hash=sha256:aaa \\"] {
            assert_eq!(names_from_reader(input.as_bytes()).unwrap(), ["first"]);
        }
        assert_eq!(
            names_from_reader(b"requ\\\nests==1.0\n".as_slice()).unwrap(),
            ["requests"]
        );
    }

    #[test]
    fn ignores_only_hash_options_on_standalone_option_lines() {
        let input = "--hash=sha256:aaa --hash sha256:bbb\n--hash\nname==1.0 --hash=not-validated\n";
        assert_eq!(names_from_reader(input.as_bytes()).unwrap(), ["name"]);
        for input in [
            "-r other.txt",
            "-e editable",
            "--index-url x",
            "--hash=sha256:aaa --index-url x",
            "--hash -r",
            "--hashes=sha256:aaa",
            "--hash=sha256:aaa unrelated",
        ] {
            let error = format!("{:#}", names_from_reader(input.as_bytes()).unwrap_err());
            assert!(
                error.contains("Pip options are unsupported on line 1"),
                "{error}"
            );
        }
    }
}
