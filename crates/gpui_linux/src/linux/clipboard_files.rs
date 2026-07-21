use std::path::{Path, PathBuf};

use gpui::{ClipboardEntry, ClipboardItem, ClipboardString, ExternalPaths};
use smallvec::SmallVec;
use url::Url;

pub(crate) const GNOME_COPIED_FILES_MIME_TYPE: &str = "x-special/gnome-copied-files";

/// Parse a `text/uri-list` payload into local file paths.
///
/// Blank lines and comment lines (starting with `#`) are skipped, as are
/// any URIs that do not resolve to a local `file://` path.
pub(crate) fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(uri_to_path)
        .collect()
}

/// Parse an `x-special/gnome-copied-files` payload.
///
/// The first line is the verb (`copy` or `cut`), the remaining lines are
/// `file://` URIs. Returns `None` when the verb is missing or unrecognized.
pub(crate) fn parse_gnome_copied_files(bytes: &[u8]) -> Option<(bool, Vec<PathBuf>)> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines();
    let is_cut = match lines.next()?.trim() {
        "cut" => true,
        "copy" => false,
        _ => return None,
    };
    let paths = lines
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(uri_to_path)
        .collect();
    Some((is_cut, paths))
}

/// Serialize file paths into a `text/uri-list` payload.
pub(crate) fn serialize_uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in paths {
        if let Some(uri) = path_to_uri(path) {
            out.push_str(&uri);
            out.push_str("\r\n");
        }
    }
    out
}

/// Serialize file paths into an `x-special/gnome-copied-files` payload.
pub(crate) fn serialize_gnome_copied_files(paths: &[PathBuf], is_cut: bool) -> String {
    let mut out = String::from(if is_cut { "cut" } else { "copy" });
    for path in paths {
        if let Some(uri) = path_to_uri(path) {
            out.push('\n');
            out.push_str(&uri);
        }
    }
    out
}

/// Build a clipboard item from parsed file paths.
///
/// A plain-text entry (newline-joined paths) is included alongside the
/// `ExternalPaths` entry so that text editors can paste the paths as text,
/// mirroring the macOS pasteboard behavior.
pub(crate) fn clipboard_item_from_paths(paths: Vec<PathBuf>) -> ClipboardItem {
    let text = paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    let paths: SmallVec<[PathBuf; 2]> = paths.into_iter().collect();
    ClipboardItem {
        entries: vec![
            ClipboardEntry::ExternalPaths(ExternalPaths(paths)),
            ClipboardEntry::String(ClipboardString::new(text)),
        ],
    }
}

/// Returns the file paths from the first `ExternalPaths` entry, if any.
pub(crate) fn external_paths(item: &ClipboardItem) -> Option<&[PathBuf]> {
    item.entries().iter().find_map(|entry| match entry {
        ClipboardEntry::ExternalPaths(paths) => Some(paths.paths()),
        _ => None,
    })
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    Url::parse(uri).ok()?.to_file_path().ok()
}

fn path_to_uri(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(|url| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gnome_copy_multiple_files() {
        let payload = b"copy\nfile:///home/user/a.txt\nfile:///home/user/b.txt";
        let (is_cut, paths) = parse_gnome_copied_files(payload).expect("should parse");
        assert!(!is_cut);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/user/a.txt"),
                PathBuf::from("/home/user/b.txt"),
            ]
        );
    }

    #[test]
    fn parse_gnome_cut_single_file() {
        let payload = b"cut\nfile:///home/user/a.txt";
        let (is_cut, paths) = parse_gnome_copied_files(payload).expect("should parse");
        assert!(is_cut);
        assert_eq!(paths, vec![PathBuf::from("/home/user/a.txt")]);
    }

    #[test]
    fn parse_gnome_percent_encoded_spaces() {
        let payload = b"copy\nfile:///home/user/my%20file%20name.txt";
        let (_, paths) = parse_gnome_copied_files(payload).expect("should parse");
        assert_eq!(paths, vec![PathBuf::from("/home/user/my file name.txt")]);
    }

    #[test]
    fn parse_gnome_missing_or_bad_verb_is_none() {
        assert_eq!(parse_gnome_copied_files(b""), None);
        assert_eq!(parse_gnome_copied_files(b"file:///home/user/a.txt"), None);
        assert_eq!(parse_gnome_copied_files(b"paste\nfile:///a.txt"), None);
    }

    #[test]
    fn parse_gnome_copy_without_files_is_empty() {
        let (is_cut, paths) = parse_gnome_copied_files(b"copy").expect("should parse");
        assert!(!is_cut);
        assert!(paths.is_empty());
    }

    #[test]
    fn parse_uri_list_skips_comments_blanks_and_non_files() {
        let payload =
            b"# a comment\n\nfile:///home/user/a.txt\nnot-a-uri\nhttp://example.com/x\nfile:///home/user/b.txt\n";
        let paths = parse_uri_list(payload);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/user/a.txt"),
                PathBuf::from("/home/user/b.txt"),
            ]
        );
    }

    #[test]
    fn parse_uri_list_empty() {
        assert!(parse_uri_list(b"").is_empty());
    }

    #[test]
    fn parse_malformed_bytes_does_not_panic() {
        let _ = parse_uri_list(&[0xff, 0xfe, b'\n', b'x', 0x00]);
        let _ = parse_gnome_copied_files(&[0xff, 0xfe, b'\n']);
    }

    #[test]
    fn round_trip_uri_list_with_spaces() {
        let paths = vec![
            PathBuf::from("/home/user/my file.txt"),
            PathBuf::from("/tmp/b.txt"),
        ];
        let serialized = serialize_uri_list(&paths);
        assert!(serialized.contains("my%20file.txt"));
        assert_eq!(parse_uri_list(serialized.as_bytes()), paths);
    }

    #[test]
    fn round_trip_gnome_copied_files() {
        let paths = vec![
            PathBuf::from("/home/user/a b.txt"),
            PathBuf::from("/home/user/c.txt"),
        ];
        let serialized = serialize_gnome_copied_files(&paths, true);
        assert!(serialized.starts_with("cut\n"));
        let (is_cut, parsed) = parse_gnome_copied_files(serialized.as_bytes()).expect("parse");
        assert!(is_cut);
        assert_eq!(parsed, paths);
    }

    #[test]
    fn clipboard_item_from_paths_includes_external_and_text() {
        let item =
            clipboard_item_from_paths(vec![PathBuf::from("/a.txt"), PathBuf::from("/b.txt")]);
        assert_eq!(item.entries.len(), 2);
        match &item.entries[0] {
            ClipboardEntry::ExternalPaths(paths) => assert_eq!(
                paths.paths(),
                &[PathBuf::from("/a.txt"), PathBuf::from("/b.txt")]
            ),
            other => panic!("expected ExternalPaths, got {other:?}"),
        }
        assert_eq!(item.text().as_deref(), Some("/a.txt\n/b.txt"));
        assert_eq!(external_paths(&item).map(|paths| paths.len()), Some(2));
    }
}
