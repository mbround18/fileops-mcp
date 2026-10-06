//! Read-only workspace inventory for sibling checkouts and local symlink wiring.
//!
//! This replaces ad-hoc shell probes like `ls ../Prefix-*` and repeated `readlink/stat`
//! loops with one bounded call that returns structured data plus concise text.

use std::path::PathBuf;

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

    let inventory = workspaceops::inventory(
        &cwd_abs,
        &prefix,
        request.base_dir.as_deref(),
        &request.inspect_links,
    )
    .map_err(|e| Error::InvalidRequest(format!("workspace inventory failed: {e}")))?;

    let siblings: Vec<SiblingEntry> = inventory
        .siblings
        .into_iter()
        .map(|s| SiblingEntry {
            name: s.name,
            path: s.path,
            has_git_dir: s.has_git_dir,
        })
        .collect();
    let links: Vec<LinkEntry> = inventory
        .links
        .into_iter()
        .map(|l| LinkEntry {
            name: l.name,
            path: l.path,
            kind: l.kind,
            target: l.target,
            resolved: l.resolved,
        })
        .collect();

    let mut text = String::new();
    text.push_str(&format!(
        "base: {}\nprefix: {}\n\n",
        inventory.base_dir, prefix
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
        base_dir: inventory.base_dir,
        prefix,
        siblings,
        links,
        text,
    })
}
