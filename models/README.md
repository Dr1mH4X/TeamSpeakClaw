# models/

唤醒门与本地 STT 的模型目录：`[headless.wakeword]` 启用时，程序从 exe 同级 `models/`（`crate::config::models_dir()`）按写死的文件名读取 onnx；Docker 部署挂载 `./models:/app/models:ro`，whisper.cpp 的 GGML 模型也放同一宿主目录。

exe 同级即运行时的二进制所在目录：`cargo run --release` 下二进制位于 `target/release/`，模型须放在 `target/release/models/`；发行归档解压后是 `<解压目录>/models/`。

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

本程序的推理基于 tract-onnx，只读取 ONNX。

## 许可

[openWakeWord](https://github.com/dscripka/openWakeWord) 的预训练模型由第三方独立提供，采用 [CC BY-NC-SA 4.0](https://creativecommons.org/licenses/by-nc-sa/4.0/) 许可证；该许可证的非商业使用限制仅适用于模型本身，与本项目代码的 AGPLv3 许可证相互独立、互不影响。如需商业部署，必须自行解决模型的商业授权问题，本项目不提供任何商业使用许可。；使用者自行下载后，模型文件本身的使用、修改与再分发受 CC BY-NC-SA 4.0 约束，商业部署需换成非 NC 授权的前端模型或自行训练。再分发这些模型时须保留 openWakeWord 的署名与许可说明。社区模型库下载的模型按其站点[商业许可](https://openwakeword.com/license)条款分发，商业部署前先核对该页条款。
