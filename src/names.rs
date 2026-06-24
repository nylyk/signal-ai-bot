use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::manager::Registered;
use presage::store::{Store, Thread};
use presage::Manager;
use tracing::warn;

// last 4 hex of the uuid, e.g. "User a3f9": stable across the session and
// distinguishes people whose profile name we can't resolve
fn fallback_name(uuid: Uuid) -> String {
    let hex = uuid.simple().to_string();
    format!("User {}", &hex[hex.len() - 4..])
}

// resolves signal display names: our own account maps to its profile name
// (fetched once at startup), everyone else to their contact-store name.
pub struct Names {
    my_aci: Uuid,
    my_name: String,
}

impl Names {
    // falls back to a uuid-derived label if the profile name can't be fetched
    pub async fn resolve<S: Store>(manager: &mut Manager<S, Registered>) -> Self {
        let my_aci = manager.registration_data().service_ids.aci;
        let my_name = match manager.retrieve_profile().await {
            Ok(profile) => profile
                .name
                .map(|n| n.to_string().trim().to_string())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| fallback_name(my_aci)),
            Err(e) => {
                warn!(%e, "could not fetch own profile name, using a uuid-derived label");
                fallback_name(my_aci)
            }
        };
        Names { my_aci, my_name }
    }

    pub fn my_aci(&self) -> Uuid {
        self.my_aci
    }

    pub async fn of<S: Store>(&self, manager: &Manager<S, Registered>, id: &ServiceId) -> String {
        if id.raw_uuid() == self.my_aci {
            return self.my_name.clone();
        }
        manager
            .store()
            .contact_by_id(id)
            .await
            .ok()
            .flatten()
            .map(|c| c.name.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| fallback_name(id.raw_uuid()))
    }

    pub async fn of_uuid<S: Store>(&self, manager: &Manager<S, Registered>, uuid: Uuid) -> String {
        self.of(manager, &ServiceId::Aci(Aci::from(uuid))).await
    }

    // a block describing where this conversation is happening, appended to the
    // system prompt so the model knows the chat type and who's involved
    pub async fn chat_context<S: Store>(
        &self,
        manager: &Manager<S, Registered>,
        thread: &Thread,
    ) -> String {
        match thread {
            Thread::Contact(sid) => {
                let other = self.of(manager, sid).await;
                format!(
                    "---\nChat context:\n- Type: direct message\n- Between: \"{}\" and \"{}\"",
                    self.my_name, other
                )
            }
            Thread::Group(master_key) => match manager.store().group(*master_key).await {
                Ok(Some(group)) => {
                    let mut members = Vec::new();
                    for m in &group.members {
                        let name = self.of(manager, &ServiceId::Aci(m.aci)).await;
                        if !members.contains(&name) {
                            members.push(name);
                        }
                    }
                    format!(
                        "---\nChat context:\n- Type: group\n- Group name: \"{}\"\n- Members: \"{}\"",
                        group.title,
                        members.join("\", \"")
                    )
                }
                other => {
                    if let Err(e) = other {
                        warn!(%e, "could not load group for chat context");
                    }
                    "---\nChat context:\n- Type: group".to_string()
                }
            },
        }
    }
}
