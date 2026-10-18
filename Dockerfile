FROM alpine:3.20
RUN apk add --no-cache ca-certificates opus ffmpeg tzdata
RUN addgroup -g 1001 appgroup && \
    adduser -u 1001 -G appgroup -D -h /home/appuser -s /sbin/nologin appuser
WORKDIR /app
COPY --chmod=0755 --chown=appuser:appgroup teamspeakclaw /app/teamspeakclaw
COPY --chown=appuser:appgroup examples/config/ /app/config/
RUN mkdir -p /app/logs && chown appuser:appgroup /app/logs
USER appuser
ENV RUST_LOG=info
ENTRYPOINT ["/app/teamspeakclaw"]
