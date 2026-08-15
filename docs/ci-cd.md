# CI/CD

GitHub Actions 共 9 个 workflow，位于 `.github/workflows/`。下表为每个 workflow 的触发条件、职责与产物。

| workflow | 触发器 | 职责 | 产物 |
|---|---|---|---|
| `ci.yml` | push/PR 到 main/master（push 按路径过滤、PR 排除 `website/**`）、手动 | 质量门 + 三平台构建 | 平台归档（见 reusable-build） |
| `release.yml` | push tag `v*` | 发版：质量门 + 构建 + Docker 镜像 + 变更日志 + 建 Release | GitHub Release、GHCR 镜像、变更日志 |
| `deploy-website.yml` | push/PR 到 main 且改动 `website/**` 或 workflow 本身、手动 | Docusaurus 站点构建与 Pages 部署 | GitHub Pages |
| `docker-sha.yml` | 手动（可选 `sha-tag` 输入） | 给当前 commit 打 SHA tag 并推 GHCR 镜像 | GHCR 镜像 |
| `cleanup-untagged.yml` | 手动 | 删 GHCR 未打标镜像 | 无 |
| `reusable-build.yml` | workflow_call | 三平台 release 构建并打包归档 | zip/tar.gz 归档、ARTIFACT 附件 |
| `reusable-changelog.yml` | workflow_call | 用 git-cliff 生成变更日志正文 | `release_body` 输出 |
| `reusable-docker.yml` | workflow_call | 构建并推送 GHCR 镜像 | GHCR 镜像 |
| `reusable-meta.yml` | workflow_call | 解析版本与发布属性 | version/tag/is-* 输出 |

## 主线流程

`ci.yml` 与 `release.yml` 结构一致：`quality` 在 ubuntu 上跑 `cargo fmt --check`、`cargo test --all-targets --locked`、`cargo clippy --all-targets --locked -- -D warnings`（装 `cmake libopus-dev`）；`meta` 调 reusable-meta 解析版本与发布属性（tag `v*` 时 version 取去 `v`，否则为 `0.0.0-dev.<短sha>`）；`build` 依赖前两者调 reusable-build。

`reusable-build.yml` 矩阵三平台：windows-amd64 / linux-amd64 / macos-aarch64，各平台装对应系统依赖（macOS `brew install autoconf automake libtool`）。发版时先用 Python 校验 semver 并改写 `Cargo.toml` / `Cargo.lock` 版本，再 `cargo build --release`，把二进制与 `examples/config/` 三个模板打进 zip 或 tar.gz 归档并上传 artifact。

`release.yml` 在 `build` 之外，`docker` 调 reusable-docker 推送镜像，`changelog` 调 reusable-changelog，最终 `release` 把归档挂到 `softprops/action-gh-release` 创建的 GitHub Release，正文取 changelog 输出，`-beta` tag 标记为 prerelease。

## Docker 与清理

`reusable-docker.yml` 构建并推送 `ghcr.io/<owner>/teamspeakclaw`：发版（含 `-beta`）打 semver tag；稳定版另打 `latest`；手动/非发版打 `sha-<短sha>` 或传入的自定义 sha-tag。`docker-sha.yml` 与 `cleanup-untagged.yml` 手动触发，后者经 `gh api` 遍历容器版本并删除无 tag 项（自动适配个人账号与组织路径）。

## 变更日志

`reusable-changelog.yml` 用 `git-cliff`（配置 `.github/cliff.toml`，`conventional_commits = true`）按分组生成历代变更，`--latest --strip header` 输出最新一版，写 `CHANGES.md` 并作为 `release_body` 输出。含 `[skip changelog]` 的提交被跳过，`rc` tag 被忽略。
