use std::collections::{HashMap, VecDeque};

// a finished conversation, ready to be continued. `convo` holds the exact turns
// that were sent (through and including the bot's answer), so a reply just
// appends to it and llama-server keeps its prompt-cache prefix. `chat_context`
// is captured so the reused `system` message stays byte-identical, and `tip_ts`
// is the answer's final-edit timestamp: the lower bound for gathering messages
// that arrived after this answer.
pub struct Cached {
    pub convo: Vec<serde_json::Value>,
    pub chat_context: String,
    pub tip_ts: u64,
}

// process-lifetime cache of conversations keyed by the bot answer's root
// (placeholder) timestamp — the same value a later reply resolves to via
// `ai_root_ts`. bounded; the oldest entry is evicted once the cap is exceeded.
pub struct ConvoCache {
    cap: usize,
    entries: HashMap<u64, Cached>,
    order: VecDeque<u64>,
}

impl ConvoCache {
    pub fn new(cap: usize) -> Self {
        ConvoCache {
            cap,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn get(&self, root_ts: u64) -> Option<&Cached> {
        self.entries.get(&root_ts)
    }

    pub fn insert(&mut self, root_ts: u64, cached: Cached) {
        if self.entries.insert(root_ts, cached).is_none() {
            self.order.push_back(root_ts);
        }
        while self.order.len() > self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    pub fn remove(&mut self, root_ts: u64) {
        if self.entries.remove(&root_ts).is_some() {
            self.order.retain(|&t| t != root_ts);
        }
    }
}
