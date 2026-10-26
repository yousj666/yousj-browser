# Yousj Browser · 人用版（human-edition）

给人用的 Windows 浏览器，EXE 形态。技术路线：**Supermium（Chromium 分支）换壳定制**。

## 为什么不用自研内核

自研 Python 内核（yousj-js）跑不了 Chrome 扩展生态，更装不上完整版 uBlock Origin——
那些都需要真正的 Chromium/Blink 引擎。人用版和自研内核是两条线，互不干扰：
自研内核继续按原路线图走（当前暂停中），人用版直接站在 Chromium 肩膀上。

## 选型

- **基座**：Supermium（win32ss/supermium）——Chromium 分支，**保留 Manifest V2**，
  完整版 uBlock Origin 可装可用；Chrome 官方 2026 年已彻底移除 MV2。
- **起始页**：自研 `ntp-extension/`（Manifest V3，`chrome_url_overrides` 接管新标签页）。
- **打包**：`scripts/Package-Windows.ps1` 在 Windows 上一键完成下载、预装、配置。

## 打包看 BUILD.md
