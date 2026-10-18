//! A scripted `claude` service, for tests: it answers each request by the
//! steps the conversation has taken since the user's last message.
use restate_sdk::prelude::*;
use serde_json::{Value, json};
use std::sync::Mutex;

pub struct Claude;

/// Every request the fake was sent, in order.
pub static REQUESTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

#[restate_sdk::service(name = "claude")]
impl Claude {
    /// Look up the catalog and the balance, then buy two widgets, then say so.
    #[handler]
    async fn messages(
        &self,
        _ctx: Context<'_>,
        request: Json<Value>,
    ) -> HandlerResult<Json<Value>> {
        let request = request.into_inner();
        let messages = request["messages"].as_array().unwrap();
        let since_user = messages
            .iter()
            .rev()
            .take_while(|m| !m["content"].is_string())
            .filter(|m| m["role"] == "assistant")
            .count();
        REQUESTS.lock().unwrap().push(request.clone());
        let use_tool = |id: &str, name: &str, input: Value| json!({"type": "tool_use", "id": id, "name": name, "input": input});
        let (content, stop_reason) = match since_user {
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
            _ => (
                vec![json!({"type": "text", "text": "Bought 2 widgets."})],
                "end_turn",
            ),
        };
        Ok(Json(json!({
            "type": "message",
            "role": "assistant",
            "model": request["model"],
            "content": content,
            "stop_reason": stop_reason,
        })))
    }
}
