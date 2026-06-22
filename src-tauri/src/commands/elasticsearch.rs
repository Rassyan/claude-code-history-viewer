use crate::elasticsearch::{self, EsClient, ProgressReporter, SyncProgress, SyncState, SyncStats};
use crate::models::ClaudeMessage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter};

#[derive(Debug, Serialize, Deserialize)]
pub struct EsSyncStatus {
    pub connected: bool,
    pub last_full_sync: Option<String>,
    pub files_tracked: usize,
    pub messages_count: u64,
    pub sessions_count: u64,
}

/// Check ES connection health.
#[tauri::command]
pub async fn es_check_connection(
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
) -> Result<bool, String> {
    let client = EsClient::new(&endpoint, username.as_deref(), password.as_deref());
    client.health().await
}

/// Run full sync of all local JSONL files to ES.
#[tauri::command]
pub async fn es_full_sync(
    app: AppHandle,
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
    device_id: String,
    custom_claude_paths: Option<Vec<String>>,
) -> Result<SyncStats, String> {
    // Persist ES settings so the file watcher can pick them up
    save_es_settings(&endpoint, username.as_deref(), password.as_deref());

    let client = EsClient::new(&endpoint, username.as_deref(), password.as_deref());

    // Update sync state with endpoint and device_id
    let mut state = elasticsearch::load_sync_state();
    state.es_endpoint.clone_from(&endpoint);
    state.device_id.clone_from(&device_id);
    save_sync_state_external(&state);

    let paths = custom_claude_paths.unwrap_or_default();
    let reporter = TauriProgressReporter { app };
    let result = elasticsearch::full_sync(&client, &device_id, &paths, &reporter).await;
    if let Err(ref e) = result {
        // Emit error phase so frontend can react with toast
        let _ = reporter.app.emit(
            "es-sync-progress",
            SyncProgress {
                phase: "error".to_string(),
                files_processed: 0,
                total_files: 0,
                messages_indexed: 0,
                sessions_indexed: 0,
                current_file: e.clone(),
            },
        );
    }
    result
}

/// Tauri-backed progress reporter that emits `es-sync-progress` events.
struct TauriProgressReporter {
    app: AppHandle,
}

impl ProgressReporter for TauriProgressReporter {
    fn report(&self, progress: &SyncProgress) {
        let _ = self.app.emit("es-sync-progress", progress);
    }
}

/// Request cancellation of the currently running full sync.
/// The sync will stop at the next safe checkpoint (between files) and flush
/// pending batches. Returns immediately.
#[tauri::command]
pub fn es_cancel_sync() -> Result<(), String> {
    elasticsearch::request_cancel_sync();
    Ok(())
}

/// Persisted ES connection settings (returned to frontend on startup).
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct PersistedEsSettings {
    pub endpoint: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub device_id: Option<String>,
}

/// Read persisted ES settings from disk (`~/.claude-history-viewer/es-settings.json`
/// + `es-sync-state.json`). Returns empty if not configured. Used by frontend
/// to restore connection settings without relying on browser-only `localStorage`,
/// enabling correct behavior in both Tauri and `--serve` (web-ui) modes.
#[tauri::command]
pub fn es_get_settings() -> Result<PersistedEsSettings, String> {
    let mut out = PersistedEsSettings::default();

    // ES credentials
    let settings_path = get_es_settings_path();
    if let Ok(content) = fs::read_to_string(&settings_path) {
        if let Ok(val) = serde_json::from_str::<Value>(&content) {
            out.endpoint = val
                .get("endpoint")
                .and_then(|v| v.as_str())
                .map(String::from);
            out.username = val
                .get("username")
                .and_then(|v| v.as_str())
                .map(String::from);
            out.password = val
                .get("password")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
    }

    // device_id lives in sync state
    let state = elasticsearch::load_sync_state();
    if !state.device_id.is_empty() {
        out.device_id = Some(state.device_id);
    }
    // Endpoint in sync state takes precedence if present (last sync's endpoint)
    if !state.es_endpoint.is_empty() {
        out.endpoint = Some(state.es_endpoint);
    }

    Ok(out)
}

/// Save ES settings explicitly (without triggering a sync).
/// Used by the settings UI so credentials are persisted independently of the
/// "Test Connection" / "Full Sync" actions.
#[tauri::command]
pub fn es_save_settings(
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
    device_id: Option<String>,
) -> Result<(), String> {
    save_es_settings(&endpoint, username.as_deref(), password.as_deref());

    let mut state = elasticsearch::load_sync_state();
    state.es_endpoint = endpoint;
    if let Some(d) = device_id {
        state.device_id = d;
    }
    save_sync_state_external(&state);
    Ok(())
}

/// Get current sync status (connection + stats).
#[tauri::command]
pub async fn es_get_sync_status(
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
) -> Result<EsSyncStatus, String> {
    let client = EsClient::new(&endpoint, username.as_deref(), password.as_deref());

    let connected = client.health().await.unwrap_or(false);

    let state = elasticsearch::load_sync_state();

    let (messages_count, sessions_count) = if connected {
        let msg_count = client
            .count(
                elasticsearch::MESSAGES_INDEX,
                &serde_json::json!({"query": {"match_all": {}}}),
            )
            .await
            .unwrap_or(0);
        let sess_count = client
            .count(
                elasticsearch::SESSIONS_INDEX,
                &serde_json::json!({"query": {"match_all": {}}}),
            )
            .await
            .unwrap_or(0);
        (msg_count, sess_count)
    } else {
        (0, 0)
    };

    Ok(EsSyncStatus {
        connected,
        last_full_sync: state.last_full_sync,
        files_tracked: state.files.len(),
        messages_count,
        sessions_count,
    })
}

/// Search messages in ES with full-text query.
///
/// Supports three modes via the `searchMode` filter:
/// - `"smart"` (default): `multi_match` `best_fields`, OR semantics, no fuzziness.
///   Sorts by `_score` first, then `timestamp desc` as tiebreaker.
/// - `"phrase"`: `match_phrase` on `content_text` — connected substring (after
///   IK tokenization) must appear in order.
/// - `"fuzzy"`: `multi_match` with `fuzziness: AUTO` for typo tolerance.
///
/// `sortBy: "time"` forces chronological ordering (for "all messages in date
/// range" style queries — relevance is meaningless then).
#[tauri::command]
pub async fn es_search_messages(
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
    query: String,
    filters: Option<Value>,
    limit: Option<usize>,
) -> Result<Vec<ClaudeMessage>, String> {
    let client = EsClient::new(&endpoint, username.as_deref(), password.as_deref());

    let max_results = limit.unwrap_or(100);

    // Determine search mode (default: smart)
    let search_mode = filters
        .as_ref()
        .and_then(|f| f.get("searchMode"))
        .and_then(Value::as_str)
        .unwrap_or("smart");
    let sort_by = filters
        .as_ref()
        .and_then(|f| f.get("sortBy"))
        .and_then(Value::as_str)
        .unwrap_or("relevance");

    // Build the main text query based on mode.
    //
    // Default ("smart") drops fuzziness — IK tokenization already provides
    // strong recall on Chinese, and fuzziness inflates noise (1746 hits for
    // "搜索" vs ~600 without fuzzy in our test data).
    let main_query = match search_mode {
        "phrase" => serde_json::json!({
            "match_phrase": {
                "content_text": { "query": query, "slop": 1 }
            }
        }),
        "fuzzy" => serde_json::json!({
            "multi_match": {
                "query": query,
                "fields": ["content_text^3", "tool_input^2", "tool_name", "content_text.standard"],
                "type": "best_fields",
                "fuzziness": "AUTO"
            }
        }),
        _ => serde_json::json!({
            "multi_match": {
                "query": query,
                "fields": ["content_text^3", "tool_input^2", "tool_name", "content_text.standard"],
                "type": "best_fields"
            }
        }),
    };

    let mut must = vec![main_query];

    let mut filter_clauses: Vec<Value> = Vec::new();

    // Apply filters if provided
    if let Some(ref f) = filters {
        if let Some(role) = f.get("messageType").and_then(Value::as_str) {
            if role != "all" {
                filter_clauses.push(serde_json::json!({"term": {"role": role}}));
            }
        }
        if let Some(provider) = f.get("provider").and_then(Value::as_str) {
            filter_clauses.push(serde_json::json!({"term": {"provider": provider}}));
        }
        if let Some(project) = f.get("project").and_then(Value::as_str) {
            filter_clauses.push(serde_json::json!({"term": {"project_name": project}}));
        }
        if let Some(device) = f.get("deviceId").and_then(Value::as_str) {
            if !device.is_empty() && device != "all" {
                filter_clauses.push(serde_json::json!({"term": {"device_id": device}}));
            }
        }
        if let Some(date_range) = f.get("dateRange").and_then(Value::as_array) {
            if let [start_val, end_val] = date_range.as_slice() {
                let mut range = serde_json::Map::new();
                if let Some(start) = start_val.as_str() {
                    range.insert("gte".to_string(), Value::String(start.to_string()));
                }
                if let Some(end) = end_val.as_str() {
                    range.insert("lte".to_string(), Value::String(end.to_string()));
                }
                filter_clauses.push(serde_json::json!({"range": {"timestamp": range}}));
            }
        }
        if f.get("hasToolCalls")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            must.push(serde_json::json!({"exists": {"field": "tool_name"}}));
        }
    }

    // Sort policy:
    // - "relevance" (default): _score first, timestamp desc as tiebreaker
    // - "time": pure timestamp desc (when user explicitly wants chronological)
    let sort = if sort_by == "time" {
        serde_json::json!([{"timestamp": "desc"}])
    } else {
        serde_json::json!(["_score", {"timestamp": "desc"}])
    };

    let es_query = serde_json::json!({
        "query": {
            "bool": {
                "must": must,
                "filter": filter_clauses
            }
        },
        "highlight": {
            // Pre-tagged so the frontend can simply render mark-tagged spans.
            "pre_tags": ["<mark>"],
            "post_tags": ["</mark>"],
            "fields": {
                "content_text": {
                    "fragment_size": 220,
                    "number_of_fragments": 3,
                    "no_match_size": 200,
                    "boundary_scanner": "sentence"
                },
                "tool_input": {
                    "fragment_size": 220,
                    "number_of_fragments": 1,
                    "no_match_size": 0
                }
            }
        },
        "size": max_results,
        "sort": sort,
        "_source": ["message_id", "session_id", "provider", "project_name",
                    "role", "message_type", "timestamp", "content_text",
                    "tool_name", "tool_input", "model", "source_file"]
    });

    let result = client
        .search(elasticsearch::MESSAGES_INDEX, &es_query)
        .await?;

    // Convert ES hits to ClaudeMessage format
    let hits = result
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let messages: Vec<ClaudeMessage> = hits
        .iter()
        .filter_map(|hit| {
            let source = hit.get("_source")?;
            let message_id = source.get("message_id").and_then(Value::as_str)?;
            let session_id = source.get("session_id").and_then(Value::as_str)?;
            let timestamp = source.get("timestamp").and_then(Value::as_str)?;
            let message_type = source
                .get("message_type")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let content_text = source
                .get("content_text")
                .and_then(Value::as_str)
                .unwrap_or("");
            let role = source.get("role").and_then(Value::as_str);
            let model = source.get("model").and_then(Value::as_str);
            let provider = source.get("provider").and_then(Value::as_str);
            let project_name = source.get("project_name").and_then(Value::as_str);
            let source_file = source
                .get("source_file")
                .and_then(Value::as_str)
                .unwrap_or("");

            // Pull highlight fragments — content_text first, then tool_input.
            // ES already inserted `<mark>...</mark>` tags via highlight settings;
            // we just join the fragments and let the frontend render them.
            let highlight_html = hit.get("highlight").and_then(|h| {
                let mut parts: Vec<String> = Vec::new();
                if let Some(arr) = h.get("content_text").and_then(Value::as_array) {
                    for v in arr {
                        if let Some(s) = v.as_str() {
                            parts.push(s.to_string());
                        }
                    }
                }
                if let Some(arr) = h.get("tool_input").and_then(Value::as_array) {
                    for v in arr {
                        if let Some(s) = v.as_str() {
                            parts.push(format!("[tool] {s}"));
                        }
                    }
                }
                if parts.is_empty() {
                    None
                } else {
                    Some(parts.join(" … "))
                }
            });

            // BM25 score (None when sort overrides relevance)
            let score = hit.get("_score").and_then(Value::as_f64);

            // Strip device_id prefix from message_id to get original UUID
            let uuid = message_id
                .split_once(':')
                .map(|(_, id)| id)
                .unwrap_or(message_id);

            // Use source_file as session_id for frontend compatibility.
            // The frontend matches search results against ClaudeSession.session_id
            // which is the file_path. Also include actual_session_id in the lookup
            // by trying source_file first, falling back to the UUID session_id.
            let effective_session_id = if source_file.is_empty() {
                session_id.to_string()
            } else {
                source_file.to_string()
            };

            Some(ClaudeMessage {
                uuid: uuid.to_string(),
                session_id: effective_session_id,
                timestamp: timestamp.to_string(),
                message_type: message_type.to_string(),
                content: Some(Value::String(content_text.to_string())),
                project_name: project_name.map(String::from),
                role: role.map(String::from),
                model: model.map(String::from),
                provider: provider.map(String::from),
                search_preview_html: highlight_html,
                search_score: score,
                ..Default::default()
            })
        })
        .collect();

    Ok(messages)
}

/// Load all messages for a session from ES (fallback when local file is missing).
#[tauri::command]
pub async fn es_load_session_messages(
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
    session_id: String,
) -> Result<Vec<ClaudeMessage>, String> {
    let client = EsClient::new(&endpoint, username.as_deref(), password.as_deref());

    // Search by session_id, return all messages sorted by timestamp
    let query = serde_json::json!({
        "query": {
            "term": { "session_id": session_id }
        },
        "size": 10000,
        "sort": [{"timestamp": "asc"}],
        "_source": ["message_id", "session_id", "provider", "project_name",
                    "role", "message_type", "timestamp", "content_text",
                    "tool_name", "model", "source_file", "raw"]
    });

    let result = client.search(elasticsearch::MESSAGES_INDEX, &query).await?;

    let hits = result
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let messages: Vec<ClaudeMessage> = hits
        .iter()
        .filter_map(|hit| {
            let source = hit.get("_source")?;
            let message_id = source.get("message_id").and_then(Value::as_str)?;
            let timestamp = source.get("timestamp").and_then(Value::as_str)?;
            let message_type = source
                .get("message_type")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let role = source.get("role").and_then(Value::as_str);
            let model = source.get("model").and_then(Value::as_str);
            let provider = source.get("provider").and_then(Value::as_str);
            let source_file = source
                .get("source_file")
                .and_then(Value::as_str)
                .unwrap_or("");

            // Use raw content if available for full fidelity, otherwise use content_text
            let content = source
                .get("raw")
                .and_then(|raw| {
                    // Try to extract content from raw message
                    raw.get("message")
                        .and_then(|m| m.get("content"))
                        .cloned()
                        .or_else(|| raw.get("content").cloned())
                })
                .or_else(|| {
                    source
                        .get("content_text")
                        .and_then(Value::as_str)
                        .map(|t| Value::String(t.to_string()))
                });

            let uuid = message_id
                .split_once(':')
                .map(|(_, id)| id)
                .unwrap_or(message_id);

            Some(ClaudeMessage {
                uuid: uuid.to_string(),
                session_id: source_file.to_string(),
                timestamp: timestamp.to_string(),
                message_type: message_type.to_string(),
                content,
                tool_use: source.get("raw").and_then(|r| r.get("toolUse").cloned()),
                tool_use_result: source
                    .get("raw")
                    .and_then(|r| r.get("toolUseResult").cloned()),
                role: role.map(String::from),
                model: model.map(String::from),
                cost_usd: source
                    .get("raw")
                    .and_then(|r| r.get("costUSD"))
                    .and_then(Value::as_f64),
                provider: provider.map(String::from),
                ..Default::default()
            })
        })
        .collect();

    Ok(messages)
}

/// Restore a session from ES to a local JSONL file.
///
/// Reads the `raw` field from each message doc and writes it as a line
/// to the target path, recreating the original JSONL format.
#[tauri::command]
pub async fn es_restore_session(
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
    session_id: String,
    target_path: String,
) -> Result<String, String> {
    // Defense-in-depth: target_path ultimately originates from ES documents
    // (`source_file` written by whichever device synced the session). On a
    // shared cluster that value is not trustworthy, so reject anything that
    // is not a plain absolute path before touching the filesystem.
    if target_path.contains('\0') {
        return Err("Invalid target path: contains null bytes".to_string());
    }
    {
        let target = std::path::Path::new(&target_path);
        if !target.is_absolute() {
            return Err("Invalid target path: must be an absolute path".to_string());
        }
        if target
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("Invalid target path: path traversal not allowed".to_string());
        }
        if target.exists() {
            return Err(format!(
                "Refusing to overwrite existing local file: {target_path}"
            ));
        }
    }

    let client = EsClient::new(&endpoint, username.as_deref(), password.as_deref());

    let query = serde_json::json!({
        "query": {
            "term": { "session_id": session_id }
        },
        "size": 10000,
        "sort": [{"timestamp": "asc"}],
        "_source": ["raw"]
    });

    let result = client.search(elasticsearch::MESSAGES_INDEX, &query).await?;

    let hits = result
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if hits.is_empty() {
        return Err(format!("No messages found for session: {session_id}"));
    }

    // Ensure target directory exists
    let target = std::path::Path::new(&target_path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;
    }

    // Write each raw message as a JSONL line
    let mut lines = Vec::new();
    for hit in &hits {
        if let Some(raw) = hit.get("_source").and_then(|s| s.get("raw")) {
            if let Ok(line) = serde_json::to_string(raw) {
                lines.push(line);
            }
        }
    }

    // Atomic write: temp file + rename so a crash mid-write never leaves a
    // half-restored JSONL behind (CLAUDE.md file-write rule).
    let content = lines.join("\n") + "\n";
    let tmp_path = format!("{target_path}.tmp");
    fs::write(&tmp_path, content).map_err(|e| format!("Failed to write JSONL file: {e}"))?;
    if let Err(e) = fs::rename(&tmp_path, &target_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("Failed to finalize JSONL file: {e}"));
    }

    Ok(format!(
        "Restored {} messages to {}",
        lines.len(),
        target_path
    ))
}

/// List sessions from ES for a given project path.
/// Used to supplement local session list with sessions that only exist in ES.
#[tauri::command]
pub async fn es_list_sessions(
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
    project_path: String,
) -> Result<Vec<crate::models::ClaudeSession>, String> {
    let client = EsClient::new(&endpoint, username.as_deref(), password.as_deref());

    // Extract project directory name from path for matching
    let project_dir = std::path::Path::new(&project_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    // Defensive guard: even after the sync engine stops emitting session
    // docs for subagent files (sync.rs::process_jsonl_file_streaming), older
    // indices may still contain subagent docs from before that fix. The
    // boolean filter below excludes them so the sidebar shows only
    // top-level sessions.
    //
    // `must_not` on `is_subagent: true` (rather than `must` on `false`) is
    // intentional — older docs missing the field altogether should be kept
    // (treated as not-a-subagent), matching the field's default semantics.
    let query = serde_json::json!({
        "query": {
            "bool": {
                "must": [
                    { "term": { "project_name": project_dir } }
                ],
                "must_not": [
                    { "term": { "is_subagent": true } }
                ]
            }
        },
        "size": 1000,
        "sort": [{"last_message_time": "desc"}]
    });

    let result = client.search(elasticsearch::SESSIONS_INDEX, &query).await?;

    let hits = result
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let sessions: Vec<crate::models::ClaudeSession> = hits
        .iter()
        .filter_map(|hit| {
            let source = hit.get("_source")?;
            let session_id = source.get("session_id").and_then(Value::as_str)?;
            let source_file = source
                .get("source_file")
                .and_then(Value::as_str)
                .unwrap_or("");
            let first_time = source
                .get("first_message_time")
                .and_then(Value::as_str)
                .unwrap_or("");
            let last_time = source
                .get("last_message_time")
                .and_then(Value::as_str)
                .unwrap_or("");
            let msg_count = source
                .get("message_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let summary = source
                .get("summary")
                .and_then(Value::as_str)
                .map(String::from);
            let provider = source
                .get("provider")
                .and_then(Value::as_str)
                .map(String::from);
            let entrypoint = source
                .get("entrypoint")
                .and_then(Value::as_str)
                .map(String::from);
            let has_tool_use = source
                .get("has_tool_use")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            Some(crate::models::ClaudeSession {
                session_id: source_file.to_string(),
                actual_session_id: session_id.to_string(),
                file_path: source_file.to_string(),
                project_name: project_dir.to_string(),
                message_count: msg_count,
                first_message_time: first_time.to_string(),
                last_message_time: last_time.to_string(),
                last_modified: last_time.to_string(),
                has_tool_use,
                has_errors: false,
                summary,
                is_renamed: false,
                provider,
                storage_type: Some("elasticsearch".to_string()),
                entrypoint,
            })
        })
        .collect();

    Ok(sessions)
}

/// List all unique projects from ES (for supplementing local project list).
#[tauri::command]
pub async fn es_list_projects(
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
) -> Result<Vec<crate::models::ClaudeProject>, String> {
    let client = EsClient::new(&endpoint, username.as_deref(), password.as_deref());

    let query = serde_json::json!({
        "size": 0,
        "query": {
            "bool": {
                "must_not": [
                    { "term": { "project_name": "subagents" } },
                    { "wildcard": { "source_file": "*/subagents/*" } }
                ]
            }
        },
        "aggs": {
            "projects": {
                "terms": { "field": "project_name", "size": 200 },
                "aggs": {
                    "provider": { "terms": { "field": "provider", "size": 1 } },
                    "project_path": { "terms": { "field": "project_path", "size": 1 } },
                    "last_time": { "max": { "field": "last_message_time" } },
                    "session_count": { "cardinality": { "field": "session_id" } },
                    "msg_count": { "value_count": { "field": "session_id" } }
                }
            }
        }
    });

    let result = client.search(elasticsearch::SESSIONS_INDEX, &query).await?;

    let buckets = result
        .get("aggregations")
        .and_then(|a| a.get("projects"))
        .and_then(|p| p.get("buckets"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let projects: Vec<crate::models::ClaudeProject> = buckets
        .iter()
        .filter_map(|bucket| {
            let name = bucket.get("key").and_then(Value::as_str)?;
            let provider = bucket
                .get("provider")
                .and_then(|p| p.get("buckets"))
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|b| b.get("key"))
                .and_then(Value::as_str)
                .unwrap_or("claude");
            let project_path = bucket
                .get("project_path")
                .and_then(|p| p.get("buckets"))
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|b| b.get("key"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let last_modified = bucket
                .get("last_time")
                .and_then(|t| t.get("value_as_string"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let session_count = bucket
                .get("session_count")
                .and_then(|s| s.get("value"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;

            Some(crate::models::ClaudeProject {
                name: name.to_string(),
                path: format!("es://{name}"),
                actual_path: project_path.to_string(),
                session_count,
                message_count: 0,
                last_modified,
                git_info: None,
                provider: Some(provider.to_string()),
                storage_type: Some("elasticsearch".to_string()),
                custom_directory_label: None,
            })
        })
        .collect();

    Ok(projects)
}

/// Get aggregation statistics from ES for the dashboard.
#[tauri::command]
pub async fn es_get_stats(
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
) -> Result<Value, String> {
    let client = EsClient::new(&endpoint, username.as_deref(), password.as_deref());

    let query = serde_json::json!({
        "size": 0,
        "aggs": {
            "by_provider": {
                "terms": { "field": "provider", "size": 20 }
            },
            "by_project": {
                "terms": { "field": "project_name", "size": 50, "order": { "_count": "desc" } }
            },
            "by_model": {
                "terms": { "field": "model", "size": 20 }
            },
            "by_role": {
                "terms": { "field": "role", "size": 10 }
            },
            "by_device": {
                "terms": { "field": "device_id", "size": 50 }
            },
            "total_tokens_in": {
                "sum": { "field": "token_input" }
            },
            "total_tokens_out": {
                "sum": { "field": "token_output" }
            },
            "total_cost": {
                "sum": { "field": "cost_usd" }
            },
            "messages_over_time": {
                "date_histogram": {
                    "field": "timestamp",
                    "calendar_interval": "day"
                },
                "aggs": {
                    "daily_tokens_in": { "sum": { "field": "token_input" } },
                    "daily_tokens_out": { "sum": { "field": "token_output" } }
                }
            },
            "time_range": {
                "stats": { "field": "timestamp" }
            }
        }
    });

    let result = client.search(elasticsearch::MESSAGES_INDEX, &query).await?;

    // Extract and reshape aggregation results
    let aggs = result.get("aggregations").cloned().unwrap_or(Value::Null);
    let total_hits = result
        .get("hits")
        .and_then(|h| h.get("total"))
        .and_then(|t| t.get("value"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    Ok(serde_json::json!({
        "total_messages": total_hits,
        "aggregations": aggs
    }))
}

/// List all unique devices that have synced data into ES.
/// Returns each device's id, message count, and most recent timestamp so the UI
/// can render a multi-device picker.
#[tauri::command]
pub async fn es_list_devices(
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
) -> Result<Value, String> {
    let client = EsClient::new(&endpoint, username.as_deref(), password.as_deref());

    let query = serde_json::json!({
        "size": 0,
        "aggs": {
            "devices": {
                "terms": { "field": "device_id", "size": 100 },
                "aggs": {
                    "last_seen": { "max": { "field": "timestamp" } },
                    "session_count": { "cardinality": { "field": "session_id" } }
                }
            }
        }
    });

    let result = client.search(elasticsearch::MESSAGES_INDEX, &query).await?;
    let buckets = result
        .get("aggregations")
        .and_then(|a| a.get("devices"))
        .and_then(|d| d.get("buckets"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let devices: Vec<Value> = buckets
        .iter()
        .filter_map(|b| {
            let id = b.get("key").and_then(Value::as_str)?;
            let count = b.get("doc_count").and_then(Value::as_u64).unwrap_or(0);
            let last_seen = b
                .get("last_seen")
                .and_then(|v| v.get("value_as_string"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let sessions = b
                .get("session_count")
                .and_then(|v| v.get("value"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Some(serde_json::json!({
                "device_id": id,
                "message_count": count,
                "session_count": sessions,
                "last_seen": last_seen
            }))
        })
        .collect();

    Ok(serde_json::json!({ "devices": devices }))
}

fn get_es_settings_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude-history-viewer").join("es-settings.json")
}

/// Save ES credentials so the file watcher can read them.
fn save_es_settings(endpoint: &str, username: Option<&str>, password: Option<&str>) {
    let path = get_es_settings_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let settings = serde_json::json!({
        "endpoint": endpoint,
        "username": username,
        "password": password
    });
    if let Ok(content) = serde_json::to_string_pretty(&settings) {
        let _ = fs::write(&path, content);
        // Credentials are stored in plain text — restrict to the owning user.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
    }
}

/// Save sync state (`endpoint` + `device_id`) externally from commands module.
fn save_sync_state_external(state: &SyncState) {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let path = home
        .join(".claude-history-viewer")
        .join("es-sync-state.json");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string_pretty(state) {
        let _ = fs::write(&path, content);
    }
}
