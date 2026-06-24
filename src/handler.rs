use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use futures::{channel::mpsc, future::join, pin_mut, StreamExt};
use presage::libsignal_service::content::DataMessage;
use presage::libsignal_service::proto::data_message::Quote;
use presage::manager::Registered;
use presage::model::messages::Received;
use presage::store::{ContentExt, Store};
use presage::Manager;
use tracing::{error, info};

use crate::ai::Phase;
use crate::config::Config;
use crate::history::{thread_history, HistMsg};
use crate::images::fetch_images;
use crate::message::{
    content_images, dm_images, extract, is_ai_message, resolve_author, resolve_mentions, ReplyRef,
    Trigger,
};
use crate::names::Names;
use crate::recipient::{send_edit, send_to, Recipient};

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64
}

fn format_user_turn(speaker: &str, reply_to: Option<&ReplyRef>, body: &str) -> String {
    let mut s = String::new();
    if let Some(r) = reply_to {
        s.push_str("[in reply to ");
        if r.is_ai {
            s.push_str("you");
        } else {
            s.push_str(&r.author);
        }
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

async fn build_convo<S: Store>(
    manager: &mut Manager<S, Registered>,
    names: &Names,
    cfg: &Config,
    sender: &str,
    trigger: &Trigger,
    reply_to_ai: bool,
    history: &[HistMsg],
) -> (Vec<serde_json::Value>, usize) {
    let vision = cfg.vision;
    let processing_msg = cfg.processing_msg.as_str();
    let mut convo: Vec<serde_json::Value> = Vec::new();
    for h in history {
        if h.is_ai {
            convo.push(serde_json::json!({ "role": "assistant", "content": h.text.clone() }));
        } else {
            let content = format_user_turn(&h.speaker, h.reply_to.as_ref(), &h.text);
            convo.push(serde_json::json!({ "role": "user", "content": content }));
        }
    }

    let reply_to = if trigger.is_reply() {
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
            is_ai: reply_to_ai,
        })
    } else {
        None
    };
    // keep the @ai prefix so this turn matches the past trigger messages in
    // context rather than looking stripped
    let q = format_user_turn(sender, reply_to.as_ref(), trigger.body.trim());

    let mut imgs = Vec::new();
    if vision {
        imgs.extend(fetch_images(manager, &trigger.own_images).await);

        // resolved images from the message the trigger replies to (if any):
        // prefer the full-resolution original from the local store, fall back to
        // the quote's embedded thumbnail when that yields nothing
        if let Some(qts) = trigger.quoted_ts {
            let from_store = match manager.store().message(&trigger.thread, qts).await {
                Ok(Some(orig)) => content_images(&orig),
                _ => Vec::new(),
            };
            let mut reply_imgs = fetch_images(manager, &from_store).await;
            let used_thumb = reply_imgs.is_empty();
            if reply_imgs.is_empty() {
                reply_imgs = fetch_images(manager, &trigger.quoted_thumbnails).await;
            }
            info!(
                store_ptrs = from_store.len(),
                thumb_ptrs = trigger.quoted_thumbnails.len(),
                fetched = reply_imgs.len(),
                used_thumb,
                "resolved reply image"
            );
            imgs.extend(reply_imgs);
        } else if !trigger.quoted_thumbnails.is_empty() {
            imgs.extend(fetch_images(manager, &trigger.quoted_thumbnails).await);
        }

        // recall: when the trigger is a reply to one of our own answers, walk
        // the reply chain backwards to collect images from the originating
        // trigger(s). each bot answer quotes its trigger, so the chain links
        // answer -> trigger -> (earlier answer) -> ... bounded & cycle-guarded.
        if reply_to_ai {
            if let Some(qts) = trigger.quoted_ts {
                let recalled =
                    recall_chain_images(manager, names, &trigger.thread, qts, processing_msg).await;
                if !recalled.is_empty() {
                    info!(recalled = recalled.len(), "recalled chain images");
                    imgs.extend(recalled);
                }
            }
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

// walk the reply chain backwards from an AI answer to collect images from the
// originating trigger(s). each bot answer quotes its trigger (set in
// handle_trigger), and a trigger may itself reply to an earlier AI answer, so
// the chain links: answer -> trigger -> (earlier answer) -> ... bounded by the
// same 16-hop limit used elsewhere, and cycle-guarded with a visited set.
async fn recall_chain_images<S: Store>(
    manager: &mut Manager<S, Registered>,
    names: &Names,
    thread: &presage::store::Thread,
    start_ts: u64,
    processing_msg: &str,
) -> Vec<(String, String)> {
    use std::collections::HashSet;

    let mut imgs = Vec::new();
    let mut visited: HashSet<u64> = HashSet::new();
    let mut ts = start_ts;

    for _ in 0..16 {
        if !visited.insert(ts) {
            break;
        }
        // resolve to the placeholder root if this is an edit of one of our
        // answers; otherwise ts is a user message we inspect directly
        let root_ts =
            match crate::message::ai_root_ts(manager, names, thread, ts, processing_msg).await {
                Some(r) => r,
                None => ts,
            };
        if root_ts != ts {
            // ts was an edit; jump to the root and re-check visited
            ts = root_ts;
            continue;
        }
        let Ok(Some(content)) = manager.store().message(thread, ts).await else {
            break;
        };
        let is_mine = crate::message::from_me(&content, names.my_aci());
        let Some((_, dm)) = crate::message::data_message(&content) else {
            break;
        };
        if is_mine {
            // an AI answer (placeholder root): its quote.id points at the
            // trigger it answered — keep walking back
            match dm.quote.as_ref().and_then(|q| q.id) {
                Some(qts) => {
                    ts = qts;
                    continue;
                }
                None => break,
            }
        } else {
            // a user trigger: collect its own images
            let own = dm_images(dm);
            if !own.is_empty() {
                imgs.extend(fetch_images(manager, &own).await);
            }
            // also resolve images the trigger itself was replying to (its quote)
            if let Some(qts) = dm.quote.as_ref().and_then(|q| q.id) {
                let from_store = match manager.store().message(thread, qts).await {
                    Ok(Some(orig)) => content_images(&orig),
                    _ => Vec::new(),
                };
                let fetched = fetch_images(manager, &from_store).await;
                let fetched = if fetched.is_empty() {
                    let thumbs = dm
                        .quote
                        .as_ref()
                        .map(|q| {
                            q.attachments
                                .iter()
                                .filter_map(|a| a.thumbnail.clone())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    fetch_images(manager, &thumbs).await
                } else {
                    fetched
                };
                imgs.extend(fetched);
                ts = qts;
                continue;
            }
            break;
        }
    }
    imgs
}

// the originating trigger's identity, passed to handle_trigger so it can quote
// the trigger from the bot's answer (closing the reply chain for image recall)
struct TriggerRef {
    ts: u64,
    sender_aci: [u8; 16],
    body: String,
}

// post the placeholder, edit it through each phase as the completion streams,
// then edit it into the answer. returns every timestamp we wrote (placeholder,
// each phase edit, final) plus the answer. presage persists these edits as
// separate rows keyed by their own timestamp, so the caller records the answer
// under all of them: that way any row of the chain is recognised as ours rather
// than leaking back into context as an owner message.
async fn handle_trigger<S: Store>(
    manager: &mut Manager<S, Registered>,
    cfg: &Config,
    recipient: Recipient,
    trigger: &TriggerRef,
    convo: Vec<serde_json::Value>,
    chat_context: &str,
) -> anyhow::Result<()> {
    // post the placeholder, forced strictly after the trigger so it sorts after
    // the prompt regardless of clock skew
    let placeholder_ts = now_ts().max(trigger.ts + 1);
    // quote the trigger so a later reply to this answer can walk back to the
    // trigger's images (and any earlier image in the chain). without this link
    // the reply chain dead-ends at the bot's answer.
    let quote = Quote {
        id: Some(trigger.ts),
        author_aci_binary: Some(trigger.sender_aci.to_vec()),
        text: Some(trigger.body.clone()),
        ..Default::default()
    };
    let placeholder = DataMessage {
        body: Some(cfg.processing_msg.clone()),
        timestamp: Some(placeholder_ts),
        group_v2: recipient.group_context(),
        quote: Some(quote.clone()),
        ..Default::default()
    };
    send_to(manager, &recipient, placeholder.into(), placeholder_ts).await?;

    let (tx, mut rx) = mpsc::unbounded();
    let mut last_ts = placeholder_ts;
    let stream = cfg.ai.complete(convo, chat_context, move |p| {
        let _ = tx.unbounded_send(p);
    });
    let edits = async {
        let mut shown: Option<Phase> = None;
        while let Some(mut phase) = rx.next().await {
            // skip to the newest phase if more piled up
            while let Ok(p) = rx.try_recv() {
                phase = p;
            }
            if shown == Some(phase) {
                continue;
            }
            shown = Some(phase);
            let msg = match phase {
                Phase::Reasoning => &cfg.reasoning_msg,
                Phase::Generating => &cfg.generating_msg,
            };
            // each edit targets the previous revision, not the original, or the
            // client renders it as a separate message
            let edit_ts = now_ts().max(last_ts + 1);
            if let Err(e) = send_edit(
                manager,
                &recipient,
                last_ts,
                msg.clone(),
                edit_ts,
                Some(quote.clone()),
            )
            .await
            {
                error!(%e, "phase edit failed");
            }
            last_ts = edit_ts;
        }
    };
    let (result, ()) = join(stream, edits).await;

    let answer = match result {
        Ok(a) if !a.is_empty() => a,
        Ok(_) => "(empty response)".to_string(),
        Err(e) => {
            error!(%e, "ai request failed");
            format!("ai error: {e}")
        }
    };

    let edit_ts = now_ts().max(last_ts + 1);
    if let Err(e) = send_edit(manager, &recipient, last_ts, answer, edit_ts, Some(quote)).await {
        error!(%e, "edit failed");
    }

    Ok(())
}

async fn process_content<S: Store>(
    manager: &mut Manager<S, Registered>,
    cfg: &Config,
    names: &Names,
    content: &presage::libsignal_service::content::Content,
) {
    let Some(mut t) = extract(content) else {
        return;
    };
    // turn @-mention placeholders into "@[name]" so the model can read them
    t.body = resolve_mentions(manager, names, &t.body, &t.body_ranges).await;
    // replying to one of the bot's own messages summons it without the trigger
    let reply_to_ai = match t.quoted_ts {
        Some(ts) => is_ai_message(manager, names, &t.thread, ts, &cfg.processing_msg).await,
        None => false,
    };
    // the trigger can appear anywhere in the message
    let trimmed = t.body.trim();
    if !trimmed.contains(&cfg.trigger) && !reply_to_ai {
        return;
    }
    let is_reply = t.is_reply();
    // allow an image-only or reply-only prompt (e.g. a photo or a reply
    // captioned just "@ai")
    let only_trigger = trimmed.is_empty() || trimmed == cfg.trigger;
    if only_trigger && t.own_images.is_empty() && !is_reply {
        return;
    }
    let recipient = Recipient::from_thread(&t.thread);

    let sender = names.of(manager, &content.metadata.sender).await;

    let trigger_ts = content.timestamp();
    let trigger_sender_aci = *content.metadata.sender.raw_uuid().as_bytes();
    let history = thread_history(
        manager,
        &t.thread,
        trigger_ts,
        cfg.context_messages,
        &cfg.processing_msg,
        &[&cfg.processing_msg, &cfg.reasoning_msg, &cfg.generating_msg],
        names,
    )
    .await;

    let (convo, images) =
        build_convo(manager, names, cfg, &sender, &t, reply_to_ai, &history).await;
    let chat_context = names.chat_context(manager, &t.thread).await;

    info!(
        thread = ?t.thread,
        replying = is_reply,
        images,
        context = history.len(),
        "handling @ai prompt"
    );
    if let Err(e) = handle_trigger(
        manager,
        cfg,
        recipient,
        &TriggerRef {
            ts: trigger_ts,
            sender_aci: trigger_sender_aci,
            body: t.body.clone(),
        },
        convo,
        &chat_context,
    )
    .await
    {
        error!(%e, "failed to handle prompt");
    }
}

pub enum Outcome {
    Done,
    Relink,
}

pub async fn run_loop<S: Store>(
    mut manager: Manager<S, Registered>,
    cfg: &Config,
) -> anyhow::Result<Outcome> {
    let names = Names::resolve(&mut manager).await;

    let messages = match manager.receive_messages().await {
        Ok(m) => m,
        // presage signals this when our device has been unlinked
        Err(presage::Error::RelinkNecessary) => return Ok(Outcome::Relink),
        Err(e) => return Err(e).context("failed to start message stream"),
    };
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
                process_content(&mut manager, cfg, &names, &content).await;
            }
        }
    }
    Ok(Outcome::Done)
}
