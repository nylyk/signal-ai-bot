use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

// entries older than this are evicted. measured from insertion, but a
// continuation inserts a fresh child entry (and drops its parent), so an active
// conversation keeps resetting its age — in effect, 12h since its last answer.
const TTL: Duration = Duration::from_secs(12 * 60 * 60);

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

struct Entry {
    created: Instant,
    cached: Cached,
}

// process-lifetime cache of conversations keyed by the bot answer's root
// (placeholder) timestamp — the same value a later reply resolves to via
// `ai_root_ts`. bounded by `cap` (oldest-inserted evicted first) and by `TTL`.
pub struct ConvoCache {
    cap: usize,
    entries: HashMap<u64, Entry>,
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

    pub fn get(&mut self, root_ts: u64) -> Option<&Cached> {
        // an aged-out entry is a miss (and gets dropped on the way)
        if self
            .entries
            .get(&root_ts)
            .is_some_and(|e| e.created.elapsed() >= TTL)
        {
            self.remove(root_ts);
        }
        self.entries.get(&root_ts).map(|e| &e.cached)
    }

    pub fn insert(&mut self, root_ts: u64, cached: Cached) {
        self.purge_expired();
        let entry = Entry {
            created: Instant::now(),
            cached,
        };
        if self.entries.insert(root_ts, entry).is_none() {
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

    fn purge_expired(&mut self) {
        let expired: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| e.created.elapsed() >= TTL)
            .map(|(&k, _)| k)
            .collect();
        for k in expired {
            self.remove(k);
        }
    }
}
