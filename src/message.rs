use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::proto::data_message::Quote;
use presage::libsignal_service::proto::sync_message::Sent;
use presage::libsignal_service::proto::AttachmentPointer;
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

// display name of a replied-to message's author. prefers the aci carried by the
// quote; if absent, falls back to the original message's sender, looked up in
// the store by its sent-timestamp.
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

// a reference to the message a message is replying to
pub struct ReplyRef {
    pub author: String, // resolved name of the person being replied to
    pub text: String,   // the quoted text (may be empty for media-only quotes)
}

// everything we need out of a triggering message
pub struct Trigger {
    pub thread: Thread,
    pub body: String,
    pub quoted_text: Option<String>,
    pub quoted_ts: Option<u64>, // sent-timestamp of the replied-to msg
    pub quoted_author: Option<Uuid>, // aci of the replied-to message's author
    pub quoted_thumbs: Vec<AttachmentPointer>, // low-res thumbnails from the quote
    pub own_images: Vec<AttachmentPointer>, // images attached to this message
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
fn dm_images(dm: &DataMessage) -> Vec<AttachmentPointer> {
    let mut ptrs = image_pointers(&dm.attachments);
    if let Some(data) = dm.sticker.as_ref().and_then(|s| s.data.clone()) {
        ptrs.push(data);
    }
    ptrs
}

// image attachments on a stored message (either direction)
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

// pull the text body, reply info, and image attachments out of a message, for
// both messages others send to us and ones we send ourselves (synced)
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
        quoted_thumbs,
        own_images,
    })
}

// resolve a message's reply reference (quoted author name + text), if it quotes
// another message
async fn reply_ref<S: Store>(
    manager: &Manager<S, Registered>,
    names: &Names,
    thread: Option<&Thread>,
    dm: &DataMessage,
) -> Option<ReplyRef> {
    let quote = dm.quote.as_ref()?;
    let text = quote.text.clone().unwrap_or_default();
    let author = resolve_author(manager, names, thread, quote_author_uuid(quote), quote.id).await;
    Some(ReplyRef { author, text })
}

// one revision of a message
pub struct Version {
    pub own_ts: u64,
    pub target: Option<u64>,
    pub is_ai: bool,                // started life as the bot's placeholder => an ai reply
    pub speaker: String,            // resolved display name of the sender
    pub reply_to: Option<ReplyRef>, // who/what this message replies to, if any
    pub body: String,
}

// extract a text version (original or edit) from a stored message. `names`
// resolves display names; `thinking` is the placeholder text used to recognise
// the bot's own replies (they all start life as that placeholder before editing).
pub async fn message_version<S: Store>(
    manager: &Manager<S, Registered>,
    content: &Content,
    names: &Names,
    thinking: &str,
) -> Option<Version> {
    // pull the relevant data message (and any edit target) out of the envelope
    let (target, dm): (Option<u64>, &DataMessage) = match &content.body {
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
    };
    let body = dm.body.clone()?;
    if body.is_empty() {
        return None;
    }
    // synced sends are ours; direct sends are ours iff the sender is our account
    let from_me = matches!(&content.body, ContentBody::SynchronizeMessage(_))
        || content.metadata.sender.raw_uuid() == names.my_aci();
    let is_ai = from_me && target.is_none() && body == thinking;
    let speaker = names.of(manager, &content.metadata.sender).await;
    let thread = Thread::try_from(content).ok();
    let reply_to = reply_ref(manager, names, thread.as_ref(), dm).await;
    Some(Version {
        own_ts: content.timestamp(),
        target,
        is_ai,
        speaker,
        reply_to,
        body,
    })
}
