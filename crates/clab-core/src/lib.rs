use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

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

#[derive(Debug, Clone)]
pub struct ClabStore {
    root: PathBuf,
}

impl ClabStore {
    pub fn with_root(root: PathBuf) -> Result<Self> {
        let root = root.join("indexes");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn from_env() -> Result<Self> {
        let root = std::env::var_os("CLAB_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".clab")))
            .unwrap_or_else(|| PathBuf::from(".clab"));
        Self::with_root(root)
    }

    pub fn dispatch(&self, tool: &str, args: Value) -> Result<Value> {
        match tool {
            "index_repository" => self.index_repository(args),
            "index_status" => self.index_status(args),
            "list_projects" => self.list_projects(),
            "delete_project" => self.delete_project(args),
            "detect_changes" => self.detect_changes(args),
            "search_graph" => self.search_graph(args),
            "search_code" => self.search_code(args),
            "get_code_snippet" => self.get_code_snippet(args),
            "trace_path" => self.trace_path(args),
            "get_architecture" => self.get_architecture(args),
            "query_graph" => self.query_graph(args),
            "get_graph_schema" => Ok(json!({
                "nodes": ["Project", "File", "Symbol"],
                "edges": ["CONTAINS", "DEFINED_IN", "MENTIONS"]
            })),
            other => Err(anyhow!("unsupported tool: {other}")),
        }
    }

    pub fn index_repository(&self, args: Value) -> Result<Value> {
        let raw = required_str(&args, "repo_path")?;
        let root = fs::canonicalize(raw)?;
        let project = project_name(&root);
        let mut files = Vec::new();
        scan_files(&root, &root, &mut files)?;
        let symbols = files.iter().flat_map(extract_symbols).collect::<Vec<_>>();
        let git = git_state(&root);
        let index = ClabIndex {
            project: project.clone(),
            root_path: root,
            status: "indexed".to_string(),
            files,
            symbols,
            indexed_at: now_secs(),
            git,
        };
        self.write_index(&index)?;
        Ok(summary(&index, "indexed"))
    }

    pub fn index_status(&self, args: Value) -> Result<Value> {
        let project = required_str(&args, "project")?;
        let index = self.read_index(&project)?;
        Ok(summary(&index, "ready"))
    }

    pub fn list_projects(&self) -> Result<Value> {
        let mut projects = Vec::new();
        for index in self.read_all()? {
            projects.push(summary(&index, &index.status));
        }
        Ok(json!({"projects": projects}))
    }

    pub fn delete_project(&self, args: Value) -> Result<Value> {
        let project = required_str(&args, "project")?;
        let path = self.index_path(&project);
        let deleted = path.exists();
        if deleted {
            fs::remove_file(path)?;
        }
        Ok(json!({"ok": true, "deleted": deleted}))
    }

    pub fn detect_changes(&self, args: Value) -> Result<Value> {
        let project = required_str(&args, "project")?;
        let index = self.read_index(&project)?;
        let current = git_state(&index.root_path);
        let mut changed = Vec::new();
        if current.head_sha != index.git.head_sha || current.branch != index.git.branch {
            changed.push("git_state".to_string());
        }
        if git_dirty(&index.root_path) {
            changed.push("working_tree".to_string());
        }
        Ok(json!({"changed_files": changed, "changed_count": changed.len()}))
    }

    pub fn search_graph(&self, args: Value) -> Result<Value> {
        let query = opt_str(&args, "query").unwrap_or_default().to_lowercase();
        let limit = opt_usize(&args, "limit").unwrap_or(20);
        let project = opt_str(&args, "project");
        let mut results = Vec::new();
        for index in self.filtered(project.as_deref())? {
            for sym in &index.symbols {
                if query.is_empty()
                    || sym.name.to_lowercase().contains(&query)
                    || sym.file.to_lowercase().contains(&query)
                {
                    results.push(json!({
                        "project": index.project,
                        "name": sym.name,
                        "label": sym.label,
                        "file": sym.file,
                        "line": sym.line,
                        "start_line": sym.line,
                    }));
                    if results.len() >= limit {
                        return Ok(json!({"results": results}));
                    }
                }
            }
        }
        Ok(json!({"results": results}))
    }

    pub fn search_code(&self, args: Value) -> Result<Value> {
        let pattern = required_str(&args, "pattern")?.to_lowercase();
        let limit = opt_usize(&args, "limit").unwrap_or(20);
        let project = opt_str(&args, "project");
        let mut results = Vec::new();
        for index in self.filtered(project.as_deref())? {
            for file in &index.files {
                for (line_idx, line) in file.text.lines().enumerate() {
                    if line.to_lowercase().contains(&pattern) {
                        results.push(json!({
                            "project": index.project,
                            "file": file.path,
                            "line": line_idx + 1,
                            "start_line": line_idx + 1,
                            "snippet": line,
                        }));
                        if results.len() >= limit {
                            return Ok(json!({"results": results}));
                        }
                    }
                }
            }
        }
        Ok(json!({"results": results}))
    }

    pub fn get_code_snippet(&self, args: Value) -> Result<Value> {
        let project = opt_str(&args, "project");
        let qn = opt_str(&args, "qualified_name")
            .or_else(|| opt_str(&args, "name"))
            .unwrap_or_default();
        for index in self.filtered(project.as_deref())? {
            for sym in &index.symbols {
                if sym.name == qn || qn.ends_with(&sym.name) {
                    if let Some(file) = index.files.iter().find(|f| f.path == sym.file) {
                        return Ok(json!({
                            "project": index.project,
                            "file": file.path,
                            "start_line": sym.line,
                            "snippet": window(&file.text, sym.line, 12),
                        }));
                    }
                }
            }
        }
        Err(anyhow!("symbol not found"))
    }

    pub fn trace_path(&self, args: Value) -> Result<Value> {
        let name = required_str(&args, "function_name")?;
        let found = self.search_graph(
            json!({"query": name, "project": opt_str(&args, "project"), "limit": 50}),
        )?;
        Ok(json!({"function_name": name, "nodes": found["results"].clone(), "edges": []}))
    }

    pub fn get_architecture(&self, args: Value) -> Result<Value> {
        let project = opt_str(&args, "project");
        let indexes = self.filtered(project.as_deref())?;
        let projects = indexes
            .iter()
            .map(|idx| {
                json!({
                    "project": idx.project,
                    "root_path": idx.root_path,
                    "files": idx.files.len(),
                    "symbols": idx.symbols.len(),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"projects": projects}))
    }

    pub fn query_graph(&self, args: Value) -> Result<Value> {
        let limit = opt_usize(&args, "limit").unwrap_or(100);
        let rows = self
            .read_all()?
            .into_iter()
            .take(limit)
            .map(|idx| {
                json!({
                    "project": idx.project,
                    "files": idx.files.len(),
                    "symbols": idx.symbols.len(),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"rows": rows}))
    }

    fn filtered(&self, project: Option<&str>) -> Result<Vec<ClabIndex>> {
        if let Some(project) = project {
            return Ok(vec![self.read_index(project)?]);
        }
        self.read_all()
    }

    fn read_all(&self) -> Result<Vec<ClabIndex>> {
        let mut indexes = Vec::new();
        if !self.root.exists() {
            return Ok(indexes);
        }
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                indexes.push(serde_json::from_slice(&fs::read(path)?)?);
            }
        }
        Ok(indexes)
    }

    fn read_index(&self, project: &str) -> Result<ClabIndex> {
        Ok(serde_json::from_slice(&fs::read(
            self.index_path(project),
        )?)?)
    }

    fn write_index(&self, index: &ClabIndex) -> Result<()> {
        fs::write(
            self.index_path(&index.project),
            serde_json::to_vec_pretty(index)?,
        )?;
        Ok(())
    }

    fn index_path(&self, project: &str) -> PathBuf {
        self.root.join(format!("{project}.json"))
    }
}

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

fn scan_files(root: &Path, path: &Path, out: &mut Vec<FileEntry>) -> Result<()> {
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

fn extract_symbols(file: &FileEntry) -> Vec<SymbolEntry> {
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

fn git_state(root: &Path) -> GitProjectState {
    GitProjectState {
        is_git: root.join(".git").exists(),
        root_exists: root.exists(),
        branch: git_out(root, &["rev-parse", "--abbrev-ref", "HEAD"]),
        head_sha: git_out(root, &["rev-parse", "HEAD"]),
    }
}

fn git_dirty(root: &Path) -> bool {
    git_out(root, &["status", "--porcelain=v1"]).is_some_and(|s| !s.trim().is_empty())
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

fn summary(index: &ClabIndex, status: &str) -> Value {
    json!({
        "project": index.project,
        "root_path": index.root_path,
        "status": status,
        "files": index.files.len(),
        "symbols": index.symbols.len(),
        "bytes": index.files.iter().map(|f| f.bytes).sum::<u64>(),
        "git": index.git,
    })
}

fn window(text: &str, line: usize, radius: usize) -> String {
    let start = line.saturating_sub(radius).max(1);
    let end = line + radius;
    text.lines()
        .enumerate()
        .filter(|(idx, _)| *idx + 1 >= start && *idx < end)
        .map(|(idx, value)| format!("{}:{}", idx + 1, value))
        .collect::<Vec<_>>()
        .join("\n")
}

fn required_str(args: &Value, key: &str) -> Result<String> {
    opt_str(args, key).ok_or_else(|| anyhow!("{key} is required"))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn opt_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|v| v as usize)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_project_name() {
        let path = PathBuf::from("/tmp/example repo");
        assert_eq!(project_name(&path), project_name(&path));
        assert_eq!(project_name(&path), "tmp-example-repo");
    }
}
