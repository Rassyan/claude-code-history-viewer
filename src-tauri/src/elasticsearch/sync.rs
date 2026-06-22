use super::client::EsClient;
use super::models::{
    messages_index_body, sessions_index_body, EsMessageDoc, EsSessionDoc, MESSAGES_INDEX,
    SESSIONS_INDEX,
};
use crate::utils::find_line_ranges;
use chrono::Utc;
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use walkdir::WalkDir;

const SYNC_STATE_FILE: &str = "es-sync-state.json";
const BULK_BATCH_SIZE: usize = 500;

/// Global cancellation flag for the currently running full sync.
/// Set via `request_cancel_sync()` from the UI to abort the operation.
/// The sync loop checks this between files and flushes pending batches before
/// returning early.
static SYNC_CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Request cancellation of the currently running full sync.
pub fn request_cancel_sync() {
    SYNC_CANCEL_REQUESTED.store(true, AtomicOrdering::SeqCst);
}

/// Check whether cancellation was requested (also clears the flag if so).
fn check_and_clear_cancel() -> bool {
    SYNC_CANCEL_REQUESTED.swap(false, AtomicOrdering::SeqCst)
}

/// Reset the cancel flag (called at the start of every sync).
fn reset_cancel() {
    SYNC_CANCEL_REQUESTED.store(false, AtomicOrdering::SeqCst);
}

/// Streaming progress update emitted during a sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncProgress {
    /// One of: "starting", "scanning", "processing", "flushing", "complete", "cancelled", "error"
    pub phase: String,
    pub files_processed: usize,
    pub total_files: usize,
    pub messages_indexed: usize,
    pub sessions_indexed: usize,
    /// Currently being processed file (relative-ish, for display). Empty during non-processing phases.
    pub current_file: String,
}

/// Progress reporter — called between batches and major checkpoints.
/// Implementations forward to a Tauri event emitter or no-op.
pub trait ProgressReporter: Send + Sync {
    fn report(&self, progress: &SyncProgress);
}

/// No-op reporter for tests / non-UI invocations.
pub struct NoopReporter;
impl ProgressReporter for NoopReporter {
    fn report(&self, _: &SyncProgress) {}
}

/// Sync progress statistics returned after a sync operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStats {
    pub files_processed: usize,
    pub messages_indexed: usize,
    pub sessions_indexed: usize,
    pub errors: Vec<String>,
    pub duration_ms: u64,
}

/// Per-file sync tracking state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSyncState {
    pub mtime: u64,
    pub byte_offset: u64,
    pub message_count: usize,
    pub last_synced: String,
}

/// Persistent sync state for all tracked files.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncState {
    pub version: u32,
    pub device_id: String,
    pub es_endpoint: String,
    pub last_full_sync: Option<String>,
    pub files: HashMap<String, FileSyncState>,
}

/// Run a full sync: scan all provider JSONL files and index into ES.
#[allow(unsafe_code)]
pub async fn full_sync(
    client: &EsClient,
    device_id: &str,
    custom_claude_paths: &[String],
    reporter: &dyn ProgressReporter,
) -> Result<SyncStats, String> {
    let start = std::time::Instant::now();
    let mut stats = SyncStats {
        files_processed: 0,
        messages_indexed: 0,
        sessions_indexed: 0,
        errors: Vec::new(),
        duration_ms: 0,
    };

    // Ensure indices exist
    ensure_indices(client).await?;

    // Reset cancel flag at the start
    reset_cancel();

    // Collect all JSONL files from all providers
    reporter.report(&SyncProgress {
        phase: "scanning".to_string(),
        files_processed: 0,
        total_files: 0,
        messages_indexed: 0,
        sessions_indexed: 0,
        current_file: String::new(),
    });
    let files = collect_all_jsonl_files(custom_claude_paths);
    let total_files = files.len();
    log::info!("ES full sync: found {total_files} JSONL files to process");
    reporter.report(&SyncProgress {
        phase: "starting".to_string(),
        files_processed: 0,
        total_files,
        messages_indexed: 0,
        sessions_indexed: 0,
        current_file: String::new(),
    });

    let mut state = load_sync_state();
    state.device_id = device_id.to_string();

    let mut message_batch: Vec<(String, Value)> = Vec::with_capacity(BULK_BATCH_SIZE);
    let mut session_batch: Vec<(String, Value)> = Vec::with_capacity(100);
    let mut cancelled = false;

    for (file_path, provider) in &files {
        // Honor cancellation request between files
        if check_and_clear_cancel() {
            log::info!("ES full sync: cancellation requested, stopping");
            cancelled = true;
            break;
        }

        let file_path_str = file_path.to_string_lossy().to_string();

        // Report progress every 5 files to keep frontend updated without spamming
        if stats.files_processed % 5 == 0 {
            // Show only the file basename to keep events small
            let display_name = file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            reporter.report(&SyncProgress {
                phase: "processing".to_string(),
                files_processed: stats.files_processed,
                total_files,
                messages_indexed: stats.messages_indexed,
                sessions_indexed: stats.sessions_indexed,
                current_file: display_name,
            });
        }

        // Check if file needs syncing
        let current_mtime = get_file_mtime(file_path);
        let current_size = fs::metadata(file_path).map(|m| m.len()).unwrap_or(0);

        if let Some(existing) = state.files.get(&file_path_str) {
            if existing.mtime == current_mtime && existing.byte_offset == current_size {
                continue; // Already synced, skip
            }
        }

        // Determine offset:
        // - If file size shrunk vs last sync, file was truncated/rewritten — restart from 0
        // - Otherwise resume from last byte_offset
        let last_offset = state
            .files
            .get(&file_path_str)
            .map(|s| s.byte_offset)
            .unwrap_or(0);
        let offset = if current_size < last_offset {
            log::warn!(
                "ES sync: file {} shrunk ({last_offset} -> {current_size}), restarting from 0",
                file_path.display()
            );
            0
        } else {
            last_offset
        };

        // Stream-process the file: messages are flushed in batches of
        // BULK_BATCH_SIZE as they're parsed, so peak memory stays bounded
        // regardless of file size.
        let mut parser = match StreamingJsonlParser::open(file_path, provider, device_id, offset) {
            Ok(p) => p,
            Err(e) => {
                stats.errors.push(format!("{}: {e}", file_path.display()));
                continue;
            }
        };

        let mut msg_count = 0usize;
        loop {
            let appended = parser.next_chunk(&mut message_batch, BULK_BATCH_SIZE);
            msg_count += appended;
            if message_batch.len() >= BULK_BATCH_SIZE {
                match client.bulk_index(MESSAGES_INDEX, &message_batch).await {
                    Ok(n) => stats.messages_indexed += n,
                    Err(e) => stats.errors.push(format!("Bulk error: {e}")),
                }
                message_batch.clear();
            }
            if parser.is_done() {
                break;
            }
        }

        let session_doc = parser.finish();

        // Queue session doc
        if let Some(session) = session_doc {
            // Session doc id: use session_id alone to dedupe across devices.
            // Multiple devices syncing the same session overwrite each other's
            // metadata (last write wins) — acceptable since session metadata
            // is roughly stable and we don't show duplicates.
            let id = session.session_id.clone();
            let doc = serde_json::to_value(&session).unwrap_or_default();
            session_batch.push((id, doc));
        }

        // Update file sync state
        state.files.insert(
            file_path_str,
            FileSyncState {
                mtime: current_mtime,
                byte_offset: current_size,
                message_count: msg_count,
                last_synced: Utc::now().to_rfc3339(),
            },
        );

        stats.files_processed += 1;
    }

    // Flush remaining batches
    if !message_batch.is_empty() {
        match client.bulk_index(MESSAGES_INDEX, &message_batch).await {
            Ok(n) => stats.messages_indexed += n,
            Err(e) => stats.errors.push(format!("Final bulk error: {e}")),
        }
    }

    if !session_batch.is_empty() {
        match client.bulk_index(SESSIONS_INDEX, &session_batch).await {
            Ok(n) => stats.sessions_indexed += n,
            Err(e) => stats.errors.push(format!("Session bulk error: {e}")),
        }
    }

    // Save sync state (preserves byte_offset progress for files already synced
    // before cancellation — a subsequent sync resumes seamlessly).
    if !cancelled {
        state.last_full_sync = Some(Utc::now().to_rfc3339());
    }
    save_sync_state(&state);

    stats.duration_ms = start.elapsed().as_millis() as u64;
    if cancelled {
        stats.errors.push("Sync cancelled by user".to_string());
        log::info!(
            "ES full sync cancelled: {} files, {} messages, {} sessions, {}ms",
            stats.files_processed,
            stats.messages_indexed,
            stats.sessions_indexed,
            stats.duration_ms
        );
    } else {
        log::info!(
            "ES full sync complete: {} files, {} messages, {} sessions, {} errors, {}ms",
            stats.files_processed,
            stats.messages_indexed,
            stats.sessions_indexed,
            stats.errors.len(),
            stats.duration_ms
        );
    }

    // Final progress emit
    reporter.report(&SyncProgress {
        phase: if cancelled {
            "cancelled".to_string()
        } else {
            "complete".to_string()
        },
        files_processed: stats.files_processed,
        total_files,
        messages_indexed: stats.messages_indexed,
        sessions_indexed: stats.sessions_indexed,
        current_file: String::new(),
    });

    Ok(stats)
}

/// Ensure the required ES indices exist, creating them if needed.
async fn ensure_indices(client: &EsClient) -> Result<(), String> {
    if !client.index_exists(MESSAGES_INDEX).await? {
        log::info!("Creating ES index: {MESSAGES_INDEX}");
        client
            .create_index(MESSAGES_INDEX, &messages_index_body())
            .await?;
    }
    if !client.index_exists(SESSIONS_INDEX).await? {
        log::info!("Creating ES index: {SESSIONS_INDEX}");
        client
            .create_index(SESSIONS_INDEX, &sessions_index_body())
            .await?;
    }
    Ok(())
}

/// Collect all JSONL file paths from known providers + user-configured paths.
fn collect_all_jsonl_files(custom_claude_paths: &[String]) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return files,
    };

    // Claude Internal
    let claude_internal = home.join(".claude-internal").join("projects");
    if claude_internal.is_dir() {
        collect_jsonl_from_dir(&claude_internal, "claude", &mut files);
    }

    // Claude (standard)
    let claude_std = home.join(".claude").join("projects");
    if claude_std.is_dir() {
        collect_jsonl_from_dir(&claude_std, "claude", &mut files);
    }

    // CodeBuddy
    let codebuddy = home.join(".codebuddy").join("projects");
    if codebuddy.is_dir() {
        collect_jsonl_from_dir(&codebuddy, "codebuddy", &mut files);
    }

    // User-configured custom Claude paths (e.g., ~/.claude-personal)
    for custom in custom_claude_paths {
        let custom_dir = PathBuf::from(custom).join("projects");
        if custom_dir.is_dir() {
            collect_jsonl_from_dir(&custom_dir, "claude", &mut files);
        }
    }

    files
}

fn collect_jsonl_from_dir(dir: &Path, provider: &str, out: &mut Vec<(PathBuf, String)>) {
    for entry in WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
    {
        out.push((entry.path().to_path_buf(), provider.to_string()));
    }
}

/// Accumulator for session-level metadata observed while streaming through
/// a JSONL file. Holds only small fields (strings, counters, flags), not the
/// per-message documents themselves — so a parser can iterate a 300MB file
/// without retaining all messages in memory.
#[derive(Default)]
struct SessionAccumulator {
    session_id: String,
    first_time: String,
    last_time: String,
    message_count: usize,
    has_tool_use: bool,
    summary: Option<String>,
    entrypoint: Option<String>,
    git_branch: Option<String>,
}

impl SessionAccumulator {
    fn observe(&mut self, doc: &EsMessageDoc) {
        if self.session_id.is_empty() {
            self.session_id.clone_from(&doc.session_id);
        }
        if self.first_time.is_empty() {
            self.first_time.clone_from(&doc.timestamp);
        }
        self.last_time.clone_from(&doc.timestamp);
        self.message_count += 1;

        if doc.tool_name.is_some() {
            self.has_tool_use = true;
        }
        if self.entrypoint.is_none() {
            self.entrypoint.clone_from(&doc.entrypoint);
        }
        if self.git_branch.is_none() {
            self.git_branch.clone_from(&doc.git_branch);
        }

        // Extract summary from first user message
        if self.summary.is_none()
            && doc.role.as_deref() == Some("user")
            && !doc.content_text.is_empty()
        {
            let text = &doc.content_text;
            if !text.starts_with('<') {
                self.summary = Some(if text.chars().count() > 100 {
                    format!("{}...", text.chars().take(100).collect::<String>())
                } else {
                    text.clone()
                });
            }
        }
    }

    /// Build the session doc, applying the same skip rules as before:
    /// require ≥1 message, a non-empty `session_id`, and skip subagent files.
    #[allow(clippy::too_many_arguments)]
    fn into_session_doc(
        self,
        provider: &str,
        device_id: &str,
        project_path: &str,
        project_name: &str,
        source_file: &str,
        is_subagent: bool,
        now: &str,
    ) -> Option<EsSessionDoc> {
        // Subagent files (`<project>/<session_id>/subagents/agent-xxx.jsonl`)
        // are not standalone sessions — they are sub-runs spawned by a parent
        // session and the parent's main `.jsonl` is the canonical session
        // document. Indexing each subagent file as its own EsSessionDoc was
        // inflating the sidebar count (one real local session showed 19+ cloud
        // sessions, all subagents) and breaking `mergeCloudSessions` dedup
        // because subagent sessions never have a local counterpart by file.
        //
        // We still index the subagent's *messages* (in the streaming pass) so
        // global search can find them; we just skip emitting a session doc.
        if self.message_count == 0 || self.session_id.is_empty() || is_subagent {
            return None;
        }
        Some(EsSessionDoc {
            session_id: self.session_id,
            provider: provider.to_string(),
            device_id: device_id.to_string(),
            project_path: project_path.to_string(),
            project_name: project_name.to_string(),
            source_file: source_file.to_string(),
            first_message_time: self.first_time,
            last_message_time: self.last_time,
            message_count: self.message_count,
            summary: self.summary,
            entrypoint: self.entrypoint,
            git_branch: self.git_branch,
            has_tool_use: self.has_tool_use,
            is_subagent,
            synced_at: now.to_string(),
        })
    }
}

/// Streaming JSONL processor: a stateful iterator over a memory-mapped
/// JSONL file that yields parsed message documents in chunks of up to
/// `BULK_BATCH_SIZE` items at a time.
///
/// Why this shape: the natural alternative — a callback-based API like
/// `process_streaming(|msg| async { flush() })` — runs into Rust borrow
/// checker grief because the callback's returned future would need to
/// hold mutable references to the calling sync's `stats` / `batch` /
/// `client` *across* the parser's `await` point. Yielding chunks back
/// to the caller keeps all flush-related state in the caller's stack
/// frame and avoids closures-returning-futures entirely.
///
/// Memory profile: at any given time this struct holds the mmap
/// (kernel-paged, not RSS-counted in the usual sense) plus the
/// `Vec<(usize, usize)>` of line ranges (~16 bytes per line) plus
/// the current chunk being built (≤`BULK_BATCH_SIZE` items). For a
/// 344MB file with ~50k lines that's ~800KB of ranges + chunk —
/// orders of magnitude below the previous "all messages at once"
/// approach which inflated to ~3GB RSS.
struct StreamingJsonlParser {
    // Owned mmap keeps the underlying bytes alive for the lifetime of the
    // parser. We index into it by absolute byte offsets rather than holding
    // a `&[u8]` reference, which would create a self-referential struct.
    mmap: Mmap,
    // Absolute byte ranges of each line in the *full* mmap. Built once at
    // construction time by offsetting the relative ranges from the
    // `start_offset`-suffix slice.
    ranges: Vec<(usize, usize)>,
    cursor: usize,
    provider: String,
    device_id: String,
    project_name: String,
    project_path: String,
    source_file: String,
    is_subagent: bool,
    now: String,
    acc: SessionAccumulator,
}

impl StreamingJsonlParser {
    #[allow(unsafe_code)]
    fn open(
        file_path: &Path,
        provider: &str,
        device_id: &str,
        start_offset: u64,
    ) -> Result<Self, String> {
        let file = File::open(file_path).map_err(|e| format!("Open failed: {e}"))?;
        // SAFETY: Read-only memory mapping
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| format!("Mmap failed: {e}"))?;

        let ranges = if (start_offset as usize) >= mmap.len() {
            Vec::new()
        } else {
            let data = &mmap[start_offset as usize..];
            // Convert relative ranges (from `data`) into absolute mmap offsets
            // so we can drop the borrow of `mmap` and index by integer later.
            find_line_ranges(data)
                .into_iter()
                .map(|(s, e)| (s + start_offset as usize, e + start_offset as usize))
                .collect()
        };

        let project_name = extract_project_name_from_path(file_path);
        let project_path = extract_project_path(&project_name);
        let source_file = file_path.to_string_lossy().to_string();
        let is_subagent = is_subagent_file(file_path);

        Ok(Self {
            mmap,
            ranges,
            cursor: 0,
            provider: provider.to_string(),
            device_id: device_id.to_string(),
            project_name,
            project_path,
            source_file,
            is_subagent,
            now: Utc::now().to_rfc3339(),
            acc: SessionAccumulator::default(),
        })
    }

    /// Fill `out` with up to `max` more `(message_id, json_value)` pairs.
    /// Returns the number of items appended. Returns 0 when fully drained.
    fn next_chunk(&mut self, out: &mut Vec<(String, Value)>, max: usize) -> usize {
        let start_len = out.len();
        while self.cursor < self.ranges.len() && (out.len() - start_len) < max {
            let (start, end) = self.ranges[self.cursor];
            self.cursor += 1;

            let line = &self.mmap[start..end];
            let mut buf = line.to_vec();
            let val: Value = match simd_json::from_slice(&mut buf) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let msg_doc = match self.provider.as_str() {
                "codebuddy" => parse_codebuddy_line(
                    &val,
                    &self.device_id,
                    &self.project_path,
                    &self.project_name,
                    &self.source_file,
                    self.is_subagent,
                    &self.now,
                ),
                _ => parse_claude_line(
                    &val,
                    &self.device_id,
                    &self.project_path,
                    &self.project_name,
                    &self.source_file,
                    self.is_subagent,
                    &self.now,
                ),
            };
            // Drop the line-level Value early; it's typically the largest
            // per-line allocation.
            drop(val);
            drop(buf);

            let Some(doc) = msg_doc else { continue };
            self.acc.observe(&doc);

            let id = doc.message_id.clone();
            let json = serde_json::to_value(&doc).unwrap_or_default();
            drop(doc);
            out.push((id, json));
        }
        out.len() - start_len
    }

    fn is_done(&self) -> bool {
        self.cursor >= self.ranges.len()
    }

    /// Consume the parser and return the accumulated session doc.
    fn finish(self) -> Option<EsSessionDoc> {
        self.acc.into_session_doc(
            &self.provider,
            &self.device_id,
            &self.project_path,
            &self.project_name,
            &self.source_file,
            self.is_subagent,
            &self.now,
        )
    }
}

/// Parse a Claude Internal JSONL line into an `EsMessageDoc`.
fn parse_claude_line(
    val: &Value,
    device_id: &str,
    project_path: &str,
    project_name: &str,
    source_file: &str,
    is_subagent: bool,
    synced_at: &str,
) -> Option<EsMessageDoc> {
    let message_type = val.get("type").and_then(Value::as_str)?;

    // Skip non-message types
    match message_type {
        "user" | "assistant" | "system" => {}
        _ => return None,
    }

    // Skip meta messages
    if val.get("isMeta").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }

    let uuid = val.get("uuid").and_then(Value::as_str).unwrap_or_default();
    let session_id = val
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let timestamp = val
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if uuid.is_empty() || session_id.is_empty() {
        return None;
    }

    // Extract role from message.role or from type
    let role = val
        .get("message")
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        .unwrap_or(message_type);

    // Extract content text
    let content_text = extract_claude_content_text(val);

    // Extract tool name + serialized input parameters
    let tool_name = extract_tool_name(val);
    let tool_input = extract_tool_input(val);

    // Extract model
    let model = val
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
        .map(String::from);

    // Extract token usage
    let (token_input, token_output) = extract_token_usage(val);

    // Extract cost
    let cost_usd = val.get("costUSD").and_then(Value::as_f64);

    // Extract metadata
    let git_branch = val
        .get("gitBranch")
        .and_then(Value::as_str)
        .map(String::from);
    let entrypoint = val
        .get("entrypoint")
        .and_then(Value::as_str)
        .map(String::from);

    Some(EsMessageDoc {
        message_id: format!("{device_id}:{uuid}"),
        session_id: session_id.to_string(),
        provider: "claude".to_string(),
        device_id: device_id.to_string(),
        project_path: project_path.to_string(),
        project_name: project_name.to_string(),
        source_file: source_file.to_string(),
        role: Some(role.to_string()),
        message_type: message_type.to_string(),
        timestamp: timestamp.to_string(),
        content_text,
        tool_name,
        tool_input,
        model,
        git_branch,
        entrypoint,
        cost_usd,
        token_input,
        token_output,
        is_subagent,
        raw: val.clone(),
        synced_at: synced_at.to_string(),
    })
}

/// Parse a `CodeBuddy` JSONL line into an `EsMessageDoc`.
fn parse_codebuddy_line(
    val: &Value,
    device_id: &str,
    project_path: &str,
    project_name: &str,
    source_file: &str,
    is_subagent: bool,
    synced_at: &str,
) -> Option<EsMessageDoc> {
    let line_type = val.get("type").and_then(Value::as_str)?;

    // Only process message types that have content
    match line_type {
        "message" | "function_call" | "function_call_result" => {}
        _ => return None,
    }

    // Skip providerData.skipRun system messages
    if val
        .get("providerData")
        .and_then(|pd| pd.get("skipRun"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }

    let id = val.get("id").and_then(Value::as_str).unwrap_or_default();
    let session_id = val
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if id.is_empty() && session_id.is_empty() {
        return None;
    }

    // Convert numeric timestamp to ISO 8601
    let timestamp = match val.get("timestamp") {
        Some(Value::Number(n)) => {
            if let Some(ms) = n.as_i64() {
                chrono::DateTime::from_timestamp_millis(ms)
                    .unwrap_or_else(Utc::now)
                    .to_rfc3339()
            } else {
                Utc::now().to_rfc3339()
            }
        }
        Some(Value::String(s)) => s.clone(),
        _ => Utc::now().to_rfc3339(),
    };

    let role = val.get("role").and_then(Value::as_str);
    let message_type = match line_type {
        "message" => role.unwrap_or("user"),
        "function_call" => "assistant",
        "function_call_result" => "tool_result",
        _ => "unknown",
    };

    // Extract content text
    let content_text = extract_codebuddy_content_text(val, line_type);

    // Extract tool name + arguments for function_call
    let (tool_name, tool_input) = if line_type == "function_call" {
        let name = val.get("name").and_then(Value::as_str).map(String::from);
        let args = val
            .get("arguments")
            .or_else(|| val.get("input"))
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => serde_json::to_string(other).unwrap_or_default(),
            })
            .map(|s| s.chars().take(5000).collect::<String>())
            .filter(|s| !s.is_empty());
        (name, args)
    } else {
        (None, None)
    };

    let message_id = if id.is_empty() {
        format!("{device_id}:codebuddy-{}", uuid::Uuid::new_v4())
    } else {
        format!("{device_id}:{id}")
    };

    Some(EsMessageDoc {
        message_id,
        session_id: session_id.to_string(),
        provider: "codebuddy".to_string(),
        device_id: device_id.to_string(),
        project_path: project_path.to_string(),
        project_name: project_name.to_string(),
        source_file: source_file.to_string(),
        role: role.map(String::from),
        message_type: message_type.to_string(),
        timestamp,
        content_text,
        tool_name,
        tool_input,
        model: None,
        git_branch: None,
        entrypoint: None,
        cost_usd: None,
        token_input: None,
        token_output: None,
        is_subagent,
        raw: val.clone(),
        synced_at: synced_at.to_string(),
    })
}

// ============================================================================
// Content text extraction
// ============================================================================

/// Extract searchable text from a Claude Internal message.
fn extract_claude_content_text(val: &Value) -> String {
    let mut texts = Vec::new();

    // Try message.content (standard assistant/user messages)
    if let Some(content) = val.get("message").and_then(|m| m.get("content")) {
        extract_text_from_content(content, &mut texts);
    }

    // Try top-level content (system messages)
    if texts.is_empty() {
        if let Some(content) = val.get("content") {
            extract_text_from_content(content, &mut texts);
        }
    }

    texts.join("\n").chars().take(50000).collect()
}

/// Extract searchable text from a `CodeBuddy` message.
fn extract_codebuddy_content_text(val: &Value, line_type: &str) -> String {
    let mut texts = Vec::new();

    match line_type {
        "message" => {
            if let Some(content) = val.get("content") {
                extract_text_from_content(content, &mut texts);
            }
        }
        "function_call" => {
            if let Some(name) = val.get("name").and_then(Value::as_str) {
                texts.push(name.to_string());
            }
        }
        "function_call_result" => {
            if let Some(content) = val.get("content") {
                extract_text_from_content(content, &mut texts);
            } else if let Some(content) = val.get("message").and_then(|m| m.get("content")) {
                extract_text_from_content(content, &mut texts);
            }
        }
        _ => {}
    }

    texts.join("\n").chars().take(50000).collect()
}

/// Recursively extract text from a content value (string or array of content items).
fn extract_text_from_content(content: &Value, out: &mut Vec<String>) {
    match content {
        Value::String(s) => {
            if !s.is_empty() {
                out.push(s.clone());
            }
        }
        Value::Array(arr) => {
            for item in arr {
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                match item_type {
                    "text" | "input_text" | "output_text" => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                out.push(text.to_string());
                            }
                        }
                    }
                    "thinking" => {
                        if let Some(text) = item.get("thinking").and_then(Value::as_str) {
                            if !text.is_empty() {
                                out.push(text.to_string());
                            }
                        }
                    }
                    "tool_use" => {
                        if let Some(name) = item.get("name").and_then(Value::as_str) {
                            out.push(name.to_string());
                        }
                    }
                    "tool_result" => {
                        if let Some(text) = item.get("content").and_then(Value::as_str) {
                            out.push(text.chars().take(20000).collect());
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Extract tool name from Claude message content.
fn extract_tool_name(val: &Value) -> Option<String> {
    let content = val.get("message")?.get("content")?.as_array()?;
    for item in content {
        if item.get("type").and_then(Value::as_str) == Some("tool_use") {
            return item.get("name").and_then(Value::as_str).map(String::from);
        }
    }
    None
}

/// Extract serialized `tool_use` `input` parameters as a single searchable string.
///
/// Concatenates input JSON for all `tool_use` items in this message. Truncated
/// per-tool to keep large inputs (e.g., long file contents in Edit) bounded.
/// Multiple `tool_use` blocks in one message are joined by " | ".
fn extract_tool_input(val: &Value) -> Option<String> {
    let content = val.get("message")?.get("content")?.as_array()?;
    let mut parts: Vec<String> = Vec::new();
    for item in content {
        if item.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(input) = item.get("input") else {
            continue;
        };
        // For object input, render as "key=value key2=value2" so individual
        // params (command, pattern, file_path) are searchable as keywords.
        let rendered = match input {
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| match v {
                    Value::String(s) => format!("{k}={s}"),
                    other => format!("{k}={other}"),
                })
                .collect::<Vec<_>>()
                .join(" "),
            Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        // Cap per-tool to 5000 chars
        let capped: String = rendered.chars().take(5000).collect();
        if !capped.is_empty() {
            parts.push(capped);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" | "))
    }
}

/// Extract token usage from Claude message.
fn extract_token_usage(val: &Value) -> (Option<u32>, Option<u32>) {
    let usage = match val.get("message").and_then(|m| m.get("usage")) {
        Some(u) => u,
        None => return (None, None),
    };

    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    (input, output)
}

// ============================================================================
// Sync state persistence
// ============================================================================

fn get_sync_state_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude-history-viewer").join(SYNC_STATE_FILE)
}

pub fn load_sync_state() -> SyncState {
    let path = get_sync_state_path();
    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(state) = serde_json::from_str::<SyncState>(&content) {
            return state;
        }
    }
    SyncState {
        version: 1,
        ..Default::default()
    }
}

fn save_sync_state(state: &SyncState) {
    let path = get_sync_state_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string_pretty(state) {
        let _ = fs::write(&path, content);
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn get_file_mtime(path: &Path) -> u64 {
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Convert a project directory name to a likely project path.
/// e.g., "-Users-alice-IdeaProjects-demo" -> "/Users/alice/IdeaProjects/demo"
/// e.g., "Users-alice-IdeaProjects-demo" -> "/Users/alice/IdeaProjects/demo"
fn extract_project_path(dir_name: &str) -> String {
    let name = dir_name.strip_prefix('-').unwrap_or(dir_name);
    format!("/{}", name.replace('-', "/"))
}

/// Extract the project name from a JSONL file path.
///
/// Walks up from the file looking for the directory immediately under `projects/`.
/// This correctly handles:
/// - `<root>/projects/<project>/<session>.jsonl` -> `<project>`
/// - `<root>/projects/<project>/<session_id>/subagents/agent-xxx.jsonl` -> `<project>`
///   (NOT `subagents` or `<session_id>`)
///
/// Returns "unknown" if no `projects` ancestor is found.
fn extract_project_name_from_path(file_path: &Path) -> String {
    let components: Vec<_> = file_path.components().collect();
    for (i, comp) in components.iter().enumerate() {
        if comp.as_os_str() == "projects" {
            // The component immediately after "projects" is the project name
            if let Some(next) = components.get(i + 1) {
                return next.as_os_str().to_string_lossy().to_string();
            }
        }
    }
    // Fallback: use parent directory name
    file_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Detect whether a JSONL file is a Claude Internal subagent log.
///
/// Subagent files live at `<project>/<session_id>/subagents/agent-xxx.jsonl`.
fn is_subagent_file(file_path: &Path) -> bool {
    file_path.components().any(|c| c.as_os_str() == "subagents")
}

// ============================================================================
// Incremental sync
// ============================================================================

/// Sync a single file incrementally (only new content since last sync).
///
/// Called when the file watcher detects a `.jsonl` file change.
#[allow(unsafe_code)]
pub async fn sync_single_file(
    client: &EsClient,
    file_path: &Path,
    device_id: &str,
) -> Result<usize, String> {
    if !file_path.exists() || file_path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
        return Ok(0);
    }

    // Determine provider from path — use anchored starts_with to avoid
    // false-positives on paths like .codebuddy-clone/ or foo.codebuddy.txt.
    let codebuddy_root = dirs::home_dir()
        .map(|h| h.join(".codebuddy").join("projects"))
        .unwrap_or_default();
    let provider = if file_path.starts_with(&codebuddy_root) {
        "codebuddy"
    } else {
        "claude"
    };

    let file_key = file_path.to_string_lossy().to_string();
    let mut state = load_sync_state();

    // Get the byte offset from last sync
    let last_offset = state
        .files
        .get(&file_key)
        .map(|s| s.byte_offset)
        .unwrap_or(0);
    let current_size = fs::metadata(file_path).map(|m| m.len()).unwrap_or(0);

    // Detect truncate/rewrite: file shrunk -> reset from 0
    use std::cmp::Ordering;
    let offset = match current_size.cmp(&last_offset) {
        Ordering::Less => {
            log::warn!(
                "ES sync: file {} shrunk ({last_offset} -> {current_size}), restarting from 0",
                file_path.display()
            );
            0
        }
        Ordering::Equal => {
            // Nothing new
            return Ok(0);
        }
        Ordering::Greater => last_offset,
    };

    // Ensure indices exist
    ensure_indices(client).await?;

    // Stream-process the file: parse messages and flush to ES in batches
    // of BULK_BATCH_SIZE so peak memory stays bounded for large files.
    // (A single 344MB jsonl previously inflated to ~3GB RSS when parsed
    // into a full Vec<EsMessageDoc> + cloned to Vec<(String, Value)>.)
    let mut parser = StreamingJsonlParser::open(file_path, provider, device_id, offset)?;
    let mut batch: Vec<(String, Value)> = Vec::with_capacity(BULK_BATCH_SIZE);
    let mut indexed = 0usize;
    let mut msg_count = 0usize;

    loop {
        let appended = parser.next_chunk(&mut batch, BULK_BATCH_SIZE);
        msg_count += appended;
        if batch.len() >= BULK_BATCH_SIZE {
            indexed += client.bulk_index(MESSAGES_INDEX, &batch).await?;
            batch.clear();
        }
        if parser.is_done() {
            break;
        }
    }

    // Flush trailing partial batch
    if !batch.is_empty() {
        indexed += client.bulk_index(MESSAGES_INDEX, &batch).await?;
        batch.clear();
    }

    let session_doc = parser.finish();

    if msg_count == 0 {
        return Ok(0);
    }

    // Update session doc (id = session_id, dedupes across devices)
    if let Some(session) = session_doc {
        let session_doc_id = session.session_id.clone();
        let doc = serde_json::to_value(&session).unwrap_or_default();
        let _ = client
            .bulk_index(SESSIONS_INDEX, &[(session_doc_id, doc)])
            .await;
    }

    // Update sync state
    state.files.insert(
        file_key,
        FileSyncState {
            mtime: get_file_mtime(file_path),
            byte_offset: current_size,
            message_count: state
                .files
                .get(&file_path.to_string_lossy().to_string())
                .map(|s| s.message_count + msg_count)
                .unwrap_or(msg_count),
            last_synced: Utc::now().to_rfc3339(),
        },
    );
    save_sync_state(&state);

    log::info!(
        "ES incremental sync: {} new messages from {}",
        indexed,
        file_path.display()
    );

    Ok(indexed)
}

/// Run incremental sync for all files that have changed since last sync.
#[allow(unsafe_code)]
pub async fn incremental_sync(
    client: &EsClient,
    device_id: &str,
    custom_claude_paths: &[String],
) -> Result<SyncStats, String> {
    let start = std::time::Instant::now();
    let mut stats = SyncStats {
        files_processed: 0,
        messages_indexed: 0,
        sessions_indexed: 0,
        errors: Vec::new(),
        duration_ms: 0,
    };

    ensure_indices(client).await?;

    let files = collect_all_jsonl_files(custom_claude_paths);
    let state = load_sync_state();

    for (file_path, _provider) in &files {
        let file_key = file_path.to_string_lossy().to_string();
        let current_size = fs::metadata(file_path).map(|m| m.len()).unwrap_or(0);
        let current_mtime = get_file_mtime(file_path);

        // Check if file needs syncing
        if let Some(existing) = state.files.get(&file_key) {
            if existing.mtime == current_mtime && existing.byte_offset == current_size {
                continue; // No changes
            }
        }

        match sync_single_file(client, file_path, device_id).await {
            Ok(n) => {
                if n > 0 {
                    stats.files_processed += 1;
                    stats.messages_indexed += n;
                }
            }
            Err(e) => {
                stats.errors.push(format!("{}: {e}", file_path.display()));
            }
        }
    }

    stats.duration_ms = start.elapsed().as_millis() as u64;
    log::info!(
        "ES incremental sync: {} files, {} messages, {}ms",
        stats.files_processed,
        stats.messages_indexed,
        stats.duration_ms
    );

    Ok(stats)
}
