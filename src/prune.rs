use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::manager::Registered;
use presage::store::{Store, Thread};
use presage::Manager;
use tracing::{info, warn};

use crate::handler::now_ts;

// collect every thread we know about: one per contact, one per group. presage
// has no unified thread list, so we union the two.
async fn all_threads<S: Store>(manager: &Manager<S, Registered>) -> Vec<Thread> {
    let mut threads = Vec::new();
    match manager.store().contacts().await {
        Ok(contacts) => threads.extend(
            contacts
                .filter_map(Result::ok)
                .map(|c| Thread::Contact(ServiceId::Aci(Aci::from(c.uuid)))),
        ),
        Err(e) => warn!(%e, "could not list contacts for pruning"),
    }
    match manager.store().groups().await {
        Ok(groups) => threads.extend(
            groups
                .filter_map(Result::ok)
                .map(|(master_key, _)| Thread::Group(master_key)),
        ),
        Err(e) => warn!(%e, "could not list groups for pruning"),
    }
    threads
}

// delete every stored message older than `retention_ms` across all threads.
// only touches the message store; presage's protocol state is untouched, so
// this never affects linking or decryption.
pub async fn prune_old_messages<S: Store + Clone>(
    manager: &Manager<S, Registered>,
    retention_ms: u64,
) {
    let cutoff = now_ts().saturating_sub(retention_ms);
    // `delete_message` needs `&mut`; the sqlite store is a cheap handle over a
    // shared pool, so a clone points at the same db
    let mut store = manager.store().clone();
    let mut deleted = 0usize;

    for thread in all_threads(manager).await {
        let Ok(iter) = manager.store().messages(&thread, 0..cutoff).await else {
            continue;
        };
        // the range bounds aren't reliably honoured, so filter ourselves. drain
        // the iterator into a vec before deleting to end its borrow of the store.
        let stale: Vec<u64> = iter
            .filter_map(Result::ok)
            .map(|c| c.metadata.timestamp)
            .filter(|ts| *ts < cutoff)
            .collect();
        for ts in stale {
            match store.delete_message(&thread, ts).await {
                Ok(true) => deleted += 1,
                Ok(false) => {}
                Err(e) => warn!(%e, "failed to delete message"),
            }
        }
    }

    if deleted > 0 {
        info!(deleted, "pruned old messages");
    }
}
