use anyhow::Result;
use std::{fs, path::Path};

use crate::types::{FileEntry, SymbolEntry};

pub fn project_name(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches('/')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

pub(crate) fn scan_files(root: &Path, path: &Path, out: &mut Vec<FileEntry>) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if matches!(
            name,
            ".git" | "target" | ".clab" | ".ai-bridge" | ".venv" | "node_modules" | "dist"
        ) {
            return Ok(());
        }
        for entry in fs::read_dir(path)? {
            scan_files(root, &entry?.path(), out)?;
        }
    } else if meta.is_file() && meta.len() <= 1_000_000 {
        if let Ok(text) = fs::read_to_string(path) {
            let rel = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            let lines = text.lines().count();
            out.push(FileEntry {
                path: rel,
                bytes: meta.len(),
                lines,
                text,
            });
        }
    }
    Ok(())
}

pub(crate) fn extract_symbols(file: &FileEntry) -> Vec<SymbolEntry> {
    file.text
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let trimmed = line.trim_start();
            let (label, rest) = if let Some(rest) = trimmed.strip_prefix("fn ") {
                ("Function", rest)
            } else if let Some(rest) = trimmed.strip_prefix("pub fn ") {
                ("Function", rest)
            } else if let Some(rest) = trimmed.strip_prefix("def ") {
                ("Function", rest)
            } else if let Some(rest) = trimmed.strip_prefix("class ") {
                ("Class", rest)
            } else if let Some(rest) = trimmed.strip_prefix("struct ") {
                ("Class", rest)
            } else if let Some(rest) = trimmed.strip_prefix("pub struct ") {
                ("Class", rest)
            } else {
                return None;
            };
            let name = rest
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()
                .unwrap_or_default();
            if name.is_empty() {
                return None;
            }
            Some(SymbolEntry {
                name: name.to_string(),
                label: label.to_string(),
                file: file.path.clone(),
                line: idx + 1,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn stable_project_name() {
        let path = PathBuf::from("/tmp/example repo");
        assert_eq!(project_name(&path), project_name(&path));
        assert_eq!(project_name(&path), "tmp-example-repo");
    }
}
