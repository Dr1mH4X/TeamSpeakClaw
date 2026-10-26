# Agent Note: 网页表格转换与连接状态修复

Status: implemented

## Problem

网页表格是外部不可信 HTML 的一部分。转换器的后代选择器会把嵌套表格的行和单元格重复交给多个祖先表格处理，单元格渲染器重置深度还会让递归失去累计上限。该路径同步运行在工具执行任务中，资源放大不能依赖 HTTP 超时或转换后的模型输出上限收回。

表格的行组、行和单元格属于常规隐藏内容清理范围。只检查表格根节点会让 `hidden`、`aria-hidden` 或 `display:none` 的结构节点绕过 `should_skip`，使页面刻意隐藏的文本进入模型上下文。

语音配置全部关闭时，管理态 headless 运行时仍会成功创建。连接状态若以运行时创建成功为准，会把禁用的语音通道误报为已连接；启用语音时，后台桥创建成功也不代表服务、事件流和 actor 已同时就绪。

## Decision

`src/tools/web_fetch/html.rs` 的 `Renderer::render_table` 只遍历当前表格直接拥有的 `thead`、`tbody`、`tfoot` 或 `tr`，只遍历当前行直接拥有的 `th`、`td`。行组、行和单元格都经过 `should_skip`，嵌套表格的文本因此只进入它真正所属的单元格一次。

单元格内容由同一个 `Renderer` 渲染，表格结构逐层传递累计深度；嵌套表格不再创建独立渲染器。单元格结果压平为一行并转义 `|`，所以 GFM 不支持的嵌套表格格式退化为内层文本，同时保留 `MAX_DEPTH` 对跨表格递归的约束。

`style_declares_hidden` 按分号拆分样式声明，再按属性和值识别 `display:none`、`visibility:hidden` 与 `visibility:collapse`。字符串内容中提到这些值不会误删可见节点。

`src/adapter.rs` 的 `run_connected_session` 通过观察任务同步连接状态。语音状态只在 `BridgeReadiness` 的 Service、Stream、Actor 全部就绪时显示已连接；NapCat 状态跟随 `NapCatConnectionState` 的 Connected、Reconnecting 与 Down，并在会话结束时回收观察任务。

网页清洗的提供方归一化、公网抓取及 ACL 决策见[网页搜索与抓取的不可信内容渲染](../../implemented/feature/2026-10-18-web-fetch-and-untrusted-web-results.md)。

## Alternatives considered

**继续使用后代选择器并增加访问计数。** 计数可以阻止重复输出，但仍需遍历不属于当前表格的节点，且难以证明每层表格只处理自己的结构；直接按所属关系遍历更简单。

**直接丢弃所有表格。** 可以规避当前转换问题，但会丢失正常页面中的结构化数据；当前实现保留简单表格，并把嵌套表格的内层文本压平。

**只给 voice_connected 增加启用条件。** 可以修正关闭功能时的误报，但不能反映后台连接和断流重试状态；状态来源仍应与真实桥就绪情况一致。

**采用完整的第三方 HTML→Markdown 转换器。** 参考实现使用 Turndown 与 GFM 插件，能够按树遍历并统一移除隐藏节点；当前转换器已有固定 Markdown 方言，直接修正表格归属和累计深度不会新增依赖或改写其他标签的输出。

**为一次转换增加共享工作量和中间输出预算。** 单次结构化遍历已经把处理量限制在源树范围内，100000 字符源上限、20000 字符最终输出上限和 512 层深度上限提供固定资源边界；额外预算在出现其他结构性放大前不增加。

## Consequences

简单表格继续输出 GFM 表格。嵌套表格不输出为嵌套 Markdown 表格，内层文本在所属单元格中出现一次；`colspan` 与 `rowspan` 仍不展开。超过 `MAX_DEPTH` 的文档整体替换为固定 omission marker。

隐藏行组、行、单元格和子元素不再进入转换结果，样式判定只识别实际声明。连接状态不再由运行时对象或外层会话生命周期推断，旧 NapCat 连接代次也不能覆盖新连接状态。

## Testing

`tools::web_fetch::html::tests` 覆盖简单表格、10 层嵌套表格只出现一次内层文本、隐藏行组/行/单元格/子节点、CSS 文本不被误判为隐藏声明，以及深层嵌套表格触发共享深度限制。`adapter::lifecycle::tests` 覆盖语音桥聚合就绪状态通知；全量测试和 clippy 质量门通过。
