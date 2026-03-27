---
sidebar_position: 2
---

# 下载与安装

## 1. 下载

请前往 GitHub Releases 页面下载最新版本的 TeamSpeakClaw：

[**下载最新版本**](https://github.com/Dr1mH4X/TeamSpeakClaw/releases/latest)

根据您的操作系统选择合适的文件（Windows, Linux, macOS）。

## 2. 安装

TeamSpeakClaw 是一个独立的二进制应用程序，无需复杂的安装过程。

1. 将下载的压缩包解压到一个文件夹中。
2. 确保您拥有该文件夹的读写权限。

## 3. 生成配置

在命令行运行以下命令，自动生成默认配置文件：

```bash
./teamspeakclaw --config generate
```

这将在 `config/` 目录下创建三个配置文件：

- `settings.toml` — 核心设置（连接、LLM、机器人行为）
- `acl.toml` — 权限控制规则
- `prompts.toml` — 系统提示词与错误消息

## 4. 编辑配置

您可以使用文本编辑器手动修改配置文件，或使用内置的交互式向导：

```bash
./teamspeakclaw --config edit
```

向导将引导您输入 TeamSpeak ServerQuery 账号密码以及 LLM API Key 等信息。

详细配置说明请参考 [配置指南](/docs/configuration)。

## 5. 启动服务

配置完成后，直接运行程序：

```bash
./teamspeakclaw
```

如果配置正确，机器人将连接到您的 TeamSpeak 服务器并开始监听事件。
