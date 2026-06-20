use presage::libsignal_service::content::{ContentBody, DataMessage, GroupContextV2};
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::zkgroup::GroupMasterKeyBytes;
use presage::manager::Registered;
use presage::proto::EditMessage;
use presage::store::{Store, Thread};
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

pub async fn send_edit<S: Store>(
    manager: &mut Manager<S, Registered>,
    recipient: &Recipient,
    target_ts: u64,
    text: String,
    edit_ts: u64,
) -> anyhow::Result<()> {
    let edited = DataMessage {
        body: Some(text),
        timestamp: Some(edit_ts),
        group_v2: recipient.group_context(),
        ..Default::default()
    };
    let edit = EditMessage {
        target_sent_timestamp: Some(target_ts),
        data_message: Some(edited),
    };
    send_to(manager, recipient, ContentBody::EditMessage(edit), edit_ts).await
}
