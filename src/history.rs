use std::collections::HashMap;

use presage::manager::Registered;
use presage::store::{Store, Thread};
use presage::Manager;

use crate::message::{message_version, ReplyRef};
use crate::names::Names;

struct Entry {
    is_ai: bool,
    speaker: String,
    reply_to: Option<ReplyRef>,
    body: String,
}

pub struct HistMsg {
    pub is_ai: bool,
    pub speaker: String,
    pub reply_to: Option<ReplyRef>,
    pub text: String,
}

// last `n` text messages in the thread before `before_ts` (excludes the
// triggering @ai message), oldest-first. the bot's own replies live in the store
// as a placeholder edited into the answer; the chain collapses to one entry,
// flagged as ai via its placeholder root.
pub async fn thread_history<S: Store>(
    manager: &Manager<S, Registered>,
    thread: &Thread,
    before_ts: u64,
    n: usize,
    thinking: &str,
    status_msgs: &[&str],
    names: &Names,
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
            (
                root,
                HistMsg {
                    is_ai: e.is_ai,
                    speaker: e.speaker,
                    reply_to: e.reply_to,
                    text: e.body,
                },
            )
        })
        .collect();
    // drop our own transient status lines: a reply whose final answer edit isn't
    // in this window yet (e.g. a concurrent trigger) would otherwise collapse to
    // a bare "thinking..."/"writing..." and surface as the bot's words
    out.retain(|(_, m)| !(m.is_ai && status_msgs.contains(&m.text.as_str())));
    out.sort_by_key(|(root, _)| *root);
    // a reply's chain collapses to one entry above; if it ever fails to,
    // adjacent entries carry the same answer text, so fold them back together
    out.dedup_by(|(_, b), (_, a)| a.is_ai && b.is_ai && a.text == b.text);
    let start = out.len().saturating_sub(n);
    out.split_off(start).into_iter().map(|(_, m)| m).collect()
}
