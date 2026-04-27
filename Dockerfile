FROM rust:1.85-slim-bookworm AS builder

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config cmake build-essential perl \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && \
    echo 'fn main() {}' > src/main.rs && \
    echo '' > src/lib.rs && \
    cargo build --release --bin haruki-event-tracker 2>/dev/null || true && \
    rm -rf src

COPY . .
ARG VERSION=2.0.0-dev
RUN if [ "$VERSION" != "2.0.0-dev" ]; then \
        sed -i "s/^version = \".*\"/version = \"${VERSION#v}\"/" Cargo.toml; \
    fi && \
    cargo build --release --bin haruki-event-tracker && \
    strip target/release/haruki-event-tracker

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/haruki-event-tracker ./haruki-event-tracker
RUN mkdir -p logs
ENV TZ=Asia/Shanghai
EXPOSE 8080
CMD ["./haruki-event-tracker"]
