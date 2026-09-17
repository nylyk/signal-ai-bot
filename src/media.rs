use presage::libsignal_service::proto::AttachmentPointer;
use presage::manager::Registered;
use presage::store::Store;
use presage::Manager;
use tracing::{error, warn};

use base64::prelude::*;

use crate::audio::fetch_audio;
use crate::config::Config;
use crate::images::fetch_images;
use crate::tools::{of_kind, Kind};

// download attachments, pairing each with its pointer and skipping any that
// fail or come back empty. `kind` only labels the log lines.
pub async fn fetch_raw<'a, S: Store>(
    manager: &mut Manager<S, Registered>,
    ptrs: &'a [AttachmentPointer],
    kind: &str,
) -> Vec<(&'a AttachmentPointer, Vec<u8>)> {
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
        out.push((p, data));
    }
    out
}

// the encoded attachments of one turn: images as (mime, base64), audio as
// base64 wav clips of at most 30s each, documents as (filename, data uri)
#[derive(Default)]
pub struct Media {
    pub images: Vec<(String, String)>,
    pub audio: Vec<String>,
    pub files: Vec<(String, String)>,
}

impl Media {
    pub fn is_empty(&self) -> bool {
        self.images.is_empty() && self.audio.is_empty() && self.files.is_empty()
    }
}

// split a "data:<mime>;base64,<data>" uri into its mime and decoded bytes
pub fn decode_data_uri(uri: &str) -> Option<(String, Vec<u8>)> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.split(';').next()?.to_string();
    let bytes = BASE64_STANDARD.decode(data.trim()).ok()?;
    (!mime.is_empty() && !bytes.is_empty()).then_some((mime, bytes))
}

// download documents as (filename, data uri), skipping anything that fails
pub async fn fetch_documents<S: Store>(
    manager: &mut Manager<S, Registered>,
    ptrs: &[AttachmentPointer],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (i, (p, bytes)) in fetch_raw(manager, ptrs, "document")
        .await
        .into_iter()
        .enumerate()
    {
        let name = p
            .file_name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("document-{}.pdf", i + 1));
        let mut uri = format!("data:{};base64,", p.content_type());
        BASE64_STANDARD.encode_string(&bytes, &mut uri);
        out.push((name, uri));
    }
    out
}

// download whichever of a message's attachments the enabled modalities cover
pub async fn fetch_media<S: Store>(
    manager: &mut Manager<S, Registered>,
    cfg: &Config,
    atts: &[AttachmentPointer],
) -> Media {
    let loadable: Vec<AttachmentPointer> = atts
        .iter()
        .filter(|a| cfg.can_load(a.content_type()))
        .cloned()
        .collect();
    Media {
        images: fetch_images(manager, &of_kind(&loadable, Kind::Image)).await,
        audio: fetch_audio(manager, &of_kind(&loadable, Kind::Audio), cfg.audio_speed).await,
        files: fetch_documents(manager, &of_kind(&loadable, Kind::Document)).await,
    }
}
