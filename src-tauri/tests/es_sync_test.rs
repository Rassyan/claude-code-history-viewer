use claude_code_history_viewer_lib::elasticsearch::{full_sync, EsClient, NoopReporter};

/// Full-sync smoke test against a live ES instance.
///
/// Connection settings come from env vars so no personal endpoint or
/// credential lives in the repo:
/// - `CCHV_TEST_ES_ENDPOINT` (required; test skips silently when unset)
/// - `CCHV_TEST_ES_USER` / `CCHV_TEST_ES_PASS` (optional Basic Auth)
/// - `CCHV_TEST_CUSTOM_CLAUDE_PATH` (optional extra Claude base path to sync)
#[tokio::test]
async fn test_full_sync_to_es() {
    let Ok(endpoint) = std::env::var("CCHV_TEST_ES_ENDPOINT") else {
        eprintln!("CCHV_TEST_ES_ENDPOINT not set; skipping ES integration test");
        return;
    };
    let user = std::env::var("CCHV_TEST_ES_USER").ok();
    let pass = std::env::var("CCHV_TEST_ES_PASS").ok();
    let client = EsClient::new(&endpoint, user.as_deref(), pass.as_deref());

    // Verify connection
    let healthy = client.health().await.expect("ES should be reachable");
    assert!(healthy, "ES cluster should be healthy");

    // Run full sync
    let device_id = "test-device";
    let custom_paths: Vec<String> = std::env::var("CCHV_TEST_CUSTOM_CLAUDE_PATH")
        .ok()
        .into_iter()
        .collect();
    let stats = full_sync(&client, device_id, &custom_paths, &NoopReporter)
        .await
        .expect("full_sync should succeed");

    println!("=== Full Sync Results ===");
    println!("Files processed: {}", stats.files_processed);
    println!("Messages indexed: {}", stats.messages_indexed);
    println!("Sessions indexed: {}", stats.sessions_indexed);
    println!("Duration: {}ms", stats.duration_ms);
    println!("Errors: {}", stats.errors.len());
    for err in &stats.errors {
        println!("  ERROR: {err}");
    }

    assert!(
        stats.files_processed > 0,
        "Should process at least some files"
    );
    assert!(
        stats.messages_indexed > 0,
        "Should index at least some messages"
    );
    assert!(
        stats.errors.len() < stats.files_processed,
        "Most files should sync without error"
    );
}
