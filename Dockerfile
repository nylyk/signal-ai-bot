FROM rust:1.96.0-slim-trixie AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler clang libssl-dev pkg-config tini

WORKDIR /app
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release \
    && cp target/release/signal-ai-bot /usr/local/bin/signal-ai-bot

FROM gcr.io/distroless/cc-debian13:nonroot

COPY --from=builder /usr/bin/tini /usr/bin/tini
COPY --from=builder /usr/local/bin/signal-ai-bot /usr/local/bin/signal-ai-bot
# static, dependency-free build — voice notes arrive as ogg/opus or aac, which
# ffmpeg transcodes to the wav the model backend can read
COPY --from=mwader/static-ffmpeg:8.1.2 /ffmpeg /usr/local/bin/ffmpeg

VOLUME /data

# voice notes are staged on disk for ffmpeg to seek, then deleted. /tmp isn't
# reliably writable by the nonroot user in distroless, and /data already is.
ENV TMPDIR=/data

ENTRYPOINT ["tini", "--", "signal-ai-bot"]
