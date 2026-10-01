# 下载 OpenWakeWord 前端模型到本目录并校验 SHA-256（Windows）。
# 用法：pwsh -File models/fetch-models.ps1
$ErrorActionPreference = 'Stop'

$Dir = $PSScriptRoot
$BaseUrl = 'https://github.com/dscripka/openWakeWord/releases/download/v0.5.1'
$Models = @(
    @{ Name = 'melspectrogram.onnx'; Sha256 = 'ba2b0e0f8b7b875369a2c89cb13360ff53bac436f2895cced9f479fa65eb176f' }
    @{ Name = 'embedding_model.onnx'; Sha256 = '70d164290c1d095d1d4ee149bc5e00543250a7316b59f31d056cff7bd3075c1f' }
)

foreach ($Model in $Models) {
    $Path = Join-Path $Dir $Model.Name
    if ((Test-Path $Path) -and ((Get-FileHash $Path -Algorithm SHA256).Hash -eq $Model.Sha256)) {
        Write-Host "ok: $($Model.Name)"
        continue
    }
    Write-Host "download: $($Model.Name)"
    Invoke-WebRequest -Uri "$BaseUrl/$($Model.Name)" -OutFile $Path
    $Actual = (Get-FileHash $Path -Algorithm SHA256).Hash
    if ($Actual -ne $Model.Sha256) {
        Remove-Item $Path -Force
        throw "SHA-256 mismatch for $($Model.Name): expected $($Model.Sha256), got $Actual"
    }
    Write-Host "verified: $($Model.Name)"
}

Write-Host "front-end models ready in $Dir; 唤醒词分类器请自行放入并填在 [headless.wakeword].model（推荐 https://openwakeword.com/library 的 ONNX 导出）"
