# signal-ai-bot

a signal client (presage) that links to your phone via qr code and runs an
openai-compatible chatbot inside your chats. when you or a contact starts a
message with `@ai`, the bot posts "ai is thinking…", queries the api, then
edits that message into the answer.

## build deps

needs `protoc` and a c toolchain for the signal crates.

arch:

    sudo pacman -S protobuf clang

debian:

    sudo apt install protobuf-compiler clang libssl-dev pkg-config

then:

    cargo build --release

first build is slow (libsignal). the curve25519 patch in Cargo.toml is required
by presage's dependency tree.

## link to your phone

    cargo run --release -- link --device-name signal-ai-bot

a qr code prints in the terminal. on your phone: signal -> settings -> linked
devices -> link new device -> scan it. linking state is stored in the sqlite db
(default `~/.config/signal-ai-bot/store.db3`).

## run

set the api config via env, then run:

    export AI_API_KEY=sk-...                       # required
    export AI_API_BASE=https://api.openai.com/v1   # any openai-compatible base
    export AI_MODEL=gpt-4o-mini
    export AI_SYSTEM_PROMPT="you are a helpful assistant"   # optional
    export REASONING_BUDGET=0                       # optional: 0 off, N cap, -1 unlimited (llama.cpp)
    export CONTEXT_MESSAGES=10                     # optional, default 0 (off)
    export TRIGGER=@ai                             # optional, default @ai
    export THINKING_MSG="ai is thinking…"          # optional

    cargo run --release -- run

works with anything exposing `/chat/completions` (openai, openrouter, groq,
local llama.cpp / ollama via its openai endpoint, etc) — just point AI_API_BASE
at it.

## notes

- triggers on `@ai` at the **start** of a message, both from contacts and from
  messages you send yourself (synced from your phone).
- works in 1:1 and group chats. replies go into the same thread, sent as your
  account.
- the backlog replayed at startup is ignored; only messages arriving after the
  initial sync are answered.
- prompts are handled one at a time (the manager is single-threaded), so a slow
  api call briefly blocks the receive loop. fine for personal use.
- the bot posts "ai is thinking…" first, then edits that message into the answer
  once the api responds.
- reasoning/thinking models: if the model wraps reasoning in `<think>…</think>`
  inside the content, that block is stripped from the answer. a separate
  `reasoning_content` field is ignored automatically.
- `REASONING_BUDGET=N` controls reasoning per request: `0` disables it (sends
  `reasoning_budget=0` and `chat_template_kwargs.enable_thinking=false`), a
  positive `N` caps the reasoning tokens, `-1` is unlimited; unset leaves the
  server default. ignored by apis that don't use those fields.
- image input: if the @ai message has image attachments (e.g. a photo captioned
  `@ai what's this`), or it's a reply to a message with an image or sticker,
  those are sent to the model as image_url data urls. needs a vision-capable
  model (an mmproj in llama.cpp). for image replies the full-resolution image is
  read from the local store, falling back to the quote's thumbnail; gif/video
  replies use that thumbnail's still frame. webp (stickers) and other non-jpeg/png
  images are transcoded to png so llama.cpp can decode them. empty or undecodable
  attachments are skipped rather than sent as broken urls.
- when the @ai message is a reply to another message, that message is included
  in the prompt as context, annotated with the name of the person being replied
  to (e.g. `[in reply to Alice: "…"]`).
- every `user` turn is labelled with the sender's signal name, including the
  triggering message itself, so the model knows who is asking. your own messages
  use your signal profile name (resolved once at startup; falls back to `Me` if
  it can't be fetched). unknown contacts show as `Them`.
- `CONTEXT_MESSAGES=N` feeds the last N messages of the thread to the api as
  real chat turns (not a transcript blob): the bot's own past replies become
  `assistant` turns, everything else is a name-prefixed `user` turn. replies in
  the history carry the same `[in reply to …]` annotation. default 0 = no
  history. only text messages are included; attachments/reactions are skipped.
- the bot's own replies are tracked in `ai_replies.json` next to the store db.
  presage doesn't persist sent edits locally, so this file is how the bot
  remembers what it answered (for context, and to tell its turns apart from
  yours). safe to delete; you just lose that memory.

## flags

    --sqlite-db-path <path>   override the store location
    --passphrase / -p <pass>  encrypt the local store
