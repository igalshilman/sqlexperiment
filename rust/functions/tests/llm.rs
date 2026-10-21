//! `ctx.llm` against fake Anthropic and OpenAI APIs on this machine: the
//! gateway's one format in, each provider's own on the wire, and
//! conversations kept in the scope's database.
mod common;

use common::{Store, attempt, catalog, make, programs};
use functions::{Extensions, Language, Llm, Programs, Provider};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// A request the fake received: its path, headers and JSON body.
#[derive(Clone, Debug)]
struct Received {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

/// A fake model API: Anthropic's at `/v1/messages`, OpenAI's at
/// `/v1/chat/completions`. Its model adds: asked first, it calls the tool
/// `add` with 2 and 3; given the result, it answers with it. It answers a
/// model named `busy` with a 429 the first time, and `bad` with a 400.
struct Fake {
    url: String,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Fake {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = received.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let log = log.clone();
                tokio::spawn(async move {
                    let Some(request) = read(&mut stream).await else {
                        return;
                    };
                    let busy = {
                        let mut log = log.lock().unwrap();
                        log.push(request.clone());
                        let model = &request.body["model"];
                        model == "busy"
                            && log.iter().filter(|r| r.body["model"] == *model).count() == 1
                    };
                    let (status, body) = answer(&request, busy);
                    let body = body.to_string();
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Self { url, received }
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    fn llm(&self) -> Llm {
        Llm::new()
            .provider(
                "anthropic",
                Provider::anthropic(Some("k-anthropic".into())).at(&self.url),
            )
            .provider(
                "openai",
                Provider::openai(Some("k-openai".into())).at(&self.url),
            )
            .provider("nokey", Provider::openai(None).at(&self.url))
            .backoff(Duration::from_millis(10))
    }
}

async fn read(stream: &mut tokio::net::TcpStream) -> Option<Received> {
    let mut data = Vec::new();
    let mut buffer = [0; 65536];
    let end = loop {
        let n = stream.read(&mut buffer).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buffer[..n]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
    };
    let head = String::from_utf8(data[..end].to_vec()).ok()?;
    let mut lines = head.split("\r\n");
    let path = lines.next()?.split(' ').nth(1)?.to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_lowercase(), value.to_owned()))
        .collect();
    let length: usize = headers.get("content-length")?.parse().ok()?;
    let mut body = data[end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut buffer).await.ok()?;
        body.extend_from_slice(&buffer[..n]);
    }
    let body = serde_json::from_slice(&body).ok()?;
    Some(Received {
        path,
        headers,
        body,
    })
}

fn answer(request: &Received, busy: bool) -> (u16, Value) {
    if busy {
        return (429, json!({"error": "slow down"}));
    }
    if request.body["model"] == "bad" {
        return (400, json!({"error": "no such model"}));
    }
    let messages = request.body["messages"].as_array().unwrap();
    let asked = !messages.iter().any(|m| m["role"] == "assistant");
    if request.path == "/v1/messages" {
        let content = if asked {
            json!([{"type": "tool_use", "id": "toolu_1", "name": "add", "input": {"a": 2, "b": 3}}])
        } else {
            let result = &messages.last().unwrap()["content"][0]["content"];
            json!([{"type": "text", "text": format!("It is {}.", result.as_str().unwrap())}])
        };
        let stop = if asked { "tool_use" } else { "end_turn" };
        let usage = json!({"input_tokens": 10, "output_tokens": 5});
        let model = &request.body["model"];
        (
            200,
            json!({"content": content, "stop_reason": stop, "usage": usage, "model": model}),
        )
    } else {
        let message = if asked {
            let call = json!({"id": "call_1", "type": "function", "function": {"name": "add", "arguments": "{\"a\":2,\"b\":3}"}});
            json!({"role": "assistant", "content": null, "tool_calls": [call]})
        } else {
            let result = messages.last().unwrap()["content"].as_str().unwrap();
            json!({"role": "assistant", "content": format!("It is {result}.")})
        };
        let finish = if asked { "tool_calls" } else { "stop" };
        let usage = json!({"prompt_tokens": 10, "completion_tokens": 5});
        let model = &request.body["model"];
        (
            200,
            json!({"choices": [{"message": message, "finish_reason": finish}], "usage": usage, "model": model}),
        )
    }
}

/// The program: asks `model` what 2 + 3 is, in the conversation `main`,
/// with the tool `add`, until it answers.
const ADD: &str = r#"
const TOOLS = [{
  name: 'add',
  description: 'Add two numbers.',
  input_schema: { type: 'object', properties: { a: { type: 'number' }, b: { type: 'number' } } },
}];

export async function main({ model }, ctx) {
  const chat = ctx.llm.conversation('main', { model, system: 'You add.', tools: TOOLS });
  let response = await chat.send({ messages: [{ role: 'user', content: 'What is 2 + 3?' }] });
  while (response.stop_reason === 'tool_use') {
    const results = response.content
      .filter((block) => block.type === 'tool_use')
      .map(({ id, input }) => ({ type: 'tool_result', tool_use_id: id, content: String(input.a + input.b) }));
    response = await chat.send({ messages: [{ role: 'user', content: results }] });
  }
  return { answer: response.content[0].text, usage: response.usage, history: chat.history() };
}
"#;

/// Run `main(input)` of `catalog`'s program in `store`, with `llm` as its
/// extension `llm`, until it settles: its outcome.
async fn drive(store: &Store, catalog: &Arc<Programs>, input: Value, llm: &Llm) -> Value {
    common::drive(store, catalog, input, &offered(llm)).await
}

fn offered(llm: &Llm) -> Extensions {
    Extensions::new().with("llm", llm.clone())
}

#[tokio::test]
async fn a_conversation_with_claude_keeps_its_messages_in_the_scope() {
    let fake = Fake::start().await;
    let input = json!({"model": "anthropic/claude-test"});
    let done = drive(&Store::default(), &programs(ADD), input, &fake.llm()).await;
    assert_eq!(done["answer"], "It is 5.", "{done}");
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 10, "output_tokens": 5})
    );
    let roles: Vec<&Value> = done["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| &m["role"])
        .collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);

    // Each request: the whole conversation, from the scope's database, with
    // the program's defaults, in the Messages API's own format.
    let [first, second] = &fake.received()[..] else {
        panic!("Expected two requests")
    };
    assert_eq!(second.path, "/v1/messages");
    assert_eq!(second.headers["x-api-key"], "k-anthropic");
    assert_eq!(second.body["model"], "claude-test");
    assert_eq!(second.body["system"], "You add.");
    assert_eq!(second.body["tools"][0]["name"], "add");
    assert_eq!(first.body["messages"].as_array().unwrap().len(), 1);
    let messages = second.body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["content"][0]["type"], "tool_use");
    // A cache breakpoint at the conversation's end: the next request reads
    // all before it from the cache.
    assert_eq!(
        messages[2]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert!(messages[0]["content"][0].get("cache_control").is_none());
}

#[tokio::test]
async fn the_same_program_talks_to_openai() {
    let fake = Fake::start().await;
    let input = json!({"model": "openai/gpt-test"});
    let done = drive(&Store::default(), &programs(ADD), input, &fake.llm()).await;
    assert_eq!(done["answer"], "It is 5.", "{done}");
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 10, "output_tokens": 5})
    );
    // The model's tool call, in the gateway's format.
    let call = &done["history"][1]["content"][0];
    assert_eq!(
        *call,
        json!({"type": "tool_use", "id": "call_1", "name": "add", "input": {"a": 2, "b": 3}})
    );

    // On the wire, chat completions: the system prompt first, the tool's
    // result as a `tool` message, the model's call as `tool_calls`.
    let second = &fake.received()[1];
    assert_eq!(second.path, "/v1/chat/completions");
    assert_eq!(second.headers["authorization"], "Bearer k-openai");
    assert_eq!(second.body["model"], "gpt-test");
    assert_eq!(second.body["tools"][0]["function"]["name"], "add");
    let messages = second.body["messages"].as_array().unwrap();
    let roles: Vec<&Value> = messages.iter().map(|m| &m["role"]).collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool"]);
    assert_eq!(
        messages[2]["tool_calls"][0]["function"]["arguments"],
        "{\"a\":2,\"b\":3}"
    );
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "call_1", "content": "5"})
    );
}

#[tokio::test]
async fn a_call_is_made_again_after_a_crash_from_the_history() {
    let fake = Fake::start().await;
    let llm = fake.llm();
    let store = Store::default();
    let catalog = programs(ADD);
    let input = json!({"model": "anthropic/claude-test"});
    // The first call is made, and the attempt crashes before its answer
    // commits.
    let mut run = attempt(&store, &catalog, input.clone(), &offered(&llm)).await;
    let id = run.view().state.pending[0].id().to_owned();
    make(&mut run, &id).await.unwrap();
    drop(run);

    let done = drive(&store, &catalog, input, &llm).await;
    assert_eq!(done["answer"], "It is 5.");
    // Made again as it was; the question was added once, and the answer too.
    let received = fake.received();
    assert_eq!(received.len(), 3);
    assert_eq!(received[0].body, received[1].body);
    assert_eq!(done["history"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn requests_are_whole_without_a_conversation() {
    let fake = Fake::start().await;
    let source = r#"
export async function main(_, ctx) {
  const ask = (model) => ctx.llm.complete({ model, messages: [{ role: 'user', content: 'What is 2 + 3?' }] });
  const errors = [];
  for (const model of ['anthropic/bad', 'nobody/x', 'claude', 'nokey/x']) {
    try {
      await ask(model);
    } catch (error) {
      errors.push(error.message);
    }
  }
  const chat = ctx.llm.conversation('c', { model: 'anthropic/claude-test' });
  const both = [chat.send({ messages: [{ role: 'user', content: 'Hi' }] }), chat.send({ messages: [] })];
  try {
    await both[1];
  } catch (error) {
    errors.push(error.message);
  }
  await both[0];
  return { busy: (await ask('anthropic/busy')).stop_reason, errors, conversation: chat.history().length };
}
"#;
    let done = drive(
        &Store::default(),
        &programs(source),
        Value::Null,
        &fake.llm(),
    )
    .await;
    assert_eq!(
        done["errors"],
        json!([
            "400 Bad Request: {\"error\":\"no such model\"}",
            "No provider \"nobody\"",
            "A model is <provider>/<model>, such as anthropic/claude-opus-5-5, not \"claude\"",
            "No API key for nokey",
            "The conversation c is waiting for the model",
        ]),
        "{done}"
    );
    // A 429 is retried.
    assert_eq!(done["busy"], "tool_use");
    let busy: Vec<_> = (fake.received().into_iter())
        .filter(|r| r.body["model"] == "busy")
        .collect();
    assert_eq!(busy.len(), 2);
    assert_eq!(done["conversation"], 2);
}

#[tokio::test]
async fn python_programs_converse_too() {
    let fake = Fake::start().await;
    let source = r#"
TOOLS = [{"name": "add", "description": "Add two numbers.", "input_schema": {"type": "object"}}]


async def main(input, ctx):
    chat = ctx.llm.conversation("main", model=input["model"], system="You add.", tools=TOOLS)
    response = await chat.send({"role": "user", "content": "What is 2 + 3?"})
    while response["stop_reason"] == "tool_use":
        results = [
            {"type": "tool_result", "tool_use_id": block["id"], "content": str(block["input"]["a"] + block["input"]["b"])}
            for block in response["content"]
            if block["type"] == "tool_use"
        ]
        response = await chat.send({"role": "user", "content": results})
    return {"answer": response["content"][0]["text"], "history": len(chat.history())}
"#;
    let catalog = catalog(Language::Python, source);
    let input = json!({"model": "openai/gpt-test"});
    let done = drive(&Store::default(), &catalog, input, &fake.llm()).await;
    assert_eq!(done, json!({"answer": "It is 5.", "history": 4}));
}

#[tokio::test]
async fn conversations_stay_and_are_listed_in_the_order_they_began() {
    let fake = Fake::start().await;
    let source = r#"
export async function main({ model }, ctx) {
  for (const name of ['task-2', 'task-10', 'scratch']) {
    await ctx.llm.conversation(name, { model }).send({ messages: [{ role: 'user', content: name }] });
  }
  const before = ctx.llm.conversations();
  ctx.llm.conversation('scratch').clear();
  return { before, after: ctx.llm.conversations(), kept: ctx.llm.conversation('task-10').history().length };
}
"#;
    let input = json!({"model": "anthropic/claude-test"});
    let done = drive(&Store::default(), &programs(source), input, &fake.llm()).await;
    assert_eq!(
        done,
        json!({
            "before": ["task-2", "task-10", "scratch"],
            "after": ["task-2", "task-10"],
            "kept": 2,
        })
    );
}
