## 变更类型

- [ ] feat
- [ ] fix
- [ ] refactor
- [ ] perf
- [ ] test
- [ ] docs
- [ ] ci
- [ ] chore
- [ ] style

## 影响范围

便于 review 与回归定位，勾选主要落点，并列出关键路径：

- [ ] adapter：TS headless / NapCat / 重连
- [ ] voice bridge / voice_router：音频、STT/TTS、gRPC
- [ ] llm：引擎、上下文、工具循环
- [ ] router：事件路由、触发前缀
- [ ] skills
- [ ] config / permission
- [ ] docs / website
- [ ] CI / 构建 / Docker

关键文件与模块：

```
src/...
docs/...
```

## 变更说明

解决什么问题；对外/对内行为如何变化，只写结论。

## 质量门

- [ ] `cargo fmt`
- [ ] `cargo clippy --all-targets --locked -- -D warnings`
- [ ] `cargo test --all-targets --locked`
- [ ] 相关文档已按 [docs/AGENTS.md](../docs/AGENTS.md) 归属更新（无文档影响则勾选）
- [ ] 已读并遵守 [docs/defensive-patterns.md](../docs/defensive-patterns.md)（改 `src/` 时）
