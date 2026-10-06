//! Reading a file's bytes and deciding what they are.

use std::path::Path;

use serde::Serialize;

/// How many bytes to sniff when deciding whether a file is text.
const SNIFF: usize = 8192;

/// Why a path produced no content. Part of the output, never an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum Status {
    /// Content was read and rendered.
    Ok,
    /// Nothing is at this path.
    Missing,
    /// The path is a directory; `find` or `inspect` is the tool for it.
    Directory,
    /// The file holds NUL bytes, so printing it would be noise.
    Binary { bytes: u64 },
    /// The file exists but could not be opened.
    Denied(String),
    /// Anything else the filesystem reported.
    Error(String),
}

impl Status {
    /// The short parenthetical that stands in for content in rendered output.
    pub fn note(&self) -> String {
        match self {
            Status::Ok => String::new(),
            Status::Missing => "(missing)".into(),
            Status::Directory => "(directory)".into(),
            Status::Binary { bytes } => format!("(binary, {})", human(*bytes)),
            Status::Denied(detail) => format!("(unreadable: {detail})"),
            Status::Error(detail) => format!("({detail})"),
        }
    }

    /// The machine-readable name, for structured output.
    pub fn kind(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Missing => "missing",
            Status::Directory => "directory",
            Status::Binary { .. } => "binary",
            Status::Denied(_) => "denied",
            Status::Error(_) => "error",
        }
    }

    /// The reason, where there is one worth repeating in structured output.
    pub fn detail(&self) -> Option<String> {
        match self {
            Status::Ok | Status::Missing | Status::Directory => None,
            Status::Binary { bytes } => Some(format!("{bytes} bytes")),
            Status::Denied(detail) | Status::Error(detail) => Some(detail.clone()),
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Status::Ok)
    }

    /// Classify an I/O failure without losing the reason.
    pub fn from_io(err: &std::io::Error) -> Self {
        match err.kind() {
            std::io::ErrorKind::NotFound => Status::Missing,
            std::io::ErrorKind::PermissionDenied => Status::Denied("permission denied".into()),
            std::io::ErrorKind::IsADirectory => Status::Directory,
            _ => Status::Error(err.to_string()),
        }
    }
}

/// A file's text, or the reason there is none.
pub enum Content {
    Text(String),
    Unavailable(Status),
}

/// Read `path` as text, refusing directories and binary files up front.
///
/// Invalid UTF-8 is replaced rather than refused: a file with one stray byte is still
/// worth reading, and the alternative is an agent falling back to `cat`.
pub fn read_text(path: &Path) -> Content {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) => return Content::Unavailable(Status::from_io(&err)),
    };
    // A symlink is followed deliberately, the way `cat` would.
    let meta = if meta.is_symlink() {
        match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(err) => return Content::Unavailable(Status::from_io(&err)),
        }
    } else {
        meta
    };
    if meta.is_dir() {
        return Content::Unavailable(Status::Directory);
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => return Content::Unavailable(Status::from_io(&err)),
    };
    if is_binary(&bytes) {
        return Content::Unavailable(Status::Binary {
            bytes: bytes.len() as u64,
        });
    }
    Content::Text(String::from_utf8_lossy(&bytes).into_owned())
}

/// A NUL byte in the first [`SNIFF`] bytes means binary, which is how `grep` decides too.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(SNIFF).any(|b| *b == 0)
}

/// Split into lines without the terminators, dropping the empty trailing element a final
/// newline would otherwise produce. `wc -l` and this agree on the count for text files.
pub fn lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    for line in &mut lines {
        *line = line.strip_suffix('\r').unwrap_or(line);
    }
    lines
}

/// Sizes the way `ls -h` writes them: short, and never more than one decimal.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else if value < 10.0 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

/// Counts in the footers: `6.1k chars` reads cheaper than `6143 chars`.
pub fn compact_count(n: usize) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Trailing whitespace is invisible and still costs tokens, so it goes unless the caller
/// asked for the bytes exactly as stored.
pub fn trim_trailing(line: &str, raw: bool) -> &str {
    if raw { line } else { line.trim_end() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_counts_match_wc_l() {
        assert_eq!(
            lines("a\nb\nc\n").len(),
            3,
            "trailing newline is a terminator"
        );
        assert_eq!(
            lines("a\nb\nc").len(),
            3,
            "a missing final newline loses nothing"
        );
        assert_eq!(lines("").len(), 0);
        assert_eq!(lines("\n").len(), 1, "one empty line");
        assert_eq!(lines("a\r\nb\r\n"), vec!["a", "b"], "CRLF is not content");
    }

    #[test]
    fn binary_is_detected_by_nul_like_grep() {
        assert!(is_binary(b"ELF\0\0\0"));
        assert!(!is_binary("héllo — ok".as_bytes()));
        // Beyond the sniff window a NUL is not looked for, matching grep's own cutoff.
        let mut late = vec![b'a'; SNIFF + 10];
        late[SNIFF + 5] = 0;
        assert!(!is_binary(&late));
    }

    #[test]
    fn sizes_and_counts_stay_short() {
        assert_eq!(human(0), "0B");
        assert_eq!(human(999), "999B");
        assert_eq!(human(4300), "4.2K");
        assert_eq!(human(24 * 1024), "24K");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(6143), "6.1k");
    }

    #[test]
    fn status_notes_say_why_there_is_no_content() {
        assert_eq!(Status::Missing.note(), "(missing)");
        assert_eq!(Status::Binary { bytes: 24_678 }.note(), "(binary, 24K)");
        assert!(
            Status::Ok.note().is_empty(),
            "no note where content follows"
        );
    }
}
