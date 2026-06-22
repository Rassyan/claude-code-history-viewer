//! Search quality tests — locks in the user-visible behaviors that the
//! search quality review fixed:
//!   - BM25 score is preserved (sort doesn't override relevance)
//!   - ES highlight fragments come back with `<mark>` tags
//!   - `tool_input` is indexed and searchable
//!   - Phrase mode returns fewer / more focused hits than smart mode
//!
//! Requires a live ES (configured via `CCHV_TEST_ES_ENDPOINT`) and at
//! least one full sync to have populated the index.

#![allow(clippy::needless_collect)]
#![allow(clippy::uninlined_format_args)]

use claude_code_history_viewer_lib::elasticsearch::EsClient;

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
async fn test_smart_search_returns_score_and_highlight() {
    let c = require_client!();
    if !c.health().await.unwrap_or(false) {
        eprintln!("ES unreachable, skipping");
        return;
    }

    // Smart mode: relevance sort + score + highlight
    let q = serde_json::json!({
        "query": {
            "multi_match": {
                "query": "搜索",
                "fields": ["content_text^3", "tool_input^2", "tool_name", "content_text.standard"],
                "type": "best_fields"
            }
        },
        "highlight": {
            "pre_tags": ["<mark>"],
            "post_tags": ["</mark>"],
            "fields": {
                "content_text": { "fragment_size": 220, "number_of_fragments": 2 }
            }
        },
        "sort": ["_score", { "timestamp": "desc" }],
        "size": 5,
        "_source": ["timestamp"]
    });

    let r = c.search("cchv-messages", &q).await.expect("search");
    let hits = r["hits"]["hits"].as_array().expect("hits array");
    assert!(!hits.is_empty(), "expected hits for '搜索'");

    let first = &hits[0];
    let score = first["_score"].as_f64();
    assert!(score.is_some(), "_score must NOT be null in smart mode");
    assert!(
        score.unwrap() > 0.0,
        "score should be positive: {:?}",
        score
    );

    // First hit should have a highlight with <mark> tags
    let highlight = first
        .get("highlight")
        .and_then(|h| h.get("content_text"))
        .and_then(|v| v.as_array())
        .expect("highlight.content_text array");
    assert!(!highlight.is_empty(), "highlight fragments should exist");
    let first_frag = highlight[0].as_str().unwrap_or("");
    assert!(
        first_frag.contains("<mark>"),
        "fragment must contain <mark>: {}",
        first_frag
    );

    // Scores should be monotonically non-increasing
    let scores: Vec<f64> = hits.iter().filter_map(|h| h["_score"].as_f64()).collect();
    for w in scores.windows(2) {
        assert!(
            w[0] >= w[1],
            "scores must be non-increasing: {} -> {}",
            w[0],
            w[1]
        );
    }
}

#[tokio::test]
async fn test_time_sort_disables_score() {
    let c = require_client!();
    if !c.health().await.unwrap_or(false) {
        return;
    }

    // Time-only sort — _score should be null
    let q = serde_json::json!({
        "query": {
            "multi_match": {
                "query": "搜索",
                "fields": ["content_text"],
                "type": "best_fields"
            }
        },
        "sort": [{ "timestamp": "desc" }],
        "size": 1,
        "_source": ["timestamp"]
    });

    let r = c.search("cchv-messages", &q).await.expect("search");
    let hits = r["hits"]["hits"].as_array().expect("hits array");
    if hits.is_empty() {
        return;
    }
    // When sort is timestamp-only, ES sets _score to null — confirm we
    // understand this and surface it correctly.
    assert!(
        hits[0]["_score"].is_null(),
        "_score should be null when sorting by timestamp only"
    );
}

#[tokio::test]
async fn test_tool_input_is_indexed_and_searchable() {
    let c = require_client!();
    if !c.health().await.unwrap_or(false) {
        return;
    }

    // Verify at least some docs have tool_input populated
    let exists_q = serde_json::json!({
        "query": { "exists": { "field": "tool_input" } },
        "size": 0
    });
    let r = c.search("cchv-messages", &exists_q).await.expect("search");
    let total = r["hits"]["total"]["value"].as_u64().unwrap_or(0);
    assert!(
        total > 0,
        "expected tool_input on at least some docs, got {}",
        total
    );

    // And it should be searchable — query for a common tool param keyword
    let kw_q = serde_json::json!({
        "query": { "match": { "tool_input": "command" } },
        "size": 1,
        "_source": ["tool_name", "tool_input"]
    });
    let r = c.search("cchv-messages", &kw_q).await.expect("search");
    let hits = r["hits"]["hits"].as_array().expect("hits array");
    assert!(
        !hits.is_empty(),
        "expected hits for tool_input:command (Bash/Edit have command/old_string fields)"
    );
}

#[tokio::test]
async fn test_phrase_mode_is_stricter_than_smart() {
    let c = require_client!();
    if !c.health().await.unwrap_or(false) {
        return;
    }

    // Pick a multi-token query that should return more hits in OR mode than
    // in phrase mode.
    let term = "搜索 增强";

    let smart_q = serde_json::json!({
        "query": {
            "multi_match": {
                "query": term,
                "fields": ["content_text"],
                "type": "best_fields"
            }
        },
        "size": 0
    });
    let phrase_q = serde_json::json!({
        "query": {
            "match_phrase": { "content_text": { "query": term, "slop": 1 } }
        },
        "size": 0
    });

    let smart_total = c.search("cchv-messages", &smart_q).await.expect("smart")["hits"]["total"]
        ["value"]
        .as_u64()
        .unwrap_or(0);
    let phrase_total = c.search("cchv-messages", &phrase_q).await.expect("phrase")["hits"]["total"]
        ["value"]
        .as_u64()
        .unwrap_or(0);

    // Phrase must be a subset of smart (≤ in count). If smart returns 0 too,
    // we don't have data for this term — skip rather than assert.
    if smart_total == 0 {
        eprintln!("no data for '{}', skipping phrase comparison", term);
        return;
    }
    assert!(
        phrase_total <= smart_total,
        "phrase mode ({}) should be ≤ smart mode ({})",
        phrase_total,
        smart_total
    );
}
