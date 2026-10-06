//! A repository at a glance: how much of what, and where the weight is.
//!
//! The first question in an unfamiliar tree is "what is this made of", and the shell
//! answer is a pipeline of `find`, `wc -l`, `sort` and `awk` that nobody wants to write
//! twice. `survey` walks once and reports two tables: a line per file type, and the
//! largest files. Both are bounded, so the answer is a dozen lines whatever the tree.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    Budget, Result,
    find::count_lines,
    read::plural,
    text::{compact_count, human},
    walk::{self, WalkOptions},
};

/// File types listed before the rest are summed into one `other` line.
pub const DEFAULT_KINDS: usize = 12;

/// Largest files listed.
pub const DEFAULT_TOP: usize = 10;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurveyRequest {
    /// Directories, files or globs to survey. Defaults to `.`.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Only count paths matching these globs.
    #[serde(default)]
    pub glob: Vec<String>,
    /// Skip paths matching these globs.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Directory levels to walk. `1` is the named directory itself.
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub no_ignore: bool,
    /// File types listed separately. Defaults to [`DEFAULT_KINDS`].
    #[serde(default)]
    pub kinds: Option<usize>,
    /// Largest files listed. Defaults to [`DEFAULT_TOP`]; `0` drops the table.
    #[serde(default)]
    pub top: Option<usize>,
    /// Count lines, which opens every file. Defaults to true; turn it off on a very large
    /// tree where sizes are enough.
    #[serde(default = "yes")]
    pub lines: bool,
    /// Directory relative paths resolve against. Defaults to the server's own.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Ceiling on rendered output. Defaults to [`crate::DEFAULT_MAX_BYTES`].
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

fn yes() -> bool {
    true
}

/// One file type's share of the tree.
#[derive(Debug, Clone, Serialize)]
pub struct Kind {
    /// The extension, or `(none)` for files without one.
    pub extension: String,
    pub files: usize,
    pub lines: usize,
    pub bytes: u64,
}

/// One of the largest files.
#[derive(Debug, Clone, Serialize)]
pub struct Largest {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<usize>,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SurveyOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Clients differ over which half of a response they
    /// show, and one that reads only `structuredContent` renders nothing without
    /// this field, so the duplication is the price of being legible everywhere.
    pub text: String,
    /// File types, heaviest first, after the `kinds` cap has folded the tail into `other`.
    pub kinds: Vec<Kind>,
    pub largest: Vec<Largest>,
    pub files: usize,
    pub dirs: usize,
    pub lines: usize,
    pub bytes: u64,
    /// Files counted but not line-counted, because they are binary or unreadable.
    pub unread: usize,
}

/// Walk the tree once and report what it is made of.
pub fn survey(request: &SurveyRequest) -> Result<SurveyOutcome> {
    let include = walk::globs(&request.glob)?;
    let exclude = walk::globs(&request.exclude)?;
    let cwd = request.cwd.as_deref();
    let kinds_shown = request.kinds.unwrap_or(DEFAULT_KINDS);
    let top = request.top.unwrap_or(DEFAULT_TOP);

    let mut totals: BTreeMap<String, Kind> = BTreeMap::new();
    let mut largest: Vec<Largest> = Vec::new();
    let mut files = 0;
    let mut dirs = 0;
    let mut lines = 0;
    let mut bytes = 0;
    let mut unread = 0;

    let roots = if request.paths.is_empty() {
        vec![".".to_owned()]
    } else {
        request.paths.clone()
    };

    for root in &roots {
        for path in walk::expand(cwd, root)? {
            let entries: Vec<(PathBuf, bool)> = if path.is_dir() {
                walk::walker(
                    std::slice::from_ref(&path),
                    &WalkOptions {
                        hidden: request.hidden,
                        no_ignore: request.no_ignore,
                        depth: request.depth,
                        follow: false,
                    },
                )
                .flatten()
                .map(|entry| {
                    let directory = entry.file_type().is_some_and(|t| t.is_dir());
                    (entry.into_path(), directory)
                })
                .collect()
            } else {
                vec![(path, false)]
            };

            for (entry, directory) in entries {
                if directory {
                    dirs += 1;
                    continue;
                }
                let shown_path = walk::display(&entry, cwd);
                if include
                    .as_ref()
                    .is_some_and(|set| !set.is_match(&shown_path))
                {
                    continue;
                }
                if exclude
                    .as_ref()
                    .is_some_and(|set| set.is_match(&shown_path))
                {
                    continue;
                }
                let Ok(meta) = std::fs::metadata(&entry) else {
                    continue;
                };

                let counted = request.lines.then(|| count_lines(&entry)).flatten();
                if counted.is_none() {
                    unread += 1;
                }
                files += 1;
                bytes += meta.len();
                lines += counted.unwrap_or(0);

                let extension = entry
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase())
                    .unwrap_or_else(|| "(none)".to_owned());
                let kind = totals.entry(extension.clone()).or_insert(Kind {
                    extension,
                    files: 0,
                    lines: 0,
                    bytes: 0,
                });
                kind.files += 1;
                kind.lines += counted.unwrap_or(0);
                kind.bytes += meta.len();

                largest.push(Largest {
                    path: shown_path,
                    lines: counted,
                    bytes: meta.len(),
                });
            }
        }
    }

    // Heaviest first, and ties broken by name so the same tree always renders the same.
    let mut kinds: Vec<Kind> = totals.into_values().collect();
    kinds.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.extension.cmp(&b.extension)));
    if kinds.len() > kinds_shown {
        let tail: Vec<Kind> = kinds.split_off(kinds_shown);
        kinds.push(Kind {
            extension: format!("other ({})", tail.len()),
            files: tail.iter().map(|k| k.files).sum(),
            lines: tail.iter().map(|k| k.lines).sum(),
            bytes: tail.iter().map(|k| k.bytes).sum(),
        });
    }
    largest.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));
    largest.truncate(top);

    let mut budget = Budget::new(request.max_bytes);
    let mut out = String::new();
    for kind in &kinds {
        let mut line = format!(
            "{} {} file{} {}",
            kind.extension,
            kind.files,
            plural(kind.files),
            human(kind.bytes)
        );
        if request.lines {
            line.push_str(&format!(" {}L", compact_count(kind.lines)));
        }
        line.push('\n');
        if !budget.try_spend(line.len()) {
            break;
        }
        out.push_str(&line);
    }
    if !largest.is_empty() && budget.try_spend(9) {
        out.push_str("largest:\n");
        for file in &largest {
            let mut line = format!("{} {}", file.path, human(file.bytes));
            if let Some(count) = file.lines {
                line.push_str(&format!(" {count}L"));
            }
            line.push('\n');
            if !budget.try_spend(line.len()) {
                break;
            }
            out.push_str(&line);
        }
    }

    let mut footer = format!(
        "[{files} file{} in {dirs} dir{}, {}",
        plural(files),
        plural(dirs),
        human(bytes)
    );
    if request.lines {
        footer.push_str(&format!(", {}L", compact_count(lines)));
        if unread > 0 {
            footer.push_str(&format!(", {unread} not counted"));
        }
    }
    footer.push_str("]\n");
    out.push_str(&footer);

    Ok(SurveyOutcome {
        text: out,
        kinds,
        largest,
        files,
        dirs,
        lines,
        bytes,
        unread,
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

        fn run(&self, mut request: SurveyRequest) -> SurveyOutcome {
            request.cwd = Some(self.path().to_path_buf());
            survey(&request).unwrap()
        }
    }

    fn sample() -> Fixture {
        let fixture = Fixture::new();
        fixture
            .write("src/lib.rs", &"line\n".repeat(40))
            .write("src/main.rs", &"line\n".repeat(10))
            .write("README.md", "# Title\n")
            .write("Makefile", "build:\n\tcargo build\n");
        fixture
    }

    fn request() -> SurveyRequest {
        SurveyRequest {
            lines: true,
            ..SurveyRequest::default()
        }
    }

    #[test]
    fn one_call_reports_what_a_tree_is_made_of() {
        let outcome = sample().run(request());
        assert_eq!(
            outcome.text,
            "rs 2 files 250B 50L\n\
             (none) 1 file 20B 2L\n\
             md 1 file 8B 1L\n\
             largest:\n\
             src/lib.rs 200B 40L\n\
             src/main.rs 50B 10L\n\
             Makefile 20B 2L\n\
             README.md 8B 1L\n\
             [4 files in 2 dirs, 278B, 53L]\n"
        );
    }

    #[test]
    fn the_tables_are_ordered_by_weight_and_the_totals_add_up() {
        let outcome = sample().run(request());
        let extensions: Vec<&str> = outcome.kinds.iter().map(|k| k.extension.as_str()).collect();
        assert_eq!(extensions, ["rs", "(none)", "md"]);
        assert_eq!(outcome.files, 4);
        assert_eq!(outcome.lines, 53);
        assert_eq!(outcome.bytes, 278);
        assert_eq!(outcome.largest.first().unwrap().path, "src/lib.rs");
    }

    #[test]
    fn the_long_tail_of_file_types_folds_into_one_line() {
        let fixture = Fixture::new();
        fixture
            .write("a.rs", "a\n")
            .write("b.md", "b\n")
            .write("c.toml", "c\n")
            .write("d.json", "d\n");
        let outcome = fixture.run(SurveyRequest {
            kinds: Some(2),
            top: Some(0),
            lines: true,
            ..SurveyRequest::default()
        });
        assert_eq!(
            outcome.text,
            "json 1 file 2B 1L\n\
             md 1 file 2B 1L\n\
             other (2) 2 files 4B 2L\n\
             [4 files in 1 dir, 8B, 4L]\n"
        );
        assert!(!outcome.text.contains("largest"));
    }

    #[test]
    fn skipping_the_line_count_leaves_only_sizes() {
        let outcome = sample().run(SurveyRequest {
            lines: false,
            top: Some(1),
            ..SurveyRequest::default()
        });
        assert_eq!(
            outcome.text,
            "rs 2 files 250B\n\
             (none) 1 file 20B\n\
             md 1 file 8B\n\
             largest:\n\
             src/lib.rs 200B\n\
             [4 files in 2 dirs, 278B]\n"
        );
        assert_eq!(outcome.lines, 0);
    }

    #[test]
    fn a_glob_narrows_the_survey_to_the_files_it_matches() {
        let outcome = sample().run(SurveyRequest {
            glob: vec!["**/*.rs".to_owned()],
            lines: true,
            ..SurveyRequest::default()
        });
        assert_eq!(outcome.files, 2);
        assert_eq!(outcome.kinds.len(), 1);
        assert!(outcome.text.starts_with("rs 2 files 250B 50L\n"));
    }

    #[test]
    fn a_file_that_cannot_be_counted_is_still_weighed() {
        let fixture = Fixture::new();
        std::fs::write(fixture.path().join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        fixture.write("a.rs", "a\n");
        let outcome = fixture.run(request());
        assert_eq!(outcome.unread, 1);
        assert!(
            outcome
                .text
                .ends_with("[2 files in 1 dir, 6B, 1L, 1 not counted]\n"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn the_budget_bounds_the_tables_and_keeps_the_footer() {
        let fixture = Fixture::new();
        for n in 0..40 {
            fixture.write(&format!("f{n}.e{n}"), "x\n");
        }
        let outcome = fixture.run(SurveyRequest {
            kinds: Some(40),
            max_bytes: Some(120),
            lines: true,
            ..SurveyRequest::default()
        });
        assert!(outcome.text.len() < 200, "{}", outcome.text);
        assert!(
            outcome.text.ends_with("[40 files in 1 dir, 80B, 40L]\n"),
            "{}",
            outcome.text
        );
    }
}
