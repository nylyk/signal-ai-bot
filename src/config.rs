use anyhow::Context as _;

use crate::ai::AiClient;

pub struct Config {
    pub trigger: String,
    pub thinking_msg: String,
    pub context_messages: usize,
    pub ai: AiClient,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let key = std::env::var("AI_API_KEY")
            .context("AI_API_KEY is required (your openai-compatible api key)")?;
        let base = std::env::var("AI_API_BASE")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let model = std::env::var("AI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
        let system = std::env::var("AI_SYSTEM_PROMPT")
            .ok()
            .filter(|s| !s.is_empty());

        let trigger = std::env::var("TRIGGER").unwrap_or_else(|_| "@ai".to_string());
        let thinking_msg =
            std::env::var("THINKING_MSG").unwrap_or_else(|_| "ai is thinking…".to_string());
        let context_messages = std::env::var("CONTEXT_MESSAGES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        // REASONING_BUDGET: 0 turns reasoning off, >0 caps it, -1 unlimited.
        // unset leaves the server default untouched.
        let reasoning_budget = std::env::var("REASONING_BUDGET")
            .ok()
            .and_then(|s| s.parse::<i64>().ok());

        Ok(Config {
            trigger,
            thinking_msg,
            context_messages,
            ai: AiClient::new(base, key, model, system, reasoning_budget),
        })
    }
}
