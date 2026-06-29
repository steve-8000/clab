use anyhow::Result;
use serde_json::{json, Value};
use std::{env, fs, path::PathBuf};

const DEFAULT_SKILL_SEARCH_LIMIT: usize = 5;

fn skills_dir() -> PathBuf {
    env::var_os("CLAB_SKILLS_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".clab/skills")))
        .unwrap_or_else(|| PathBuf::from(".clab/skills"))
}

fn skill_path(name: &str) -> PathBuf {
    skills_dir().join(format!("{name}.md"))
}

pub(crate) fn skill_search(args: Value) -> Result<Value> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_SKILL_SEARCH_LIMIT as u64) as usize;
    let mut results = Vec::new();
    for skill in read_skills()? {
        let haystack =
            format!("{} {} {}", skill["name"], skill["summary"], skill["body"]).to_lowercase();
        if query.is_empty() || haystack.contains(&query) {
            results.push(json!({
                "name": skill["name"],
                "summary": skill["summary"],
                "tags": skill["tags"],
                "version": skill["version"],
                "score": if query.is_empty() { 1 } else { 3 },
                "source": "skill"
            }));
        }
    }
    results.sort_by(|left, right| {
        right
            .get("score")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .cmp(&left.get("score").and_then(Value::as_i64).unwrap_or(0))
    });
    results.truncate(limit);
    Ok(Value::Array(results))
}

pub(crate) fn skill_get(args: Value) -> Result<Value> {
    let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
    let summary_only = args
        .get("summary_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let max_chars = args
        .get("max_chars")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    for skill in read_skills()? {
        if skill.get("name").and_then(Value::as_str) == Some(name) {
            return Ok(compact_skill(skill, summary_only, max_chars));
        }
    }
    Ok(json!({"error": format!("Skill not found: {name}")}))
}

pub(crate) fn compact_skill(
    mut skill: Value,
    summary_only: bool,
    max_chars: Option<usize>,
) -> Value {
    if let Some(obj) = skill.as_object_mut() {
        if summary_only {
            obj.remove("body");
            return skill;
        }
        if let Some(max_chars) = max_chars {
            if let Some(body) = obj.get("body").and_then(Value::as_str) {
                let truncated = truncate_chars(body, max_chars);
                let was_truncated = truncated.len() < body.len();
                obj.insert("body".to_string(), json!(truncated));
                obj.insert("truncated".to_string(), json!(was_truncated));
            }
        }
    }
    skill
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

pub(crate) fn skill_upsert(args: Value) -> Result<Value> {
    let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
    let summary = args
        .get("summary")
        .or_else(|| args.get("description"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let body = args.get("body").and_then(Value::as_str).unwrap_or_default();
    fs::create_dir_all(skills_dir())?;
    fs::write(
        skill_path(name),
        format!("---\nsummary: {summary}\nversion: 1\n---\n\n{body}\n"),
    )?;
    Ok(json!({"name": name, "version": 1, "created": true}))
}

pub(crate) fn skill_delete(args: Value) -> Result<Value> {
    let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
    let path = skill_path(name);
    let deleted = path.exists();
    if deleted {
        fs::remove_file(path)?;
    }
    Ok(json!({"name": name, "deleted": deleted}))
}
fn read_skills() -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let dir = skills_dir();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&path) {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let summary = text
                .lines()
                .find_map(|line| line.strip_prefix("summary:"))
                .map(str::trim)
                .unwrap_or("");
            out.push(
                json!({"name": name, "summary": summary, "tags": [], "version": 1, "body": text}),
            );
        }
    }
    Ok(out)
}
