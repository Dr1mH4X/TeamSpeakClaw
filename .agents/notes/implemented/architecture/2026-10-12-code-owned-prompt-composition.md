# Agent Note: 提示词改为代码内有序段落装配

Status: implemented

## Problem

模型可见提示词是一份用户配置文件：`config/prompts.toml` 的 `[system].content` 是一个整块文本，`[tts].style_prompt` 是 TTS 事实却住在提示词文件里。由此带来四个具体问题。

一是默认值分叉：`SystemPrompts::default()` 的兜底是 `You are a helpful TeamSpeak assistant powered by LLM.`，与 `examples/config/prompts.toml` 里那份中文提示词完全无关；文件缺失时程序直接报错退出，用户永远见不到那个兜底，它只是一个不会被执行的分支。

二是装配没有结构：整块文本靠手写换行组织，`{date}` 占位符的替换藏在 `LlmEngine::build_context_base`（`src/llm/engine.rs`），引擎因此兼管提示词渲染，而引擎本来只该把提示词放进 system 角色。

三是运行期事实三处各写一遍：`ts_router`、`voice_router`、`nc_router` 各自用 `format!` 拼 `invoker: {...}\nOnline: ...`，语音与文本两条 TS 路径是同一段代码的两个副本。

四是服务端事实被用户配置覆盖：改行为要改配置文件，`.github/workflows/reusable-build.yml` 还要把 `prompts.toml` 打进 Release 包，`docs/`、`website/` 中英镜像、issue 模板都得跟着维护这份本不该外露的配置面。

## Decision

新增 `src/prompt.rs` 与 `src/prompt/{sections,context}.rs`，作为模型可见提示词的唯一归属地，结构对齐参考项目 deepseek-harness 的 system-prompt 组成方式。

- `PromptSection { name, order, text }`：第一方段落带显式位置，装配时按 `(order, name)` 升序拼接，位置之间留出至少 10 的插入空档；段落文本为空的直接被丢弃。`SECTIONS`（`src/prompt/sections.rs`）当前九段：`identity -1000`、`persona 0`、`reply_behavior 100`、`default_help 300`、`refusal 350`、`formatting 400`、`tone 450`、`language_and_time 500`、`confidentiality 900`。工具用法指引不作为段落存在：`send_message` 的跨平台路由说明只在工具自己的 `description` 与参数 `description` 里（`src/tools/communication.rs`），system 段落不重复一遍，也就不需要在 ACL 过滤掉该工具时把段落自抑制。
- `{{name}}` 变量在装配期严格解析（`src/prompt.rs` 的 `interpolate`）：未注册、未赋值、名字不合法或引用残缺一律 `Err`，不发出残缺提示词。当前只注册 `date`（`PromptVariables::now()` 取本地 `YYYY-MM-DD`）。
- `system_prompt()` 是唯一的 system 装配入口；`render(&[PromptSection], &PromptVariables)` 单独可测。
- 运行期事实不再进 system 提示词：`RuntimeContext` 按命名有序字段（`caller 100`、`online 200`）拼成文本，`ts_context` / `nc_context` 是 TS 与 NapCat 两条路径的唯一措辞处；文本与改动前逐字节一致，仍作为同一回合 user 消息里的 `runtime_context` 字段。
- 装配时机的唯一归属是回合骨架：`TurnRequest::run`（`src/router/turn.rs`）在拿到会话后调一次 `prompt::system_prompt()`，装配失败即以 `TurnError::Failed` 返回、不发起模型请求；`TurnRequest` 不再有 `system_prompt` 字段，三个调用点不再接触提示词。引擎侧 `build_context_base` 只把收到的文本放进 system 角色，`{date}` 替换随之删除。
- TTS 风格提示改为常量 `prompt::TTS_STYLE_PROMPT`，由 `OpenAiSpeechProvider::new` 的调用点传入（`src/router/voice_router.rs`）；`src/adapter/headless/speech.rs` 仍是通用 TTS 客户端，不认识提示词归属。
- 配置面收缩：`config/prompts.toml`、`examples/config/prompts.toml`、`src/config/prompts.rs`、`PromptsConfig` 与 `Arc<PromptsConfig>` 装配链全部删除，`AppConfig::load_all()` 返回 `(AppConfig, AclConfig)`，Release 打包只带 `settings.toml` 与 `acl.toml`。

段落文本与 `examples/config/prompts.toml` 的原 `[system].content` 对照等价：去掉 `回复要求：` 这个小标题，`cross_platform` 那一条移交给工具 `description` 承载，其余文字一字未改。

## Alternatives considered

- **保留文件、把默认值填成完整中文提示词**：仍是用户配置面，`prompts.toml` 的存在本身就要求文档、Release 打包、issue 模板各维护一遍；用户改坏提示词后没有回退路径，这正是要找的东西没了。
- **把提示词常量放进 `src/config/`**：`src/config/AGENTS.md` 的规则是「只暴露用户真正需要的设置」，一个不可配置的提示词常量住在 config 模块里会让归属判断失效；模型可见文本与配置加载是两回事。
- **照搬参考项目的注册表 + 作用域覆盖 + assembly waterfall + `complete` 段落**：那套机制为多 agent 作用域遮蔽与迁移期兼容而存在；本项目单 agent、单部署，没有同名段落需要遮蔽，注册表只会变成一层没有第二实现的间接。
- **沿用引擎里的 `{date}` 替换**：把渲染留在引擎意味着 prompt 模块只管一半，段落仍然依赖引擎做二次处理；变量在装配期一次解析完，引擎回到「只放 system 角色」。
- **把日期移到运行期上下文，让 system 前缀不随日期变化**（参考项目的 KV cache 取向）：信息位置会变，本轮不追求缓存收益，保留原位置更省事；若日后要做前缀复用，`PromptVariables` 与 `RuntimeContext` 已是两个可独立挪动的输入。
- **TTS 风格提示移入 `settings.toml` 的 `[headless.tts]`**：能保住用户可调性，但那是提示词文本，放回配置面等于把刚删掉的配置项换个文件重现；本轮选择常量。
- **保留 `TurnRequest.system_prompt` 字段、由调用点传常量**：三个调用点会各自持有一份装配结果（`voice` 已经要 clone），装配时机与失败处理也会散开；放进 `run()` 一处装配，失败即整回合失败。

## Review 后的三处收敛

- **装配失败不再静默**：`TurnRequest::run` 的提示词装配失败与后端失败共用 `fail_with_notice`——先尽力送出失败说明，再返回 `TurnError::Failed`。此前它直接返回 `Failed` 而不发文案，违反了该变体的契约（「失败说明已尽力送达」），TS 与 NapCat 调用点只记日志，用户收不到任何回复；参考实现遇到装配异常同样是失败整个回合并把错误摊到界面上，不吞。
- **引用外的 `}}` 是普通文本**：扫描只看 `{{`，单独的 `}}` 原样进入提示词；这条与参考实现一致（`packages/core/system-prompt/src/index.ts` 的分隔符扫描，其注释写明「否则就是普通散文」），已用 `a_closing_delimiter_outside_a_reference_is_literal_text` 钉住，免得日后被当成漏洞「修」掉。
- **开引用缺闭合仍然报错**：这里比参考实现严格。参考实现把没有后续 `}}` 的 `{{` 也当普通散文，因为它的段落可以由插件与任意散文中来；我们的段落是代码常量，残缺引用只可能是笔误，FAILFAST 比悄悄发出走样的提示词合适。差异是有意保留的，`interpolate` 的文档注释写明了理由。
- **日期用例不再看表**：`PromptVariables::on(&str)` 让 `now()` 与用例共用同一构造路径，用例用固定日期渲染并断言，消除渲染与期望值跨零点分别取 `now()` 的偶发失败。

## Consequences

- 所得：提示词是编译期资产，改动要重新构建；装配有结构可查——新增段落插进 `SECTIONS` 并分配位置即可，位置冲突、名字重复、变量未解析都由 `src/prompt.rs` 的单测挡住（`first_party_sections_are_spaced_and_named_uniquely`、`system_prompt_resolves_every_variable_and_keeps_section_order`、`sections_render_in_order_and_blank_lines_separate_them`、`unknown_or_malformed_variables_fail_assembly`、`a_closing_delimiter_outside_a_reference_is_literal_text`，上下文两个用例在 `src/prompt/context.rs`）。运行期事实的措辞只剩 `ts_context` / `nc_context` 两处。
- 线上文本变化（有意）：段落之间统一以空行分隔，`回复要求：` 小标题消失，`{date}` 换成 `{{date}}`，跨平台路由说明从 system 段落移到 `send_message` 的 `description` 与参数 `description`；system 消息内容因此与旧版不完全逐字节相同，语义与信息量不变。
- 代价：用户不能再调整机器人人设与 MiMo TTS 语音风格，改了 `prompts.toml` 的用户其文件被静默忽略（程序不再读取该路径）；提示词调整需要改动源码并重新构建，也不再能靠配置在多个部署间分化人设。
- 验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`（351 用例全绿，含新增 8 个 prompt 用例）。段落全文与旧 `prompts.toml` 的等价性用一次性脚本比对（除 `回复要求：` 小标题与已移交工具的 `cross_platform` 一条外逐字一致）。实机需确认 TS 文本、NapCat 与语音三条路径的 system 消息与运行期上下文快照与旧版一致。

相关：[LLM 回合骨架收敛为 `router/turn.rs` 单一归属](../../implemented/architecture/2026-10-03-llm-turn-module.md) 中 `TurnRequest` 的字段集在本轮少了一个 `system_prompt`。