use std::collections::BTreeMap;
use std::path::PathBuf;

const MAX_ENTRIES: usize = 500;

// the bot's own replies aren't reliably persisted by presage (sent edits don't
// update the local store), so we keep our own record: sent-timestamp -> final
// answer text. used to show past replies in context and to label them as AI.
pub struct AiReplies {
    path: PathBuf,
    map: BTreeMap<u64, String>,
}

impl AiReplies {
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        AiReplies { path, map }
    }

    pub fn get(&self, ts: u64) -> Option<&str> {
        self.map.get(&ts).map(String::as_str)
    }

    pub fn record(&mut self, ts: u64, text: String) {
        self.map.insert(ts, text);
        while self.map.len() > MAX_ENTRIES {
            self.map.pop_first();
        }
        if let Ok(s) = serde_json::to_string(&self.map) {
            let _ = std::fs::write(&self.path, s);
        }
    }
}
