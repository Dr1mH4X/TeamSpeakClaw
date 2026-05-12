FROM ubuntu:24.04

RUN apt-get update && apt-get install -y ca-certificates libopus0 ffmpeg curl && rm -rf /var/lib/apt/lists/*

RUN YTDLP_VERSION="2026.03.17" && \
    YTDLP_SHA256="3bda0968a01cde70d26720653003b28553c71be14dcb2e5f4c24e9921fdad745" && \
    curl -L "https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/yt-dlp" \
         -o /usr/local/bin/yt-dlp && \
    echo "${YTDLP_SHA256}  /usr/local/bin/yt-dlp" | sha256sum -c - && \
    chmod +x /usr/local/bin/yt-dlp

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
