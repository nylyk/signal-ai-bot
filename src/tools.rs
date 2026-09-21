use presage::libsignal_service::content::GroupContextV2;
use presage::libsignal_service::proto::AttachmentPointer;
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::manager::Registered;
use presage::store::{Store, Thread};
use presage::Manager;
use tracing::info;

use crate::ai::ToolCall;
use crate::config::Config;
use crate::images::encode_image;
use crate::media::{fetch_media, fetch_raw, Media};
use crate::names::Names;

const MAX_TEXT_BYTES: usize = 32 * 1024;

const LOAD_ATTACHMENT: &str = "load_attachment";
const LOAD_AVATAR: &str = "load_avatar";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Audio,
    Text,
    Document,
}

pub fn kind_of(mime: &str) -> Option<Kind> {
    // a content type may carry parameters, e.g. "text/plain; charset=utf-8"
    let mime = mime.split(';').next().unwrap_or_default().trim();
    match mime {
        m if m.starts_with("image/") => Some(Kind::Image),
        m if m.starts_with("audio/") => Some(Kind::Audio),
        m if m.starts_with("text/") => Some(Kind::Text),
        "application/json"
        | "application/xml"
        | "application/yaml"
        | "application/x-yaml"
        | "application/toml"
        | "application/x-ndjson"
        | "application/javascript"
        | "application/sql"
        | "application/x-sh" => Some(Kind::Text),
        m if m.ends_with("+json") || m.ends_with("+xml") => Some(Kind::Text),
        "application/pdf" => Some(Kind::Document),
        _ => None,
    }
}

pub fn of_kind(atts: &[AttachmentPointer], kind: Kind) -> Vec<AttachmentPointer> {
    atts.iter()
        .filter(|a| kind_of(a.content_type.as_deref().unwrap_or_default()) == Some(kind))
        .cloned()
        .collect()
}

// whether the attachments being announced are also attached to the same turn
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Inline {
    Shown,
    Listed,
}

// an attachment the model has been told about and may ask for by id
#[derive(Clone)]
struct Entry {
    ptr: AttachmentPointer,
    mime: String,
}

// the attachments named in the conversation so far, addressed by 1-based id
// ids are positions in `entries`, so it is only ever appended to: a continued
// conversation keeps addressing each attachment by the id it was already shown.
#[derive(Default, Clone)]
pub struct Catalog {
    entries: Vec<Entry>,
}

impl Catalog {
    // list and number every attachment, marking the unreadable ones and, when
    // `inline`, the ones this turn already carries
    pub fn announce(&mut self, cfg: &Config, atts: &[AttachmentPointer], inline: Inline) -> String {
        let mut listed = Vec::new();
        for ptr in atts {
            let mime = ptr.content_type.clone().unwrap_or_default();
            let name = ptr.file_name.clone().filter(|n| !n.is_empty());
            let id = match self.entries.iter().position(|e| same_file(&e.ptr, ptr)) {
                Some(i) => i + 1,
                None => {
                    self.entries.push(Entry {
                        ptr: ptr.clone(),
                        mime: mime.clone(),
                    });
                    self.entries.len()
                }
            };
            let mut s = format!("#{id} {mime}");
            if let Some(name) = name {
                s.push(' ');
                s.push_str(&name);
            }
            if let Some(size) = ptr.size {
                s.push_str(&format!(" {}", human_size(size)));
            }
            if !cfg.can_load(&mime) {
                s.push_str(" (unreadable)");
            } else if inline == Inline::Shown && cfg.sent_inline(&mime) {
                s.push_str(" (shown above)");
            }
            listed.push(s);
        }
        if listed.is_empty() {
            return String::new();
        }
        format!("[attachments: {}]", listed.join(", "))
    }

    fn get(&self, id: usize) -> Option<&Entry> {
        id.checked_sub(1).and_then(|i| self.entries.get(i))
    }

    fn valid_ids(&self) -> String {
        match self.entries.len() {
            0 => "there are no attachments in this conversation".to_string(),
            1 => "the only id is 1".to_string(),
            n => format!("valid ids are 1 to {n}"),
        }
    }
}

// signal gives every upload its own digest, so the same file quoted from
// outside the window is recognised as one already numbered. without a digest
// there is nothing to compare, so it counts as a new attachment.
pub fn same_file(a: &AttachmentPointer, b: &AttachmentPointer) -> bool {
    match (a.digest.as_deref(), b.digest.as_deref()) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn human_size(bytes: u32) -> String {
    match bytes {
        b if b >= 1024 * 1024 => format!("{:.1}MB", b as f64 / (1024.0 * 1024.0)),
        b if b >= 1024 => format!("{}KB", b / 1024),
        b => format!("{b}B"),
    }
}

pub fn definitions() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": LOAD_AVATAR,
                "description": "Look at someone's profile picture, or the group's own picture. \
                                Takes a display name exactly as it appears in this chat.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "the person's display name, or the group's title"
                        }
                    },
                    "required": ["name"]
                }
            }
        }),
        serde_json::json!({
        "type": "function",
        "function": {
            "name": LOAD_ATTACHMENT,
            "description": "Load an attachment from this chat so you can see, hear or read it. \
                            Attachments are listed in the messages as [attachments: #id type name size]. \
                            Only call this for an id that appears in the conversation.",
            "parameters": {
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "the id shown after # in the attachments annotation"
                    }
                },
                "required": ["id"]
            }
        }
        }),
    ]
}

// what one tool call produced: the text handed back as the tool result, plus any
// media that has to travel in a separate user turn (a tool result is text-only)
pub struct ToolResult {
    pub content: String,
    pub media: Option<Media>,
}

impl ToolResult {
    fn text(content: impl Into<String>) -> Self {
        ToolResult {
            content: content.into(),
            media: None,
        }
    }
}

// who the model may ask for a picture of: the thread's own participants, and
// the group itself. never the rest of the contact store.
async fn avatar_of<S: Store>(
    manager: &mut Manager<S, Registered>,
    names: &Names,
    thread: &Thread,
    wanted: &str,
) -> Option<(String, Vec<u8>)> {
    let matches = |name: &str| name.eq_ignore_ascii_case(wanted.trim());
    let mut people: Vec<ServiceId> = Vec::new();
    match thread {
        Thread::Contact(sid) => {
            people.push(*sid);
            people.push(ServiceId::Aci(Aci::from(names.my_aci())));
        }
        Thread::Group(master_key) => {
            let group = manager.store().group(*master_key).await.ok().flatten()?;
            if matches(&group.title) {
                let context = GroupContextV2 {
                    master_key: Some(master_key.to_vec()),
                    revision: Some(0),
                    ..Default::default()
                };
                let bytes = manager
                    .retrieve_group_avatar(context)
                    .await
                    .ok()
                    .flatten()?;
                return Some((String::new(), bytes));
            }
            people.extend(group.members.iter().map(|m| ServiceId::Aci(m.aci)));
        }
    }

    for sid in people {
        if !matches(&names.of(manager, &sid).await) {
            continue;
        }
        // our own account has no contact record, so its key comes from the
        // registration
        let key = match sid.raw_uuid() == names.my_aci() {
            true => Some(manager.registration_data().profile_key()),
            false => manager.store().profile_key(&sid).await.ok().flatten(),
        };
        if let Some(key) = key {
            if let Ok(Some(bytes)) = manager
                .retrieve_profile_avatar_by_uuid(sid.raw_uuid(), key)
                .await
            {
                return Some((String::new(), bytes));
            }
        }
        if let Ok(Some(contact)) = manager.store().contact_by_id(&sid).await {
            if let Some(avatar) = contact.avatar {
                return Some((avatar.content_type, avatar.reader.to_vec()));
            }
        }
    }
    None
}

pub async fn dispatch<S: Store>(
    manager: &mut Manager<S, Registered>,
    cfg: &Config,
    names: &Names,
    thread: &Thread,
    catalog: &Catalog,
    call: &ToolCall,
) -> ToolResult {
    if call.name == LOAD_AVATAR {
        return load_avatar(manager, cfg, names, thread, call).await;
    }
    if call.name != LOAD_ATTACHMENT {
        return ToolResult::text(format!("error: no tool named {}", call.name));
    }
    let Some(id) = call.arg("id").and_then(|v| v.as_u64()) else {
        return ToolResult::text("error: expected an integer `id` argument");
    };
    let id = id as usize;
    let Some(entry) = catalog.get(id) else {
        return ToolResult::text(format!(
            "error: no attachment #{id} — {}. Don't retry with a guess.",
            catalog.valid_ids()
        ));
    };
    info!(id, mime = %entry.mime, "loading attachment");

    if !cfg.can_load(&entry.mime) {
        return ToolResult::text(format!(
            "error: attachment #{id} is {} and cannot be read",
            entry.mime
        ));
    }

    let ptrs = std::slice::from_ref(&entry.ptr);
    if kind_of(&entry.mime) == Some(Kind::Text) {
        return match fetch_raw(manager, ptrs, "text").await.pop() {
            Some((_, bytes)) => ToolResult::text(truncated_text(&bytes)),
            None => ToolResult::text(format!("error: attachment #{id} could not be loaded")),
        };
    }
    let media = fetch_media(manager, cfg, ptrs).await;
    if media.is_empty() {
        return ToolResult::text(format!("error: attachment #{id} could not be loaded"));
    }
    ToolResult {
        content: format!("attachment #{id} follows"),
        media: Some(media),
    }
}

async fn load_avatar<S: Store>(
    manager: &mut Manager<S, Registered>,
    cfg: &Config,
    names: &Names,
    thread: &Thread,
    call: &ToolCall,
) -> ToolResult {
    if !cfg.vision {
        return ToolResult::text("error: I can't see images");
    }
    let Some(wanted) = call
        .arg("name")
        .and_then(|v| v.as_str().map(str::to_string))
    else {
        return ToolResult::text("error: expected a `name` argument");
    };
    info!(name = %wanted, "loading avatar");

    let Some((mime, bytes)) = avatar_of(manager, names, thread, &wanted).await else {
        return ToolResult::text(format!("error: no picture for \"{wanted}\" in this chat"));
    };
    // a profile avatar arrives as bare bytes, so the encoder decides its type
    let Some(image) = encode_image(&mime, bytes) else {
        return ToolResult::text(format!("error: the picture for \"{wanted}\" is unreadable"));
    };
    ToolResult {
        content: format!("{wanted}'s picture follows"),
        media: Some(Media {
            images: vec![image],
            ..Media::default()
        }),
    }
}

fn truncated_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_TEXT_BYTES {
        return text.into_owned();
    }
    let mut cut = MAX_TEXT_BYTES;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[truncated]", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::AiClient;

    fn config(vision: bool, audio: bool, documents: bool) -> Config {
        Config {
            trigger: "@ai".to_string(),
            processing_msg: String::new(),
            reasoning_msg: String::new(),
            generating_msg: String::new(),
            tool_msg: String::new(),
            context_messages: 5,
            vision,
            audio,
            documents,
            audio_speed: 1.0,
            retention: None,
            ai: AiClient::new(
                String::new(),
                String::new(),
                String::new(),
                None,
                0,
                Vec::new(),
            ),
        }
    }

    fn pointer(mime: &str, name: &str, size: u32) -> AttachmentPointer {
        AttachmentPointer {
            content_type: Some(mime.to_string()),
            file_name: Some(name.to_string()),
            size: Some(size),
            ..Default::default()
        }
    }

    #[test]
    fn numbers_every_attachment_and_marks_the_unreadable() {
        let cfg = config(true, false, false);
        let mut catalog = Catalog::default();
        let note = catalog.announce(
            &cfg,
            &[
                pointer("image/jpeg", "photo.jpg", 2 * 1024 * 1024),
                pointer("application/pdf", "report.pdf", 56 * 1024),
                pointer("audio/aac", "note.aac", 1024),
            ],
            Inline::Listed,
        );
        assert_eq!(
            note,
            "[attachments: #1 image/jpeg photo.jpg 2.0MB, \
             #2 application/pdf report.pdf 56KB (unreadable), \
             #3 audio/aac note.aac 1KB (unreadable)]"
        );
    }

    #[test]
    fn marks_what_the_turn_already_carries() {
        let cfg = config(true, false, false);
        let mut catalog = Catalog::default();
        let note = catalog.announce(
            &cfg,
            &[
                pointer("image/jpeg", "photo.jpg", 1),
                pointer("text/plain", "notes.txt", 1),
            ],
            Inline::Shown,
        );
        assert_eq!(
            note,
            "[attachments: #1 image/jpeg photo.jpg 1B (shown above), #2 text/plain notes.txt 1B]"
        );
    }

    #[test]
    fn documents_are_readable_only_behind_the_flag() {
        let pdf = [pointer("application/pdf", "report.pdf", 1)];
        let note = Catalog::default().announce(&config(true, false, false), &pdf, Inline::Listed);
        assert!(note.ends_with("(unreadable)]"));
        let note = Catalog::default().announce(&config(true, false, true), &pdf, Inline::Listed);
        assert!(!note.contains("unreadable"));
    }

    #[test]
    fn reads_structured_text_formats() {
        for mime in [
            "text/plain; charset=utf-8",
            "text/x-signal-plain",
            "application/json",
            "application/x-yaml",
            "application/ld+json",
        ] {
            assert!(
                matches!(kind_of(mime), Some(Kind::Text)),
                "{mime} should be text"
            );
        }
        assert!(kind_of("application/zip").is_none());
    }

    #[test]
    fn gives_the_same_file_one_id() {
        let cfg = config(true, false, false);
        let mut catalog = Catalog::default();
        let mut photo = pointer("image/jpeg", "photo.jpg", 1);
        photo.digest = Some(vec![1, 2, 3]);

        catalog.announce(&cfg, std::slice::from_ref(&photo), Inline::Listed);
        let again = catalog.announce(&cfg, &[photo], Inline::Shown);
        assert!(again.starts_with("[attachments: #1 "));
        assert!(catalog.get(2).is_none());
    }

    #[test]
    fn keeps_numbering_across_calls() {
        let cfg = config(true, false, false);
        let mut catalog = Catalog::default();
        catalog.announce(&cfg, &[pointer("image/jpeg", "a.jpg", 1)], Inline::Listed);
        let note = catalog.announce(&cfg, &[pointer("image/jpeg", "b.jpg", 1)], Inline::Listed);
        assert!(note.starts_with("[attachments: #2 "));
    }

    #[test]
    fn announces_nothing_without_attachments() {
        let cfg = config(false, false, false);
        let mut catalog = Catalog::default();
        assert!(catalog.announce(&cfg, &[], Inline::Listed).is_empty());
    }

    #[test]
    fn resolves_ids_within_the_catalog_only() {
        let cfg = config(true, false, false);
        let mut catalog = Catalog::default();
        catalog.announce(&cfg, &[pointer("image/jpeg", "a.jpg", 1)], Inline::Listed);
        assert!(catalog.get(1).is_some());
        assert!(catalog.get(0).is_none());
        assert!(catalog.get(2).is_none());
    }

    #[test]
    fn truncates_text_on_a_char_boundary() {
        let long = "ä".repeat(MAX_TEXT_BYTES);
        let out = truncated_text(long.as_bytes());
        assert!(out.ends_with("\n[truncated]"));
        assert!(out.len() < long.len());
    }
}
