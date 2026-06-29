use std::{path::Path, process::Command};

use crate::types::GitProjectState;

pub(crate) fn git_state(root: &Path) -> GitProjectState {
    GitProjectState {
        is_git: root.join(".git").exists(),
        root_exists: root.exists(),
        branch: git_out(root, &["rev-parse", "--abbrev-ref", "HEAD"]),
        head_sha: git_out(root, &["rev-parse", "HEAD"]),
    }
}

fn git_out(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitStatusEntry {
    pub(crate) kind: String,
    pub(crate) path: String,
}

pub(crate) fn git_status_entries(root: &Path) -> Vec<GitStatusEntry> {
    let Some(output) = git_out(root, &["status", "--porcelain=v1"]) else {
        return Vec::new();
    };
    parse_git_status_entries(&output)
}

fn parse_git_status_entries(output: &str) -> Vec<GitStatusEntry> {
    output
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            if let Some(path) = line.strip_prefix("?? ") {
                return Some(GitStatusEntry {
                    kind: "untracked".to_string(),
                    path: path.trim().to_string(),
                });
            }
            let status = line.get(0..2)?.trim();
            let path = line.get(2..)?.trim_start();
            if path.is_empty() {
                return None;
            }
            let kind = if status.contains('D') {
                "deleted"
            } else if status.contains('R') {
                "renamed"
            } else if status.contains('M') {
                "modified"
            } else if status.contains('A') {
                "added"
            } else {
                "changed"
            };
            Some(GitStatusEntry {
                kind: kind.to_string(),
                path: path.to_string(),
            })
        })
        .collect()
}

pub(crate) fn maybe_limit_paths(
    paths: Vec<String>,
    include_files: bool,
    limit: usize,
) -> Vec<String> {
    if !include_files {
        return Vec::new();
    }
    let mut paths = paths;
    paths.sort();
    paths.dedup();
    paths.truncate(limit);
    paths
}

pub(crate) fn summarize_change_state(
    head_changed: bool,
    branch_changed: bool,
    dirty: bool,
    file_change_total: usize,
) -> String {
    let mut parts = Vec::new();
    if head_changed {
        parts.push("Git HEAD changed");
    }
    if branch_changed {
        parts.push("branch changed");
    }
    if dirty {
        if file_change_total > 0 {
            parts.push("working tree is dirty");
        } else {
            parts.push("working tree changed");
        }
    }
    if file_change_total > 0 {
        format!(
            "{} ({} file changes).",
            parts.join(" and "),
            file_change_total
        )
    } else {
        format!("{}.", parts.join(" and "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_git_status_entries_classifies_paths() {
        let rows = parse_git_status_entries(" M src/lib.rs\nD  old.rs\n?? new.rs\nA  added.rs\n");
        assert_eq!(
            rows,
            vec![
                GitStatusEntry {
                    kind: "modified".to_string(),
                    path: "src/lib.rs".to_string()
                },
                GitStatusEntry {
                    kind: "deleted".to_string(),
                    path: "old.rs".to_string()
                },
                GitStatusEntry {
                    kind: "untracked".to_string(),
                    path: "new.rs".to_string()
                },
                GitStatusEntry {
                    kind: "added".to_string(),
                    path: "added.rs".to_string()
                },
            ]
        );
    }
}
