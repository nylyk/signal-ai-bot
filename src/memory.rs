use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use presage::libsignal_service::prelude::{GroupMasterKey, GroupSecretParams};
use presage::store::Thread;
use tracing::{info, warn};

use crate::ai::ToolCall;

const DIR: &str = "/data/memories";

const MAX_NAME_LEN: usize = 64;
const MAX_MEMORY_BYTES: usize = 8 * 1024;

const LIST: &str = "list_memories";
const READ: &str = "read_memory";
const SAVE: &str = "save_memory";
const DELETE: &str = "delete_memory";

pub fn handles(tool: &str) -> bool {
    [LIST, READ, SAVE, DELETE].contains(&tool)
}

// the folder name must never reveal a group's master key, which is its secret
pub fn dir_of(thread: &Thread) -> PathBuf {
    let chat = match thread {
        Thread::Contact(sid) => sid.raw_uuid().to_string(),
        Thread::Group(master_key) => {
            let params =
                GroupSecretParams::derive_from_master_key(GroupMasterKey::new(*master_key));
            params
                .get_group_identifier()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        }
    };
    Path::new(DIR).join(chat)
}

pub fn definitions() -> Vec<serde_json::Value> {
    let name = serde_json::json!({
        "type": "string",
        "description": "lowercase letters, digits and dashes naming one aspect, e.g. alice-birthday"
    });
    vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": LIST,
                "description": "List the names of the memories saved in this chat. \
                                The system prompt lists them as of the start of this conversation; \
                                call this for the current list.",
                "parameters": { "type": "object", "properties": {} }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": READ,
                "description": "Read one saved memory by name.",
                "parameters": {
                    "type": "object",
                    "properties": { "name": name },
                    "required": ["name"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": SAVE,
                "description": "Save a memory for later conversations in this chat, replacing any memory with the same name. \
                                Each memory covers exactly one aspect. To add to an aspect that \
                                already has a memory, read it and save the combined text under \
                                the same name rather than starting a second one.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": name,
                        "content": {
                            "type": "string",
                            "description": "the memory as markdown"
                        }
                    },
                    "required": ["name", "content"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": DELETE,
                "description": "Delete a saved memory that is wrong or no longer needed.",
                "parameters": {
                    "type": "object",
                    "properties": { "name": name },
                    "required": ["name"]
                }
            }
        }),
    ]
}

pub async fn dispatch(dir: &Path, call: &ToolCall) -> String {
    if call.name == LIST {
        let names = names(dir).await;
        if names.is_empty() {
            return "no memories saved yet".to_string();
        }
        return names.join("\n");
    }
    let Some(name) = call
        .arg("name")
        .and_then(|v| v.as_str().map(str::to_string))
    else {
        return "error: expected a `name` argument".to_string();
    };
    let Some(path) = path_of(dir, &name) else {
        return format!(
            "error: \"{name}\" is not a valid name; use up to {MAX_NAME_LEN} lowercase \
             letters, digits and dashes"
        );
    };
    info!(tool = %call.name, name, "memory");
    match call.name.as_str() {
        READ => match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(e) if e.kind() == ErrorKind::NotFound => missing(&name),
            Err(e) => format!("error: could not read {name}: {e}"),
        },
        SAVE => {
            let Some(content) = call
                .arg("content")
                .and_then(|v| v.as_str().map(str::to_string))
            else {
                return "error: expected a `content` argument".to_string();
            };
            if content.len() > MAX_MEMORY_BYTES {
                return format!(
                    "error: the memory is {} bytes, the limit is {MAX_MEMORY_BYTES}",
                    content.len()
                );
            }
            let staged = dir.join(format!(".{name}.md.tmp"));
            let written = async {
                tokio::fs::create_dir_all(dir).await?;
                tokio::fs::write(&staged, content).await?;
                tokio::fs::rename(&staged, &path).await
            };
            match written.await {
                Ok(()) => format!("saved {name}"),
                Err(e) => format!("error: could not save {name}: {e}"),
            }
        }
        DELETE => match tokio::fs::remove_file(&path).await {
            Ok(()) => format!("deleted {name}"),
            Err(e) if e.kind() == ErrorKind::NotFound => missing(&name),
            Err(e) => format!("error: could not delete {name}: {e}"),
        },
        other => format!("error: no tool named {other}"),
    }
}

fn missing(name: &str) -> String {
    format!(
        "error: no memory named {name}. The memories may have changed since this \
         conversation started; call {LIST} for the current list."
    )
}

pub async fn prompt_section(dir: &Path) -> String {
    let names = names(dir).await;
    if names.is_empty() {
        return "---\nSaved memories: none yet".to_string();
    }
    format!(
        "---\nSaved memories, read one with {READ} when it could matter:\n- {}",
        names.join("\n- ")
    )
}

async fn names(dir: &Path) -> Vec<String> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warn!(%e, dir = %dir.display(), "could not list memories");
            return Vec::new();
        }
    };
    let mut names = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if is_valid(stem) {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    names
}

fn is_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn path_of(dir: &Path, name: &str) -> Option<PathBuf> {
    is_valid(name).then(|| dir.join(format!("{name}.md")))
}

#[cfg(test)]
mod tests {
    use presage::libsignal_service::protocol::{Aci, ServiceId};

    use super::*;

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: String::new(),
            name: name.to_string(),
            arguments: arguments.to_string(),
        }
    }

    #[test]
    fn rejects_names_that_leave_the_folder() {
        let dir = Path::new("/memories");
        assert_eq!(
            path_of(dir, "alice-birthday"),
            Some(dir.join("alice-birthday.md"))
        );
        for bad in ["", "../secret", "a/b", "Alice", "a.b", &"a".repeat(65)] {
            assert!(path_of(dir, bad).is_none(), "{bad} should be rejected");
        }
    }

    #[test]
    fn gives_each_chat_its_own_folder_without_exposing_the_group_key() {
        let key = [7u8; 32];
        let group = dir_of(&Thread::Group(key));
        let folder = group.file_name().unwrap().to_str().unwrap();
        assert_eq!(folder.len(), 64);
        assert!(!folder.contains(&"07".repeat(32)));
        assert_ne!(group, dir_of(&Thread::Group([8u8; 32])));

        let uuid = presage::libsignal_service::prelude::Uuid::from_bytes([1; 16]);
        let contact = Thread::Contact(ServiceId::Aci(Aci::from(uuid)));
        assert_eq!(dir_of(&contact), Path::new(DIR).join(uuid.to_string()));
    }

    #[tokio::test]
    async fn saves_reads_and_deletes_and_lists_names() {
        let dir = std::env::temp_dir().join(format!("signal-ai-bot-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(prompt_section(&dir).await, "---\nSaved memories: none yet");
        let saved = call(
            SAVE,
            serde_json::json!({ "name": "bob-pets", "content": "a cat" }),
        );
        assert_eq!(dispatch(&dir, &saved).await, "saved bob-pets");
        let saved = call(
            SAVE,
            serde_json::json!({ "name": "alice-diet", "content": "vegan" }),
        );
        dispatch(&dir, &saved).await;

        assert_eq!(
            prompt_section(&dir).await,
            "---\nSaved memories, read one with read_memory when it could matter:\n\
             - alice-diet\n- bob-pets"
        );
        let read = call(READ, serde_json::json!({ "name": "bob-pets" }));
        assert_eq!(dispatch(&dir, &read).await, "a cat");
        assert_eq!(
            dispatch(&dir, &call(LIST, serde_json::json!({}))).await,
            "alice-diet\nbob-pets"
        );
        std::fs::write(dir.join("Not-Valid.md"), "x").unwrap();
        assert!(!prompt_section(&dir).await.contains("Not-Valid"));

        let delete = call(DELETE, serde_json::json!({ "name": "bob-pets" }));
        assert_eq!(dispatch(&dir, &delete).await, "deleted bob-pets");
        assert!(dispatch(&dir, &read).await.contains("call list_memories"));
        assert!(dispatch(&dir, &delete).await.contains("call list_memories"));

        let huge = "x".repeat(MAX_MEMORY_BYTES + 1);
        let too_big = call(SAVE, serde_json::json!({ "name": "big", "content": huge }));
        assert!(dispatch(&dir, &too_big).await.starts_with("error:"));
        assert!(!dir.join("big.md").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
