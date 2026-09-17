use base64::prelude::*;
use presage::libsignal_service::proto::AttachmentPointer;
use presage::manager::Registered;
use presage::store::Store;
use presage::Manager;
use tracing::warn;

use crate::media::fetch_raw;

// base64-encode raw image bytes as a (mime, data) pair the model can load
pub fn encode_image(mime: &str, data: Vec<u8>) -> Option<(String, String)> {
    if mime == "image/jpeg" || mime == "image/png" {
        return Some((mime.to_string(), BASE64_STANDARD.encode(&data)));
    }
    // llama.cpp's image loader (stb_image) handles jpeg/png/gif/bmp but not
    // webp, so transcode anything else (webp stickers, etc.) to png
    let img = image::load_from_memory(&data).ok()?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(("image/png".to_string(), BASE64_STANDARD.encode(&png)))
}

// download image attachments and base64-encode them as (mime, data) pairs,
// skipping anything empty or undecodable
pub async fn fetch_images<S: Store>(
    manager: &mut Manager<S, Registered>,
    ptrs: &[AttachmentPointer],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (p, data) in fetch_raw(manager, ptrs, "image").await {
        let mime = p.content_type();
        match encode_image(mime, data) {
            Some(pair) => out.push(pair),
            None => warn!(mime, "could not decode image attachment, skipping"),
        }
    }
    out
}
