<#Requires -Version 5.1
<#
.SYNOPSIS
  Yousj Browser 人用版一键打包脚本（在 Windows 上运行）。

.DESCRIPTION
  1. 下载 Supermium（Chromium 分支，保留 Manifest V2，完整版 uBlock Origin 可用）zip 包并解压
  2. 预装扩展：Yousj 起始页（新标签页）+ uBlock Origin 完整版（MV2）
  3. 写入默认配置：书签栏（10 个收藏）、主页=新标签页、关闭浏览器登录入口
  4. 在桌面创建 "Yousj Browser" 快捷方式（独立用户数据目录，不污染其他浏览器）

.PARAMETER SupermiumZipUrl
  从 https://github.com/win32ss/supermium/releases 复制的 64 位 zip 包直链。
  例：-SupermiumZipUrl "https://github.com/win32ss/supermium/releases/download/132.0.6834.223-R7/supermium_132_win64.zip"
  （以 releases 页面实际文件名为准，选名字里带 win64 的 .zip）

.PARAMETER InstallDir
  安装目录，默认脚本所在目录下的 YousjBrowser。

.EXAMPLE
  .\Package-Windows.ps1 -SupermiumZipUrl "https://github.com/win32ss/supermium/releases/download/<版本>/supermium_<版本>_win64.zip"
#>
param(
  [string]$SupermiumZipUrl = "",
  [string]$InstallDir = (Join-Path $PSScriptRoot "YousjBrowser"),
  [switch]$Force
)

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$NtpExtId    = "pcbehpmgjpgjkjbpepplockbdeoacmpe"   # Yousj 起始页（本仓库 ntp-extension/）
$UblockExtId = "cjpalhdlnbpafiamejdnhcphjbkeiagm"   # uBlock Origin（众所周知的固定 ID）

# 收藏夹（与 ntp-extension/bookmarks.js 保持一致）
$Bookmarks = @(
  @{ name = "YouTube";   url = "https://www.youtube.com" },
  @{ name = "Gmail";     url = "https://mail.google.com" },
  @{ name = "GitHub";    url = "https://github.com" },
  @{ name = "X";         url = "https://x.com" },
  @{ name = "拼多多";     url = "https://www.pinduoduo.com" },
  @{ name = "哔哩哔哩";   url = "https://www.bilibili.com" },
  @{ name = "Formula 1"; url = "https://www.formula1.com" },
  @{ name = "Ferrari";   url = "https://www.ferrari.com" },
  @{ name = "腾讯体育";   url = "https://sports.qq.com" },
  @{ name = "DeepSeek";  url = "https://chat.deepseek.com" }
)

function Download-File([string]$Url, [string]$Dest) {
  Write-Host "下载: $Url"
  Invoke-WebRequest -Uri $Url -OutFile $Dest -UseBasicParsing
  Write-Host "已保存: $Dest"
}

# ---------- 1. 基座 ----------
if ([string]::IsNullOrWhiteSpace($SupermiumZipUrl)) {
  Write-Host ""
  Write-Host "请先去 https://github.com/win32ss/supermium/releases 复制 64 位 .zip 包的直链，" -ForegroundColor Yellow
  Write-Host "然后这样运行：" -ForegroundColor Yellow
  Write-Host '  .\Package-Windows.ps1 -SupermiumZipUrl "粘贴直链"' -ForegroundColor Cyan
  Start-Process "https://github.com/win32ss/supermium/releases"
  exit 1
}

if ((Test-Path $InstallDir) -and -not $Force) {
  Write-Host "安装目录已存在：$InstallDir（加 -Force 覆盖）" -ForegroundColor Yellow
  exit 1
}
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

$zipPath = Join-Path $env:TEMP "supermium-base.zip"
Download-File $SupermiumZipUrl $zipPath
Write-Host "解压基座到 $InstallDir ..."
Expand-Archive -Path $zipPath -DestinationPath $InstallDir -Force

$chromeExe = Get-ChildItem -Path $InstallDir -Filter "chrome.exe" -Recurse | Select-Object -First 1
if (-not $chromeExe) { throw "解压后找不到 chrome.exe，请检查 zip 包是否正确。" }
Write-Host "浏览器主程序：$($chromeExe.FullName)"

# ---------- 2. 扩展 ----------
$userData   = Join-Path $InstallDir "User Data"
$defaultDir = Join-Path $userData "Default"
$extRoot    = Join-Path $defaultDir "Extensions"
New-Item -ItemType Directory -Force -Path $extRoot | Out-Null

# 2a. Yousj 起始页（本仓库自带）
$ntpSrc = Join-Path $PSScriptRoot "..\ntp-extension"
$ntpDst = Join-Path $extRoot "$NtpExtId\1.0.0"
New-Item -ItemType Directory -Force -Path $ntpDst | Out-Null
Get-ChildItem -Path $ntpSrc -File |
  Where-Object { $_.Name -notin @("ntp-key.pem", "ntp-key.b64") } |
  Copy-Item -Destination $ntpDst -Force
Write-Host "已预装：Yousj 起始页 ($NtpExtId)"

# 2b. uBlock Origin 完整版（MV2，取最新 release 的 chromium 包）
Write-Host "查询 uBlock Origin 最新版本..."
$rel   = Invoke-RestMethod -Uri "https://api.github.com/repos/gorhill/uBlock/releases/latest" -UseBasicParsing
$asset = $rel.assets | Where-Object { $_.name -like "uBlock0_*_chromium.zip" } | Select-Object -First 1
if (-not $asset) { throw "在 uBlock releases 里没找到 chromium 包。" }
if ($asset.name -match "uBlock0_([\d\.]+)\.chromium\.zip") { $ubVer = $Matches[1] } else { $ubVer = "1.0.0" }
$ubZip = Join-Path $env:TEMP "ublock.zip"
Download-File $asset.browser_download_url $ubZip
$ubDst = Join-Path $extRoot "$UblockExtId\$ubVer"
New-Item -ItemType Directory -Force -Path $ubDst | Out-Null
Expand-Archive -Path $ubZip -DestinationPath $ubDst -Force
Write-Host "已预装：uBlock Origin $ubVer ($UblockExtId)"

# ---------- 3. 默认配置 Preferences ----------
function New-ExtSetting([string]$ExtDir) {
  $manifest = Get-Content (Join-Path $ExtDir "manifest.json") -Raw -Encoding UTF8 | ConvertFrom-Json
  return @{
    active_bit = $true
    manifest   = $manifest
    path       = $ExtDir
    state      = 1
  }
}

$bmChildren = @()
$i = 1
foreach ($b in $Bookmarks) {
  $bmChildren += @{ id = "$i"; name = $b.name; type = "url"; url = $b.url }
  $i++
}

$prefs = [ordered]@{
  account_info  = @()
  bookmark_bar  = @{ show_on_all_tabs = $true }
  bookmarks     = @{
    roots = @{
      bookmark_bar = @{ children = $bmChildren; name = "书签栏"; type = "folder" }
      other        = @{ children = @(); name = "其他书签"; type = "folder" }
      synced       = @{ children = @(); name = "移动设备书签"; type = "folder" }
    }
  }
  browser       = @{ show_home_button = $true }
  homepage      = "chrome://newtab/"
  homepage_is_newtabpage = $true
  signin        = @{ allowed = $false }   # 不要登录功能：禁用浏览器登录入口
  extensions    = @{
    settings = @{
      $NtpExtId    = (New-ExtSetting $ntpDst)
      $UblockExtId = (New-ExtSetting $ubDst)
    }
  }
}

New-Item -ItemType Directory -Force -Path $defaultDir | Out-Null
$prefsPath = Join-Path $defaultDir "Preferences"
($prefs | ConvertTo-Json -Depth 100 -Compress) | Out-File -FilePath $prefsPath -Encoding UTF8
Write-Host "已写入默认配置：$prefsPath"

# ---------- 4. 桌面快捷方式 ----------
$wshell   = New-Object -ComObject WScript.Shell
$shortcut = $wshell.CreateShortcut((Join-Path $wshell.SpecialFolders("Desktop") "Yousj Browser.lnk"))
$shortcut.TargetPath      = $chromeExe.FullName
$shortcut.Arguments       = '--user-data-dir="' + $userData + '"'
$shortcut.WorkingDirectory = $chromeExe.DirectoryName
$shortcut.Description     = "Yousj Browser"
$shortcut.Save()
Write-Host "已创建桌面快捷方式：Yousj Browser"

Write-Host ""
Write-Host "完成！双击桌面的 Yousj Browser 即可启动。" -ForegroundColor Green
Write-Host "- 新标签页：Yousj 起始页（搜索栏无默认文字、放大镜、搜索引擎切换、无麦克风）"
Write-Host "- 扩展：uBlock Origin 完整版已启用；Chrome 应用商店可直接装更多插件，也支持拖入 .crx 外部安装"
Write-Host "- 无账号登录入口；图片正常显示"
