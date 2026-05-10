FROM rust:alpine AS builder

WORKDIR /app

RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconf git

COPY Cargo.toml Cargo.lock* ./

RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release 2>/dev/null || true

COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM alpine:3.21

WORKDIR /app

RUN apk add --no-cache \
    ca-certificates \
    ffmpeg \
    bash \
    font-dejavu

COPY --from=builder /app/target/release/moqtail-pub /usr/local/bin/moqtail-pub

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

ENV RELAY_URL="https://relay:4433"
ENV STREAM_NAME="live"
ENV INPUT_FILE="/source.mp4"
ENV LOOP="true"
ENV RUST_LOG="info"

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
