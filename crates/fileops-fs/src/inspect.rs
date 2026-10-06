//! Size, line count and kind for a batch of paths — what `ls -l`, `wc -l` and `file`
//! answer together.
//!
//! This is the cheap call that stops an expensive one: knowing a file is 40k lines of
//! generated JSON, or a symlink, or simply not there, is usually enough to decide not to
//! read it. One line per path, and nothing is opened unless a line count was asked for.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    Budget, Error, Result,
    find::{count_lines, date},
    read::plural,
    text::{self, Status, human},
    walk,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    /// Paths or globs to describe.
    pub paths: Vec<String>,
    /// Count lines, which means reading each file. On by default: the line count is what
    /// makes the size meaningful. Turn it off to stat very large files cheaply.
    #[serde(default = "yes")]
    pub lines: bool,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

fn yes() -> bool {
    true
}

impl Default for InspectRequest {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            lines: true,
            cwd: None,
            max_bytes: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Described {
    pub path: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    pub symlink: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InspectOutcome {
    pub text: String,
    pub paths: Vec<Described>,
    pub total_bytes: u64,
    pub missing: usize,
}

pub fn inspect(request: &InspectRequest) -> Result<InspectOutcome> {
    if request.paths.is_empty() {
        return Err(Error::InvalidRequest(
            "pass at least one path: {\"paths\": [\"...\"]}".into(),
        ));
    }
    let cwd = request.cwd.as_deref();
    let mut budget = Budget::new(request.max_bytes);
    let mut out = String::new();
    let mut described = Vec::new();
    let mut total_bytes = 0;
    let mut missing = 0;

    for pattern in &request.paths {
        let paths = walk::expand(cwd, pattern)?;
        if paths.is_empty() {
            let line = format!("{pattern} (no matches)\n");
            budget.spend(line.len());
            out.push_str(&line);
            continue;
        }
        for path in paths {
            let shown = walk::display(&path, cwd);
            let link = std::fs::symlink_metadata(&path)
                .map(|meta| meta.is_symlink())
                .unwrap_or(false);

            let (mut entry, line) = match std::fs::metadata(&path) {
                Err(err) => {
                    let status = Status::from_io(&err);
                    if matches!(status, Status::Missing) {
                        missing += 1;
                    }
                    (
                        Described {
                            path: shown.clone(),
                            status: status.kind(),
                            detail: status.detail(),
                            bytes: None,
                            lines: None,
                            entries: None,
                            modified: None,
                            symlink: link,
                        },
                        format!("{shown} {}\n", status.note()),
                    )
                }
                Ok(meta) if meta.is_dir() => {
                    let entries = std::fs::read_dir(&path).map(|dir| dir.count()).ok();
                    (
                        Described {
                            path: shown.clone(),
                            status: "directory",
                            detail: None,
                            bytes: None,
                            lines: None,
                            entries,
                            modified: meta.modified().ok().map(date),
                            symlink: link,
                        },
                        format!(
                            "{shown}/ {} entr{}\n",
                            entries.unwrap_or(0),
                            if entries == Some(1) { "y" } else { "ies" }
                        ),
                    )
                }
                Ok(meta) => {
                    total_bytes += meta.len();
                    let bytes = std::fs::read(&path).ok();
                    let binary = bytes.as_deref().map(text::is_binary).unwrap_or(false);
                    let lines = if request.lines && !binary {
                        count_lines(&path)
                    } else {
                        None
                    };
                    let modified = meta.modified().ok().map(date);
                    let mut line = format!("{shown} {}", human(meta.len()));
                    if binary {
                        line.push_str(" binary");
                    }
                    if let Some(lines) = lines {
                        line.push_str(&format!(" {lines}L"));
                    }
                    if link {
                        line.push_str(" symlink");
                    }
                    if let Some(modified) = &modified {
                        line.push(' ');
                        line.push_str(modified);
                    }
                    line.push('\n');
                    (
                        Described {
                            path: shown.clone(),
                            status: if binary { "binary" } else { "ok" },
                            detail: None,
                            bytes: Some(meta.len()),
                            lines,
                            entries: None,
                            modified,
                            symlink: link,
                        },
                        line,
                    )
                }
            };

            if !budget.try_spend(line.len()) {
                entry.status = "skipped";
                described.push(entry);
                out.push_str("(budget reached; remaining paths not described)\n");
                break;
            }
            out.push_str(&line);
            described.push(entry);
        }
    }

    let files = described.iter().filter(|d| d.bytes.is_some()).count();
    let mut footer = format!(
        "[{} path{}, {files} file{}, {}",
        described.len(),
        plural(described.len()),
        plural(files),
        human(total_bytes)
    );
    if missing > 0 {
        footer.push_str(&format!(", {missing} missing"));
    }
    footer.push_str("]\n");
    out.push_str(&footer);

    Ok(InspectOutcome {
        text: out,
        paths: described,
        total_bytes,
        missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    struct Fixture(tempfile::TempDir);

    impl Fixture {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }

        fn path(&self) -> &Path {
            self.0.path()
        }

        fn write(&self, name: &str, body: &str) -> &Self {
            let path = self.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
            self
        }

        fn run(&self, paths: &[&str]) -> InspectOutcome {
            inspect(&InspectRequest {
                paths: paths.iter().map(|p| (*p).to_owned()).collect(),
                cwd: Some(self.path().to_path_buf()),
                ..Default::default()
            })
            .unwrap()
        }
    }

    #[test]
    fn one_line_per_path_with_size_and_line_count() {
        let fixture = Fixture::new();
        fixture.write("spec.md", "a\nb\nc\n");
        let outcome = fixture.run(&["spec.md"]);
        let first = outcome.text.lines().next().unwrap();
        assert!(first.starts_with("spec.md 6B 3L 2"), "{first}");
        assert_eq!(outcome.paths[0].lines, Some(3));
        assert_eq!(outcome.total_bytes, 6);
    }

    #[test]
    fn a_binary_file_is_named_as_one_and_not_counted_in_lines() {
        let fixture = Fixture::new();
        fixture.write("a.bin", "ELF\0\0\0data");
        let outcome = fixture.run(&["a.bin"]);
        assert!(
            outcome.text.contains("a.bin 10B binary"),
            "{}",
            outcome.text
        );
        assert_eq!(outcome.paths[0].lines, None);
    }

    #[test]
    fn directories_report_their_entry_count() {
        let fixture = Fixture::new();
        fixture.write("src/a.rs", "a\n").write("src/b.rs", "b\n");
        let outcome = fixture.run(&["src"]);
        assert!(
            outcome.text.starts_with("src/ 2 entries\n"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn a_missing_path_is_reported_and_tallied() {
        let fixture = Fixture::new();
        fixture.write("there.txt", "x\n");
        let outcome = fixture.run(&["there.txt", "gone.txt"]);
        assert!(
            outcome.text.contains("gone.txt (missing)\n"),
            "{}",
            outcome.text
        );
        assert_eq!(outcome.missing, 1);
        assert!(outcome.text.contains("1 missing]"), "{}", outcome.text);
    }

    #[test]
    fn globs_describe_a_whole_set_in_one_call() {
        let fixture = Fixture::new();
        fixture
            .write("specs/a/spec.md", "a\n")
            .write("specs/b/spec.md", "b\nb\n");
        let outcome = fixture.run(&["specs/*/spec.md", "specs/*/none.md"]);
        assert_eq!(outcome.paths.len(), 2);
        assert!(
            outcome.text.contains("specs/*/none.md (no matches)\n"),
            "{}",
            outcome.text
        );
        assert!(
            outcome.text.contains("[2 paths, 2 files, 6B]"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn a_symlink_is_marked_so_nobody_follows_it_by_accident() {
        let fixture = Fixture::new();
        fixture.write("real.txt", "x\n");
        std::os::unix::fs::symlink(
            fixture.path().join("real.txt"),
            fixture.path().join("link.txt"),
        )
        .unwrap();
        let outcome = fixture.run(&["link.txt"]);
        assert!(
            outcome.text.contains("link.txt 2B 1L symlink"),
            "{}",
            outcome.text
        );
        assert!(outcome.paths[0].symlink);
    }

    #[test]
    fn line_counting_can_be_turned_off() {
        let fixture = Fixture::new();
        fixture.write("a.txt", "a\nb\n");
        let outcome = inspect(&InspectRequest {
            paths: vec!["a.txt".into()],
            lines: false,
            cwd: Some(fixture.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert!(!outcome.text.contains("2L"), "{}", outcome.text);
        assert_eq!(outcome.paths[0].bytes, Some(4));
    }

    #[test]
    fn an_empty_request_is_refused() {
        assert!(matches!(
            inspect(&InspectRequest::default()),
            Err(Error::InvalidRequest(_))
        ));
    }
}
