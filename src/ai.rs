// ---------------------------------------------------------------------------
// openai-compatible client
// ---------------------------------------------------------------------------

use futures::StreamExt;

// which stage the model is in, inferred from the streamed deltas
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

    // stream a completion, calling `on_phase` when the model moves from reasoning
    // to writing (so the caller can update its status message), and returning the
    // visible answer once the stream ends. tokens themselves are never surfaced.
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
            // server-sent events: one `data: <json>` per line, blank-line separated
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

        Ok(visible_answer(&content)
            .map(str::trim)
            .unwrap_or("")
            .to_string())
    }
}

// the phase implied by what's arrived so far: any visible (non-reasoning) text
// means writing; reasoning before that means thinking; nothing visible yet means
// neither (still prefilling).
fn next_phase(saw_reasoning: bool, content: &str) -> Option<Phase> {
    match visible_answer(content) {
        Some(s) if !s.trim().is_empty() => Some(Phase::Generating),
        _ if saw_reasoning || content.contains("<think>") => Some(Phase::Reasoning),
        _ => None,
    }
}

// the user-visible answer within content, hiding an in-progress <think> block
// (for models that inline reasoning in the content field).
fn visible_answer(content: &str) -> Option<&str> {
    if let Some(idx) = content.rfind("</think>") {
        Some(content[idx + "</think>".len()..].trim_start())
    } else if content.contains("<think>") {
        None
    } else {
        Some(content)
    }
}
