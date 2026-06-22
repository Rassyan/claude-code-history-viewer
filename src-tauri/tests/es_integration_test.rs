use claude_code_history_viewer_lib::elasticsearch::{
    load_sync_state, sync_single_file, EsClient, MESSAGES_INDEX, SESSIONS_INDEX,
};
use serde_json::Value;

const DEVICE_ID: &str = "test-integration";

/// Connection settings come from env vars so no personal endpoint or
/// credential lives in the repo. Set `CCHV_TEST_ES_ENDPOINT` (and optionally
/// `CCHV_TEST_ES_USER` / `CCHV_TEST_ES_PASS`) to run these integration
/// tests; each test skips silently when the endpoint is not configured.
fn get_client() -> Option<EsClient> {
    let endpoint = std::env::var("CCHV_TEST_ES_ENDPOINT").ok()?;
    let user = std::env::var("CCHV_TEST_ES_USER").ok();
    let pass = std::env::var("CCHV_TEST_ES_PASS").ok();
    Some(EsClient::new(&endpoint, user.as_deref(), pass.as_deref()))
}

macro_rules! require_client {
    () => {
        match get_client() {
            Some(c) => c,
            None => {
                eprintln!("CCHV_TEST_ES_ENDPOINT not set; skipping ES integration test");
                return;
            }
        }
    };
}

#[tokio::test]
async fn test_es_connection_health() {
    let client = require_client!();
    let healthy = client.health().await.expect("should connect");
    assert!(healthy, "cluster should be healthy");
}

#[tokio::test]
async fn test_es_indices_exist_after_sync() {
    let client = require_client!();
    // Indices should exist from previous full sync
    assert!(
        client.index_exists(MESSAGES_INDEX).await.unwrap(),
        "cchv-messages index should exist"
    );
    assert!(
        client.index_exists(SESSIONS_INDEX).await.unwrap(),
        "cchv-sessions index should exist"
    );
}

#[tokio::test]
async fn test_es_search_chinese() {
    let client = require_client!();
    let query = serde_json::json!({
        "query": {
            "match": { "content_text": "Elasticsearch" }
        },
        "size": 5
    });

    let result = client
        .search(MESSAGES_INDEX, &query)
        .await
        .expect("search should work");
    let total = result
        .get("hits")
        .and_then(|h| h.get("total"))
        .and_then(|t| t.get("value"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    assert!(total > 0, "should find messages containing 'Elasticsearch'");
    println!("Chinese search 'Elasticsearch': {total} hits");
}

#[tokio::test]
async fn test_es_search_with_filters() {
    let client = require_client!();
    let query = serde_json::json!({
        "query": {
            "bool": {
                "must": [
                    { "match": { "content_text": "搜索" } }
                ],
                "filter": [
                    { "term": { "role": "assistant" } }
                ]
            }
        },
        "size": 3
    });

    let result = client
        .search(MESSAGES_INDEX, &query)
        .await
        .expect("filtered search should work");
    let hits = result
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .expect("should have hits array");

    for hit in hits {
        let role = hit
            .get("_source")
            .and_then(|s| s.get("role"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(
            role, "assistant",
            "filtered results should only be assistant"
        );
    }
}

#[tokio::test]
async fn test_es_session_count() {
    let client = require_client!();
    let query = serde_json::json!({"query": {"match_all": {}}});
    let count = client
        .count(SESSIONS_INDEX, &query)
        .await
        .expect("count should work");
    assert!(count > 0, "should have sessions indexed");
    println!("Total sessions in ES: {count}");
}

#[tokio::test]
async fn test_es_message_has_raw_field() {
    let client = require_client!();
    let query = serde_json::json!({
        "query": { "match_all": {} },
        "size": 1,
        "_source": ["raw", "session_id", "message_id"]
    });

    let result = client
        .search(MESSAGES_INDEX, &query)
        .await
        .expect("search should work");
    let hit = result
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .expect("should have at least one hit");

    let raw = hit.get("_source").and_then(|s| s.get("raw"));
    assert!(
        raw.is_some(),
        "message should have raw field for restoration"
    );
    assert!(raw.unwrap().is_object(), "raw should be a JSON object");
}

#[tokio::test]
async fn test_incremental_sync_no_changes() {
    let client = require_client!();
    // After a full sync, incremental sync with same state should do nothing
    let state = load_sync_state();
    assert!(
        !state.files.is_empty(),
        "sync state should have tracked files"
    );

    // Pick a file that was already synced
    if let Some((file_path, file_state)) = state.files.iter().next() {
        let path = std::path::Path::new(file_path);
        if path.exists() {
            let current_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            // If file size hasn't changed, incremental should index 0 new messages
            if current_size == file_state.byte_offset {
                let result = sync_single_file(&client, path, DEVICE_ID)
                    .await
                    .expect("incremental sync should succeed");
                assert_eq!(result, 0, "should not re-index unchanged file");
            }
        }
    }
}

#[tokio::test]
async fn test_es_load_session_by_id() {
    let client = require_client!();
    // Get a session_id from ES
    let query = serde_json::json!({
        "query": { "match_all": {} },
        "size": 1,
        "_source": ["session_id"]
    });

    let result = client.search(SESSIONS_INDEX, &query).await.unwrap();
    let session_id = result
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|hit| hit.get("_source"))
        .and_then(|s| s.get("session_id"))
        .and_then(Value::as_str)
        .expect("should have a session");

    // Load all messages for this session
    let msg_query = serde_json::json!({
        "query": { "term": { "session_id": session_id } },
        "size": 1,
        "_source": ["message_id", "timestamp", "role"]
    });

    let msg_result = client.search(MESSAGES_INDEX, &msg_query).await.unwrap();
    let msg_total = msg_result
        .get("hits")
        .and_then(|h| h.get("total"))
        .and_then(|t| t.get("value"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    assert!(msg_total > 0, "session should have messages in ES");
    println!("Session {session_id}: {msg_total} messages");
}
