use std::collections::{HashMap, HashSet};

use presage::libsignal_service::proto::AttachmentPointer;

use crate::config::Config;
use crate::handler::truncated;
use crate::history::HistMsg;
use crate::media::Media;
use crate::message::ReplyRef;
use crate::tools::{Catalog, Inline};

// how much of a quoted message's text is kept when it can't be cited by number
const QUOTE_MAX: usize = 120;

// the conversation as the model sees it: every message numbered `[msg N]` so a
// reply cites the number instead of repeating the quoted text, every attachment
// numbered `#N` by the catalog, and a record of which messages already have
// their media inline so it is never sent twice.
#[derive(Default, Clone)]
pub struct Transcript {
    catalog: Catalog,
    numbers: HashMap<u64, usize>,
    inlined: HashSet<u64>,
}

impl Transcript {
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn announce(&mut self, cfg: &Config, atts: &[AttachmentPointer], inline: Inline) -> String {
        self.catalog.announce(cfg, atts, inline)
    }

    // the message's number, assigned on first sight
    pub fn number(&mut self, ts: u64) -> usize {
        let next = self.numbers.len() + 1;
        *self.numbers.entry(ts).or_insert(next)
    }

    fn number_of(&self, ts: u64) -> Option<usize> {
        self.numbers.get(&ts).copied()
    }

    // does this message's media already sit in the messages array?
    pub fn is_inlined(&self, ts: u64) -> bool {
        self.inlined.contains(&ts)
    }

    pub fn mark_inlined(&mut self, ts: u64) {
        self.inlined.insert(ts);
    }

    pub fn user_text(
        &mut self,
        ts: u64,
        speaker: &str,
        reply_to: Option<&ReplyRef>,
        body: &str,
    ) -> String {
        let num = self.number(ts);
        let mut s = format!("[msg {num}] ");
        if let Some(r) = reply_to {
            s.push_str("[in reply to ");
            s.push_str(&self.reply_target(r));
            s.push_str("] ");
        }
        s.push_str(speaker);
        s.push_str(": ");
        s.push_str(body);
        s
    }

    // cite the quoted message by number when it is already in the array; only a
    // message from outside it needs its text repeating
    fn reply_target(&self, r: &ReplyRef) -> String {
        if r.is_ai {
            return "you".to_string();
        }
        if let Some(n) = r.ts.and_then(|ts| self.number_of(ts)) {
            return format!("msg {n}");
        }
        if r.text.is_empty() {
            return r.author.clone();
        }
        format!(
            "{}: \"{}\"",
            r.author,
            truncated(&r.text.replace('\n', " "), QUOTE_MAX)
        )
    }

    // turn a window of history into chat messages. the bot's own answers become
    // `assistant` turns; everyone else's become `user` turns. attachments are
    // only listed here, for the model to load if it wants them.
    pub fn history_turns(&mut self, cfg: &Config, history: &[HistMsg]) -> Vec<serde_json::Value> {
        let mut turns = Vec::with_capacity(history.len());
        for h in history {
            let note = self.announce(cfg, &h.atts, Inline::Listed);
            let mut text = match h.is_ai {
                true => h.text.clone(),
                false => self.user_text(h.ts, &h.speaker, h.reply_to.as_ref(), &h.text),
            };
            if !note.is_empty() {
                text.push('\n');
                text.push_str(&note);
            }
            let role = match h.is_ai {
                true => "assistant",
                false => "user",
            };
            turns.push(serde_json::json!({ "role": role, "content": text }));
        }
        turns
    }
}

// a `user` turn: plain string content when there's no media, else a multimodal
// parts array with the text, then each image as an image_url, then each audio
// clip. audio goes last because gemma 4 asks for it after the text, and it
// carries raw base64 rather than a data uri — llama.cpp only unwraps `data:`
// uris for images.
pub fn user_turn_value(text: String, media: &Media) -> serde_json::Value {
    if media.is_empty() {
        return serde_json::json!({ "role": "user", "content": text });
    }
    let mut parts = vec![serde_json::json!({ "type": "text", "text": text })];
    for (mime, data) in &media.images {
        parts.push(serde_json::json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{mime};base64,{data}") }
        }));
    }
    for data in &media.audio {
        parts.push(serde_json::json!({
            "type": "input_audio",
            "input_audio": { "data": data, "format": "wav" }
        }));
    }
    for (name, data) in &media.files {
        parts.push(serde_json::json!({
            "type": "file",
            "file": { "filename": name, "file_data": data }
        }));
    }
    serde_json::json!({ "role": "user", "content": parts })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::AiClient;

    fn config() -> Config {
        Config {
            trigger: "@ai".to_string(),
            processing_msg: String::new(),
            reasoning_msg: String::new(),
            generating_msg: String::new(),
            tool_msg: String::new(),
            context_messages: 5,
            vision: true,
            audio: false,
            documents: false,
            memory: false,
            audio_speed: 1.0,
            retention: None,
            ai: AiClient::new(
                String::new(),
                String::new(),
                String::new(),
                None,
                0,
                Vec::new(),
            ),
        }
    }

    fn msg(ts: u64, speaker: &str, text: &str, reply_to: Option<ReplyRef>) -> HistMsg {
        HistMsg {
            is_ai: false,
            speaker: speaker.to_string(),
            reply_to,
            text: text.to_string(),
            atts: Vec::new(),
            ts,
        }
    }

    fn ai(ts: u64, text: &str) -> HistMsg {
        HistMsg {
            is_ai: true,
            speaker: String::new(),
            reply_to: None,
            text: text.to_string(),
            atts: Vec::new(),
            ts,
        }
    }

    fn reply(ts: Option<u64>, author: &str, text: &str, is_ai: bool) -> ReplyRef {
        ReplyRef {
            author: author.to_string(),
            text: text.to_string(),
            ts,
            is_ai,
        }
    }

    fn contents(turns: &[serde_json::Value]) -> Vec<String> {
        turns
            .iter()
            .map(|t| t["content"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn numbers_messages_and_cites_a_reply_by_number() {
        let cfg = config();
        let mut t = Transcript::default();
        let turns = t.history_turns(
            &cfg,
            &[
                msg(10, "alice", "look at this", None),
                msg(20, "carol", "nice", None),
                msg(
                    30,
                    "bob",
                    "what is that?",
                    Some(reply(Some(10), "alice", "look at this", false)),
                ),
            ],
        );
        assert_eq!(
            contents(&turns),
            vec![
                "[msg 1] alice: look at this",
                "[msg 2] carol: nice",
                "[msg 3] [in reply to msg 1] bob: what is that?",
            ]
        );
    }

    #[test]
    fn keeps_numbering_across_calls() {
        let cfg = config();
        let mut t = Transcript::default();
        t.history_turns(&cfg, &[msg(10, "alice", "hi", None)]);
        let turns = t.history_turns(&cfg, &[msg(20, "bob", "hey", None)]);
        assert_eq!(contents(&turns), vec!["[msg 2] bob: hey"]);
    }

    #[test]
    fn quotes_the_text_only_for_a_message_outside_the_window() {
        let cfg = config();
        let mut t = Transcript::default();
        let turns = t.history_turns(
            &cfg,
            &[msg(
                30,
                "bob",
                "what is that?",
                Some(reply(Some(10), "alice", "look\nat this", false)),
            )],
        );
        assert_eq!(
            contents(&turns),
            vec!["[msg 1] [in reply to alice: \"look at this\"] bob: what is that?"]
        );
    }

    #[test]
    fn truncates_a_long_quote() {
        let cfg = config();
        let mut t = Transcript::default();
        let long = "x".repeat(QUOTE_MAX + 50);
        let turns = t.history_turns(
            &cfg,
            &[msg(
                30,
                "bob",
                "?",
                Some(reply(Some(10), "alice", &long, false)),
            )],
        );
        assert!(contents(&turns)[0].contains(&format!("\"{}…\"", "x".repeat(QUOTE_MAX))));
    }

    #[test]
    fn addresses_the_bot_as_you_and_leaves_its_turns_unnumbered() {
        let cfg = config();
        let mut t = Transcript::default();
        let turns = t.history_turns(
            &cfg,
            &[
                ai(10, "it's a cat"),
                msg(
                    20,
                    "bob",
                    "are you sure?",
                    Some(reply(Some(10), "bot", "it's a cat", true)),
                ),
            ],
        );
        assert_eq!(turns[0]["role"], "assistant");
        assert_eq!(
            contents(&turns),
            vec!["it's a cat", "[msg 1] [in reply to you] bob: are you sure?"]
        );
    }

    #[test]
    fn remembers_which_messages_have_their_media_inline() {
        let mut t = Transcript::default();
        assert!(!t.is_inlined(10));
        t.mark_inlined(10);
        assert!(t.is_inlined(10));
        assert!(!t.is_inlined(20));
    }
}
