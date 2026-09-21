use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use futures::{channel::mpsc, future::join, pin_mut, StreamExt};
use presage::libsignal_service::content::DataMessage;
use presage::libsignal_service::proto::data_message::Quote;
use presage::libsignal_service::proto::AttachmentPointer;
use presage::manager::Registered;
use presage::model::messages::Received;
use presage::store::{ContentExt, Store, Thread};
use presage::Manager;
use tracing::{debug, error, info};

use crate::ai::{Phase, ToolCall};
use crate::config::Config;
use crate::convo_cache::{Cached, ConvoCache};
use crate::history::{thread_history, HistMsg};
use crate::images::fetch_images;
use crate::media::{decode_data_uri, fetch_media, Media};
use crate::message::{
    ai_root_ts, content_attachments, extract, is_ai_message, requested_context, resolve_author,
    resolve_mentions, ReplyRef, Trigger,
};
use crate::names::Names;
use crate::prune::prune_old_messages;
use crate::recipient::{send_attachments, send_edit, send_to, upload, Recipient};
use crate::tools::{self, same_file, Inline};
use crate::transcript::{user_turn_value, Transcript};

// how many finished conversations to keep warm for continuation
const CONVO_CACHE_CAP: usize = 20;
// how often to sweep the store for messages past the retention window
const PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_AI_REQUESTS: usize = 4;
const EMPTY_ANSWER: &str = "(empty response)";
// stands in for a replied-to attachment the store no longer has
const MISSING_QUOTED: &str =
    "[the attachment on the message being replied to is no longer available]";

pub(crate) fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64
}

fn call_key(call: &ToolCall) -> (String, String) {
    (call.name.clone(), call.arguments.clone())
}

// `text` cut to `max` characters, for sending somewhere a full one won't fit
pub(crate) fn truncated(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

struct Ctx<'a> {
    names: &'a Names,
    cfg: &'a Config,
    thread: &'a Thread,
    transcript: &'a mut Transcript,
}

// the final `user` turn for the triggering message: its text plus its own
// attachments. the quoted message's media rides along too — a reply is the
// clearest signal that it matters — unless this conversation already carries it
// inline, in which case the annotation points back at the copy that's there.
async fn trigger_turn<S: Store>(
    manager: &mut Manager<S, Registered>,
    ctx: &mut Ctx<'_>,
    sender: &str,
    trigger: &Trigger,
    trigger_ts: u64,
    reply_to_ai: bool,
) -> (serde_json::Value, Media) {
    let cfg = ctx.cfg;
    let reply_to = if trigger.is_reply() {
        let author = resolve_author(
            manager,
            ctx.names,
            Some(&trigger.thread),
            trigger.quoted_author,
            trigger.quoted_ts,
        )
        .await;
        Some(ReplyRef {
            author,
            text: trigger.quoted_text.clone().unwrap_or_default(),
            ts: trigger.quoted_ts,
            is_ai: reply_to_ai,
        })
    } else {
        None
    };
    // keep the @ai prefix so this turn matches the past trigger messages in
    // context rather than looking stripped
    let mut q =
        ctx.transcript
            .user_text(trigger_ts, sender, reply_to.as_ref(), trigger.body.trim());
    let mut notes = vec![ctx
        .transcript
        .announce(cfg, &trigger.own_atts, Inline::Shown)];

    let mut media = fetch_media(manager, cfg, &trigger.own_atts).await;
    if !media.is_empty() {
        ctx.transcript.mark_inlined(trigger_ts);
    }

    // prefer the full original from the local store, falling back to the quote's
    // embedded thumbnail when that yields nothing. a quote only ever embeds
    // still-image thumbnails, so audio has no such fallback.
    match trigger.quoted_ts {
        Some(qts) if ctx.transcript.is_inlined(qts) => {
            let orig = stored_attachments(manager, &trigger.thread, qts).await;
            notes.push(ctx.transcript.announce(cfg, &orig, Inline::Shown));
            debug!(
                quoted_ts = qts,
                "quoted media already in context, not resending"
            );
        }
        Some(qts) => {
            let ptrs = stored_attachments(manager, &trigger.thread, qts).await;
            notes.push(ctx.transcript.announce(cfg, &ptrs, Inline::Shown));
            // a file attached to both messages is already encoded in this turn
            let fresh: Vec<_> = ptrs
                .iter()
                .filter(|p| !trigger.own_atts.iter().any(|o| same_file(o, p)))
                .cloned()
                .collect();
            let overlaps = fresh.len() < ptrs.len();
            let mut quoted = fetch_media(manager, cfg, &fresh).await;
            let used_thumb = cfg.vision && quoted.images.is_empty() && !overlaps;
            if used_thumb {
                quoted.images = fetch_images(manager, &trigger.quoted_thumbnails).await;
            }
            info!(
                quoted_ts = qts,
                store_atts = ptrs.len(),
                thumb_ptrs = trigger.quoted_thumbnails.len(),
                images = quoted.images.len(),
                audio = quoted.audio.len(),
                files = quoted.files.len(),
                used_thumb,
                "resolved reply media"
            );
            // a quote embeds a thumbnail per attachment, so it shows one was
            // there even when the message itself is gone from the store
            let had_attachment = !ptrs.is_empty() || !trigger.quoted_thumbnails.is_empty();
            if quoted.is_empty() && had_attachment && !overlaps {
                notes.push(MISSING_QUOTED.to_string());
            }
            if !quoted.is_empty() {
                ctx.transcript.mark_inlined(qts);
            }
            media.images.extend(quoted.images);
            media.audio.extend(quoted.audio);
            media.files.extend(quoted.files);
        }
        None if cfg.vision && !trigger.quoted_thumbnails.is_empty() => {
            media
                .images
                .extend(fetch_images(manager, &trigger.quoted_thumbnails).await);
        }
        _ => {}
    }

    for note in notes.iter().filter(|n| !n.is_empty()) {
        q.push('\n');
        q.push_str(note);
    }

    (user_turn_value(q, &media), media)
}

async fn stored_attachments<S: Store>(
    manager: &Manager<S, Registered>,
    thread: &Thread,
    ts: u64,
) -> Vec<AttachmentPointer> {
    manager
        .store()
        .message(thread, ts)
        .await
        .ok()
        .flatten()
        .as_ref()
        .map(content_attachments)
        .unwrap_or_default()
}

// a fresh conversation: the last-N window turned into turns, then the trigger
async fn build_convo<S: Store>(
    manager: &mut Manager<S, Registered>,
    ctx: &mut Ctx<'_>,
    sender: &str,
    trigger: &Trigger,
    trigger_ts: u64,
    reply_to_ai: bool,
    history: &[HistMsg],
) -> (Vec<serde_json::Value>, Media) {
    let mut convo = ctx.transcript.history_turns(ctx.cfg, history);
    let (turn, media) = trigger_turn(manager, ctx, sender, trigger, trigger_ts, reply_to_ai).await;
    convo.push(turn);
    (convo, media)
}

// the originating trigger's identity, passed to handle_trigger so it can quote
// the trigger from the bot's answer (closing the reply chain for image recall)
struct TriggerRef {
    ts: u64,
    sender_aci: [u8; 16],
    body: String,
}

// post the placeholder, edit it through each phase as the completion streams,
// then edit it into the answer. returns (answer root ts, answer tip ts, the
// generated-image message's ts if one was sent, answer text, the convo it was
// given), so the caller can cache the finished conversation for continuation.
async fn handle_trigger<S: Store>(
    manager: &mut Manager<S, Registered>,
    ctx: &Ctx<'_>,
    recipient: Recipient,
    trigger: &TriggerRef,
    mut convo: Vec<serde_json::Value>,
    chat_context: &str,
) -> anyhow::Result<(u64, u64, Option<u64>, String, Vec<serde_json::Value>)> {
    let cfg = ctx.cfg;
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

    let mut last_ts = placeholder_ts;
    let mut shown: Option<Phase> = None;
    let mut answer = String::new();
    let mut generated: Vec<String> = Vec::new();
    // tool calls already made and answered; repeating one means the model is
    // going in circles rather than making progress
    let mut spent: HashSet<(String, String)> = HashSet::new();
    let mut needs_conclusion = false;

    for round in 0..MAX_AI_REQUESTS {
        let (tx, mut rx) = mpsc::unbounded();
        let stream = cfg.ai.complete(&convo, chat_context, move |p| {
            let _ = tx.unbounded_send(p);
        });
        let edits = async {
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
                    Phase::Tool => &cfg.tool_msg,
                };
                // each edit targets the previous revision, not the original, or
                // the client renders it as a separate message
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

        let turn = match result {
            Ok(turn) => turn,
            Err(e) => {
                error!(%e, "ai request failed");
                answer = format!("ai error: {}", truncated(&e.to_string(), 200));
                break;
            }
        };
        generated.extend(turn.images);

        if turn.calls.is_empty() {
            answer = turn.text;
            break;
        }
        // every call is recorded before the verdict; a short-circuiting
        // iterator method would leave later ones out of `spent`
        let mut repeating = true;
        for call in &turn.calls {
            repeating &= !spent.insert(call_key(call));
        }
        if repeating || round + 1 == MAX_AI_REQUESTS {
            needs_conclusion = true;
            break;
        }
        convo.push(turn.assistant);
        // media can't ride in a tool result, so it follows as user turns once
        // every call has been answered
        let mut follow_ups = Vec::new();
        for call in &turn.calls {
            let result = tools::dispatch(
                manager,
                cfg,
                ctx.names,
                ctx.thread,
                ctx.transcript.catalog(),
                call,
            )
            .await;
            convo.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": result.content.clone(),
            }));
            if let Some(media) = result.media {
                follow_ups.push(user_turn_value(result.content, &media));
            }
        }
        convo.extend(follow_ups);
    }

    // out of rounds, or repeating itself: ask once more with no tools, so the
    // reply is an answer rather than a report of our own limit
    if needs_conclusion {
        match cfg.ai.conclude(&convo, chat_context).await {
            Ok(text) if !text.trim().is_empty() => answer = text,
            Ok(_) => {}
            Err(e) => error!(%e, "concluding without tools failed"),
        }
    }

    // a provider-run image tool ends the turn with no words of its own
    if !generated.is_empty() && answer.trim().is_empty() {
        match cfg.ai.caption(&convo, chat_context, &generated).await {
            Ok(line) if !line.trim().is_empty() => answer = line,
            Ok(_) => {}
            Err(e) => error!(%e, "captioning generated images failed"),
        }
    }
    let mut attachments = Vec::new();
    if !generated.is_empty() {
        info!(count = generated.len(), "posting generated images");
    }
    for uri in &generated {
        let Some((mime, bytes)) = decode_data_uri(uri) else {
            error!("generated image was not a readable data uri");
            continue;
        };
        match upload(manager, mime, bytes).await {
            Ok(pointer) => attachments.push(pointer),
            Err(e) => error!(%e, "uploading generated image failed"),
        }
    }

    // only now is it certain nothing at all is going to be sent
    if answer.trim().is_empty() && attachments.is_empty() {
        answer = EMPTY_ANSWER.to_string();
    }

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

    // an edit can't carry attachments — signal clients drop them — so generated
    // images follow as their own message
    let mut image_ts = None;
    if !attachments.is_empty() {
        let ts = now_ts().max(edit_ts + 1);
        match send_attachments(manager, &recipient, attachments, ts).await {
            Ok(()) => image_ts = Some(ts),
            Err(e) => error!(%e, "sending generated images failed"),
        }
    }

    Ok((placeholder_ts, edit_ts, image_ts, answer, convo))
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
    // allow a media-only or reply-only prompt (e.g. a photo or a reply
    // captioned just "@ai")
    let only_trigger =
        trimmed.is_empty() || trimmed.trim_end_matches(|c: char| c.is_ascii_digit()) == cfg.trigger;
    if only_trigger && t.own_atts.is_empty() && !is_reply {
        return;
    }
    // `@ai2` asks for a shorter window than the configured one; it can only
    // narrow it
    let context_messages = requested_context(trimmed, &cfg.trigger)
        .unwrap_or(cfg.context_messages)
        .min(cfg.context_messages);
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
    let (cached, mut transcript) = match parent_root.and_then(|r| cache.get(r)) {
        Some(c) => (
            Some((c.convo.clone(), c.chat_context.clone(), c.tip_ts)),
            c.transcript.clone(),
        ),
        None => (None, Transcript::default()),
    };
    let mut ctx = Ctx {
        names,
        cfg,
        thread: &t.thread,
        transcript: &mut transcript,
    };

    let (convo, media, chat_context, context) = match cached {
        Some((cached_convo, cached_context, tip_ts)) => {
            // reuse the cached array (its media included), append whatever was
            // written since (with theirs), then this turn. the reused prefix
            // stays byte-identical so llama-server keeps its prompt cache.
            let between =
                thread_history(manager, &t.thread, tip_ts, trigger_ts, None, cfg, names).await;
            let mut convo = cached_convo;
            let turns = ctx.transcript.history_turns(cfg, &between);
            convo.extend(turns);
            let (turn, media) =
                trigger_turn(manager, &mut ctx, &sender, &t, trigger_ts, reply_to_ai).await;
            convo.push(turn);
            (convo, media, cached_context, between.len())
        }
        None => {
            // fresh @ai, or a miss (restart / evicted / older answer): build from
            // the last-N window
            let history = thread_history(
                manager,
                &t.thread,
                0,
                trigger_ts,
                Some(context_messages),
                cfg,
                names,
            )
            .await;
            let (convo, media) = build_convo(
                manager,
                &mut ctx,
                &sender,
                &t,
                trigger_ts,
                reply_to_ai,
                &history,
            )
            .await;
            let chat_context = names.chat_context(manager, &t.thread).await;
            (convo, media, chat_context, history.len())
        }
    };

    info!(
        thread = ?t.thread,
        replying = is_reply,
        continued = parent_root.is_some(),
        images = media.images.len(),
        audio = media.audio.len(),
        context,
        "handling @ai prompt"
    );
    match handle_trigger(
        manager,
        &ctx,
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
        Ok((root_ts, tip_ts, image_ts, answer, mut convo)) => {
            // don't cache error/empty sentinels, so a retry rebuilds cleanly
            let is_error = answer == EMPTY_ANSWER || answer.starts_with("ai error:");
            if !is_error {
                convo.push(serde_json::json!({ "role": "assistant", "content": answer }));
                cache.insert(
                    root_ts,
                    Cached {
                        convo,
                        chat_context,
                        transcript,
                        tip_ts,
                    },
                );
                // a generated image is its own message, outside the edit chain
                // `ai_root_ts` walks, so point it at the answer explicitly
                if let Some(ts) = image_ts {
                    cache.alias(ts, root_ts);
                }
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
                    Received::DecryptionError(sender) => {
                        info!("dropping undecryptable message from {sender:?}");
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
