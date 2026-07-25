use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context as _;
use base64::prelude::*;
use presage::libsignal_service::proto::AttachmentPointer;
use presage::manager::Registered;
use presage::store::Store;
use presage::Manager;
use tracing::{error, warn};

use crate::media::fetch_raw;

// gemma 4 takes 16 kHz mono audio, at most 30s per clip
const SAMPLE_RATE: u32 = 16_000;
const CHUNK_SECS: usize = 30;
// a longer voice note is truncated rather than flooding the context: audio costs
// ~25 tokens per second, so this caps one note at ~7.5k tokens
const MAX_CHUNKS: usize = 10;
const MAX_SECS: usize = CHUNK_SECS * MAX_CHUNKS;

// s16le, one channel
const BYTES_PER_SAMPLE: usize = 2;
const CHUNK_BYTES: usize = SAMPLE_RATE as usize * CHUNK_SECS * BYTES_PER_SAMPLE;

// is ffmpeg callable? checked once at startup so a missing binary is reported
// there rather than once per voice message
pub async fn have_ffmpeg() -> bool {
    tokio::process::Command::new("ffmpeg")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

// write the attachment somewhere ffmpeg can seek. an m4a voice note (ios) keeps
// its index at the end of the file, so demuxing it from a stdin pipe fails.
async fn stage(data: &[u8]) -> anyhow::Result<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "signal-ai-bot-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    tokio::fs::write(&path, data)
        .await
        .with_context(|| format!("failed to stage audio at {}", path.display()))?;
    Ok(path)
}

// decode any container ffmpeg understands into raw 16 kHz mono s16le pcm.
// signal voice notes arrive as ogg/opus or aac, neither of which llama.cpp can
// read. `speed` above 1.0 fits more audio into each chunk via atempo.
async fn decode_pcm(path: &Path, speed: f32) -> anyhow::Result<Vec<u8>> {
    let mut args: Vec<String> = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-i".to_string(),
        path.to_string_lossy().into_owned(),
    ];
    if speed != 1.0 {
        args.push("-filter:a".to_string());
        args.push(format!("atempo={speed}"));
    }
    // -t bounds the decode so a long recording can't balloon into memory; it
    // applies to the sped-up output, which is what we chunk
    args.extend([
        "-t".to_string(),
        MAX_SECS.to_string(),
        "-f".to_string(),
        "s16le".to_string(),
        "-acodec".to_string(),
        "pcm_s16le".to_string(),
        "-ar".to_string(),
        SAMPLE_RATE.to_string(),
        "-ac".to_string(),
        "1".to_string(),
        "pipe:1".to_string(),
    ]);

    let out = tokio::process::Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run ffmpeg")?;
    // a partial decode is still worth keeping, so only a run that produced
    // nothing counts as an error
    if !out.status.success() && out.stdout.is_empty() {
        anyhow::bail!("ffmpeg: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

// stage the attachment, decode it, and clean up regardless of the outcome
async fn transcode(data: &[u8], speed: f32) -> anyhow::Result<Vec<u8>> {
    let path = stage(data).await?;
    let pcm = decode_pcm(&path, speed).await;
    let _ = tokio::fs::remove_file(&path).await;
    pcm
}

// wrap raw pcm in a 44-byte RIFF/WAVE header. llama.cpp sniffs magic bytes and
// only decodes wav, mp3 and flac, so the container has to be one of those.
fn to_wav(pcm: &[u8]) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let block_align = BYTES_PER_SAMPLE as u16;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // pcm
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * block_align as u32).to_le_bytes()); // byte rate
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

// download audio attachments and base64-encode each as one or more wav clips of
// at most 30s, skipping anything empty or undecodable. a clip longer than that
// becomes several consecutive chunks.
pub async fn fetch_audio<S: Store>(
    manager: &mut Manager<S, Registered>,
    ptrs: &[AttachmentPointer],
    speed: f32,
) -> Vec<String> {
    let mut out = Vec::new();
    for (mime, data) in fetch_raw(manager, ptrs, "audio").await {
        let pcm = match transcode(&data, speed).await {
            Ok(p) if !p.is_empty() => p,
            Ok(_) => {
                warn!(mime, "audio attachment decoded to no samples, skipping");
                continue;
            }
            Err(e) => {
                error!(%e, mime, "could not decode audio attachment, skipping");
                continue;
            }
        };
        if pcm.len() >= MAX_SECS * SAMPLE_RATE as usize * BYTES_PER_SAMPLE {
            warn!(mime, MAX_SECS, "audio attachment too long, truncated");
        }
        out.extend(
            pcm.chunks(CHUNK_BYTES)
                .map(|c| BASE64_STANDARD.encode(to_wav(c))),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // these drive the real ffmpeg path, so they no-op where it isn't installed
    macro_rules! needs_ffmpeg {
        () => {
            if !have_ffmpeg().await {
                eprintln!("skipping: ffmpeg not on PATH");
                return;
            }
        };
    }

    async fn encode_fixture(secs: u32, codec: &str, ext: &str) -> Vec<u8> {
        // tests run concurrently, so each fixture needs its own path
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fixture-{secs}-{}.{ext}",
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let st = tokio::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={secs}"),
                "-c:a",
                codec,
            ])
            .arg(&path)
            .status()
            .await
            .unwrap();
        assert!(st.success(), "fixture encode failed");
        let bytes = tokio::fs::read(&path).await.unwrap();
        let _ = tokio::fs::remove_file(&path).await;
        bytes
    }

    #[tokio::test]
    async fn decodes_opus_and_m4a_to_expected_pcm_length() {
        needs_ffmpeg!();
        for (codec, ext) in [("libopus", "ogg"), ("aac", "m4a")] {
            let src = encode_fixture(65, codec, ext).await;
            let pcm = transcode(&src, 1.0).await.expect(ext);
            let secs = pcm.len() as f64 / (SAMPLE_RATE as f64 * BYTES_PER_SAMPLE as f64);
            assert!((secs - 65.0).abs() < 0.5, "{ext}: got {secs}s of pcm");
        }
    }

    #[tokio::test]
    async fn chunks_a_long_note_into_30s_wavs() {
        needs_ffmpeg!();
        let src = encode_fixture(65, "libopus", "ogg").await;
        let pcm = transcode(&src, 1.0).await.unwrap();
        let chunks: Vec<_> = pcm.chunks(CHUNK_BYTES).map(to_wav).collect();
        assert_eq!(chunks.len(), 3, "65s should split into 30+30+5");
        for (i, w) in chunks.iter().enumerate() {
            assert_eq!(&w[0..4], b"RIFF", "chunk {i} magic");
            assert_eq!(&w[8..12], b"WAVE", "chunk {i} format");
            // the header must declare the exact payload that follows it
            let declared = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
            assert_eq!(declared, w.len() - 44, "chunk {i} data size");
        }
        assert_eq!(chunks[0].len(), 44 + CHUNK_BYTES);
    }

    #[tokio::test]
    async fn speeding_up_shortens_the_note() {
        needs_ffmpeg!();
        let src = encode_fixture(60, "libopus", "ogg").await;
        let fast = transcode(&src, 1.5).await.unwrap();
        let secs = fast.len() as f64 / (SAMPLE_RATE as f64 * BYTES_PER_SAMPLE as f64);
        assert!(
            (secs - 40.0).abs() < 0.5,
            "60s at 1.5x should be ~40s, got {secs}"
        );
        assert_eq!(fast.chunks(CHUNK_BYTES).count(), 2);
    }

    #[tokio::test]
    async fn caps_a_very_long_note() {
        needs_ffmpeg!();
        let src = encode_fixture((MAX_SECS + 120) as u32, "libopus", "ogg").await;
        let pcm = transcode(&src, 1.0).await.unwrap();
        assert_eq!(pcm.chunks(CHUNK_BYTES).count(), MAX_CHUNKS);
    }

    #[tokio::test]
    async fn rejects_garbage() {
        needs_ffmpeg!();
        assert!(transcode(b"not audio at all", 1.0).await.is_err());
    }
}
