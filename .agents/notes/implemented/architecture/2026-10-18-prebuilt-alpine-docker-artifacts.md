# Agent Note: Docker 镜像组装预构建 Alpine 产物

Status: implemented

## Problem

Docker 构建在 `rust:alpine` 内执行完整的 release 编译。源码变化会让 `COPY src` 之后的 `cargo build` 层失效，Rust 依赖与静态 opus 每次都从头编译；发布时 Docker job 明显慢于 Ubuntu 上的 Linux 原生构建。另一方面，Docker 运行层是 `alpine:3.20`，Debian/Ubuntu 构建的 glibc 二进制不能直接复制进这个 musl 运行环境。

## Decision

`reusable-build-alpine.yml` 在 `rust:1.98.0-alpine` 中构建 `x86_64-unknown-linux-musl`，产出内部 `Alpine-amd64` 归档；构建目标目录与 Cargo registry 分别用独立的缓存 key。`reusable-docker.yml` 下载该归档，提取 `teamspeakclaw`，用 `Dockerfile` 组装 `alpine:3.20`，不在 Docker 内编译。`Dockerfile` 只负责运行层与文件复制。

发布与 CI 同时构建 `Ubuntu-amd64`（`ubuntu-24.04`）、`Debian-amd64`（`debian:bookworm` 容器）与内部 `Alpine-amd64`；Ubuntu 与 Debian 产物进入 Release，Alpine 产物只作为 Docker 的二进制输入。最终镜像仍使用 `alpine:3.20`、相同的运行包与 UID/GID。

## Alternatives considered

**Debian/Ubuntu 编译后复制到 Alpine。** Docker 的 `COPY --from` 不检查发行版，但 glibc 动态二进制需要 glibc 加载器与动态库；`gcompat` 不能可靠覆盖 opus/ffmpeg 的依赖链，排除。

**把 Docker 运行层改成 Ubuntu。** Debian bookworm 构建的 glibc 2.36 二进制可以运行在 Ubuntu 24.04 上，但会放弃 Alpine 的体积优势；当前要求保持运行镜像不变，排除。

**只在 Docker 内增加 cargo cache。** 依赖编译仍与源码 `COPY` 处于同一层，源码变化会重建该层；Alpine 的静态 opus 构建成本仍然存在。缓存只能改善重复构建，不能把 Docker job 变成纯组装，排除作为主方案。

**直接把 Debian 产物作为 Docker 输入。** 与第一个方案相同的 glibc/musl 不兼容问题，排除。

## Consequences

Docker job 的主要工作变成下载 artifact、解压与组装运行镜像，Rust 编译由独立的 Alpine 构建任务承担；该任务可以用 Rust cache 与 Cargo registry cache 跨提交复用。最终镜像的软件包、配置、用户和入口不变，镜像体积不因换构建方式增加。

发布新增 `Debian-amd64` 归档，原来的 Linux 归档改名为 `Ubuntu-amd64`；`Alpine-amd64` 只保留在 Actions artifact 中。外部脚本若写死旧的 `linux-amd64` 名称需要调整。`Alpine-amd64` 构建会验证产物不是动态链接；正式镜像仍需在发布前用真实配置启动一次，重点检查 DNS、时区与日志时间戳。

## Testing

`reusable-build-alpine.yml` 用 `file` 检查产物不是 `dynamically linked`，并把 `TeamSpeakClaw-v<version>-Alpine-amd64.tar.gz` 上传为内部 artifact。Docker 组装后以 `docker run --rm <image> /app/teamspeakclaw --version` 验证入口可执行，再用 `ffmpeg -version` 与最小配置启动验证运行时依赖。
