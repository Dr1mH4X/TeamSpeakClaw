# models/

唤醒门与本地 STT 的模型目录：`[headless.wakeword]` 启用时，程序从 exe 同级 `models/`（`crate::config::models_dir()`）按写死的文件名读取 onnx；Docker 部署挂载 `./models:/app/models:ro`，whisper.cpp 的 GGML 模型也放同一宿主目录。

本目录提供模型清单与下载脚本，不含模型文件；仓库与发行归档均不携带任何 onnx 或 GGML 模型，全部由使用者自行下载到本目录，理由见「许可」。

## 前端模型

`[headless.wakeword]` 需要两个固定名前端模型：

| 文件 | 来源 | SHA-256 |
|---|---|---|
| `melspectrogram.onnx` | [openWakeWord v0.5.1 release](https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/melspectrogram.onnx) | `ba2b0e0f8b7b875369a2c89cb13360ff53bac436f2895cced9f479fa65eb176f` |
| `embedding_model.onnx` | [openWakeWord v0.5.1 release](https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/embedding_model.onnx) | `70d164290c1d095d1d4ee149bc5e00543250a7316b59f31d056cff7bd3075c1f` |

下载脚本按上表逐文件校验 SHA-256，已存在且校验通过的文件跳过，可在本目录直接重复执行：

```bash
bash models/fetch-models.sh
```

```powershell
pwsh -File models/fetch-models.ps1
```

手动下载等价：

```bash
curl -fL -o models/melspectrogram.onnx https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/melspectrogram.onnx
curl -fL -o models/embedding_model.onnx https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/embedding_model.onnx
```

## 唤醒词分类器

第三个 onnx 是唤醒词分类器，文件名填在 `[headless.wakeword].model`，由使用者自备：取自 openWakeWord 的[预训练模型](https://github.com/dscripka/openWakeWord#pre-trained-models)，或按上游 notebook 自训练。

## 许可

两个前端模型与使用者在 openWakeWord 侧取得的预训练分类器，均为 [openWakeWord](https://github.com/dscripka/openWakeWord) 的预训练模型，按 [CC BY-NC-SA 4.0](https://creativecommons.org/licenses/by-nc-sa/4.0/)（署名—非商业性使用—相同方式共享）授权，上游仓库代码为 Apache-2.0。该许可的非商业与相同方式共享条款与本项目的 AGPL-3.0 无法同时满足，因此这些文件不由本仓库与发行归档提供；使用者自行下载后，模型文件本身的使用、修改与再分发受 CC BY-NC-SA 4.0 约束，商业部署需换成非 NC 授权的前端模型或自行训练。再分发这些模型时须保留 openWakeWord 的署名与许可说明。
