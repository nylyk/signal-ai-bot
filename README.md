# signal-ai-bot

a signal client that runs an openai-compatible chatbot in your chats. start a
message with `@ai` and the bot replies. works in 1:1 and group chats, with text
and images (vision models), and uses recent messages as context.

## run

    docker compose up --build

the first run prints a qr code — on your phone, scan it under signal -> settings
-> linked devices. the link is stored in the `/data` volume, so later starts go
straight to running.

## config

set via env vars (see `docker-compose.yml`):

| var                | required | default           | meaning                                            |
| ------------------ | -------- | ----------------- | -------------------------------------------------- |
| `API_BASE`         | yes      | —                 | openai-compatible base url (`…/v1`)                |
| `API_KEY`          | no       | empty             | api key (llama.cpp etc. need none)                 |
| `MODEL`            | yes      | —                 | model id                                           |
| `SYSTEM_PROMPT`    | no       | none              | system prompt                                      |
| `TRIGGER`          | no       | `@ai`             | prefix that summons the bot                        |
| `THINKING_MESSAGE` | no       | `ai is thinking…` | placeholder shown while generating                 |
| `CONTEXT_MESSAGES` | no       | `5`               | recent messages fed as context                     |
| `REASONING_BUDGET` | no       | `0`               | `0` off, `N` token cap, `-1` unlimited (llama.cpp) |
| `DEVICE_NAME`      | no       | `signal-ai-bot`   | name shown in signal's linked devices              |

works with openai, openrouter, groq, local llama.cpp / ollama, or anything else
exposing `/chat/completions`.
