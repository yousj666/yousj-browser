# Yousj Browser

A from-scratch browser engine, built for AI. Headless first.

No Chromium, no WebKit — the HTML tokenizer, tree builder and DOM are
hand-written. Three languages, each doing what it's best at:

| Layer  | Language | Job |
|--------|----------|-----|
| Engine core | **Rust** | HTML tokenizer, parser, DOM tree, C ABI |
| Hot paths   | **C**    | ASCII case-insensitive tag compare, void-element table, entity decoding |
| Glue        | **Python** | HTTP fetching, CLI, AI-friendly API (`ctypes` bindings) |

## v0.1 — what works

```bash
cd python
python3 -m yousj https://example.com
```

- Fetches a URL, parses HTML into a DOM, extracts `<title>`, visible text
  (script/style skipped, whitespace collapsed) and `<a href>` links.
- Handles: comments, doctype, quoted/unquoted attributes, character
  entities (`&amp;`, `&#65;`, `&#x41;`…), void elements, `<p>` auto-closing,
  raw-text `<script>`/`<style>`.

## Build

```bash
# needs: gcc, python3, rustup
cd engine && cargo build --release   # builds C + Rust -> libyousj_engine.so
cd ../python && python3 -m yousj <url>
```

## v0.2 — F12 devtools + web search

```bash
cd python
python3 -m yousj devtools https://example.com   # F12: Network/Elements/Console/Sources/Performance
python3 -m yousj search "Lightpanda browser"    # web search through our own engine
python3 -m yousj config engine bing             # switch search engine
```

**F12 for AI** (`yousj.devtools`): every HTTP request is logged to the
Network panel automatically; Elements shows an indented DOM tree from the
Rust engine; Console collects engine logs; Sources keeps raw HTML;
Performance reports fetch/parse timings.

**Search** (`yousj.search`): query goes to the configured engine, the
results page is fetched and parsed by *our* HTML engine (no third-party
scraping lib). Engine presets: `brave` (default, most bot-friendly),
`duckduckgo`, `google`, `bing`. AI agents can point at anything:

```python
from yousj import settings, search
settings.set_search_engine("google")                        # preset
settings.set_search_engine("https://example.com/s?q={q}")   # custom
search("hello", engine_name="bing")  # one-off override, not persisted
```

Parser fixes in v0.2: no more doubled `<html>` (real tag reuses the implied
one, attributes merged); character entities now decoded in attribute values
too (`href="/?x=1&amp;y=2"`); new `yousj_anchors` / `yousj_dom_tree` FFI.

## V5 — 自研 JS 引擎 yousj-js 公开

从零手写的 JavaScript 引擎（Rust，约 2 万行），此前为内部项目，V5 起正式公开并默认集成：

- **解释器 + 字节码 VM**：树遍历解释器与栈式 VM（约 70 种指令）双执行模式，可切换
- **语言特性**：生成器（含 `yield*`）、async/await、async 生成器、解构赋值（~90%）、类（含私有字段/静态块）、Proxy（get/set/has/apply/construct）、模板字面量、展开/剩余参数
- **标准库**：Map/Set/WeakMap/WeakSet、ArrayBuffer + TypedArray + DataView、Date、Intl.NumberFormat/DateTimeFormat
- **Web API**：WebSocket、Worker、localStorage/sessionStorage、URL/URLSearchParams、TextEncoder/Decoder、Blob、FormData、atob/btoa、setInterval
- **合规**：test262 `test/language` 通过率 36.6%（16,022/45,907），自有测试 447 全过
- **性能**：字符串驻留 + Fx 哈希，类反复创建场景峰值内存降 274 倍；调用密集型基准优化后快约 50%

构建（含 JS 引擎）：

```bash
./build-js.sh   # 编译 js-engine 为 rlib，再以 --features js 编译 engine
```

JS 可直接操作实时 DOM（`document.getElementById('t').textContent = 'hi'` 立即可见），支持事件监听与 `fetch`。

## V5 新功能：会话 / 表单 / 下载 / 代理 / 历史

```python
from yousj import fetch

# 登录会话（cookie 持久化）：登一次，下次直接用
# cookie 存在 ~/.config/yousj/cookies.txt（600 权限），自动加载/保存
doc = fetch("https://example.com/login")
form = doc.forms()[0]
home = form.fill({"user": "alice", "pass": "s3cret"}).submit()  # GET/POST 自动按 method

# 代理
from yousj import settings
settings.set_proxy("http://127.0.0.1:8080")  # 未设置时走 http_proxy/https_proxy 环境变量
settings.clear_proxy()

# 历史记录
from yousj import history
history.recent(10)  # [{"url", "title", "ts"}...]，存在 ~/.config/yousj/history.jsonl
```

```bash
python -m yousj download <url> [-o 文件名] [--yes]  # SSRF 安全校验，大文件流式落盘（默认 200MB 软上限，超限先确认）
python -m yousj history [n]                 # 查看访问历史
python -m yousj config proxy [url|off]      # 设置代理
python -m yousj config max-download-size [MB]  # 下载大小上限（默认 200）
```

说明：

- **Cookie**：标准 `http.cookiejar`（MozillaCookieJar），Secure/HttpOnly 按规范处理；`net.clear_cookies()` 登出全部站点。
- **表单**：`doc.forms()` 列出所有表单；`fill({...})` 按 name 填值（select 会校验选项、checkbox 支持 True/False、未知字段名直接报错）；`submit()` 按 method 做 GET/POST，返回新 Document（`.url` 为最终地址）。
- **下载**：走 `net.get` 同样的 SSRF 校验与跳转检查，但 body 流式写入磁盘（不占内存）。大小是**软上限**（默认 200MB，`config max-download-size <MB>` 可调），超限不直接掐断，而是先确认：
  - 已知大小（Content-Length）超限 → 下载前问：`确定要下载吗？该文件大于 200 MB。(确认下载/取消下载)`，确认后本次不限量下完；
  - 大小未知、中途超限 → 暂停问：`文件大小未知，已下载 X MB，是否继续？(确认下载/取消下载)`，取消则删掉已下载的部分文件。
  - CLI 在 TTY 下直接 prompt；管道/脚本等非 TTY 默认取消并提示加 `--yes` 跳过确认；Python API 可传 `on_oversize(url, 已知大小或None, 已下载字节) → True/False` 回调自定义确认逻辑。
- **代理**：`settings` 的 `proxy` 优先，其次环境变量；每次请求前重建 opener，生即生效。
- **历史**：`yousj.fetch()` 与表单提交自动记录（url、title、时间戳），最多保留 1000 条。

## Roadmap

- ~~v0.3: embed QuickJS~~ → 已被自研 yousj-js 取代（见上）
- v0.4: CSS parsing + box layout (towards a visible browser)
- Later: UI shell

## Security (SSRF protection)

Every URL fetched through `yousj.net` is validated first (`yousj.security`):

- only `http://` / `https://` — `file://`, `ftp://`, … are rejected
- hosts must resolve to public IPs — loopback, RFC1918, link-local
  (e.g. cloud metadata `169.254.169.254`), multicast and reserved ranges
  are blocked, including literal IPs in the URL
- every redirect hop is validated too (no 302 bypass to an intranet address);
  each hop is logged in the F12 Network panel
- fail closed: unverifiable URLs are not fetched
- response bodies are capped at 10MB (`net.get(..., max_bytes=...)`),
  counting gzip-decompressed size too (no decompression bombs)

Scraping your own intranet? Opt out explicitly:

```python
yousj.settings.set("allow_private_urls", True)
```

### Two-layer risk confirmation ("偏要进去")

A blocked URL isn't unvisitable — but the AI must walk through two warnings.
`yousj.security`:

```python
p1 = security.warn_first(url)
# {"text": "此网站危险，不建议访问。原因：…",
#  "choices": ["退出不访问（推荐）", "访问（不推荐）"]}
# AI chooses "访问（不推荐）" ->
p2 = security.warn_second(url)
# {"text": "确定要访问吗？如果出了事概不负责。",
#  "choices": ["不访问", "访问"]}
# AI chooses "访问" ->
token = security.confirm_visit(url)          # one-time bypass token
html = net.get(url, bypass_token=token)       # now it fetches
```

Layers can't be skipped (each step requires the previous one, 10-minute
window). The token is single-use, bound to the exact URL, expires after
10 minutes. A blocked redirect hop needs its own confirmation. The CLI
(`fetch` / `devtools`) runs the same two prompts interactively.

**DNS rebinding — fixed for direct connections (V5.1, fail-closed)**:
`yousj.net` performs exactly **one** DNS resolution per request hop,
SSRF-validates *every* returned IP (one bad IP rejects the whole host),
then pins the first valid IP and dials it directly. There is no second
lookup and no fallback to an unpinned connection: any validation or
resolution failure raises `SecurityError` and the request is aborted
(fail closed, never fail open). The HTTP Host header — and TLS SNI /
certificate hostname verification for https — still use the original
domain, so virtual hosting and cert checks behave as before. Residual
gaps: proxy mode (the proxy resolves the name, pinning is skipped) and
the explicit two-layer bypass flow (the user accepted that URL as-is;
the single resolution is still pinned, just without the public-IP
filter).

## AI 反馈

AI Agent 发现 bug、JS 语义偏差或解析异常，请走结构化反馈入口：
[🤖 AI 反馈](.github/ISSUE_TEMPLATE/ai-feedback.yml)（Issues → New issue → AI 反馈）。
填上版本、复现脚本、期望 vs 实际即可，人类用户同样欢迎使用。

## AI 浏览：元素清单 / 像素 / 鼠标 / 截图（本地开发中，未发布）

给外部 AI（以及浏览器内置 AI，见 `yousj.ai`，由主 Agent 维护）
用的操作接口。核心思想：AI 按编号操作元素，也能按像素操作鼠标，
并能截图“看到”页面。

```python
from yousj import fetch
from yousj import mouse

doc = fetch("https://example.com")
print(doc.format_elements())          # 编号清单：像素盒子 + ✓/✗可触摸
result = doc.activate(1, pace=False)  # 按编号点：返回跳到哪、变了什么
box = doc.element_box(1)              # {'x','y','width','height'} 或 None
shot = doc.screenshot(path="shot.png")  # 线框截图（PNG/SVG）

m = mouse.Mouse()
m.move_to_element(doc, 1)             # 鼠标自己移到元素上（真人轨迹）
m.click_element(doc, 1, activate=True)  # 移过去 + 点 + 附带 DOM 效果
m.drag(40, 100, 345, 100)             # 拖拽（滑块手势的路径）
print(m.position())                   # 鼠标当前像素位置
```

模块分工（`yousj.ai` / `settings.py` 的 BYOK 配置由主 Agent 负责，
不在本节范围内）：

| 模块 | 作用 |
| --- | --- |
| `yousj.interact` | 元素清单 `interactive_elements()`、点按 `activate()`、可触摸判断 |
| `yousj.layout` | 最小盒模型 `layout()`（Rust `yousj_layout`），输出像素盒子 |
| `yousj.mouse` | 模拟鼠标 `Mouse`：移动 / 点击 / 拖拽 / 元素定位 / 命中测试 |
| `yousj.human` | 真人节奏：随机延迟 `pace()`、真人轨迹 `human_path()` |
| `yousj.screenshot` | 线框截图：PNG（PIL）/ SVG，含鼠标标记与高亮框 |

CLI：`python -m yousj elements <url>`、`activate <url> <id>`、
`layout <url> [--width N]`、`mouse move|click|drag … [--dry-run]`、
`screenshot <url> [-o file] [--svg] [--full-page]`。

可触摸（touchable）定义：按下去有可观察效果（跳转、提交、触发动作、
状态变化）才算可触摸，按了什么都不发生的算不可触摸。静态判断规则：
链接（有 href）、按钮、表单控件、有对应控件的 label、带 onclick 的
元素 = 可触摸；disabled、没有静态点击处理的 role=button、什么都没有
的元素 = 不可触摸。**边界**：静态判断看不到脚本用 `addEventListener`
注册的处理函数，也分辨不出实际什么都不做的空处理函数——所以它是
“尽力而为的静态结论”，不是保证。 ground truth 是点下去之后
`activate()` 返回的 `changed` / `console` / `new_url`：点完什么都没变，
那它实际就是不可触摸。

如实说明的局限（不是真浏览器）：
- 布局是最小可用版：块级/行内/行内块、px 样式、估算字宽（中文 1.0em、
  西文 0.5em、行高 1.2em）；不支持 %/em/float/定位/flex/grid/table
  布局（按块级回退）、边距不合并、行内元素内部不换行、嵌套行内元素
  没有独立盒子。坐标是估算值，够指鼠标，不等于真实渲染像素。
- 鼠标是模拟的：没有真实窗口和真实指针，不向页面 JS 派发指针事件
 （mousemove/mousedown/mouseup）。所以真实滑块验证码（读指针事件、
  服务端校验）**不在本引擎能力内**；本包的滑块测试只验证拖拽路径 +
  落点逻辑（模拟滑块），结论见下。
- 截图是线框图：只画布局盒子 + 文字，没有 CSS 样式、图片、网页字体。
  能看清结构和位置，看不到页面真实视觉效果。
- 按钮点按：页面内联脚本与点击在同一次 `run_js` 里跑（引擎监听器
  跨调用不持久）；外部脚本不拉取、DOMContentLoaded 处理函数不触发、
  内联 `onclick` 属性不执行（只触发 `addEventListener` 处理函数）。
- 当前 example.com 静态 HTML 里已没有链接/按钮（内容只剩一段 `<p>`，
  链接改由外部脚本注入），所以对它跑 `elements` 会得到空清单——这是
  正确的，不是 bug。

滑块验证结论（模拟滑块，轨道 40..360、目标区 330..360）：
真人拖拽到目标区 → 通过；拖拽停在半路 → 不通过；一步到位的机器人
式跳跃（单点路径）→ 不通过（判为瞬移）；拖过头再拉回目标区 → 通过。
真实抖音滑块不在本引擎能力内，需要带渲染的浏览器才能验证。

## 真实渲染后端：Chromium via CDP（本地开发中，未发布）

上一节的从零引擎没有真实窗口、没有真实指针，所以真实滑块验证码
过不了。本节是第二后端：驱动一台**真实 Chromium**（Chrome DevTools
Protocol），做从零引擎做不到的事——真实页面渲染、真实指针事件、
真实截图。两个后端分工如下：

| 任务 | 走哪个后端 | 为什么 |
| --- | --- | --- |
| 轻量抓取、解析、读内容、搜东西 | 从零引擎（`fetch` / `engine` / `layout` / `interact` / `mouse` / `screenshot`） | 快、零依赖、不启动浏览器 |
| 需要真实 JS + 真实输入的交互（滑块验证码、复杂表单、JS 渲染页） | 真实渲染后端（`yousj.real.RealBrowser`） | 真实渲染、真实指针事件、真实截图 |

```python
from yousj.real import RealBrowser

with RealBrowser() as b:  # 启动真实 Chromium（全新临时 profile）
    b.goto("https://example.com")
    print(b.format_elements())   # 编号清单：真实渲染盒子 + ✓/✗可触摸
    shot = b.screenshot(path="shot.png")  # 真实渲染 PNG（不是线框图）
    b.mouse.move_to_element(3)   # 真鼠标自己移过去（真人轨迹）
    res = b.activate(3)          # 真点 + 点完变了什么（changed/console）
    b.fill(2, "皮蛋")            # 真打字（Input.insertText 键盘路径）
    res = b.drag_slider(handle_id=5, target_x=370,
                        check_js="document.getElementById('status')"
                                 ".dataset.status")
    print(res["state_after"])    # 页面自己说的滑块结果（passed/failed）
```

API（`yousj.real`，只用 Python 标准库，内置最小 WebSocket 客户端，
无第三方依赖）：
- `RealBrowser(chrome_path=None, headless=True, width=1280, height=800,
  port=None, user_data_dir=None, proxy=None)`：默认用本机 Chromium
  （`/opt/meta-chromium/chrome`，不在就传 `chrome_path=`）；`proxy`
  为空时强制直连（`--no-proxy-server`，不吃环境代理）；传
  `user_data_dir=` 可持久化 profile（默认每次全新临时目录，不带走
  登录态）。
- `goto(url)` / `evaluate(js)` / `url()` / `title()` / `text()`。
- `elements()` / `format_elements()`：与 `yousj.interact` 相同的字段
  与行格式，但 `box` 是 `getBoundingClientRect` 的真实渲染盒子；
  可触摸判断规则与从零引擎一致（静态结论，点完以 `changed` 为准）。
- `mouse`（`RealMouse`）：`move_to` / `click` / `drag` /
  `move_to_element` / `click_element` / `drag_element` / `hit_test` /
  `position`，与模拟 `Mouse` 同形，但每个动作都经 CDP `Input`
  派发为真实鼠标/指针事件（页面 JS 看到的是可信事件
  `isTrusted=true`，滑块手柄真的会动）。
- `screenshot(path=None, full_page=False)`：`Page.captureScreenshot`
  的真实渲染 PNG。
- `activate(id)`：真点 + 前后文本/URL 对比（`changed`）。
- `fill(id, text)`：聚焦 + `Input.insertText` 真打字（select 走 JS
  赋值 + change 事件，并注明）。
- `drag_slider(handle_id, target_x=None, target_id=None, check_js=None,
  ...)`：按 `human_path` 真人轨迹拖滑块手柄到目标像素，返回路径点数、
  拖前/拖后手柄盒子、`check_js` 读到的页面 verdict。**只认页面 verdict，
  不认路径本身**——路径再像人，页面说没过就是没过。

测试（只用本地测试页，不碰任何真实站点、不登录）：
`PYTHONPATH=python python3 tests/test_real_browser.py`（测试页在
`tests/pages/`：`form.html`、`slider.html`，用 `file://` 打开）。
19 项全过：跳转、真实盒子、可触摸、真打字、真点（按钮处理函数触发，
文本含填入的名字）、真实 PNG、链接跳转、滑块手柄定位、真人拖拽后页面
报 `passed`、手柄坐标移动、轨迹 26 点且终点精确、拖过头（415px）再拉回
目标（408px）、鼠标位置同步、浏览器正常关闭。

如实说明的局限：
- 需要一台 Chromium 二进制（默认路径找不到就传 `chrome_path=`）。
- 本地 HTTP 目标在无头 Chrome 152 里会被 Local Network Access 检查
  挡住（`ERR_BLOCKED_BY_LOCAL_NETWORK_ACCESS_CHECKS`，报错“由公开页面
  发起到本地网络的连接被阻止”）：`http://127.0.0.1` 和
  `http://localhost` 都试过，`--no-proxy-server` 与 `--disable-features`
  的 LNA 相关开关都试过，均未解除；`file://` 可正常加载。所以本地
  测试页走 `file://`。**公开站点之间跳转不受 LNA 影响**（这才是真实
  注册场景）；本地 HTTP 服务（如 localhost 后台）仍是本后端的已知限制，
  需按所用 Chrome 版本另找正确的放行开关或策略。
- 本地滑块通过，证明的是**机制**（真实指针事件能驱动真实滑块），不是
  “任何真实站点都能过”。真实站点还可能用：服务端拖拽校验（轨迹/时间/
  行为分析——`human_path` 只能缓解，不能保证）、IP/设备信誉、账号与频率
  限制、多步验证（选图、短信等）、以及 CDP/无头检测。真实站点的验证
  由主 Agent 另外走浏览器路线做，不在本测试范围内。
- 单页实例（一个 tab），多 tab 未实现；每次默认全新 profile（无登录态
  残留，需要持久化就传 `user_data_dir=`）。

## License

MIT — see [LICENSE](LICENSE). (The future UI shell / product layer is not
decided yet and may be licensed separately.)
