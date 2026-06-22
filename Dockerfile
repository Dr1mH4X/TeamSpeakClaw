FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev cmake make gcc protoc
WORKDIR /app
COPY . .
ENV PROTOC=/usr/bin/protoc
RUN cargo build --release

FROM alpine:3.20
RUN apk add --no-cache ca-certificates libopus ffmpeg
RUN addgroup -g 1001 appgroup && \
    adduser -u 1001 -G appgroup -D -h /home/appuser -s /sbin/nologin appuser
WORKDIR /app
COPY --from=builder /app/target/release/teamspeakclaw /app/teamspeakclaw
COPY examples/config/ /app/config/
RUN chmod +x /app/teamspeakclaw && \
    mkdir -p /app/logs && \
    chown -R appuser:appgroup /app
USER appuser
ENV RUST_LOG=info
ENTRYPOINT ["/app/teamspeakclaw"]
