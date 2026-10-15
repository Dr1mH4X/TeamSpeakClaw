# Agent Note: TS 调用者身份键：准入键与会话键分离

Status: implemented

## Problem

语音桥文本的准入（容量票据 + 会话锁）必须在事件循环里同步完成，而 caller 的身份要查服务器（`clientlist` + `clientinfo`），因此准入只能拿事件自带的 UID。桥接事件在 `notifytextmessage` 省略 `invokeruid` 且 tsclient 的 clid→ClientInfo 缓存没有该客户端时会退成 `clid:<id>`：这个缓存只由 `notifycliententerview` 写入、每次重连清空，重连后已经在线的客户端不会再触发进入事件，就一直不在缓存里。

`SessionSource` 同时是串行锁键与历史键，于是这个回退键把两件事绑在一起：同一个 clid 的不同占用者共用一份历史（clid 会被复用），同一个人的事件一旦一次带 UID、一次不带，就分成两条票据链、两把锁、两份历史。三条入口原本各行其是：音频用解析出的真实 UID，语音桥文本用事件 UID 或 clid 回退，纯文本路由直接用事件 UID——空串即键，所有拿不到 UID 的发送者共用一份历史与一把锁。

## Decision

`router/turn.rs` 收下唯一一份规则：`ts_caller_clid_uid(clid)`、`ts_caller_admission_uid(event_uid, clid)`（事件 UID，空则 `clid:<id>`）、`ts_caller_session_uid(resolved_uid, admission_uid)`（解析出的真实 UID 优先，空才退回准入键，绝不落空串）。三条入口共用这三枚函数。

语音桥文本：`prepare_chat` 的票据与会话锁仍用准入键（到达顺序修复要求它在解析之前取定）；`resolve_chat_caller` 的 `caller_uid`——即 `build_llm_request` 建出的 `SessionSource`，历史键——改用同一次 `clientinfo` 快照里的真实 UID，因为回复发给的就是这个 clid 当前的占用者。音频：`resolve_caller_from_audio` 用 `ts_caller_session_uid(caller.uid, ts_caller_clid_uid(clid))`，名单 UID 为空时不再落空串。纯文本路由：票据用 `ts_caller_admission_uid`，`handle_message` 把 `get_client_channel_group_id` 换成同一次 `clientinfo` 的 `get_client_channel_group_and_uid`，历史键用解析出的 UID，不增加查询次数。

## Alternatives considered

- **不解析，统一用 `clid:<id>`**：clid 会被复用，等于把下一个占用者拼进上一任的历史；也丢掉「同一人换 clid 仍同会话」的语义。
- **UID 缺失的排队聊天直接拒收**（评审建议）：键空间只剩真实 UID，规则最少，但机器人重连后「已在别的频道、且通知不带 `invokeruid`」的客户端消息会被静默丢掉，对聊天机器人是可感知的退化。
- **把解析挪进事件循环，让准入直接用真实 UID**：解析要 await 两次服务器查询，会挡住事件循环（音频事件与取消都排在同一条广播流上），代价高于收益。
- **各路径各写一份回退**：三处必然漂移（纯文本路由的空串键就是证据），因此收进 `router/turn.rs`。

## Consequences

- 所得：历史不再挂在可复用的 clid 上，同一人的事件无论带不带 UID 都落同一份历史；三条入口的键规则统一在一处。
- 代价：UID 缺失时准入键与历史键不同名——`SessionSource` 从「锁与历史同一身份」变成两个用途各取一份。同一个 uid 的人在缺 UID 状态下换 clid，理论上会并发写同一份历史；未找到真实触发路径。
- 验证：反向验证（把 `caller_uid` 改回准入键）让 `chat_resolution_uses_the_resolved_uid_without_an_event_uid` 与 `chat_admission_falls_back_to_clid_while_the_session_uses_the_resolved_uid` 失败（left `clid:7`、right `replacement-user`）；`cargo fmt`、`cargo clippy --all-targets --offline --locked -- -D warnings`、`cargo test --all-targets --offline --locked`（392 用例全绿）。

相关：[同会话回合按到达顺序取锁（到达顺序票据）](../../implemented/bug-fix/2026-10-04-turn-arrival-order-tickets.md) 记录了票据链与准入在事件循环内的位置，本轮的准入键必须沿用那条修复留下的取定时机。