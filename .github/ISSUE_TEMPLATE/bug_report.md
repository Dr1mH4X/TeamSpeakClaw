---
name: 报告错误
about: 报告可复现或可定位的异常，便于排查
---

### 模块定位

勾选主要落点（可多选），便于分派与排查：

- [ ] TeamSpeak 无头客户端 / 连接 / 重连
- [ ] NapCat (QQ) OneBot 11
- [ ] Voice bridge / 语音（STT、TTS、gRPC、音乐 bot 音频）
- [ ] LLM（OpenAI 兼容接口、上下文、工具调用）
- [ ] 技能（音乐 / 管理 / 信息查询 / 语音回放 / web_search 等）
- [ ] 权限 ACL
- [ ] 配置加载（settings / acl / prompts）
- [ ] 其他：

关键路径或报错涉及的文件/函数（若能从日志看出）：

```
例如 src/adapter/headless/... 或 voice_router
```

### 错误描述

一句话说明现象是什么，不要写排查过程。

### 复现步骤

1.
2.
3.

触发条件（频道消息 / 私聊 / QQ / 语音指令 / 定时 / 启动时等）：

### 预期行为

### 实际行为

### 排查记录

便于他人接着查。没有的项写 N/A，不要整段空着。

- 是否稳定复现 / 偶发：
- 最近是否改过配置或升级版本：
- 重启后是否仍出现：
- 已尝试的步骤与结果：
- 日志中可检索的关键行（原文摘录，已脱敏）：

```
粘贴日志片段
```

### 环境信息

- 操作系统：（例如 Windows 11 25H2 / Ubuntu 22.04 / Docker / MacOS 27.0）
- TeamSpeakClaw 版本：（例如 v0.6.0 或 commit）
- 部署方式：（本机二进制 / Docker / 其他）
- TeamSpeak 服务器版本：
- NapCat：（未使用 / 版本）
- LLM Provider 与模型名：（例如 DeepSeek deepseek-chat）
- 音乐后端：（未使用 / TS3AudioBot / TSMusicBot / NeteaseTSBot）

### 配置信息

不要上传含密钥的完整文件。只贴与问题相关的结构或键名：

- 相关配置项（名称与非敏感取值）：
- 若需完整对照，自行脱敏后粘贴 `settings.toml` / `acl.toml` / `prompts.toml` 中相关段落
- 日志：脱敏后摘录 `config` 目录下 `tsclaw-YY-MM-DD.log` 相关时间段
