# ── Build stages (cargo-chef: dependencies are cooked in their own cached layer) ──
FROM lukemathwalker/cargo-chef:0.1.78-rust-1.98-alpine AS chef

RUN apk add --no-cache \
    musl-dev gcc g++ cmake make perl pkgconfig linux-headers

WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked --bin haruki-event-tracker --recipe-path recipe.json
# The version comes from Cargo.toml (bumped before tagging); CI never rewrites it.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin haruki-event-tracker && \
    strip target/release/haruki-event-tracker

FROM alpine:3.24 AS runtime
RUN apk add --no-cache ca-certificates tzdata
WORKDIR /app
COPY --from=builder /app/target/release/haruki-event-tracker ./haruki-event-tracker
RUN addgroup -S -g 101 haruki && \
    adduser -S -D -H -u 100 -G haruki -h /app haruki && \
    mkdir -p logs && \
    chown -R haruki:haruki /app
USER 100:101
ENV TZ=Asia/Shanghai
EXPOSE 8080
CMD ["./haruki-event-tracker"]
