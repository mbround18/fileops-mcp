//! Listings: `ls`, `find` and `tree` in one call, grouped by directory.
//!
//! A flat listing repeats the directory on every line, which is the bulk of its cost in a
//! context window. Grouping writes each directory once and packs its entries onto wrapped
//! lines, so a few hundred paths read in a handful of lines. `.gitignore` is respected by
//! default for the same reason: nobody asked to see `target/`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    Budget, Error, Result,
    read::plural,
    text::{self, human},
    walk::{self, WalkOptions},
};

/// Default ceiling on entries rendered.
pub const DEFAULT_LIMIT: usize = 500;
/// Target width for a packed line of entry names.
const WIDTH: usize = 96;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Any,
    File,
    Dir,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    /// Alphabetical, which is also how the entries are grouped.
    #[default]
    Path,
    /// Largest first.
    Size,
    /// Most recently modified first.
    Modified,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindRequest {
    /// Directories (or globs) to list. Defaults to `.`.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Keep only entries matching these globs (`*.rs`, `specs/**/tasks.md`).
    #[serde(default)]
    pub glob: Vec<String>,
    /// Drop entries matching these globs.
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub kind: Kind,
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
    /// Entries to render. Defaults to [`DEFAULT_LIMIT`].
    #[serde(default)]
    pub limit: Option<usize>,
    /// One full path per line instead of grouping by directory. Implied by `stat`.
    #[serde(default)]
    pub flat: bool,
    /// Add size and modification date, which forces the flat form.
    #[serde(default)]
    pub stat: bool,
    #[serde(default)]
    pub sort: Sort,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub path: String,
    pub dir: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindOutcome {
    ///
    /// Serialized deliberately. Clients differ over which half of a response they
    /// show, and one that reads only `structuredContent` renders nothing without
    /// this field, so the duplication is the price of being legible everywhere.
    pub text: String,
    pub entries: Vec<Entry>,
    /// Entries found, including any the limit or budget kept out.
    pub found: usize,
    pub truncated: bool,
}

pub fn find(request: &FindRequest) -> Result<FindOutcome> {
    let include = walk::globs(&request.glob)?;
    let exclude = walk::globs(&request.exclude)?;
    let cwd = request.cwd.as_deref();
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);

    let roots = if request.roots.is_empty() {
        vec![".".to_owned()]
    } else {
        request.roots.clone()
    };
    let mut resolved = Vec::new();
    for root in &roots {
        resolved.extend(walk::expand(cwd, root)?);
    }
    if resolved.is_empty() {
        return Err(Error::InvalidRequest(format!(
            "none of {} exists",
            roots.join(", ")
        )));
    }

    let options = WalkOptions {
        hidden: request.hidden,
        no_ignore: request.no_ignore,
        depth: request.depth,
        follow: request.follow,
    };

    let mut entries: Vec<Entry> = Vec::new();
    for entry in walk::walker(&resolved, &options).flatten() {
        let dir = entry.file_type().is_some_and(|t| t.is_dir());
        // The roots themselves are the question, not part of the answer.
        if entry.depth() == 0 {
            continue;
        }
        match request.kind {
            Kind::Any => {}
            Kind::File if dir => continue,
            Kind::Dir if !dir => continue,
            _ => {}
        }
        let path = walk::display(entry.path(), cwd);
        if include.as_ref().is_some_and(|set| !set.is_match(&path)) {
            continue;
        }
        if exclude.as_ref().is_some_and(|set| set.is_match(&path)) {
            continue;
        }
        let meta = entry.metadata().ok();
        entries.push(Entry {
            path,
            dir,
            bytes: meta.as_ref().filter(|_| !dir).map(|m| m.len()),
            modified: meta.as_ref().and_then(|m| m.modified().ok()).map(date),
        });
    }

    let found = entries.len();
    match request.sort {
        Sort::Path => entries.sort_by(|a, b| a.path.cmp(&b.path)),
        Sort::Size => entries.sort_by_key(|e| std::cmp::Reverse(e.bytes.unwrap_or(0))),
        Sort::Modified => entries.sort_by(|a, b| b.modified.cmp(&a.modified)),
    }
    let mut truncated = found > limit;
    entries.truncate(limit);

    let mut budget = Budget::new(request.max_bytes);
    // Grouping is alphabetical by construction, so a size or date ordering would be lost
    // in it; those render flat.
    let body = if request.stat || request.flat || request.sort != Sort::Path {
        flat(&entries, request.stat, &mut budget, &mut truncated)
    } else {
        grouped(&entries, &mut budget, &mut truncated)
    };

    let dirs = entries.iter().filter(|e| e.dir).count();
    let shown = entries.len();
    let mut footer = format!(
        "[{} of {found} entr{} ({} dir{})",
        shown,
        if found == 1 { "y" } else { "ies" },
        dirs,
        plural(dirs)
    );
    if truncated {
        footer.push_str(", truncated: raise limit/max_bytes or narrow with glob/depth");
    }
    footer.push_str("]\n");

    Ok(FindOutcome {
        text: format!("{body}{footer}"),
        entries,
        found,
        truncated,
    })
}

/// One path per line, optionally with size and date.
fn flat(entries: &[Entry], stat: bool, budget: &mut Budget, truncated: &mut bool) -> String {
    let mut out = String::new();
    for entry in entries {
        let suffix = if entry.dir { "/" } else { "" };
        let line = if stat {
            format!(
                "{}{suffix} {} {}\n",
                entry.path,
                entry.bytes.map(human).unwrap_or_else(|| "-".into()),
                entry.modified.as_deref().unwrap_or("-")
            )
        } else {
            format!("{}{suffix}\n", entry.path)
        };
        if !budget.try_spend(line.len()) {
            *truncated = true;
            break;
        }
        out.push_str(&line);
    }
    out
}

/// Directory headers with their entries packed onto wrapped lines.
///
/// Entries are collected per directory first, so each directory is named exactly once
/// however the walk interleaved them.
fn grouped(entries: &[Entry], budget: &mut Budget, truncated: &mut bool) -> String {
    let mut groups: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    for entry in entries {
        let (parent, name) = split(&entry.path);
        let name = if entry.dir {
            format!("{name}/")
        } else {
            name.to_owned()
        };
        groups.entry(parent).or_default().push(name);
    }

    let mut out = String::new();
    for (parent, mut names) in groups {
        names.sort();
        let header = format!("{}/\n", if parent.is_empty() { "." } else { parent });
        if !budget.try_spend(header.len()) {
            *truncated = true;
            return out;
        }
        out.push_str(&header);

        let mut line = String::new();
        for name in names {
            if !line.is_empty() && line.len() + name.len() + 1 > WIDTH {
                line.push('\n');
                if !budget.try_spend(line.len()) {
                    *truncated = true;
                    return out;
                }
                out.push_str(&line);
                line.clear();
            }
            if line.is_empty() {
                line.push_str("  ");
            } else {
                line.push(' ');
            }
            line.push_str(&name);
        }
        line.push('\n');
        if !budget.try_spend(line.len()) {
            *truncated = true;
            return out;
        }
        out.push_str(&line);
    }
    out
}

fn split(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}

/// `YYYY-MM-DD`, computed from the epoch seconds — a date is enough to sort by eye, and a
/// full timestamp costs more than it tells.
pub(crate) fn date(time: std::time::SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's `civil_from_days`: days since the epoch to a calendar date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Line counts for `inspect`, which shares the walk but not the rendering.
pub(crate) fn count_lines(path: &std::path::Path) -> Option<usize> {
    match text::read_text(path) {
        text::Content::Text(content) => Some(text::lines(&content).len()),
        text::Content::Unavailable(_) => None,
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

        fn run(&self, mut request: FindRequest) -> FindOutcome {
            request.cwd = Some(self.path().to_path_buf());
            find(&request).unwrap()
        }
    }

    fn sample() -> Fixture {
        let fixture = Fixture::new();
        fixture
            .write("src/lib.rs", "a\n")
            .write("src/main.rs", "b\n")
            .write("src/deep/mod.rs", "c\n")
            .write("README.md", "d\n");
        fixture
    }

    #[test]
    fn a_directory_is_named_once_and_its_entries_are_packed() {
        let outcome = sample().run(FindRequest::default());
        assert_eq!(
            outcome.text,
            "\
./
  README.md src/
src/
  deep/ lib.rs main.rs
src/deep/
  mod.rs
[6 of 6 entries (2 dirs)]
"
        );
    }

    #[test]
    fn depth_one_is_ls() {
        let outcome = sample().run(FindRequest {
            depth: Some(1),
            ..Default::default()
        });
        assert_eq!(
            outcome.text,
            "./\n  README.md src/\n[2 of 2 entries (1 dir)]\n"
        );
    }

    #[test]
    fn globs_and_kind_narrow_what_is_listed() {
        let fixture = sample();
        let rust = fixture.run(FindRequest {
            glob: vec!["*.rs".to_owned()],
            ..Default::default()
        });
        assert_eq!(rust.found, 3, "{}", rust.text);

        let dirs = fixture.run(FindRequest {
            kind: Kind::Dir,
            ..Default::default()
        });
        assert_eq!(dirs.entries.iter().filter(|e| !e.dir).count(), 0);

        let without_deep = fixture.run(FindRequest {
            kind: Kind::File,
            exclude: vec!["src/deep/*".to_owned()],
            ..Default::default()
        });
        assert!(
            !without_deep.text.contains("mod.rs"),
            "{}",
            without_deep.text
        );
    }

    #[test]
    fn the_flat_form_is_there_when_full_paths_are_wanted() {
        let outcome = sample().run(FindRequest {
            kind: Kind::File,
            flat: true,
            ..Default::default()
        });
        assert_eq!(
            outcome.text,
            "README.md\nsrc/deep/mod.rs\nsrc/lib.rs\nsrc/main.rs\n[4 of 4 entries (0 dirs)]\n"
        );
    }

    #[test]
    fn stat_adds_size_and_date_and_implies_the_flat_form() {
        let fixture = Fixture::new();
        fixture.write("a.txt", "0123456789");
        let outcome = fixture.run(FindRequest {
            kind: Kind::File,
            stat: true,
            ..Default::default()
        });
        let line = outcome.text.lines().next().unwrap();
        assert!(line.starts_with("a.txt 10B 2"), "{line}");
    }

    #[test]
    fn ignored_paths_stay_out_unless_asked_for() {
        let fixture = Fixture::new();
        fixture
            .write(".gitignore", "target\n")
            .write("target/debug/huge", "x")
            .write(".env", "secret")
            .write("src/a.rs", "a\n");

        let default = fixture.run(FindRequest::default());
        assert!(!default.text.contains("target"), "{}", default.text);
        assert!(!default.text.contains(".env"), "{}", default.text);

        let everything = fixture.run(FindRequest {
            hidden: true,
            no_ignore: true,
            ..Default::default()
        });
        assert!(everything.text.contains("target/"), "{}", everything.text);
        assert!(everything.text.contains(".env"), "{}", everything.text);
    }

    #[test]
    fn the_git_directory_is_never_listed() {
        let fixture = Fixture::new();
        fixture.write(".git/config", "x").write("a.txt", "a\n");
        let outcome = fixture.run(FindRequest {
            hidden: true,
            no_ignore: true,
            ..Default::default()
        });
        assert!(!outcome.text.contains(".git"), "{}", outcome.text);
    }

    #[test]
    fn a_limit_truncates_and_says_how_to_see_more() {
        let fixture = Fixture::new();
        for n in 0..50 {
            fixture.write(&format!("f{n:02}.txt"), "x\n");
        }
        let outcome = fixture.run(FindRequest {
            limit: Some(10),
            ..Default::default()
        });
        assert_eq!(outcome.entries.len(), 10);
        assert_eq!(outcome.found, 50);
        assert!(outcome.truncated);
        assert!(
            outcome.text.contains("[10 of 50 entries"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn sorting_by_size_puts_the_big_ones_first() {
        let fixture = Fixture::new();
        fixture
            .write("small.txt", "x")
            .write("big.txt", &"x".repeat(5000));
        let outcome = fixture.run(FindRequest {
            kind: Kind::File,
            sort: Sort::Size,
            stat: true,
            ..Default::default()
        });
        assert!(outcome.text.starts_with("big.txt 4.9K"), "{}", outcome.text);
    }

    #[test]
    fn a_root_that_is_not_there_is_a_request_error() {
        let fixture = Fixture::new();
        let err = find(&FindRequest {
            roots: vec!["nope/*".to_owned()],
            cwd: Some(fixture.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }

    #[test]
    fn dates_are_computed_without_a_calendar_crate() {
        let epoch = std::time::UNIX_EPOCH;
        assert_eq!(date(epoch), "1970-01-01");
        assert_eq!(
            date(epoch + std::time::Duration::from_secs(1_767_225_600)),
            "2026-01-01"
        );
        // A leap day, which is where a hand-rolled conversion would go wrong.
        assert_eq!(
            date(epoch + std::time::Duration::from_secs(1_709_164_800)),
            "2024-02-29"
        );
    }
}
