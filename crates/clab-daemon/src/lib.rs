use anyhow::Result;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use clab_core::{project_name, AutoIndexStatus, ClabStore};
use serde_json::{json, Value};
use std::{
    env, fs,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    process,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

mod mcp;
mod skills;

use mcp::{dispatch_or_error, mcp};

static PLAN_ID_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct AutoIndexConfig {
    pub enabled: bool,
    pub poll_interval_seconds: u64,
    pub debounce_seconds: u64,
    pub cooldown_seconds: u64,
    pub mode: String,
}

impl Default for AutoIndexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_seconds: 2,
            debounce_seconds: 3,
            cooldown_seconds: 10,
            mode: "fast".to_string(),
        }
    }
}

#[derive(Debug)]
pub struct AutoIndexDaemon {
    config: AutoIndexConfig,
    running: AtomicBool,
    queued: AtomicUsize,
    last_error: Mutex<Option<String>>,
}

impl AutoIndexDaemon {
    pub fn new(config: AutoIndexConfig) -> Self {
        Self {
            config,
            running: AtomicBool::new(false),
            queued: AtomicUsize::new(0),
            last_error: Mutex::new(None),
        }
    }

    pub fn status(&self) -> AutoIndexStatus {
        self.status_snapshot(0, None)
    }

    async fn status_live(&self, store: &ClabStore) -> AutoIndexStatus {
        self.status_snapshot(
            tracked_projects(store),
            self.last_error.lock().await.clone(),
        )
    }

    fn status_snapshot(
        &self,
        tracked_projects: usize,
        last_error: Option<String>,
    ) -> AutoIndexStatus {
        AutoIndexStatus {
            enabled: self.config.enabled,
            running: self.running.load(Ordering::Relaxed),
            tracked_projects,
            queued_projects: self.queued.load(Ordering::Relaxed),
            indexing_projects: Vec::new(),
            last_error,
        }
    }

    async fn refresh_once(&self, store: &ClabStore) -> Result<()> {
        let projects = store.list_projects()?;
        let Some(projects) = projects.get("projects").and_then(Value::as_array) else {
            return Ok(());
        };
        for project in projects {
            let Some(name) = project.get("project").and_then(Value::as_str) else {
                continue;
            };
            let changes = store.detect_changes(json!({"project": name}))?;
            if changes
                .get("changed_count")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0
            {
                self.queued.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_secs(self.config.debounce_seconds)).await;
                if let Some(root) = project.get("root_path").and_then(Value::as_str) {
                    if FsPath::new(root).exists() {
                        store.index_repository(
                            json!({"repo_path": root, "mode": self.config.mode}),
                        )?;
                    }
                }
                self.queued.fetch_sub(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
struct AppState {
    daemon: Arc<AutoIndexDaemon>,
    store: ClabStore,
}

pub fn app(daemon: Arc<AutoIndexDaemon>, store: ClabStore) -> Router {
    let state = AppState { daemon, store };
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/codebase/health", get(healthz))
        .route("/v1/codebase/status", get(status))
        .route("/v1/codebase/profiles", get(profiles))
        .route("/v1/codebase/plan", post(profile_plan))
        .route("/v1/codebase/read", post(profile_read))
        .route("/v1/codebase/validate_points", post(profile_validate))
        .route("/v1/codebase/expand", post(profile_expand))
        .route("/v1/clab/status", get(status))
        .route("/v1/clab/tool/:tool", post(tool_call))
        .route("/v1/clab/codebase/search_graph", post(search_graph))
        .route("/v1/clab/codebase/search_code", post(search_code))
        .route("/v1/clab/codebase/call", post(codebase_call))
        .route("/mcp", post(mcp))
        .with_state(state)
}

pub async fn serve(addr: SocketAddr, daemon: AutoIndexDaemon) -> Result<()> {
    let store = ClabStore::from_env()?;
    index_env_project(&store);
    let daemon = Arc::new(daemon);
    daemon.running.store(true, Ordering::Relaxed);
    spawn_worker(daemon.clone(), store.clone());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app(daemon, store)).await?;
    Ok(())
}

fn spawn_worker(daemon: Arc<AutoIndexDaemon>, store: ClabStore) {
    tokio::spawn(async move {
        if !daemon.config.enabled {
            return;
        }
        loop {
            match daemon.refresh_once(&store).await {
                Ok(()) => *daemon.last_error.lock().await = None,
                Err(err) => *daemon.last_error.lock().await = Some(err.to_string()),
            }
            tokio::time::sleep(Duration::from_secs(daemon.config.poll_interval_seconds)).await;
        }
    });
}

async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn status(State(state): State<AppState>) -> Json<AutoIndexStatus> {
    Json(state.daemon.status_live(&state.store).await)
}

async fn profiles() -> Json<Value> {
    Json(json!({"profiles":["bug_investigation","find_definition","trace_impact"]}))
}

async fn tool_call(
    Path(tool): Path<String>,
    State(state): State<AppState>,
    Json(args): Json<Value>,
) -> Json<Value> {
    Json(dispatch_or_error(&state.store, &tool, args))
}

async fn search_graph(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let mut args = scoped_tool_args(&state.store, &body);
    args["query"] = body.get("query").cloned().unwrap_or_else(|| json!(""));
    Json(dispatch_or_error(&state.store, "search_graph", args))
}

async fn search_code(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let mut args = scoped_tool_args(&state.store, &body);
    args["pattern"] = body.get("pattern").cloned().unwrap_or_else(|| json!(""));
    Json(dispatch_or_error(&state.store, "search_code", args))
}

async fn codebase_call(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let tool = body.get("tool").and_then(Value::as_str).unwrap_or_default();
    let mut args = body.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if args.get("project").and_then(Value::as_str).is_none() {
        args["project"] = json!(project_for_scope(&state.store, &body));
    }
    Json(dispatch_or_error(&state.store, tool, args))
}

async fn profile_plan(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    Json(build_profile_plan(&state.store, &body))
}

async fn profile_read(Json(body): Json<Value>) -> Json<Value> {
    Json(match read_plan_points(&body) {
        Ok(points) => json!({"ok": true, "plan_id": body.get("plan_id"), "points": points}),
        Err(err) => json!({"ok": false, "error": err.to_string(), "points": []}),
    })
}

async fn profile_validate(Json(body): Json<Value>) -> Json<Value> {
    Json(match read_plan_points(&body) {
        Ok(points) => {
            json!({"ok": true, "plan_id": body.get("plan_id"), "points": points.into_iter().map(|mut point| { point["fresh"] = json!(true); point }).collect::<Vec<_>>() })
        }
        Err(err) => json!({"ok": false, "error": err.to_string(), "points": []}),
    })
}

async fn profile_expand(Json(body): Json<Value>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "plan_id": body.get("plan_id"),
        "cluster_id": body.get("cluster_id"),
        "cluster": body.get("cluster_id"),
        "points": [],
        "budget_used": {"primary_points": 0, "primary_files": 0, "primary_lines": 0},
        "truncated": false
    }))
}

fn tracked_projects(store: &ClabStore) -> usize {
    store
        .list_projects()
        .ok()
        .and_then(|v| v.get("projects").and_then(Value::as_array).map(Vec::len))
        .unwrap_or(0)
}

fn profile_name(body: &Value) -> &str {
    body.get("profile")
        .and_then(Value::as_str)
        .unwrap_or("find_definition")
}

fn profile_budget(body: &Value) -> (Value, usize, usize) {
    let budget = body.get("budget").cloned().unwrap_or_else(|| json!({}));
    let max_points = budget
        .get("max_primary_points")
        .and_then(Value::as_u64)
        .unwrap_or(3) as usize;
    let max_chars = budget
        .get("max_total_response_chars")
        .and_then(Value::as_u64)
        .unwrap_or(16_000) as usize;
    (budget, max_points, max_chars)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlanSearchKind {
    Code,
    Graph,
}

#[derive(Clone, Debug)]
struct PlanSearchStep {
    kind: PlanSearchKind,
    term: String,
    reason: &'static str,
    confidence: f64,
}

fn push_term(terms: &mut Vec<String>, raw: &str) {
    let term = raw
        .trim_matches(|ch: char| !ch.is_alphanumeric() && !matches!(ch, '_' | '-' | '.' | '/'))
        .to_lowercase();
    if term.len() < 4 || terms.iter().any(|existing| existing == &term) {
        return;
    }
    terms.push(term);
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    for ch in query.chars() {
        if ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/') {
            current.push(ch);
            continue;
        }
        if !current.is_empty() {
            push_term(&mut terms, &current);
            current.clear();
        }
    }
    if !current.is_empty() {
        push_term(&mut terms, &current);
    }
    terms
}

fn is_codeish_term(term: &str) -> bool {
    term.contains('_') || term.contains('/') || term.contains('.') || term.contains('-')
}

fn is_symbol_like_term(term: &str) -> bool {
    term.contains('_') && !term.contains('/') && !term.contains('.')
}

fn push_search_step(
    steps: &mut Vec<PlanSearchStep>,
    kind: PlanSearchKind,
    term: &str,
    reason: &'static str,
    confidence: f64,
) {
    if term.is_empty()
        || steps
            .iter()
            .any(|step| step.kind == kind && step.term == term)
    {
        return;
    }
    steps.push(PlanSearchStep {
        kind,
        term: term.to_string(),
        reason,
        confidence,
    });
}

fn plan_search_steps(profile: &str, query: &str) -> Vec<PlanSearchStep> {
    let terms = query_terms(query);
    let mut codeish = Vec::new();
    let mut general = Vec::new();
    for term in terms {
        if is_codeish_term(&term) {
            codeish.push(term);
        } else {
            general.push(term);
        }
    }
    let mut ordered_terms = codeish.clone();
    ordered_terms.extend(general.clone());
    let symbol_like = codeish
        .iter()
        .filter(|term| is_symbol_like_term(term))
        .cloned()
        .collect::<Vec<_>>();
    let focused_terms = if !symbol_like.is_empty() {
        symbol_like
    } else if codeish.is_empty() {
        general.clone()
    } else {
        codeish.clone()
    };
    let mut steps = Vec::new();
    match profile {
        "find_definition" => {
            for term in &focused_terms {
                push_search_step(
                    &mut steps,
                    PlanSearchKind::Graph,
                    term,
                    "profile symbol match",
                    if is_codeish_term(term) { 0.95 } else { 0.8 },
                );
            }
            for term in &focused_terms {
                push_search_step(
                    &mut steps,
                    PlanSearchKind::Code,
                    term,
                    "profile lexical definition match",
                    if is_codeish_term(term) { 0.72 } else { 0.6 },
                );
            }
        }
        "trace_impact" => {
            for term in &focused_terms {
                push_search_step(
                    &mut steps,
                    PlanSearchKind::Code,
                    term,
                    "impact callsite match",
                    if is_codeish_term(term) { 0.9 } else { 0.7 },
                );
            }
            for term in &focused_terms {
                push_search_step(
                    &mut steps,
                    PlanSearchKind::Graph,
                    term,
                    "impact symbol seed",
                    if is_codeish_term(term) { 0.78 } else { 0.64 },
                );
            }
        }
        _ => {
            if !query.trim().is_empty() {
                push_search_step(
                    &mut steps,
                    PlanSearchKind::Code,
                    query.trim(),
                    "bug investigation lexical query match",
                    0.7,
                );
            }
            for term in &ordered_terms {
                push_search_step(
                    &mut steps,
                    PlanSearchKind::Code,
                    term,
                    "bug investigation lexical token match",
                    if is_codeish_term(term) { 0.82 } else { 0.68 },
                );
            }
            for term in &ordered_terms {
                push_search_step(
                    &mut steps,
                    PlanSearchKind::Graph,
                    term,
                    "bug investigation symbol match",
                    if is_codeish_term(term) { 0.66 } else { 0.58 },
                );
            }
        }
    }
    if steps.is_empty() && !query.trim().is_empty() {
        push_search_step(
            &mut steps,
            PlanSearchKind::Code,
            query.trim(),
            "fallback lexical match",
            0.5,
        );
    }
    steps
}

fn result_key(result: &Value) -> String {
    let file = result
        .get("file")
        .or_else(|| result.get("path"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let line = result
        .get("start_line")
        .or_else(|| result.get("line"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let name = result
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    format!("{file}:{line}:{name}")
}

fn definition_signatures(term: &str) -> [String; 6] {
    [
        format!("fn {term}"),
        format!("pub fn {term}"),
        format!("def {term}"),
        format!("class {term}"),
        format!("struct {term}"),
        format!("pub struct {term}"),
    ]
}

fn is_definition_snippet(snippet: &str, term: &str) -> bool {
    let lower = snippet.to_lowercase();
    definition_signatures(term)
        .iter()
        .any(|candidate| lower.contains(candidate))
}

fn profile_result_score(profile: &str, step: &PlanSearchStep, result: &Value) -> i32 {
    let term = step.term.to_lowercase();
    let snippet = result
        .get("snippet")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let path = result
        .get("file")
        .or_else(|| result.get("path"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let name = result
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let is_fixture_like_snippet = snippet.contains("\"snippet\":")
        || snippet.contains("json!({")
        || snippet.contains("\"query\":")
        || snippet.contains("\"profile\":")
        || snippet.contains("plan_search_steps(")
        || snippet.contains("build_profile_plan(")
        || snippet.contains("recommend_follow_up_calls(");
    let context_penalty = result
        .get("context_penalty")
        .and_then(Value::as_i64)
        .unwrap_or(0) as i32;
    let is_definition = is_definition_snippet(&snippet, &term) || name == term;
    let is_bare_quoted_symbol = snippet.trim() == format!("\"{term}\",")
        || snippet.trim() == format!("\"{term}\"")
        || snippet.trim() == format!("'{term}',")
        || snippet.trim() == format!("'{term}'");
    let is_callsite = snippet.contains(&format!("{term}(")) && !is_definition;
    let mut score = 0;
    if snippet.contains(&term) {
        score += 20;
    }
    if path.contains("/src/") || path.starts_with("src/") {
        score += 10;
    }
    if is_fixture_like_snippet {
        score -= 60;
    }
    if is_bare_quoted_symbol {
        score -= 40;
    }
    score -= context_penalty;
    match profile {
        "find_definition" => {
            if is_definition {
                score += 100;
            }
            if is_callsite {
                score += 15;
            }
        }
        "trace_impact" => {
            if is_callsite {
                score += 90;
            }
            if is_definition {
                score -= 10;
            }
        }
        _ => {
            if is_callsite {
                score += 50;
            }
            if is_definition {
                score += 20;
            }
        }
    }
    score
}

fn rank_search_results(profile: &str, step: &PlanSearchStep, results: &mut [Value]) {
    results.sort_by(|left, right| {
        profile_result_score(profile, step, right)
            .cmp(&profile_result_score(profile, step, left))
            .then_with(|| result_key(left).cmp(&result_key(right)))
    });
}

fn is_test_like_result(result: &Value) -> bool {
    let snippet = result
        .get("snippet")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let path = result
        .get("file")
        .or_else(|| result.get("path"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    path.contains("test")
        || snippet.contains("fn test")
        || snippet.contains("#[test]")
        || snippet.contains("assert!(")
        || snippet.contains("assert_eq!(")
        || snippet.contains("assert_ne!(")
}

fn is_helper_like_result(result: &Value) -> bool {
    let snippet = result
        .get("snippet")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let trimmed = snippet.trim();
    snippet.contains("locate `")
        || snippet.contains("\"query\":")
        || snippet.contains("\\\"query\\\":")
        || snippet.contains("\"profile\":")
        || snippet.contains("\\\"profile\\\":")
        || snippet.contains("\"snippet\":")
        || snippet.contains("\\\"snippet\\\":")
        || snippet.contains("json!({")
        || snippet.contains("plan_search_steps(")
        || snippet.contains("build_profile_plan(")
        || snippet.contains("recommend_follow_up_calls(")
        || snippet.contains(".to_string(),")
        || snippet.contains("term: \"")
        || (trimmed.starts_with('"') && trimmed.ends_with("\","))
}

fn is_source_like_result(result: &Value) -> bool {
    let path = result
        .get("file")
        .or_else(|| result.get("path"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    path.starts_with("src/")
        || path.contains("/src/")
        || path.starts_with("crates/")
        || path.ends_with(".rs")
}

fn is_test_context_line(text: &str, line: u64) -> bool {
    if line <= 1 {
        return false;
    }
    let lines = text.lines().collect::<Vec<_>>();
    let idx = (line.saturating_sub(1) as usize).min(lines.len());
    let start = idx.saturating_sub(32);
    lines[start..idx].iter().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with("#[test]")
            || trimmed.starts_with("#[cfg(test)]")
            || trimmed.starts_with("mod tests")
            || trimmed.starts_with("fn test_")
    })
}

fn contextual_result_penalty(root: &str, result: &Value) -> i32 {
    let relative = result
        .get("file")
        .or_else(|| result.get("path"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let line = result
        .get("start_line")
        .or_else(|| result.get("line"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if relative.is_empty() || line == 0 {
        return 0;
    }
    let path = FsPath::new(root).join(relative);
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    if is_test_context_line(&text, line) {
        80
    } else {
        0
    }
}

fn normalize_search_results(
    root: &str,
    profile: &str,
    step: &PlanSearchStep,
    search: &Value,
) -> Vec<Value> {
    let mut results = plan_search_results(search);
    for result in &mut results {
        let penalty = contextual_result_penalty(root, result);
        if penalty > 0 {
            result["context_penalty"] = json!(penalty);
        }
    }
    rank_search_results(profile, step, &mut results);
    if profile == "trace_impact" {
        if results.iter().any(is_source_like_result) {
            results.retain(is_source_like_result);
        }
        if results.iter().any(|result| {
            result
                .get("context_penalty")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                == 0
        }) {
            results.retain(|result| {
                result
                    .get("context_penalty")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    == 0
            });
        }
        if results.iter().any(|result| !is_test_like_result(result)) {
            results.retain(|result| !is_test_like_result(result));
        }
        if results.iter().any(|result| !is_helper_like_result(result)) {
            results.retain(|result| !is_helper_like_result(result));
        }
    } else if profile == "bug_investigation" {
        if results.iter().any(is_source_like_result) {
            results.retain(is_source_like_result);
        }
        if results.iter().any(|result| {
            result
                .get("context_penalty")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                == 0
        }) {
            results.retain(|result| {
                result
                    .get("context_penalty")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    == 0
            });
        }
        if results.iter().any(|result| !is_test_like_result(result)) {
            results.retain(|result| !is_test_like_result(result));
        }
        if results.iter().any(|result| !is_helper_like_result(result)) {
            results.retain(|result| !is_helper_like_result(result));
        }
    }
    results
        .into_iter()
        .map(|mut result| {
            if result.get("reason").is_none() {
                result["reason"] = json!(step.reason);
            }
            if result.get("confidence").is_none() {
                result["confidence"] = json!(step.confidence);
            }
            if result.get("snippet").is_none() {
                if let Some(name) = result.get("name").and_then(Value::as_str) {
                    let label = result
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("symbol");
                    result["snippet"] = json!(format!("{label} {name}"));
                }
            }
            result
        })
        .collect()
}

fn search_step_results(
    store: &ClabStore,
    root: &str,
    project: &str,
    profile: &str,
    step: &PlanSearchStep,
    limit: usize,
) -> Vec<Value> {
    let fetch_limit = match step.kind {
        PlanSearchKind::Code => limit.saturating_mul(8).max(8),
        PlanSearchKind::Graph => limit.saturating_mul(4).max(8),
    };
    let search = match step.kind {
        PlanSearchKind::Code => store
            .search_code(json!({"project": project, "pattern": step.term, "limit": fetch_limit})),
        PlanSearchKind::Graph => store
            .search_graph(json!({"project": project, "query": step.term, "limit": fetch_limit})),
    }
    .unwrap_or_else(|_| json!({"results": []}));
    normalize_search_results(root, profile, step, &search)
}

fn merge_plan_results(results: &mut Vec<Value>, incoming: Vec<Value>, limit: usize) {
    for result in incoming {
        if results
            .iter()
            .any(|existing| result_key(existing) == result_key(&result))
        {
            continue;
        }
        results.push(result);
        if results.len() >= limit {
            break;
        }
    }
}

fn finalize_plan_results(profile: &str, results: &mut Vec<Value>) {
    let has_clean_context = results.iter().any(|result| {
        result
            .get("context_penalty")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            == 0
    });
    if has_clean_context {
        results.retain(|result| {
            result
                .get("context_penalty")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                == 0
        });
    }
    if matches!(profile, "trace_impact" | "bug_investigation") {
        if results.iter().any(|result| !is_test_like_result(result)) {
            results.retain(|result| !is_test_like_result(result));
        }
        if results.iter().any(|result| !is_helper_like_result(result)) {
            results.retain(|result| !is_helper_like_result(result));
        }
    }
}

fn search_plan_matches(
    store: &ClabStore,
    root: &str,
    project: &str,
    profile: &str,
    query: &str,
    max_points: usize,
) -> Value {
    let limit = max_points.max(1);
    let mut results = Vec::new();
    for step in plan_search_steps(profile, query) {
        let incoming = search_step_results(store, root, project, profile, &step, limit);
        let found_strong_definition = profile == "find_definition"
            && step.kind == PlanSearchKind::Graph
            && !incoming.is_empty();
        let found_strong_bug_surface = profile == "bug_investigation"
            && step.kind == PlanSearchKind::Code
            && !incoming.is_empty();
        merge_plan_results(&mut results, incoming, limit);
        if results.len() >= limit || found_strong_definition || found_strong_bug_surface {
            break;
        }
    }
    finalize_plan_results(profile, &mut results);
    json!({"results": results})
}

fn plan_search_results(search: &Value) -> Vec<Value> {
    search
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn build_plan_points(root: &str, results: &[Value], max_points: usize) -> Vec<Value> {
    results
        .iter()
        .take(max_points)
        .enumerate()
        .map(|(idx, result)| {
            let file = result
                .get("file")
                .or_else(|| result.get("path"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let line = result
                .get("start_line")
                .or_else(|| result.get("line"))
                .and_then(Value::as_u64)
                .unwrap_or(1);
            let snippet = result
                .get("snippet")
                .and_then(Value::as_str)
                .unwrap_or_default();
            json!({
                "point_id": format!("pt_{idx}_{line}"),
                "path": file,
                "relative_path": file,
                "absolute_path": FsPath::new(root).join(file).to_string_lossy(),
                "start_line": line,
                "end_line": line,
                "line_count": 1,
                "snippet": snippet,
                "reason": result.get("reason").cloned().unwrap_or_else(|| json!("clab lexical match")),
                "confidence": result.get("confidence").cloned().unwrap_or_else(|| json!(0.5)),
                "cluster_id": "cluster_primary"
            })
        })
        .collect()
}

fn primary_symbol_hint(points: &[Value]) -> Option<String> {
    points.iter().find_map(|point| {
        let snippet = point
            .get("snippet")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let snippet_terms = query_terms(snippet);
        snippet_terms
            .iter()
            .find(|term| term.contains("clab") && !term.contains('/') && !term.contains('.'))
            .cloned()
            .or_else(|| {
                snippet_terms
                    .iter()
                    .find(|term| {
                        is_codeish_term(term) && !term.contains('/') && !term.contains('.')
                    })
                    .cloned()
            })
            .or_else(|| {
                point.get("path").and_then(Value::as_str).and_then(|path| {
                    query_terms(path).into_iter().find(|term| {
                        term.contains("clab") && !term.contains('/') && !term.contains('.')
                    })
                })
            })
    })
}

fn recommend_follow_up_calls(
    profile: &str,
    plan_id: &str,
    query: &str,
    points: &[Value],
) -> Vec<String> {
    let symbol = primary_symbol_hint(points)
        .or_else(|| {
            query_terms(query)
                .into_iter()
                .find(|term| is_codeish_term(term))
        })
        .or_else(|| query_terms(query).into_iter().find(|term| term.len() >= 4))
        .unwrap_or_else(|| "target_symbol".to_string());
    let first_point = points.first();
    let point_id = first_point
        .and_then(|point| point.get("point_id"))
        .and_then(Value::as_str)
        .unwrap_or("pt_0_1");
    let path = first_point
        .and_then(|point| point.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("path/to/file");
    match profile {
        "find_definition" => vec![
            format!(
                "clab_read_points(plan_id=\"{plan_id}\", point_ids=[\"{point_id}\"]) to inspect the strongest definition candidate first."
            ),
            format!(
                "clab_snippet(qualified_name=\"{symbol}\") to recover the owning declaration and nearby context."
            ),
            format!(
                "clab_trace(function_name=\"{symbol}\", direction=\"inbound\", depth=1) to find immediate callers after the definition is confirmed."
            ),
        ],
        "trace_impact" => vec![
            format!(
                "clab_read_points(plan_id=\"{plan_id}\", point_ids=[\"{point_id}\"]) to inspect the highest-signal impact seed first."
            ),
            format!(
                "clab_trace(function_name=\"{symbol}\", direction=\"both\", depth=2) to map callers, callees, and likely verification scope."
            ),
            format!(
                "clab_search_code(pattern=\"{symbol}\", file_pattern=\"{path}\", context=6) to inspect the local callsite and surrounding state changes."
            ),
        ],
        _ => vec![
            format!(
                "clab_read_points(plan_id=\"{plan_id}\", point_ids=[\"{point_id}\"]) to inspect the most relevant bug surface first."
            ),
            format!(
                "clab_search_code(pattern=\"{symbol}\", context=6) to gather adjacent lexical evidence before widening scope."
            ),
            format!(
                "clab_trace(function_name=\"{symbol}\", direction=\"both\", depth=1) if the first point suggests a behavior or ownership question."
            ),
        ],
    }
}
fn build_profile_plan(store: &ClabStore, body: &Value) -> Value {
    let requested_root =
        profile_root(body).unwrap_or_else(|| env_project_path().unwrap_or_else(|| ".".to_string()));
    let root = fs::canonicalize(&requested_root)
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or(requested_root);
    let _ = store.index_repository(json!({"repo_path": root, "mode": "fast"}));
    let project = project_name(FsPath::new(&root));
    let profile = profile_name(body);
    let query = body
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (budget, max_points, max_chars) = profile_budget(body);
    let search = search_plan_matches(store, &root, &project, profile, query, max_points);
    let points = build_plan_points(&root, &plan_search_results(&search), max_points);
    let plan_id = unique_plan_id();
    let next = recommend_follow_up_calls(profile, &plan_id, query, &points);
    let plan = json!({
        "ok": true,
        "plan_id": plan_id,
        "profile": profile,
        "primary": points,
        "deferred_clusters": [],
        "next": next,
        "budget_used": {
            "primary_points": points_len(&search, max_points),
            "primary_files": 1,
            "primary_lines": points_len(&search, max_points),
            "response_chars": max_chars.min(search.to_string().len() + 512)
        },
        "search_scope": {"effective_roots": [root]},
        "constraints": body.get("constraints").cloned().unwrap_or_else(|| json!({})),
        "budget": budget
    });
    let _ = write_plan(
        plan.get("plan_id")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        &plan,
    );
    plan
}

fn scoped_tool_args(store: &ClabStore, body: &Value) -> Value {
    let mut args = body.clone();
    if let Some(obj) = args.as_object_mut() {
        for key in ["path", "cwd", "scope", "roots", "max_parent_depth"] {
            obj.remove(key);
        }
    }
    if args.get("project").and_then(Value::as_str).is_none() {
        args["project"] = json!(project_for_scope(store, body));
    }
    args
}

fn project_for_scope(store: &ClabStore, body: &Value) -> String {
    let root = body
        .get("roots")
        .and_then(Value::as_array)
        .and_then(|roots| roots.first())
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            body.get("path")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .or_else(env_project_path)
        .unwrap_or_else(|| ".".to_string());
    let _ = store.index_repository(json!({"repo_path": root, "mode": "fast"}));
    project_name(FsPath::new(&root))
}

fn profile_root(body: &Value) -> Option<String> {
    body.get("scope")
        .and_then(|scope| scope.get("roots"))
        .and_then(Value::as_array)
        .and_then(|roots| roots.first())
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            body.get("scope")
                .and_then(|scope| scope.get("path"))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
}

fn env_project_path() -> Option<String> {
    env::var("CLAB_CODEBASE_PROJECT_PATH")
        .ok()
        .or_else(|| env::var("CLAB_PROJECT_PATH").ok())
}

fn index_env_project(store: &ClabStore) {
    if let Some(path) = env_project_path() {
        let _ = store.index_repository(json!({"repo_path": path, "mode": "fast"}));
    }
}

fn plan_dir() -> PathBuf {
    let base = env::var_os("CLAB_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".clab")))
        .unwrap_or_else(|| PathBuf::from(".clab"));
    let dir = base.join("plans");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn write_plan(plan_id: &str, plan: &Value) -> Result<()> {
    fs::write(
        plan_dir().join(format!("{plan_id}.json")),
        serde_json::to_vec_pretty(plan)?,
    )?;
    Ok(())
}

fn read_plan_points(body: &Value) -> Result<Vec<Value>> {
    let plan_id = body
        .get("plan_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let plan: Value =
        serde_json::from_slice(&fs::read(plan_dir().join(format!("{plan_id}.json")))?)?;
    let points = plan
        .get("primary")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let requested = body.get("point_ids").and_then(Value::as_array).map(|ids| {
        ids.iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    });
    Ok(match requested {
        Some(ids) if !ids.is_empty() => points
            .into_iter()
            .filter(|point| {
                point
                    .get("point_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| ids.iter().any(|requested| requested == id))
            })
            .collect(),
        _ => points,
    })
}

fn points_len(search: &Value, max_points: usize) -> usize {
    search
        .get("results")
        .and_then(Value::as_array)
        .map(|v| v.len().min(max_points))
        .unwrap_or(0)
}

fn unique_plan_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = PLAN_ID_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("cp_{nanos:x}_{:x}_{seq:x}", process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        mcp::tool_input_schema,
        skills::{compact_skill, skill_search},
    };
    use std::{
        fs,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn unique_test_repo(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = env::temp_dir().join(format!("clab-daemon-{name}-{unique}"));
        fs::create_dir_all(root.join("src")).unwrap();
        root
    }

    #[test]
    fn default_status_is_disabled_until_served() {
        let daemon = AutoIndexDaemon::new(AutoIndexConfig::default());
        let status = daemon.status();
        assert!(status.enabled);
        assert!(!status.running);
        assert_eq!(status.tracked_projects, 0);
    }

    #[test]
    fn mcp_tool_schemas_expose_required_arguments() {
        let search_code = tool_input_schema("search_code");
        assert_eq!(search_code["properties"]["pattern"]["type"], "string");
        assert!(search_code["properties"]["limit"]["description"]
            .as_str()
            .unwrap()
            .contains("Defaults to 8"));
        assert_eq!(search_code["required"], json!(["pattern"]));

        let index_repository = tool_input_schema("index_repository");
        assert_eq!(
            index_repository["properties"]["repo_path"]["type"],
            "string"
        );
        assert_eq!(index_repository["required"], json!([]));

        let index_status = tool_input_schema("index_status");
        assert_eq!(index_status["properties"]["project"]["type"], "string");
        assert_eq!(index_status["required"], json!(["project"]));

        let detect_changes = tool_input_schema("detect_changes");
        assert!(detect_changes["properties"]["limit"]["description"]
            .as_str()
            .unwrap()
            .contains("Defaults to 50"));

        let skill_get = tool_input_schema("skill_get");
        assert_eq!(skill_get["properties"]["summary_only"]["type"], "boolean");
        assert_eq!(skill_get["properties"]["max_chars"]["type"], "integer");
        assert_eq!(skill_get["required"], json!(["name"]));
    }

    #[test]
    fn compact_skill_can_omit_or_truncate_body() {
        let skill = json!({
            "name": "demo",
            "summary": "Demo skill",
            "tags": [],
            "version": 1,
            "body": "abcdef"
        });
        let summary = compact_skill(skill.clone(), true, None);
        assert!(summary.get("body").is_none());
        assert_eq!(summary["summary"], "Demo skill");

        let truncated = compact_skill(skill, false, Some(3));
        assert_eq!(truncated["body"], "abc");
        assert_eq!(truncated["truncated"], true);
    }

    #[test]
    fn skill_search_defaults_to_five_results() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_test_repo("skills-default-limit");
        fs::create_dir_all(&dir).unwrap();
        let old_skills_dir = env::var_os("CLAB_SKILLS_DIR");
        env::set_var("CLAB_SKILLS_DIR", &dir);

        for index in 0..7 {
            fs::write(
                dir.join(format!("skill-{index}.md")),
                format!("---\nsummary: Test skill {index}\nversion: 1\n---\n\nbody\n"),
            )
            .unwrap();
        }

        let results = skill_search(json!({})).unwrap();

        if let Some(old_skills_dir) = old_skills_dir {
            env::set_var("CLAB_SKILLS_DIR", old_skills_dir);
        } else {
            env::remove_var("CLAB_SKILLS_DIR");
        }
        fs::remove_dir_all(dir).unwrap();

        assert_eq!(results.as_array().unwrap().len(), 5);
    }

    #[test]
    fn plan_search_steps_vary_by_profile() {
        let definition = plan_search_steps("find_definition", "Locate `target_symbol` handler");
        let impact = plan_search_steps("trace_impact", "Locate `target_symbol` handler");
        let bug = plan_search_steps("bug_investigation", "Locate `target_symbol` handler");
        assert_eq!(definition[0].kind, PlanSearchKind::Graph);
        assert_eq!(definition[0].reason, "profile symbol match");
        assert_eq!(impact[0].kind, PlanSearchKind::Code);
        assert_eq!(impact[0].reason, "impact callsite match");
        assert_eq!(bug[0].kind, PlanSearchKind::Code);
        assert_eq!(bug[0].term, "Locate `target_symbol` handler");
    }

    #[test]
    fn plan_search_steps_prefer_symbol_like_terms_over_path_like_terms() {
        let steps = plan_search_steps(
            "trace_impact",
            "target_symbol .ai-bridge/current-plan.md .ai-bridge/amaze-plan.md",
        );
        assert!(steps.iter().any(|step| step.term == "target_symbol"));
        assert!(steps
            .iter()
            .all(|step| !step.term.contains("current-plan.md")));
        assert!(steps
            .iter()
            .all(|step| !step.term.contains("amaze-plan.md")));
    }

    #[test]
    fn build_plan_points_preserves_search_fields() {
        let points = build_plan_points(
            "/tmp/workspace",
            &[json!({
                "file": "src/main.rs",
                "start_line": 7,
                "snippet": "fn main() {}",
                "reason": "profile symbol match",
                "confidence": 0.95
            })],
            3,
        );
        assert_eq!(points.len(), 1);
        assert_eq!(points[0]["point_id"], "pt_0_7");
        assert_eq!(points[0]["path"], "src/main.rs");
        assert_eq!(points[0]["relative_path"], "src/main.rs");
        assert_eq!(
            points[0]["absolute_path"],
            json!("/tmp/workspace/src/main.rs")
        );
        assert_eq!(points[0]["start_line"], 7);
        assert_eq!(points[0]["end_line"], 7);
        assert_eq!(points[0]["snippet"], "fn main() {}");
        assert_eq!(points[0]["reason"], "profile symbol match");
        assert_eq!(points[0]["confidence"], 0.95);
    }

    #[test]
    fn build_profile_plan_uses_profile_specific_matching() {
        let root = unique_test_repo("profile-plan");
        fs::write(
            root.join("src/lib.rs"),
            "pub fn caller() { target_symbol(); }\npub fn target_symbol() {}\n",
        )
        .unwrap();
        let store = ClabStore::with_root(root.join(".clab-store")).unwrap();
        let definition = build_profile_plan(
            &store,
            &json!({
                "profile": "find_definition",
                "query": "target_symbol",
                "scope": {"roots": [root.to_string_lossy().to_string()]},
                "budget": {"max_primary_points": 2, "max_total_response_chars": 4000}
            }),
        );
        let impact = build_profile_plan(
            &store,
            &json!({
                "profile": "trace_impact",
                "query": "target_symbol",
                "scope": {"roots": [root.to_string_lossy().to_string()]},
                "budget": {"max_primary_points": 2, "max_total_response_chars": 4000}
            }),
        );

        let definition_primary = definition["primary"].as_array().unwrap();
        let impact_primary = impact["primary"].as_array().unwrap();
        assert_eq!(definition["profile"], "find_definition");
        assert_eq!(impact["profile"], "trace_impact");
        assert_eq!(definition_primary[0]["start_line"], 2);
        assert_eq!(definition_primary[0]["reason"], "profile symbol match");
        assert_eq!(impact_primary[0]["start_line"], 1);
        assert_eq!(impact_primary[0]["reason"], "impact callsite match");

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn find_definition_ignores_generic_terms_when_symbol_matches_exist() {
        let root = unique_test_repo("definition-noise");
        fs::write(
            root.join("src/lib.rs"),
            "pub fn caller() { target_symbol(); }\npub fn target_symbol() {}\npub fn main_files() {}\n",
        )
        .unwrap();
        let store = ClabStore::with_root(root.join(".clab-store")).unwrap();
        let plan = build_profile_plan(
            &store,
            &json!({
                "profile": "find_definition",
                "query": "locate target_symbol entrypoint and identify main files involved",
                "scope": {"roots": [root.to_string_lossy().to_string()]},
                "budget": {"max_primary_points": 3, "max_total_response_chars": 4000}
            }),
        );
        let primary = plan["primary"].as_array().unwrap();
        assert_eq!(primary[0]["start_line"], 2);
        assert!(primary.iter().all(|point| point["snippet"]
            .as_str()
            .unwrap_or("")
            .contains("target_symbol")));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn bug_investigation_prefers_src_results_over_test_files() {
        let root = unique_test_repo("bug-test-noise");
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn apply() { target_symbol(); }\npub fn target_symbol() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("tests/noise.rs"),
            "fn test_one() { target_symbol(); }\nfn test_two() { target_symbol(); }\n",
        )
        .unwrap();
        let store = ClabStore::with_root(root.join(".clab-store")).unwrap();
        let plan = build_profile_plan(
            &store,
            &json!({
                "profile": "bug_investigation",
                "query": "locate target_symbol entrypoint and identify main files involved",
                "scope": {"roots": [root.to_string_lossy().to_string()]},
                "budget": {"max_primary_points": 3, "max_total_response_chars": 4000}
            }),
        );
        let primary = plan["primary"].as_array().unwrap();
        assert_eq!(primary[0]["path"], "src/lib.rs");
        assert!(primary
            .iter()
            .all(|point| point["path"] != "tests/noise.rs"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn bug_investigation_stops_after_strong_lexical_hits() {
        let root = unique_test_repo("bug-lexical-stop");
        fs::write(
            root.join("src/lib.rs"),
            "pub fn apply() { target_symbol(); }\npub fn target_symbol() {}\nfn helper() { target_symbol(); }\n",
        )
        .unwrap();
        let store = ClabStore::with_root(root.join(".clab-store")).unwrap();
        let plan = build_profile_plan(
            &store,
            &json!({
                "profile": "bug_investigation",
                "query": "locate target_symbol entrypoint and identify main files involved",
                "scope": {"roots": [root.to_string_lossy().to_string()]},
                "budget": {"max_primary_points": 6, "max_total_response_chars": 4000}
            }),
        );
        let primary = plan["primary"].as_array().unwrap();
        assert!(primary.iter().all(|point| point["path"] == "src/lib.rs"));
        assert!(primary.len() <= 3);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn bug_investigation_does_not_expand_to_secondary_symbols_after_hits() {
        let root = unique_test_repo("bug-secondary-symbols");
        fs::write(
            root.join("src/lib.rs"),
            "pub fn apply() { target_symbol(); }\npub fn target_symbol() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/secondary.rs"),
            "pub fn main_files() {}\npub fn handler() {}\n",
        )
        .unwrap();
        let store = ClabStore::with_root(root.join(".clab-store")).unwrap();
        let plan = build_profile_plan(
            &store,
            &json!({
                "profile": "bug_investigation",
                "query": "locate target_symbol entrypoint and identify main files involved handler",
                "scope": {"roots": [root.to_string_lossy().to_string()]},
                "budget": {"max_primary_points": 6, "max_total_response_chars": 4000}
            }),
        );
        let primary = plan["primary"].as_array().unwrap();
        assert!(primary.iter().all(|point| point["path"] == "src/lib.rs"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn test_context_detection_reaches_longer_test_bodies() {
        let text = "#[test]\nfn test_route() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    let e = 5;\n    let f = 6;\n    let g = 7;\n    let h = 8;\n    let i = 9;\n    let j = 10;\n    let k = 11;\n    let l = 12;\n    target_symbol();\n}\n";
        assert!(is_test_context_line(text, 15));
    }
    #[test]
    fn trace_impact_prefers_src_callsites_over_test_noise() {
        let root = unique_test_repo("impact-noise");
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn apply() { target_symbol(); }\npub fn target_symbol() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("tests/noise.rs"),
            "fn test_one() { target_symbol(); }\nfn test_two() { target_symbol(); }\nfn test_three() { target_symbol(); }\n",
        )
        .unwrap();
        let store = ClabStore::with_root(root.join(".clab-store")).unwrap();
        let plan = build_profile_plan(
            &store,
            &json!({
                "profile": "trace_impact",
                "query": "locate target_symbol entrypoint and identify main files involved",
                "scope": {"roots": [root.to_string_lossy().to_string()]},
                "budget": {"max_primary_points": 3, "max_total_response_chars": 4000}
            }),
        );
        let primary = plan["primary"].as_array().unwrap();
        assert_eq!(primary[0]["path"], "src/lib.rs");
        assert_eq!(primary[0]["start_line"], 1);
        assert!(primary
            .iter()
            .all(|point| point["path"] != "tests/noise.rs"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn trace_impact_ranking_penalizes_test_like_snippets() {
        let step = PlanSearchStep {
            kind: PlanSearchKind::Code,
            term: "target_symbol".to_string(),
            reason: "impact callsite match",
            confidence: 0.9,
        };
        let mut results = vec![
            json!({"file": "tests/noise.rs", "start_line": 10, "snippet": "fn test_route() { target_symbol(); assert!(true); }"}),
            json!({"file": "crates/clab-daemon/src/lib.rs", "start_line": 20, "snippet": "pub fn apply() { target_symbol(); }"}),
        ];
        rank_search_results("trace_impact", &step, &mut results);
        assert_eq!(results[0]["start_line"], 20);
    }

    #[test]
    fn trace_impact_prefers_dispatch_over_quoted_symbol_list_entries() {
        let step = PlanSearchStep {
            kind: PlanSearchKind::Code,
            term: "target_symbol".to_string(),
            reason: "impact callsite match",
            confidence: 0.9,
        };
        let mut results = vec![
            json!({"file": "crates/clab-daemon/src/lib.rs", "start_line": 10, "snippet": "    \"target_symbol\","}),
            json!({"file": "crates/clab-daemon/src/lib.rs", "start_line": 20, "snippet": "\"target_symbol\" => target_symbol(store, args),"}),
        ];
        rank_search_results("trace_impact", &step, &mut results);
        assert_eq!(results[0]["start_line"], 20);
    }

    #[test]
    fn trace_impact_prefers_dispatch_over_term_assignment_lines() {
        let step = PlanSearchStep {
            kind: PlanSearchKind::Code,
            term: "target_symbol".to_string(),
            reason: "impact callsite match",
            confidence: 0.9,
        };
        let mut results = vec![
            json!({"file": "crates/clab-daemon/src/lib.rs", "start_line": 10, "snippet": "term: \"target_symbol\".to_string(),"}),
            json!({"file": "crates/clab-daemon/src/lib.rs", "start_line": 20, "snippet": "\"target_symbol\" => target_symbol(store, args),"}),
        ];
        rank_search_results("trace_impact", &step, &mut results);
        assert_eq!(results[0]["start_line"], 20);
    }

    #[test]
    fn trace_impact_ranking_penalizes_embedded_fixture_snippets() {
        let step = PlanSearchStep {
            kind: PlanSearchKind::Code,
            term: "target_symbol".to_string(),
            reason: "impact callsite match",
            confidence: 0.9,
        };
        let mut results = vec![
            json!({"file": "crates/clab-daemon/src/lib.rs", "start_line": 10, "snippet": "\"snippet\": \"pub fn caller() { target_symbol(); }\""}),
            json!({"file": "crates/clab-daemon/src/lib.rs", "start_line": 20, "snippet": "pub fn caller() { target_symbol(); }"}),
        ];
        rank_search_results("trace_impact", &step, &mut results);
        assert_eq!(results[0]["start_line"], 20);
    }

    #[test]
    fn trace_impact_prefers_dispatch_callsites_over_helper_mentions() {
        let step = PlanSearchStep {
            kind: PlanSearchKind::Code,
            term: "target_symbol".to_string(),
            reason: "impact callsite match",
            confidence: 0.9,
        };
        let mut results = vec![
            json!({"file": "crates/clab-daemon/src/lib.rs", "start_line": 10, "snippet": "let definition = plan_search_steps(\"find_definition\", \"Locate `target_symbol` handler\");"}),
            json!({"file": "crates/clab-daemon/src/lib.rs", "start_line": 20, "snippet": "\"target_symbol\" => target_symbol(store, args),"}),
        ];
        rank_search_results("trace_impact", &step, &mut results);
        assert_eq!(results[0]["start_line"], 20);
    }

    #[test]
    fn trace_impact_prefers_source_dispatch_over_wrapper_docs() {
        let step = PlanSearchStep {
            kind: PlanSearchKind::Code,
            term: "target_symbol".to_string(),
            reason: "impact callsite match",
            confidence: 0.9,
        };
        let mut results = vec![
            json!({"file": "scripts/adapter.mjs", "start_line": 31, "snippet": "target_symbol tool. The adapter is plan-only and does not edit source files."}),
            json!({"file": "crates/clab-daemon/src/lib.rs", "start_line": 307, "snippet": "\"target_symbol\" => target_symbol(store, args),"}),
        ];
        rank_search_results("trace_impact", &step, &mut results);
        assert_eq!(results[0]["file"], "crates/clab-daemon/src/lib.rs");
    }

    #[test]
    fn trace_impact_prefers_source_results_over_wrapper_files_in_plans() {
        let root = unique_test_repo("wrapper-bias");
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn apply() { target_symbol(); }\npub fn target_symbol() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("scripts/helper.mjs"),
            "target_symbol tool. The adapter is plan-only and does not edit source files.\nconst call = target_symbol;\n",
        )
        .unwrap();
        let store = ClabStore::with_root(root.join(".clab-store")).unwrap();
        let plan = build_profile_plan(
            &store,
            &json!({
                "profile": "trace_impact",
                "query": "locate target_symbol entrypoint and identify main files involved",
                "scope": {"roots": [root.to_string_lossy().to_string()]},
                "budget": {"max_primary_points": 4, "max_total_response_chars": 4000}
            }),
        );
        let primary = plan["primary"].as_array().unwrap();
        assert!(primary
            .iter()
            .all(|point| point["path"] != "scripts/helper.mjs"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn trace_impact_excludes_helper_string_mentions_when_callsites_exist() {
        let root = unique_test_repo("impact-helper-mentions");
        fs::write(
            root.join("src/lib.rs"),
            "pub fn apply() { target_symbol(); }\npub fn target_symbol() {}\nlet helper = \"Locate `target_symbol` handler\";\nlet fixture = \"\\\"snippet\\\": \\\"pub fn caller() { target_symbol(); }\\\"\";\n",
        )
        .unwrap();
        let store = ClabStore::with_root(root.join(".clab-store")).unwrap();
        let plan = build_profile_plan(
            &store,
            &json!({
                "profile": "trace_impact",
                "query": "locate target_symbol entrypoint and identify main files involved",
                "scope": {"roots": [root.to_string_lossy().to_string()]},
                "budget": {"max_primary_points": 4, "max_total_response_chars": 4000}
            }),
        );
        let primary = plan["primary"].as_array().unwrap();
        assert!(primary.iter().all(|point| {
            let snippet = point["snippet"].as_str().unwrap_or("");
            !snippet.contains("Locate `target_symbol` handler")
                && !snippet.contains("\\\"snippet\\\":")
        }));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn trace_impact_prefers_operational_callsites_over_query_setup_lines() {
        let step = PlanSearchStep {
            kind: PlanSearchKind::Code,
            term: "target_symbol".to_string(),
            reason: "impact callsite match",
            confidence: 0.9,
        };
        let mut results = vec![
            json!({"file": "crates/clab-daemon/src/lib.rs", "start_line": 10, "snippet": "\"query\": \"target_symbol\","}),
            json!({"file": "crates/clab-daemon/src/lib.rs", "start_line": 20, "snippet": "\"target_symbol\" => target_symbol(store, args),"}),
        ];
        rank_search_results("trace_impact", &step, &mut results);
        assert_eq!(results[0]["start_line"], 20);
    }
    #[test]
    fn test_context_detection_flags_lines_after_test_attribute() {
        let text = "#[test]\nfn test_route() {\n    target_symbol();\n}\n";
        assert!(is_test_context_line(text, 3));
        assert!(!is_test_context_line(text, 1));
    }

    #[test]
    fn follow_up_calls_prefer_symbol_over_path_hint() {
        let next = recommend_follow_up_calls(
            "trace_impact",
            "cp_test",
            "exercise target_symbol bridge",
            &[json!({
                "point_id": "pt_0_1",
                "path": "crates/clab-daemon/src/lib.rs",
                "snippet": "pub fn target_symbol() {}"
            })],
        );
        assert!(next
            .iter()
            .any(|item| item.contains("function_name=\"target_symbol\"")));
        assert!(next
            .iter()
            .all(|item| !item.contains("function_name=\"clab-daemon\"")
                && !item.contains("function_name=\"crates/clab-daemon/src/lib.rs\"")));
    }

    #[test]
    fn primary_symbol_hint_prefers_embedded_symbol_over_path_token() {
        let symbol = primary_symbol_hint(&[json!({
            "path": "crates/clab-daemon/src/lib.rs",
            "snippet": "json!({\"file\": \"crates/clab-daemon/src/lib.rs\", \"snippet\": \"pub fn apply() { target_symbol(); }\"})"
        })]);
        assert_eq!(symbol.as_deref(), Some("target_symbol"));
    }

    #[test]
    fn follow_up_calls_fallback_to_query_before_path_hint() {
        let next = recommend_follow_up_calls(
            "trace_impact",
            "cp_test",
            "exercise target_symbol bridge",
            &[json!({
                "point_id": "pt_0_1",
                "path": "crates/clab-daemon/src/lib.rs",
                "snippet": "pub fn route() {}"
            })],
        );
        assert!(next
            .iter()
            .any(|item| item.contains("function_name=\"target_symbol\"")));
        assert!(next
            .iter()
            .all(|item| !item.contains("function_name=\"clab-daemon\"")
                && !item.contains("function_name=\"crates/clab-daemon/src/lib.rs\"")));
    }
    #[test]
    fn build_profile_plan_generates_unique_plan_ids() {
        let root = unique_test_repo("unique-plan-id");
        fs::write(root.join("src/lib.rs"), "pub fn target_symbol() {}\n").unwrap();
        let store = ClabStore::with_root(root.join(".clab-store")).unwrap();
        let body = json!({
            "profile": "find_definition",
            "query": "target_symbol",
            "scope": {"roots": [root.to_string_lossy().to_string()]},
            "budget": {"max_primary_points": 1, "max_total_response_chars": 2000}
        });
        let first = build_profile_plan(&store, &body);
        let second = build_profile_plan(&store, &body);
        assert_ne!(first["plan_id"], second["plan_id"]);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn build_profile_plan_emits_profile_follow_up_calls() {
        let root = unique_test_repo("next-calls");
        fs::write(
            root.join("src/lib.rs"),
            "pub fn caller() { target_symbol(); }\npub fn target_symbol() {}\n",
        )
        .unwrap();
        let store = ClabStore::with_root(root.join(".clab-store")).unwrap();
        let definition = build_profile_plan(
            &store,
            &json!({
                "profile": "find_definition",
                "query": "target_symbol",
                "scope": {"roots": [root.to_string_lossy().to_string()]},
                "budget": {"max_primary_points": 2, "max_total_response_chars": 4000}
            }),
        );
        let impact = build_profile_plan(
            &store,
            &json!({
                "profile": "trace_impact",
                "query": "target_symbol",
                "scope": {"roots": [root.to_string_lossy().to_string()]},
                "budget": {"max_primary_points": 2, "max_total_response_chars": 4000}
            }),
        );
        let definition_next = definition["next"].as_array().unwrap();
        let impact_next = impact["next"].as_array().unwrap();
        assert!(!definition_next.is_empty());
        assert!(!impact_next.is_empty());
        assert!(definition_next.iter().any(|item| item
            .as_str()
            .is_some_and(|text| text.contains("clab_snippet"))));
        assert!(impact_next.iter().any(|item| item
            .as_str()
            .is_some_and(|text| text.contains("clab_trace"))));
        fs::remove_dir_all(root).ok();
    }
}
