# Agent Note: OpenWakeWord 前端模型退出仓库与发行归档
Status: implemented — 模型文件改为使用者自行下载，仓库只留清单、下载脚本与许可说明

## Problem

唤醒门需要三个 onnx，其中两个固定名前端模型（`melspectrogram.onnx`、`embedding_model.onnx`）原随仓库 `models/` 提供，并由 `reusable-build.yml` 打进 Release 归档。这两个文件是 openWakeWord 的预训练模型，按 CC BY-NC-SA 4.0 授权（上游代码 Apache-2.0，许可见 [openWakeWord README](https://github.com/dscripka/openWakeWord#license)）。其非商业性与相同方式共享条款与本项目 AGPL-3.0 的「不得对授予的权利附加进一步限制」（AGPL 第 10 条）无法同时满足：仓库或归档一旦携带这些文件，再分发者就同时面对两套互斥条款。项目 README 与 `docs/` 在此前把「发行包已含两个前端模型」当作现状写给使用者，事实来源因此也错了。

## Decision

`models/` 只保留 `README.md` 与两个下载脚本 `fetch-models.sh`（POSIX sh，Linux/macOS）、`fetch-models.ps1`（Windows），不再有任何 onnx；`.gitignore` 忽略 `/models/*.onnx`。两个前端模型的来源 URL 与 SHA-256 记在 `models/README.md`，脚本按表逐文件校验，校验失败删文件并非零退出。`reusable-build.yml` 只把这三个文本文件拷进归档的 `models/`，`AGENTS.md` 的目录说明、`docs/ci-cd.md`、`examples/config/settings.toml` 的 `[headless.wakeword]` 注释、`website/` 中英双语文档同步改为「自行下载」。`wakeword::read_model_file` 缺文件时的 anyhow 消息带上 `see models/README.md to download it`，启动期失败直接指向下载说明。

## Alternatives considered

**继续随仓库与归档提供，仅在文档标注 CC BY-NC-SA 4.0**：改动最小、开箱即用；代价是归档与仓库仍是一个混合许可的载体，再分发者要自行拆解哪一层受哪套条款约束，非商业限制也随发行传递，弃用。

**把模型作为 GitHub Release 的独立附件提供**：仓库干净、下载省一步；但附件与代码归档同样是项目主体发布的一部分，非商业与相同方式共享限制并未消失，只是换了宿主，弃用。

**只写文档、不加下载脚本**：少两个文件要维护；代价是两个固定文件名写死在代码里、没有选择余地，SHA-256 靠人手核对，弃用。脚本约 30 行，换掉的是最常见的「文件名拼错 / 下载被截断」两类支持成本。

**运行期自动下载**：省掉使用者一步；代价是二进制联网取模型、启动路径引入网络失败与代理分支，与「模型放宿主目录」的部署约定冲突，且 Docker 只读挂载下写不进去，弃用。

## Consequences

- 启用唤醒门的使用者多一步：跑一次下载脚本或手动 curl 两个文件，之后行为不变。唤醒词分类器本来就由使用者自备，这一步与它同类。
- 归档不再受 CC BY-NC-SA 4.0 约束，仓库与发行物统一为 AGPL-3.0；使用者下载模型后，模型文件本身的使用、修改与再分发由 CC BY-NC-SA 4.0 约束，`models/README.md` 的「许可」一节写明这一点。
- git 历史里仍留有这两个 blob（`refactor/wakeword-oww` 分支已推送）。要让历史也干净需重写该分支并强推，属另一项决定。
- 验证：`bash models/fetch-models.sh` 与 `pwsh -File models/fetch-models.ps1` 在模型齐备时输出 `ok:` 并跳过下载；`ReadAllBytes` 确认 `.sh` 为 LF，`sh` 与 `bash` 下均可执行。
