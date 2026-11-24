<#Requires -Version 5.1
<#
.SYNOPSIS
  把 Firefox（火狐）格式的 .xpi 插件转换成 Yousj Browser 可加载的解压目录。

.DESCRIPTION
  .xpi 本质是 zip 包，里面同样是 WebExtensions 的 manifest.json。
  本脚本做三件事：
    1. 解压 .xpi 到目标目录
    2. 从 manifest.json 里去掉 Firefox 专属键
       （browser_specific_settings / applications），Chromium 不认这些键
    3. 体检并报告兼容性：Manifest V2/V3、是否用了 Firefox 独有写法
       （background.scripts 等），让你装之前心里有数

  转换后这样安装：打开 chrome://extensions → 打开右上角「开发者模式」
  → 「加载已解压的扩展程序」→ 选转换出来的文件夹。

  注意：不是每个火狐插件都能转成功。用了 Firefox 独有 API 的插件
  （比如 browser.* 的某些接口）转完也跑不起来，这类插件请优先去
  Chrome 应用商店找它的 Chrome 版（大多数热门插件两边都有）。

.PARAMETER XpiPath
  .xpi 文件路径。

.PARAMETER OutDir
  输出目录，默认桌面下的 XpiConverted\<插件名>。

.EXAMPLE
  .\Convert-Xpi.ps1 -XpiPath "C:\Downloads\some-addon.xpi"
#>
param(
  [Parameter(Mandatory = $true)][string]$XpiPath,
  [string]$OutDir = ""
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $XpiPath)) { throw "找不到文件：$XpiPath" }
if ([string]::IsNullOrWhiteSpace($OutDir)) {
  $name = [IO.Path]::GetFileNameWithoutExtension($XpiPath)
  $OutDir = Join-Path ([Environment]::GetFolderPath("Desktop")) "XpiConverted\$name"
}
if (Test-Path $OutDir) { throw "输出目录已存在：$OutDir（先删掉或换个 -OutDir）" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

Write-Host "解压 $XpiPath ..."
Expand-Archive -Path $XpiPath -DestinationPath $OutDir -Force

$manifestPath = Join-Path $OutDir "manifest.json"
if (-not (Test-Path $manifestPath)) {
  throw "这个包里没有 manifest.json，不是标准的浏览器插件包。"
}
$manifest = Get-Content $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json

$notes = @()
foreach ($key in @("browser_specific_settings", "applications")) {
  if ($manifest.PSObject.Properties.Name -contains $key) {
    $manifest.PSObject.Properties.Remove($key)
    $notes += "已去掉 Firefox 专属键：$key"
  }
}
($manifest | ConvertTo-Json -Depth 100) | Out-File -FilePath $manifestPath -Encoding UTF8

Write-Host ""
Write-Host "插件：$($manifest.name)  版本：$($manifest.version)  Manifest V$($manifest.manifest_version)"
foreach ($n in $notes) { Write-Host " - $n" }
if ($manifest.manifest_version -eq 2) {
  Write-Host " - Manifest V2：Yousj Browser 的基座保留了 MV2 支持，可以尝试加载" -ForegroundColor Green
}
if ($manifest.background -and $manifest.background.scripts) {
  Write-Host " - 警告：用了 background.scripts（火狐/MV2 写法），MV3 环境下可能跑不起来" -ForegroundColor Yellow
}
if ($manifest.background -and $manifest.background.page) {
  Write-Host " - 警告：用了 background.page（火狐写法），Chromium 不支持，可能跑不起来" -ForegroundColor Yellow
}

Write-Host ""
Write-Host "转换完成：$OutDir" -ForegroundColor Green
Write-Host "安装：chrome://extensions → 开发者模式 → 加载已解压的扩展程序 → 选上面这个文件夹"
