// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! File references in timeline markdown (port of the original desktop
//! main-process `file-reference` module): resolve a transcript link to a real
//! file inside the
//! session's own roots, then hand an editor scheme URL to the OS opener.
//!
//! Transcript text is untrusted input, so containment is enforced after
//! realpath — a symlink pointing outside the session root must not widen what
//! the app will open.

use std::path::{Path, PathBuf};

pub const DEFAULT_EDITOR_SCHEME: &str = "vscode";
const KNOWN_EDITOR_SCHEMES: &[&str] = &["vscode", "vscode-insiders", "cursor", "windsurf", "zed"];

/// A parsed file reference: path plus optional line/column anchor
/// (Vue `parseFileReference`: trailing `:line`, `:line:col` or `:line-end`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReference {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub end_line: Option<u32>,
}

/// Vue REFERENCE_SUFFIX: `/:(\d+)(?::(\d+)|-(\d+))?$/`.
pub fn parse_file_reference(raw: &str) -> FileReference {
    let text = raw.trim();
    match split_anchor(text) {
        Some((path, line, column, end_line)) => FileReference {
            path: path.to_string(),
            line,
            column,
            end_line,
        },
        None => FileReference {
            path: text.to_string(),
            line: None,
            column: None,
            end_line: None,
        },
    }
}

/// Split `path:line`, `path:line:col`, or `path:line-end` from the right.
/// Anything without a trailing `:digits` anchor stays whole.
type SplitAnchor<'a> = (&'a str, Option<u32>, Option<u32>, Option<u32>);

fn split_anchor(text: &str) -> Option<SplitAnchor<'_>> {
    let (head1, num1) = split_trailing_digits(text)?;
    if let Some(head) = head1.strip_suffix(':') {
        // `…:num1` — num1 is the line, unless one more `:digits` group makes
        // it `path:line(num2):col(num1)`.
        if let Some((head2, num2)) = split_trailing_digits(head) {
            if let Some(path) = head2.strip_suffix(':') {
                return Some((path, Some(num2), Some(num1), None));
            }
        }
        return Some((head, Some(num1), None, None));
    }
    // `…-num1` only exists as `path:line(num2)-num1`.
    let before_dash = head1.strip_suffix('-')?;
    let (head2, num2) = split_trailing_digits(before_dash)?;
    let path = head2.strip_suffix(':')?;
    Some((path, Some(num2), None, Some(num1)))
}

/// Split a run of trailing ASCII digits; `0` is rejected (not a line anchor).
fn split_trailing_digits(text: &str) -> Option<(&str, u32)> {
    let bytes = text.as_bytes();
    let mut start = text.len();
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start == text.len() {
        return None;
    }
    let number: u32 = text[start..].parse().ok()?;
    (number > 0).then(|| (&text[..start], number))
}

/// Vue hrefToPath: only `file:` URLs are percent-encoded; decoding a plain
/// path would corrupt filenames that legitimately contain `%`.
pub fn href_to_path(href: &str) -> String {
    if let Some(rest) = href.strip_prefix("file://") {
        return percent_decode(rest);
    }
    if let Some(rest) = href.strip_prefix("file:") {
        if !rest.starts_with("//") {
            return percent_decode(rest);
        }
    }
    href.to_string()
}

/// Minimal RFC 3986 percent-decoding; invalid escapes pass through untouched.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16)),
                bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16)),
            ) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve a reference against the session cwd, enforcing containment after
/// realpath (Vue `resolveFileReference` with the cwd as the only root). A
/// leading slash is project-relative (Vue: `path.join` treats it as a no-op),
/// so both the absolute and cwd-relative candidates are tried.
pub fn resolve_file_reference(raw_path: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let cwd = cwd?;
    let cwd = cwd.canonicalize().ok()?;
    let cleaned = raw_path.trim();
    if cleaned.is_empty() {
        return None;
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cleaned.starts_with('/') {
        candidates.push(PathBuf::from(cleaned));
    }
    candidates.push(cwd.join(cleaned.trim_start_matches('/')));
    candidates
        .into_iter()
        .filter(|candidate| is_within(candidate, &cwd))
        .find_map(|candidate| {
            let real = candidate.canonicalize().ok()?;
            (is_within(&real, &cwd) && real.is_file()).then_some(real)
        })
}

fn is_within(candidate: &Path, root: &Path) -> bool {
    candidate == root || candidate.starts_with(root)
}

/// Build the editor URL (Vue `buildEditorUrl`).
pub fn build_editor_url(
    scheme: &str,
    file_path: &Path,
    line: Option<u32>,
    column: Option<u32>,
) -> String {
    let resolved = KNOWN_EDITOR_SCHEMES
        .iter()
        .find(|known| **known == scheme)
        .copied()
        .unwrap_or(DEFAULT_EDITOR_SCHEME);
    let encoded: String = encode_uri_path(&file_path.to_string_lossy());
    let mut target = format!("{resolved}://file{encoded}");
    if let Some(line) = line.filter(|line| *line > 0) {
        target.push_str(&format!(":{line}"));
        if let Some(column) = column.filter(|column| *column > 0) {
            target.push_str(&format!(":{column}"));
        }
    }
    target
}

/// Encode a path for a URI the way `encodeURI` does: everything except the
/// reserved + unreserved sets; `/` survives.
fn encode_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'/' | b';'
                    | b','
                    | b'?'
                    | b':'
                    | b'@'
                    | b'&'
                    | b'='
                    | b'+'
                    | b'$'
                    | b'-'
                    | b'_'
                    | b'.'
                    | b'!'
                    | b'~'
                    | b'*'
                    | b'\''
                    | b'('
                    | b')'
                    | b'#'
            );
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Full open path: parse a markdown link, resolve against the session cwd,
/// read the editor scheme from settings, and hand the URL to the OS opener.
/// Returns the resolved path on success (Vue returns `{opened, path}`).
pub fn open_markdown_link(href: &str, cwd: Option<&Path>, home: &Path) -> Option<PathBuf> {
    let reference = parse_file_reference(&href_to_path(href));
    let resolved = resolve_file_reference(&reference.path, cwd)?;
    let scheme = editor_scheme(home);
    let url = build_editor_url(&scheme, &resolved, reference.line, reference.column);
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .ok()
        .map(|_| resolved)
}

/// `~/.obelisk/settings.json` → `editorScheme` (default vscode).
fn editor_scheme(home: &Path) -> String {
    let settings = obelisk_core::provider_settings::read_persisted_provider_settings(home);
    match settings {
        obelisk_core::provider_settings::SettingsRead::Ok(value) => value
            .get("editorScheme")
            .and_then(|scheme| scheme.as_str())
            .unwrap_or(DEFAULT_EDITOR_SCHEME)
            .to_string(),
        _ => DEFAULT_EDITOR_SCHEME.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_line_column_and_range_suffixes() {
        assert_eq!(
            parse_file_reference("src/foo.ts:40"),
            FileReference {
                path: "src/foo.ts".into(),
                line: Some(40),
                column: None,
                end_line: None
            }
        );
        assert_eq!(
            parse_file_reference("src/foo.ts:40:12"),
            FileReference {
                path: "src/foo.ts".into(),
                line: Some(40),
                column: Some(12),
                end_line: None
            }
        );
        assert_eq!(
            parse_file_reference("src/foo.ts:40-52"),
            FileReference {
                path: "src/foo.ts".into(),
                line: Some(40),
                column: None,
                end_line: Some(52)
            }
        );
        assert_eq!(
            parse_file_reference("/plain/path.md"),
            FileReference {
                path: "/plain/path.md".into(),
                line: None,
                column: None,
                end_line: None
            }
        );
    }

    #[test]
    fn href_to_path_decodes_only_file_urls() {
        assert_eq!(href_to_path("file:///a%20b/c.txt"), "/a b/c.txt");
        assert_eq!(href_to_path("/a%20b/c.txt"), "/a%20b/c.txt");
        assert_eq!(
            href_to_path("https://example.com/x"),
            "https://example.com/x"
        );
    }

    #[test]
    fn percent_decode_passes_invalid_escapes_through() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
    }

    #[test]
    fn resolution_is_contained_and_realpath_checked() {
        let dir = std::env::temp_dir().join("obelisk-fr-test");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let file = dir.join("src/foo.ts");
        std::fs::write(&file, "x").unwrap();

        assert_eq!(
            resolve_file_reference("src/foo.ts", Some(&dir)),
            Some(file.canonicalize().unwrap())
        );
        // Project-relative leading slash resolves the same way.
        assert!(resolve_file_reference("/src/foo.ts", Some(&dir)).is_some());
        // Escape attempts are rejected.
        assert_eq!(resolve_file_reference("../etc/passwd", Some(&dir)), None);
        assert_eq!(resolve_file_reference("/etc/passwd", Some(&dir)), None);
        // Missing file.
        assert_eq!(resolve_file_reference("src/missing.ts", Some(&dir)), None);
        // No cwd → no resolution.
        assert_eq!(resolve_file_reference("src/foo.ts", None), None);
    }

    #[test]
    fn editor_url_shape() {
        assert_eq!(
            build_editor_url("vscode", Path::new("/tmp/a b.md"), Some(12), Some(3)),
            "vscode://file/tmp/a%20b.md:12:3"
        );
        assert_eq!(
            build_editor_url("unknown-scheme", Path::new("/x.ts"), None, None),
            "vscode://file/x.ts"
        );
        assert_eq!(
            build_editor_url("zed", Path::new("/x.ts"), Some(7), None),
            "zed://file/x.ts:7"
        );
    }
}
