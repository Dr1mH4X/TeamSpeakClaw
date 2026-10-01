# models/

OpenWakeWord 唤醒门的前端模型目录：`[headless.wakeword]` 启用时，程序从 exe 同级 `models/`（`crate::config::models_dir()`）读取这里的 onnx 文件；Docker 部署挂载 `./models:/app/models:ro`，whisper.cpp 的 GGML 模型也放同一宿主目录。

本目录随仓库与发行归档提供两个固定名前端模型，启用语音唤醒时还须放入一个唤醒词分类器，其文件名填在 `[headless.wakeword].model`。

| 文件 | 来源 | SHA-256 |
|---|---|---|
| `melspectrogram.onnx` | [openWakeWord v0.5.1 release](https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/melspectrogram.onnx) | `ba2b0e0f8b7b875369a2c89cb13360ff53bac436f2895cced9f479fa65eb176f` |
| `embedding_model.onnx` | [openWakeWord v0.5.1 release](https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/embedding_model.onnx) | `70d164290c1d095d1d4ee149bc5e00543250a7316b59f31d056cff7bd3075c1f` |

## 许可

这两个文件是 [openWakeWord](https://github.com/dscripka/openWakeWord) 的预训练模型，按 **CC BY-NC-SA 4.0**（署名—非商业性使用—相同方式共享）授权，上游仓库代码为 Apache-2.0。非商业限制只作用于模型文件本身：以本目录模型提供语音唤醒功能的部署与再发行不得用于商业用途，商业部署需自行训练替换分类器并确认前端模型的可用来源。发行归档中的 `models/` 目录同时附带本文件以满足署名要求。
