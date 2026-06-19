use std::collections::HashMap;

use presage::manager::Registered;
use presage::store::{Store, Thread};
use presage::Manager;

use crate::message::{message_version, ReplyRef};
use crate::names::Names;
use crate::replies::AiReplies;

struct Entry {
    is_ai: bool,
    speaker: String,
    reply_to: Option<ReplyRef>,
    body: String,
}

// a resolved message for the prompt context
pub struct HistMsg {
    pub is_ai: bool,
    pub speaker: String,            // sender's display name (unused when is_ai)
    pub reply_to: Option<ReplyRef>, // who/what this message replied to, if any
    pub text: String,
}

// last `n` text messages in the thread before `before_ts` (excludes the
// triggering @ai message), oldest-first. the bot's own replies are pulled from
// `replies` (since presage doesn't persist sent edits) and flagged as ai.
pub async fn thread_history<S: Store>(
    manager: &Manager<S, Registered>,
    thread: &Thread,
    before_ts: u64,
    n: usize,
    thinking: &str,
    names: &Names,
    replies: &AiReplies,
) -> Vec<HistMsg> {
    if n == 0 {
        return Vec::new();
    }
    let Ok(iter) = manager.store().messages(thread, 0..before_ts).await else {
        return Vec::new();
    };

    let mut versions = Vec::new();
    for msg in iter.filter_map(Result::ok) {
        if let Some(v) = message_version(manager, &msg, names, thinking).await {
            // the store doesn't reliably honour the range's upper bound, so
            // drop the trigger message (and anything newer) ourselves
            if v.own_ts >= before_ts {
                continue;
            }
            versions.push(v);
        }
    }
    // apply oldest revision first so the newest edit wins
    versions.sort_by_key(|v| v.own_ts);

    let mut entries: HashMap<u64, Entry> = HashMap::new();
    let mut root_of: HashMap<u64, u64> = HashMap::new();

    for v in versions {
        let root = match v.target {
            None => v.own_ts,
            Some(t) => root_of.get(&t).copied().unwrap_or(t),
        };
        root_of.insert(v.own_ts, root);
        match entries.get_mut(&root) {
            // edit of a message we already have: keep the sender/reply, swap text
            Some(e) => e.body = v.body,
            None => {
                entries.insert(
                    root,
                    Entry {
                        is_ai: v.is_ai,
                        speaker: v.speaker,
                        reply_to: v.reply_to,
                        body: v.body,
                    },
                );
            }
        }
    }

    let mut out: Vec<(u64, HistMsg)> = entries
        .into_iter()
        .map(|(root, e)| {
            // our recorded answer is authoritative for the bot's own messages
            let (is_ai, text) = match replies.get(root) {
                Some(answer) => (true, answer.to_string()),
                None => (e.is_ai, e.body),
            };
            (
                root,
                HistMsg {
                    is_ai,
                    speaker: e.speaker,
                    reply_to: e.reply_to,
                    text,
                },
            )
        })
        .collect();
    out.sort_by_key(|(root, _)| *root);
    let start = out.len().saturating_sub(n);
    out.split_off(start).into_iter().map(|(_, m)| m).collect()
}
