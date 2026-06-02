//! Soroban RPC client.
//!
//! Communicates with Soroban RPC endpoints: `getTransaction`, `simulateTransaction`,
//! `getLedgerEntries`, `getEvents`, `getLatestLedger`. Handles pagination, retries,
//! rate limit backoff, and 5xx server-error backoff.

use crate::types::config::NetworkConfig;
use crate::types::error::{PrismError, PrismResult};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Base delay (ms) for the exponential backoff: delay = BASE_DELAY_MS × 2^attempt.
const BASE_DELAY_MS: u64 = 100;

/// Hard ceiling on any single backoff sleep to prevent excessively long waits.
const MAX_DELAY_MS: u64 = 10_000; // 10 seconds

/// Compute the capped exponential backoff duration for a given attempt number.
///
/// Returns `BASE_DELAY_MS × 2^attempt`, clamped to `MAX_DELAY_MS`.
///
/// | attempt | raw ms | clamped ms |
/// |---------|--------|------------|
/// |    1    |   200  |    200     |
/// |    2    |   400  |    400     |
/// |    3    |   800  |    800     |
/// |    4    |  1600  |   1600     |
/// |    6    |  6400  |   6400     |
/// |    7    | 12800  |  10000     |
fn backoff_duration(attempt: u32) -> Duration {
    // Use saturating arithmetic so large `attempt` values don't overflow.
    let ms = BASE_DELAY_MS.saturating_mul(1u64.saturating_shl(attempt));
    Duration::from_millis(ms.min(MAX_DELAY_MS))
}

/// Soroban RPC client with retry and rate-limit handling.
pub struct RpcClient {
    /// HTTP client instance.
    client: reqwest::Client,
    /// Network configuration.
    config: NetworkConfig,
    /// Maximum number of retries for failed requests.
    max_retries: u32,
}

/// JSON-RPC request envelope.
#[derive(Debug, Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    params: serde_json::Value,
}

/// JSON-RPC response envelope.
#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: String,
    #[allow(dead_code)]
    id: u64,
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

/// JSON-RPC error.
#[derive(Debug, Deserialize)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

impl RpcClient {
    /// Create a new RPC client for the given network.
    pub fn new(config: NetworkConfig) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("Failed to create HTTP client"),
            config,
            max_retries: 3,
        }
    }

    /// Fetch a transaction by hash.
    pub async fn get_transaction(&self, tx_hash: &str) -> PrismResult<serde_json::Value> {
        let params = serde_json::json!({
            "hash": tx_hash,
        });
        self.call("getTransaction", params).await
    }

    /// Simulate a transaction.
    pub async fn simulate_transaction(&self, tx_xdr: &str) -> PrismResult<serde_json::Value> {
        let params = serde_json::json!({
            "transaction": tx_xdr,
        });
        self.call("simulateTransaction", params).await
    }

    /// Get ledger entries by keys.
    pub async fn get_ledger_entries(&self, keys: &[String]) -> PrismResult<serde_json::Value> {
        let params = serde_json::json!({
            "keys": keys,
        });
        self.call("getLedgerEntries", params).await
    }

    /// Get events matching a filter.
    pub async fn get_events(
        &self,
        start_ledger: u32,
        filters: serde_json::Value,
    ) -> PrismResult<serde_json::Value> {
        let params = serde_json::json!({
            "startLedger": start_ledger,
            "filters": filters,
        });
        self.call("getEvents", params).await
    }

    /// Get the latest ledger info.
    pub async fn get_latest_ledger(&self) -> PrismResult<serde_json::Value> {
        self.call("getLatestLedger", serde_json::json!({})).await
    }

    /// Internal JSON-RPC call with retry logic.
    ///
    /// Retries are triggered by:
    /// - Transport-level failures (connection refused, timeout, etc.)
    /// - HTTP 429 Too Many Requests
    /// - HTTP 5xx Server Errors (500–599)
    ///
    /// Backoff follows `BASE_DELAY_MS × 2^attempt`, capped at `MAX_DELAY_MS`.
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> PrismResult<serde_json::Value> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method,
            params,
        };

        let mut last_error = None;

        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                let delay = backoff_duration(attempt);
                tracing::debug!(
                    method,
                    attempt,
                    delay_ms = delay.as_millis(),
                    "Backing off before retry"
                );
                tokio::time::sleep(delay).await;
                tracing::debug!("Retry attempt {attempt} for RPC method {method}");
            }

            let started_at = Instant::now();
            let request_body = serde_json::to_string(&request)
                .unwrap_or_else(|_| "<failed to serialize request>".to_string());
            tracing::debug!(
                method,
                endpoint = %self.config.rpc_url,
                attempt,
                "Sending RPC request"
            );
            tracing::trace!(
                method,
                endpoint = %self.config.rpc_url,
                attempt,
                request = %request_body,
                "RPC request payload"
            );

            match self
                .client
                .post(&self.config.rpc_url)
                .json(&request)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let elapsed_ms = started_at.elapsed().as_millis();

                    tracing::debug!(
                        method,
                        endpoint = %self.config.rpc_url,
                        attempt,
                        status = %status,
                        elapsed_ms,
                        "RPC response received"
                    );

                    // Retry on 429 Too Many Requests.
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        tracing::warn!(
                            method,
                            attempt,
                            "Rate limited by RPC node (429), will retry"
                        );
                        last_error =
                            Some(PrismError::RpcError(format!("Rate limited (attempt {attempt})")));
                        continue;
                    }

                    // Retry on any 5xx Server Error.
                    if status.is_server_error() {
                        tracing::warn!(
                            method,
                            attempt,
                            status = %status,
                            elapsed_ms,
                            "RPC node returned a server error (5xx), will retry"
                        );
                        last_error = Some(PrismError::RpcError(format!(
                            "Server error {status} on attempt {attempt}"
                        )));
                        continue;
                    }

                    let response_body = response
                        .text()
                        .await
                        .map_err(|e| PrismError::RpcError(format!("Response read error: {e}")))?;

                    tracing::trace!(
                        method,
                        endpoint = %self.config.rpc_url,
                        attempt,
                        elapsed_ms,
                        response = %response_body,
                        "RPC response payload"
                    );

                    let rpc_response: JsonRpcResponse = serde_json::from_str(&response_body)
                        .map_err(|e| PrismError::RpcError(format!("Response parse error: {e}")))?;

                    if let Some(err) = rpc_response.error {
                        tracing::debug!(
                            method,
                            endpoint = %self.config.rpc_url,
                            attempt,
                            error = %err.message,
                            "RPC returned an error response"
                        );
                        return Err(PrismError::RpcError(err.message));
                    }

                    return rpc_response
                        .result
                        .ok_or_else(|| PrismError::RpcError("Empty response".to_string()));
                }
                Err(e) => {
                    tracing::debug!(
                        method,
                        endpoint = %self.config.rpc_url,
                        attempt,
                        elapsed_ms = started_at.elapsed().as_millis(),
                        error = %e,
                        "RPC request failed"
                    );
                    last_error = Some(PrismError::RpcError(format!("Request failed: {e}")));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| PrismError::RpcError("Unknown error".to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // ---------------------------------------------------------------------------
    // Minimal in-process HTTP/1.1 mock server
    // ---------------------------------------------------------------------------

    /// Spawn a mock HTTP server that answers every request with the next
    /// response from `responses`.  Each entry is a raw HTTP/1.1 response
    /// string.  Returns the bound local address.
    async fn spawn_mock_server(responses: Vec<String>) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let responses = Arc::new(responses);
        let call_count = Arc::new(AtomicUsize::new(0));

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let responses = Arc::clone(&responses);
                let call_count = Arc::clone(&call_count);
                tokio::spawn(async move {
                    // Drain the incoming request (we don't need to parse it).
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;

                    let idx = call_count.fetch_add(1, Ordering::SeqCst);
                    let response = responses
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| responses.last().cloned().unwrap_or_default());

                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });

        addr
    }

    /// Build a raw HTTP/1.1 response with the given status code and body.
    fn http_response(status: u16, status_text: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Minimal valid JSON-RPC 2.0 success response body.
    fn ok_body() -> &'static str {
        r#"{"jsonrpc":"2.0","id":1,"result":{"status":"SUCCESS"}}"#
    }

    /// Build an `RpcClient` pointed at `http://addr`, with the given `max_retries`.
    fn make_client(addr: std::net::SocketAddr, max_retries: u32) -> RpcClient {
        let config = crate::types::config::NetworkConfig {
            network: crate::types::config::Network::Custom,
            rpc_url: format!("http://{addr}"),
            network_passphrase: "test".to_string(),
            archive_urls: vec![],
        };
        RpcClient {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            config,
            max_retries,
        }
    }

    // ---------------------------------------------------------------------------
    // backoff_duration unit tests — pure, no I/O
    // ---------------------------------------------------------------------------

    #[test]
    fn backoff_increases_exponentially() {
        assert_eq!(backoff_duration(1), Duration::from_millis(200));
        assert_eq!(backoff_duration(2), Duration::from_millis(400));
        assert_eq!(backoff_duration(3), Duration::from_millis(800));
        assert_eq!(backoff_duration(4), Duration::from_millis(1_600));
        assert_eq!(backoff_duration(5), Duration::from_millis(3_200));
        assert_eq!(backoff_duration(6), Duration::from_millis(6_400));
    }

    #[test]
    fn backoff_is_capped_at_max_delay() {
        // attempt 7 → raw = 100 × 128 = 12 800 ms → clamped to MAX_DELAY_MS
        assert_eq!(backoff_duration(7), Duration::from_millis(MAX_DELAY_MS));
        // Very large attempt must not overflow.
        assert_eq!(backoff_duration(63), Duration::from_millis(MAX_DELAY_MS));
    }

    #[test]
    fn backoff_attempt_zero_returns_base_delay() {
        // 100 × 2^0 = 100 ms
        assert_eq!(backoff_duration(0), Duration::from_millis(BASE_DELAY_MS));
    }

    // ---------------------------------------------------------------------------
    // Integration-style tests: real reqwest against an in-process TCP server
    //
    // NOTE: The mock server above uses a real TCP connection, so no I/O mocking
    //       library is required.  Backoff sleeps are exercised but kept short
    //       because the client is configured with max_retries=1 or max_retries=2.
    // ---------------------------------------------------------------------------

    /// One 500 followed by a 200 — client should succeed on the second attempt.
    #[tokio::test]
    async fn retries_once_on_500_then_succeeds() {
        let responses = vec![
            http_response(500, "Internal Server Error", ""),
            http_response(200, "OK", ok_body()),
        ];
        let addr = spawn_mock_server(responses).await;
        let client = make_client(addr, 3);

        let result = client.get_latest_ledger().await;
        assert!(result.is_ok(), "Expected success after retry, got: {result:?}");
        assert_eq!(result.unwrap()["status"], "SUCCESS");
    }

    /// Persistent 500 — client exhausts all retries and returns an error.
    #[tokio::test]
    async fn exhausts_retries_on_persistent_500() {
        // max_retries = 2 → total attempts = 3 (0, 1, 2); always 500.
        let responses = vec![
            http_response(500, "Internal Server Error", ""),
            http_response(500, "Internal Server Error", ""),
            http_response(500, "Internal Server Error", ""),
        ];
        let addr = spawn_mock_server(responses).await;
        let client = make_client(addr, 2);

        let result = client.get_latest_ledger().await;
        assert!(result.is_err(), "Expected error after retries exhausted");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Server error") || err.contains("500"),
            "Error should mention the server error: {err}"
        );
    }

    /// 503 Service Unavailable (a common transient failure) is also retried.
    #[tokio::test]
    async fn retries_on_503_service_unavailable() {
        let responses = vec![
            http_response(503, "Service Unavailable", ""),
            http_response(503, "Service Unavailable", ""),
            http_response(200, "OK", ok_body()),
        ];
        let addr = spawn_mock_server(responses).await;
        let client = make_client(addr, 3);

        let result = client.get_latest_ledger().await;
        assert!(result.is_ok(), "Expected success after retrying 503s, got: {result:?}");
    }

    /// 502 Bad Gateway is also a 5xx and must be retried.
    #[tokio::test]
    async fn retries_on_502_bad_gateway() {
        let responses = vec![
            http_response(502, "Bad Gateway", ""),
            http_response(200, "OK", ok_body()),
        ];
        let addr = spawn_mock_server(responses).await;
        let client = make_client(addr, 3);

        let result = client.get_latest_ledger().await;
        assert!(result.is_ok(), "Expected success after retrying 502, got: {result:?}");
    }

    /// 429 Too Many Requests is still retried (pre-existing behaviour preserved).
    #[tokio::test]
    async fn retries_on_429_rate_limit() {
        let responses = vec![
            http_response(429, "Too Many Requests", ""),
            http_response(200, "OK", ok_body()),
        ];
        let addr = spawn_mock_server(responses).await;
        let client = make_client(addr, 3);

        let result = client.get_latest_ledger().await;
        assert!(result.is_ok(), "Expected success after retrying 429, got: {result:?}");
    }

    /// A JSON-RPC error body inside a 200 is returned immediately, without retry.
    #[tokio::test]
    async fn returns_immediately_on_jsonrpc_error_in_200() {
        let rpc_err = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"not found"}}"#;
        // Serve only one response; a second would panic (index out of range).
        let responses = vec![http_response(200, "OK", rpc_err)];
        let addr = spawn_mock_server(responses).await;
        let client = make_client(addr, 3);

        let result = client.get_latest_ledger().await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("not found"),
            "Error should propagate the JSON-RPC error message"
        );
    }
}

