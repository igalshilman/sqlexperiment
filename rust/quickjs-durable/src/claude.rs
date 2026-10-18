//! `ctx.claude.messages(request)`: Claude's Messages API, called by the run's
//! handler itself, not through Restate. The request and the response are the
//! API's own JSON (`POST /v1/messages`).
//!
//! A long agent's request is its whole conversation, so nothing stores it:
//! the guest keeps it in the heap, which is saved anyway, and the handler
//! reads it from there when it sends it ([`crate::Run::request`]). The
//! response comes back to the VM, and is saved with the heap at the next
//! commit. A crash before that sends the request again.
//!
//! Requests go with `fallbacks: "default"` (unless they set `fallbacks`): a
//! declined request is retried on another model by the API itself. Rate
//! limits, overloads, server and network errors are retried here, with
//! backoff; other errors (a bad request, no ANTHROPIC_API_KEY) fail the call.
use anyhow::{anyhow, bail};
use futures::future::BoxFuture;
use serde_json::Value;
use std::time::Duration;

const MESSAGES: &str = "https://api.anthropic.com/v1/messages";
const ATTEMPTS: u32 = 8;

/// Where `ctx.claude.messages` goes: [`Claude`], or a fake in tests.
pub trait Model: Send + Sync {
    /// One Messages API request: its JSON in, the response's JSON out.
    fn messages(&self, request: Value) -> BoxFuture<'static, anyhow::Result<Value>>;
}

/// The Claude API, with ANTHROPIC_API_KEY.
pub struct Claude {
    http: reqwest::Client,
    api_key: Option<String>,
}

impl Claude {
    /// Calls the API with ANTHROPIC_API_KEY, read now.
    pub fn from_env() -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
        }
    }
}

impl Model for Claude {
    fn messages(&self, mut request: Value) -> BoxFuture<'static, anyhow::Result<Value>> {
        let (http, api_key) = (self.http.clone(), self.api_key.clone());
        Box::pin(async move {
            let api_key = api_key.ok_or_else(|| anyhow!("Set ANTHROPIC_API_KEY to call Claude"))?;
            if let Some(body) = request.as_object_mut() {
                body.entry("fallbacks").or_insert_with(|| "default".into());
            }
            let mut backoff = Duration::from_secs(1);
            for attempt in 1.. {
                match send(&http, &api_key, &request).await {
                    Ok(response) => return Ok(response),
                    Err(Failed::Retryable(error, wait)) if attempt < ATTEMPTS => {
                        tracing::warn!("{error}; retrying");
                        tokio::time::sleep(wait.unwrap_or(backoff)).await;
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                    }
                    Err(Failed::Retryable(error, _) | Failed::Final(error)) => bail!(error),
                }
            }
            unreachable!()
        })
    }
}

enum Failed {
    /// Worth another try, after the time the API asked for, if it did.
    Retryable(String, Option<Duration>),
    Final(String),
}

async fn send(http: &reqwest::Client, api_key: &str, request: &Value) -> Result<Value, Failed> {
    let response = http
        .post(MESSAGES)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "server-side-fallback-2026-07-01")
        .json(request)
        .send()
        .await
        .map_err(|error| Failed::Retryable(format!("Claude API: {error}"), None))?;
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok()?.parse().ok())
        .map(Duration::from_secs);
    let body = response
        .text()
        .await
        .map_err(|error| Failed::Retryable(format!("Claude API: {error}"), None))?;
    if status.is_success() {
        return serde_json::from_str(&body).map_err(|error| Failed::Final(error.to_string()));
    }
    let error = format!("Claude API {status}: {body}");
    if matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error() {
        Err(Failed::Retryable(error, retry_after))
    } else {
        Err(Failed::Final(error))
    }
}
