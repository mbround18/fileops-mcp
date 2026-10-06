//! Batched line slices: one call in place of a chain of `cat`, `head`, `tail` and
//! `sed -n '12,40p'` joined by `echo` separators.
//!
//! The rendered form is the point. Each file gets a one-line header naming the path and
//! exactly which lines follow (`#3 src/lib.rs 12-40,98-120/412`), so the reader can tell
//! what they are looking at and what they are not, and a footer tallies the batch. There
//! are no banner lines, no blank separators, and no repetition of the path per line.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    Budget, Error, Result,
    slice::{self, Span},
    text::{self, Content, Status},
    walk,
};

/// One file to read, and how much of it.
///
/// The window is chosen by the first of these that is given: `lines`, then `from`/`to`,
/// then `head`/`tail`, then the whole file. `grep` filters whatever the window selected,
/// so `{head: 200, grep: "TODO"}` means "TODOs in the first 200 lines" rather than a
/// whole-file search. Patterns are Rust regexes; `(?i)` at the front makes one
/// case-insensitive.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadSpec {
    /// File to read. A glob (`specs/*/spec.md`) expands to every match; `~` is your home.
    pub path: String,
    /// `sed`-style selection: `12`, `12-40`, `40-`, or `12-40,98-120`.
    #[serde(default)]
    pub lines: Option<String>,
    /// First N lines. Combines with `tail` to show both ends of an unfamiliar file.
    #[serde(default)]
    pub head: Option<usize>,
    /// Last N lines.
    #[serde(default)]
    pub tail: Option<usize>,
    /// Start the selection at the first line matching this regex — `sed '/pattern/,$p'`.
    /// Ignored when `lines` is given.
    #[serde(default)]
    pub from: Option<String>,
    /// End the selection at the next line matching this regex, inclusive. With `from`,
    /// the pair repeats like `sed -n '/a/,/b/p'`, so a heading pattern yields a section
    /// per match. Alone, it reads from the top of the file down to the first match.
    #[serde(default)]
    pub to: Option<String>,
    /// Keep only lines matching this regex, within whatever the window selected.
    #[serde(default)]
    pub grep: Option<String>,
    /// Lines of context either side of a `grep` match. Clipped to the window.
    #[serde(default)]
    pub context: Option<usize>,
    /// Stop after this many lines, whatever the selection asked for.
    #[serde(default)]
    pub max_lines: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    /// The batch. Nothing stops this from being one spec, but one is rarely the answer.
    pub specs: Vec<ReadSpec>,
    /// Directory relative paths resolve against. Defaults to the server's own.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Ceiling on rendered output. Defaults to [`crate::DEFAULT_MAX_BYTES`].
    #[serde(default)]
    pub max_bytes: Option<usize>,
    /// Prefix each line with its number. On by default: a slice without line numbers
    /// cannot be edited against, and the header alone does not locate a filtered line.
    #[serde(default = "yes")]
    pub number: bool,
    /// Reproduce bytes exactly, keeping trailing whitespace.
    #[serde(default)]
    pub raw: bool,
}

fn yes() -> bool {
    true
}

impl Default for ReadRequest {
    fn default() -> Self {
        Self {
            specs: Vec::new(),
            cwd: None,
            max_bytes: None,
            number: true,
            raw: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FileRead {
    /// The `#N` in the header, so a structured reader and a text reader agree.
    pub index: usize,
    pub path: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Lines in the whole file, where it was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<usize>,
    /// The spans actually rendered, after every cap.
    pub shown: Vec<Span>,
    /// Selected lines that were cut by `max_lines` or the byte budget.
    pub dropped: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Clients differ over which half of a response they
    /// show, and one that reads only `structuredContent` renders nothing without
    /// this field, so the duplication is the price of being legible everywhere.
    pub text: String,
    pub files: Vec<FileRead>,
    pub lines_shown: usize,
    /// Files whose output was cut short.
    pub truncated: usize,
    /// Files the budget ran out before reaching.
    pub skipped: usize,
}

/// Read every spec in the batch, in order, against one shared byte budget.
pub fn read(request: &ReadRequest) -> Result<ReadOutcome> {
    if request.specs.is_empty() {
        return Err(Error::InvalidRequest(
            "pass at least one spec: {\"specs\": [{\"path\": \"...\"}]}".into(),
        ));
    }
    let cwd = request.cwd.as_deref();
    let mut budget = Budget::new(request.max_bytes);
    let mut out = String::new();
    let mut files: Vec<FileRead> = Vec::new();
    let mut lines_shown = 0;
    let mut truncated = 0;
    let mut skipped = 0;

    for spec in &request.specs {
        let matcher = compile(spec.grep.as_deref())?;

        let from = compile(spec.from.as_deref())?;
        let to = compile(spec.to.as_deref())?;

        let paths = walk::expand(cwd, &spec.path)?;
        if paths.is_empty() {
            let index = files.len() + 1;
            let line = format!("#{index} {} (no matches)\n", spec.path);
            budget.spend(line.len());
            out.push_str(&line);
            files.push(FileRead {
                index,
                path: spec.path.clone(),
                status: "no_matches",
                detail: None,
                total_lines: None,
                shown: Vec::new(),
                dropped: 0,
            });
            continue;
        }

        for path in paths {
            let index = files.len() + 1;
            let shown_path = walk::display(&path, cwd);

            // Out of budget: say so per file rather than trailing off mid-batch.
            if budget.exhausted() {
                skipped += 1;
                let line = format!("#{index} {shown_path} (skipped: byte budget reached)\n");
                out.push_str(&line);
                files.push(FileRead {
                    index,
                    path: shown_path,
                    status: "skipped",
                    detail: None,
                    total_lines: None,
                    shown: Vec::new(),
                    dropped: 0,
                });
                continue;
            }

            let content = match text::read_text(&path) {
                Content::Text(text) => text,
                Content::Unavailable(status) => {
                    let line = format!("#{index} {shown_path} {}\n", status.note());
                    budget.spend(line.len());
                    out.push_str(&line);
                    files.push(FileRead {
                        index,
                        path: shown_path,
                        status: status.kind(),
                        detail: status.detail(),
                        total_lines: None,
                        shown: Vec::new(),
                        dropped: 0,
                    });
                    continue;
                }
            };

            let all = text::lines(&content);
            let total = all.len();
            let window = if spec.lines.is_none() && (from.is_some() || to.is_some()) {
                slice::ranges(&all, from.as_ref(), to.as_ref())
            } else {
                slice::window(spec.lines.as_deref(), spec.head, spec.tail, total)?
            };
            let selected = match &matcher {
                None => window.clone(),
                Some(re) => {
                    let hits: Vec<usize> = window
                        .iter()
                        .flat_map(|span| span.start..=span.end)
                        .filter(|line| re.is_match(all[line - 1]))
                        .collect();
                    let grown = slice::with_context(&hits, spec.context.unwrap_or(0), total);
                    slice::intersect(&grown, &window)
                }
            };
            let (selected, cut) = slice::cap(&selected, spec.max_lines);

            let mut body = String::new();
            let mut rendered = 0;
            'spans: for (position, span) in selected.iter().enumerate() {
                // Without line numbers a jump between spans is invisible, so mark it.
                if position > 0 && !request.number && budget.try_spend(4) {
                    body.push_str("...\n");
                }
                for line in span.start..=span.end {
                    let content = text::trim_trailing(all[line - 1], request.raw);
                    let rendered_line = if request.number {
                        format!("{line}: {content}\n")
                    } else {
                        format!("{content}\n")
                    };
                    if !budget.try_spend(rendered_line.len()) {
                        break 'spans;
                    }
                    body.push_str(&rendered_line);
                    rendered += 1;
                }
            }

            // The header describes what was rendered, not what was asked for.
            let (shown, lost_to_budget) = slice::cap(&selected, Some(rendered));
            let dropped = cut + lost_to_budget;
            let header = header(
                index,
                &shown_path,
                &shown,
                total,
                dropped,
                matcher.is_some(),
            );
            budget.spend(header.len());
            out.push_str(&header);
            out.push_str(&body);

            lines_shown += rendered;
            if dropped > 0 {
                truncated += 1;
            }
            files.push(FileRead {
                index,
                path: shown_path,
                status: Status::Ok.kind(),
                detail: None,
                total_lines: Some(total),
                shown,
                dropped,
            });
        }
    }

    let footer = footer(files.len(), lines_shown, out.len(), truncated, skipped);
    out.push_str(&footer);

    Ok(ReadOutcome {
        text: out,
        files,
        lines_shown,
        truncated,
        skipped,
    })
}

fn header(
    index: usize,
    path: &str,
    shown: &[Span],
    total: usize,
    dropped: usize,
    filtered: bool,
) -> String {
    let mut header = format!("#{index} {path} ");
    if total == 0 {
        header.push_str("(empty)\n");
        return header;
    }
    if shown.is_empty() {
        header.push_str(if filtered {
            "(no matching lines)\n"
        } else {
            "(nothing selected)\n"
        });
        return header;
    }
    header.push_str(&format!("{}/{total}", slice::describe(shown)));
    if dropped > 0 {
        header.push_str(&format!(" +{dropped} lines cut"));
    }
    header.push('\n');
    header
}

fn footer(files: usize, lines: usize, bytes: usize, truncated: usize, skipped: usize) -> String {
    let mut footer = format!(
        "[{files} file{}, {lines} line{}, {} chars",
        plural(files),
        plural(lines),
        text::compact_count(bytes)
    );
    if truncated > 0 {
        footer.push_str(&format!(", {truncated} truncated"));
    }
    if skipped > 0 {
        footer.push_str(&format!(
            ", {skipped} skipped: raise max_bytes or narrow the specs"
        ));
    }
    footer.push_str("]\n");
    footer
}

/// A bad regex is the request's problem, not the file's, so it fails the whole call.
fn compile(pattern: Option<&str>) -> Result<Option<regex::Regex>> {
    pattern
        .map(|pattern| {
            regex::Regex::new(pattern).map_err(|err| Error::BadPattern {
                pattern: pattern.to_owned(),
                detail: first_line(&err.to_string()),
            })
        })
        .transpose()
}

pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

pub(crate) fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim().to_owned()
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
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, body).unwrap();
            self
        }

        fn numbered(&self, name: &str, lines: usize) -> &Self {
            let body: String = (1..=lines).map(|n| format!("line {n}\n")).collect();
            self.write(name, &body)
        }

        fn read(&self, specs: Vec<ReadSpec>) -> ReadOutcome {
            self.request(ReadRequest {
                specs,
                cwd: Some(self.path().to_path_buf()),
                ..Default::default()
            })
        }

        fn request(&self, mut request: ReadRequest) -> ReadOutcome {
            request.cwd = request.cwd.or_else(|| Some(self.path().to_path_buf()));
            read(&request).unwrap()
        }
    }

    /// Rendered output minus its footer, plus the footer, so a test can be exact about
    /// the body without restating the byte total the footer reports.
    fn split(text: &str) -> (String, String) {
        let (body, footer) = text
            .trim_end()
            .rsplit_once('\n')
            .unwrap_or(("", text.trim_end()));
        (format!("{body}\n"), footer.to_owned())
    }

    fn spec(path: &str) -> ReadSpec {
        ReadSpec {
            path: path.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn a_section_can_be_named_by_its_heading_instead_of_its_line_numbers() {
        let fixture = Fixture::new();
        fixture.write(
            "doc.md",
            "# Title\nintro\n## Invariants\nfirst\nsecond\n## Testing\nmake check\n",
        );

        let outcome = fixture.read(vec![ReadSpec {
            path: "doc.md".into(),
            from: Some("^## Invariants".into()),
            to: Some("^## ".into()),
            ..Default::default()
        }]);

        let (body, _) = split(&outcome.text);
        assert_eq!(
            body,
            "#1 doc.md 3-6/7\n3: ## Invariants\n4: first\n5: second\n6: ## Testing\n"
        );
    }

    #[test]
    fn an_explicit_line_selection_wins_over_a_pattern_range() {
        let fixture = Fixture::new();
        fixture.numbered("a.txt", 10);

        let outcome = fixture.read(vec![ReadSpec {
            path: "a.txt".into(),
            lines: Some("2-3".into()),
            from: Some("^line 7$".into()),
            ..Default::default()
        }]);

        assert!(
            outcome.text.contains("#1 a.txt 2-3/10\n"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn a_range_that_never_opens_reports_the_file_as_read_and_empty() {
        let fixture = Fixture::new();
        fixture.numbered("a.txt", 5);

        let outcome = fixture.read(vec![ReadSpec {
            path: "a.txt".into(),
            from: Some("^nothing here$".into()),
            ..Default::default()
        }]);

        assert_eq!(outcome.lines_shown, 0);
        assert_eq!(outcome.files[0].total_lines, Some(5));
    }

    #[test]
    fn one_call_reads_a_whole_batch_in_order() {
        let fixture = Fixture::new();
        fixture
            .numbered("a.txt", 3)
            .numbered("b.txt", 200)
            .write("c.json", "{}\n");

        let outcome = fixture.read(vec![
            spec("a.txt"),
            ReadSpec {
                head: Some(2),
                ..spec("b.txt")
            },
            spec("c.json"),
        ]);

        let (body, footer) = split(&outcome.text);
        assert_eq!(
            body,
            "\
#1 a.txt 1-3/3
1: line 1
2: line 2
3: line 3
#2 b.txt 1-2/200
1: line 1
2: line 2
#3 c.json 1/1
1: {}
"
        );
        assert_eq!(footer, "[3 files, 6 lines, 102 chars]");
        assert_eq!(outcome.files.len(), 3);
        assert_eq!(outcome.truncated, 0);
    }

    #[test]
    fn the_footer_counts_what_was_actually_emitted() {
        let fixture = Fixture::new();
        fixture.numbered("a.txt", 2);
        let outcome = fixture.read(vec![spec("a.txt")]);
        let (body, footer) = split(&outcome.text);
        assert_eq!(footer, format!("[1 file, 2 lines, {} chars]", body.len()));
    }

    #[test]
    fn a_header_says_exactly_which_lines_follow() {
        let fixture = Fixture::new();
        fixture.numbered("big.txt", 412);
        let outcome = fixture.read(vec![ReadSpec {
            lines: Some("12-14,98-99".into()),
            ..spec("big.txt")
        }]);
        assert!(
            outcome.text.starts_with("#1 big.txt 12-14,98-99/412\n"),
            "{}",
            outcome.text
        );
        assert!(outcome.text.contains("98: line 98\n"));
        assert_eq!(outcome.lines_shown, 5);
    }

    #[test]
    fn head_and_tail_together_describe_a_file_in_two_slices() {
        let fixture = Fixture::new();
        fixture.numbered("big.txt", 100);
        let outcome = fixture.read(vec![ReadSpec {
            head: Some(2),
            tail: Some(1),
            ..spec("big.txt")
        }]);
        assert!(
            outcome.text.contains("#1 big.txt 1-2,100/100\n"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn grep_within_a_window_cannot_report_a_line_outside_it() {
        let fixture = Fixture::new();
        fixture.write(
            "tasks.md",
            "- [ ] one\n- [x] two\nfiller\n- [ ] three\n- [ ] four\n",
        );
        let outcome = fixture.read(vec![ReadSpec {
            grep: Some(r"^- \[ \]".into()),
            head: Some(3),
            context: Some(5),
            ..spec("tasks.md")
        }]);
        assert_eq!(
            split(&outcome.text).0,
            "#1 tasks.md 1-3/5\n1: - [ ] one\n2: - [x] two\n3: filler\n",
            "context is clipped to head: 3"
        );
    }

    #[test]
    fn a_filter_that_matches_nothing_says_so_instead_of_printing_the_file() {
        let fixture = Fixture::new();
        fixture.numbered("a.txt", 50);
        let outcome = fixture.read(vec![ReadSpec {
            grep: Some("nowhere".into()),
            ..spec("a.txt")
        }]);
        assert_eq!(
            outcome.text,
            "#1 a.txt (no matching lines)\n[1 file, 0 lines, 29 chars]\n"
        );
    }

    #[test]
    fn numbers_can_be_dropped_and_gaps_are_marked_instead() {
        let fixture = Fixture::new();
        fixture.numbered("a.txt", 100);
        let outcome = fixture.request(ReadRequest {
            specs: vec![ReadSpec {
                lines: Some("1,50".into()),
                ..spec("a.txt")
            }],
            number: false,
            ..Default::default()
        });
        assert_eq!(
            outcome.text,
            "#1 a.txt 1,50/100\nline 1\n...\nline 50\n[1 file, 2 lines, 37 chars]\n"
        );
    }

    #[test]
    fn a_path_that_cannot_be_read_costs_one_line_and_not_the_batch() {
        let fixture = Fixture::new();
        fixture
            .numbered("ok.txt", 1)
            .write("bin.dat", "ELF\0\0payload");
        std::fs::create_dir_all(fixture.path().join("adir")).unwrap();

        let outcome = fixture.read(vec![
            spec("nope.txt"),
            spec("bin.dat"),
            spec("adir"),
            spec("ok.txt"),
        ]);
        assert_eq!(
            split(&outcome.text).0,
            "\
#1 nope.txt (missing)
#2 bin.dat (binary, 12B)
#3 adir (directory)
#4 ok.txt 1/1
1: line 1
"
        );
        let statuses: Vec<&str> = outcome.files.iter().map(|f| f.status).collect();
        assert_eq!(statuses, vec!["missing", "binary", "directory", "ok"]);
    }

    #[test]
    fn a_glob_expands_into_the_batch() {
        let fixture = Fixture::new();
        fixture
            .write("specs/010-x/spec.md", "ten\n")
            .write("specs/011-y/spec.md", "eleven\n")
            .write("specs/011-y/notes.txt", "skip\n");

        let outcome = fixture.read(vec![spec("specs/*/spec.md"), spec("specs/*/missing.md")]);
        assert_eq!(
            split(&outcome.text).0,
            "\
#1 specs/010-x/spec.md 1/1
1: ten
#2 specs/011-y/spec.md 1/1
1: eleven
#3 specs/*/missing.md (no matches)
"
        );
    }

    #[test]
    fn max_lines_truncates_and_the_header_admits_it() {
        let fixture = Fixture::new();
        fixture.numbered("big.txt", 500);
        let outcome = fixture.read(vec![ReadSpec {
            max_lines: Some(3),
            ..spec("big.txt")
        }]);
        assert!(
            outcome
                .text
                .starts_with("#1 big.txt 1-3/500 +497 lines cut\n"),
            "{}",
            outcome.text
        );
        assert_eq!(outcome.truncated, 1);
        assert_eq!(outcome.files[0].dropped, 497);
    }

    #[test]
    fn the_budget_stops_output_and_names_what_it_skipped() {
        let fixture = Fixture::new();
        fixture.numbered("a.txt", 100).numbered("b.txt", 100);
        let outcome = fixture.request(ReadRequest {
            specs: vec![spec("a.txt"), spec("b.txt")],
            max_bytes: Some(120),
            ..Default::default()
        });

        assert!(
            outcome.text.len() < 400,
            "budget kept it small: {}",
            outcome.text.len()
        );
        assert!(outcome.files[0].dropped > 0);
        assert_eq!(outcome.skipped, 1, "the second file never started");
        assert!(
            outcome
                .text
                .contains("#2 b.txt (skipped: byte budget reached)")
        );
        assert!(
            outcome.text.contains("1 truncated, 1 skipped"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn an_empty_file_and_an_empty_batch_are_different_answers() {
        let fixture = Fixture::new();
        fixture.write("empty.txt", "");
        let outcome = fixture.read(vec![spec("empty.txt")]);
        assert_eq!(
            outcome.text,
            "#1 empty.txt (empty)\n[1 file, 0 lines, 21 chars]\n"
        );
        assert_eq!(outcome.files[0].total_lines, Some(0));

        let err = read(&ReadRequest::default()).unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }

    #[test]
    fn a_bad_pattern_or_range_fails_the_call_rather_than_a_file() {
        let fixture = Fixture::new();
        fixture.numbered("a.txt", 5);
        let bad_pattern = read(&ReadRequest {
            specs: vec![ReadSpec {
                grep: Some("(".into()),
                ..spec("a.txt")
            }],
            cwd: Some(fixture.path().to_path_buf()),
            ..Default::default()
        });
        assert!(matches!(bad_pattern, Err(Error::BadPattern { .. })));

        let bad_range = read(&ReadRequest {
            specs: vec![ReadSpec {
                lines: Some("40-12".into()),
                ..spec("a.txt")
            }],
            cwd: Some(fixture.path().to_path_buf()),
            ..Default::default()
        });
        assert!(matches!(bad_range, Err(Error::BadRange { .. })));
    }

    #[test]
    fn trailing_whitespace_goes_unless_raw_was_asked_for() {
        let fixture = Fixture::new();
        fixture.write("ws.txt", "code   \n");
        assert!(
            fixture
                .read(vec![spec("ws.txt")])
                .text
                .contains("1: code\n")
        );

        let raw = fixture.request(ReadRequest {
            specs: vec![spec("ws.txt")],
            raw: true,
            ..Default::default()
        });
        assert!(raw.text.contains("1: code   \n"));
    }
}
