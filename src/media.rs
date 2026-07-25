use presage::libsignal_service::proto::AttachmentPointer;
use presage::manager::Registered;
use presage::store::Store;
use presage::Manager;
use tracing::{error, warn};

// download attachments as (mime, bytes), skipping any that fail or come back
// empty. `kind` only labels the log lines.
pub async fn fetch_raw<S: Store>(
    manager: &mut Manager<S, Registered>,
    ptrs: &[AttachmentPointer],
    kind: &str,
) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for p in ptrs {
        let data = match manager.get_attachment(p).await {
            Ok(d) => d,
            Err(e) => {
                error!(%e, kind, "failed to fetch attachment");
                continue;
            }
        };
        if data.is_empty() {
            warn!(kind, "empty attachment, skipping");
            continue;
        }
        out.push((p.content_type.clone().unwrap_or_default(), data));
    }
    out
}
