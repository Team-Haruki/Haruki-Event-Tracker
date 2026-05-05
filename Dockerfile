FROM rust:1.95-alpine AS builder

WORKDIR /app

RUN apk add --no-cache \
    musl-dev gcc g++ cmake make perl pkgconfig linux-headers

COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && \
    echo 'fn main() {}' > src/main.rs && \
    echo '' > src/lib.rs && \
    cargo build --release --bin haruki-event-tracker 2>/dev/null || true && \
    rm -rf src

COPY . .
ARG VERSION=3.0.0-dev
RUN if [ "$VERSION" != "3.0.0-dev" ]; then \
        sed -i "s/^version = \".*\"/version = \"${VERSION#v}\"/" Cargo.toml; \
    fi && \
    find src -name '*.rs' -exec touch {} + && \
    cargo build --release --bin haruki-event-tracker && \
    strip target/release/haruki-event-tracker

FROM alpine:3.23 AS runtime
RUN apk add --no-cache ca-certificates tzdata
WORKDIR /app
COPY --from=builder /app/target/release/haruki-event-tracker ./haruki-event-tracker
RUN mkdir -p logs
ENV TZ=Asia/Shanghai
EXPOSE 8080
CMD ["./haruki-event-tracker"]
