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
python -m yousj download <url> [-o 文件名]  # SSRF 安全校验，大文件流式落盘
python -m yousj history [n]                 # 查看访问历史
python -m yousj config proxy [url|off]      # 设置代理
```

说明：

- **Cookie**：标准 `http.cookiejar`（MozillaCookieJar），Secure/HttpOnly 按规范处理；`net.clear_cookies()` 登出全部站点。
- **表单**：`doc.forms()` 列出所有表单；`fill({...})` 按 name 填值（select 会校验选项、checkbox 支持 True/False、未知字段名直接报错）；`submit()` 按 method 做 GET/POST，返回新 Document（`.url` 为最终地址）。
- **下载**：走 `net.get` 同样的 SSRF 校验与跳转检查，但 body 流式写入磁盘（无 10MB 上限、不占内存）。
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

**Known residual risk — DNS rebinding**: the check is resolve-then-validate,
so a hostile DNS could flip public→private in the millisecond window before
connect. Exploiting it needs attacker-controlled DNS *and* winning that race
on every request (each hop re-validates). Closing it properly needs DNS
pinning inside our own network layer (a bigger project, on the roadmap) —
a rushed 20-line patch here risks worse TLS bugs than it fixes, so it's
documented, not half-fixed.

## AI 反馈

AI Agent 发现 bug、JS 语义偏差或解析异常，请走结构化反馈入口：
[🤖 AI 反馈](.github/ISSUE_TEMPLATE/ai-feedback.yml)（Issues → New issue → AI 反馈）。
填上版本、复现脚本、期望 vs 实际即可，人类用户同样欢迎使用。

## License

MIT — see [LICENSE](LICENSE). (The future UI shell / product layer is not
decided yet and may be licensed separately.)
