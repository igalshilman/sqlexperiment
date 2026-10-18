//! `claude/messages`: the Claude Messages API as a Restate service, so that a
//! program calls a model as it calls anything else, with
//! `ctx.rpc.call({ service: 'claude', handler: 'messages' }, request)`.
//!
//! The request and the response are the API's own JSON
//! (`POST /v1/messages`). The call is journaled (`ctx.run`): once answered,
//! a retry gets the same answer, and the program's call to the service is
//! attached to, not made again. Requests are sent with `fallbacks: "default"`
//! (unless they set `fallbacks`): a declined request is retried on another
//! model by the API itself. Rate limits and server errors are retried; other
//! errors (a bad request, a missing ANTHROPIC_API_KEY) fail the call.
use restate_sdk::prelude::*;
use serde_json::Value;

const MESSAGES: &str = "https://api.anthropic.com/v1/messages";

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

    async fn send(&self, api_key: &str, request: &Value) -> HandlerResult<Json<Value>> {
        let response = self
            .http
            .post(MESSAGES)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "server-side-fallback-2026-07-01")
            .json(request)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if status.is_success() {
            return Ok(Json(serde_json::from_str(&body)?));
        }
        let error = format!("Claude API {status}: {body}");
        let retryable = matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error();
        if retryable {
            Err(std::io::Error::other(error).into())
        } else {
            Err(TerminalError::new(error).into())
        }
    }
}

#[restate_sdk::service(name = "claude")]
impl Claude {
    /// One Messages API request: its JSON in, the response's JSON out.
    #[handler]
    async fn messages(&self, ctx: Context<'_>, request: Json<Value>) -> HandlerResult<Json<Value>> {
        let api_key = self
            .api_key
            .clone()
            .ok_or_else(|| TerminalError::new("Set ANTHROPIC_API_KEY to call Claude"))?;
        let mut request = request.into_inner();
        if let Some(body) = request.as_object_mut() {
            body.entry("fallbacks").or_insert_with(|| "default".into());
        }
        let response = ctx
            .run(|| self.send(&api_key, &request))
            .name("messages")
            .await?;
        Ok(response)
    }
}
