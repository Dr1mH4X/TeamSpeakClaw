FROM ubuntu:24.04

RUN apt-get update && apt-get install -y ca-certificates libopus0 ffmpeg && rm -rf /var/lib/apt/lists/*

RUN groupadd --gid 1001 appgroup && \
    useradd --uid 1001 --gid appgroup --create-home --shell /usr/sbin/nologin appuser

WORKDIR /app

COPY teamspeakclaw /app/teamspeakclaw
COPY config-docker/ /app/config/
RUN chmod +x /app/teamspeakclaw

RUN mkdir -p /app/config /app/logs && \
    chown -R appuser:appgroup /app

USER appuser

ENV RUST_LOG=info

ENTRYPOINT ["/app/teamspeakclaw"]
