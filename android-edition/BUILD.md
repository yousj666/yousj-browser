# Yousj 浏览器 · 安卓版（手机版）

基于 **GeckoView**（Mozilla 的可嵌入 Firefox 引擎）的安卓浏览器，原生支持 Firefox 插件（.xpi）。

## 功能（v1.0）

- GeckoView 138 引擎，arm64-v8a
- 与电脑版同一套起始页：闽江夜景 + Yousj 3D 活背景（视差/倒影）
- 地址栏（网址/搜索两用）、前进/后退/刷新、分享
- **插件系统**：内置 uBlock Origin 1.77.0；插件管理页可查看/启用/停用/卸载，
  并可从手机文件里安装任意 .xpi 插件
- 包名 `cc.yousj.browser`，固定签名（证书 SHA-256 e769a1f9…），versionCode 只升不降

## 构建

沙箱里构建走 `./build.sh`（普通环境用 `gradle :app:assembleRelease` 即可）。

沙箱特别说明：本环境的回环 TCP 被出口拦截层破坏，Gradle 守护进程握手必然失败
（已用最小 Java 套接字实验复现）。`build.sh` 的解法是绕开 `gradle` 脚本，
直接以与构建要求**完全一致的 JVM 参数**启动 GradleMain，使构建在启动器 JVM 内
进程内运行、不分叉守护进程；Kotlin 编译同样设为 `in-process` 策略。
JVM 参数取自 `gradle help --info` 输出的 "needs to match the parameters" 一行，
Gradle/JDK 版本变化时需重新对齐。

依赖版本说明：GeckoView 160 要求 compileSdk 37（需新版 AGP），当前工具链
（AGP 8.5.2 + compileSdk 35）下钉在 **138.0.20250517143237**；升级工具链后再跟进。

## 签名

发布钥 `signing/yousj-browser-release.jks`（alias `yousj-browser`），口令在
同目录 `passwords.txt`，均不入库。构建时经环境变量
`YOUSJ_STORE_PASSWORD` / `YOUSJ_KEY_PASSWORD` 注入（见 build.sh）。
