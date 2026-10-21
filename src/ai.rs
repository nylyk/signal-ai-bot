use futures::StreamExt;
use tracing::{debug, warn};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Reasoning,
    Generating,
    Tool,
}

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    pub fn arg(&self, name: &str) -> Option<serde_json::Value> {
        let mut args: serde_json::Value = serde_json::from_str(&self.arguments).ok()?;
        Some(args[name].take())
    }
}

// what one streamed turn produced. `assistant` is the turn to append before
// asking again.
pub struct Completion {
    pub assistant: serde_json::Value,
    pub text: String,
    pub generated: Generated,
    pub calls: Vec<ToolCall>,
}

// `Generated` holds the data uris a provider-run image or video tool returned
// beside the answer
#[derive(Default, Clone)]
pub struct Generated {
    pub images: Vec<String>,
    pub videos: Vec<String>,
}

impl Generated {
    pub fn is_empty(&self) -> bool {
        self.images.is_empty() && self.videos.is_empty()
    }

    pub fn extend(&mut self, other: Generated) {
        self.images.extend(other.images);
        self.videos.extend(other.videos);
    }

    pub fn uris(&self) -> impl Iterator<Item = &String> {
        self.images.iter().chain(&self.videos)
    }

    // echoed back in the shape they arrived in, which is the only one the api
    // accepts on an assistant turn. an edit relies on the data being unchanged.
    pub fn echo_onto(&self, assistant: &mut serde_json::Value) {
        if !self.images.is_empty() {
            assistant["images"] = media_parts(&self.images, "image_url");
        }
        if !self.videos.is_empty() {
            assistant["videos"] = media_parts(&self.videos, "video_url");
        }
    }
}

// the api rejects a body over 64 MB; this leaves headroom below that
const MAX_BODY_BYTES: usize = 48 * 1024 * 1024;
const MEDIA_KEYS: [&str; 2] = ["images", "videos"];
const MEDIA_PARTS: [&str; 4] = ["image_url", "video_url", "input_audio", "file"];
const SHED_NOTE: &str =
    "[the attachment shown here was dropped to keep the request small; load_attachment brings it back]";

pub struct AiClient {
    http: reqwest::Client,
    base: String,
    key: String,
    model: String,
    system: Option<String>,
    reasoning_budget: i64,
    tools: Vec<serde_json::Value>,
}

impl AiClient {
    pub fn new(
        base: String,
        key: String,
        model: String,
        system: Option<String>,
        reasoning_budget: i64,
        tools: Vec<serde_json::Value>,
    ) -> Self {
        AiClient {
            http: reqwest::Client::new(),
            base,
            key,
            model,
            system,
            reasoning_budget,
            tools,
        }
    }

    fn with_system(
        &self,
        convo: &[serde_json::Value],
        chat_context: &str,
    ) -> Vec<serde_json::Value> {
        let system = match &self.system {
            Some(s) if !chat_context.is_empty() => format!("{s}\n\n{chat_context}"),
            Some(s) => s.clone(),
            None => chat_context.to_string(),
        };
        let system = match system.is_empty() {
            true => now_line(),
            false => format!("{system}\n\n{}", now_line()),
        };
        let mut messages = vec![serde_json::json!({ "role": "system", "content": system })];
        messages.extend(convo.iter().cloned());
        messages
    }

    // stream one turn, calling `on_phase` at each phase transition. only the
    // final answer is returned; tokens are never surfaced.
    pub async fn complete(
        &self,
        convo: &[serde_json::Value],
        chat_context: &str,
        on_phase: impl FnMut(Phase),
    ) -> anyhow::Result<Completion> {
        let messages = self.with_system(convo, chat_context);
        self.stream_turn(messages, &self.tools, on_phase).await
    }

    // one line to send with media the model made but said nothing about
    pub async fn caption(
        &self,
        convo: &[serde_json::Value],
        chat_context: &str,
        generated: &Generated,
    ) -> anyhow::Result<String> {
        let mut made = serde_json::json!({ "role": "assistant", "content": "" });
        generated.echo_onto(&mut made);
        self.ask_without_tools(
            convo,
            chat_context,
            vec![made],
            "Write one short line to send along with the image or video you just made. \
             Text only — do not make another.",
        )
        .await
    }

    // a plain answer with no tools on offer, for when the model has stopped
    // making progress with them
    pub async fn conclude(
        &self,
        convo: &[serde_json::Value],
        chat_context: &str,
    ) -> anyhow::Result<String> {
        self.ask_without_tools(
            convo,
            chat_context,
            Vec::new(),
            "Answer now, with what you already have. Do not call any tools.",
        )
        .await
    }

    async fn ask_without_tools(
        &self,
        convo: &[serde_json::Value],
        chat_context: &str,
        extra: Vec<serde_json::Value>,
        instruction: &str,
    ) -> anyhow::Result<String> {
        let mut messages = self.with_system(convo, chat_context);
        messages.extend(extra);
        // some providers reject a request ending on an assistant turn
        messages.push(serde_json::json!({ "role": "user", "content": instruction }));
        Ok(self.stream_turn(messages, &[], |_| {}).await?.text)
    }

    async fn stream_turn(
        &self,
        messages: Vec<serde_json::Value>,
        tools: &[serde_json::Value],
        mut on_phase: impl FnMut(Phase),
    ) -> anyhow::Result<Completion> {
        let url = format!("{}/chat/completions", self.base.trim_end_matches('/'));
        let thinking = self.reasoning_budget != 0;
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "chat_template_kwargs": { "enable_thinking": thinking },
        });
        // a positive budget caps thinking to that many tokens (llama.cpp), which
        // needs reasoning control on; negative means unlimited, 0 is off (above)
        if self.reasoning_budget > 0 {
            body["reasoning_control"] = serde_json::json!(true);
            body["reasoning_format"] = serde_json::json!("auto");
            body["thinking_budget_tokens"] = serde_json::json!(self.reasoning_budget);
        }
        // an empty array is rejected by some servers, so only send one with tools
        if !tools.is_empty() {
            body["tools"] = serde_json::json!(tools);
        }

        let mut payload = serde_json::to_vec(&body)?;
        if payload.len() > MAX_BODY_BYTES {
            warn!(
                bytes = payload.len(),
                "request too large, dropping the oldest media"
            );
            if let Some(messages) = body["messages"].as_array_mut() {
                shed_oldest_media(messages, payload.len(), MAX_BODY_BYTES);
            }
            payload = serde_json::to_vec(&body)?;
        }

        debug!(messages = %redact_media(&messages), "sending request to ai");

        // reqwest prints the endpoint in its errors, which end up in the chat
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            anyhow::bail!("{status}: {}", detail.trim());
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut content = String::new();
        let mut saw_reasoning = false;
        let mut phase: Option<Phase> = None;
        let mut calls: Vec<PartialCall> = Vec::new();
        let mut generated = Generated::default();

        'outer: while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.map_err(reqwest::Error::without_url)?);
            // server-sent events: one `data: <json>` per line
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&line);
                let Some(data) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    break 'outer;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                // a provider-run tool that fails reports it in-stream, leaving
                // the rest of the turn intact
                if let Some(message) = v["error"]["message"].as_str() {
                    warn!(message, "provider reported an error mid-stream");
                }
                let delta = &v["choices"][0]["delta"];
                generated
                    .images
                    .extend(media_urls(&delta["images"], "image_url"));
                generated
                    .videos
                    .extend(media_urls(&delta["videos"], "video_url"));
                if let Some(fragments) = delta["tool_calls"].as_array() {
                    for f in fragments {
                        accumulate_call(&mut calls, f);
                    }
                    if phase != Some(Phase::Tool) {
                        phase = Some(Phase::Tool);
                        on_phase(Phase::Tool);
                    }
                }
                // llama.cpp names it `reasoning_content`, openrouter `reasoning`
                if ["reasoning_content", "reasoning"]
                    .iter()
                    .any(|k| delta[k].as_str().is_some_and(|s| !s.is_empty()))
                {
                    saw_reasoning = true;
                }
                if let Some(c) = delta["content"].as_str() {
                    content.push_str(c);
                }
                if let Some(want) = next_phase(saw_reasoning, &content) {
                    if phase != Some(want) {
                        phase = Some(want);
                        on_phase(want);
                    }
                }
            }
        }

        let text = stitched_answer(&content);
        let mut assistant = serde_json::json!({ "role": "assistant", "content": text });
        if !calls.is_empty() {
            assistant["tool_calls"] =
                serde_json::json!(calls.iter().map(PartialCall::to_value).collect::<Vec<_>>());
        }
        generated.echo_onto(&mut assistant);
        Ok(Completion {
            assistant,
            text,
            generated,
            calls: calls.into_iter().map(PartialCall::into_call).collect(),
        })
    }
}

fn media_urls<'a>(
    parts: &'a serde_json::Value,
    kind: &'a str,
) -> impl Iterator<Item = String> + 'a {
    parts
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(move |p| p[kind]["url"].as_str())
        .map(str::to_string)
}

fn media_parts(urls: &[String], kind: &str) -> serde_json::Value {
    urls.iter()
        .map(|url| serde_json::json!({ "type": kind, kind: { "url": url } }))
        .collect()
}

// a tool call being assembled: the arguments arrive as string fragments spread
// over several deltas, keyed by their position in the call array
struct PartialCall {
    index: u64,
    id: String,
    name: String,
    arguments: String,
}

impl PartialCall {
    fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "type": "function",
            "function": { "name": self.name, "arguments": self.arguments },
        })
    }

    fn into_call(self) -> ToolCall {
        ToolCall {
            id: self.id,
            name: self.name,
            arguments: self.arguments,
        }
    }
}

fn accumulate_call(calls: &mut Vec<PartialCall>, fragment: &serde_json::Value) {
    let index = fragment["index"].as_u64().unwrap_or(0);
    let slot = match calls.iter().position(|c| c.index == index) {
        Some(i) => &mut calls[i],
        None => {
            calls.push(PartialCall {
                index,
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
            calls.last_mut().expect("just pushed")
        }
    };
    if let Some(id) = fragment["id"].as_str() {
        slot.id.push_str(id);
    }
    if let Some(name) = fragment["function"]["name"].as_str() {
        slot.name.push_str(name);
    }
    if let Some(args) = fragment["function"]["arguments"].as_str() {
        slot.arguments.push_str(args);
    }
}

fn now_line() -> String {
    format!(
        "The current date and time is {}.",
        chrono::Local::now().format("%A %-d %B %Y, %H:%M %Z")
    )
}

fn is_media_part(part: &serde_json::Value) -> bool {
    part["type"]
        .as_str()
        .is_some_and(|kind| MEDIA_PARTS.contains(&kind))
}

fn json_len(value: &serde_json::Value) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

// drops media from the oldest messages first until a body of `size` bytes fits
// under `limit`, putting a note where a user turn's attachment was
fn shed_oldest_media(messages: &mut [serde_json::Value], mut size: usize, limit: usize) {
    for m in messages {
        for key in MEDIA_KEYS {
            if size <= limit {
                return;
            }
            if let Some(media) = m.as_object_mut().and_then(|o| o.remove(key)) {
                size = size.saturating_sub(json_len(&media));
            }
        }
        let Some(parts) = m.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for part in parts.iter_mut().filter(|p| is_media_part(p)) {
            if size <= limit {
                return;
            }
            let dropped = std::mem::replace(
                part,
                serde_json::json!({ "type": "text", "text": SHED_NOTE }),
            );
            size = (size + json_len(part)).saturating_sub(json_len(&dropped));
        }
    }
}

// render the messages array as compact json for logging, replacing each media
// part with a short placeholder so base64 payloads don't flood logs
fn redact_media(messages: &[serde_json::Value]) -> String {
    let mut msgs = messages.to_vec();
    for m in &mut msgs {
        for key in MEDIA_KEYS {
            if let Some(media) = m.get_mut(key) {
                *media = serde_json::json!(format!("<{key} omitted>"));
            }
        }
        let Some(parts) = m.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for part in parts {
            let Some(kind) = part["type"].as_str() else {
                continue;
            };
            if MEDIA_PARTS.contains(&kind) {
                *part = serde_json::json!({ "type": kind, kind: "<omitted>" });
            }
        }
    }
    serde_json::to_string(&msgs).unwrap_or_else(|_| "<unserializable>".to_string())
}

// the answer with reasoning removed. the prompt opens the first <think> for the
// model, so content can start inside reasoning with only a closing </think>;
// drop that leading block, then strip any further explicit <think>…</think>
// blocks (the model may re-enter reasoning between answer segments).
fn stitched_answer(content: &str) -> String {
    let mut rest = content;
    if let Some(close) = rest.find("</think>") {
        if rest.find("<think>").is_none_or(|open| close < open) {
            rest = &rest[close + "</think>".len()..];
        }
    }
    let mut out = String::new();
    loop {
        let Some(open) = rest.find("<think>") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open]);
        let after = &rest[open + "<think>".len()..];
        match after.find("</think>") {
            Some(close) => rest = &after[close + "</think>".len()..],
            None => break,
        }
    }
    out.trim().to_string()
}

// the phase implied by what's arrived so far: visible text means writing,
// reasoning before that means thinking, nothing yet means neither.
fn next_phase(saw_reasoning: bool, content: &str) -> Option<Phase> {
    match visible_answer(content) {
        Some(s) if !s.trim().is_empty() => Some(Phase::Generating),
        _ if saw_reasoning || content.contains("<think>") => Some(Phase::Reasoning),
        _ => None,
    }
}

// the user-visible answer within content, hiding an in-progress <think> block
fn visible_answer(content: &str) -> Option<&str> {
    if let Some(idx) = content.rfind("</think>") {
        Some(content[idx + "</think>".len()..].trim_start())
    } else if content.contains("<think>") {
        None
    } else {
        Some(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_a_call_from_fragments() {
        let mut calls = Vec::new();
        accumulate_call(
            &mut calls,
            &serde_json::json!({
                "index": 0, "id": "call_1", "function": { "name": "load_attachment", "arguments": "" }
            }),
        );
        accumulate_call(
            &mut calls,
            &serde_json::json!({ "index": 0, "function": { "arguments": "{\"id\":" } }),
        );
        accumulate_call(
            &mut calls,
            &serde_json::json!({ "index": 0, "function": { "arguments": "3}" } }),
        );

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "load_attachment");
        assert_eq!(calls[0].arguments, "{\"id\":3}");
    }

    #[test]
    fn keeps_parallel_calls_apart() {
        let mut calls = Vec::new();
        for f in [
            serde_json::json!({ "index": 1, "id": "b", "function": { "name": "two", "arguments": "{}" } }),
            serde_json::json!({ "index": 0, "id": "a", "function": { "name": "one", "arguments": "{}" } }),
            serde_json::json!({ "index": 1, "function": { "arguments": "" } }),
        ] {
            accumulate_call(&mut calls, &f);
        }
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "two");
        assert_eq!(calls[1].name, "one");
    }

    #[test]
    fn echoes_generated_videos_in_the_shape_they_arrived_in() {
        let delta = serde_json::json!({
            "videos": [{ "type": "video_url", "index": 0, "video_url": { "url": "data:video/mp4;base64,AAAA" } }]
        });
        let generated = Generated {
            images: Vec::new(),
            videos: media_urls(&delta["videos"], "video_url").collect(),
        };
        let mut assistant = serde_json::json!({ "role": "assistant", "content": "" });
        generated.echo_onto(&mut assistant);
        assert_eq!(
            assistant["videos"],
            serde_json::json!([{ "type": "video_url", "video_url": { "url": "data:video/mp4;base64,AAAA" } }])
        );
        assert!(assistant.get("images").is_none());
    }

    #[test]
    fn sheds_the_oldest_media_first_until_the_body_fits() {
        let video = |url: &str| serde_json::json!({ "type": "video_url", "video_url": { "url": url.repeat(1000) } });
        let mut messages = vec![
            serde_json::json!({ "role": "user", "content": [{ "type": "text", "text": "a" }, video("1")] }),
            serde_json::json!({ "role": "assistant", "content": "made", "videos": [video("2")] }),
            serde_json::json!({ "role": "user", "content": [{ "type": "text", "text": "b" }, video("3")] }),
        ];
        let size = serde_json::to_vec(&messages).unwrap().len();
        let limit = size - 1500;
        shed_oldest_media(&mut messages, size, limit);
        assert_eq!(messages[0]["content"][1]["text"], SHED_NOTE);
        assert!(messages[1].get("videos").is_none());
        assert_eq!(messages[2]["content"][1]["type"], "video_url");
        assert!(serde_json::to_vec(&messages).unwrap().len() <= limit);
    }

    #[test]
    fn redacts_videos_from_the_log() {
        let logged = redact_media(&[
            serde_json::json!({ "role": "assistant", "videos": [{ "video_url": { "url": "data:video/mp4;base64,AAAA" } }] }),
            serde_json::json!({ "role": "user", "content": [{ "type": "video_url", "video_url": { "url": "data:video/mp4;base64,AAAA" } }] }),
        ]);
        assert!(!logged.contains("AAAA"));
    }
}
