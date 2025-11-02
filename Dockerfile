FROM golang:1.25.3-alpine3.22 AS builder

WORKDIR /app
COPY go.mod go.sum ./
RUN go mod download
COPY . .
ARG VERSION
RUN CGO_ENABLED=0 GOOS=linux go build \
    -ldflags "-s -w -X haruki-tracker/config.Version=${VERSION}" \
    -o haruki-event-tracker \
    -trimpath \
    -tags netgo \
    .

FROM alpine:latest
RUN apk --no-cache add ca-certificates tzdata
WORKDIR /app
COPY --from=builder /app/haruki-event-tracker .
RUN mkdir -p logs
EXPOSE 8080
ENV TZ=Asia/Shanghai

CMD ["./haruki-event-tracker"]
