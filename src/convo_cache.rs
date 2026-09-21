use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::transcript::Transcript;

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
    // the message and attachment numbering already shown to the model, and which
    // attachments are inline, so a continuation keeps addressing everything by
    // the same id and never resends media it already carries
    pub transcript: Transcript,
    pub tip_ts: u64,
}

struct Entry {
    created: Instant,
    cached: Cached,
}

// process-lifetime cache of conversations keyed by the bot answer's root
// (placeholder) timestamp — the same value a later reply resolves to via
// `ai_root_ts`. messages of the same answer that sit outside its edit chain
// (generated images travel as their own message) reach the entry through
// `aliases`. bounded by `cap` (oldest-inserted evicted first) and by `TTL`.
pub struct ConvoCache {
    cap: usize,
    entries: HashMap<u64, Entry>,
    order: VecDeque<u64>,
    aliases: HashMap<u64, u64>,
}

impl ConvoCache {
    pub fn new(cap: usize) -> Self {
        ConvoCache {
            cap,
            entries: HashMap::new(),
            order: VecDeque::new(),
            aliases: HashMap::new(),
        }
    }

    pub fn get(&mut self, ts: u64) -> Option<&Cached> {
        let root_ts = self.aliases.get(&ts).copied().unwrap_or(ts);
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

    // let a reply to `ts` resolve to the answer rooted at `root_ts`
    pub fn alias(&mut self, ts: u64, root_ts: u64) {
        if self.entries.contains_key(&root_ts) {
            self.aliases.insert(ts, root_ts);
        }
    }

    pub fn insert(&mut self, root_ts: u64, cached: Cached) {
        self.purge_expired();
        let entry = Entry {
            created: Instant::now(),
            cached,
        };
        // a timestamp is either a root or an alias, never both
        self.aliases.remove(&root_ts);
        if self.entries.insert(root_ts, entry).is_none() {
            self.order.push_back(root_ts);
        }
        while self.order.len() > self.cap {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
            self.aliases.retain(|_, &mut r| r != oldest);
        }
    }

    pub fn remove(&mut self, ts: u64) {
        let root_ts = self.aliases.get(&ts).copied().unwrap_or(ts);
        if self.entries.remove(&root_ts).is_some() {
            self.order.retain(|&t| t != root_ts);
            self.aliases.retain(|_, &mut r| r != root_ts);
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
