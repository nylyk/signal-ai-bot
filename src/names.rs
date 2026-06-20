use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::manager::Registered;
use presage::store::Store;
use presage::Manager;
use tracing::warn;

// resolves signal display names: our own account maps to its profile name
// (fetched once at startup), everyone else to their contact-store name.
pub struct Names {
    my_aci: Uuid,
    my_name: String,
}

impl Names {
    // falls back to "Me" if the profile name can't be fetched (e.g. offline)
    pub async fn resolve<S: Store>(manager: &mut Manager<S, Registered>) -> Self {
        let my_aci = manager.registration_data().service_ids.aci;
        let my_name = match manager.retrieve_profile().await {
            Ok(profile) => profile
                .name
                .map(|n| n.to_string().trim().to_string())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "Me".to_string()),
            Err(e) => {
                warn!(%e, "could not fetch own profile name, labelling own messages \"Me\"");
                "Me".to_string()
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
            .unwrap_or_else(|| "Them".to_string())
    }

    pub async fn of_uuid<S: Store>(&self, manager: &Manager<S, Registered>, uuid: Uuid) -> String {
        self.of(manager, &ServiceId::Aci(Aci::from(uuid))).await
    }
}
