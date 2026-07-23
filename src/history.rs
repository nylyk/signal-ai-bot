use std::collections::HashMap;

use presage::libsignal_service::proto::AttachmentPointer;
use presage::manager::Registered;
use presage::store::{Store, Thread};
use presage::Manager;

use crate::config::Config;
use crate::message::{message_version, ReplyRef};
use crate::names::Names;

pub struct HistMsg {
    pub is_ai: bool,
    pub speaker: String,
    pub reply_to: Option<ReplyRef>,
    pub text: String,
    // the message's own image attachments (empty for the bot's own answers)
    pub images: Vec<AttachmentPointer>,
    // the collapsed root timestamp, used to dedupe a directly-quoted image that
    // is already present in this window
    pub ts: u64,
}

// messages in the thread within `(since_ts, before_ts)`, oldest-first, excluding
// the triggering @ai message. `limit` optionally keeps only the newest that many
// (used for the fresh-build window; `None` gathers every intervening message).
// the bot's own replies live in the store as a placeholder edited into the
// answer; the chain collapses to one entry, flagged as ai via its placeholder
// root. each non-ai entry carries its own image attachments.
pub async fn thread_history<S: Store>(
    manager: &Manager<S, Registered>,
    thread: &Thread,
    since_ts: u64,
    before_ts: u64,
    limit: Option<usize>,
    cfg: &Config,
    names: &Names,
) -> Vec<HistMsg> {
    if limit == Some(0) {
        return Vec::new();
    }
    let thinking = cfg.processing_msg.as_str();
    // our own transient status lines, dropped below so they never surface as
    // the bot's words
    let status_msgs = [
        cfg.processing_msg.as_str(),
        cfg.reasoning_msg.as_str(),
        cfg.generating_msg.as_str(),
    ];
    let Ok(iter) = manager.store().messages(thread, 0..before_ts).await else {
        return Vec::new();
    };

    let mut versions = Vec::new();
    for msg in iter.filter_map(Result::ok) {
        if let Some(v) = message_version(manager, &msg, names, thinking).await {
            // the store doesn't reliably honour the range's bounds, so drop the
            // trigger (and anything newer) and anything at/before the lower
            // bound ourselves
            if v.own_ts >= before_ts || v.own_ts <= since_ts {
                continue;
            }
            versions.push(v);
        }
    }
    // apply oldest revision first so the newest edit wins
    versions.sort_by_key(|v| v.own_ts);

    let mut entries: HashMap<u64, HistMsg> = HashMap::new();
    let mut root_of: HashMap<u64, u64> = HashMap::new();

    for v in versions {
        let root = match v.target {
            None => v.own_ts,
            Some(t) => root_of.get(&t).copied().unwrap_or(t),
        };
        root_of.insert(v.own_ts, root);
        match entries.get_mut(&root) {
            // edit of a message we already have: keep the sender/reply/images
            // (from the original), swap text
            Some(e) => e.text = v.body,
            None => {
                entries.insert(
                    root,
                    HistMsg {
                        is_ai: v.is_ai,
                        speaker: v.speaker,
                        reply_to: v.reply_to,
                        text: v.body,
                        images: v.images,
                        ts: root,
                    },
                );
            }
        }
    }

    let mut out: Vec<HistMsg> = entries.into_values().collect();
    // drop our own transient status lines: a reply whose final answer edit isn't
    // in this window yet (e.g. a concurrent trigger) would otherwise collapse to
    // a bare "thinking..."/"writing..." and surface as the bot's words
    out.retain(|m| !(m.is_ai && status_msgs.contains(&m.text.as_str())));
    out.sort_by_key(|m| m.ts);
    // a reply's chain collapses to one entry above; if it ever fails to,
    // adjacent entries carry the same answer text, so fold them back together
    out.dedup_by(|b, a| a.is_ai && b.is_ai && a.text == b.text);
    let start = match limit {
        Some(n) => out.len().saturating_sub(n),
        None => 0,
    };
    out.split_off(start)
}
