//! The shape of a file without its contents: headings, definitions, targets, top-level
//! keys.
//!
//! The question "what is in this file" is usually answered by reading the first hundred
//! lines and hoping, or by a `grep -nE '^(pub )?(fn|struct|impl)'` written from memory per
//! language. Both cost more than the answer is worth. `outline` keeps the one line that
//! declares each thing, with its line number, so the next call can be a `read` of exactly
//! the right span.
//!
//! The patterns are deliberately shallow — a regex per language family, not a parser. They
//! are tuned to miss rather than to guess: a line that is obviously a declaration is kept,
//! anything clever is left for `grep`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    Budget, Error, Result,
    read::{first_line, plural},
    text::{self, Content, compact_count},
    walk::{self, WalkOptions},
};

/// Symbols rendered per file before the rest are counted but not shown.
pub const DEFAULT_MAX_PER_FILE: usize = 30;

/// Symbols rendered across the whole call.
pub const DEFAULT_LIMIT: usize = 180;

/// Longest declaration line rendered before it is elided — a signature spanning 200
/// columns says nothing the first 110 do not.
const WIDTH: usize = 110;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutlineRequest {
    /// Files, directories or globs to outline. Defaults to `.`.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Only outline paths matching these globs.
    #[serde(default)]
    pub glob: Vec<String>,
    /// Skip paths matching these globs.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Use this regex instead of the built-in patterns, for a language that has none or a
    /// convention of your own.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Markdown heading depth: `2` keeps `#` and `##` and drops the rest.
    #[serde(default)]
    pub levels: Option<usize>,
    /// Symbols rendered per file. Defaults to [`DEFAULT_MAX_PER_FILE`].
    #[serde(default)]
    pub max_per_file: Option<usize>,
    /// Symbols rendered in total. Defaults to [`DEFAULT_LIMIT`].
    #[serde(default)]
    pub limit: Option<usize>,
    /// Directory levels to walk. `1` is the named directory itself.
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub no_ignore: bool,
    /// Directory relative paths resolve against. Defaults to the server's own.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Ceiling on rendered output. Defaults to [`crate::DEFAULT_MAX_BYTES`].
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileOutline {
    pub path: String,
    /// What the file is outlined as: `rust`, `markdown`, `custom`, …
    pub language: &'static str,
    /// Declarations found.
    pub symbols: usize,
    /// Declarations rendered, after the caps.
    pub shown: usize,
    /// Their line numbers, in order.
    pub lines: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutlineOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Clients differ over which half of a response they
    /// show, and one that reads only `structuredContent` renders nothing without
    /// this field, so the duplication is the price of being legible everywhere.
    pub text: String,
    pub files: Vec<FileOutline>,
    pub symbols: usize,
    /// Files opened, whether or not they declared anything.
    pub read: usize,
    /// Files whose outline was cut by a cap or the budget.
    pub truncated: usize,
    /// Files whose type has no pattern, and which a `grep` would have to answer for.
    pub unsupported: usize,
}

/// Outline every path in the batch against one shared byte budget.
pub fn outline(request: &OutlineRequest) -> Result<OutlineOutcome> {
    let custom = match &request.pattern {
        Some(pattern) => Some(regex::Regex::new(pattern).map_err(|err| Error::BadPattern {
            pattern: pattern.clone(),
            detail: first_line(&err.to_string()),
        })?),
        None => None,
    };
    let include = walk::globs(&request.glob)?;
    let exclude = walk::globs(&request.exclude)?;
    let cwd = request.cwd.as_deref();
    let max_per_file = request.max_per_file.unwrap_or(DEFAULT_MAX_PER_FILE);
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);

    let mut budget = Budget::new(request.max_bytes);
    let mut out = String::new();
    let mut files: Vec<FileOutline> = Vec::new();
    let mut symbols = 0;
    let mut read = 0;
    let mut truncated = 0;
    let mut unsupported = 0;
    let mut stopped = false;

    let roots = if request.paths.is_empty() {
        vec![".".to_owned()]
    } else {
        request.paths.clone()
    };

    'roots: for root in &roots {
        for path in walk::expand(cwd, root)? {
            // A path named outright is outlined whatever the globs say; the filters exist
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

                let language = match (&custom, Language::of(&entry)) {
                    (Some(_), _) => Language::custom(),
                    (None, Some(language)) => language,
                    // A type with no pattern is reported once, and only when it was named
                    // outright: saying it of every file in a walk would cost more than the
                    // outline itself.
                    (None, None) => {
                        if !walked {
                            let line = format!("{shown_path} (no outline for this type)\n");
                            budget.spend(line.len());
                            out.push_str(&line);
                            files.push(FileOutline {
                                path: shown_path,
                                language: "unsupported",
                                symbols: 0,
                                shown: 0,
                                lines: Vec::new(),
                            });
                        }
                        unsupported += 1;
                        continue;
                    }
                };

                let Content::Text(content) = text::read_text(&entry) else {
                    continue;
                };
                read += 1;

                let matcher = custom.as_ref().unwrap_or(&language.pattern);
                let lines = text::lines(&content);
                let hits: Vec<usize> = lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| matcher.is_match(line))
                    .filter(|(_, line)| language.within_levels(line, request.levels))
                    .map(|(index, _)| index + 1)
                    .collect();
                if hits.is_empty() {
                    continue;
                }
                symbols += hits.len();

                let room = limit.saturating_sub(files.iter().map(|f| f.shown).sum::<usize>());
                let shown: Vec<usize> = hits.iter().copied().take(max_per_file.min(room)).collect();
                let block = render(&shown_path, &hits, &shown, &lines);

                if !budget.try_spend(block.len()) {
                    stopped = true;
                    truncated += 1;
                    files.push(FileOutline {
                        path: shown_path,
                        language: language.name,
                        symbols: hits.len(),
                        shown: 0,
                        lines: Vec::new(),
                    });
                    break 'roots;
                }
                out.push_str(&block);
                if shown.len() < hits.len() {
                    truncated += 1;
                }
                files.push(FileOutline {
                    path: shown_path,
                    language: language.name,
                    symbols: hits.len(),
                    shown: shown.len(),
                    lines: shown,
                });
                if files.iter().map(|f| f.shown).sum::<usize>() >= limit {
                    stopped = true;
                    break 'roots;
                }
            }
        }
    }

    let outlined = files.iter().filter(|f| f.shown > 0).count();
    let mut footer = format!(
        "[{} symbol{} in {outlined} file{}, {read} read",
        compact_count(symbols),
        plural(symbols),
        plural(outlined),
    );
    if unsupported > 0 {
        footer.push_str(&format!(", {unsupported} without a pattern"));
    }
    if truncated > 0 {
        footer.push_str(&format!(", {truncated} truncated"));
    }
    if stopped {
        footer.push_str(", stopped at a cap: raise limit/max_bytes or narrow the paths");
    }
    footer.push_str("]\n");
    out.push_str(&footer);

    Ok(OutlineOutcome {
        text: out,
        files,
        symbols,
        read,
        truncated,
        unsupported,
    })
}

/// One file's block: the path and its language once, then a line per declaration.
fn render(path: &str, hits: &[usize], shown: &[usize], lines: &[&str]) -> String {
    let mut block = if shown.len() < hits.len() {
        format!("{path} ({} of {})\n", shown.len(), hits.len())
    } else {
        format!("{path} ({})\n", hits.len())
    };
    for line in shown {
        block.push_str(&format!("{line}: {}\n", declaration(lines[line - 1])));
    }
    block
}

/// A declaration reduced to what identifies it: no trailing brace, empty body, comma or
/// semicolon, and no more than [`WIDTH`] columns.
fn declaration(line: &str) -> String {
    let trimmed = line
        .trim_end()
        .trim_end_matches(['{', '}', '(', ',', ':', ';'])
        .trim_end();
    let kept = if trimmed.is_empty() {
        line.trim_end()
    } else {
        trimmed
    };
    if kept.chars().count() <= WIDTH {
        return kept.to_owned();
    }
    let mut short: String = kept.chars().take(WIDTH).collect();
    short.push('…');
    short
}

/// A language family and the one pattern that finds its declarations.
struct Language {
    name: &'static str,
    pattern: regex::Regex,
    markdown: bool,
}

impl Language {
    fn custom() -> Self {
        // The pattern is the caller's; this only names what they get back.
        Self {
            name: "custom",
            pattern: regex::Regex::new("^").expect("trivial pattern"),
            markdown: false,
        }
    }

    /// The family a path belongs to, by extension or by well-known file name.
    fn of(path: &std::path::Path) -> Option<Self> {
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        let (language, pattern) = match extension.as_str() {
            "rs" => (
                "rust",
                r##"^\s*(pub(\([^)]*\))?\s+)?(default\s+)?(unsafe\s+)?(extern\s+("[^"]*"\s+)?)?(async\s+)?(const\s+)?(fn|struct|enum|union|trait|impl|mod|type|macro_rules!)\b"##,
            ),
            "py" | "pyi" => ("python", r"^\s*(async\s+def|def|class)\s"),
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" => (
                "javascript",
                r"^\s*(export\s+)?(default\s+)?(declare\s+)?(abstract\s+)?(async\s+)?(function\*?\s|class\s|interface\s|type\s+[A-Za-z_$][\w$]*\s*=|enum\s|(const|let|var)\s+[A-Za-z_$][\w$]*\s*=\s*(async\s*)?(\(|function))",
            ),
            "go" => ("go", r"^(func|type|var|const)\s"),
            "rb" => ("ruby", r"^\s*(class|module|def)\s"),
            "java" | "kt" | "kts" | "scala" | "cs" | "swift" | "dart" => (
                "jvm-like",
                r"^\s*((public|private|protected|internal|open|final|static|abstract|override|suspend|sealed|data|partial)\s+)*(class|interface|enum|record|struct|object|trait|fun|func|def)\s",
            ),
            "c" | "h" | "cc" | "cpp" | "hpp" | "cxx" | "hh" => (
                "c-like",
                r"^\s*(struct|class|enum|union|typedef|namespace|template|#define)\s|^[A-Za-z_][\w \*&:<>,]*\s[\*&]?[A-Za-z_]\w*\s*\([^;]*$",
            ),
            "sh" | "bash" | "zsh" => (
                "shell",
                r"^\s*(function\s+[A-Za-z_][\w-]*|[A-Za-z_][\w-]*\s*\(\s*\))",
            ),
            "md" | "mdx" | "markdown" => ("markdown", r"^#{1,6}\s"),
            "toml" => ("toml", r"^\s*\["),
            "yaml" | "yml" => ("yaml", r#"^[A-Za-z_'"][^:]*:"#),
            "json" => ("json", r#"^\s{0,2}"[^"]+"\s*:"#),
            "sql" => ("sql", r"(?i)^\s*(create|alter|drop)\s"),
            "mk" => ("make", r"^[A-Za-z0-9_./%-]+\s*:($|[^=])"),
            _ if name == "makefile" || name == "gnumakefile" => {
                ("make", r"^[A-Za-z0-9_./%-]+\s*:($|[^=])")
            }
            _ => return None,
        };

        Some(Self {
            name: language,
            pattern: regex::Regex::new(pattern).expect("built-in patterns compile"),
            markdown: language == "markdown",
        })
    }

    /// `levels` only means anything where nesting is written into the line itself.
    fn within_levels(&self, line: &str, levels: Option<usize>) -> bool {
        match (self.markdown, levels) {
            (true, Some(levels)) => line.chars().take_while(|c| *c == '#').count() <= levels,
            _ => true,
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
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, body).unwrap();
            self
        }

        fn run(&self, mut request: OutlineRequest) -> OutlineOutcome {
            request.cwd = request.cwd.or_else(|| Some(self.path().to_path_buf()));
            outline(&request).unwrap()
        }
    }

    fn paths(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn one_call_outlines_a_batch_of_languages() {
        let fixture = Fixture::new();
        fixture
            .write(
                "src/lib.rs",
                "use std::fmt;\n\npub struct Spec {\n    pub path: String,\n}\n\nimpl Spec {\n    pub fn new() -> Self {\n        Self::default()\n    }\n}\n",
            )
            .write(
                "docs/guide.md",
                "# Guide\n\nprose\n\n## Install\n\nmore prose\n\n### Notes\n",
            )
            .write("app.py", "import os\n\n\nclass Thing:\n    def run(self):\n        pass\n");

        let outcome = fixture.run(OutlineRequest {
            paths: paths(&["."]),
            ..Default::default()
        });

        assert_eq!(
            outcome.text,
            "app.py (2)\n\
             4: class Thing\n\
             5:     def run(self)\n\
             docs/guide.md (3)\n\
             1: # Guide\n\
             5: ## Install\n\
             9: ### Notes\n\
             src/lib.rs (3)\n\
             3: pub struct Spec\n\
             7: impl Spec\n\
             8:     pub fn new() -> Self\n\
             [8 symbols in 3 files, 3 read]\n"
        );
        assert_eq!(outcome.symbols, 8);
        assert_eq!(outcome.files[2].language, "rust");
    }

    #[test]
    fn a_declaration_keeps_its_identity_and_loses_its_punctuation() {
        assert_eq!(declaration("pub struct Spec {"), "pub struct Spec");
        assert_eq!(declaration("    def run(self):"), "    def run(self)");
        assert_eq!(declaration("fn f("), "fn f");
        assert_eq!(declaration("pub fn run() {}"), "pub fn run()");
        assert_eq!(declaration("type Id = u64;"), "type Id = u64");
        assert_eq!(
            declaration("{"),
            "{",
            "a line that is only punctuation is kept"
        );
        let long = format!("pub fn f({}) {{", "a: usize, ".repeat(30));
        assert!(declaration(&long).ends_with('…'));
        assert_eq!(declaration(&long).chars().count(), WIDTH + 1);
    }

    #[test]
    fn markdown_depth_can_be_limited() {
        let fixture = Fixture::new();
        fixture.write("doc.md", "# One\n## Two\n### Three\n#### Four\n");

        let outcome = fixture.run(OutlineRequest {
            paths: paths(&["doc.md"]),
            levels: Some(2),
            ..Default::default()
        });

        assert_eq!(outcome.symbols, 2);
        assert!(outcome.text.contains("2: ## Two\n"), "{}", outcome.text);
        assert!(!outcome.text.contains("### Three"), "{}", outcome.text);
    }

    #[test]
    fn a_type_without_a_pattern_says_so_only_when_it_was_named() {
        let fixture = Fixture::new();
        fixture
            .write("notes.rtf", "{\\rtf1}\n")
            .write("a.rs", "pub fn a() {}\n");

        let named = fixture.run(OutlineRequest {
            paths: paths(&["notes.rtf"]),
            ..Default::default()
        });
        assert_eq!(
            named.text,
            "notes.rtf (no outline for this type)\n[0 symbols in 0 files, 0 read, 1 without a pattern]\n"
        );

        let walked = fixture.run(OutlineRequest {
            paths: paths(&["."]),
            ..Default::default()
        });
        assert!(!walked.text.contains("notes.rtf"), "{}", walked.text);
        assert_eq!(walked.unsupported, 1, "it is still counted in the footer");
    }

    #[test]
    fn a_custom_pattern_replaces_the_built_in_ones() {
        let fixture = Fixture::new();
        fixture.write("notes.rtf", "TASK one\nprose\nTASK two\n");

        let outcome = fixture.run(OutlineRequest {
            paths: paths(&["notes.rtf"]),
            pattern: Some("^TASK ".into()),
            ..Default::default()
        });

        assert_eq!(outcome.symbols, 2);
        assert_eq!(outcome.files[0].language, "custom");
        assert_eq!(outcome.unsupported, 0);
    }

    #[test]
    fn a_bad_custom_pattern_is_a_request_error() {
        let fixture = Fixture::new();
        fixture.write("a.rs", "pub fn a() {}\n");
        let err = outline(&OutlineRequest {
            paths: paths(&["a.rs"]),
            pattern: Some("(".into()),
            cwd: Some(fixture.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, Error::BadPattern { .. }));
    }

    #[test]
    fn the_per_file_cap_reports_what_it_left_out() {
        let fixture = Fixture::new();
        let body: String = (1..=10).map(|n| format!("pub fn f{n}() {{}}\n")).collect();
        fixture.write("a.rs", &body);

        let outcome = fixture.run(OutlineRequest {
            paths: paths(&["a.rs"]),
            max_per_file: Some(3),
            ..Default::default()
        });

        assert!(
            outcome.text.contains("a.rs (3 of 10)\n"),
            "{}",
            outcome.text
        );
        assert_eq!(
            outcome.symbols, 10,
            "the count is honest about what is there"
        );
        assert_eq!(outcome.files[0].shown, 3);
        assert!(outcome.text.contains("1 truncated"));
    }

    #[test]
    fn the_call_wide_limit_stops_the_outline() {
        let fixture = Fixture::new();
        for name in ["a.rs", "b.rs", "c.rs"] {
            fixture.write(name, "pub fn one() {}\npub fn two() {}\n");
        }

        let outcome = fixture.run(OutlineRequest {
            paths: paths(&["."]),
            limit: Some(3),
            ..Default::default()
        });

        assert_eq!(outcome.files.iter().map(|f| f.shown).sum::<usize>(), 3);
        assert!(
            outcome.text.contains("stopped at a cap"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn the_byte_budget_cuts_whole_files_rather_than_half_a_block() {
        let fixture = Fixture::new();
        fixture
            .write("a.rs", "pub fn one() {}\n")
            .write("b.rs", "pub fn two() {}\n");

        let outcome = fixture.run(OutlineRequest {
            paths: paths(&["."]),
            max_bytes: Some(30),
            ..Default::default()
        });

        assert!(outcome.text.contains("a.rs (1)\n"), "{}", outcome.text);
        assert!(!outcome.text.contains("b.rs (1)\n"), "{}", outcome.text);
        assert_eq!(outcome.truncated, 1);
        assert!(outcome.text.contains("stopped at a cap"));
    }

    #[test]
    fn ignored_and_hidden_paths_stay_out_unless_asked_for() {
        let fixture = Fixture::new();
        fixture
            .write(".gitignore", "target\n")
            .write("target/build.rs", "pub fn generated() {}\n")
            .write("src/lib.rs", "pub fn kept() {}\n");

        let quiet = fixture.run(OutlineRequest {
            paths: paths(&["."]),
            ..Default::default()
        });
        assert!(!quiet.text.contains("target/"), "{}", quiet.text);

        let everything = fixture.run(OutlineRequest {
            paths: paths(&["."]),
            no_ignore: true,
            hidden: true,
            ..Default::default()
        });
        assert!(
            everything.text.contains("target/build.rs"),
            "{}",
            everything.text
        );
    }

    #[test]
    fn globs_narrow_a_walk_but_never_a_path_that_was_named() {
        let fixture = Fixture::new();
        fixture
            .write("src/lib.rs", "pub fn kept() {}\n")
            .write("docs/guide.md", "# Heading\n");

        let walked = fixture.run(OutlineRequest {
            paths: paths(&["."]),
            glob: paths(&["**/*.rs"]),
            ..Default::default()
        });
        assert!(!walked.text.contains("guide.md"), "{}", walked.text);

        let named = fixture.run(OutlineRequest {
            paths: paths(&["docs/guide.md"]),
            glob: paths(&["**/*.rs"]),
            ..Default::default()
        });
        assert!(named.text.contains("1: # Heading\n"), "{}", named.text);
    }

    #[test]
    fn makefiles_and_configs_outline_by_their_own_shape() {
        let fixture = Fixture::new();
        fixture
            .write(
                "Makefile",
                "APP := x\n\nbuild:\n\tcargo build\n\ntest:\n\tcargo test\n",
            )
            .write(
                "Cargo.toml",
                "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n",
            );

        let outcome = fixture.run(OutlineRequest {
            paths: paths(&["Makefile", "Cargo.toml"]),
            ..Default::default()
        });

        assert_eq!(
            outcome.text,
            "Makefile (2)\n\
             3: build\n\
             6: test\n\
             Cargo.toml (2)\n\
             1: [package]\n\
             4: [dependencies]\n\
             [4 symbols in 2 files, 2 read]\n"
        );
    }
}
