use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MESSAGES_INDEX: &str = "cchv-messages";
pub const SESSIONS_INDEX: &str = "cchv-sessions";

/// A single message document stored in Elasticsearch.
///
/// Contains both searchable extracted fields and the full raw JSONL line
/// for lossless restoration to the local filesystem.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EsMessageDoc {
    /// Unique message ID (`uuid` from Claude, `id` from `CodeBuddy`)
    pub message_id: String,
    /// Session this message belongs to
    pub session_id: String,
    /// Provider source: "claude", "codebuddy", etc.
    pub provider: String,
    /// Device identifier for multi-device support
    pub device_id: String,
    /// Original project path on the source machine
    pub project_path: String,
    /// Short project name for display
    pub project_name: String,
    /// Path to the source JSONL file
    pub source_file: String,

    /// Message role: "user", "assistant", or None for system/tool messages
    pub role: Option<String>,
    /// Message type: `"user"`, `"assistant"`, `"system"`, `"tool_use"`, `"tool_result"`, etc.
    pub message_type: String,
    /// ISO 8601 timestamp
    pub timestamp: String,

    /// Extracted searchable text content (indexed with IK analyzer)
    pub content_text: String,
    /// Tool name if this is a `tool_use` message
    pub tool_name: Option<String>,
    /// Serialized `tool_use` input parameters (`Bash` command, `Grep` pattern,
    /// `Read` path, `Edit` `old_string`, etc.) — indexed for searchability so
    /// users can find "what command did I run last week?" or "find that PR
    /// with the edit on auth.rs". Truncated to 5000 chars to keep the index
    /// lean.
    pub tool_input: Option<String>,
    /// AI model used (e.g., "claude-4.7-opus")
    pub model: Option<String>,
    /// Git branch at the time of the conversation
    pub git_branch: Option<String>,
    /// Client entrypoint: "cli", "claude-vscode", "claude-desktop"
    pub entrypoint: Option<String>,
    /// Cost in USD (if available)
    pub cost_usd: Option<f64>,
    /// Input token count
    pub token_input: Option<u32>,
    /// Output token count
    pub token_output: Option<u32>,

    /// Whether this message is from a subagent log (Claude Internal feature)
    #[serde(default)]
    pub is_subagent: bool,

    /// Full original JSONL line as raw JSON (not indexed, for restoration)
    pub raw: Value,

    /// When this document was synced to ES
    pub synced_at: String,
}

/// A session metadata document stored in Elasticsearch.
///
/// Used for session listing when local files have been cleaned up.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EsSessionDoc {
    /// Session ID (actual session UUID)
    pub session_id: String,
    /// Provider source
    pub provider: String,
    /// Device identifier
    pub device_id: String,
    /// Original project path
    pub project_path: String,
    /// Short project name
    pub project_name: String,
    /// Path to the source JSONL file
    pub source_file: String,
    /// First message timestamp
    pub first_message_time: String,
    /// Last message timestamp
    pub last_message_time: String,
    /// Total message count in session
    pub message_count: usize,
    /// Session summary text
    pub summary: Option<String>,
    /// Client entrypoint
    pub entrypoint: Option<String>,
    /// Git branch
    pub git_branch: Option<String>,
    /// Whether tool calls were used
    pub has_tool_use: bool,
    /// Whether this is a subagent session (Claude Internal feature)
    #[serde(default)]
    pub is_subagent: bool,
    /// When this document was synced to ES
    pub synced_at: String,
}

/// Generate the index creation body for `cchv-messages`.
///
/// Uses IK analyzer for Chinese text search with standard analyzer fallback.
/// The `raw` field is stored but not indexed (for restoration only).
pub fn messages_index_body() -> Value {
    json!({
        "settings": {
            "number_of_shards": 1,
            "number_of_replicas": 0,
            "refresh_interval": "5s"
        },
        "mappings": {
            "properties": {
                "message_id":   { "type": "keyword" },
                "session_id":   { "type": "keyword" },
                "provider":     { "type": "keyword" },
                "device_id":    { "type": "keyword" },
                "project_path": { "type": "keyword" },
                "project_name": { "type": "keyword" },
                "source_file":  { "type": "keyword" },
                "role":         { "type": "keyword" },
                "message_type": { "type": "keyword" },
                "timestamp":    { "type": "date" },
                "content_text": {
                    "type": "text",
                    "analyzer": "ik_max_word",
                    "search_analyzer": "ik_smart",
                    "fields": {
                        "standard": { "type": "text", "analyzer": "standard" }
                    }
                },
                "tool_name":    { "type": "keyword" },
                "tool_input": {
                    "type": "text",
                    "analyzer": "ik_max_word",
                    "search_analyzer": "ik_smart",
                    "fields": {
                        "standard": { "type": "text", "analyzer": "standard" }
                    }
                },
                "model":        { "type": "keyword" },
                "git_branch":   { "type": "keyword" },
                "entrypoint":   { "type": "keyword" },
                "cost_usd":     { "type": "float" },
                "token_input":  { "type": "integer" },
                "token_output": { "type": "integer" },
                "is_subagent":  { "type": "boolean" },
                "raw":          { "type": "object", "enabled": false },
                "synced_at":    { "type": "date" }
            }
        }
    })
}

/// Generate the index creation body for `cchv-sessions`.
pub fn sessions_index_body() -> Value {
    json!({
        "settings": {
            "number_of_shards": 1,
            "number_of_replicas": 0
        },
        "mappings": {
            "properties": {
                "session_id":         { "type": "keyword" },
                "provider":           { "type": "keyword" },
                "device_id":          { "type": "keyword" },
                "project_path":       { "type": "keyword" },
                "project_name":       { "type": "keyword" },
                "source_file":        { "type": "keyword" },
                "first_message_time": { "type": "date" },
                "last_message_time":  { "type": "date" },
                "message_count":      { "type": "integer" },
                "summary": {
                    "type": "text",
                    "analyzer": "ik_max_word",
                    "search_analyzer": "ik_smart",
                    "fields": {
                        "standard": { "type": "text", "analyzer": "standard" }
                    }
                },
                "entrypoint":    { "type": "keyword" },
                "git_branch":    { "type": "keyword" },
                "has_tool_use":  { "type": "boolean" },
                "is_subagent":   { "type": "boolean" },
                "synced_at":     { "type": "date" }
            }
        }
    })
}
