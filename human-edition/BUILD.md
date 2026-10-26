# Yousj Browser 人用版 · 打包说明

在你的 Windows 笔记本上按下面三步走，就能得到一个能直接用的 `Yousj Browser.exe`
（实际是文件夹 + 桌面快捷方式，绿色免安装）。

## 准备

- 一台 Windows 10/11 电脑（64 位）
- 能上网（要下载约 130MB 的浏览器基座 + uBlock）

## 步骤

### 1. 拿到基座下载直链

打开 https://github.com/win32ss/supermium/releases，找到最新版本里名字带 **win64**
的 **.zip** 包（不是 setup.exe），右键复制链接地址。

> 为什么用 Supermium：它是 Chromium 的分支，**保留了 Manifest V2 扩展支持**，
> 完整版 uBlock Origin 才能装；普通 Chrome 2026 年已经彻底移除了 MV2。

### 2. 运行一键打包脚本

把本仓库的 `human-edition/` 整个文件夹拷到 Windows 电脑上，打开 PowerShell，
进到 `human-edition\scripts` 目录，执行：

```powershell
.\Package-Windows.ps1 -SupermiumZipUrl "粘贴第 1 步复制的直链"
```

脚本会自动做完这些事：

- 下载并解压 Supermium 基座到 `YousjBrowser/`
- 预装 **Yousj 起始页**（新标签页，扩展 ID 固定）
- 从 GitHub 自动拉取最新 **uBlock Origin 完整版**（MV2）并预装
- 写入默认配置：10 个收藏书签、主页=新标签页、**关闭浏览器登录入口**
- 在桌面创建 **Yousj Browser** 快捷方式（用独立用户数据目录，不污染你别的浏览器）

### 3. 启动验证

双击桌面 **Yousj Browser**，检查：

- [ ] 新标签页中间是 **Yousj** 五彩大字，下面搜索栏（无默认文字）
- [ ] 搜索栏右侧有**放大镜**按钮；原来相机的位置是**搜索引擎切换**（Google/Bing/Brave/百度/DuckDuckGo）
- [ ] 没有麦克风图标；背景纯黑
- [ ] 搜索栏下方是 10 个收藏（YouTube、Gmail、GitHub、X、拼多多、哔哩哔哩、F1、Ferrari、腾讯体育、DeepSeek）
- [ ] 地址栏输入 `chrome://extensions`，能看到 **uBlock Origin** 和 **Yousj 起始页** 都是启用状态
- [ ] 打开 [Chrome 应用商店](https://chromewebstore.google.com/) 可以直接装插件；也可以把下载好的 `.crx` 文件拖进 `chrome://extensions` 页面安装（先开右上角"开发者模式"）

## 你的要求对照表

| 要求 | 落实 |
|---|---|
| EXE 格式、人能用的浏览器 | ✅ 基于 Chromium，Windows 原生 |
| 简洁，顶部 Yousj 大字、谷歌式排版 | ✅ Yousj 起始页 |
| 搜索栏无默认文字、有放大镜 | ✅ |
| 无麦克风 | ✅ 已去掉 |
| 相机换成选择搜索引擎 | ✅ 下拉切换 5 个搜索引擎 |
| 收藏列表我来填 | ✅ 10 个已填好 |
| 不要登录功能 | ✅ `signin.allowed=false`，无登录入口 |
| 可以有插件、必须显示图片 | ✅ 图片正常渲染（背景默认为纯黑） |
| 像谷歌一样导入外部插件 | ✅ Chrome 应用商店直装 + .crx 拖入安装 |
| 支持 uBlock 完整版 | ✅ MV2 的 uBlock Origin 预装启用 |
| 插件来自谷歌插件商城 | ✅ 商店直装 |

## 常见问题

- **脚本提示目录已存在**：加 `-Force` 参数覆盖，或删掉旧的 `YousjBrowser` 文件夹。
- **想换收藏夹**：改 `ntp-extension/bookmarks.js`（也是打包脚本书签数据的来源），重新跑一遍脚本即可；也可以直接在浏览器书签栏里手动改。
- **想换背景**：改 `ntp-extension/ntp.css` 里 `.bg` 的 `background`，重跑脚本。
- **uBlock 没生效**：确认 `chrome://extensions` 里它是启用状态；Supermium 保留 MV2，不会被 Chrome 那样强制禁用。
- **杀毒软件误报**：Supermium 是小众 Chromium 分支，偶尔被误报，加白名单即可。

## 目录结构

```
human-edition/
├── ntp-extension/        # Yousj 起始页扩展（Manifest V3）
│   ├── manifest.json     # 扩展 ID: pcbehpmgjpgjkjbpepplockbdeoacmpe（固定 key）
│   ├── ntp.html / ntp.css / ntp.js
│   ├── bookmarks.js      # 收藏夹数据
│   ├── bg.jpg            # 壁纸
│   └── icon-*.png
├── assets/               # 原始素材（壁纸、图标源文件）
├── scripts/
│   └── Package-Windows.ps1
└── BUILD.md              # 本文件
```

注意：`ntp-extension/ntp-key.pem` 是扩展签名私钥，**不要**随打包发布（脚本已自动排除）。
