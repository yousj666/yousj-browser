"""Yousj Browser AI service: a persistent RealBrowser behind a local HTTP API.

Why this exists: before this service, every AI step was a new Python
script that started a fresh Chromium, did one thing, then closed it.
Login dialogs, filled phone numbers and page state were lost between
steps. With this service, one browser stays alive, and the AI calls
tools one by one, sees the screenshot/result, then decides the next
tool. No new script per step.

Only binds 127.0.0.1. No auth, because it is local-only by design.
Do not expose it on a public port.

API: POST /tool with JSON {"tool": "...", "args": {...}}
Tools:
  goto: {"url": "..."}
  elements: {}
  text: {}
  title: {}
  url: {}
  click_element: {"id": 29}
  click_xy: {"x": 723, "y": 275}
  fill: {"id": 47, "text": "..."}  (value is masked in the reply)
  screenshot: {"path": "/tmp/shot.png"}  (assistant reads the file)
  evaluate: {"js": "..."}
  drag_slider: {"handle_id": 1, "target_x": 400}
  close: {}  (closes the browser; next goto restarts it)
  status: {}

Run: python3 -m yousj.browser_service
  or: python3 python/yousj/browser_service.py
Port: 18082 by default, BROWSER_SERVICE_PORT to change it.
Proxy and chrome wrapper are taken from env:
  YOUSJ_PROXY (default http://127.0.0.1:18080)
  YOUSJ_CHROME (default the cert-bypass wrapper in douyin-exe, else
  /opt/meta-chromium/chrome)
"""
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "python"))
# When run as a file, the package import above may already work; if not,
# fall back to adding the python dir itself.
if "yousj.real" not in sys.modules:
    try:
        from yousj.real import RealBrowser
    except Exception:
        sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
        from yousj.real import RealBrowser
else:
    from yousj.real import RealBrowser

PORT = int(os.environ.get("BROWSER_SERVICE_PORT", "18082"))
PROXY = os.environ.get("YOUSJ_PROXY", "http://127.0.0.1:18080")
CHROME = os.environ.get("YOUSJ_CHROME", "/home/hatch/workspace/douyin-exe/chrome-no-cert-check.sh")
if not os.path.isfile(CHROME):
    CHROME = "/opt/meta-chromium/chrome"

_lock = threading.Lock()
_browser = None


def _get_browser():
    global _browser
    if _browser is None:
        _browser = RealBrowser(proxy=PROXY, chrome_path=CHROME)
    return _browser


def _close_browser():
    global _browser
    if _browser is not None:
        try:
            _browser.close()
        except Exception:
            pass
        _browser = None


def _mask(text):
    s = str(text)
    if len(s) <= 4:
        return "***"
    return s[:3] + "****" + s[-3:]


def call_tool(tool, args):
    args = args or {}
    if tool == "status":
        return {"ok": True, "browser_open": _browser is not None, "port": PORT}
    if tool == "close":
        _close_browser()
        return {"ok": True, "browser_open": False}
    b = _get_browser()
    if tool == "goto":
        time.sleep(0.2)
        return {"ok": True, "result": b.goto(args["url"])}
    if tool == "elements":
        els = b.elements()
        out = []
        for e in els:
            out.append({"id": e["id"], "tag": e["tag"], "type": e.get("type"),
                        "html_id": e.get("html_id"), "text": (e.get("text") or "")[:60],
                        "box": e.get("box")})
        return {"ok": True, "elements": out}
    if tool == "text":
        return {"ok": True, "text": (b.text() or "")[:2000]}
    if tool == "title":
        return {"ok": True, "title": b.title()}
    if tool == "url":
        return {"ok": True, "url": b.url()}
    if tool == "click_element":
        b.mouse.click_element(int(args["id"]))
        time.sleep(0.5)
        return {"ok": True, "clicked_id": int(args["id"])}
    if tool == "click_xy":
        b.mouse.click(int(args["x"]), int(args["y"]))
        time.sleep(0.5)
        return {"ok": True, "clicked": {"x": int(args["x"]), "y": int(args["y"])}}
    if tool == "fill":
        r = b.fill(int(args["id"]), str(args["text"]))
        return {"ok": True, "id": r["id"], "value_len": len(str(r.get("value_after") or "")),
                "value_masked": _mask(r.get("value_after") or "")}
    if tool == "screenshot":
        path = args.get("path") or "/tmp/yousj-browser-shot.png"
        return {"ok": True, "screenshot": b.screenshot(path=path)}
    if tool == "evaluate":
        return {"ok": True, "result": b.evaluate(str(args["js"]))}
    if tool == "drag_slider":
        r = b.drag_slider(handle_id=int(args["handle_id"]), target_x=int(args["target_x"]))
        return {"ok": True, "drag": {"from": r.get("from"), "to": r.get("to"),
                                      "path_points": r.get("path_points"),
                                      "state_after": r.get("state_after")}}
    if tool == "send_code":
        # Douyin Send Code, written 2026-10-26 after live investigation:
        # the Send code control is a span, NOT a button in elements().
        # Its React props are onMouseDown/onKeyDown (no onClick),
        # sendBtnDisabled=false, areaCode and mobile are in React state.
        # Click it with the real mouse, then prove the result instead
        # of assuming success: a countdown, a slider, or a network
        # request must appear. If none appears, the mouse event did
        # not reach the page (observed 2026-10-26: document mousedown
        # and mousemove listeners stayed empty after click_xy), and
        # the caller must fix RealMouse dispatch, not retry blindly.
        info = b.evaluate(
            "(()=>{const s=[...document.querySelectorAll('span')]"
            ".find(e=>(e.innerText||'').trim()==='Send code');"
            " if(!s) return 'no-send-code-span';"
            " const r=s.getBoundingClientRect();"
            " return JSON.stringify({x:Math.round(r.x+r.width/2),"
            " y:Math.round(r.y+r.height/2), w:Math.round(r.width),"
            " h:Math.round(r.height)});})()")
        if info == "no-send-code-span":
            return {"ok": False, "error": "no-send-code-span"}
        import json as _json
        pt = _json.loads(info)
        # Install a one-shot proof listener before clicking, so the
        # result is evidence, not a guess.
        b.evaluate(
            "(()=>{window.__sendcode_proof=[];"
            " document.addEventListener('mousedown',e=>{"
            " window.__sendcode_proof.push([e.clientX,e.clientY]);},"
            " true);})()")
        b.mouse.click(int(pt["x"]), int(pt["y"]))
        time.sleep(6)
        proof = b.evaluate("JSON.stringify(window.__sendcode_proof||[])")
        shot = b.screenshot(path=args.get("shot_path") or "/tmp/yousj-sendcode.png")
        text = b.text() or ""
        return {"ok": True, "clicked": pt, "mouse_proof": proof,
                "screenshot": shot,
                "countdown_hint": ("秒" in text or "s" in text),
                "note": "mouse_proof empty means the CDP mouse event did not "
                        "reach the page; do not claim SMS was sent."}
    if tool == "new_page":
        # Open a new tab and switch control to it. The old tab stays
        # open; use switch_page to go back. Screenshot after this
        # captures the NEW page, not the old one.
        page = b.new_page(args.get("url"))
        return {"ok": True, "page": page, "pages": b.list_pages()}
    if tool == "list_pages":
        return {"ok": True, "pages": b.list_pages()}
    if tool == "switch_page":
        page = b.switch_page(args["page_id"])
        return {"ok": True, "page": page, "url": b.url(),
                "title": b.title()}
    if tool == "sleep":
        time.sleep(float(args.get("seconds", 1)))
        return {"ok": True}
    raise ValueError("unknown tool: %s" % tool)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        # Keep logs short: tool name only, never argument values.
        pass

    def do_POST(self):
        if self.path != "/tool":
            self._send(404, {"ok": False, "error": "only POST /tool"})
            return
        try:
            length = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(length) or b"{}")
            tool = body.get("tool")
            with _lock:
                result = call_tool(tool, body.get("args") or {})
            self._send(200, result)
        except Exception as e:
            self._send(500, {"ok": False, "error": str(e)[:500]})

    def do_GET(self):
        if self.path == "/health":
            self._send(200, {"ok": True, "browser_open": _browser is not None})
        else:
            self._send(404, {"ok": False, "error": "use POST /tool"})

    def _send(self, code, obj):
        data = json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


if __name__ == "__main__":
    print("Yousj Browser AI service on 127.0.0.1:%d proxy=%s" % (PORT, PROXY), flush=True)
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
