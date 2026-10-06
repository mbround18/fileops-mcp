//! MCP adapter: exposes `fileops-fs` over the Model Context Protocol.
//!
//! Thin by design. Everything here either describes a tool's parameters for the caller or
//! translates them into a `fileops-fs` request; the rendering they are judged on lives in
//! the core crate, next to its tests.

use std::path::PathBuf;

use fileops_fs::{FindRequest, GrepRequest, InspectRequest, ReadRequest, ReadSpec};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_router,
};
use serde::Deserialize;

const INSTRUCTIONS: &str = "\
Reading the filesystem without spending a context window on it. Prefer these tools over \
shell `cat`, `head`, `tail`, `sed -n`, `grep`, `ls`, `find`, `tree` and `wc -l`.

- `read` — line slices of many files in one call. Each spec takes `lines` (`12-40,98-120`), \
`head`/`tail`, `from`/`to` regexes for a named section, a `grep` filter with `context`, and \
`max_lines`; `path` may be a glob.
- `grep` — search several patterns at once, grouped by file, with `mode: counts` or \
`mode: files` when the matches themselves are not the question.
- `find` — listings grouped by directory. `depth: 1` is `ls`; `glob`, `kind` and `stat` \
narrow or widen it.
- `inspect` — size, line count and kind for a batch of paths. The cheap call that tells you \
which expensive read is worth making.

Two habits make the difference:

1. **Batch.** One `read` with eight specs costs one round trip and one header per file. \
Eight calls, or one shell command joining eight `cat`s with `echo` separators, cost far \
more for the same bytes.
2. **Ask for less.** Name the lines you need, cap what you do not. Every response is \
truncated at a byte budget and says what it left out, so a reply that ends in `truncated` \
is an instruction to narrow the request, not to retry it unchanged.

Paths may be absolute, relative to `cwd`, `~`-prefixed, or globs. `.gitignore` and hidden \
files are skipped while searching and listing (`no_ignore`/`hidden` override that), and a \
path named outright is always read.";

/// Line numbers, byte budgets and the working directory are shared by every tool.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ReadParams {
    /// The batch of files to read. One call with many specs is the point of this tool.
    pub specs: Vec<SpecParams>,
    /// Directory relative paths resolve against. Defaults to the server's own.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Ceiling on rendered output in bytes (default 40000, max 400000). Output is
    /// truncated with a marker rather than silently cut.
    #[serde(default)]
    pub max_bytes: Option<usize>,
    /// Prefix each line with its number. Defaults to true; turn it off only when the
    /// content is being read as prose rather than located for an edit.
    #[serde(default)]
    pub number: Option<bool>,
    /// Keep trailing whitespace exactly as stored.
    #[serde(default)]
    pub raw: bool,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SpecParams {
    /// File to read. A glob (`specs/*/spec.md`) expands to every match; `~` is your home.
    pub path: String,
    /// `sed`-style line selection: `12`, `12-40`, `40-` (to the end), `12-40,98-120`.
    #[serde(default)]
    pub lines: Option<String>,
    /// First N lines. Combine with `tail` to see both ends of an unfamiliar file.
    #[serde(default)]
    pub head: Option<usize>,
    /// Last N lines.
    #[serde(default)]
    pub tail: Option<usize>,
    /// Start at the first line matching this regex and run to `to`, or to the end of the
    /// file — `sed -n '/pattern/,$p'`. Ignored when `lines` is given.
    #[serde(default)]
    pub from: Option<String>,
    /// End the selection at the next line matching this regex, inclusive. `{from: "^## \
    /// Invariants", to: "^## "}` is how you read one section of a document without
    /// knowing its line numbers; alone, it reads from the top down to the first match.
    #[serde(default)]
    pub to: Option<String>,
    /// Keep only lines matching this regex, within whatever `lines`/`head`/`tail` selected.
    /// Prefix with `(?i)` for case-insensitive.
    #[serde(default)]
    pub grep: Option<String>,
    /// Lines of context either side of a `grep` match.
    #[serde(default)]
    pub context: Option<usize>,
    /// Hard cap on lines from this file.
    #[serde(default)]
    pub max_lines: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct GrepParams {
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
    /// Treat the patterns as literal text.
    #[serde(default)]
    pub fixed: bool,
    #[serde(default)]
    pub ignore_case: bool,
    /// Lines of context either side of each match.
    #[serde(default)]
    pub context: usize,
    /// Matches rendered per file before truncating (default 20, `lines` mode only).
    #[serde(default)]
    pub max_per_file: Option<usize>,
    /// Matches rendered in total (default 200, `lines` mode only).
    #[serde(default)]
    pub max_matches: Option<usize>,
    /// `lines` (default) renders matching lines; `counts` one line per file with its match
    /// count; `files` just the paths. Use the cheaper modes to locate before reading.
    #[serde(default)]
    pub mode: Option<GrepMode>,
    /// Search dotfiles and dot-directories.
    #[serde(default)]
    pub hidden: bool,
    /// Search paths `.gitignore` excludes.
    #[serde(default)]
    pub no_ignore: bool,
    /// Maximum depth below each directory searched.
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrepMode {
    Lines,
    Counts,
    Files,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct FindParams {
    /// Directories or globs to list. Defaults to `.`.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Keep only entries matching these globs.
    #[serde(default)]
    pub glob: Vec<String>,
    /// Drop entries matching these globs.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// `any` (default), `file` or `dir`.
    #[serde(default)]
    pub kind: Option<EntryKind>,
    /// Maximum depth below each root. `1` is `ls`.
    #[serde(default)]
    pub depth: Option<usize>,
    /// Include dotfiles and dot-directories. `.git` is never listed either way.
    #[serde(default)]
    pub hidden: bool,
    /// List paths `.gitignore` excludes.
    #[serde(default)]
    pub no_ignore: bool,
    /// Follow symlinks.
    #[serde(default)]
    pub follow: bool,
    /// Entries rendered (default 500).
    #[serde(default)]
    pub limit: Option<usize>,
    /// One full path per line instead of grouping by directory.
    #[serde(default)]
    pub flat: bool,
    /// Add size and modification date. Implies `flat`.
    #[serde(default)]
    pub stat: bool,
    /// `path` (default, grouped), `size` or `modified`. Sorting by size or date renders
    /// flat, since grouping is alphabetical.
    #[serde(default)]
    pub sort: Option<SortBy>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Any,
    File,
    Dir,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SortBy {
    Path,
    Size,
    Modified,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InspectParams {
    /// Paths or globs to describe.
    pub paths: Vec<String>,
    /// Count lines, which reads each file. Defaults to true; turn it off to stat very
    /// large files cheaply.
    #[serde(default)]
    pub lines: Option<bool>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct FileOpsServer {
    /// Server-wide default budget, from `--max-bytes`. A request may still name its own.
    default_max_bytes: Option<usize>,
}

#[tool_router]
impl FileOpsServer {
    pub fn new(default_max_bytes: Option<usize>) -> Self {
        Self { default_max_bytes }
    }

    fn budget(&self, requested: Option<usize>) -> Option<usize> {
        requested.or(self.default_max_bytes)
    }

    #[tool(
        name = "read",
        description = "Read line slices of many files in one call — `cat`, `head`, `tail` and `sed -n` batched. Each spec names a `path` (a glob is fine) and optionally `lines` (`12-40,98-120`), `head`/`tail`, a `grep` filter with `context`, or `max_lines`. Output is one compact header per file saying exactly which lines follow, numbered by default, under a byte budget that reports what it truncated. Prefer this over a shell command that chains reads together."
    )]
    fn read(
        &self,
        Parameters(params): Parameters<ReadParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = ReadRequest {
            specs: params
                .specs
                .into_iter()
                .map(|spec| ReadSpec {
                    path: spec.path,
                    lines: spec.lines,
                    head: spec.head,
                    tail: spec.tail,
                    from: spec.from,
                    to: spec.to,
                    grep: spec.grep,
                    context: spec.context,
                    max_lines: spec.max_lines,
                })
                .collect(),
            cwd: params.cwd.map(PathBuf::from),
            max_bytes: self.budget(params.max_bytes),
            number: params.number.unwrap_or(true),
            raw: params.raw,
        };

        match fileops_fs::read(&request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "grep",
        description = "Search files for one or more regexes, grouped by file: the path is written once, matches carry their line numbers, and two caps (per file and per call) are always in force and always reported. `mode: counts` or `mode: files` answers \"where is this?\" for a fraction of the output. Respects `.gitignore` unless `no_ignore` is set. Prefer this over shell `grep -r`."
    )]
    fn grep(
        &self,
        Parameters(params): Parameters<GrepParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = GrepRequest {
            patterns: params.patterns,
            paths: params.paths,
            glob: params.glob,
            exclude: params.exclude,
            fixed: params.fixed,
            ignore_case: params.ignore_case,
            context: params.context,
            max_per_file: params.max_per_file,
            max_matches: params.max_matches,
            mode: match params.mode {
                None | Some(GrepMode::Lines) => fileops_fs::grep::Mode::Lines,
                Some(GrepMode::Counts) => fileops_fs::grep::Mode::Counts,
                Some(GrepMode::Files) => fileops_fs::grep::Mode::Files,
            },
            hidden: params.hidden,
            no_ignore: params.no_ignore,
            depth: params.depth,
            cwd: params.cwd.map(PathBuf::from),
            max_bytes: self.budget(params.max_bytes),
        };

        match fileops_fs::grep(&request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "find",
        description = "List a tree, grouped by directory so each directory is named once and its entries are packed onto wrapped lines — `ls`, `find` and `tree` in one call. `depth: 1` is `ls`; `glob`, `exclude` and `kind` narrow it; `stat` adds size and date; `sort: size` finds what is big. Skips `.gitignore`d and hidden paths, and never lists `.git`."
    )]
    fn find(
        &self,
        Parameters(params): Parameters<FindParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = FindRequest {
            roots: params.roots,
            glob: params.glob,
            exclude: params.exclude,
            kind: match params.kind {
                None | Some(EntryKind::Any) => fileops_fs::find::Kind::Any,
                Some(EntryKind::File) => fileops_fs::find::Kind::File,
                Some(EntryKind::Dir) => fileops_fs::find::Kind::Dir,
            },
            depth: params.depth,
            hidden: params.hidden,
            no_ignore: params.no_ignore,
            follow: params.follow,
            limit: params.limit,
            flat: params.flat,
            stat: params.stat,
            sort: match params.sort {
                None | Some(SortBy::Path) => fileops_fs::find::Sort::Path,
                Some(SortBy::Size) => fileops_fs::find::Sort::Size,
                Some(SortBy::Modified) => fileops_fs::find::Sort::Modified,
            },
            cwd: params.cwd.map(PathBuf::from),
            max_bytes: self.budget(params.max_bytes),
        };

        match fileops_fs::find(&request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "inspect",
        description = "Describe a batch of paths — size, line count, kind, modification date, whether it is a symlink or binary or simply missing — one line each. This is the cheap call that prevents an expensive one: check before reading anything you have not seen, and it is also the fastest way to confirm a path exists. `lines: false` skips opening files."
    )]
    fn inspect(
        &self,
        Parameters(params): Parameters<InspectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = InspectRequest {
            paths: params.paths,
            lines: params.lines.unwrap_or(true),
            cwd: params.cwd.map(PathBuf::from),
            max_bytes: self.budget(params.max_bytes),
        };

        match fileops_fs::inspect(&request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }
}

#[rmcp::tool_handler]
impl ServerHandler for FileOpsServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

/// A bad request comes back as a tool error rather than a protocol error, so the caller
/// reads what was wrong with it and fixes the call.
fn failed(err: fileops_fs::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.to_string())])
}

/// The rendered text is what a model reads; the structured copy is for anything that
/// would otherwise have to parse it.
fn with_structured<T: serde::Serialize>(text: String, value: &T) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = serde_json::to_value(value).ok();
    result
}
