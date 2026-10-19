# Agent Note: 简化走查阶段 1-2（死面清理与路由骨架统一）
Status: implemented — 六个 commit 已落地，fmt/clippy/test 全绿

## Problem

全仓走查（覆盖 src/ 全部 31 个文件、Cargo.toml、proto/voice.proto、.github/workflows）发现三类冗余：Cargo.toml 九个直接依赖与 proto 语音桥一批「写而不读」的字段/消息/整条 LogEvent 管道无人消费；Skill trait 的 `execute_nc` 死方法、`UnifiedInboundEvent.should_respond` 恒同值字段、`HeadlessRuntimeConfig`/`TsRouterExit` 等薄壳与双胞胎类型（`MainSubscriptions`/`TsSubscriptions`、两个 RAII guard）只增维护成本；三个 router 的 LLM 单回合执行、任务超时收割、reply-mode 映射、在线客户端 JSON、server groups 解析等骨架各写一份。

## Decision

阶段 1（纯删除，零行为面）：删除 `md5`/`dialoguer`/`winapi`/`rand`/`percent-encoding`/`bytes`/`once_cell`/`prost-types`/`futures` 依赖（后五者仍是传递依赖，仅清直接声明）；proto 删 `Ping` rpc、`PlaybackEvent`、`LogEvent` 死管道（含 `emit_log`/`include_log`/订阅开关）、`Event.unix_ms`、`ChatEvent.should_respond`/`invoker_avatar_hash`/`invoker_description`、`AudioFrameEvent.is_whisper`，`adapter/headless/types.rs` 随之整体删除；删 `Skill::execute_nc` trait 方法与 `to_nc_ctx`（`NcExecutionContext` 保留，nc_router 仍构造）；删 `should_respond`/`AclRule.name`/`ToolLoopResult.finish_reason` 的 pub/`TsRouterExit`/`TsSubscriptions`；`OpusSttPipeline` 四字段改模块级 const；`HeadlessRuntimeConfig` 移除（`Runtime` 直传 `Arc<AppConfig>`）；`permission.rs` 壳合并、`connect_if_enabled` 下沉 `napcat/ws.rs`；`validate_target` 改 `Result<()>`。

阶段 2（去重）：`SkillRegistry::execute_skill` 收拢为带 ACL 检查的唯一入口（统一上下文 + 平台分支日志，错误文案单点），ts/nc/voice 三路径复用；任务收割五份合并进 `adapter/reconnect.rs`（`drain_managed_tasks`/`abort_managed_tasks`/`join_or_abort`/`wait_with_timeout` 与 `now_unix_ms`/`now_unix_secs`）；reply-mode 映射并入 `config.rs::reply_target_mode`；`TsAdapter::list_clients_json`（带 caller_clid 返回 `(String, u64)` 保同快照语义）与 `parse_server_groups` 提为共享；`AppConfig::is_music_bot_name`；`skills/http.rs::shared_client(timeout)` 合并两个 reqwest 单例；`config.rs::load_toml` 统一三处配置加载；`router.rs::run_llm_turn` 共享单回合骨架（消息构建闭包参数化 omni 差异，`SqExecutor`/`NcExecutor`/`SkillExecutor` 三个薄包装消除，`LLM_ERROR_REPLY` 文案单点）；nc_router 的 `spawn_handle_private`/`spawn_handle_group` 合并；moderation 三个 `execute_unified` 冗余覆盖删除。

## Alternatives considered

**llm.rs / config.rs 的 pub use 聚合层**：同为转发壳，但调用面广、是标准公共入口，保留，收益小于改动面。

**LlmProvider trait**：仅一个生产实现，但是 tool_loop 单测的 HTTP 隔离缝，收敛风险大于收益，保留。

**ToolExecutor trait**：三个包装消除后仍有 `CountingExecutor` 测试桩，保留。

**omni 音频路径与文本路径强合并**：tts_guard/abort/finish 语义、发送通道、save_turn 时机差异过大，强行闭包化引入回调嵌套损害可读性；只共享消息构建差异点，后置处理留在各调用点。

**moderation 三技能的 NapCat 跨适配器执行**：原覆盖在 ACL 通配时允许从 QQ 跨适配器执行 TS 命令；删除后走默认「不支持 NapCat」。按技能 TS-only 定位接受，路径若需保留应另立决策。

**PokeClient 的 NapCat 分支**：走查曾认为其与 GetClientInfo 同构（list_clients → find → 组解析），实读发现 Poke 无在线校验；为避免给离线 clid 引入新的提前报错路径，只切换 adapter 获取，不补校验。

## Consequences

净删约 750 行（6 个 commit，分支 refactor/simplifications，工作区干净；cargo fmt --check、clippy -D warnings、test 118 全绿）。行为面差异已记录：moderation 三技能 NapCat 跨适配器路径不可达；`validate_target` 对非法组 ID 由报错改静默忽略（原返回值本就全被丢弃）；nc_router 的 `list_clients_json` 失败路径多一条 warn；PromptsConfig 缺失时错误消息多一层 parse 前缀；`musicbot_name` 为空时不再匹配任意客户端。阶段 3-6（voice_router/provider 大函数拆分、音乐后端公共骨架、配置改常量、SessionLocks/guard 并发收敛）留待后续任务。
