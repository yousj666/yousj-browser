"""Real rendering backend for the Yousj browser (Chromium via CDP).

Division of labor (see README, "真实渲染后端"):
- Lightweight fetch / parse / estimate: the from-scratch engine
  (:mod:`yousj.engine`, :mod:`yousj.layout`, :mod:`yousj.interact`,
  :mod:`yousj.mouse`, :mod:`yousj.screenshot`). No real window, no real
  pointer, wireframe screenshots only. Use it for fast, cheap reads.
- Real interaction: THIS module. It drives a real Chromium binary
  over the Chrome DevTools Protocol (CDP): real page rendering, real
  pointer events (:meth:`Input.dispatchMouseEvent` — trusted by the
  page, unlike JS-synthesized events), real screenshots
  (:meth:`Page.captureScreenshot`). Use it when a page needs real JS
  plus real input — e.g. a slider CAPTCHA, which the from-scratch
  engine cannot drive (it dispatches no pointer events).

Only the Python standard library is used. A minimal WebSocket client
is implemented here (CDP speaks WebSocket; the stdlib has none), so
this module has no third-party dependency.

Typical use::

    from yousj.real import RealBrowser
    with RealBrowser() as b:
        b.goto("http://127.0.0.1:8000/slider.html")
        print(b.format_elements())
        shot = b.screenshot(path="shot.png")   # real rendering PNG
        res = b.drag_slider(handle_id=3, target_x=370)
        print(res["state_after"])              # what the page says

The browser AI drives this the same way it drives the simulated
:class:`yousj.mouse.Mouse` — pick an element number or a pixel, move /
click / drag — except here the pointer is real: the page's JS sees
genuine mouse/pointer events, and a slider handle really moves.
Trajectories reuse :func:`yousj.human.human_path` (variable speed,
wobble, pauses, overshoot-and-pullback), so a drag never looks like a
perfect robot line.

This module never touches :mod:`yousj.settings` or :mod:`yousj.ai`
(BYOK config, owned by Muse) and never modifies the from-scratch
engine — it only adds a second backend beside it.
"""

import base64
import hashlib
import json
import os
import random
import shutil
import socket
import struct
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request

from . import human as _human

_WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


# ---------------------------------------------------------------------------
# Minimal WebSocket client (stdlib only; CDP needs it, stdlib lacks it).
# ---------------------------------------------------------------------------

class _WSClient:
    """A WebSocket client good enough for CDP: text messages, arbitrary
    size, fragmentation, ping/pong. Client frames are masked (required).
    Server frames are unmasked. Only ``ws://`` (no TLS) is supported —
    CDP here always runs on localhost."""

    def __init__(self, url, timeout=60):
        parts = urllib.parse.urlparse(url)
        if parts.scheme != "ws":
            raise ValueError("only ws:// URLs are supported, got %r"
                             % (parts.scheme,))
        self._host = parts.hostname or "127.0.0.1"
        self._port = parts.port or 80
        self._path = parts.path or "/"
        if parts.query:
            self._path += "?" + parts.query
        self._timeout = timeout
        self._sock = socket.create_connection(
            (self._host, self._port), timeout=timeout)
        self._buf = b""
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        req = (
            "GET %s HTTP/1.1\r\n"
            "Host: %s:%d\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            "Sec-WebSocket-Key: %s\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n" % (self._path, self._host, self._port, key))
        self._sock.sendall(req.encode("ascii"))
        headers = b""
        while b"\r\n\r\n" not in headers:
            chunk = self._sock.recv(4096)
            if not chunk:
                raise RuntimeError(
                    "websocket handshake: connection closed by peer")
            headers += chunk
        head, rest = headers.split(b"\r\n\r\n", 1)
        self._buf = rest
        lines = head.decode("latin-1").split("\r\n")
        if " 101 " not in lines[0]:
            raise RuntimeError(
                "websocket handshake: unexpected status %r" % (lines[0],))
        accept = base64.b64encode(
            hashlib.sha1((key + _WS_GUID).encode("ascii")).digest()
        ).decode("ascii")
        if not any(l.lower().startswith("sec-websocket-accept:")
                   and accept in l for l in lines[1:]):
            raise RuntimeError("websocket handshake: bad Sec-WebSocket-Accept")

    def set_timeout(self, timeout):
        self._timeout = timeout
        self._sock.settimeout(timeout)

    def _recv_exact(self, n):
        while len(self._buf) < n:
            chunk = self._sock.recv(65536)
            if not chunk:
                raise RuntimeError("websocket: connection closed by peer")
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def _recv_frame(self):
        """One frame -> (fin, opcode, payload)."""
        h = self._recv_exact(2)
        b0, b1 = h[0], h[1]
        fin = bool(b0 & 0x80)
        opcode = b0 & 0x0F
        masked = bool(b1 & 0x80)
        ln = b1 & 0x7F
        if ln == 126:
            ln = struct.unpack(">H", self._recv_exact(2))[0]
        elif ln == 127:
            ln = struct.unpack(">Q", self._recv_exact(8))[0]
        mask = self._recv_exact(4) if masked else None
        payload = self._recv_exact(ln) if ln else b""
        if mask:
            payload = bytes(p ^ mask[i % 4]
                            for i, p in enumerate(payload))
        return fin, opcode, payload

    def _send_frame(self, opcode, payload):
        if isinstance(payload, str):
            payload = payload.encode("utf-8")
        mask = os.urandom(4)
        ln = len(payload)
        if ln < 126:
            header = struct.pack(">BB", 0x80 | opcode, 0x80 | ln)
        elif ln < 65536:
            header = struct.pack(">BBH", 0x80 | opcode, 0x80 | 126, ln)
        else:
            header = struct.pack(">BBQ", 0x80 | opcode, 0x80 | 127, ln)
        masked = bytes(p ^ mask[i % 4] for i, p in enumerate(payload))
        self._sock.sendall(header + mask + masked)

    def send_text(self, text):
        self._send_frame(0x1, text)

    def recv_message(self):
        """Next complete message as str (text) or bytes (binary).

        Reassembles fragmented messages; answers pings; raises on close.
        """
        parts = []
        opcode = None
        while True:
            fin, op, payload = self._recv_frame()
            if op == 0x9:  # ping -> pong
                self._send_frame(0xA, payload)
                continue
            if op == 0x8:  # close
                raise RuntimeError("websocket: closed by peer")
            if op in (0x1, 0x2):
                opcode = op
                parts.append(payload)
            elif op == 0x0:  # continuation
                parts.append(payload)
            else:
                continue
            if fin:
                break
        data = b"".join(parts)
        return data.decode("utf-8") if opcode == 0x1 else data

    def close(self):
        try:
            self._send_frame(0x8, b"")
        except Exception:
            pass
        try:
            self._sock.close()
        except Exception:
            pass


class _CDP:
    """Synchronous CDP request/response over one page WebSocket.

    Events (messages without ``id``) are collected, not dropped:
    ``console`` holds recent ``Runtime.consoleAPICalled`` lines, so
    callers can report what the page logged. Everything else is ignored
    (load is detected by polling ``document.readyState``, not events).
    """

    def __init__(self, ws_url, timeout=60):
        self._ws = _WSClient(ws_url, timeout=timeout)
        self._timeout = timeout
        self._next_id = 0
        self.console = []

    def call(self, method, params=None, timeout=None):
        self._ws.set_timeout(timeout or self._timeout)
        self._next_id += 1
        mid = self._next_id
        msg = {"id": mid, "method": method}
        if params is not None:
            msg["params"] = params
        self._ws.send_text(json.dumps(msg))
        while True:
            raw = self._ws.recv_message()
            try:
                data = json.loads(raw)
            except ValueError:
                continue
            if "id" not in data:
                if data.get("method") == "Runtime.consoleAPICalled":
                    args = data.get("params", {}).get("args", [])
                    for a in args:
                        v = a.get("value")
                        if v is not None:
                            self.console.append(str(v))
                        elif a.get("description"):
                            self.console.append(a["description"])
                    self.console = self.console[-50:]
                continue
            if data["id"] != mid:
                continue
            if "error" in data:
                err = data["error"]
                raise RuntimeError("CDP %s failed: %s"
                                   % (method, err.get("message", err)))
            return data.get("result", {})

    def close(self):
        self._ws.close()


def _free_port():
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]
    finally:
        s.close()


def _http_no_proxy(method, url, timeout=10):
    """HTTP request that bypasses proxy env vars (localhost CDP must not
    go through the sandbox egress proxy — it would hang or fail)."""
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    req = urllib.request.Request(url, method=method)
    with opener.open(req, timeout=timeout) as resp:
        return resp.status, resp.read().decode("utf-8", "replace")


# ---------------------------------------------------------------------------
# Interactive elements: same shape and verdicts as yousj.interact, but
# with REAL boxes from getBoundingClientRect. The touchable rules mirror
# yousj.interact._touchable (kept in sync manually so this module stays
# independent of the from-scratch engine's native library).
# ---------------------------------------------------------------------------

def _touchable(el):
    """Static touchable verdict: (touchable, reason).

    Same definition as the from-scratch engine: pressing has an
    observable effect (navigation, submission, action, state change)
    => touchable; pressing with no effect => untouchable. Static
    judgment cannot see addEventListener handlers or tell a no-op
    handler from a working one — so verify by actually clicking
    (RealBrowser.activate reports ``changed`` after a real click).
    """
    if el.get("disabled"):
        return False, "disabled (browsers do not activate disabled controls)"
    if el.get("onclick"):
        return True, "inline onclick handler"
    kind = el.get("kind")
    if kind == "link":
        href = (el.get("href") or "").strip()
        low = href.lower()
        if low.startswith("javascript:"):
            return True, "javascript: link (code runs on click)"
        if low.startswith(("mailto:", "tel:")):
            return True, "external-scheme link (opens mail/phone app)"
        if low.startswith("#"):
            return True, "in-page anchor (fragment navigation)"
        if href:
            return True, "navigates to %s" % (el.get("url") or href)
        return False, "link without href"
    if kind in ("button", "submit"):
        if kind == "submit" and el.get("form_index") is not None:
            return True, "submits its form"
        return True, ("button (a click handler may be registered by page "
                      "script; verify by clicking)")
    if kind in ("input", "toggle", "select", "textarea"):
        return True, "form control (focus / input / state change)"
    if kind == "label":
        if el.get("label_for") or el.get("wraps_control"):
            return True, "activates its associated control"
        return False, "label without associated control"
    if kind == "clickable":
        return False, ("no static click handler detected (role=%s; a "
                       "script-registered handler is possible; verify by "
                       "clicking)" % (el.get("role") or "interactive"))
    return False, "no link, button, form behavior, or click handler detected"


# Enumerates interactive elements in document order. tag_index is the
# index among ALL elements of that tag (querySelectorAll(tag) order),
# so an element can be re-located later with
# querySelectorAll(tag)[tag_index] (or getElementById when it has one).
_ELEMENTS_JS = """(() => {
  const out = [];
  // True per-tag index for every element (one pass, then lookup).
  const perTag = new Map();
  document.querySelectorAll('*').forEach((el) => {
    const t = el.tagName.toLowerCase();
    const n = perTag.get(t) || 0;
    perTag.set(t, n + 1);
    el.__yousjTagIndex = n;
  });
  const formIndex = new Map();
  document.querySelectorAll('form').forEach((f, i) => formIndex.set(f, i));
  const SEL = 'a, button, input, select, textarea, label, [onclick], '
    + '[role="button"], [role="checkbox"], [role="radio"], [role="link"], '
    + '[role="slider"]';
  document.querySelectorAll(SEL).forEach((el) => {
    const tag = el.tagName.toLowerCase();
    const r = el.getBoundingClientRect();
    let kind = 'clickable';
    if (tag === 'a') kind = 'link';
    else if (tag === 'button') kind = (el.type === 'submit') ? 'submit' : 'button';
    else if (tag === 'input') {
      const t = (el.type || '').toLowerCase();
      if (t === 'checkbox' || t === 'radio') kind = 'toggle';
      else if (t === 'button' || t === 'submit') kind = (t === 'submit') ? 'submit' : 'button';
      else kind = 'input';
    }
    else if (tag === 'select') kind = 'select';
    else if (tag === 'textarea') kind = 'textarea';
    else if (tag === 'label') kind = 'label';
    let options = null;
    if (tag === 'select') {
      options = Array.from(el.options).map((o) => ({value: o.value, text: o.text}));
    }
    const form = el.form || el.closest('form');
    out.push({
      tag: tag, kind: kind, type: el.type || '',
      text: (el.innerText || '').trim().slice(0, 80),
      href: el.getAttribute('href') || '', name: el.name || '',
      html_id: el.id || '',
      value: (el.value !== undefined ? String(el.value) : ''),
      disabled: !!el.disabled, onclick: el.hasAttribute('onclick'),
      role: el.getAttribute('role') || '',
      tag_index: el.__yousjTagIndex,
      form_index: (form ? formIndex.get(form) : null),
      checked: (el.checked !== undefined ? !!el.checked : null),
      options: options, label_for: el.htmlFor || '',
      wraps_control: (tag === 'label'
        ? !!el.querySelector('input, select, textarea, button') : false),
      box: {x: Math.round(r.x), y: Math.round(r.y),
            width: Math.round(r.width), height: Math.round(r.height)}
    });
  });
  return out;
})()"""


def _js_string(s):
    """Escape a Python string for a JS single-quoted literal."""
    return ("'" + s.replace("\\", "\\\\").replace("'", "\\'")
            .replace("\n", "\\n").replace("\r", "\\r") + "'")


def _element_js(el):
    """JS expression locating this element (id preferred, else tag+index)."""
    el_id = el.get("html_id") or ""
    if el_id:
        return "document.getElementById(%s)" % _js_string(el_id)
    tag = el.get("tag") or ""
    if not tag.isalnum():
        return None
    return "document.querySelectorAll('%s')[%d]" % (
        tag, el.get("tag_index", 0))


def _snip(text, n=500):
    return (text or "")[:n]


def _format_elements(elements):
    """Render an element list as AI-readable text (same line format as
    yousj.interact.format_elements, but boxes are REAL rendered boxes)."""
    lines = []
    for el in elements:
        parts = ["[%d]" % el["id"], "<%s>" % el["tag"]]
        if el.get("type"):
            parts.append("type=%s" % el["type"])
        label = el.get("text") or ""
        if not label and el.get("value"):
            label = el["value"]
        if label:
            parts.append("'%s'" % label[:80])
        if el.get("kind") == "link" and el.get("url"):
            parts.append("-> %s" % el["url"])
        if el.get("box"):
            b = el["box"]
            parts.append("@(%d,%d) %dx%d" % (
                b["x"], b["y"], b["width"], b["height"]))
        if "touchable" in el:
            if el["touchable"]:
                parts.append("✓touchable")
            else:
                parts.append("✗untouchable (%s)"
                             % el.get("touchable_reason", ""))
        if el.get("html_id"):
            parts.append("html_id=%s" % el["html_id"])
        if el.get("name"):
            parts.append("name=%s" % el["name"])
        if el.get("kind") == "select" and el.get("options"):
            opts = ",".join(o["value"] for o in el["options"][:8])
            parts.append("options=[%s]" % opts)
        if el.get("kind") == "toggle":
            parts.append("checked=%s" % bool(el.get("checked")))
        if el.get("kind") == "label" and el.get("label_for"):
            parts.append("for=%s" % el["label_for"])
        if el.get("disabled"):
            parts.append("(disabled)")
        if el.get("onclick"):
            parts.append("(onclick)")
        if el.get("role"):
            parts.append("role=%s" % el["role"])
        lines.append(" ".join(parts))
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Real mouse: same API shape as yousj.mouse.Mouse, but every move/click/
# drag is dispatched to the page as genuine CDP input events. The page's
# JS sees real mouse/pointer events (trusted, isTrusted=true), so a
# slider handle really moves and a button handler really fires.
# ---------------------------------------------------------------------------

class RealMouse:
    """A REAL mouse cursor inside a Chromium page (via CDP Input).

    Tracks its own position (starts at (0, 0)); every move/click/drag
    walks a :func:`yousj.human.human_path` trajectory and dispatches
    each point as an Input.dispatchMouseEvent. ``dry_run=True`` skips
    both sleeping and dispatching (paths still generated, position
    still updates) — use it to inspect a trajectory without side
    effects.
    """

    def __init__(self, browser):
        self._b = browser
        self._pos = (0, 0)
        self.history = []

    def position(self):
        """Current cursor position ``(x, y)``."""
        return self._pos

    def reset(self):
        """Clear history and park the cursor at the origin (0, 0)."""
        self._pos = (0, 0)
        self.history = []

    def _dispatch(self, kind, x, y, button=None):
        params = {"type": kind, "x": int(x), "y": int(y)}
        if button:
            params["button"] = button
        if kind in ("mousePressed", "mouseReleased"):
            params["clickCount"] = 1
        self._b._cdp.call("Input.dispatchMouseEvent", params)

    def _walk(self, path, dry_run, button=None):
        """Sleep each point's delay (unless dry_run), dispatch it as a
        mouseMoved event (unless dry_run), update position."""
        for x, y, delay in path:
            if not dry_run and delay > 0:
                time.sleep(delay)
            if not dry_run:
                self._dispatch("mouseMoved", x, y, button=button)
            self._pos = (x, y)
        return path

    def move_to(self, x, y, human=True, dry_run=False, rng=None,
                overshoot=None):
        """Move the cursor to ``(x, y)``. Returns the path taken."""
        x, y = int(x), int(y)
        start = self._pos
        if human:
            path = _human.human_path(start[0], start[1], x, y, rng=rng,
                                     overshoot=overshoot)
        else:
            path = [(x, y, 0.0)]
        self._walk(path, dry_run)
        self.history.append({"action": "move", "from": start,
                             "to": (x, y), "points": len(path),
                             "human": human})
        return path

    def click(self, x=None, y=None, human=True, dry_run=False, rng=None,
              overshoot=None):
        """Click at the current position, or move to (x, y) first.

        Returns ``{"at": (x, y), "path": [...]}``. Unlike the simulated
        mouse, this press/release is real: the page's click handlers
        fire.
        """
        path = []
        if x is not None and y is not None:
            path = self.move_to(x, y, human=human, dry_run=dry_run,
                                rng=rng, overshoot=overshoot)
        else:
            path = [(self._pos[0], self._pos[1], 0.0)]
        if human and not dry_run:
            time.sleep(random.uniform(0.05, 0.15))  # press+release dwell
        if not dry_run:
            self._dispatch("mousePressed", self._pos[0], self._pos[1],
                           button="left")
            time.sleep(random.uniform(0.03, 0.08))
            self._dispatch("mouseReleased", self._pos[0], self._pos[1],
                           button="left")
        self.history.append({"action": "click", "at": self._pos,
                             "path_points": len(path)})
        return {"at": self._pos, "path": path}

    def drag(self, x0, y0, x1, y1, human=True, dry_run=False, rng=None,
             overshoot=None):
        """Drag from (x0, y0) to (x1, y1): move, press, walk, release.

        Returns ``{"from", "to", "path", "points"}``. The walk happens
        while the button is held, so the page sees a genuine drag
        (pointerdown -> pointermove* -> pointerup). This is the slider
        gesture: a real slider handle follows it.
        """
        r = rng or random
        x0, y0, x1, y1 = int(x0), int(y0), int(x1), int(y1)
        if self._pos != (x0, y0):
            self.move_to(x0, y0, human=human, dry_run=dry_run, rng=r)
        if human and not dry_run:
            time.sleep(r.uniform(0.1, 0.3))  # grab pause before moving
        if not dry_run:
            self._dispatch("mousePressed", x0, y0, button="left")
        if human:
            path = _human.human_path(x0, y0, x1, y1, rng=r,
                                     overshoot=overshoot)
        else:
            path = [(x1, y1, 0.0)]
        self._walk(path, dry_run, button="left")
        if not dry_run:
            self._dispatch("mouseReleased", x1, y1, button="left")
        if human and not dry_run:
            time.sleep(r.uniform(0.05, 0.2))  # release pause
        self.history.append({"action": "drag", "from": (x0, y0),
                             "to": (x1, y1), "points": len(path),
                             "human": human})
        return {"from": (x0, y0), "to": (x1, y1), "path": path,
                "points": len(path)}

    def _element_target(self, element_id, rng):
        """(element, box, target) for an interactive element: its real
        box plus a jittered point INSIDE it (never dead-center twice).
        Raises ValueError when the element has no number or no usable
        (non-zero) box."""
        els = self._b.elements()
        el = next((e for e in els if e["id"] == element_id), None)
        if el is None:
            raise ValueError("no interactive element with id %r"
                             % (element_id,))
        # Boxes are viewport-relative: an element below the fold sits
        # outside the hittable area and a click there lands nowhere.
        # Scroll it to the middle of the viewport first, then re-read
        # its (fresh) box.
        locator = _element_js(el)
        if locator:
            try:
                self._b.evaluate(
                    "(function(){var e=%s;"
                    "if(e){e.scrollIntoView({block:'center',"
                    "inline:'center'});}return !!e;})()" % locator)
                els = self._b.elements()
                el = next((e for e in els if e["id"] == element_id), el)
            except Exception:
                pass
        box = el.get("box")
        if not box or box["width"] <= 0 or box["height"] <= 0:
            raise ValueError(
                "element %d (<%s>) has no usable box (hidden or zero-size); "
                "aim by pixels instead" % (element_id, el["tag"]))
        r = rng or random
        jx = r.uniform(-0.3, 0.3) * box["width"]
        jy = r.uniform(-0.3, 0.3) * box["height"]
        tx = int(round(box["x"] + box["width"] / 2.0 + jx))
        ty = int(round(box["y"] + box["height"] / 2.0 + jy))
        return el, box, (tx, ty)

    def move_to_element(self, element_id, human=True, dry_run=False,
                        rng=None, overshoot=None):
        """Move the cursor onto interactive element ``element_id``."""
        el, box, target = self._element_target(element_id, rng)
        path = self.move_to(target[0], target[1], human=human,
                            dry_run=dry_run, rng=rng, overshoot=overshoot)
        return {"element": element_id, "tag": el["tag"], "box": box,
                "target": target, "path": path}

    def click_element(self, element_id, human=True, dry_run=False,
                      rng=None, overshoot=None):
        """Move onto element ``element_id`` and really click it."""
        moved = self.move_to_element(element_id, human=human,
                                     dry_run=dry_run, rng=rng,
                                     overshoot=overshoot)
        clicked = self.click(human=human, dry_run=dry_run, rng=rng)
        return {"element": element_id, "tag": moved["tag"],
                "box": moved["box"], "target": moved["target"],
                "path": moved["path"], "at": clicked["at"]}

    def drag_element(self, element_id, x1, y1, human=True, dry_run=False,
                     rng=None, overshoot=None):
        """Drag interactive element ``element_id`` (from its box center)
        to pixel ``(x1, y1)`` — the slider gesture by element number."""
        el, box, start = self._element_target(element_id, rng)
        res = self.drag(start[0], start[1], x1, y1, human=human,
                        dry_run=dry_run, rng=rng, overshoot=overshoot)
        res["element"] = element_id
        res["tag"] = el["tag"]
        res["box"] = box
        return res

    def hit_test(self, x, y):
        """Interactive element whose REAL box contains pixel (x, y).

        Smallest containing box wins. Approximate: real hit-testing
        also needs paint order and z-index, which this does not model
        (it uses boxes only).
        """
        best = None
        best_area = None
        for el in self._b.elements():
            b = el.get("box")
            if not b:
                continue
            if b["x"] <= x < b["x"] + b["width"] and \
                    b["y"] <= y < b["y"] + b["height"]:
                area = b["width"] * b["height"]
                if best_area is None or area < best_area:
                    best, best_area = el, area
        return best


# ---------------------------------------------------------------------------
# RealBrowser: one Chromium page, driven over CDP.
# ---------------------------------------------------------------------------

class RealBrowser:
    """A real Chromium page, driven over CDP.

    ``RealBrowser()`` launches Chromium headless with a fresh temporary
    profile (no login state leaks in or out), connects over CDP, and
    exposes the AI layer with REAL effects: ``goto``, ``elements``,
    ``mouse`` (real pointer), ``screenshot`` (real rendering),
    ``activate`` (real click + what changed), ``fill`` (real typing),
    ``drag_slider`` (real slider drag + page verdict).

    Use as a context manager (``with RealBrowser() as b: ...``) so the
    browser is always closed; or call :meth:`close` explicitly.

    ``chrome_path`` defaults to the agent's Chromium
    (``/opt/meta-chromium/chrome``); pass another binary if yours lives
    elsewhere. ``proxy`` (e.g. ``"http://127.0.0.1:7890"``) is forwarded
    to Chromium as ``--proxy-server`` when set; local test pages need
    none. ``headless=False`` needs a display (Xvfb) and is only for
    watching a run by eye.
    """

    # /opt/meta-chromium/chrome refuses standalone startup ("Hatch
    # WebGL domain policy is required"), so prefer the project wrapper
    # (Chrome-for-Testing + cert bypass) whenever it exists.
    _WRAPPER = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                            "..", "..", "chrome-yousj.sh")
    DEFAULT_CHROME = (_WRAPPER if os.path.isfile(_WRAPPER)
                      else "/opt/meta-chromium/chrome")

    def __init__(self, chrome_path=None, headless=True, width=1280,
                 height=800, port=None, user_data_dir=None, proxy=None,
                 extra_args=None, timeout=60):
        self.width = int(width)
        self.height = int(height)
        self._timeout = timeout
        self._chrome = chrome_path or self.DEFAULT_CHROME
        if not (os.path.isfile(self._chrome)
                and os.access(self._chrome, os.X_OK)):
            raise RuntimeError(
                "Chromium binary not found or not executable: %s "
                "(pass chrome_path=)" % self._chrome)
        self._port = port or _free_port()
        self._tmp_profile = False
        if user_data_dir is None:
            user_data_dir = tempfile.mkdtemp(prefix="yousj-real-")
            self._tmp_profile = True
        self._user_data_dir = user_data_dir
        args = [self._chrome]
        if headless:
            args.append("--headless=new")
        args += ["--no-sandbox", "--disable-gpu",
                 "--disable-dev-shm-usage",
                 # Chrome 152 enforces Local Network Access checks: a
                 # navigation from a public page (including about:blank)
                 # to 127.0.0.1 is blocked with
                 # ERR_BLOCKED_BY_LOCAL_NETWORK_ACCESS_CHECKS. An AI
                 # browser must be able to drive local pages (tests,
                 # local services) from any start page, so these checks
                 # are disabled. This does not weaken remote-site
                 # security — it only stops Chrome blocking local
                 # targets for local automation.
                 "--disable-features=LocalNetworkAccessChecks,"
                 "PrivateNetworkAccessSendPreflights,"
                 "BlockInsecurePrivateNetworkRequests,"
                 "LocalNetworkAccessPermissionPrompt",
                 "--remote-debugging-port=%d" % self._port,
                 "--window-size=%d,%d" % (self.width, self.height),
                 "--user-data-dir=%s" % self._user_data_dir]
        if proxy:
            args.append("--proxy-server=%s" % proxy)
        else:
            # Direct connection, ignoring env/system proxies. This
            # matters in sandboxes whose egress proxy refuses localhost
            # (a Chromium that inherits http_proxy would fail to reach
            # 127.0.0.1 test pages with chrome-error://chromewebdata/).
            args.append("--no-proxy-server")
        if extra_args:
            args += list(extra_args)
        args.append("about:blank")
        log_path = os.path.join(self._user_data_dir, "chrome.log")
        self._log = open(log_path, "wb")
        try:
            self._proc = subprocess.Popen(
                args, stdout=self._log, stderr=subprocess.STDOUT)
        except Exception:
            self._log.close()
            raise
        try:
            ws_url = self._wait_for_page(timeout)
            self._cdp = _CDP(ws_url, timeout=timeout)
            self._cdp.call("Page.enable")
            self._cdp.call("Runtime.enable")
            # Chrome 158 headless otherwise lays out pages in a default
            # 800x600 viewport even when --window-size says otherwise,
            # which leaves element boxes below y=600 outside the area
            # real mouse events can hit. Pin the layout viewport to the
            # configured window size.
            try:
                self._cdp.call("Emulation.setDeviceMetricsOverride", {
                    "width": self.width, "height": self.height,
                    "deviceScaleFactor": 1, "mobile": False})
            except Exception:
                pass
        except Exception:
            self.close()
            raise
        self._url = "about:blank"
        self.mouse = RealMouse(self)

    def _wait_for_page(self, timeout):
        """Poll the DevTools HTTP endpoint until a page target appears;
        return its WebSocket URL."""
        deadline = time.monotonic() + timeout
        last_error = None
        tried_new = False
        while time.monotonic() < deadline:
            if self._proc.poll() is not None:
                raise RuntimeError(
                    "Chromium exited during startup (code %s); see %s"
                    % (self._proc.returncode,
                       os.path.join(self._user_data_dir, "chrome.log")))
            try:
                _, body = _http_no_proxy(
                    "GET", "http://127.0.0.1:%d/json/list" % self._port)
                targets = json.loads(body) if body else []
                for t in targets:
                    if t.get("type") == "page" and t.get(
                            "webSocketDebuggerUrl"):
                        return t["webSocketDebuggerUrl"]
                if not tried_new:
                    tried_new = True
                    try:
                        _http_no_proxy(
                            "PUT",
                            "http://127.0.0.1:%d/json/new?about:blank"
                            % self._port)
                    except Exception:
                        pass
            except Exception as e:  # noqa: BLE001 - keep polling
                last_error = e
            time.sleep(0.5)
        raise RuntimeError(
            "Chromium DevTools endpoint did not come up in %ss: %s"
            % (timeout, last_error))

    # -- multiple pages ---------------------------------------------------
    # One Chromium, many tabs. The AI drives one page at a time; the
    # others stay open in the background. Opening a new page is how
    # the AI retries a login from a clean page without losing the
    # old page's state, and a screenshot always captures the
    # CURRENT page, so a new page can be screenshotted right away.

    def _list_targets(self):
        _, body = _http_no_proxy(
            "GET", "http://127.0.0.1:%d/json/list" % self._port)
        return json.loads(body) if body else []

    def list_pages(self):
        """All open page tabs: ``[{"id", "title", "url"}]``.

        Chrome's own omnibox popup pages (chrome://) are excluded —
        they are not tabs the AI can drive."""
        pages = []
        for t in self._list_targets():
            if t.get("type") == "page" and not (t.get("url") or "") \
                    .startswith("chrome://"):
                pages.append({"id": t.get("id"), "title": t.get("title"),
                              "url": t.get("url")})
        return pages

    def _switch_to(self, ws_url):
        """Point this browser's CDP connection at another page tab."""
        old = getattr(self, "_cdp", None)
        if old is not None:
            try:
                old.close()
            except Exception:
                pass
        self._cdp = _CDP(ws_url, timeout=self._timeout)
        self._cdp.call("Page.enable")
        self._cdp.call("Runtime.enable")
        self.mouse.reset()

    def new_page(self, url=None):
        """Open a new tab and switch control to it.

        ``url`` is optional; without it the new tab starts at
        about:blank and the caller drives it with ``goto``. Returns
        ``{"id", "title", "url"}`` for the new tab. The previous tab
        stays open — use :meth:`switch_page` to go back to it."""
        target = url or "about:blank"
        _, body = _http_no_proxy(
            "PUT", "http://127.0.0.1:%d/json/new?%s"
            % (self._port, urllib.parse.quote(target, safe="")))
        data = json.loads(body) if body else {}
        ws_url = data.get("webSocketDebuggerUrl")
        if not ws_url:
            # /json/new sometimes returns an empty body; fall back to
            # the last page in /json/list (the newly created tab).
            pages = [t for t in self._list_targets()
                     if t.get("type") == "page"
                     and t.get("webSocketDebuggerUrl")]
            if not pages:
                raise RuntimeError("new page created but no page target found")
            data = pages[-1]
            ws_url = data["webSocketDebuggerUrl"]
        self._switch_to(ws_url)
        self._url = target
        return {"id": data.get("id"), "title": data.get("title"),
                "url": data.get("url")}

    def switch_page(self, page_id):
        """Switch control to an already-open tab by its ``id``.

        Returns ``{"id", "title", "url"}`` for the tab now controlled."""
        for t in self._list_targets():
            if t.get("type") == "page" and t.get("id") == page_id:
                ws_url = t.get("webSocketDebuggerUrl")
                if not ws_url:
                    raise RuntimeError("page %s has no WebSocket URL"
                                       % page_id)
                self._switch_to(ws_url)
                self._url = t.get("url") or "about:blank"
                return {"id": t.get("id"), "title": t.get("title"),
                        "url": t.get("url")}
        raise ValueError("no open page with id %r" % page_id)

    def close(self):
        """Close CDP, terminate Chromium, remove a temporary profile."""
        try:
            if getattr(self, "_cdp", None) is not None:
                self._cdp.close()
        except Exception:
            pass
        self._cdp = None
        proc = getattr(self, "_proc", None)
        if proc is not None and proc.poll() is None:
            try:
                proc.terminate()
                proc.wait(timeout=5)
            except Exception:
                try:
                    proc.kill()
                except Exception:
                    pass
        self._proc = None
        try:
            self._log.close()
        except Exception:
            pass
        if self._tmp_profile and self._user_data_dir:
            shutil.rmtree(self._user_data_dir, ignore_errors=True)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False

    # -- navigation / evaluation -----------------------------------------

    def goto(self, url, timeout=30):
        """Navigate to ``url`` and wait for the document to finish
        loading. Returns ``{"url", "title"}`` (final URL after any
        redirect)."""
        self._cdp.call("Page.navigate", {"url": url})
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                if self.evaluate("document.readyState") == "complete":
                    break
            except Exception:
                pass
            time.sleep(0.3)
        self._url = self.url()
        return {"url": self._url, "title": self.title()}

    def evaluate(self, js, await_promise=False):
        """Evaluate ``js`` in the page; return its value (by value).

        Raises RuntimeError when the page JS throws."""
        res = self._cdp.call("Runtime.evaluate", {
            "expression": js, "returnByValue": True,
            "awaitPromise": await_promise})
        if "exceptionDetails" in res:
            exc = res["exceptionDetails"].get("exception", {})
            msg = exc.get("description") or exc.get("value") or res[
                "exceptionDetails"]
            raise RuntimeError("page JS error: %s" % (msg,))
        result = res.get("result", {})
        return result.get("value")

    def url(self):
        """Current page URL (after redirects)."""
        try:
            return self.evaluate("location.href")
        except Exception:
            return self._url

    def title(self):
        """Current page title."""
        try:
            return self.evaluate("document.title") or ""
        except Exception:
            return ""

    def text(self, limit=2000):
        """Current page visible text (snipped)."""
        try:
            return (self.evaluate(
                "document.body ? document.body.innerText : ''") or "")[:limit]
        except Exception:
            return ""

    # -- cookies -----------------------------------------------------------

    def _network_enable(self):
        if not getattr(self, "_network_enabled", False):
            self._cdp.call("Network.enable")
            self._network_enabled = True

    def get_cookies(self, urls=None):
        """All cookies for ``urls`` (default: the current page URL),
        as raw CDP cookie dicts (name, value, domain, path, expires,
        httpOnly, secure, session, sameSite). Values are secrets:
        callers must not log or print them."""
        self._network_enable()
        params = {}
        if urls:
            params["urls"] = list(urls)
        res = self._cdp.call("Network.getCookies", params)
        return res.get("cookies", [])

    def set_cookies(self, cookies):
        """Set cookies from Cookie-Editor style dicts. Each item needs
        name+value and either url or domain. Returns how many were set."""
        self._network_enable()
        cleaned = []
        for c in cookies or []:
            if not c.get("name") or c.get("value") is None:
                continue
            item = {"name": c["name"], "value": str(c["value"])}
            if c.get("url"):
                item["url"] = c["url"]
            elif c.get("domain"):
                item["domain"] = c["domain"]
            else:
                continue
            for k in ("path", "secure", "httpOnly", "sameSite"):
                if c.get(k) is not None:
                    item[k] = c[k]
            exp = c.get("expires") or c.get("expirationDate")
            if exp and float(exp) > 0:
                item["expires"] = float(exp)
            cleaned.append(item)
        if cleaned:
            self._cdp.call("Network.setCookies", {"cookies": cleaned})
        return len(cleaned)

    def delete_cookies(self, name=None, url=None, domain=None):
        """Delete cookies matching name (and optionally url/domain)."""
        self._network_enable()
        params = {}
        if name:
            params["name"] = name
        if url:
            params["url"] = url
        if domain:
            params["domain"] = domain
        self._cdp.call("Network.deleteCookies", params)
        return True

    def clear_cookies(self):
        """Remove ALL cookies in the browser profile."""
        self._network_enable()
        self._cdp.call("Network.clearBrowserCookies")
        return True

    # -- elements ----------------------------------------------------------

    def elements(self):
        """All interactive elements as a numbered list of dicts.

        Same fields as :func:`yousj.interact.interactive_elements`
        (``id``, ``tag``, ``kind``, ``type``, ``text``, ``href``,
        ``name``, ``html_id``, ``value``, ``disabled``, ``onclick``,
        ``role``, ``tag_index``, ``form_index``, ``checked``,
        ``options``, ``label_for``, ``wraps_control``, ``url``,
        ``box``, ``touchable``, ``touchable_reason``) — except ``box``
        is a REAL rendered box from getBoundingClientRect, not an
        estimate. ``id`` is 1-based document order, stable per page
        load (re-list after navigation or DOM changes).
        """
        raw = self.evaluate(_ELEMENTS_JS)
        if not isinstance(raw, list):
            return []
        cur = self.url()
        out = []
        for i, it in enumerate(raw, 1):
            it = dict(it)
            it["id"] = i
            href = it.get("href") or ""
            if href and cur:
                it["url"] = urllib.parse.urljoin(cur, href)
            else:
                it["url"] = href
            touchable, reason = _touchable(it)
            it["touchable"] = touchable
            it["touchable_reason"] = reason
            out.append(it)
        return out

    def format_elements(self, elements=None):
        """Render :meth:`elements` as AI-readable text (same line format
        as :func:`yousj.interact.format_elements`)."""
        if elements is None:
            elements = self.elements()
        return _format_elements(elements)

    # -- actions -----------------------------------------------------------

    def screenshot(self, path=None, full_page=False):
        """A REAL rendering screenshot (PNG).

        Returns ``{"format", "width", "height", "bytes", "path",
        "note"}``. ``full_page=True`` captures beyond the viewport
        (best-effort; height then reflects content height). This is a
        photograph of the rendered page — not the from-scratch
        engine's wireframe.
        """
        params = {"format": "png"}
        if full_page:
            params["captureBeyondViewport"] = True
        res = self._cdp.call("Page.captureScreenshot", params)
        data = base64.b64decode(res.get("data", ""))
        height = self.height
        if full_page:
            try:
                height = int(self.evaluate(
                    "Math.max(document.body ? document.body.scrollHeight : 0,"
                    " document.documentElement.scrollHeight)"))
            except Exception:
                pass
        if path:
            with open(path, "wb") as f:
                f.write(data)
        return {"format": "png", "width": self.width, "height": height,
                "bytes": len(data), "path": path,
                "note": "real rendering screenshot from Chromium "
                        "(not a wireframe)"}

    def activate(self, element_id):
        """Really click interactive element ``element_id`` and report
        what happened (same result shape as
        :func:`yousj.interact.activate`, but the click is a genuine
        pointer event and ``changed`` reflects the real page).

        Returns ``{"action", "id", "tag", "kind", "box", "new_url",
        "new_title", "text_before", "text_after", "changed",
        "console", "js_error", "note"}``.
        """
        before_text = self.text()
        before_url = self.url()
        els = self.elements()
        el = next((e for e in els if e["id"] == element_id), None)
        if el is None:
            return {"action": "error", "id": element_id, "tag": "",
                    "kind": "", "box": None, "new_url": before_url,
                    "new_title": self.title(), "text_before": _snip(before_text),
                    "text_after": _snip(before_text), "changed": False,
                    "console": [], "js_error": None,
                    "note": "no interactive element with id %r (have 1..%d)"
                          % (element_id, len(els))}
        if el.get("disabled"):
            return {"action": "info", "id": element_id, "tag": el["tag"],
                    "kind": el["kind"], "box": el.get("box"),
                    "new_url": before_url, "new_title": self.title(),
                    "text_before": _snip(before_text),
                    "text_after": _snip(before_text), "changed": False,
                    "console": [], "js_error": None,
                    "note": "element is disabled; browsers do not "
                            "activate disabled controls"}
        self.mouse.click_element(element_id)
        time.sleep(0.5)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                if self.evaluate("document.readyState") == "complete":
                    break
            except Exception:
                break
            time.sleep(0.3)
        after_text = self.text()
        after_url = self.url()
        return {"action": "real_click", "id": element_id, "tag": el["tag"],
                "kind": el["kind"], "box": el.get("box"),
                "new_url": after_url, "new_title": self.title(),
                "text_before": _snip(before_text),
                "text_after": _snip(after_text),
                "changed": (before_text != after_text
                            or before_url != after_url),
                "console": list(self._cdp.console), "js_error": None,
                "note": "real pointer click dispatched via CDP Input"}

    def fill(self, element_id, text):
        """Really type ``text`` into input/textarea ``element_id``
        (focus + Input.insertText, i.e. the real keyboard path).

        Returns ``{"id", "tag", "text", "value_after"}``. Selects are
        set via JS value + change event (insertText does not apply)
        and noted as such.
        """
        els = self.elements()
        el = next((e for e in els if e["id"] == element_id), None)
        if el is None:
            raise ValueError("no interactive element with id %r"
                             % (element_id,))
        expr = _element_js(el)
        if expr is None:
            raise ValueError("cannot address element %d from JS"
                             % element_id)
        if el["kind"] == "select":
            self.evaluate(
                "%s.value = %s; %s.dispatchEvent(new Event('change', "
                "{bubbles: true}));" % (expr, _js_string(text), expr))
            note = "select set via JS value + change event"
        else:
            self.evaluate("%s.focus();" % expr)
            self._cdp.call("Input.insertText", {"text": text})
            note = "typed via Input.insertText (real keyboard path)"
        value_after = self.evaluate("%s.value" % expr)
        return {"id": element_id, "tag": el["tag"], "text": text,
                "value_after": value_after, "note": note}

    def drag_slider(self, handle_id=None, target_x=None, target_id=None,
                    check_js=None, human=True, dry_run=False, rng=None,
                    overshoot=None):
        """Drag a slider handle to a target pixel and report the page's
        verdict — the slider CAPTCHA gesture, for real.

        ``handle_id`` is an elements-list id (e.g. the ``role=slider``
        handle); ``target_x`` is the destination pixel (same y as the
        handle center), or ``target_id`` for another element's center x.
        ``check_js`` is JS evaluated after the drop that returns the
        page's slider state (e.g. a status string); pass it to prove
        the drag worked — the path alone is not proof.

        Returns ``{"from", "to", "path_points", "path", "handle_box_before",
        "handle_box_after", "state_after", "note"}``. The drag walks a
        :func:`yousj.human.human_path` trajectory (variable speed,
        wobble, pauses, possible overshoot-and-pullback) while the
        button is held, so the page sees a human drag, not a teleport.
        """
        if handle_id is None:
            raise ValueError("drag_slider needs handle_id (elements id)")
        els = self.elements()
        handle = next((e for e in els if e["id"] == handle_id), None)
        if handle is None:
            raise ValueError("no interactive element with id %r"
                             % (handle_id,))
        box = handle.get("box") or {}
        if box.get("width", 0) <= 0 or box.get("height", 0) <= 0:
            raise ValueError(
                "slider handle %d has no usable box" % handle_id)
        start_x = int(round(box["x"] + box["width"] / 2.0))
        start_y = int(round(box["y"] + box["height"] / 2.0))
        if target_id is not None:
            tgt = next((e for e in els if e["id"] == target_id), None)
            if tgt is None:
                raise ValueError("no interactive element with id %r"
                                 % (target_id,))
            tb = tgt.get("box") or {}
            target_x = int(round(tb["x"] + tb["width"] / 2.0))
        if target_x is None:
            raise ValueError("drag_slider needs target_x or target_id")
        target_x = int(target_x)
        res = self.mouse.drag(start_x, start_y, target_x, start_y,
                              human=human, dry_run=dry_run, rng=rng,
                              overshoot=overshoot)
        time.sleep(0.5)
        state_after = None
        if check_js:
            try:
                state_after = self.evaluate(check_js)
            except Exception as e:  # noqa: BLE001 - reported, not raised
                state_after = "check_js failed: %s" % (e,)
        box_after = None
        try:
            els2 = self.elements()
            h2 = next((e for e in els2 if e["id"] == handle_id), None)
            box_after = h2.get("box") if h2 else None
        except Exception:
            pass
        return {"from": res["from"], "to": res["to"],
                "path_points": res["points"], "path": res["path"],
                "handle_box_before": box, "handle_box_after": box_after,
                "state_after": state_after,
                "note": "real drag via CDP Input; verify state_after, "
                        "not the path alone"}
