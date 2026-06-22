FROM alpine:3.20

RUN apk add --no-cache ca-certificates libopus ffmpeg

RUN addgroup --gid 1001 appgroup && \
    adduser --uid 1001 --ingroup appgroup --disabled-password --gecos "" appuser

WORKDIR /app

COPY teamspeakclaw /app/teamspeakclaw
COPY config-docker/ /app/config/
RUN chmod +x /app/teamspeakclaw

RUN mkdir -p /app/config /app/logs && \
    chown -R appuser:appgroup /app

USER appuser

ENV RUST_LOG=info

ENTRYPOINT ["/app/teamspeakclaw"]
