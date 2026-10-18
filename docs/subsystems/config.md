# 配置加载与日志

配置加载与日志负责把磁盘上的配置文件变成运行期的 `AppConfig`，并初始化进程级日志。代码入口是 `src/config.rs` 的 `AppConfig::load_all()`，日志初始化在 `src/log.rs`。

## 职责

- `AppConfig::load_all()` 加载 `config/settings.toml` 与 `config/acl.toml`，目录由 `config_dir()` 求得（可执行文件同级的 `config`）。
- 按配置域划分子模块：`acl`、`bot`、`headless`、`llm`、`logging`、`music_backend`、`napcat`、`voice_replay`、`web_search`，每个子模块拥有自己那段的字段与默认值。
- `log.rs` 初始化 tracing 与按日轮转的文件日志，级别来自启动参数与日志配置；rich 模式不输出 tracing 控制台日志，只把 `warn/error` 计数交给 `ui.rs`，plain 模式保留控制台日志。
- 约定敏感配置的位置：API key 一类只在 config 目录内，不进 git、不写日志。

## 关键路径与序列

`main.rs` 先解析启动参数，再调 `AppConfig::load_all()`；它逐个子模块读取配置文件并反序列化成 typed config，任一步失败即终止启动。日志在装配早期初始化一次，之后的模块只取已装好的订阅者。

## 不变量

- 配置目录一律由 `config_dir()` 求得，不从当前工作目录推断；否则从不同目录启动会读不到同一份配置。
- 全局 tracing 订阅者只初始化一次，模块内不得自行安装。
- 密钥一类字段不得进入日志；读敏感文件只取结构，不整读内容。
- 加载或解析失败终止启动，不静默回退到默认值：缺配置要在启动时暴露，而不是在运行期表现为行为异常。
- 模型可见提示词不是配置项：system 提示词与 TTS 风格提示是代码常量，见[模型可见提示词装配](prompt.md)。

## 相关决策

- [提示词由代码装配](../../.agents/notes/implemented/architecture/2026-10-12-code-owned-prompt-composition.md) — 提示词段落改为代码内常量，`config/prompts.toml` 与对应配置类型被删除，配置面因此缩小。
