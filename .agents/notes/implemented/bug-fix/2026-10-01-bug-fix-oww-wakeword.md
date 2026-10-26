# Agent Note: 语音唤醒改用 OpenWakeWord 音频门（Issue #91）
Status: implemented — 分支 refactor/wakeword-oww 落地，fmt / clippy -D warnings / test 214 / release build 全绿

## Problem

Issue #91：语音唤醒不触发。唤醒判定原先是 STT 转写后的文本前缀匹配（`preprocess_stt_text` 内 `wake_words`/`wake_word_required`），两个硬伤：唤醒排在 STT 之后，命中与否受转写错字影响；omni 模式（`llm.omni_model`）把音频直送 LLM，完全绕过文本匹配，唤醒门对它不存在。

## Decision

唤醒前移到音频层：`handle_audio_chunk`（`router/voice_router.rs`）在 caller 解析与 gRPC 查询之前跑 `WakewordGate`（`src/adapter/headless/wakeword/`），关门语音直接丢弃，omni 与 STT 两路共用同一道门。推理核心 vendor 自 oww_rs（MIT，@d945876）：tract 0.23 纯 Rust ONNX 推理；三个 onnx（`melspectrogram.onnx`/`embedding_model.onnx` 前端 + `[headless.wakeword].model` 指定的分类器）均从 `models_dir()`（exe 同级 `models/`，与 docker-compose 挂载的 `./models` 对应）运行时读取，不随二进制内置；加载接缝是 `load_models_from(dir, cfg)`，生产薄封装 `load_models(cfg)` 传 `crate::config::models_dir()`，缺文件/空文件与解析失败均按 anyhow 在 `Runtime::start` 上抛终止启动（metadata 预检 + `catch_unwind` → anyhow，与 gRPC bind 同为启动期资源），缺文件消息指向 [models/README.md](../../../../models/README.md) 的下载说明。三个模型文件全部不入库、不进发行归档，由使用者按 `models/README.md` 的清单与脚本下载（CC BY-NC-SA 4.0 与 AGPL-3.0 冲突，理由见 [2026-10-01-process-oww-model-license.md](../../implemented/architecture/2026-10-01-process-oww-model-license.md)）；GHCR 镜像不含模型，靠 `./models:/app/models` 挂载供给。旧文本唤醒整体删除：`wake_words`/`wake_word_required` 配置字段、`preprocess_stt_text` 内的匹配逻辑与 `skip_punct_and_whitespace`；`preprocess_stt_text` 签名改为 `(raw, wakeword_gate: bool)`，保留归一化、剥前导标点、240 字截断，最小 CJK 长度门按 `wakeword.enabled` 开关（门开时语音已在音频层筛选，短句放行；门关时维持原噪声过滤）。

门控状态为 per-clid 实例池：每个说话人独立的特征滑窗、1280 采样余量跨 utterance 携带、`last_wake` 开门窗口（`window_secs`，默认 15s，窗口内持续喂模型允许续命中）；池上限 6 人 LRU 驱逐 + 空闲驱逐（阈值取 `300s` 与 `window_secs + 60s` 的较大者，避免 `window_secs` 取上限 300s 时窗口尾被驱逐截断），与 voice_replay 的 `SpeakerRings` 同范式。检测器经 `Detector` trait + 工厂闭包注入（生产闭包捕获 `Arc<WakewordModels>`，测试注入桩），`feed` 只有一条路径。命中记 `info!(event = "voice.wakeword", probability, ...)`，关门丢弃记 debug。`[headless.wakeword]` 校验在 `AppConfig::validate()`：threshold ∈ (0,1)、window_secs ∈ 1..=300，enabled 时 `model` 非空且 `stt.enabled || llm.omni_model`。

## Alternatives considered

**直接依赖 oww-rs crate**：会连带 cpal 麦克风胶水、内置唤醒词清单与整个 workspace，CI 与依赖面都要动；本项目只要推理核心，vendor 合并为 `wakeword/model.rs` 一个文件改动面更小。

**前端模型 `include_bytes!` 内嵌**：少两个必配文件、开箱即用；与「模型放 `models/` 宿主目录、不进二进制」的部署约定冲突（与 whisper 模型同一目录），二进制体积也随模型增长，弃用，三个 onnx 统一走 `models_dir()`。

**单实例共享推理状态**：上游 `OwwModel` 单实例在多说话人下把不同人的音频混进同一特征窗，2s 不应期是全局的，开门无法归因到触发者；per-clid 池把特征窗、窗口、不应期全部隔离到 clid，代价是每会话约几十 KB 缓冲（480 采样 lookback、16×96 特征环、10 块 mel 环、12 次检测、≤1280 采样余量）与 LRU 上限。三个 `Arc<RunnableModel>` 在 `WakewordModels` 里解析一次后共享，per-clid 实例只 clone 这个 `Arc`，不复制权重。

**三个 onnx 全由使用者自备**：发行包体积最小，也不把模型许可带进仓库；代价是每个想试唤醒门的人先要自己凑齐两个固定名前端模型，而这两个文件名写死在代码里、没有选择余地。此取舍后由 [2026-10-01-process-oww-model-license.md](../../implemented/architecture/2026-10-01-process-oww-model-license.md) 改判：CC BY-NC-SA 4.0 与 AGPL-3.0 冲突，三个模型统一改由使用者按 `models/README.md` 下载，下载脚本与 SHA-256 校验抵消凑文件名的成本。

**门放在 caller 解析之后**：解析要经 gRPC 查身份；门前置后关门语音省掉解析与查询，clid 在 `SpeechChunk` 上现成，无需先解析。

**保留文本匹配做二次确认**：omni 路径拿不到文本，双确认门不闭环；音频判定命中即开门，STT 文本只做清洗。

## Consequences

- release `panic = "abort"`（`Cargo.toml`）下 `catch_unwind` 失效，非法分类器的防线是 metadata 预检与 tract 解析 Err；解析深处 panic 会中止进程，发生在启动期。
- 每次 `feed` 是同步 tract 推理（每 1280 采样一次），长 utterance（12s ≈ 150 次推理）会短暂阻塞 audio drain 任务；队列深度 8，聊天事件在独立任务不受影响。
- 模型不进二进制：启用时 `models/` 必须齐备三个 onnx，全部由使用者按 [models/README.md](../../../../models/README.md) 下载（来源、校验和与许可都在该文件），缺任一文件或文件为空启动即 Err。模型按 CC BY-NC-SA 4.0 授权，非商业限制只作用于下载到本地之后的模型文件，不再随归档传递。
- 验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`（214 通过，其中 gate 5 例、模型加载接缝 2 例、preprocess 3 例、validate 2 例）、`cargo build --release --locked` 全绿；仓库 `models/` 内两个前端模型另经一次性解析用例确认可被 tract 0.23 解析。
