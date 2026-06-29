use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::ClabIndex;

pub(crate) fn summary(index: &ClabIndex, status: &str) -> Value {
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

pub(crate) fn window(text: &str, line: usize, radius: usize) -> String {
    let start = line.saturating_sub(radius).max(1);
    let end = line + radius;
    text.lines()
        .enumerate()
        .filter(|(idx, _)| *idx + 1 >= start && *idx < end)
        .map(|(idx, value)| format!("{}:{}", idx + 1, value))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn required_str(args: &Value, key: &str) -> Result<String> {
    opt_str(args, key).ok_or_else(|| anyhow!("{key} is required"))
}

pub(crate) fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

pub(crate) fn opt_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|v| v as usize)
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
