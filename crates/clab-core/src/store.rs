use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{fs, path::PathBuf};

use crate::{
    architecture::architecture_map,
    git::{git_state, git_status_entries, maybe_limit_paths, summarize_change_state},
    scanner::{extract_symbols, project_name, scan_files},
    types::ClabIndex,
    util::{now_secs, opt_str, opt_usize, required_str, summary, window},
};

const DEFAULT_CHANGE_FILE_LIMIT: usize = 50;
const DEFAULT_SEARCH_LIMIT: usize = 8;
const DEFAULT_SNIPPET_RADIUS: usize = 6;
const DEFAULT_ARCHITECTURE_COMPONENTS: usize = 8;
const DEFAULT_ARCHITECTURE_HOTSPOTS: usize = 5;

#[derive(Debug, Clone)]
pub struct ClabStore {
    root: PathBuf,
}

impl ClabStore {
    pub fn with_root(home: PathBuf) -> Result<Self> {
        let root = home.join("indexes");
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
            "get_architecture" => self.get_architecture(args),
            other => Err(anyhow!("unsupported tool: {other}")),
        }
    }

    pub fn index_repository(&self, args: Value) -> Result<Value> {
        let raw = opt_str(&args, "repo_path").unwrap_or_else(|| ".".to_string());
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
        #[derive(Deserialize)]
        struct DetectChangesArgs {
            project: String,
            include_files: Option<bool>,
            limit: Option<usize>,
        }

        let input: DetectChangesArgs = serde_json::from_value(args)?;
        let index = self.read_index(&input.project)?;
        let current = git_state(&index.root_path);
        let include_files = input.include_files.unwrap_or(true);
        let limit = input.limit.unwrap_or(DEFAULT_CHANGE_FILE_LIMIT);
        let entries = git_status_entries(&index.root_path);
        let mut changed_files = Vec::new();
        let mut untracked_files = Vec::new();
        let mut deleted_files = Vec::new();
        for entry in &entries {
            match entry.kind.as_str() {
                "untracked" => untracked_files.push(entry.path.clone()),
                "deleted" => deleted_files.push(entry.path.clone()),
                _ => changed_files.push(entry.path.clone()),
            }
        }
        let head_changed = current.head_sha != index.git.head_sha;
        let branch_changed = current.branch != index.git.branch;
        let dirty = !entries.is_empty();
        let mut stale_reason = Vec::new();
        if head_changed {
            stale_reason.push("head_changed".to_string());
        }
        if branch_changed {
            stale_reason.push("branch_changed".to_string());
        }
        if dirty {
            stale_reason.push("working_tree_dirty".to_string());
        }
        let stale = !stale_reason.is_empty();
        let file_change_total = changed_files.len() + untracked_files.len() + deleted_files.len();
        let recommended_action = if !stale {
            "none"
        } else if head_changed || branch_changed || file_change_total > limit {
            "reindex_full"
        } else {
            "reindex_partial"
        };
        let summary = if !stale {
            "Index is current.".to_string()
        } else {
            summarize_change_state(head_changed, branch_changed, dirty, file_change_total)
        };
        let changed_count = if stale {
            file_change_total.max(stale_reason.len())
        } else {
            0
        };
        Ok(json!({
            "project": index.project,
            "root_path": index.root_path,
            "indexed_head_sha": index.git.head_sha,
            "current_head_sha": current.head_sha,
            "branch": {
                "indexed": index.git.branch,
                "current": current.branch,
                "changed": branch_changed
            },
            "dirty": dirty,
            "head_changed": head_changed,
            "branch_changed": branch_changed,
            "stale": stale,
            "stale_reason": stale_reason,
            "changed_files": maybe_limit_paths(changed_files, include_files, limit),
            "untracked_files": maybe_limit_paths(untracked_files, include_files, limit),
            "deleted_files": maybe_limit_paths(deleted_files, include_files, limit),
            "changed_count": changed_count,
            "recommended_action": recommended_action,
            "summary": summary
        }))
    }

    pub fn search_graph(&self, args: Value) -> Result<Value> {
        let query = opt_str(&args, "query").unwrap_or_default().to_lowercase();
        let limit = opt_usize(&args, "limit").unwrap_or(DEFAULT_SEARCH_LIMIT);
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
        let limit = opt_usize(&args, "limit").unwrap_or(DEFAULT_SEARCH_LIMIT);
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
                            "snippet": window(&file.text, sym.line, DEFAULT_SNIPPET_RADIUS),
                        }));
                    }
                }
            }
        }
        Err(anyhow!("symbol not found"))
    }

    pub fn get_architecture(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct ArchitectureArgs {
            project: Option<String>,
            max_components: Option<usize>,
            max_hotspots: Option<usize>,
            include_summary: Option<bool>,
        }

        let input: ArchitectureArgs = serde_json::from_value(args)?;
        let indexes = self.filtered(input.project.as_deref())?;
        let max_components = input
            .max_components
            .unwrap_or(DEFAULT_ARCHITECTURE_COMPONENTS);
        let max_hotspots = input.max_hotspots.unwrap_or(DEFAULT_ARCHITECTURE_HOTSPOTS);
        let include_summary = input.include_summary.unwrap_or(true);
        let maps = indexes
            .iter()
            .map(|idx| architecture_map(idx, max_components, max_hotspots, include_summary))
            .collect::<Vec<_>>();
        if maps.len() == 1 {
            return Ok(maps.into_iter().next().unwrap_or_else(|| json!({})));
        }
        Ok(json!({ "projects": maps }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileEntry, GitProjectState, SymbolEntry};
    use std::{
        env, fs,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    static CURRENT_DIR_LOCK: Mutex<()> = Mutex::new(());

    fn unique_test_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        env::temp_dir().join(format!("clab-core-{name}-{unique}"))
    }

    fn temp_store(name: &str) -> (ClabStore, PathBuf) {
        let root = unique_test_path(name);
        let store = ClabStore::with_root(root.clone()).unwrap();
        (store, root)
    }

    fn git_state() -> GitProjectState {
        GitProjectState {
            is_git: false,
            root_exists: true,
            branch: None,
            head_sha: None,
        }
    }

    fn indexed_project(root: &PathBuf) -> ClabIndex {
        let mut files = Vec::new();
        let mut symbols = Vec::new();
        for index in 0..12 {
            let file = format!("src/module_{index}.rs");
            files.push(FileEntry {
                path: file.clone(),
                bytes: 16,
                lines: 1,
                text: format!("fn target_{index}() {{ target_call(); }}"),
            });
            symbols.push(SymbolEntry {
                name: format!("target_{index}"),
                label: "fn".to_string(),
                file,
                line: 1,
            });
        }
        ClabIndex {
            project: "demo".to_string(),
            root_path: root.clone(),
            status: "indexed".to_string(),
            files,
            symbols,
            indexed_at: 0,
            git: git_state(),
        }
    }

    #[test]
    fn default_searches_return_eight_results() {
        let (store, root) = temp_store("default-search-limit");
        let index = indexed_project(&root);
        store.write_index(&index).unwrap();

        let graph = store.search_graph(json!({"query": "target"})).unwrap();
        let code = store
            .search_code(json!({"pattern": "target_call"}))
            .unwrap();

        fs::remove_dir_all(root).unwrap();

        assert_eq!(graph["results"].as_array().unwrap().len(), 8);
        assert_eq!(code["results"].as_array().unwrap().len(), 8);
    }

    #[test]
    fn default_architecture_is_compact() {
        let (store, root) = temp_store("default-architecture-limit");
        let index = indexed_project(&root);
        store.write_index(&index).unwrap();

        let architecture = store.get_architecture(json!({"project": "demo"})).unwrap();

        fs::remove_dir_all(root).unwrap();

        assert!(architecture["components"].as_array().unwrap().len() <= 8);
        assert!(architecture["hotspots"].as_array().unwrap().len() <= 5);
    }

    #[test]
    fn index_repository_defaults_to_current_directory() {
        let _guard = CURRENT_DIR_LOCK.lock().unwrap();
        let (store, home) = temp_store("default-index-root");
        let repo = unique_test_path("default-index-root-repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn default_root() {}\n").unwrap();
        let canonical_repo = fs::canonicalize(&repo).unwrap();
        let old_dir = env::current_dir().unwrap();
        env::set_current_dir(&repo).unwrap();

        let summary = store.index_repository(json!({})).unwrap();

        env::set_current_dir(old_dir).unwrap();
        fs::remove_dir_all(&repo).unwrap();
        fs::remove_dir_all(home).unwrap();
        assert_eq!(summary["root_path"], json!(canonical_repo));
        assert_eq!(summary["symbols"], json!(1));
    }

    #[test]
    fn default_symbol_snippet_uses_compact_window() {
        let (store, root) = temp_store("default-snippet-radius");
        let text = (1..=20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let index = ClabIndex {
            project: "demo".to_string(),
            root_path: root.clone(),
            status: "indexed".to_string(),
            files: vec![FileEntry {
                path: "src/lib.rs".to_string(),
                bytes: text.len() as u64,
                lines: 20,
                text,
            }],
            symbols: vec![SymbolEntry {
                name: "target".to_string(),
                label: "fn".to_string(),
                file: "src/lib.rs".to_string(),
                line: 10,
            }],
            indexed_at: 0,
            git: git_state(),
        };
        store.write_index(&index).unwrap();

        let snippet = store
            .get_code_snippet(json!({"project": "demo", "name": "target"}))
            .unwrap();

        fs::remove_dir_all(root).unwrap();

        assert_eq!(snippet["snippet"].as_str().unwrap().lines().count(), 13);
    }
}
