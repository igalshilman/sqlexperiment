//! `claude.messages`: the Claude Messages API as a host binding, so that a
//! program calls a model with `ctx.call('claude.messages', request)`.
//!
//! The request and the response are the API's own JSON
//! (`POST /v1/messages`). The host reads the request from the program's
//! heap when it sends it: neither the run's commit nor Restate's journal
//! keeps a copy of the conversation, and the response is saved with the
//! heap. A crash before the response commits sends the request again: it
//! does not deduplicate, so a program sets no key of its own. Requests are
//! sent with
//! `fallbacks: "default"` (unless they set `fallbacks`): a declined request
//! is retried on another model by the API itself. Rate limits, server and
//! network errors are retried, with backoff; other errors (a bad request, a
//! missing ANTHROPIC_API_KEY) fail the call.
use crate::{Binding, GuestError, HostCall};
use futures::future::BoxFuture;
use serde_json::Value;
use std::time::Duration;

const MESSAGES: &str = "https://api.anthropic.com/v1/messages";

#[derive(Clone)]
pub struct Claude {
    http: reqwest::Client,
    api_key: Option<String>,
}

/// Why a request failed: worth sending again, or not.
enum Failure {
    Retry(String),
    Fail(String),
}

impl Claude {
    /// Calls the API with ANTHROPIC_API_KEY, read now.
    pub fn from_env() -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
        }
    }

    async fn messages(self, call: HostCall) -> Result<Value, GuestError> {
        let HostCall {
            idempotency_key: key,
            input: mut request,
            ..
        } = call;
        let api_key = (self.api_key.clone())
            .ok_or_else(|| GuestError::new("Error", "Set ANTHROPIC_API_KEY to call Claude"))?;
        if let Some(body) = request.as_object_mut() {
            body.entry("fallbacks").or_insert_with(|| "default".into());
        }
        let mut backoff = Duration::from_millis(500);
        loop {
            match self.send(&api_key, &request).await {
                Ok(response) => return Ok(response),
                Err(Failure::Fail(error)) => return Err(GuestError::new("Error", error)),
                Err(Failure::Retry(error)) => {
                    tracing::warn!(call = key, "{error}; retrying in {backoff:?}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    async fn send(&self, api_key: &str, request: &Value) -> Result<Value, Failure> {
        let response = self
            .http
            .post(MESSAGES)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "server-side-fallback-2026-07-01")
            .json(request)
            .send()
            .await
            .map_err(|error| Failure::Retry(format!("Claude API: {error}")))?;
        let status = response.status();
        let body = (response.text().await)
            .map_err(|error| Failure::Retry(format!("Claude API: {error}")))?;
        if status.is_success() {
            return serde_json::from_str(&body)
                .map_err(|error| Failure::Fail(format!("Claude API response: {error}")));
        }
        let error = format!("Claude API {status}: {body}");
        let retryable = matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error();
        Err(if retryable {
            Failure::Retry(error)
        } else {
            Failure::Fail(error)
        })
    }
}

impl Binding for Claude {
    /// One Messages API request: its JSON in, the response's JSON out.
    fn call(&self, call: HostCall) -> BoxFuture<'static, Result<Value, GuestError>> {
        Box::pin(self.clone().messages(call))
    }
}
