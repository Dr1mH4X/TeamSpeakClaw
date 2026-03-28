FROM alpine:3.19

RUN apk add --no-cache ca-certificates tzdata

RUN addgroup -g 1001 -S appgroup && \
    adduser -u 1001 -S appuser -G appgroup

WORKDIR /app

COPY teamspeakclaw /app/teamspeakclaw
RUN chmod +x /app/teamspeakclaw

RUN mkdir -p /app/config /app/logs && \
    chown -R appuser:appgroup /app

USER appuser

ENV RUST_LOG=info

ENTRYPOINT ["/app/teamspeakclaw"]
