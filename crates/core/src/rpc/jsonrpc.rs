//! Generic JSON-RPC 2.0 client primitives.
//!
//! Provides strongly-typed request/response envelopes and a reusable HTTP
//! transport so every RPC call is validated at compile time via Serde.

use crate::error::{PrismError, PrismResult, JsonRpcError};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Base delay (ms) for exponential backoff: delay = `BASE_DELAY_MS × 2^attempt`.
const BASE_DELAY_MS: u64 = 100;

/// Hard ceiling on any single backoff sleep to avoid excessively long waits.
const MAX_DELAY_MS: u64 = 10_000; // 10 seconds

/// Compute the capped exponential backoff duration for a given attempt number.
///
/// Returns `BASE_DELAY_MS × 2^attempt`, clamped to [`MAX_DELAY_MS`].
fn backoff_duration(attempt: u32) -> Duration {
    let ms = BASE_DELAY_MS.saturating_mul(1u64.saturating_shl(attempt));
    Duration::from_millis(ms.min(MAX_DELAY_MS))
}


/// JSON-RPC 2.0 request envelope.
///
/// `T` is the method-specific params struct; it must implement [`Serialize`].
#[derive(Debug, Serialize)]
pub struct JsonRpcRequest<T: Serialize> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'static str,
    pub params: T,
}

impl<T: Serialize> JsonRpcRequest<T> {
    /// Construct a request with the standard `"2.0"` version string.
    pub fn new(id: u64, method: &'static str, params: T) -> Self {
        Self { jsonrpc: "2.0", id, method, params }
    }
}

/// JSON-RPC 2.0 response envelope.
///
/// `T` is the method-specific result struct; it must implement [`Deserialize`].
#[derive(Debug, Deserialize)]
pub struct JsonRpcResponse<T> {
    #[allow(dead_code)]
    pub jsonrpc: String,
    #[allow(dead_code)]
    pub id: u64,
    pub result: Option<T>,
    pub error: Option<JsonRpcError>,
}


/// Params for `getTransaction`.
#[derive(Debug, Serialize)]
pub struct GetTransactionParams {
    pub hash: String,
}

/// Params for `simulateTransaction`.
#[derive(Debug, Serialize)]
pub struct SimulateTransactionParams {
    pub transaction: String,
}

/// Params for `getLedgerEntries`.
#[derive(Debug, Serialize)]
pub struct GetLedgerEntriesParams {
    pub keys: Vec<String>,
}

/// Params for `getEvents`.
#[derive(Debug, Serialize)]
pub struct GetEventsParams {
    #[serde(rename = "startLedger")]
    pub start_ledger: u32,
    pub filters: serde_json::Value,
}

/// Params for `getLatestLedger` — the method takes no parameters.
#[derive(Debug, Serialize)]
pub struct EmptyParams {}

/// Params for `getHealth` — the method takes no parameters.
pub type GetHealthParams = EmptyParams;


/// Low-level JSON-RPC HTTP transport.
///
/// Handles serialization, deserialization, retry, and rate-limit backoff.
/// Higher-level clients (e.g. [`super::rpc::RpcClient`]) build on top of this.
pub struct JsonRpcTransport {
    client: reqwest::Client,
    endpoint: String,
    max_retries: u32,
}

impl JsonRpcTransport {
    /// Create a transport pointed at `endpoint` with the given retry limit.
    pub fn new(endpoint: impl Into<String>, max_retries: u32) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            endpoint: endpoint.into(),
            max_retries,
        }
    }

    /// Execute a typed JSON-RPC call and return the typed result.
    ///
    /// Retries are triggered by:
    /// - Transport-level failures (connection refused, timeout, etc.)
    /// - HTTP 429 Too Many Requests
    /// - HTTP 5xx Server Errors (500–599)
    ///
    /// Backoff follows `BASE_DELAY_MS × 2^attempt`, capped at `MAX_DELAY_MS`.
    pub async fn call<P, R>(&self, request: &JsonRpcRequest<P>) -> PrismResult<R>
    where
        P: Serialize + std::fmt::Debug,
        R: for<'de> Deserialize<'de>,
    {
        let method = request.method;
        let mut last_error: Option<PrismError> = None;

        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                let delay = backoff_duration(attempt);
                tracing::debug!(attempt, method, delay_ms = delay.as_millis(), "backing off before retry");
                tokio::time::sleep(delay).await;
                tracing::debug!(attempt, method, "retrying RPC request");
            }

            let started_at = Instant::now();
            tracing::debug!(method, endpoint = %self.endpoint, attempt, "sending RPC request");

            match self.client.post(&self.endpoint).json(request).send().await {
                Ok(response) => {
                    let status = response.status();
                    let elapsed_ms = started_at.elapsed().as_millis();

                    tracing::debug!(
                        method,
                        endpoint = %self.endpoint,
                        attempt,
                        status = %status,
                        elapsed_ms,
                        "RPC response received"
                    );

                    // Retry on 429 Too Many Requests.
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        tracing::warn!(method, attempt, "rate limited by RPC endpoint, will retry");
                        last_error = Some(PrismError::RpcError(format!("rate limited (attempt {attempt})")));
                        continue;
                    }

                    // Retry on any 5xx Server Error.
                    if status.is_server_error() {
                        tracing::warn!(
                            method,
                            attempt,
                            status = %status,
                            elapsed_ms,
                            "RPC endpoint returned server error (5xx), will retry"
                        );
                        last_error = Some(PrismError::RpcError(format!(
                            "server error {status} on attempt {attempt}"
                        )));
                        continue;
                    }

                    let body = response.text().await.map_err(|e| {
                        PrismError::RpcError(format!("response read error: {e}"))
                    })?;

                    tracing::trace!(method, elapsed_ms, response = %body, "RPC response payload");

                    let envelope: JsonRpcResponse<R> =
                        serde_json::from_str(&body).map_err(|e| {
                            PrismError::RpcError(format!("response parse error: {e}"))
                        })?;

                    if let Some(err) = envelope.error {
                        tracing::debug!(
                            method,
                            endpoint = %self.endpoint,
                            error = %err.message,
                            code = err.code,
                            "RPC returned error response"
                        );
                        return Err(PrismError::JsonRpc(err));
                    }

                    return envelope
                        .result
                        .ok_or_else(|| PrismError::RpcError("empty result".to_string()));
                }
                Err(e) => {
                    tracing::debug!(
                        method,
                        endpoint = %self.endpoint,
                        attempt,
                        elapsed_ms = started_at.elapsed().as_millis(),
                        error = %e,
                        "RPC request failed"
                    );
                    last_error = Some(PrismError::RpcError(format!("request failed: {e}")));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| PrismError::RpcError("unknown error".to_string())))
    }
}
