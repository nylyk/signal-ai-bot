use futures::StreamExt;
use tracing::debug;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Reasoning,
    Generating,
}

pub struct AiClient {
    http: reqwest::Client,
    base: String,
    key: String,
    model: String,
    system: Option<String>,
    reasoning_budget: i64,
}

impl AiClient {
    pub fn new(
        base: String,
        key: String,
        model: String,
        system: Option<String>,
        reasoning_budget: i64,
    ) -> Self {
        AiClient {
            http: reqwest::Client::new(),
            base,
            key,
            model,
            system,
            reasoning_budget,
        }
    }

    // stream a completion, calling `on_phase` at each reasoning→writing
    // transition. only the final answer is returned; tokens are never surfaced.
    pub async fn complete(
        &self,
        convo: Vec<serde_json::Value>,
        mut on_phase: impl FnMut(Phase),
    ) -> anyhow::Result<String> {
        let mut messages = Vec::new();
        if let Some(sys) = &self.system {
            messages.push(serde_json::json!({ "role": "system", "content": sys }));
        }
        messages.extend(convo);

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

        debug!(messages = %redact_images(&messages), "sending request to ai");

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut content = String::new();
        let mut saw_reasoning = false;
        let mut phase: Option<Phase> = None;

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
                let delta = &v["choices"][0]["delta"];
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

        Ok(stitched_answer(&content))
    }
}

// render the messages array as compact json for logging, replacing each
// image_url part with a short placeholder so base64 payloads don't flood logs
fn redact_images(messages: &[serde_json::Value]) -> String {
    let mut msgs = messages.to_vec();
    for m in &mut msgs {
        let Some(parts) = m.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) == Some("image_url") {
                *part = serde_json::json!({ "type": "image_url", "image_url": "<image omitted>" });
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
