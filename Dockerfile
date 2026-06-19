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

VOLUME /data

ENTRYPOINT ["tini", "--", "signal-ai-bot"]
