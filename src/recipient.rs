use presage::libsignal_service::content::{ContentBody, DataMessage, GroupContextV2};
use presage::libsignal_service::proto::data_message::Quote;
use presage::libsignal_service::proto::AttachmentPointer;
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::sender::AttachmentSpec;
use presage::libsignal_service::zkgroup::GroupMasterKeyBytes;
use presage::manager::Registered;
use presage::proto::EditMessage;
use presage::store::{Store, Thread};

use crate::message::BOT_FILE_NAME;
use presage::Manager;

pub enum Recipient {
    Contact(ServiceId),
    Group(GroupMasterKeyBytes),
}

impl Recipient {
    pub fn from_thread(thread: &Thread) -> Self {
        match thread {
            Thread::Contact(sid) => Recipient::Contact(*sid),
            Thread::Group(key) => Recipient::Group(*key),
        }
    }

    // group messages must carry the group context, 1:1 must not
    pub fn group_context(&self) -> Option<GroupContextV2> {
        match self {
            Recipient::Group(mk) => Some(GroupContextV2 {
                master_key: Some(mk.to_vec()),
                revision: Some(0),
                ..Default::default()
            }),
            Recipient::Contact(_) => None,
        }
    }
}

pub async fn send_to<S: Store>(
    manager: &mut Manager<S, Registered>,
    recipient: &Recipient,
    body: ContentBody,
    timestamp: u64,
) -> anyhow::Result<()> {
    match recipient {
        Recipient::Contact(sid) => {
            manager.send_message(*sid, body, timestamp).await?;
        }
        Recipient::Group(mk) => {
            manager.send_message_to_group(mk, body, timestamp).await?;
        }
    }
    Ok(())
}

// upload one attachment so a message can link it
pub async fn upload<S: Store>(
    manager: &mut Manager<S, Registered>,
    mime: String,
    bytes: Vec<u8>,
) -> anyhow::Result<AttachmentPointer> {
    let spec = AttachmentSpec {
        content_type: mime,
        length: bytes.len(),
        file_name: Some(BOT_FILE_NAME.to_string()),
        preview: None,
        voice_note: None,
        borderless: None,
        width: None,
        height: None,
        caption: None,
        blur_hash: None,
    };
    manager
        .upload_attachment(spec, bytes)
        .await?
        .map_err(|e| anyhow::anyhow!("attachment upload failed: {e}"))
}

// post the bot's attachments as their own message, marked as ours
pub async fn send_attachments<S: Store>(
    manager: &mut Manager<S, Registered>,
    recipient: &Recipient,
    attachments: Vec<AttachmentPointer>,
    timestamp: u64,
) -> anyhow::Result<()> {
    let message = DataMessage {
        attachments,
        timestamp: Some(timestamp),
        group_v2: recipient.group_context(),
        ..Default::default()
    };
    send_to(manager, recipient, message.into(), timestamp).await
}

pub async fn send_edit<S: Store>(
    manager: &mut Manager<S, Registered>,
    recipient: &Recipient,
    target_ts: u64,
    text: String,
    edit_ts: u64,
    quote: Option<Quote>,
) -> anyhow::Result<()> {
    let edited = DataMessage {
        body: Some(text),
        timestamp: Some(edit_ts),
        group_v2: recipient.group_context(),
        quote,
        ..Default::default()
    };
    let edit = EditMessage {
        target_sent_timestamp: Some(target_ts),
        data_message: Some(edited),
    };
    send_to(manager, recipient, ContentBody::EditMessage(edit), edit_ts).await
}
