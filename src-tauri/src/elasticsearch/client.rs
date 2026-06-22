use base64::Engine;
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

/// Elasticsearch HTTP client for CCHV conversation sync.
///
/// Supports Basic Auth and provides methods for index management,
/// bulk indexing, search, and document retrieval.
///
/// Connection pool is intentionally small (max 2 idle per host) because
/// the sync workload is sequential per-file; each bulk request carries
/// up to 500 documents at once and we never fan-out writes across many
/// connections to the same ES host.
#[derive(Clone)]
pub struct EsClient {
    client: Client,
    base_url: String,
}

impl EsClient {
    /// Create a new ES client with optional Basic Auth credentials.
    ///
    /// Timeouts are essential: without them, a single unreachable ES
    /// endpoint can hang an async task indefinitely. Each hung task
    /// holds an mmap and a batch buffer, and enough concurrent hangs
    /// will exhaust system memory (~26GB observed in the wild).
    pub fn new(base_url: &str, username: Option<&str>, password: Option<&str>) -> Self {
        let mut builder = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(2);

        if let (Some(user), Some(pass)) = (username, password) {
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            let mut headers = reqwest::header::HeaderMap::new();
            // Infallible: base64 output is always visible ASCII, which is a
            // valid HeaderValue regardless of what bytes the password held.
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Basic {encoded}"))
                    .expect("base64 alphabet is always a valid header value"),
            );
            builder = builder.default_headers(headers);
        }

        Self {
            client: builder.build().expect("failed to build HTTP client"),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Check if the ES cluster is reachable and healthy.
    pub async fn health(&self) -> Result<bool, String> {
        let resp = self
            .client
            .get(format!("{}/_cluster/health", self.base_url))
            .send()
            .await
            .map_err(|e| format!("ES connection failed: {e}"))?;

        Ok(resp.status().is_success())
    }

    /// Check if an index exists.
    pub async fn index_exists(&self, index: &str) -> Result<bool, String> {
        let resp = self
            .client
            .head(format!("{}/{index}", self.base_url))
            .send()
            .await
            .map_err(|e| format!("ES request failed: {e}"))?;

        Ok(resp.status().is_success())
    }

    /// Create an index with the given settings and mappings.
    pub async fn create_index(&self, index: &str, body: &Value) -> Result<(), String> {
        let resp = self
            .client
            .put(format!("{}/{index}", self.base_url))
            .json(body)
            .send()
            .await
            .map_err(|e| format!("ES create index failed: {e}"))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("ES create index error: {text}"));
        }

        Ok(())
    }

    /// Bulk index documents. Returns the number of successfully indexed docs.
    ///
    /// Uses the `_bulk` API with `index` actions.
    pub async fn bulk_index(&self, index: &str, docs: &[(String, Value)]) -> Result<usize, String> {
        if docs.is_empty() {
            return Ok(0);
        }

        // Build NDJSON bulk body
        let mut body = String::new();
        for (id, doc) in docs {
            let action = serde_json::to_string(&serde_json::json!({
                "index": { "_index": index, "_id": id }
            }))
            .map_err(|e| format!("ES bulk action serialize failed: {e}"))?;
            body.push_str(&action);
            body.push('\n');
            let doc_line = serde_json::to_string(doc)
                .map_err(|e| format!("ES bulk doc serialize failed: {e}"))?;
            body.push_str(&doc_line);
            body.push('\n');
        }

        let resp = self
            .client
            .post(format!("{}/_bulk", self.base_url))
            .header("Content-Type", "application/x-ndjson")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("ES bulk request failed: {e}"))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("ES bulk error: {text}"));
        }

        let result: Value = resp.json().await.map_err(|e| e.to_string())?;
        let errors = result
            .get("errors")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if errors {
            let items = result.get("items").and_then(Value::as_array);
            let error_count = items
                .map(|items| {
                    items
                        .iter()
                        .filter(|item| item.get("index").and_then(|i| i.get("error")).is_some())
                        .count()
                })
                .unwrap_or(0);
            log::warn!(
                "ES bulk indexing had {error_count} errors out of {} docs",
                docs.len()
            );
            return Ok(docs.len() - error_count);
        }

        Ok(docs.len())
    }

    /// Execute a search query against an index.
    pub async fn search(&self, index: &str, query: &Value) -> Result<Value, String> {
        let resp = self
            .client
            .post(format!("{}/{index}/_search", self.base_url))
            .json(query)
            .send()
            .await
            .map_err(|e| format!("ES search failed: {e}"))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("ES search error: {text}"));
        }

        resp.json()
            .await
            .map_err(|e| format!("ES response parse error: {e}"))
    }

    /// Get a document by ID from an index.
    pub async fn get_by_id(&self, index: &str, id: &str) -> Result<Option<Value>, String> {
        let resp = self
            .client
            .get(format!("{}/{index}/_doc/{id}", self.base_url))
            .send()
            .await
            .map_err(|e| format!("ES get failed: {e}"))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("ES get error: {text}"));
        }

        let doc: Value = resp.json().await.map_err(|e| e.to_string())?;
        Ok(doc.get("_source").cloned())
    }

    /// Count documents matching a query.
    pub async fn count(&self, index: &str, query: &Value) -> Result<u64, String> {
        let resp = self
            .client
            .post(format!("{}/{index}/_count", self.base_url))
            .json(query)
            .send()
            .await
            .map_err(|e| format!("ES count failed: {e}"))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("ES count error: {text}"));
        }

        let result: Value = resp.json().await.map_err(|e| e.to_string())?;
        Ok(result.get("count").and_then(Value::as_u64).unwrap_or(0))
    }

    /// Delete an index (for testing/reset purposes).
    pub async fn delete_index(&self, index: &str) -> Result<(), String> {
        let resp = self
            .client
            .delete(format!("{}/{index}", self.base_url))
            .send()
            .await
            .map_err(|e| format!("ES delete index failed: {e}"))?;

        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("ES delete index error: {text}"));
        }

        Ok(())
    }
}
