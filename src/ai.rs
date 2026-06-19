// ---------------------------------------------------------------------------
// openai-compatible client
// ---------------------------------------------------------------------------

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

    // request a full completion; returns the visible answer (reasoning stripped)
    pub async fn complete(&self, convo: Vec<serde_json::Value>) -> anyhow::Result<String> {
        let mut messages = Vec::new();
        if let Some(sys) = &self.system {
            messages.push(serde_json::json!({ "role": "system", "content": sys }));
        }
        messages.extend(convo);

        let url = format!("{}/chat/completions", self.base.trim_end_matches('/'));
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
        });

        // per-request reasoning control (llama.cpp), harmless on apis that
        // ignore these fields. budget 0 disables thinking; the template kwarg is
        // sent too since on some models the budget alone isn't enough.
        let budget = self.reasoning_budget;
        body["reasoning_budget"] = serde_json::json!(budget);
        body["chat_template_kwargs"] = serde_json::json!({ "enable_thinking": budget != 0 });

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let v: serde_json::Value = resp.json().await?;
        let content = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
        // strip any inline <think>…</think> block
        let answer = visible_answer(content)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        Ok(answer)
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
