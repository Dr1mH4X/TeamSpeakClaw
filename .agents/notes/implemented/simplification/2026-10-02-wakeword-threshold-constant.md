# Agent Note: 唤醒阈值固定为常量

Status: implemented

## Problem

`[headless.wakeword].threshold` 曾是配置项，默认 0.3，校验区间 (0, 1)。阈值是分类器的召回/误报权衡点，用户既没有基准、也没有测量手段判断该填多少，暴露它等于把调参成本转移给使用者；配置面还因此多一条校验与一个模板字段。

## Decision

- 阈值固定为 `DETECTION_THRESHOLD: f32 = 0.3`，定义在 `src/adapter/headless/wakeword/model.rs`，与 `DETECTION_BUFFER_SIZE`、`MIN_POSITIVE_DETECTIONS`、`NO_DETECTION_MS` 同组；检测与平滑均值两处直接引用该常量。
- `HeadlessWakewordConfig` 删除 `threshold` 字段及其默认值，`config.rs` 删除阈值校验分支，`examples/config/settings.toml` 删除对应行；`WakewordModels` 不再持有阈值，`from_bytes` 只接收三个模型字节。
- 旧 `settings.toml` 里残留的 `threshold` 行由 serde 按未知字段忽略（配置结构未启用 `deny_unknown_fields`），升级不报错。

## Alternatives considered

- **保留字段、只在文档里给建议值**：用户仍面对一个无基准的数字，改错值与不改值都无据可依。
- **改用环境变量覆盖**：同样需要用户理解阈值语义，且与「配置集中在 `settings.toml`」的既有约定不一致。
- **把阈值升级为分类器元数据**：社区模型不提供推荐阈值字段，需要另立清单与解析逻辑，当前只有一个固定用法，属 YAGNI。

## Consequences

- 所得：配置面收窄，唤醒只需 `enabled`、`model`、`window_secs` 三项；少一条校验分支与一处模板字段；检测参数集中在一处便于对读上游实现。
- 代价：临时放宽/收紧召回率需要改代码重新编译；若日后多分类器并存且各自需要不同阈值，需重新引入配置或元数据。
- 验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`。
