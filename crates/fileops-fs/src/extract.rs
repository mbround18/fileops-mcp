//! Values out of structured documents: `jq '.a.b[0]'` without putting the document
//! through a context window first.
//!
//! A config file is read to answer a question about three of its keys, and the usual way
//! to do that is `cat` — which spends the whole file to learn one line of it. `extract`
//! parses JSON, YAML and TOML, selects the part that was asked for, and renders it as flat
//! `path = value` lines, which are both the cheapest form and the one a reader can act on.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    Budget, Error, Result,
    read::plural,
    text::{self, Content},
    walk,
};

/// Leaf lines rendered per spec before the rest are counted but not shown.
pub const DEFAULT_MAX_LEAVES: usize = 80;

/// Longest value rendered before it is elided.
const WIDTH: usize = 200;

/// One document to read, and which part of it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractSpec {
    /// Document to read. A glob expands to every match; `~` is your home.
    pub path: String,
    /// Where to look: `profile`, `extensions[0].name`, `jobs[].id`, `["odd.key"]`. The
    /// whole document when omitted.
    #[serde(default)]
    pub query: Option<String>,
    /// List what is at that path instead of its values: one line per key, with its type
    /// and size. The table of contents of a config.
    #[serde(default)]
    pub keys: bool,
    /// Stop descending this many levels below the query, rendering `{3 keys}` or `[12]`
    /// for what is deeper. Unlimited when omitted.
    #[serde(default)]
    pub depth: Option<usize>,
    /// Leaf lines from this document. Defaults to [`DEFAULT_MAX_LEAVES`].
    #[serde(default)]
    pub max_leaves: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractRequest {
    /// The batch. Three keys out of three config files is one call.
    pub specs: Vec<ExtractSpec>,
    /// Directory relative paths resolve against. Defaults to the server's own.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Ceiling on rendered output. Defaults to [`crate::DEFAULT_MAX_BYTES`].
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Extracted {
    /// The `#N` in the header, so a structured reader and a text reader agree.
    pub index: usize,
    pub path: String,
    /// `json`, `yaml`, `toml`, or why nothing was read: `missing`, `unparsed`, …
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Lines rendered for this document.
    pub shown: usize,
    /// Leaves the caps left out.
    pub dropped: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtractOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Clients differ over which half of a response they
    /// show, and one that reads only `structuredContent` renders nothing without
    /// this field, so the duplication is the price of being legible everywhere.
    pub text: String,
    pub documents: Vec<Extracted>,
    pub leaves: usize,
    /// Documents whose output was cut short.
    pub truncated: usize,
    /// Documents that could not be read, parsed, or matched.
    pub failed: usize,
}

/// Extract every spec in the batch, in order, against one shared byte budget.
pub fn extract(request: &ExtractRequest) -> Result<ExtractOutcome> {
    if request.specs.is_empty() {
        return Err(Error::InvalidRequest(
            "pass at least one spec: {\"specs\": [{\"path\": \"...\", \"query\": \"...\"}]}".into(),
        ));
    }
    let cwd = request.cwd.as_deref();
    let mut budget = Budget::new(request.max_bytes);
    let mut out = String::new();
    let mut documents: Vec<Extracted> = Vec::new();
    let mut leaves = 0;
    let mut truncated = 0;
    let mut failed = 0;

    for spec in &request.specs {
        let steps = match &spec.query {
            Some(query) => parse(query)?,
            None => Vec::new(),
        };
        let max_leaves = spec.max_leaves.unwrap_or(DEFAULT_MAX_LEAVES);

        for path in walk::expand(cwd, &spec.path)? {
            let index = documents.len() + 1;
            let shown_path = walk::display(&path, cwd);
            let mut note = |status: &'static str, detail: Option<String>, text: String| {
                let line = format!("#{index} {shown_path} {text}\n");
                budget.spend(line.len());
                out.push_str(&line);
                documents.push(Extracted {
                    index,
                    path: shown_path.clone(),
                    status,
                    detail,
                    shown: 0,
                    dropped: 0,
                });
            };

            let Some(format) = Format::of(&path) else {
                note("unsupported", None, "(no parser for this type)".into());
                failed += 1;
                continue;
            };
            let content = match text::read_text(&path) {
                Content::Text(content) => content,
                Content::Unavailable(status) => {
                    note(status.kind(), status.detail(), status.note());
                    failed += 1;
                    continue;
                }
            };
            let document = match format.parse(&content) {
                Ok(document) => document,
                Err(detail) => {
                    note(
                        "unparsed",
                        Some(detail.clone()),
                        format!("(unparsed: {detail})"),
                    );
                    failed += 1;
                    continue;
                }
            };

            let selected = select(&document, &steps);
            if selected.is_empty() {
                let query = spec.query.as_deref().unwrap_or(".");
                note("no_match", None, format!("(no match for {query})"));
                failed += 1;
                continue;
            }

            // Lines first, then the header, so the header can report what was cut.
            let mut lines = Vec::new();
            for (prefix, value) in &selected {
                if spec.keys {
                    keys(prefix, value, &mut lines);
                } else {
                    flatten(prefix, value, spec.depth, &mut lines);
                }
            }
            let found = lines.len();
            lines.truncate(max_leaves);

            let mut body = String::new();
            let mut rendered = 0;
            for line in &lines {
                let line = format!("{line}\n");
                if !budget.try_spend(line.len()) {
                    break;
                }
                body.push_str(&line);
                rendered += 1;
            }

            let dropped = found - rendered;
            let mut header = format!("#{index} {shown_path}");
            if let Some(query) = &spec.query {
                header.push(' ');
                header.push_str(query);
            }
            header.push_str(&format!(" ({rendered}"));
            if dropped > 0 {
                header.push_str(&format!(" of {found}"));
            }
            header.push_str(")\n");
            budget.spend(header.len());
            out.push_str(&header);
            out.push_str(&body);

            leaves += found;
            if dropped > 0 {
                truncated += 1;
            }
            documents.push(Extracted {
                index,
                path: shown_path,
                status: format.name(),
                detail: None,
                shown: rendered,
                dropped,
            });
        }
    }

    let read = documents.iter().filter(|d| d.shown > 0).count();
    let mut footer = format!(
        "[{} document{}, {leaves} value{}",
        documents.len(),
        plural(documents.len()),
        plural(leaves),
    );
    if read != documents.len() {
        footer.push_str(&format!(", {failed} unread"));
    }
    if truncated > 0 {
        footer.push_str(&format!(
            ", {truncated} truncated: raise max_leaves/max_bytes or narrow the query"
        ));
    }
    footer.push_str("]\n");
    out.push_str(&footer);

    Ok(ExtractOutcome {
        text: out,
        documents,
        leaves,
        truncated,
        failed,
    })
}

/// One step along a query.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(usize),
    /// `[]` — every element of an array, or every value of a map.
    Each,
}

/// Parse a query: `a.b`, `a[0].b`, `a[].b`, `["odd.key"]`, with or without a leading dot.
fn parse(query: &str) -> Result<Vec<Step>> {
    let bad = || Error::BadQuery {
        query: query.to_owned(),
    };
    let mut steps = Vec::new();
    let mut rest = query.trim();
    // A leading dot is optional (`.a.b` and `a.b` are the same query), but everything
    // after that is exact: `a..b` is a typo, not a shorthand.
    rest = rest.strip_prefix('.').unwrap_or(rest);
    if rest.is_empty() {
        return Err(bad());
    }
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix('[') {
            let (inside, after) = tail.split_once(']').ok_or_else(bad)?;
            let inside = inside.trim();
            if inside.is_empty() {
                steps.push(Step::Each);
            } else if let Some(quoted) = inside
                .strip_prefix('"')
                .and_then(|i| i.strip_suffix('"'))
                .or_else(|| inside.strip_prefix('\'').and_then(|i| i.strip_suffix('\'')))
            {
                steps.push(Step::Key(quoted.to_owned()));
            } else {
                steps.push(Step::Index(inside.parse().map_err(|_| bad())?));
            }
            rest = after;
        } else {
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            let (key, after) = rest.split_at(end);
            if key.is_empty() {
                return Err(bad());
            }
            steps.push(Step::Key(key.to_owned()));
            rest = after;
        }
        if let Some(tail) = rest.strip_prefix('.') {
            if tail.is_empty() || tail.starts_with('.') {
                return Err(bad());
            }
            rest = tail;
        }
    }
    Ok(steps)
}

/// Walk a query over a document. `[]` fans out, so one query can select many values, and
/// each result carries the path it was found at — a line nobody has to count back to.
fn select<'a>(document: &'a Value, steps: &[Step]) -> Vec<(String, &'a Value)> {
    let mut current = vec![(String::new(), document)];
    for step in steps {
        let mut next = Vec::new();
        for (prefix, value) in current {
            match step {
                Step::Key(key) => {
                    if let Some(found) = value.get(key.as_str()) {
                        next.push((join(&prefix, key), found));
                    }
                }
                Step::Index(index) => {
                    if let Some(found) = value.get(index) {
                        next.push((format!("{prefix}[{index}]"), found));
                    }
                }
                Step::Each => match value {
                    Value::Array(items) => next.extend(
                        items
                            .iter()
                            .enumerate()
                            .map(|(index, item)| (format!("{prefix}[{index}]"), item)),
                    ),
                    Value::Object(map) => {
                        next.extend(map.iter().map(|(key, item)| (join(&prefix, key), item)))
                    }
                    _ => {}
                },
            }
        }
        current = next;
    }
    current
}

/// Every leaf under a value, as `path = value` lines. A container deeper than `depth` is
/// summarised by its size rather than expanded.
fn flatten(prefix: &str, value: &Value, depth: Option<usize>, out: &mut Vec<String>) {
    let spent = depth == Some(0);
    match value {
        Value::Object(map) if !map.is_empty() => {
            if spent {
                out.push(format!("{} {}", label(prefix), summary(value)));
                return;
            }
            for (key, item) in map {
                flatten(&join(prefix, key), item, depth.map(|d| d - 1), out);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            if spent {
                out.push(format!("{} {}", label(prefix), summary(value)));
                return;
            }
            for (index, item) in items.iter().enumerate() {
                flatten(
                    &format!("{prefix}[{index}]"),
                    item,
                    depth.map(|d| d - 1),
                    out,
                );
            }
        }
        _ => out.push(format!("{} = {}", label(prefix), scalar(value))),
    }
}

/// What is directly at a path: one line per key, with its type and size.
fn keys(prefix: &str, value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => out.extend(
            map.iter()
                .map(|(key, item)| format!("{key} {}", summary(item))),
        ),
        Value::Array(items) => out.extend(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| format!("[{index}] {}", summary(item))),
        ),
        _ => out.push(format!("{} = {}", label(prefix), scalar(value))),
    }
}

/// A value's shape, for a line that is not showing the value itself.
fn summary(value: &Value) -> String {
    match value {
        Value::Object(map) => format!("{{{} key{}}}", map.len(), plural(map.len())),
        Value::Array(items) => format!("[{}]", items.len()),
        Value::String(_) => "str".into(),
        Value::Number(_) => "num".into(),
        Value::Bool(_) => "bool".into(),
        Value::Null => "null".into(),
    }
}

/// A scalar as one line: strings bare (quoting every string doubles the punctuation for
/// nothing), everything else as JSON writes it.
fn scalar(value: &Value) -> String {
    let rendered = match value {
        Value::String(text) => text.replace('\n', "\\n"),
        Value::Object(_) => "{}".into(),
        Value::Array(_) => "[]".into(),
        other => other.to_string(),
    };
    if rendered.chars().count() <= WIDTH {
        return rendered;
    }
    let mut short: String = rendered.chars().take(WIDTH).collect();
    short.push('…');
    short
}

/// The document root has no name of its own, so it is written as `.`.
fn label(prefix: &str) -> &str {
    if prefix.is_empty() { "." } else { prefix }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

/// The document formats with a parser here.
enum Format {
    Json,
    Yaml,
    Toml,
}

impl Format {
    fn of(path: &std::path::Path) -> Option<Self> {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "json" => Some(Self::Json),
            "yaml" | "yml" => Some(Self::Yaml),
            "toml" | "lock" => Some(Self::Toml),
            _ => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
        }
    }

    /// Every format is parsed into one shape, so the query and the rendering are written
    /// once rather than three times.
    fn parse(&self, content: &str) -> std::result::Result<Value, String> {
        match self {
            Self::Json => serde_json::from_str(content)
                .map_err(|err| crate::read::first_line(&err.to_string())),
            Self::Yaml => serde_yaml_ng::from_str(content)
                .map_err(|err| crate::read::first_line(&err.to_string())),
            Self::Toml => {
                toml::from_str(content).map_err(|err| crate::read::first_line(&err.to_string()))
            }
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
            std::fs::write(self.path().join(name), body).unwrap();
            self
        }

        fn run(&self, specs: Vec<ExtractSpec>) -> ExtractOutcome {
            extract(&ExtractRequest {
                specs,
                cwd: Some(self.path().to_path_buf()),
                max_bytes: None,
            })
            .unwrap()
        }
    }

    fn spec(path: &str, query: Option<&str>) -> ExtractSpec {
        ExtractSpec {
            path: path.to_owned(),
            query: query.map(str::to_owned),
            ..Default::default()
        }
    }

    const JSON: &str = r#"{"profile": "spec-kit", "retries": 3, "ok": true,
        "integrations": [{"name": "nats", "port": 4222}, {"name": "pg", "port": 5432}]}"#;

    #[test]
    fn one_call_reaches_into_a_batch_of_documents() {
        let fixture = Fixture::new();
        fixture
            .write("options.json", JSON)
            .write(
                "extensions.yml",
                "extensions:\n  - name: specify\n    version: 2\n",
            )
            .write(
                "Cargo.toml",
                "[package]\nname = \"x\"\nedition = \"2024\"\n",
            );

        let outcome = fixture.run(vec![
            spec("options.json", Some("integrations[].name")),
            spec("extensions.yml", Some("extensions[0]")),
            spec("Cargo.toml", Some("package.edition")),
        ]);

        assert_eq!(
            outcome.text,
            "#1 options.json integrations[].name (2)\n\
             integrations[0].name = nats\n\
             integrations[1].name = pg\n\
             #2 extensions.yml extensions[0] (2)\n\
             extensions[0].name = specify\n\
             extensions[0].version = 2\n\
             #3 Cargo.toml package.edition (1)\n\
             package.edition = 2024\n\
             [3 documents, 5 values]\n"
        );
    }

    #[test]
    fn the_whole_document_flattens_to_one_line_per_leaf() {
        let fixture = Fixture::new();
        fixture.write("options.json", JSON);

        let outcome = fixture.run(vec![spec("options.json", None)]);

        assert_eq!(
            outcome.text,
            "#1 options.json (7)\n\
             integrations[0].name = nats\n\
             integrations[0].port = 4222\n\
             integrations[1].name = pg\n\
             integrations[1].port = 5432\n\
             ok = true\n\
             profile = spec-kit\n\
             retries = 3\n\
             [1 document, 7 values]\n"
        );
    }

    #[test]
    fn depth_summarises_what_it_does_not_expand() {
        let fixture = Fixture::new();
        fixture.write("options.json", JSON);

        let outcome = fixture.run(vec![ExtractSpec {
            path: "options.json".into(),
            depth: Some(1),
            ..Default::default()
        }]);

        assert!(
            outcome.text.contains("integrations [2]\n"),
            "{}",
            outcome.text
        );
        assert!(
            outcome.text.contains("profile = spec-kit\n"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn keys_are_a_table_of_contents_rather_than_a_value_list() {
        let fixture = Fixture::new();
        fixture.write("options.json", JSON);

        let outcome = fixture.run(vec![ExtractSpec {
            path: "options.json".into(),
            keys: true,
            ..Default::default()
        }]);

        assert_eq!(
            outcome.text,
            "#1 options.json (4)\n\
             integrations [2]\n\
             ok bool\n\
             profile str\n\
             retries num\n\
             [1 document, 4 values]\n"
        );
    }

    #[test]
    fn a_query_can_name_a_key_that_contains_a_dot() {
        let fixture = Fixture::new();
        fixture.write("a.json", r#"{"a.b": {"c": 1}}"#);

        let outcome = fixture.run(vec![spec("a.json", Some("[\"a.b\"].c"))]);

        assert!(outcome.text.contains("a.b.c = 1\n"), "{}", outcome.text);
    }

    #[test]
    fn queries_that_cannot_mean_anything_are_refused() {
        for bad in ["[", "[x]", "a..b", "a[1", "."] {
            assert!(
                matches!(parse(bad), Err(Error::BadQuery { .. })),
                "`{bad}` should be refused"
            );
        }
        assert_eq!(parse("a.b[0]").unwrap().len(), 3);
        assert_eq!(parse(".a[].b").unwrap()[1], Step::Each);
    }

    #[test]
    fn a_document_that_cannot_be_used_is_one_line_not_a_failed_call() {
        let fixture = Fixture::new();
        fixture
            .write("broken.json", "{not json")
            .write("notes.txt", "prose\n")
            .write("options.json", JSON);

        let outcome = fixture.run(vec![
            spec("broken.json", None),
            spec("notes.txt", None),
            spec("gone.json", None),
            spec("options.json", Some("nope.deeper")),
            spec("options.json", Some("profile")),
        ]);

        assert!(
            outcome.text.contains("#1 broken.json (unparsed: "),
            "{}",
            outcome.text
        );
        assert!(
            outcome
                .text
                .contains("#2 notes.txt (no parser for this type)\n"),
            "{}",
            outcome.text
        );
        assert!(
            outcome.text.contains("#3 gone.json (missing)\n"),
            "{}",
            outcome.text
        );
        assert!(
            outcome
                .text
                .contains("#4 options.json (no match for nope.deeper)\n"),
            "{}",
            outcome.text
        );
        assert!(
            outcome
                .text
                .contains("#5 options.json profile (1)\nprofile = spec-kit\n"),
            "{}",
            outcome.text
        );
        assert_eq!(outcome.failed, 4);
        assert!(outcome.text.contains("4 unread"), "{}", outcome.text);
    }

    #[test]
    fn a_long_value_is_elided_and_newlines_do_not_break_the_line_format() {
        let fixture = Fixture::new();
        let long = "x".repeat(400);
        fixture.write(
            "a.json",
            &serde_json::json!({"long": long, "multi": "a\nb"}).to_string(),
        );

        let outcome = fixture.run(vec![spec("a.json", None)]);

        assert!(outcome.text.contains("multi = a\\nb\n"), "{}", outcome.text);
        let line = outcome
            .text
            .lines()
            .find(|line| line.starts_with("long = "))
            .unwrap();
        assert_eq!(line.chars().count(), "long = ".len() + WIDTH + 1);
    }

    #[test]
    fn the_leaf_cap_reports_what_it_left_out() {
        let fixture = Fixture::new();
        let items: Vec<usize> = (1..=20).collect();
        fixture.write("a.json", &serde_json::json!({"items": items}).to_string());

        let outcome = fixture.run(vec![ExtractSpec {
            path: "a.json".into(),
            max_leaves: Some(3),
            ..Default::default()
        }]);

        assert!(
            outcome.text.contains("#1 a.json (3 of 20)\n"),
            "{}",
            outcome.text
        );
        assert_eq!(
            outcome.leaves, 20,
            "the count is honest about what is there"
        );
        assert_eq!(outcome.documents[0].dropped, 17);
        assert!(outcome.text.contains("1 truncated"), "{}", outcome.text);
    }

    #[test]
    fn an_empty_batch_is_a_request_error() {
        assert!(matches!(
            extract(&ExtractRequest::default()),
            Err(Error::InvalidRequest(_))
        ));
    }

    #[test]
    fn a_glob_reaches_every_document_of_a_shape() {
        let fixture = Fixture::new();
        fixture
            .write("a.json", r#"{"id": "a"}"#)
            .write("b.json", r#"{"id": "b"}"#);

        let outcome = fixture.run(vec![spec("*.json", Some("id"))]);

        assert_eq!(outcome.documents.len(), 2);
        assert!(
            outcome.text.contains("#1 a.json id (1)\nid = a\n"),
            "{}",
            outcome.text
        );
        assert!(
            outcome.text.contains("#2 b.json id (1)\nid = b\n"),
            "{}",
            outcome.text
        );
    }
}
