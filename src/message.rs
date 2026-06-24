use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::proto::body_range::AssociatedValue;
use presage::libsignal_service::proto::data_message::Quote;
use presage::libsignal_service::proto::sync_message::Sent;
use presage::libsignal_service::proto::{AttachmentPointer, BodyRange};
use presage::manager::Registered;
use presage::proto::{EditMessage, SyncMessage};
use presage::store::{ContentExt, Store, Thread};
use presage::Manager;

use crate::names::Names;

// the aci of a quote's author. modern signal clients send it as raw bytes in
// `author_aci_binary`; older ones use the `author_aci` uuid string. try both.
fn quote_author_uuid(quote: &Quote) -> Option<Uuid> {
    quote
        .author_aci
        .as_deref()
        .and_then(|a| Uuid::parse_str(a).ok())
        .or_else(|| {
            quote
                .author_aci_binary
                .as_deref()
                .and_then(|b| Uuid::from_slice(b).ok())
        })
}

// signal puts an OBJECT REPLACEMENT CHARACTER in the body for each @-mention;
// body_ranges says which aci each one refers to
const MENTION: char = '\u{FFFC}';

// the aci a mention range points at (uuid string or 16-byte binary form)
fn mention_aci(range: &BodyRange) -> Option<Uuid> {
    match range.associated_value.as_ref()? {
        AssociatedValue::MentionAci(s) => Uuid::parse_str(s).ok(),
        AssociatedValue::MentionAciBinary(b) => Uuid::from_slice(b).ok(),
        AssociatedValue::Style(_) => None,
    }
}

// rewrite each `￼` mention placeholder as "@[name]" so the model sees who was
// mentioned. placeholders map 1:1 to mention ranges in left-to-right order.
pub async fn resolve_mentions<S: Store>(
    manager: &Manager<S, Registered>,
    names: &Names,
    body: &str,
    ranges: &[BodyRange],
) -> String {
    if !body.contains(MENTION) {
        return body.to_string();
    }
    let mut mentions: Vec<(u32, Uuid)> = ranges
        .iter()
        .filter_map(|r| Some((r.start?, mention_aci(r)?)))
        .collect();
    mentions.sort_by_key(|(start, _)| *start);
    // resolve names up front (async), then splice positionally
    let mut tags = Vec::with_capacity(mentions.len());
    for (_, aci) in &mentions {
        tags.push(format!("@[{}]", names.of_uuid(manager, *aci).await));
    }
    let mut tags = tags.into_iter();
    let mut out = String::with_capacity(body.len());
    for ch in body.chars() {
        match (ch == MENTION).then(|| tags.next()).flatten() {
            Some(tag) => out.push_str(&tag),
            None => out.push(ch),
        }
    }
    out
}

// display name of a replied-to message's author: prefer the quote's aci, else
// fall back to the original message's sender, looked up by sent-timestamp.
pub async fn resolve_author<S: Store>(
    manager: &Manager<S, Registered>,
    names: &Names,
    thread: Option<&Thread>,
    author: Option<Uuid>,
    quoted_ts: Option<u64>,
) -> String {
    if let Some(uuid) = author {
        return names.of_uuid(manager, uuid).await;
    }
    if let (Some(thread), Some(ts)) = (thread, quoted_ts) {
        if let Ok(Some(orig)) = manager.store().message(thread, ts).await {
            return names.of(manager, &orig.metadata.sender).await;
        }
    }
    "someone".to_string()
}

pub struct ReplyRef {
    pub author: String,
    pub text: String,
    // the quoted message is one of the bot's own AI replies
    pub is_ai: bool,
}

pub struct Trigger {
    pub thread: Thread,
    pub body: String,
    pub quoted_text: Option<String>,
    pub quoted_ts: Option<u64>,
    pub quoted_author: Option<Uuid>,
    pub quoted_thumbnails: Vec<AttachmentPointer>,
    pub own_images: Vec<AttachmentPointer>,
    pub body_ranges: Vec<BodyRange>,
}

impl Trigger {
    pub fn is_reply(&self) -> bool {
        self.quoted_text.is_some()
            || self.quoted_ts.is_some()
            || self.quoted_author.is_some()
            || !self.quoted_thumbnails.is_empty()
    }
}

fn image_pointers(atts: &[AttachmentPointer]) -> Vec<AttachmentPointer> {
    atts.iter()
        .filter(|a| {
            a.content_type
                .as_deref()
                .is_some_and(|t| t.starts_with("image/"))
        })
        .cloned()
        .collect()
}

// image attachments of a message: regular image attachments plus a sticker's
// image (stickers live in their own field, not in `attachments`)
pub(crate) fn dm_images(dm: &DataMessage) -> Vec<AttachmentPointer> {
    let mut ptrs = image_pointers(&dm.attachments);
    if let Some(data) = dm.sticker.as_ref().and_then(|s| s.data.clone()) {
        ptrs.push(data);
    }
    ptrs
}

pub fn content_images(content: &Content) -> Vec<AttachmentPointer> {
    match &content.body {
        ContentBody::DataMessage(dm) => dm_images(dm),
        ContentBody::SynchronizeMessage(SyncMessage {
            sent: Some(Sent {
                message: Some(dm), ..
            }),
            ..
        }) => dm_images(dm),
        _ => Vec::new(),
    }
}

// pull text, reply info, and images out of a message (sent or received)
pub fn extract(content: &Content) -> Option<Trigger> {
    let thread = Thread::try_from(content).ok()?;
    let dm = match &content.body {
        ContentBody::DataMessage(dm) => dm,
        ContentBody::SynchronizeMessage(SyncMessage {
            sent: Some(Sent {
                message: Some(dm), ..
            }),
            ..
        }) => dm,
        _ => return None,
    };
    let body = dm.body.clone()?;
    let quote = dm.quote.as_ref();
    let quoted_text = quote.and_then(|q| q.text.clone()).filter(|t| !t.is_empty());
    let quoted_ts = quote.and_then(|q| q.id);
    let quoted_author = quote.and_then(quote_author_uuid);
    // a quote embeds a still-image thumbnail for each attached media item,
    // including videos and gifs, so take them all regardless of the original
    // media type — the thumbnail itself is always an image
    let quoted_thumbs = quote
        .map(|q| {
            q.attachments
                .iter()
                .filter_map(|a| a.thumbnail.clone())
                .collect()
        })
        .unwrap_or_default();
    let own_images = dm_images(dm);
    Some(Trigger {
        thread,
        body,
        quoted_text,
        quoted_ts,
        quoted_author,
        quoted_thumbnails: quoted_thumbs,
        own_images,
        body_ranges: dm.body_ranges.clone(),
    })
}

async fn reply_ref<S: Store>(
    manager: &Manager<S, Registered>,
    names: &Names,
    thread: Option<&Thread>,
    dm: &DataMessage,
    processing_msg: &str,
) -> Option<ReplyRef> {
    let quote = dm.quote.as_ref()?;
    let text = resolve_mentions(
        manager,
        names,
        &quote.text.clone().unwrap_or_default(),
        &quote.body_ranges,
    )
    .await;
    let author = resolve_author(manager, names, thread, quote_author_uuid(quote), quote.id).await;
    let is_ai = match (thread, quote.id) {
        (Some(thread), Some(ts)) => is_ai_message(manager, names, thread, ts, processing_msg).await,
        _ => false,
    };
    Some(ReplyRef {
        author,
        text,
        is_ai,
    })
}

// pull the data message (and edit target, if any) out of a stored envelope.
// returns None for non-text bodies, or an edit missing its target/message.
pub(crate) fn data_message(content: &Content) -> Option<(Option<u64>, &DataMessage)> {
    Some(match &content.body {
        ContentBody::DataMessage(dm) => (None, dm),
        ContentBody::EditMessage(EditMessage {
            target_sent_timestamp,
            data_message,
        }) => (Some((*target_sent_timestamp)?), data_message.as_ref()?),
        ContentBody::SynchronizeMessage(SyncMessage {
            sent: Some(Sent {
                message: Some(dm), ..
            }),
            ..
        }) => (None, dm),
        ContentBody::SynchronizeMessage(SyncMessage {
            sent:
                Some(Sent {
                    edit_message:
                        Some(EditMessage {
                            target_sent_timestamp,
                            data_message,
                        }),
                    ..
                }),
            ..
        }) => (Some((*target_sent_timestamp)?), data_message.as_ref()?),
        _ => return None,
    })
}

// a stored message is ours if it's a synced send, or its sender is our account
pub(crate) fn from_me(content: &Content, my_aci: Uuid) -> bool {
    matches!(&content.body, ContentBody::SynchronizeMessage(_))
        || content.metadata.sender.raw_uuid() == my_aci
}

// is the stored message at `ts` one of the bot's own AI replies? a reply lives
// in the store as a placeholder (body == processing_msg) edited into the answer,
// each revision its own row. a quote may reference any revision, so walk the
// edit chain back to the original and check it's our placeholder — no separate
// record needed.
//
// returns the root placeholder's timestamp when it is one of our replies (so
// callers can read the quote link the placeholder carries), else None.
pub async fn ai_root_ts<S: Store>(
    manager: &Manager<S, Registered>,
    names: &Names,
    thread: &Thread,
    ts: u64,
    processing_msg: &str,
) -> Option<u64> {
    let mut ts = ts;
    // bounded: a reply is at most placeholder + a few edits deep
    for _ in 0..16 {
        let Ok(Some(content)) = manager.store().message(thread, ts).await else {
            return None;
        };
        if !from_me(&content, names.my_aci()) {
            return None;
        }
        let (target, dm) = data_message(&content)?;
        match target {
            // an edit: follow it to the revision it targets
            Some(t) => ts = t,
            // the original: it's ours iff it's the placeholder
            None => return (dm.body.as_deref() == Some(processing_msg)).then_some(ts),
        }
    }
    None
}

// retained for callers that only need the boolean answer
pub async fn is_ai_message<S: Store>(
    manager: &Manager<S, Registered>,
    names: &Names,
    thread: &Thread,
    ts: u64,
    processing_msg: &str,
) -> bool {
    ai_root_ts(manager, names, thread, ts, processing_msg)
        .await
        .is_some()
}

// one revision of a message (original or edit)
pub struct Version {
    pub own_ts: u64,
    pub target: Option<u64>,
    pub is_ai: bool,
    pub speaker: String,
    pub reply_to: Option<ReplyRef>,
    pub body: String,
}

// extract a text version (original or edit) from a stored message. `thinking` is
// the placeholder text used to recognise the bot's own replies (they all start
// life as that placeholder before editing).
pub async fn message_version<S: Store>(
    manager: &Manager<S, Registered>,
    content: &Content,
    names: &Names,
    thinking: &str,
) -> Option<Version> {
    // pull the relevant data message (and any edit target) out of the envelope
    let (target, dm) = data_message(content)?;
    let body = dm.body.clone()?;
    if body.is_empty() {
        return None;
    }
    let is_ai = from_me(content, names.my_aci()) && target.is_none() && body == thinking;
    let body = resolve_mentions(manager, names, &body, &dm.body_ranges).await;
    let speaker = names.of(manager, &content.metadata.sender).await;
    let thread = Thread::try_from(content).ok();
    let reply_to = reply_ref(manager, names, thread.as_ref(), dm, thinking).await;
    // content.timestamp() returns the *target* for edits; we need this revision's
    // own id (its envelope ts) so edit chains link by target instead of orphaning
    let own_ts = if target.is_some() {
        content.metadata.timestamp
    } else {
        content.timestamp()
    };
    Some(Version {
        own_ts,
        target,
        is_ai,
        speaker,
        reply_to,
        body,
    })
}
