use anyhow::Context as _;

use crate::ai::AiClient;
use crate::tools::{self, Kind};

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|s| {
        matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub struct Config {
    pub trigger: String,
    pub processing_msg: String,
    pub reasoning_msg: String,
    pub generating_msg: String,
    pub tool_msg: String,
    pub context_messages: usize,
    pub vision: bool,
    pub audio: bool,
    pub documents: bool,
    // playback rate applied to voice notes before chunking; above 1.0 fits more
    // of a long note into the model's 30s-per-clip limit
    pub audio_speed: f32,
    // how old a message may get before it's pruned from the store, in ms.
    // `None` keeps history forever.
    pub retention: Option<u64>,
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
        let tool_msg = std::env::var("TOOL_MESSAGE")
            .unwrap_or_else(|_| "ai is looking things up...".to_string());
        let context_messages = std::env::var("CONTEXT_MESSAGES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);
        let retention_days = std::env::var("MESSAGE_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(7);
        // 0 keeps history forever; otherwise convert days to milliseconds
        let retention = (retention_days != 0).then(|| retention_days * 24 * 60 * 60 * 1000);
        let reasoning_budget = std::env::var("REASONING_BUDGET")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let vision = env_flag("VISION");
        let audio = env_flag("AUDIO");
        let documents = env_flag("DOCUMENTS");
        // tools the provider runs itself, passed through verbatim so any
        // provider's server-tool syntax works without a code change
        let extra_tools: Vec<serde_json::Value> = match std::env::var("EXTRA_TOOLS") {
            Ok(s) if !s.trim().is_empty() => {
                serde_json::from_str(&s).context("EXTRA_TOOLS must be a json array of tools")?
            }
            _ => Vec::new(),
        };
        let mut tool_defs = tools::definitions();
        tool_defs.extend(extra_tools);
        // clamped to atempo's per-instance range; 1.0 leaves the note untouched
        let audio_speed = std::env::var("AUDIO_SPEED")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(1.0)
            .clamp(0.5, 2.0);

        Ok(Config {
            trigger,
            processing_msg,
            reasoning_msg,
            generating_msg,
            tool_msg,
            context_messages,
            vision,
            audio,
            documents,
            audio_speed,
            retention,
            ai: AiClient::new(base, key, model, system, reasoning_budget, tool_defs),
        })
    }

    // is this kind attached to its turn directly, rather than only listed?
    pub fn sent_inline(&self, mime: &str) -> bool {
        self.can_load(mime) && tools::kind_of(mime) != Some(Kind::Text)
    }

    // can the model be given this attachment, either inline or on request?
    pub fn can_load(&self, mime: &str) -> bool {
        match tools::kind_of(mime) {
            Some(Kind::Image) => self.vision,
            Some(Kind::Audio) => self.audio,
            Some(Kind::Text) => true,
            Some(Kind::Document) => self.documents,
            None => false,
        }
    }
}
