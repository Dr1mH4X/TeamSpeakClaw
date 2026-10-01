#!/bin/sh
# 下载 OpenWakeWord 前端模型到本目录并校验 SHA-256（Linux/macOS）。
# 用法：sh models/fetch-models.sh
set -eu

DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BASE_URL="https://github.com/dscripka/openWakeWord/releases/download/v0.5.1"

hash_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

fetch() {
  name="$1"
  expected="$2"
  path="$DIR/$name"
  if [ -f "$path" ] && [ "$(hash_of "$path")" = "$expected" ]; then
    echo "ok: $name"
    return 0
  fi
  echo "download: $name"
  curl -fL --retry 3 -o "$path" "$BASE_URL/$name"
  actual="$(hash_of "$path")"
  if [ "$actual" != "$expected" ]; then
    rm -f "$path"
    echo "SHA-256 mismatch for $name: expected $expected, got $actual" >&2
    return 1
  fi
  echo "verified: $name"
}

fetch melspectrogram.onnx ba2b0e0f8b7b875369a2c89cb13360ff53bac436f2895cced9f479fa65eb176f
fetch embedding_model.onnx 70d164290c1d095d1d4ee149bc5e00543250a7316b59f31d056cff7bd3075c1f

echo "front-end models ready in $DIR; 唤醒词分类器请自行放入并填在 [headless.wakeword].model（推荐 https://openwakeword.com/library 的 ONNX 导出）"
