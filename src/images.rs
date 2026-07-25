use base64::prelude::*;
use presage::libsignal_service::proto::AttachmentPointer;
use presage::manager::Registered;
use presage::store::Store;
use presage::Manager;
use tracing::warn;

use crate::media::fetch_raw;

// llama.cpp's image loader (stb_image) handles jpeg/png/gif/bmp but not webp,
// so transcode anything that isn't already jpeg/png (webp stickers, etc.) to png
fn to_loadable(mime: &str, data: Vec<u8>) -> Option<(String, Vec<u8>)> {
    if mime == "image/jpeg" || mime == "image/png" {
        return Some((mime.to_string(), data));
    }
    let img = image::load_from_memory(&data).ok()?;
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(("image/png".to_string(), out))
}

// download image attachments and base64-encode them as (mime, data) pairs,
// skipping anything empty or undecodable
pub async fn fetch_images<S: Store>(
    manager: &mut Manager<S, Registered>,
    ptrs: &[AttachmentPointer],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (mime, data) in fetch_raw(manager, ptrs, "image").await {
        match to_loadable(&mime, data) {
            Some((mime, bytes)) => out.push((mime, BASE64_STANDARD.encode(&bytes))),
            None => warn!(mime, "could not decode image attachment, skipping"),
        }
    }
    out
}
