use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitProjectState {
    pub is_git: bool,
    pub root_exists: bool,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoIndexStatus {
    pub enabled: bool,
    pub running: bool,
    pub tracked_projects: usize,
    pub queued_projects: usize,
    pub indexing_projects: Vec<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClabIndex {
    pub project: String,
    pub root_path: PathBuf,
    pub status: String,
    pub files: Vec<FileEntry>,
    pub symbols: Vec<SymbolEntry>,
    pub indexed_at: u64,
    pub git: GitProjectState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub bytes: u64,
    pub lines: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolEntry {
    pub name: String,
    pub label: String,
    pub file: String,
    pub line: usize,
}
