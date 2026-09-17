use futures::StreamExt;
use tracing::{debug, warn};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Reasoning,
    Generating,
    Tool,
}

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    pub fn arg(&self, name: &str) -> Option<serde_json::Value> {
        let mut args: serde_json::Value = serde_json::from_str(&self.arguments).ok()?;
        Some(args[name].take())
    }
}

// what one streamed turn produced. `assistant` is the turn to append before
// asking again; `images` holds data uris a provider-run image tool produced.
pub struct Completion {
    pub assistant: serde_json::Value,
    pub text: String,
    pub images: Vec<String>,
    pub calls: Vec<ToolCall>,
}

pub struct AiClient {
    http: reqwest::Client,
    base: String,
    key: String,
    model: String,
    system: Option<String>,
    reasoning_budget: i64,
    tools: Vec<serde_json::Value>,
}

impl AiClient {
    pub fn new(
        base: String,
        key: String,
        model: String,
        system: Option<String>,
        reasoning_budget: i64,
        tools: Vec<serde_json::Value>,
    ) -> Self {
        AiClient {
            http: reqwest::Client::new(),
            base,
            key,
            model,
            system,
            reasoning_budget,
            tools,
        }
    }

    fn with_system(
        &self,
        convo: &[serde_json::Value],
        chat_context: &str,
    ) -> Vec<serde_json::Value> {
        let system = match &self.system {
            Some(s) if !chat_context.is_empty() => format!("{s}\n\n{chat_context}"),
            Some(s) => s.clone(),
            None => chat_context.to_string(),
        };
        let system = match system.is_empty() {
            true => now_line(),
            false => format!("{system}\n\n{}", now_line()),
        };
        let mut messages = vec![serde_json::json!({ "role": "system", "content": system })];
        messages.extend(convo.iter().cloned());
        messages
    }

    // stream one turn, calling `on_phase` at each phase transition. only the
    // final answer is returned; tokens are never surfaced.
    pub async fn complete(
        &self,
        convo: &[serde_json::Value],
        chat_context: &str,
        on_phase: impl FnMut(Phase),
    ) -> anyhow::Result<Completion> {
        let messages = self.with_system(convo, chat_context);
        self.stream_turn(messages, &self.tools, on_phase).await
    }

    // one line to send with images the model made but said nothing about
    pub async fn caption(
        &self,
        convo: &[serde_json::Value],
        chat_context: &str,
        images: &[String],
    ) -> anyhow::Result<String> {
        let made = serde_json::json!({
            "role": "assistant",
            "content": "",
            "images": images.iter().map(image_part).collect::<Vec<_>>(),
        });
        self.ask_without_tools(
            convo,
            chat_context,
            vec![made],
            "Write one short line to send along with the image you just made. \
             Text only — do not make another image.",
        )
        .await
    }

    // a plain answer with no tools on offer, for when the model has stopped
    // making progress with them
    pub async fn conclude(
        &self,
        convo: &[serde_json::Value],
        chat_context: &str,
    ) -> anyhow::Result<String> {
        self.ask_without_tools(
            convo,
            chat_context,
            Vec::new(),
            "Answer now, with what you already have. Do not call any tools.",
        )
        .await
    }

    async fn ask_without_tools(
        &self,
        convo: &[serde_json::Value],
        chat_context: &str,
        extra: Vec<serde_json::Value>,
        instruction: &str,
    ) -> anyhow::Result<String> {
        let mut messages = self.with_system(convo, chat_context);
        messages.extend(extra);
        // some providers reject a request ending on an assistant turn
        messages.push(serde_json::json!({ "role": "user", "content": instruction }));
        Ok(self.stream_turn(messages, &[], |_| {}).await?.text)
    }

    async fn stream_turn(
        &self,
        messages: Vec<serde_json::Value>,
        tools: &[serde_json::Value],
        mut on_phase: impl FnMut(Phase),
    ) -> anyhow::Result<Completion> {
        let url = format!("{}/chat/completions", self.base.trim_end_matches('/'));
        let thinking = self.reasoning_budget != 0;
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "chat_template_kwargs": { "enable_thinking": thinking },
        });
        // a positive budget caps thinking to that many tokens (llama.cpp), which
        // needs reasoning control on; negative means unlimited, 0 is off (above)
        if self.reasoning_budget > 0 {
            body["reasoning_control"] = serde_json::json!(true);
            body["reasoning_format"] = serde_json::json!("auto");
            body["thinking_budget_tokens"] = serde_json::json!(self.reasoning_budget);
        }
        // an empty array is rejected by some servers, so only send one with tools
        if !tools.is_empty() {
            body["tools"] = serde_json::json!(tools);
        }

        debug!(messages = %redact_media(&messages), "sending request to ai");

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            anyhow::bail!("{status}: {}", detail.trim());
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut content = String::new();
        let mut saw_reasoning = false;
        let mut phase: Option<Phase> = None;
        let mut calls: Vec<PartialCall> = Vec::new();
        let mut images: Vec<String> = Vec::new();

        'outer: while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            // server-sent events: one `data: <json>` per line
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&line);
                let Some(data) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    break 'outer;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                // a provider-run tool that fails reports it in-stream, leaving
                // the rest of the turn intact
                if let Some(message) = v["error"]["message"].as_str() {
                    warn!(message, "provider reported an error mid-stream");
                }
                let delta = &v["choices"][0]["delta"];
                if let Some(generated) = delta["images"].as_array() {
                    images.extend(
                        generated
                            .iter()
                            .filter_map(|i| i["image_url"]["url"].as_str())
                            .map(str::to_string),
                    );
                }
                if let Some(fragments) = delta["tool_calls"].as_array() {
                    for f in fragments {
                        accumulate_call(&mut calls, f);
                    }
                    if phase != Some(Phase::Tool) {
                        phase = Some(Phase::Tool);
                        on_phase(Phase::Tool);
                    }
                }
                if delta["reasoning_content"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                {
                    saw_reasoning = true;
                }
                if let Some(c) = delta["content"].as_str() {
                    content.push_str(c);
                }
                if let Some(want) = next_phase(saw_reasoning, &content) {
                    if phase != Some(want) {
                        phase = Some(want);
                        on_phase(want);
                    }
                }
            }
        }

        let text = stitched_answer(&content);
        let mut assistant = serde_json::json!({ "role": "assistant", "content": text });
        if !calls.is_empty() {
            assistant["tool_calls"] =
                serde_json::json!(calls.iter().map(PartialCall::to_value).collect::<Vec<_>>());
        }
        // echoed back in the shape they arrived in, which is the only one the
        // api accepts on an assistant turn
        if !images.is_empty() {
            assistant["images"] =
                serde_json::json!(images.iter().map(image_part).collect::<Vec<_>>());
        }
        Ok(Completion {
            assistant,
            text,
            images,
            calls: calls.into_iter().map(PartialCall::into_call).collect(),
        })
    }
}

fn image_part(url: &String) -> serde_json::Value {
    serde_json::json!({ "type": "image_url", "image_url": { "url": url } })
}

// a tool call being assembled: the arguments arrive as string fragments spread
// over several deltas, keyed by their position in the call array
struct PartialCall {
    index: u64,
    id: String,
    name: String,
    arguments: String,
}

impl PartialCall {
    fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "type": "function",
            "function": { "name": self.name, "arguments": self.arguments },
        })
    }

    fn into_call(self) -> ToolCall {
        ToolCall {
            id: self.id,
            name: self.name,
            arguments: self.arguments,
        }
    }
}

fn accumulate_call(calls: &mut Vec<PartialCall>, fragment: &serde_json::Value) {
    let index = fragment["index"].as_u64().unwrap_or(0);
    let slot = match calls.iter().position(|c| c.index == index) {
        Some(i) => &mut calls[i],
        None => {
            calls.push(PartialCall {
                index,
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
            calls.last_mut().expect("just pushed")
        }
    };
    if let Some(id) = fragment["id"].as_str() {
        slot.id.push_str(id);
    }
    if let Some(name) = fragment["function"]["name"].as_str() {
        slot.name.push_str(name);
    }
    if let Some(args) = fragment["function"]["arguments"].as_str() {
        slot.arguments.push_str(args);
    }
}

fn now_line() -> String {
    format!(
        "The current date and time is {}.",
        chrono::Local::now().format("%A %-d %B %Y, %H:%M %Z")
    )
}

// render the messages array as compact json for logging, replacing each media
// part with a short placeholder so base64 payloads don't flood logs
fn redact_media(messages: &[serde_json::Value]) -> String {
    let mut msgs = messages.to_vec();
    for m in &mut msgs {
        if let Some(images) = m.get_mut("images") {
            *images = serde_json::json!("<images omitted>");
        }
        let Some(parts) = m.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for part in parts {
            let Some(kind) = part.get("type").and_then(|t| t.as_str()) else {
                continue;
            };
            if matches!(kind, "image_url" | "input_audio" | "file") {
                *part = serde_json::json!({ "type": kind, kind: "<omitted>" });
            }
        }
    }
    serde_json::to_string(&msgs).unwrap_or_else(|_| "<unserializable>".to_string())
}

// the answer with reasoning removed. the prompt opens the first <think> for the
// model, so content can start inside reasoning with only a closing </think>;
// drop that leading block, then strip any further explicit <think>…</think>
// blocks (the model may re-enter reasoning between answer segments).
fn stitched_answer(content: &str) -> String {
    let mut rest = content;
    if let Some(close) = rest.find("</think>") {
        if rest.find("<think>").is_none_or(|open| close < open) {
            rest = &rest[close + "</think>".len()..];
        }
    }
    let mut out = String::new();
    loop {
        let Some(open) = rest.find("<think>") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open]);
        let after = &rest[open + "<think>".len()..];
        match after.find("</think>") {
            Some(close) => rest = &after[close + "</think>".len()..],
            None => break,
        }
    }
    out.trim().to_string()
}

// the phase implied by what's arrived so far: visible text means writing,
// reasoning before that means thinking, nothing yet means neither.
fn next_phase(saw_reasoning: bool, content: &str) -> Option<Phase> {
    match visible_answer(content) {
        Some(s) if !s.trim().is_empty() => Some(Phase::Generating),
        _ if saw_reasoning || content.contains("<think>") => Some(Phase::Reasoning),
        _ => None,
    }
}

// the user-visible answer within content, hiding an in-progress <think> block
fn visible_answer(content: &str) -> Option<&str> {
    if let Some(idx) = content.rfind("</think>") {
        Some(content[idx + "</think>".len()..].trim_start())
    } else if content.contains("<think>") {
        None
    } else {
        Some(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_a_call_from_fragments() {
        let mut calls = Vec::new();
        accumulate_call(
            &mut calls,
            &serde_json::json!({
                "index": 0, "id": "call_1", "function": { "name": "load_attachment", "arguments": "" }
            }),
        );
        accumulate_call(
            &mut calls,
            &serde_json::json!({ "index": 0, "function": { "arguments": "{\"id\":" } }),
        );
        accumulate_call(
            &mut calls,
            &serde_json::json!({ "index": 0, "function": { "arguments": "3}" } }),
        );

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "load_attachment");
        assert_eq!(calls[0].arguments, "{\"id\":3}");
    }

    #[test]
    fn keeps_parallel_calls_apart() {
        let mut calls = Vec::new();
        for f in [
            serde_json::json!({ "index": 1, "id": "b", "function": { "name": "two", "arguments": "{}" } }),
            serde_json::json!({ "index": 0, "id": "a", "function": { "name": "one", "arguments": "{}" } }),
            serde_json::json!({ "index": 1, "function": { "arguments": "" } }),
        ] {
            accumulate_call(&mut calls, &f);
        }
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "two");
        assert_eq!(calls[1].name, "one");
    }
}
