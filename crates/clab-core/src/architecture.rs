use serde_json::{json, Value};

use crate::types::ClabIndex;

#[cfg(test)]
use crate::types::{FileEntry, GitProjectState, SymbolEntry};

pub(crate) fn architecture_map(
    index: &ClabIndex,
    max_components: usize,
    max_hotspots: usize,
    include_summary: bool,
) -> Value {
    let components = architecture_components(index, max_components);
    let entrypoints = architecture_entrypoints(index);
    let hotspots = architecture_hotspots(index, max_hotspots);
    let important_files = architecture_important_files(index);
    let summary = if include_summary {
        architecture_summary(index, &components)
    } else {
        String::new()
    };
    json!({
        "project": index.project,
        "root_path": index.root_path,
        "summary": summary,
        "components": components,
        "entrypoints": entrypoints,
        "hotspots": hotspots,
        "important_files": important_files,
        "confidence": architecture_confidence(index),
        "derived_from": {
            "indexed_files": index.files.len(),
            "indexed_symbols": index.symbols.len()
        }
    })
}

fn architecture_components(index: &ClabIndex, max_components: usize) -> Vec<Value> {
    let mut buckets = std::collections::BTreeMap::<String, (String, usize, usize)>::new();
    for file in &index.files {
        let (name, kind) = component_identity(&file.path);
        let entry = buckets.entry(name).or_insert((kind, 0, 0));
        entry.1 += 1;
    }
    for sym in &index.symbols {
        let (name, _) = component_identity(&sym.file);
        if let Some(entry) = buckets.get_mut(&name) {
            entry.2 += 1;
        }
    }
    let mut components = buckets
        .into_iter()
        .filter(|(_, (_, files, symbols))| *files > 1 || *symbols > 0)
        .map(|(name, (kind, files, symbols))| {
            json!({
                "name": name,
                "kind": kind,
                "files": files,
                "symbols": symbols,
                "role": component_role_hint(&name)
            })
        })
        .collect::<Vec<_>>();
    components.sort_by(|a, b| {
        let a_symbols = a.get("symbols").and_then(Value::as_u64).unwrap_or(0);
        let b_symbols = b.get("symbols").and_then(Value::as_u64).unwrap_or(0);
        b_symbols.cmp(&a_symbols)
    });
    components.truncate(max_components);
    components
}

fn component_identity(path: &str) -> (String, String) {
    let mut parts = path.split('/').collect::<Vec<_>>();
    if parts.len() >= 2 && parts[0] == "crates" {
        return (format!("{}/{}", parts[0], parts[1]), "crate".to_string());
    }
    if parts.len() >= 2 && parts[0] == "src" {
        return (format!("{}/{}", parts[0], parts[1]), "module".to_string());
    }
    let first = parts.drain(..1).next().unwrap_or(path);
    (first.to_string(), "group".to_string())
}

fn component_role_hint(name: &str) -> String {
    if name.contains("daemon") {
        "HTTP and MCP surface".to_string()
    } else if name.contains("core") {
        "indexing and knowledge storage".to_string()
    } else if name.contains("launchd") {
        "service bootstrap".to_string()
    } else {
        "code component".to_string()
    }
}

fn architecture_entrypoints(index: &ClabIndex) -> Vec<Value> {
    let mut out = Vec::new();
    for file in &index.files {
        let basename = file.path.rsplit('/').next().unwrap_or(&file.path);
        if basename == "main.rs" || basename == "lib.rs" {
            if let Some(sym) = index.symbols.iter().find(|sym| {
                sym.file == file.path
                    && (sym.name == "main" || sym.name == "serve" || sym.name == "run")
            }) {
                out.push(json!({
                    "file": file.path,
                    "symbol": sym.name,
                    "kind": sym.label
                }));
            }
        }
    }
    out
}

fn architecture_hotspots(index: &ClabIndex, max_hotspots: usize) -> Vec<Value> {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for sym in &index.symbols {
        *counts.entry(sym.file.clone()).or_default() += 1;
    }
    let mut rows = counts
        .into_iter()
        .map(|(file, symbols)| {
            json!({
                "file": file,
                "reason": "high symbol density",
                "symbols": symbols
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        let a_symbols = a.get("symbols").and_then(Value::as_u64).unwrap_or(0);
        let b_symbols = b.get("symbols").and_then(Value::as_u64).unwrap_or(0);
        b_symbols.cmp(&a_symbols)
    });
    rows.truncate(max_hotspots);
    rows
}

fn architecture_important_files(index: &ClabIndex) -> Vec<String> {
    let mut files = index
        .files
        .iter()
        .filter_map(|file| {
            let basename = file.path.rsplit('/').next().unwrap_or(&file.path);
            if matches!(basename, "lib.rs" | "main.rs" | "Cargo.toml" | "Makefile") {
                Some(file.path.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    files
}

fn architecture_summary(index: &ClabIndex, components: &[Value]) -> String {
    let top = components
        .iter()
        .take(2)
        .filter_map(|value| value.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if top.is_empty() {
        format!(
            "Indexed project with {} files and {} symbols.",
            index.files.len(),
            index.symbols.len()
        )
    } else {
        format!(
            "Indexed project centered on {} with {} files and {} symbols.",
            top.join(" and "),
            index.files.len(),
            index.symbols.len()
        )
    }
}

fn architecture_confidence(index: &ClabIndex) -> f64 {
    if index.files.is_empty() {
        0.1
    } else if index.symbols.is_empty() {
        0.35
    } else if index.symbols.len() < 25 {
        0.55
    } else {
        0.75
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn architecture_map_reports_components_and_hotspots() {
        let index = ClabIndex {
            project: "Users-steve-clab".to_string(),
            root_path: PathBuf::from("/tmp/clab"),
            status: "indexed".to_string(),
            indexed_at: 1,
            git: GitProjectState {
                is_git: true,
                root_exists: true,
                branch: Some("main".to_string()),
                head_sha: Some("abc".to_string()),
            },
            files: vec![
                FileEntry {
                    path: "crates/clab-core/src/lib.rs".to_string(),
                    bytes: 10,
                    lines: 3,
                    text: "pub fn alpha() {}\npub fn beta() {}\n".to_string(),
                },
                FileEntry {
                    path: "crates/clab-daemon/src/lib.rs".to_string(),
                    bytes: 10,
                    lines: 3,
                    text: "pub fn serve() {}\npub fn route() {}\n".to_string(),
                },
            ],
            symbols: vec![
                SymbolEntry {
                    name: "alpha".to_string(),
                    label: "Function".to_string(),
                    file: "crates/clab-core/src/lib.rs".to_string(),
                    line: 1,
                },
                SymbolEntry {
                    name: "beta".to_string(),
                    label: "Function".to_string(),
                    file: "crates/clab-core/src/lib.rs".to_string(),
                    line: 2,
                },
                SymbolEntry {
                    name: "serve".to_string(),
                    label: "Function".to_string(),
                    file: "crates/clab-daemon/src/lib.rs".to_string(),
                    line: 1,
                },
                SymbolEntry {
                    name: "route".to_string(),
                    label: "Function".to_string(),
                    file: "crates/clab-daemon/src/lib.rs".to_string(),
                    line: 2,
                },
            ],
        };
        let map = architecture_map(&index, 10, 10, true);
        assert_eq!(
            map.get("project").and_then(Value::as_str),
            Some("Users-steve-clab")
        );
        assert_eq!(
            map.get("components")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
        assert!(map
            .get("summary")
            .and_then(Value::as_str)
            .is_some_and(|s| s.contains("crates/clab-core")));
        assert!(map
            .get("hotspots")
            .and_then(Value::as_array)
            .is_some_and(|rows| !rows.is_empty()));
    }
}
