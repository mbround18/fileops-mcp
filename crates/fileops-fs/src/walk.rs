//! Turning path arguments into real paths: `~` expansion, glob expansion, and the
//! directory walk that `find` and `grep` share.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

use crate::{Error, Result};

/// Resolve one literal path argument: `~` means `$HOME`, relative means relative to
/// `cwd` (the server's working directory when the request did not name one).
pub fn resolve(cwd: Option<&Path>, path: &str) -> PathBuf {
    let expanded = expand_home(path);
    if expanded.is_absolute() {
        return expanded;
    }
    match cwd {
        Some(cwd) => cwd.join(expanded),
        None => expanded,
    }
}

fn expand_home(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix('~') else {
        return PathBuf::from(path);
    };
    let Some(home) = std::env::var_os("HOME") else {
        return PathBuf::from(path);
    };
    match rest.strip_prefix('/') {
        Some(rest) => PathBuf::from(home).join(rest),
        // `~other/...` is another user's home, which is not ours to guess at.
        None if rest.is_empty() => PathBuf::from(home),
        None => PathBuf::from(path),
    }
}

pub fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '{'])
}

/// Expand one path argument into the paths it names.
///
/// A literal path comes back as itself even if nothing is there — a missing file is a line
/// of output, and silently dropping it would leave the caller wondering. A glob comes back
/// as whatever it matches, sorted, and matching nothing is simply an empty list.
///
/// Expansion ignores `.gitignore` and hidden-file rules: naming a path is asking for it.
pub fn expand(cwd: Option<&Path>, pattern: &str) -> Result<Vec<PathBuf>> {
    if !is_glob(pattern) {
        return Ok(vec![resolve(cwd, pattern)]);
    }

    let expanded = expand_home(pattern);
    let pattern_text = expanded.to_string_lossy().into_owned();
    let (root, depth) = glob_root(&pattern_text);
    let set = globs(std::slice::from_ref(&pattern_text))?.expect("one pattern");

    let base = resolve(cwd, &root);
    let mut builder = WalkBuilder::new(&base);
    builder
        .hidden(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .parents(false)
        .ignore(false)
        .follow_links(false);
    if let Some(depth) = depth {
        builder.max_depth(Some(depth));
    }

    // Matching happens on the path spelled the way the caller spelled it, so a relative
    // pattern is compared against a relative path.
    let prefix = cwd.filter(|_| !expanded.is_absolute());
    let mut matches: Vec<PathBuf> = Vec::new();
    for entry in builder.build().flatten() {
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();
        let candidate = match prefix {
            Some(prefix) => path.strip_prefix(prefix).unwrap_or(path),
            None => path,
        };
        if set.is_match(candidate) {
            matches.push(path.to_path_buf());
        }
    }
    matches.sort();
    matches.dedup();
    Ok(matches)
}

/// The deepest directory a glob cannot escape, plus how many components below it the
/// pattern can still reach (`None` once `**` is involved).
fn glob_root(pattern: &str) -> (String, Option<usize>) {
    let mut root = Vec::new();
    let mut rest = 0;
    let mut recursive = false;
    let mut components = pattern.split('/').peekable();
    while let Some(component) = components.next() {
        if rest == 0 && !is_glob(component) && components.peek().is_some() {
            root.push(component);
            continue;
        }
        if component.contains("**") {
            recursive = true;
        }
        rest += 1;
    }
    let root = if root.is_empty() {
        ".".to_owned()
    } else if root == [""] {
        "/".to_owned()
    } else {
        root.join("/")
    };
    (root, (!recursive).then_some(rest))
}

/// Build a glob set from `--glob`-style patterns. A pattern without a `/` matches a file
/// name at any depth, the way ripgrep's does; one with a `/` matches the whole path.
pub fn globs(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        // A name-only pattern is added twice — bare and under `**/` — so it matches at the
        // root as well as at depth without relying on `**/` matching zero components.
        let spellings = if pattern.contains('/') {
            vec![pattern.clone()]
        } else {
            vec![pattern.clone(), format!("**/{pattern}")]
        };
        for spelling in spellings {
            let glob = GlobBuilder::new(&spelling)
                // `*` stops at a path separator, so `src/*.rs` is not `src/**/*.rs`.
                .literal_separator(true)
                .build()
                .map_err(|err| Error::BadGlob {
                    glob: pattern.clone(),
                    detail: err.to_string(),
                })?;
            builder.add(glob);
        }
    }
    builder.build().map(Some).map_err(|err| Error::BadGlob {
        glob: patterns.join(","),
        detail: err.to_string(),
    })
}

/// What a walk is allowed to visit.
#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    /// Include dotfiles and dot-directories.
    pub hidden: bool,
    /// Walk paths `.gitignore` and friends exclude.
    pub no_ignore: bool,
    /// Maximum depth below each root; `None` is unlimited.
    pub depth: Option<usize>,
    /// Follow symlinks. Off by default, so a link loop cannot hang a listing.
    pub follow: bool,
}

/// A walker over `roots`. Ignore files are respected unless `no_ignore` is set, because a
/// listing full of `target/` and `node_modules/` is the most expensive output there is.
pub fn walker(roots: &[PathBuf], options: &WalkOptions) -> ignore::Walk {
    let (first, rest) = roots.split_first().expect("at least one root");
    let mut builder = WalkBuilder::new(first);
    for root in rest {
        builder.add(root);
    }
    let respect_ignore = !options.no_ignore;
    builder
        .hidden(!options.hidden)
        .git_ignore(respect_ignore)
        .git_exclude(respect_ignore)
        .git_global(respect_ignore)
        .ignore(respect_ignore)
        .parents(respect_ignore)
        // Ignore files are honoured whether or not this is a git repository; a listing of
        // someone's build output is expensive either way.
        .require_git(false)
        .follow_links(options.follow)
        .max_depth(options.depth)
        // Deterministic order: the same tree must render the same way every call.
        .sort_by_file_name(|a, b| a.cmp(b))
        // The `.git` directory is never what anyone meant, even with `hidden`.
        .filter_entry(|entry| entry.file_name() != ".git");
    builder.build()
}

/// How a path is written in output: relative to `cwd` when it is under it, so the common
/// case costs a few characters instead of an absolute path per line.
pub fn display(path: &Path, cwd: Option<&Path>) -> String {
    let trimmed = match cwd {
        Some(cwd) => path.strip_prefix(cwd).unwrap_or(path),
        None => path,
    };
    let text = trimmed.to_string_lossy();
    let text = text.strip_prefix("./").unwrap_or(&text);
    if text.is_empty() {
        ".".to_owned()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expands_only_as_our_own_home() {
        let home = std::env::var("HOME").expect("HOME");
        assert_eq!(resolve(None, "~/x/y"), PathBuf::from(&home).join("x/y"));
        assert_eq!(resolve(None, "~"), PathBuf::from(&home));
        assert_eq!(resolve(None, "~other/x"), PathBuf::from("~other/x"));
    }

    #[test]
    fn relative_paths_resolve_against_the_request_cwd() {
        let cwd = PathBuf::from("/repo");
        assert_eq!(
            resolve(Some(&cwd), "src/lib.rs"),
            PathBuf::from("/repo/src/lib.rs")
        );
        assert_eq!(
            resolve(Some(&cwd), "/etc/hosts"),
            PathBuf::from("/etc/hosts")
        );
    }

    #[test]
    fn a_glob_is_rooted_at_the_deepest_fixed_directory() {
        assert_eq!(glob_root("specs/010-*/*.md"), ("specs".into(), Some(2)));
        assert_eq!(glob_root("*.rs"), (".".into(), Some(1)));
        assert_eq!(glob_root("src/**/*.rs"), ("src".into(), None));
        assert_eq!(glob_root("/var/log/*.log"), ("/var/log".into(), Some(1)));
    }

    #[test]
    fn a_literal_path_survives_expansion_even_when_it_is_missing() {
        let paths = expand(None, "/nope/does-not-exist").unwrap();
        assert_eq!(paths, vec![PathBuf::from("/nope/does-not-exist")]);
    }

    #[test]
    fn globs_expand_against_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("specs/010-x")).unwrap();
        std::fs::write(root.join("specs/010-x/spec.md"), "a").unwrap();
        std::fs::write(root.join("specs/010-x/tasks.md"), "b").unwrap();
        std::fs::write(root.join("specs/010-x/notes.txt"), "c").unwrap();
        // Ignored paths are still reachable when named: expansion is not a search.
        std::fs::write(root.join(".gitignore"), "specs/\n").unwrap();

        let found = expand(Some(root), "specs/010-*/*.md").unwrap();
        let names: Vec<String> = found.iter().map(|p| display(p, Some(root))).collect();
        assert_eq!(names, vec!["specs/010-x/spec.md", "specs/010-x/tasks.md"]);
    }

    #[test]
    fn a_bare_glob_matches_a_file_name_at_any_depth() {
        let set = globs(&["*.rs".to_owned()]).unwrap().unwrap();
        assert!(set.is_match("src/deep/lib.rs"));
        assert!(!set.is_match("src/lib.md"));

        let set = globs(&["src/*.rs".to_owned()]).unwrap().unwrap();
        assert!(set.is_match("src/lib.rs"));
        assert!(
            !set.is_match("src/deep/lib.rs"),
            "a slash anchors the pattern"
        );
    }

    #[test]
    fn an_unparseable_glob_is_a_request_error() {
        assert!(matches!(
            globs(&["[".to_owned()]),
            Err(Error::BadGlob { .. })
        ));
        assert!(globs(&[]).unwrap().is_none());
    }

    #[test]
    fn paths_are_printed_relative_to_the_cwd() {
        let cwd = PathBuf::from("/repo");
        assert_eq!(
            display(Path::new("/repo/src/lib.rs"), Some(&cwd)),
            "src/lib.rs"
        );
        assert_eq!(display(Path::new("/etc/hosts"), Some(&cwd)), "/etc/hosts");
        assert_eq!(display(Path::new("/repo"), Some(&cwd)), ".");
    }
}
