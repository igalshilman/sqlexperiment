//! A scripted model, as the `llm` binding for tests: it answers each request
//! in the gateway's format by the steps the conversation has taken, as the
//! shop assistant or, offered `bash`, as the coding agent.
use functions::{Binding, GuestError, HostCall};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::Mutex;

#[derive(Clone)]
pub struct Model;

/// Every request the fake was sent, in order, its conversation's messages
/// read from the scope.
pub static REQUESTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

impl Binding for Model {
    fn call(&self, call: HostCall) -> BoxFuture<'static, Result<Value, GuestError>> {
        let request = call.input;
        assert!(request["model"].as_str().unwrap().starts_with("openai/"));
        REQUESTS.lock().unwrap().push(request.clone());
        let messages = request["messages"].as_array().unwrap();
        let codes = request["tools"][0]["name"] == "bash";
        let (content, stop_reason) = if codes {
            coder(messages)
        } else {
            shop(messages)
        };
        let response = json!({
            "type": "message",
            "role": "assistant",
            "model": request["model"],
            "content": content,
            "stop_reason": stop_reason,
        });
        Box::pin(async move { Ok(response) })
    }
}

fn use_tool(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

fn text(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// Look up the catalog and the balance, then buy two widgets, then say so.
fn shop(messages: &[Value]) -> (Vec<Value>, &'static str) {
    let since_user = messages
        .iter()
        .rev()
        .take_while(|m| !m["content"].is_string())
        .filter(|m| m["role"] == "assistant")
        .count();
    match since_user {
        0 => (
            vec![
                use_tool("toolu_1", "catalog", json!({})),
                use_tool("toolu_2", "balance", json!({})),
            ],
            "tool_use",
        ),
        1 => (
            vec![use_tool(
                "toolu_3",
                "buy",
                json!({"items": [{"sku": "widget", "qty": 2}]}),
            )],
            "tool_use",
        ),
        _ => (vec![text("Bought 2 widgets.")], "end_turn"),
    }
}

/// Write a script and run it, fail to edit and read what is not there, edit
/// the script and run it again, then say so.
fn coder(messages: &[Value]) -> (Vec<Value>, &'static str) {
    let steps = messages.iter().filter(|m| m["role"] == "assistant").count();
    let script = "echo hello from $1";
    let tools = match steps {
        0 => vec![
            use_tool(
                "toolu_1",
                "write_file",
                json!({"path": "bin/hello.sh", "content": script}),
            ),
            use_tool(
                "toolu_2",
                "bash",
                json!({"command": "sh bin/hello.sh world"}),
            ),
        ],
        1 => vec![
            use_tool(
                "toolu_3",
                "edit_file",
                json!({"path": "bin/hello.sh", "old_text": "hi", "new_text": "bye"}),
            ),
            use_tool("toolu_4", "read_file", json!({"path": "missing.txt"})),
            use_tool("toolu_5", "read_file", json!({"path": "../outside"})),
        ],
        2 => vec![
            use_tool(
                "toolu_6",
                "edit_file",
                json!({"path": "bin/hello.sh", "old_text": "hello", "new_text": "goodbye"}),
            ),
            use_tool(
                "toolu_7",
                "bash",
                json!({"command": "sh bin/hello.sh world; echo oops >&2; exit 3"}),
            ),
        ],
        _ => return (vec![text("Done.")], "end_turn"),
    };
    (tools, "tool_use")
}
