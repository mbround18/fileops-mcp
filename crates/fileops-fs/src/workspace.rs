//! Read-only workspace inventory for sibling checkouts and local symlink wiring.
//!
//! This replaces ad-hoc shell probes like `ls ../Prefix-*` and repeated `readlink/stat`
//! loops with one bounded call that returns structured data plus concise text.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceInventoryRequest {
    /// Prefix used to match sibling directories (`<prefix>-*`).
    #[serde(default)]
    pub prefix: Option<String>,
    /// Directory containing sibling workspaces. Defaults to parent of `cwd`.
    #[serde(default)]
    pub base_dir: Option<PathBuf>,
    /// Names to inspect as local links/symlinks under `cwd`.
    #[serde(default)]
    pub inspect_links: Vec<String>,
    /// Directory relative paths resolve against. Defaults to process cwd.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceInventoryOutcome {
    pub base_dir: String,
    pub prefix: String,
    pub siblings: Vec<SiblingEntry>,
    pub links: Vec<LinkEntry>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SiblingEntry {
    pub name: String,
    pub path: String,
    pub has_git_dir: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkEntry {
    pub name: String,
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
}

pub fn workspace_inventory(
    request: &WorkspaceInventoryRequest,
) -> Result<WorkspaceInventoryOutcome> {
    let cwd = request.cwd.clone().unwrap_or_else(|| PathBuf::from("."));
    let cwd_abs = if cwd.is_absolute() {
        cwd
    } else {
        std::env::current_dir()
            .map_err(|e| Error::InvalidRequest(format!("could not read current directory: {e}")))?
            .join(cwd)
    };
    let prefix = request
        .prefix
        .as_deref()
        .unwrap_or("ThunderForgeVTT")
        .trim()
        .to_owned();
    if prefix.is_empty() {
        return Err(Error::InvalidRequest("prefix cannot be empty".into()));
    }

    let base_dir = match &request.base_dir {
        Some(dir) if dir.is_absolute() => dir.clone(),
        Some(dir) => cwd_abs.join(dir),
        None => cwd_abs
            .parent()
            .ok_or_else(|| {
                Error::InvalidRequest("cannot infer base directory from filesystem root".into())
            })?
            .to_path_buf(),
    };

    let mut siblings = collect_siblings(&base_dir, &prefix)?;
    siblings.sort_by(|a, b| a.name.cmp(&b.name));

    let mut links = Vec::new();
    for name in &request.inspect_links {
        links.push(inspect_link(&cwd_abs, name));
    }

    let mut text = String::new();
    text.push_str(&format!(
        "base: {}\nprefix: {}\n\n",
        base_dir.display(),
        prefix
    ));
    if siblings.is_empty() {
        text.push_str("siblings: none\n");
    } else {
        text.push_str("siblings:\n");
        for s in &siblings {
            text.push_str(&format!(
                "- {} ({}){}\n",
                s.name,
                s.path,
                if s.has_git_dir { " [.git]" } else { "" }
            ));
        }
    }
    if !links.is_empty() {
        text.push_str("\nlinks:\n");
        for l in &links {
            text.push_str(&format!("- {}: {}", l.name, l.kind));
            if let Some(t) = &l.target {
                text.push_str(&format!(" -> {t}"));
            }
            if let Some(r) = &l.resolved {
                text.push_str(&format!(" (resolves to {r})"));
            }
            text.push('\n');
        }
    }

    Ok(WorkspaceInventoryOutcome {
        base_dir: base_dir.display().to_string(),
        prefix,
        siblings,
        links,
        text,
    })
}

fn collect_siblings(base_dir: &Path, prefix: &str) -> Result<Vec<SiblingEntry>> {
    let mut out = Vec::new();
    let entries = fs::read_dir(base_dir).map_err(|e| {
        Error::InvalidRequest(format!(
            "could not read base directory `{}`: {e}",
            base_dir.display()
        ))
    })?;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(&format!("{prefix}-")) {
            continue;
        }
        out.push(SiblingEntry {
            name: name.to_owned(),
            path: path.display().to_string(),
            has_git_dir: path.join(".git").exists(),
        });
    }
    Ok(out)
}

fn inspect_link(cwd: &Path, name: &str) -> LinkEntry {
    let path = cwd.join(name);
    let display = path.display().to_string();
    match fs::symlink_metadata(&path) {
        Err(_) => LinkEntry {
            name: name.to_owned(),
            path: display,
            kind: "missing".into(),
            target: None,
            resolved: None,
        },
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = fs::read_link(&path).ok().map(|t| t.display().to_string());
            let resolved = fs::canonicalize(&path)
                .ok()
                .map(|p| p.display().to_string());
            LinkEntry {
                name: name.to_owned(),
                path: display,
                kind: "symlink".into(),
                target,
                resolved,
            }
        }
        Ok(meta) if meta.is_dir() => LinkEntry {
            name: name.to_owned(),
            path: display,
            kind: "directory".into(),
            target: None,
            resolved: fs::canonicalize(&path)
                .ok()
                .map(|p| p.display().to_string()),
        },
        Ok(_) => LinkEntry {
            name: name.to_owned(),
            path: display,
            kind: "file".into(),
            target: None,
            resolved: fs::canonicalize(&path)
                .ok()
                .map(|p| p.display().to_string()),
        },
    }
}
