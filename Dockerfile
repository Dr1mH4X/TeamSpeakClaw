FROM alpine:3.20

RUN apk add --no-cache ca-certificates libopus ffmpeg

RUN addgroup -g 1001 appgroup && \
    adduser -u 1001 -G appgroup -D -h /home/appuser -s /sbin/nologin appuser

WORKDIR /app

COPY teamspeakclaw /app/teamspeakclaw
COPY config-docker/ /app/config/
RUN chmod +x /app/teamspeakclaw

RUN mkdir -p /app/config /app/logs && \
    chown -R appuser:appgroup /app

USER appuser

ENV RUST_LOG=info

ENTRYPOINT ["/app/teamspeakclaw"]
