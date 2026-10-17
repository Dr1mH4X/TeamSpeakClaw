# Agent Note: 联网搜索服务显式选择

Status: implemented

## Problem

联网查询需要可选的搜索服务，同时保留既有部署的服务选择与 ACL。不同服务的参数能力不一致，统一暴露所有参数会让模型误以为每个服务都支持结果条数与搜索深度控制。

## Decision

`web_search.provider` 使用枚举选择 Exa 或 Parallel，缺省为 Exa，未知值在配置加载时失败。`ToolRegistry` 按配置创建 `WebSearch`，其参数 schema 与选中的服务一致。Parallel 只暴露 `query`，将同一查询映射到匿名 Search MCP 的 `objective` 与 `search_queries`；服务端管理 Fast 模式的搜索设置。

工具名、ACL 与成功结果的 `status`、`result` 字段保持一致。Parallel 的 HTTP、JSON-RPC 与工具级错误沿 `Result` 返回，注册表将其转成模型可见的失败文案；请求复用 25 秒超时客户端，调用取消时随 future 丢弃，不启动后台任务。

## Alternatives considered

**由模型为每次调用选择服务。** 这会把部署者的服务选择交给模型，还扩大参数与权限的解释范围，因此服务选择保留在配置中。

**复用 Exa 的全部参数。** Parallel 匿名端点不接受 `numResults`、`type` 等调用级搜索设置，静默忽略会误导模型；Parallel schema 只列出支持的参数，执行时也拒绝额外字段。

**失败时自动回落到 Exa。** 这会将查询发送给部署者未选中的服务，且掩盖限额或网络故障，因此失败直接反馈给模型。

## Consequences

部署者切换服务后需重启进程，已有配置与示例仍选择 Exa。Parallel 搜索无需密钥，但受匿名服务限额约束。用户设置与 ACL 用法归属[用户配置指南](../../../../website/docs/configuration.md#联网搜索)。

定向测试通过注册表发送本地 HTTP 请求，检查匿名请求、参数映射、多个文本块、JSON 与 SSE、协议和 HTTP 错误、超时与取消。联网测试默认忽略，可显式运行 `cargo test --locked parallel_live_registry_search -- --ignored --nocapture`。
