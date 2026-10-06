//! Search, grouped by file and capped twice.
//!
//! The shape that makes `grep` expensive in a context window is not the search, it is the
//! output: a path repeated on every line, and no ceiling on how many lines there are. Here
//! the path is written once per file, matches carry their line number, and two caps are
//! always in force — per file and per call — each reported rather than silent.

use std::path::PathBuf;

use regex::RegexSet;
use serde::{Deserialize, Serialize};

use crate::{
    Budget, Error, Result,
    read::{first_line, plural},
    slice,
    text::{self, Content},
    walk::{self, WalkOptions},
};

/// How much of each hit to render.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Matching lines, grouped under their path. The default.
    #[default]
    Lines,
    /// One line per file: the path and its match count.
    Counts,
    /// Just the paths that matched — the cheapest answer to "where is this?".
    Files,
}

/// Default ceiling on matches rendered per file.
pub const DEFAULT_MAX_PER_FILE: usize = 20;
/// Default ceiling on matches rendered per call.
pub const DEFAULT_MAX_MATCHES: usize = 200;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrepRequest {
    /// Regexes, searched as alternatives. Several patterns in one call beat several calls.
    pub patterns: Vec<String>,
    /// Files, directories or globs to search. Defaults to `.`.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Only search paths matching these globs (`*.rs`, `src/**/*.ts`).
    #[serde(default)]
    pub glob: Vec<String>,
    /// Skip paths matching these globs.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Treat the patterns as literal text rather than regexes.
    #[serde(default)]
    pub fixed: bool,
    #[serde(default)]
    pub ignore_case: bool,
    /// Lines of context either side of each match.
    #[serde(default)]
    pub context: usize,
    /// Matches to render per file before truncating. Defaults to [`DEFAULT_MAX_PER_FILE`].
    #[serde(default)]
    pub max_per_file: Option<usize>,
    /// Matches to render in total. Defaults to [`DEFAULT_MAX_MATCHES`].
    #[serde(default)]
    pub max_matches: Option<usize>,
    #[serde(default)]
    pub mode: Mode,
    /// Search dotfiles and dot-directories too.
    #[serde(default)]
    pub hidden: bool,
    /// Search paths `.gitignore` excludes. Off by default, and leaving it off is most of
    /// why this is cheaper than `grep -r`.
    #[serde(default)]
    pub no_ignore: bool,
    /// Maximum depth below each directory root.
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileMatches {
    pub path: String,
    /// Matches found, including any not rendered.
    pub matches: usize,
    /// Matches rendered.
    pub shown: usize,
    pub lines: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GrepOutcome {
    /// Not serialized: the adapter sends it as the response's text, and a structured
    /// copy of the same bytes would double the cost of every call.
    #[serde(skip_serializing)]
    pub text: String,
    pub files: Vec<FileMatches>,
    pub matches: usize,
    /// Files opened and searched, whether or not they matched.
    pub searched: usize,
    /// Files whose matches were cut by a cap or the budget.
    pub truncated: usize,
}

pub fn grep(request: &GrepRequest) -> Result<GrepOutcome> {
    if request.patterns.is_empty() || request.patterns.iter().all(|p| p.is_empty()) {
        return Err(Error::InvalidRequest(
            "pass at least one pattern: {\"patterns\": [\"...\"]}".into(),
        ));
    }
    let set = compile(request)?;
    let include = walk::globs(&request.glob)?;
    let exclude = walk::globs(&request.exclude)?;
    let cwd = request.cwd.as_deref();
    // `counts` and `files` render one line per file however many hits it holds, so the
    // match caps would cut the listing short and under-report the totals. Only the byte
    // budget bounds them.
    let (max_per_file, max_matches) = if matches!(request.mode, Mode::Lines) {
        (
            request.max_per_file.unwrap_or(DEFAULT_MAX_PER_FILE),
            request.max_matches.unwrap_or(DEFAULT_MAX_MATCHES),
        )
    } else {
        (usize::MAX, usize::MAX)
    };

    let mut budget = Budget::new(request.max_bytes);
    let mut out = String::new();
    let mut files: Vec<FileMatches> = Vec::new();
    let mut total = 0;
    let mut searched = 0;
    let mut truncated = 0;
    let mut stopped = false;

    let roots = if request.paths.is_empty() {
        vec![".".to_owned()]
    } else {
        request.paths.clone()
    };

    'roots: for root in &roots {
        for path in walk::expand(cwd, root)? {
            // A path named outright is searched whatever the globs say; the filters exist
            // to narrow a walk, not to overrule the caller.
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
                .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
                .map(|entry| (entry.into_path(), true))
                .collect()
            } else {
                vec![(path, false)]
            };

            for (entry, walked) in entries {
                let shown_path = walk::display(&entry, cwd);
                if walked {
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
                }

                let Content::Text(content) = text::read_text(&entry) else {
                    continue;
                };
                searched += 1;

                let lines = text::lines(&content);
                let hits: Vec<usize> = lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| set.is_match(line))
                    .map(|(index, _)| index + 1)
                    .collect();
                if hits.is_empty() {
                    continue;
                }
                total += hits.len();

                let room = max_matches.saturating_sub(rendered_matches(&files));
                let allowance = max_per_file.min(room);
                let shown: Vec<usize> = hits.iter().copied().take(allowance).collect();

                let block = render(request, &shown_path, &hits, &shown, &lines, lines.len());
                if !budget.try_spend(block.len()) {
                    stopped = true;
                }
                if stopped {
                    files.push(FileMatches {
                        path: shown_path,
                        matches: hits.len(),
                        shown: 0,
                        lines: Vec::new(),
                    });
                    truncated += 1;
                    break 'roots;
                }
                out.push_str(&block);
                if shown.len() < hits.len() {
                    truncated += 1;
                }
                files.push(FileMatches {
                    path: shown_path,
                    matches: hits.len(),
                    shown: shown.len(),
                    lines: shown,
                });
                if rendered_matches(&files) >= max_matches {
                    stopped = true;
                    break 'roots;
                }
            }
        }
    }

    let matched_files = files.iter().filter(|f| f.matches > 0).count();
    let mut footer = format!(
        "[{total} match{} in {matched_files} file{}, {searched} searched",
        if total == 1 { "" } else { "es" },
        plural(matched_files),
    );
    if truncated > 0 {
        footer.push_str(&format!(", {truncated} truncated"));
    }
    if stopped {
        footer.push_str(", stopped at a cap: raise max_matches/max_bytes or narrow the search");
    }
    footer.push_str("]\n");
    out.push_str(&footer);

    Ok(GrepOutcome {
        text: out,
        files,
        matches: total,
        searched,
        truncated,
    })
}

fn rendered_matches(files: &[FileMatches]) -> usize {
    files.iter().map(|f| f.shown).sum()
}

fn compile(request: &GrepRequest) -> Result<RegexSet> {
    let patterns: Vec<String> = request
        .patterns
        .iter()
        .map(|pattern| {
            let body = if request.fixed {
                regex::escape(pattern)
            } else {
                pattern.clone()
            };
            if request.ignore_case {
                format!("(?i){body}")
            } else {
                body
            }
        })
        .collect();
    RegexSet::new(&patterns).map_err(|err| Error::BadPattern {
        pattern: request.patterns.join("|"),
        detail: first_line(&err.to_string()),
    })
}

/// One file's block: the path once, then the matches.
fn render(
    request: &GrepRequest,
    path: &str,
    hits: &[usize],
    shown: &[usize],
    lines: &[&str],
    total_lines: usize,
) -> String {
    match request.mode {
        Mode::Files => format!("{path}\n"),
        Mode::Counts => format!("{path} {}\n", hits.len()),
        Mode::Lines => {
            let mut block = if shown.len() < hits.len() {
                format!("{path} ({} of {})\n", shown.len(), hits.len())
            } else {
                format!("{path} ({})\n", hits.len())
            };
            let spans = slice::with_context(shown, request.context, total_lines);
            for span in &spans {
                for line in span.start..=span.end {
                    // `:` marks a match, `-` a context line, the way grep writes them.
                    let marker = if shown.contains(&line) { ':' } else { '-' };
                    block.push_str(&format!(
                        "{line}{marker} {}\n",
                        text::trim_trailing(lines[line - 1], false)
                    ));
                }
            }
            block
        }
    }
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

        fn run(&self, mut request: GrepRequest) -> GrepOutcome {
            request.cwd = Some(self.path().to_path_buf());
            grep(&request).unwrap()
        }
    }

    fn patterns(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn a_path_is_written_once_per_file_not_once_per_line() {
        let fixture = Fixture::new();
        fixture
            .write("src/a.rs", "fn one() {}\nlet x = 1;\nfn two() {}\n")
            .write("src/b.rs", "let y = 2;\n");

        let outcome = fixture.run(GrepRequest {
            patterns: patterns(&["^fn "]),
            ..Default::default()
        });
        assert_eq!(
            outcome.text,
            "\
src/a.rs (2)
1: fn one() {}
3: fn two() {}
[2 matches in 1 file, 2 searched]
"
        );
        assert_eq!(outcome.searched, 2);
    }

    #[test]
    fn several_patterns_are_one_search() {
        let fixture = Fixture::new();
        fixture.write("a.txt", "alpha\nbeta\ngamma\n");
        let outcome = fixture.run(GrepRequest {
            patterns: patterns(&["alpha", "gamma"]),
            ..Default::default()
        });
        assert_eq!(outcome.matches, 2);
        assert!(
            outcome.text.contains("1: alpha\n3: gamma\n"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn context_lines_are_marked_apart_from_matches() {
        let fixture = Fixture::new();
        fixture.write("a.txt", "one\ntwo\nthree\nfour\n");
        let outcome = fixture.run(GrepRequest {
            patterns: patterns(&["three"]),
            context: 1,
            ..Default::default()
        });
        assert_eq!(
            outcome.text,
            "a.txt (1)\n2- two\n3: three\n4- four\n[1 match in 1 file, 1 searched]\n"
        );
    }

    #[test]
    fn counts_and_files_modes_cost_a_line_each() {
        let fixture = Fixture::new();
        fixture.write("a.txt", "hit\nhit\n").write("b.txt", "hit\n");

        let counts = fixture.run(GrepRequest {
            patterns: patterns(&["hit"]),
            mode: Mode::Counts,
            ..Default::default()
        });
        assert_eq!(
            counts.text,
            "a.txt 2\nb.txt 1\n[3 matches in 2 files, 2 searched]\n"
        );

        let files = fixture.run(GrepRequest {
            patterns: patterns(&["hit"]),
            mode: Mode::Files,
            ..Default::default()
        });
        assert_eq!(
            files.text,
            "a.txt\nb.txt\n[3 matches in 2 files, 2 searched]\n"
        );
    }

    #[test]
    fn a_file_full_of_matches_is_capped_and_says_so() {
        let fixture = Fixture::new();
        let body: String = (1..=100).map(|n| format!("hit {n}\n")).collect();
        fixture.write("many.txt", &body);

        let outcome = fixture.run(GrepRequest {
            patterns: patterns(&["hit"]),
            max_per_file: Some(3),
            ..Default::default()
        });
        assert!(
            outcome.text.starts_with("many.txt (3 of 100)\n"),
            "{}",
            outcome.text
        );
        assert_eq!(
            outcome.matches, 100,
            "the count is honest about what is there"
        );
        assert_eq!(outcome.files[0].shown, 3);
        assert!(outcome.text.contains("1 truncated"));
    }

    #[test]
    fn the_call_wide_cap_stops_the_search() {
        let fixture = Fixture::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fixture.write(name, "hit\nhit\n");
        }
        let outcome = fixture.run(GrepRequest {
            patterns: patterns(&["hit"]),
            max_matches: Some(3),
            ..Default::default()
        });
        assert_eq!(rendered_matches(&outcome.files), 3);
        assert!(
            outcome.text.contains("stopped at a cap"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn the_match_caps_do_not_shorten_a_counts_listing() {
        let fixture = Fixture::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fixture.write(name, "hit\nhit\n");
        }
        let outcome = fixture.run(GrepRequest {
            patterns: patterns(&["hit"]),
            mode: Mode::Counts,
            max_matches: Some(3),
            ..Default::default()
        });
        assert_eq!(outcome.files.len(), 3, "every file is still counted");
        assert_eq!(outcome.matches, 6);
        assert!(
            !outcome.text.contains("stopped at a cap"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn ignored_and_hidden_paths_stay_out_unless_asked_for() {
        let fixture = Fixture::new();
        fixture
            .write(".gitignore", "target\n")
            .write("target/huge.txt", "hit\n")
            .write(".secret/notes.txt", "hit\n")
            .write("src/a.rs", "hit\n");

        let default = fixture.run(GrepRequest {
            patterns: patterns(&["hit"]),
            ..Default::default()
        });
        assert_eq!(default.files.len(), 1, "only src/a.rs: {}", default.text);

        let everything = fixture.run(GrepRequest {
            patterns: patterns(&["hit"]),
            hidden: true,
            no_ignore: true,
            ..Default::default()
        });
        assert_eq!(everything.files.len(), 3, "{}", everything.text);
    }

    #[test]
    fn globs_narrow_the_walk_but_never_a_path_named_outright() {
        let fixture = Fixture::new();
        fixture.write("a.rs", "hit\n").write("a.md", "hit\n");

        let only_rust = fixture.run(GrepRequest {
            patterns: patterns(&["hit"]),
            glob: patterns(&["*.rs"]),
            ..Default::default()
        });
        assert_eq!(only_rust.files.len(), 1);

        let named = fixture.run(GrepRequest {
            patterns: patterns(&["hit"]),
            paths: patterns(&["a.md"]),
            glob: patterns(&["*.rs"]),
            ..Default::default()
        });
        assert_eq!(
            named.files.len(),
            1,
            "an explicit path wins: {}",
            named.text
        );
    }

    #[test]
    fn fixed_strings_and_case_folding_do_not_need_regex_syntax() {
        let fixture = Fixture::new();
        fixture.write("a.txt", "a.b\naxb\nHIT\n");

        let fixed = fixture.run(GrepRequest {
            patterns: patterns(&["a.b"]),
            fixed: true,
            ..Default::default()
        });
        assert_eq!(fixed.matches, 1, "`.` is a dot, not any character");

        let folded = fixture.run(GrepRequest {
            patterns: patterns(&["hit"]),
            ignore_case: true,
            ..Default::default()
        });
        assert_eq!(folded.matches, 1);
    }

    #[test]
    fn binary_files_are_searched_but_never_printed() {
        let fixture = Fixture::new();
        fixture
            .write("bin.dat", "hit\0hit\n")
            .write("a.txt", "hit\n");
        let outcome = fixture.run(GrepRequest {
            patterns: patterns(&["hit"]),
            ..Default::default()
        });
        assert_eq!(outcome.searched, 1);
        assert_eq!(outcome.files.len(), 1);
        assert_eq!(outcome.files[0].path, "a.txt");
    }

    #[test]
    fn a_search_with_no_pattern_or_a_broken_one_is_refused() {
        assert!(matches!(
            grep(&GrepRequest::default()),
            Err(Error::InvalidRequest(_))
        ));
        assert!(matches!(
            grep(&GrepRequest {
                patterns: patterns(&["("]),
                ..Default::default()
            }),
            Err(Error::BadPattern { .. })
        ));
    }

    #[test]
    fn a_search_that_matches_nothing_is_one_line() {
        let fixture = Fixture::new();
        fixture.write("a.txt", "nothing here\n");
        let outcome = fixture.run(GrepRequest {
            patterns: patterns(&["absent"]),
            ..Default::default()
        });
        assert_eq!(outcome.text, "[0 matches in 0 files, 1 searched]\n");
    }
}
