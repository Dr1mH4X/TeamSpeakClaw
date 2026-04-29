# 构建阶段：使用 musl 静态编译
FROM rust:latest AS builder

RUN apt-get update && apt-get install -y musl-tools && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /usr/src/teamspeakclaw

# 复制依赖清单以利用 Docker 缓存
COPY Cargo.toml Cargo.lock ./

# 预构建依赖（创建空项目骨架）
RUN mkdir -p src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --target x86_64-unknown-linux-musl --release && \
    rm -rf src

# 复制完整源码
COPY . .

# 实际构建
RUN cargo build --target x86_64-unknown-linux-musl --release

# 运行阶段：极简镜像
FROM alpine:latest

RUN apk add --no-cache ca-certificates

RUN addgroup -g 1001 appgroup && \
    adduser -D -u 1001 -G appgroup -s /sbin/nologin appuser

WORKDIR /app

COPY --from=builder /usr/src/teamspeakclaw/target/x86_64-unknown-linux-musl/release/teamspeakclaw /app/teamspeakclaw

# 复制默认配置目录（如果存在）
COPY config-docker/ /app/config/

RUN chmod +x /app/teamspeakclaw && \
    mkdir -p /app/config /app/logs && \
    chown -R appuser:appgroup /app

USER appuser

ENV RUST_LOG=info

ENTRYPOINT ["/app/teamspeakclaw"]
