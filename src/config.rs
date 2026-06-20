use anyhow::Context as _;

use crate::ai::AiClient;

pub struct Config {
    pub trigger: String,
    pub processing_msg: String,
    pub reasoning_msg: String,
    pub generating_msg: String,
    pub context_messages: usize,
    pub vision: bool,
    pub ai: AiClient,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let base = std::env::var("API_BASE").context("API_BASE is required")?;
        let key = std::env::var("API_KEY").unwrap_or_default();
        let model = std::env::var("MODEL").context("MODEL is required")?;
        let system = std::env::var("SYSTEM_PROMPT")
            .ok()
            .filter(|s| !s.is_empty());

        let trigger = std::env::var("TRIGGER").unwrap_or_else(|_| "@ai".to_string());
        let processing_msg = std::env::var("PROCESSING_MESSAGE")
            .unwrap_or_else(|_| "ai is processing...".to_string());
        let reasoning_msg =
            std::env::var("REASONING_MESSAGE").unwrap_or_else(|_| "ai is thinking...".to_string());
        let generating_msg =
            std::env::var("GENERATING_MESSAGE").unwrap_or_else(|_| "ai is writing...".to_string());
        let context_messages = std::env::var("CONTEXT_MESSAGES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);
        let reasoning_budget = std::env::var("REASONING_BUDGET")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let vision = std::env::var("VISION")
            .ok()
            .map(|s| {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);

        Ok(Config {
            trigger,
            processing_msg,
            reasoning_msg,
            generating_msg,
            context_messages,
            vision,
            ai: AiClient::new(base, key, model, system, reasoning_budget),
        })
    }
}
