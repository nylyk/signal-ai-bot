use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
use crate::convo_cache::{Cached, ConvoCache};
use crate::history::{thread_history, HistMsg};
use crate::images::fetch_images;
use crate::message::{
    ai_root_ts, content_images, extract, is_ai_message, resolve_author, resolve_mentions, ReplyRef,
    Trigger,
};
use crate::names::Names;
use crate::prune::prune_old_messages;
use crate::recipient::{send_edit, send_to, Recipient};

// how many finished conversations to keep warm for continuation
const CONVO_CACHE_CAP: usize = 64;
// how often to sweep the store for messages past the retention window
const PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

pub(crate) fn now_ts() -> u64 {
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

// a `user` turn: plain string content when there are no images, else a
// multimodal parts array with the text followed by each image as an image_url.
fn user_turn_value(text: String, imgs: &[(String, String)]) -> serde_json::Value {
    if imgs.is_empty() {
        return serde_json::json!({ "role": "user", "content": text });
    }
    let mut parts = vec![serde_json::json!({ "type": "text", "text": text })];
    for (mime, data) in imgs {
        parts.push(serde_json::json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{mime};base64,{data}") }
        }));
    }
    serde_json::json!({ "role": "user", "content": parts })
}

// turn a window of history into chat messages. the bot's own answers become
// `assistant` turns; everyone else's become `user` turns carrying that message's
// own images (fetched only when vision is on).
async fn history_turns<S: Store>(
    manager: &mut Manager<S, Registered>,
    cfg: &Config,
    history: &[HistMsg],
) -> Vec<serde_json::Value> {
    let mut turns = Vec::with_capacity(history.len());
    for h in history {
        if h.is_ai {
            turns.push(serde_json::json!({ "role": "assistant", "content": h.text.clone() }));
            continue;
        }
        let imgs = if cfg.vision {
            fetch_images(manager, &h.images).await
        } else {
            Vec::new()
        };
        let text = format_user_turn(&h.speaker, h.reply_to.as_ref(), &h.text);
        turns.push(user_turn_value(text, &imgs));
    }
    turns
}

// the final `user` turn for the triggering message: its text plus its own
// images. `window` is the set of turns already in the array; a directly-quoted
// image is added only when the quoted message isn't already one of them (so an
// in-window image isn't sent twice), covering replies to a photo from before the
// window.
async fn trigger_turn<S: Store>(
    manager: &mut Manager<S, Registered>,
    names: &Names,
    cfg: &Config,
    sender: &str,
    trigger: &Trigger,
    reply_to_ai: bool,
    window: &[HistMsg],
) -> (serde_json::Value, usize) {
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
    if cfg.vision {
        imgs.extend(fetch_images(manager, &trigger.own_images).await);

        // a directly-quoted image the model can't already see in this window:
        // prefer the full-resolution original from the local store, fall back to
        // the quote's embedded thumbnail when that yields nothing
        if let Some(qts) = trigger.quoted_ts {
            if !window.iter().any(|h| h.ts == qts) {
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
            }
        } else if !trigger.quoted_thumbnails.is_empty() {
            imgs.extend(fetch_images(manager, &trigger.quoted_thumbnails).await);
        }
    }

    (user_turn_value(q, &imgs), imgs.len())
}

// a fresh conversation: the last-N window turned into turns, then the trigger
async fn build_convo<S: Store>(
    manager: &mut Manager<S, Registered>,
    names: &Names,
    cfg: &Config,
    sender: &str,
    trigger: &Trigger,
    reply_to_ai: bool,
    history: &[HistMsg],
) -> (Vec<serde_json::Value>, usize) {
    let mut convo = history_turns(manager, cfg, history).await;
    let (turn, images) =
        trigger_turn(manager, names, cfg, sender, trigger, reply_to_ai, history).await;
    convo.push(turn);
    (convo, images)
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
// returns (answer root ts, answer tip ts, answer text, the convo it was given),
// so the caller can cache the finished conversation for continuation.
async fn handle_trigger<S: Store>(
    manager: &mut Manager<S, Registered>,
    cfg: &Config,
    recipient: Recipient,
    trigger: &TriggerRef,
    convo: Vec<serde_json::Value>,
    chat_context: &str,
) -> anyhow::Result<(u64, u64, String, Vec<serde_json::Value>)> {
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
    let stream = cfg.ai.complete(&convo, chat_context, move |p| {
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
    if let Err(e) = send_edit(
        manager,
        &recipient,
        last_ts,
        answer.clone(),
        edit_ts,
        Some(quote),
    )
    .await
    {
        error!(%e, "edit failed");
    }

    Ok((placeholder_ts, edit_ts, answer, convo))
}

async fn process_content<S: Store>(
    manager: &mut Manager<S, Registered>,
    cfg: &Config,
    names: &Names,
    cache: &mut ConvoCache,
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

    // if this reply continues one of our answers, which answer's root does it
    // resolve to? that's the cache key.
    let parent_root = match (reply_to_ai, t.quoted_ts) {
        (true, Some(qts)) => ai_root_ts(manager, names, &t.thread, qts, &cfg.processing_msg).await,
        _ => None,
    };
    // clone eagerly so the cache borrow ends before we build the new turn
    let hit = parent_root
        .and_then(|r| cache.get(r))
        .map(|c| (c.convo.clone(), c.chat_context.clone(), c.tip_ts));

    let (convo, images, chat_context, context) = match hit {
        Some((cached_convo, cached_context, tip_ts)) => {
            // reuse the cached array (its images included), append whatever was
            // written since (with their images), then this turn. the reused
            // prefix stays byte-identical so llama-server keeps its prompt cache.
            let between =
                thread_history(manager, &t.thread, tip_ts, trigger_ts, None, cfg, names).await;
            let mut convo = cached_convo;
            convo.extend(history_turns(manager, cfg, &between).await);
            let (turn, images) =
                trigger_turn(manager, names, cfg, &sender, &t, reply_to_ai, &between).await;
            convo.push(turn);
            (convo, images, cached_context, between.len())
        }
        None => {
            // fresh @ai, or a miss (restart / evicted / older answer): build from
            // the last-N window
            let history = thread_history(
                manager,
                &t.thread,
                0,
                trigger_ts,
                Some(cfg.context_messages),
                cfg,
                names,
            )
            .await;
            let (convo, images) =
                build_convo(manager, names, cfg, &sender, &t, reply_to_ai, &history).await;
            let chat_context = names.chat_context(manager, &t.thread).await;
            (convo, images, chat_context, history.len())
        }
    };

    info!(
        thread = ?t.thread,
        replying = is_reply,
        continued = parent_root.is_some(),
        images,
        context,
        "handling @ai prompt"
    );
    match handle_trigger(
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
        Ok((root_ts, tip_ts, answer, mut convo)) => {
            // don't cache error/empty sentinels, so a retry rebuilds cleanly
            let is_error = answer == "(empty response)" || answer.starts_with("ai error:");
            if !is_error {
                convo.push(serde_json::json!({ "role": "assistant", "content": answer }));
                cache.insert(
                    root_ts,
                    Cached {
                        convo,
                        chat_context,
                        tip_ts,
                    },
                );
                // the answer we continued from is now superseded by this child
                if let Some(p) = parent_root {
                    cache.remove(p);
                }
            }
        }
        Err(e) => error!(%e, "failed to handle prompt"),
    }
}

pub enum Outcome {
    Done,
    Relink,
}

pub async fn run_loop<S: Store + Clone>(
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
    // finished conversations kept warm so a reply to one continues its exact
    // messages array instead of rebuilding from the store
    let mut cache = ConvoCache::new(CONVO_CACHE_CAP);

    info!("running. send `{}  <prompt>` in any chat", cfg.trigger);

    let mut prune_tick = tokio::time::interval(PRUNE_INTERVAL);

    loop {
        tokio::select! {
            received = messages.next() => {
                let Some(received) = received else { break };
                match received {
                    Received::QueueEmpty => live = true,
                    Received::Contacts => {}
                    Received::Content(content) => {
                        if !live {
                            continue;
                        }
                        process_content(&mut manager, cfg, &names, &mut cache, &content).await;
                    }
                }
            }
            // don't prune before the initial sync drains
            _ = prune_tick.tick(), if live => {
                if let Some(retention) = cfg.retention {
                    prune_old_messages(&manager, retention).await;
                }
            }
        }
    }
    Ok(Outcome::Done)
}
