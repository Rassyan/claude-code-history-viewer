//! End-to-end test: cloud session loading after local file deletion.
//!
//! Simulates: user deletes a local project directory → the project still exists
//! in ES → frontend should be able to search and load messages from ES.

#![allow(clippy::missing_panics_doc)]
#![allow(clippy::case_sensitive_file_extension_comparisons)]
#![allow(clippy::doc_markdown)]

use claude_code_history_viewer_lib::elasticsearch::{EsClient, MESSAGES_INDEX, SESSIONS_INDEX};
use serde_json::Value;

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

/// E2E-1: Verify ES contains data needed to display projects after local deletion.
///
/// Even if a local project directory is gone, ES must still let us:
/// 1. List the project (`es_list_projects`)
/// 2. List sessions in it (`es_list_sessions`)
/// 3. Load messages of any session (`es_load_session_messages`)
#[tokio::test]
async fn test_full_recovery_chain_for_deleted_project() {
    let client = require_client!();

    // 1. Aggregate projects from ES (subagents excluded)
    let projects_query = serde_json::json!({
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
            "projects": { "terms": { "field": "project_name", "size": 100 } }
        }
    });

    let result = client
        .search(SESSIONS_INDEX, &projects_query)
        .await
        .unwrap();
    let buckets = result
        .get("aggregations")
        .and_then(|a| a.get("projects"))
        .and_then(|p| p.get("buckets"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    assert!(!buckets.is_empty(), "Expected at least one project in ES");

    // 'subagents' must NOT be in the project list
    for bucket in &buckets {
        let key = bucket.get("key").and_then(Value::as_str).unwrap_or("");
        assert_ne!(
            key, "subagents",
            "Subagents directory leaked into project list"
        );
        assert!(!key.is_empty(), "Project name must not be empty");
    }

    println!("Found {} non-subagent projects in ES", buckets.len());

    // 2. Pick a project, list its sessions
    let some_project = buckets[0].get("key").and_then(Value::as_str).unwrap();

    let sessions_query = serde_json::json!({
        "query": { "term": { "project_name": some_project } },
        "size": 5,
        "_source": ["session_id", "source_file", "message_count"]
    });

    let sessions_result = client
        .search(SESSIONS_INDEX, &sessions_query)
        .await
        .unwrap();
    let session_hits = sessions_result
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    assert!(
        !session_hits.is_empty(),
        "Project '{some_project}' must have sessions"
    );

    // 3. Load messages of the first session — verify we can reconstruct content
    let session_id = session_hits[0]
        .get("_source")
        .and_then(|s| s.get("session_id"))
        .and_then(Value::as_str)
        .unwrap();

    let messages_query = serde_json::json!({
        "query": { "term": { "session_id": session_id } },
        "size": 3,
        "sort": [{"timestamp": "asc"}],
        "_source": ["message_id", "role", "content_text", "raw"]
    });

    let messages_result = client
        .search(MESSAGES_INDEX, &messages_query)
        .await
        .unwrap();
    let message_hits = messages_result
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    assert!(
        !message_hits.is_empty(),
        "Session '{session_id}' must have messages in ES"
    );

    // Verify each message has the raw field (for restoration)
    for hit in &message_hits {
        let raw = hit.get("_source").and_then(|s| s.get("raw"));
        assert!(
            raw.is_some(),
            "Message must carry raw field for restoration"
        );
    }

    println!(
        "✓ Full recovery chain works: project={some_project} session={session_id} msgs={}",
        message_hits.len()
    );
}

/// E2E-2: Verify subagent messages are queryable by their session_id but
/// don't pollute the project listing.
#[tokio::test]
async fn test_subagent_messages_present_but_not_independent() {
    let client = require_client!();

    // Subagent messages must exist in messages index
    let subagent_count_query = serde_json::json!({
        "query": { "term": { "is_subagent": true } }
    });

    let count = client
        .count(MESSAGES_INDEX, &subagent_count_query)
        .await
        .unwrap();
    println!("Subagent messages count: {count}");
    // Don't assert > 0 because not every test environment has subagent data.

    // But subagent project must not be in project aggregations
    let projects_query = serde_json::json!({
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
            "projects": { "terms": { "field": "project_name", "size": 100 } }
        }
    });

    let result = client
        .search(SESSIONS_INDEX, &projects_query)
        .await
        .unwrap();
    let buckets = result
        .get("aggregations")
        .and_then(|a| a.get("projects"))
        .and_then(|p| p.get("buckets"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let has_subagents = buckets
        .iter()
        .any(|b| b.get("key").and_then(Value::as_str) == Some("subagents"));

    assert!(
        !has_subagents,
        "Subagents must not appear as a project in the filtered query"
    );
}

/// E2E-3: Multi-device dedup — session_id is used as ES doc id (not device_id:session_id),
/// so syncing same session from multiple devices should produce ONE session doc.
#[tokio::test]
async fn test_session_doc_dedup_by_session_id() {
    let client = require_client!();

    // Total unique session_ids in messages index
    let unique_sessions_query = serde_json::json!({
        "size": 0,
        "aggs": {
            "unique_sessions": { "cardinality": { "field": "session_id" } }
        }
    });

    let result = client
        .search(MESSAGES_INDEX, &unique_sessions_query)
        .await
        .unwrap();
    let unique_msg_session_count = result
        .get("aggregations")
        .and_then(|a| a.get("unique_sessions"))
        .and_then(|u| u.get("value"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    // Total session docs in sessions index
    let total_session_docs = client
        .count(
            SESSIONS_INDEX,
            &serde_json::json!({"query": {"match_all": {}}}),
        )
        .await
        .unwrap();

    println!(
        "Unique session_ids in messages: {unique_msg_session_count}, session docs: {total_session_docs}"
    );

    // session docs should be <= unique message session_ids
    // (we may have duplicate subagent files mapping to same session_id, so docs ≤ unique)
    assert!(
        total_session_docs <= unique_msg_session_count,
        "Session doc count ({total_session_docs}) should not exceed unique message session_ids ({unique_msg_session_count})"
    );
}

/// E2E-4: Search results must contain `source_file` for navigation.
#[tokio::test]
async fn test_search_results_have_source_file() {
    let client = require_client!();

    let query = serde_json::json!({
        "query": {
            "multi_match": {
                "query": "test",
                "fields": ["content_text^3", "tool_name", "content_text.standard"],
                "type": "best_fields",
                "fuzziness": "AUTO"
            }
        },
        "size": 5,
        "_source": ["source_file", "session_id", "message_id"]
    });

    let result = client.search(MESSAGES_INDEX, &query).await.unwrap();
    let hits = result
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if hits.is_empty() {
        println!("Skipping: no test query matches");
        return;
    }

    for hit in &hits {
        let source = hit.get("_source").unwrap();
        let source_file = source
            .get("source_file")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            source_file.starts_with('/'),
            "source_file must be absolute path: {source_file}"
        );
        assert!(
            source_file.ends_with(".jsonl"),
            "source_file must be .jsonl: {source_file}"
        );

        // message_id must be device:uuid format
        let message_id = source
            .get("message_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            message_id.contains(':'),
            "message_id must be device:uuid format: {message_id}"
        );
    }
}

/// E2E-5: project name extraction must be consistent across messages and sessions.
#[tokio::test]
async fn test_project_name_consistency() {
    let client = require_client!();

    // Get a sample of messages and verify project_name doesn't include 'subagents'
    let query = serde_json::json!({
        "size": 100,
        "query": { "match_all": {} },
        "_source": ["project_name", "source_file", "is_subagent"]
    });

    let result = client.search(MESSAGES_INDEX, &query).await.unwrap();
    let hits = result
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for hit in &hits {
        let source = hit.get("_source").unwrap();
        let project_name = source
            .get("project_name")
            .and_then(Value::as_str)
            .unwrap_or("");
        let source_file = source
            .get("source_file")
            .and_then(Value::as_str)
            .unwrap_or("");
        let is_subagent = source
            .get("is_subagent")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Critical invariant: even subagent messages should have a real project name
        assert_ne!(
            project_name, "subagents",
            "Subagent message has wrong project_name: {source_file}"
        );

        // is_subagent flag must match the path
        let path_has_subagents = source_file.contains("/subagents/");
        assert_eq!(
            is_subagent, path_has_subagents,
            "is_subagent flag mismatch with path: file={source_file}, flag={is_subagent}"
        );
    }
}
