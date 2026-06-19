use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use futures::{pin_mut, StreamExt};
use presage::libsignal_service::content::DataMessage;
use presage::manager::Registered;
use presage::model::messages::Received;
use presage::store::{ContentExt, Store};
use presage::Manager;
use tracing::{error, info};

use crate::config::Config;
use crate::history::{thread_history, HistMsg};
use crate::images::fetch_images;
use crate::message::{content_images, extract, resolve_author, ReplyRef, Trigger};
use crate::names::Names;
use crate::recipient::{send_edit, send_to, Recipient};
use crate::replies::AiReplies;

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64
}

// render a user turn as `<speaker>: <body>`, prefixed with a reply annotation
// naming who is being replied to (and their quoted text) when present, so the
// model can tell people apart and follow reply chains.
fn format_user_turn(speaker: &str, reply_to: Option<&ReplyRef>, body: &str) -> String {
    let mut s = String::new();
    if let Some(r) = reply_to {
        s.push_str("[in reply to ");
        s.push_str(&r.author);
        if !r.text.is_empty() {
            // collapse newlines so the quote stays on the annotation line
            s.push_str(&format!(", who wrote: \"{}\"", r.text.replace('\n', " ")));
        }
        s.push_str("]:\n");
    }
    s.push_str(speaker);
    s.push_str(": ");
    s.push_str(body);
    s
}

// build the chat-completions message array: each past message is its own turn
// (assistant for the bot, user otherwise), the current question last, with any
// images attached to that final turn as openai-style image_url parts.
async fn build_convo<S: Store>(
    manager: &mut Manager<S, Registered>,
    names: &Names,
    sender: &str,
    trigger: &Trigger,
    history: &[HistMsg],
    vision: bool,
) -> (Vec<serde_json::Value>, usize) {
    let mut convo: Vec<serde_json::Value> = Vec::new();
    for h in history {
        if h.is_ai {
            convo.push(serde_json::json!({ "role": "assistant", "content": h.text.clone() }));
        } else {
            let content = format_user_turn(&h.speaker, h.reply_to.as_ref(), &h.text);
            convo.push(serde_json::json!({ "role": "user", "content": content }));
        }
    }

    // resolve who this message is replying to (text comes from the quote itself)
    let is_reply = trigger.quoted_author.is_some()
        || trigger.quoted_ts.is_some()
        || trigger.quoted_text.is_some();
    let reply_to = if is_reply {
        let author = resolve_author(
            manager,
            names,
            Some(&trigger.thread),
            trigger.quoted_author,
            trigger.quoted_ts,
        )
        .await;
        Some(ReplyRef {
            author,
            text: trigger.quoted_text.clone().unwrap_or_default(),
        })
    } else {
        None
    };
    // keep the @ai prefix so this message looks like the past trigger messages
    // in the context, rather than a stripped one; prefix the asker's name so the
    // model knows who is asking
    let q = format_user_turn(sender, reply_to.as_ref(), trigger.body.trim());

    let mut imgs = if vision {
        fetch_images(manager, &trigger.own_images).await
    } else {
        Vec::new()
    };

    // replied-to image: prefer the full-resolution original from the local
    // store, but fall back to the quote's embedded thumbnail whenever that
    // yields nothing — the original may not be stored, or its stored pointer
    // may not be downloadable
    if vision {
        if let Some(qts) = trigger.quoted_ts {
            let from_store = match manager.store().message(&trigger.thread, qts).await {
                Ok(Some(orig)) => content_images(&orig),
                _ => Vec::new(),
            };
            let mut reply_imgs = fetch_images(manager, &from_store).await;
            let used_thumb = reply_imgs.is_empty();
            if reply_imgs.is_empty() {
                reply_imgs = fetch_images(manager, &trigger.quoted_thumbs).await;
            }
            info!(
                store_ptrs = from_store.len(),
                thumb_ptrs = trigger.quoted_thumbs.len(),
                fetched = reply_imgs.len(),
                used_thumb,
                "resolved reply image"
            );
            imgs.extend(reply_imgs);
        } else if !trigger.quoted_thumbs.is_empty() {
            let thumbs = fetch_images(manager, &trigger.quoted_thumbs).await;
            imgs.extend(thumbs);
        }
    }

    let image_count = imgs.len();
    if imgs.is_empty() {
        convo.push(serde_json::json!({ "role": "user", "content": q }));
    } else {
        let mut parts = vec![serde_json::json!({ "type": "text", "text": q })];
        for (mime, data) in &imgs {
            parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime};base64,{data}") }
            }));
        }
        convo.push(serde_json::json!({ "role": "user", "content": parts }));
    }

    (convo, image_count)
}

// post the "thinking" placeholder, ask the ai, then edit the placeholder into
// the answer. returns the placeholder's sent-timestamp and the final answer so
// the caller can record it (presage won't persist this edit).
async fn handle_trigger<S: Store>(
    manager: &mut Manager<S, Registered>,
    cfg: &Config,
    recipient: Recipient,
    trigger_ts: u64,
    convo: Vec<serde_json::Value>,
) -> anyhow::Result<(u64, String)> {
    // post the placeholder, forced strictly after the trigger so it sorts after
    // the prompt regardless of clock skew
    let thinking_ts = now_ts().max(trigger_ts + 1);
    let placeholder = DataMessage {
        body: Some(cfg.thinking_msg.clone()),
        timestamp: Some(thinking_ts),
        group_v2: recipient.group_context(),
        ..Default::default()
    };
    send_to(manager, &recipient, placeholder.into(), thinking_ts).await?;

    // ask the ai, then edit the placeholder to the answer
    let answer = match cfg.ai.complete(convo).await {
        Ok(a) if !a.is_empty() => a,
        Ok(_) => "(empty response)".to_string(),
        Err(e) => {
            error!(%e, "ai request failed");
            format!("ai error: {e}")
        }
    };

    let edit_ts = now_ts().max(thinking_ts + 1);
    if let Err(e) = send_edit(manager, &recipient, thinking_ts, answer.clone(), edit_ts).await {
        error!(%e, "edit failed");
    }

    Ok((thinking_ts, answer))
}

// handle a single received message: ignore anything that isn't a trigger, then
// gather context, build the prompt, and answer.
async fn process_content<S: Store>(
    manager: &mut Manager<S, Registered>,
    cfg: &Config,
    names: &Names,
    replies: &mut AiReplies,
    content: &presage::libsignal_service::content::Content,
) {
    let Some(t) = extract(content) else {
        return;
    };
    let trimmed = t.body.trim_start();
    let Some(rest) = trimmed.strip_prefix(&cfg.trigger) else {
        return;
    };
    let question = rest.trim();
    let is_reply = t.quoted_text.is_some() || t.quoted_ts.is_some() || !t.quoted_thumbs.is_empty();
    // allow an image-only or reply-only prompt (e.g. a photo or a reply
    // captioned just "@ai")
    if question.is_empty() && t.own_images.is_empty() && !is_reply {
        return;
    }
    let Some(recipient) = Recipient::from_thread(&t.thread) else {
        return;
    };

    // who is asking, so the model can be told and the trigger turn labelled
    let sender = names.of(manager, &content.metadata.sender).await;

    // gather recent thread context (excludes the @ai message itself)
    let trigger_ts = content.timestamp();
    let history = thread_history(
        manager,
        &t.thread,
        trigger_ts,
        cfg.context_messages,
        &cfg.thinking_msg,
        names,
        replies,
    )
    .await;

    let (convo, images) = build_convo(manager, names, &sender, &t, &history, cfg.vision).await;

    info!(
        thread = ?t.thread,
        replying = is_reply,
        images,
        context = history.len(),
        "handling @ai prompt"
    );
    match handle_trigger(manager, cfg, recipient, trigger_ts, convo).await {
        Ok((ts, answer)) => replies.record(ts, answer),
        Err(e) => error!(%e, "failed to handle prompt"),
    }
}

pub async fn run_loop<S: Store>(
    mut manager: Manager<S, Registered>,
    cfg: Config,
    mut replies: AiReplies,
) -> anyhow::Result<()> {
    // resolve our own display name once up front (used to label our messages)
    let names = Names::resolve(&mut manager).await;

    let messages = manager
        .receive_messages()
        .await
        .context("failed to start message stream")?;
    pin_mut!(messages);

    // don't answer the backlog that gets replayed on startup. only act on
    // messages that arrive after the initial sync drains (first QueueEmpty).
    let mut live = false;

    info!("running. send `{}  <prompt>` in any chat", cfg.trigger);

    while let Some(received) = messages.next().await {
        match received {
            Received::QueueEmpty => live = true,
            Received::Contacts => {}
            Received::Content(content) => {
                if !live {
                    continue;
                }
                process_content(&mut manager, &cfg, &names, &mut replies, &content).await;
            }
        }
    }
    Ok(())
}
