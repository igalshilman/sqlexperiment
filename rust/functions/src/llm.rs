//! `llm`: a model gateway on the host, the extension behind `ctx.llm`. A
//! program asks any provider's models in one format, and the host translates,
//! adds the provider's key, retries and caches.
//!
//! The format is the Anthropic Messages API's, provider-neutral:
//!
//! - A request: `{model, system?, messages, tools?, max_tokens?,
//!   temperature?, options?}`. `model` is `<provider>/<model>`, such as
//!   `anthropic/claude-opus-5-5` or `openai/gpt-5`. `options` go to the
//!   provider as they are, merged into its request.
//! - A message: `{role: "user" | "assistant", content}`, its content a
//!   string or blocks: `{type: "text", text}`, `{type: "tool_use", id, name,
//!   input}`, `{type: "tool_result", tool_use_id, content, is_error?}`.
//! - A response: `{content, stop_reason, usage: {input_tokens,
//!   output_tokens}, model}`, `stop_reason` one of `end_turn`, `tool_use`,
//!   `max_tokens`, `refusal`.
//!
//! A conversation (`ctx.llm.conversation(name)`) keeps its messages in a
//! table of the scope's database, `llm_messages`. In a turn, the program adds
//! its new ones (the call `llm.append`). As the host makes the model call, it
//! reads the conversation as the commit left it; in the turn the answer
//! arrives, it adds the answer, before the program sees it. So the request is
//! in neither the program's heap nor its commit.
//!
//! A call is made at least once: again after a crash, as it was.
use crate::{
    Binding, GuestError, Operation, Outcome,
    extension::{Attached, Extension, HostCall, Making, Tables},
    transaction::{Participant, Record, ScopeBlobs},
};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

/// The output a request asks for when it does not say.
const MAX_TOKENS: u64 = 16_000;
/// How many times a call is sent, at most, while its failures are worth retrying.
const ATTEMPTS: u32 = 8;

/// How a provider's API is spoken.
#[derive(Clone)]
enum Api {
    /// The Messages API: `POST /v1/messages`.
    Anthropic,
    /// Chat completions: `POST /chat/completions`, as OpenAI, xAI and many
    /// others serve it.
    OpenAi,
    /// In this process: a binding, given the request as the program sent it.
    Local(Arc<dyn Binding>),
}

/// A model provider: where it is, how it is spoken, and its key.
#[derive(Clone)]
pub struct Provider {
    api: Api,
    base_url: String,
    key: Option<String>,
}

impl Provider {
    /// Anthropic, at `https://api.anthropic.com`.
    pub fn anthropic(key: Option<String>) -> Self {
        Self {
            api: Api::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            key,
        }
    }

    /// OpenAI, at `https://api.openai.com`.
    pub fn openai(key: Option<String>) -> Self {
        Self::openai_compatible("https://api.openai.com/v1", key)
    }

    /// Any provider serving OpenAI's chat completions under `base_url`, such
    /// as xAI's `https://api.x.ai/v1`.
    pub fn openai_compatible(base_url: &str, key: Option<String>) -> Self {
        Self {
            api: Api::OpenAi,
            base_url: base_url.trim_end_matches('/').into(),
            key,
        }
    }

    /// A provider in this process: `binding` answers each request, in the
    /// gateway's format, as the program sent it (a scripted model, say).
    pub fn local(binding: impl Binding) -> Self {
        Self {
            api: Api::Local(Arc::new(binding)),
            base_url: String::new(),
            key: None,
        }
    }

    /// This provider, at `base_url` instead: a proxy, or a fake in tests.
    pub fn at(mut self, base_url: &str) -> Self {
        self.base_url = base_url.trim_end_matches('/').into();
        self
    }
}

/// The gateway: the providers a deployment offers, by name.
#[derive(Clone)]
pub struct Llm {
    http: reqwest::Client,
    providers: Arc<BTreeMap<String, Provider>>,
    backoff: Duration,
}

impl Default for Llm {
    fn default() -> Self {
        Self::new()
    }
}

impl Llm {
    /// A gateway with no providers.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            providers: Arc::default(),
            backoff: Duration::from_millis(500),
        }
    }

    /// `anthropic`, `openai` and `xai`, with ANTHROPIC_API_KEY,
    /// OPENAI_API_KEY and XAI_API_KEY.
    pub fn from_env() -> Self {
        let key = |name| std::env::var(name).ok();
        Self::new()
            .provider("anthropic", Provider::anthropic(key("ANTHROPIC_API_KEY")))
            .provider("openai", Provider::openai(key("OPENAI_API_KEY")))
            .provider(
                "xai",
                Provider::openai_compatible("https://api.x.ai/v1", key("XAI_API_KEY")),
            )
    }

    /// Offer `provider` as `name`: models `<name>/<model>`.
    pub fn provider(mut self, name: &str, provider: Provider) -> Self {
        Arc::make_mut(&mut self.providers).insert(name.into(), provider);
        self
    }

    /// Wait `backoff` before the first retry, twice as long before each next.
    pub fn backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    async fn complete(self, call: HostCall) -> Result<Value, GuestError> {
        let request = call.input;
        let model = request["model"].as_str().unwrap_or_default();
        let (name, model) = model.split_once('/').ok_or_else(|| {
            type_error(format!(
                "A model is <provider>/<model>, such as anthropic/claude-opus-5-5, not {model:?}"
            ))
        })?;
        let provider = (self.providers.get(name))
            .ok_or_else(|| type_error(format!("No provider {name:?}")))?;
        if let Api::Local(binding) = &provider.api {
            return binding
                .call(HostCall {
                    input: request,
                    ..call
                })
                .await;
        }
        let key = (provider.key.as_deref())
            .ok_or_else(|| GuestError::new("Error", format!("No API key for {name}")))?;
        let body = match provider.api {
            Api::Anthropic => anthropic::request(model, &request)?,
            _ => openai::request(model, &request)?,
        };
        let response = self
            .send(provider, key, &body, &call.idempotency_key)
            .await?;
        Ok(match provider.api {
            Api::Anthropic => anthropic::response(response),
            _ => openai::response(response),
        })
    }

    /// Send `body`, retrying rate limits, overloads and network errors.
    async fn send(
        &self,
        provider: &Provider,
        key: &str,
        body: &Value,
        call: &str,
    ) -> Result<Value, GuestError> {
        let mut backoff = self.backoff;
        let mut attempt = 1;
        loop {
            let request = match provider.api {
                Api::Anthropic => (self.http)
                    .post(format!("{}/messages", provider.base_url))
                    .header("x-api-key", key)
                    .header("anthropic-version", "2023-06-01"),
                _ => (self.http)
                    .post(format!("{}/chat/completions", provider.base_url))
                    .bearer_auth(key),
            };
            let error = match request.json(body).send().await {
                Ok(response) => {
                    let status = response.status();
                    match response.text().await {
                        Ok(text) if status.is_success() => {
                            return serde_json::from_str(&text).map_err(|error| {
                                GuestError::new("Error", format!("Response is not JSON: {error}"))
                            });
                        }
                        Ok(text) => {
                            let error = format!("{status}: {text}");
                            let retry = matches!(status.as_u16(), 408 | 409 | 429 | 529)
                                || status.is_server_error();
                            if !retry {
                                return Err(GuestError::new("Error", error));
                            }
                            error
                        }
                        Err(error) => error.to_string(),
                    }
                }
                Err(error) => error.to_string(),
            };
            if attempt == ATTEMPTS {
                return Err(GuestError::new("Error", error));
            }
            tracing::warn!(call, "{error}; retrying in {backoff:?}");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
            attempt += 1;
        }
    }
}

#[async_trait]
impl Extension for Llm {
    async fn attach(
        &self,
        _blobs: ScopeBlobs,
        _record: Record,
    ) -> anyhow::Result<Box<dyn Attached>> {
        Ok(Box::new(Asking {
            llm: self.clone(),
            asked: HashMap::new(),
        }))
    }
}

/// The gateway in a run: the conversations its model calls ask for.
struct Asking {
    llm: Llm,
    /// Each model call made in this attempt, by operation: its conversation.
    asked: HashMap<String, String>,
}

/// Its conversations are in the scope's tables, which keep them.
impl Participant for Asking {}

#[async_trait]
impl Attached for Asking {
    /// `llm.append`, `llm.history`, `llm.list` and `llm.clear`, on the
    /// conversations' table in the turn.
    async fn call(
        &mut self,
        tables: &mut Tables<'_>,
        op: &str,
        input: Value,
    ) -> Result<Value, GuestError> {
        let named = || match input["conversation"].as_str() {
            Some(name) if !name.is_empty() => Ok(name.to_owned()),
            _ => Err(type_error("A conversation has a name")),
        };
        let answer = match op {
            "llm.append" => {
                let messages = (input["messages"].as_array())
                    .ok_or_else(|| type_error("messages is an array"))?;
                conversations::append(tables, &named()?, messages)
                    .await
                    .map(|()| Value::Null)
            }
            "llm.history" => conversations::history(tables, &named()?)
                .await
                .map(Value::Array),
            "llm.list" => conversations::list(tables).await.map(Value::Array),
            "llm.clear" => conversations::clear(tables, &named()?)
                .await
                .map(|()| Value::Null),
            _ => return Err(type_error(format!("No call {op}"))),
        };
        answer.map_err(|error| GuestError::new("Error", format!("{error:#}")))
    }

    /// A model call: a conversation's reads its messages, as the commit left
    /// them.
    async fn operation(
        &mut self,
        mut call: HostCall,
        tables: &mut Tables<'_>,
    ) -> Result<Making, GuestError> {
        if let Some(Value::String(conversation)) = call.input.get("conversation").cloned() {
            let messages = (conversations::history(tables, &conversation).await)
                .map_err(|error| GuestError::new("Error", format!("{error:#}")))?;
            call.input["messages"] = Value::Array(messages);
            call.input
                .as_object_mut()
                .map(|input| input.remove("conversation"));
            self.asked.insert(call.id.clone(), conversation);
        }
        Ok(Box::pin(self.llm.clone().complete(call)))
    }

    /// A conversation's answer joins it, in the turn it arrives.
    async fn delivered(
        &mut self,
        tables: &mut Tables<'_>,
        operation: &Operation,
        outcome: &mut Outcome,
    ) -> anyhow::Result<()> {
        let Some(conversation) = self.asked.remove(operation.id()) else {
            return Ok(());
        };
        if let Outcome::Success { value, .. } = outcome {
            let answer = json!({"role": "assistant", "content": value["content"]});
            conversations::append(tables, &conversation, &[answer]).await?;
        }
        Ok(())
    }
}

/// The conversations' table: each message of each, in order.
mod conversations {
    use super::*;
    use anyhow::{Context, Result};

    const TABLE: &str = "CREATE TABLE IF NOT EXISTS llm_messages(\
        conversation TEXT NOT NULL, seq INTEGER NOT NULL, message TEXT NOT NULL, \
        PRIMARY KEY (conversation, seq))";

    pub async fn history(tables: &mut Tables<'_>, conversation: &str) -> Result<Vec<Value>> {
        tables.execute(TABLE, &[]).await?;
        let select = "SELECT message FROM llm_messages WHERE conversation = ? ORDER BY seq";
        let rows = tables.query(select, &[conversation.into()]).await?;
        (rows.iter())
            .map(|row| {
                Ok(serde_json::from_str(
                    row[0].as_str().context("A message is text")?,
                )?)
            })
            .collect()
    }

    pub async fn append(
        tables: &mut Tables<'_>,
        conversation: &str,
        messages: &[Value],
    ) -> Result<()> {
        tables.execute(TABLE, &[]).await?;
        let next = "SELECT COALESCE(MAX(seq) + 1, 0) FROM llm_messages WHERE conversation = ?";
        let rows = tables.query(next, &[conversation.into()]).await?;
        let next = rows[0][0].as_i64().context("No next message")?;
        for (seq, message) in (next..).zip(messages) {
            let insert = "INSERT INTO llm_messages VALUES (?, ?, ?)";
            let row = [conversation.into(), seq.into(), message.to_string().into()];
            tables.execute(insert, &row).await?;
        }
        Ok(())
    }

    /// The conversations' names, in the order they began.
    pub async fn list(tables: &mut Tables<'_>) -> Result<Vec<Value>> {
        tables.execute(TABLE, &[]).await?;
        let select =
            "SELECT conversation FROM llm_messages GROUP BY conversation ORDER BY MIN(rowid)";
        Ok(tables
            .query(select, &[])
            .await?
            .into_iter()
            .map(|row| row[0].clone())
            .collect())
    }

    pub async fn clear(tables: &mut Tables<'_>, conversation: &str) -> Result<()> {
        tables.execute(TABLE, &[]).await?;
        let delete = "DELETE FROM llm_messages WHERE conversation = ?";
        tables.execute(delete, &[conversation.into()]).await
    }
}

fn type_error(message: impl Into<String>) -> GuestError {
    GuestError::new("TypeError", message)
}

/// A message's content as blocks: a string is one text block.
fn blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        Value::Array(blocks) => blocks.clone(),
        _ => Vec::new(),
    }
}

/// `options` merged into `body`: the provider's own parameters.
fn merge(mut body: Value, request: &Value) -> Result<Value, GuestError> {
    match &request["options"] {
        Value::Null => {}
        Value::Object(options) => {
            for (key, value) in options {
                body[key] = value.clone();
            }
        }
        _ => return Err(type_error("options is an object")),
    }
    Ok(body)
}

mod anthropic {
    use super::*;

    /// The gateway's format is the Messages API's: the request goes as it is,
    /// with a cache breakpoint at the conversation's end, so that each call
    /// reads the conversation so far from the cache.
    pub fn request(model: &str, request: &Value) -> Result<Value, GuestError> {
        let mut messages = request["messages"].as_array().cloned().unwrap_or_default();
        if let Some(last) = messages.last_mut() {
            let mut content = blocks(&last["content"]);
            if let Some(block) = content.last_mut() {
                block["cache_control"] = json!({"type": "ephemeral"});
            }
            last["content"] = content.into();
        }
        let mut body = json!({
            "model": model,
            "max_tokens": request["max_tokens"].as_u64().unwrap_or(MAX_TOKENS),
            "messages": messages,
        });
        for field in ["system", "tools", "temperature"] {
            if !request[field].is_null() {
                body[field] = request[field].clone();
            }
        }
        merge(body, request)
    }

    pub fn response(response: Value) -> Value {
        json!({
            "content": response["content"],
            "stop_reason": response["stop_reason"],
            "usage": {
                "input_tokens": response["usage"]["input_tokens"],
                "output_tokens": response["usage"]["output_tokens"],
            },
            "model": response["model"],
        })
    }
}

mod openai {
    use super::*;

    /// Messages as chat completions has them: the system prompt first, a
    /// tool's results as `tool` messages, a model's tool calls as
    /// `tool_calls`.
    pub fn request(model: &str, request: &Value) -> Result<Value, GuestError> {
        let mut messages = Vec::new();
        if let Some(system) = request["system"].as_str() {
            messages.push(json!({"role": "system", "content": system}));
        }
        for message in request["messages"].as_array().into_iter().flatten() {
            let content = blocks(&message["content"]);
            let text = |block: &Value| (block["type"] == "text").then(|| block["text"].clone());
            match message["role"].as_str() {
                Some("user") => {
                    for block in content.iter().filter(|b| b["type"] == "tool_result") {
                        let mut result = tool_result_text(&block["content"]);
                        if block["is_error"] == true {
                            result = format!("Error: {result}");
                        }
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": block["tool_use_id"],
                            "content": result,
                        }));
                    }
                    let parts: Vec<Value> = content.iter().filter_map(text).collect();
                    if !parts.is_empty() {
                        let parts: Vec<Value> = (parts.into_iter())
                            .map(|text| json!({"type": "text", "text": text}))
                            .collect();
                        messages.push(json!({"role": "user", "content": parts}));
                    }
                }
                Some("assistant") => {
                    let text: Vec<String> = (content.iter().filter_map(text))
                        .filter_map(|text| text.as_str().map(str::to_owned))
                        .collect();
                    let calls: Vec<Value> = (content.iter())
                        .filter(|block| block["type"] == "tool_use")
                        .map(|block| {
                            json!({
                                "id": block["id"],
                                "type": "function",
                                "function": {
                                    "name": block["name"],
                                    "arguments": block["input"].to_string(),
                                },
                            })
                        })
                        .collect();
                    let mut message = json!({"role": "assistant", "content": text.join("\n")});
                    if !calls.is_empty() {
                        message["tool_calls"] = calls.into();
                    }
                    messages.push(message);
                }
                role => {
                    return Err(type_error(format!(
                        "A message's role is user or assistant, not {role:?}"
                    )));
                }
            }
        }
        let mut body = json!({
            "model": model,
            "max_completion_tokens": request["max_tokens"].as_u64().unwrap_or(MAX_TOKENS),
            "messages": messages,
        });
        if let Some(tools) = request["tools"].as_array() {
            let tools: Vec<Value> = (tools.iter())
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool["name"],
                            "description": tool["description"],
                            "parameters": tool["input_schema"],
                        },
                    })
                })
                .collect();
            body["tools"] = tools.into();
        }
        if !request["temperature"].is_null() {
            body["temperature"] = request["temperature"].clone();
        }
        merge(body, request)
    }

    /// A tool result's content, as text: a string, or its text blocks.
    fn tool_result_text(content: &Value) -> String {
        match content {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => (blocks.iter())
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            other => other.to_string(),
        }
    }

    pub fn response(response: Value) -> Value {
        let choice = &response["choices"][0];
        let message = &choice["message"];
        let mut content = Vec::new();
        if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
            content.push(json!({"type": "text", "text": text}));
        }
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            let arguments = call["function"]["arguments"].as_str().unwrap_or("{}");
            let input =
                serde_json::from_str(arguments).unwrap_or_else(|_| Value::Object(Map::new()));
            content.push(json!({
                "type": "tool_use",
                "id": call["id"],
                "name": call["function"]["name"],
                "input": input,
            }));
        }
        let stop_reason = match choice["finish_reason"].as_str() {
            Some("tool_calls") => "tool_use",
            Some("length") => "max_tokens",
            Some("content_filter") => "refusal",
            _ if message["refusal"].is_string() => "refusal",
            _ => "end_turn",
        };
        json!({
            "content": content,
            "stop_reason": stop_reason,
            "usage": {
                "input_tokens": response["usage"]["prompt_tokens"],
                "output_tokens": response["usage"]["completion_tokens"],
            },
            "model": response["model"],
        })
    }
}
