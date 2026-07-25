# signal-ai-bot

a signal client that runs an openai-compatible chatbot in your chats. start a
message with `@ai` and the bot replies. works in 1:1 and group chats, with text,
images (vision models) and voice messages (audio models), and uses recent
messages as context.

## run

    docker compose up --build

the first run prints a qr code — on your phone, scan it under signal -> settings
-> linked devices. the link is stored in the `/data` volume, so later starts go
straight to running.

## config

set via env vars (see `docker-compose.yml`):

| var                      | required | default               | meaning                                              |
| ------------------------ | -------- | --------------------- | ---------------------------------------------------- |
| `API_BASE`               | yes      | —                     | openai-compatible base url (`…/v1`)                  |
| `API_KEY`                | no       | empty                 | api key (llama.cpp etc. need none)                   |
| `MODEL`                  | yes      | —                     | model id                                             |
| `SYSTEM_PROMPT`          | no       | none                  | system prompt                                        |
| `VISION`                 | no       | off                   | send images — only enable for vision models          |
| `AUDIO`                  | no       | off                   | send voice messages — only enable for audio models   |
| `AUDIO_SPEED`            | no       | `1.0`                 | speed up voice messages before sending (`0.5`–`2.0`) |
| `TRIGGER`                | no       | `@ai`                 | prefix that summons the bot                          |
| `PROCESSING_MESSAGE`     | no       | `ai is processing...` | placeholder while the model ingests the prompt       |
| `REASONING_MESSAGE`      | no       | `ai is thinking...`   | placeholder while the model reasons                  |
| `GENERATING_MESSAGE`     | no       | `ai is writing...`    | placeholder while the model writes the answer        |
| `CONTEXT_MESSAGES`       | no       | `5`                   | recent messages fed as context                       |
| `MESSAGE_RETENTION_DAYS` | no       | `7`                   | prune stored messages older than this; `0` keeps all |
| `REASONING_BUDGET`       | no       | `0`                   | `0` off, `N` token cap, `-1` unlimited (llama.cpp)   |
| `DEVICE_NAME`            | no       | `signal-ai-bot`       | name shown in signal's linked devices                |

works with openai, openrouter, groq, local llama.cpp / ollama, or anything else
exposing `/chat/completions`.

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
