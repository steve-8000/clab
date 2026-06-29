use anyhow::Result;
use axum::{extract::State, Json};
use clab_core::ClabStore;
use serde_json::{json, Value};

use crate::{
    skills::{skill_delete, skill_get, skill_search, skill_upsert},
    AppState,
};

const TOOLS: &[&str] = &[
    "index_repository",
    "list_projects",
    "index_status",
    "detect_changes",
    "get_architecture",
    "search_graph",
    "search_code",
    "get_code_snippet",
    "skill_search",
    "skill_get",
    "put_skill",
    "delete_skill",
];

pub(crate) async fn mcp(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let method = body
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let result = match method {
        "initialize" => {
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"clab","version":"0.1.0"}})
        }
        "ping" => json!({}),
        "tools/list" => {
            json!({"tools": TOOLS.iter().map(|name| {
                json!({
                    "name": name,
                    "description": tool_description(name),
                    "inputSchema": tool_input_schema(name)
                })
            }).collect::<Vec<_>>() })
        }
        "tools/call" => {
            let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match dispatch_mcp_tool(&state.store, name, args) {
                Ok(value) => json!({"content":[{"type":"text","text": value.to_string()}]}),
                Err(err) => {
                    json!({"content":[{"type":"text","text": json!({"error": err.to_string()}).to_string()}],"isError":true})
                }
            }
        }
        _ => json!({}),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

pub(crate) fn dispatch_mcp_tool(store: &ClabStore, name: &str, args: Value) -> Result<Value> {
    match name {
        "skill_search" => skill_search(args),
        "skill_get" => skill_get(args),
        "put_skill" => skill_upsert(args),
        "delete_skill" => skill_delete(args),
        _ => store.dispatch(name, args),
    }
}

pub(crate) fn dispatch_or_error(store: &ClabStore, tool: &str, args: Value) -> Value {
    match dispatch_mcp_tool(store, tool, args) {
        Ok(value) => value,
        Err(err) => json!({"error": err.to_string()}),
    }
}

fn tool_description(name: &str) -> String {
    match name {
        "index_repository" => {
            "Index the current/default repository, or an explicit repository path."
        }
        "search_graph" => "Search indexed symbols by name or file path.",
        "get_code_snippet" => "Return a focused snippet for an indexed symbol.",
        "get_architecture" => {
            "Return a compact architecture summary for one or more indexed projects."
        }
        "search_code" => "Search indexed file contents for a text pattern.",
        "list_projects" => "List indexed projects.",
        "delete_project" => "Delete a project index.",
        "index_status" => "Return status for one indexed project.",
        "detect_changes" => "Compare a project index with current git state.",
        "skill_search" => "Search stored skills.",
        "skill_get" => "Get one stored skill, optionally without the full body.",
        "put_skill" => "Create or replace a stored skill file.",
        "delete_skill" => "Delete a stored skill file.",
        _ => "Clab tool",
    }
    .to_string()
}

pub(crate) fn tool_input_schema(name: &str) -> Value {
    let object = |properties: Value, required: &[&str]| {
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        })
    };
    match name {
        "index_repository" => object(
            json!({
                "repo_path": string_prop("Optional repository path to index; defaults to the current directory."),
                "mode": string_prop("Indexing mode hint, such as fast.")
            }),
            &[],
        ),
        "search_graph" => object(
            json!({
                "project": string_prop("Optional project id to limit search."),
                "query": string_prop("Symbol or file query. Empty returns first matches."),
                "limit": int_prop("Maximum results to return. Defaults to 8.")
            }),
            &[],
        ),
        "get_code_snippet" => object(
            json!({
                "project": string_prop("Optional project id to limit lookup."),
                "qualified_name": string_prop("Symbol qualified name."),
                "name": string_prop("Symbol name fallback.")
            }),
            &[],
        ),
        "get_architecture" => object(
            json!({
                "project": string_prop("Optional project id to return one project only."),
                "max_components": int_prop("Maximum components per project. Defaults to 8."),
                "max_hotspots": int_prop("Maximum hotspots per project. Defaults to 5."),
                "include_summary": bool_prop("Include human-readable summary text.")
            }),
            &[],
        ),
        "search_code" => object(
            json!({
                "project": string_prop("Optional project id to limit search."),
                "pattern": string_prop("Case-insensitive text pattern."),
                "limit": int_prop("Maximum results to return. Defaults to 8.")
            }),
            &["pattern"],
        ),
        "list_projects" => object(json!({}), &[]),
        "delete_project" | "index_status" => {
            object(json!({"project": string_prop("Project id.")}), &["project"])
        }
        "detect_changes" => object(
            json!({
                "project": string_prop("Project id."),
                "include_files": bool_prop("Include changed file lists."),
                "limit": int_prop("Maximum file paths to include. Defaults to 50.")
            }),
            &["project"],
        ),
        "skill_search" => object(
            json!({
                "query": string_prop("Skill search query. Empty returns top entries."),
                "limit": int_prop("Maximum results to return. Defaults to 5.")
            }),
            &[],
        ),
        "skill_get" => object(
            json!({
                "name": string_prop("Skill name."),
                "summary_only": bool_prop("Return metadata without body."),
                "max_chars": int_prop("Maximum body characters to return.")
            }),
            &["name"],
        ),
        "put_skill" => object(
            json!({
                "name": string_prop("Skill name."),
                "summary": string_prop("Short skill summary."),
                "description": string_prop("Alias for summary."),
                "body": string_prop("Skill body markdown.")
            }),
            &["name"],
        ),
        "delete_skill" => object(json!({"name": string_prop("Skill name.")}), &["name"]),
        _ => object(json!({}), &[]),
    }
}

fn string_prop(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn int_prop(description: &str) -> Value {
    json!({"type": "integer", "minimum": 0, "description": description})
}

fn bool_prop(description: &str) -> Value {
    json!({"type": "boolean", "description": description})
}
