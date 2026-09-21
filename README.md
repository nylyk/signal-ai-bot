# signal-ai-bot

a signal client that runs an openai-compatible chatbot in your chats. start a
message with `@ai` and the bot replies. works in 1:1 and group chats, with
text, images (vision models) and voice messages (audio models), and uses recent
messages as context. digits right after the trigger shorten that window for one
prompt — `@ai2` sees only the last two messages.

## run

    docker compose up --build

the first run prints a qr code — on your phone, scan it under signal -> settings
-> linked devices. the link is stored in the `/data` volume, so later starts go
straight to running.

## config

set via env vars (see `docker-compose.yml`):

| var                      | required | default                      | meaning                                              |
| ------------------------ | -------- | ---------------------------- | ---------------------------------------------------- |
| `API_BASE`               | yes      | —                            | openai-compatible base url (`…/v1`)                  |
| `API_KEY`                | no       | empty                        | api key (llama.cpp etc. need none)                   |
| `MODEL`                  | yes      | —                            | model id                                             |
| `SYSTEM_PROMPT`          | no       | none                         | system prompt                                        |
| `REASONING_BUDGET`       | no       | `0`                          | `0` off, `N` token cap, `-1` unlimited (llama.cpp)   |
| `VISION`                 | no       | off                          | send images — only enable for vision models          |
| `AUDIO`                  | no       | off                          | send voice messages — only enable for audio models   |
| `AUDIO_SPEED`            | no       | `1.0`                        | speed up voice messages before sending (`0.5`–`2.0`) |
| `DOCUMENTS`              | no       | off                          | send pdfs — only enable for models that read them    |
| `EXTRA_TOOLS`            | no       | none                         | json array of tools the provider runs itself         |
| `TRIGGER`                | no       | `@ai`                        | prefix that summons the bot                          |
| `CONTEXT_MESSAGES`       | no       | `5`                          | recent messages fed as context (max; see `@ai2`)     |
| `MESSAGE_RETENTION_DAYS` | no       | `7`                          | prune stored messages older than this; `0` keeps all |
| `DEVICE_NAME`            | no       | `signal-ai-bot`              | name shown in signal's linked devices                |
| `PROCESSING_MESSAGE`     | no       | `ai is processing...`        | placeholder while the model ingests the prompt       |
| `REASONING_MESSAGE`      | no       | `ai is thinking...`          | placeholder while the model reasons                  |
| `GENERATING_MESSAGE`     | no       | `ai is writing...`           | placeholder while the model writes the answer        |
| `TOOL_MESSAGE`           | no       | `ai is looking things up...` | placeholder while a tool call runs                   |

works with openai, openrouter, groq, local llama.cpp / ollama, or anything else
exposing `/chat/completions`. `docker-compose.yml` is set up for a local
llama.cpp; `docker-compose.openrouter.yml` overlays an openrouter configuration
with the server tools turned on:

```sh
echo 'API_KEY=sk-or-v1-...' > .env
docker compose -f docker-compose.yml -f docker-compose.openrouter.yml up -d
```

## tools

the model always has a `load_attachment` tool, so it needs one that supports
tool calling. every attachment in the context window is listed and numbered —
`[attachments: #1 image/jpeg photo.jpg 2.1MB]` — and fetched only when the model
asks for it by id. the triggering message's own attachments, and those of a
message it replies to, are sent inline as well, so captioning a photo with `@ai`
needs no round-trip; those are marked `(shown above)`.

text is always readable — anything `text/*` plus json, xml, yaml, toml and the
like, including the `text/x-signal-plain` attachment signal uses for an
oversized message body. images, voice messages and pdfs each need their flag
(`VISION`, `AUDIO`, `DOCUMENTS`); pdfs travel as a `file` content part, which
hosted providers parse (openrouter bills per page on models without native
document input) and llama.cpp does not accept at all.

anything a flag rejects is still listed, marked `(unreadable)`, so the model
knows the attachment is there instead of inventing an id for it. ids only ever
refer to attachments on messages already in the context window, so the tool
reads what the model was already shown and never widens its view of the chat.

a second tool, `load_avatar`, takes a display name and returns that person's
profile picture, or the group's own picture when given the group title. it
reaches the thread's participants only, never the rest of the contact store, and
needs `VISION`.

when a provider runs an image generation tool, the images it returns are posted
into the chat as attachments. on openrouter that is one more `EXTRA_TOOLS` entry:

```yaml
EXTRA_TOOLS: '[{"type":"openrouter:image_generation"}]'
```

`EXTRA_TOOLS` passes tools straight through to the provider, for the ones it runs
itself. on openrouter:

```yaml
EXTRA_TOOLS: '[{"type":"openrouter:web_search"},{"type":"openrouter:web_fetch"}]'
```

the value is sent verbatim in the request's `tools` array, so any provider's
server-tool syntax works. both need a model that supports tool calling.

## voice messages

with `AUDIO` on, reply to a voice message with `@ai what did they say` and the
recording is sent along as `input_audio` parts. voice messages in the recent
context window are sent too, so the bot can follow a conversation that includes
them.

needs `ffmpeg` on `PATH` — the docker image ships a static build. signal records
voice notes as ogg/opus or aac, and audio backends generally want wav, so each
one is transcoded to 16 kHz mono wav first.

models cap a single clip at 30 seconds, so longer recordings are split into
consecutive 30s chunks, up to 10 of them (5 minutes); anything past that is
dropped. `AUDIO_SPEED` above `1.0` fits more of a long recording into that
budget at some cost to transcription accuracy — `1.5` covers 7m30s.

only some models take audio at all. for gemma 4 that's the E2B, E4B and 12B
variants; the 26B-A4B and 31B ones are text and images only.
