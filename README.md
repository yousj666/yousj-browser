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

## Roadmap

- v0.2: embed QuickJS, run page scripts headlessly
- v0.3: CSS parsing + box layout (towards a visible browser)
- Later: configurable search engine, UI shell

## License

TBD (intended: MIT for the engine).
