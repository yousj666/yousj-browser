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

## Roadmap

- v0.3: embed QuickJS, run page scripts headlessly (+ `Console` JS eval)
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

## License

MIT — see [LICENSE](LICENSE). (The future UI shell / product layer is not
decided yet and may be licensed separately.)
