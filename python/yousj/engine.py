"""ctypes bindings to the Yousj engine (Rust cdylib + C helpers)."""
import ctypes
import json
import os
import warnings

# The fetch C callback returns a c_char_p pointing to Python-owned bytes;
# Rust copies it synchronously during the call, so there is no real leak,
# but ctypes warns anyway. Silence that specific warning.
warnings.filterwarnings(
    "ignore",
    message="memory leak in callback function",
    category=RuntimeWarning,
)


def _default_lib_path() -> str:
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.normpath(
        os.path.join(here, "..", "..", "engine", "target", "release",
                     "libyousj_engine.so")
    )


_LIB_PATH = os.environ.get("YOUSJ_ENGINE_LIB", _default_lib_path())

_lib = ctypes.CDLL(_LIB_PATH)

_lib.yousj_parse.argtypes = [ctypes.c_char_p, ctypes.c_size_t]
_lib.yousj_parse.restype = ctypes.c_void_p

_lib.yousj_free_doc.argtypes = [ctypes.c_void_p]
_lib.yousj_free_doc.restype = None

_lib.yousj_title.argtypes = [ctypes.c_void_p]
_lib.yousj_title.restype = ctypes.c_void_p
_lib.yousj_text.argtypes = [ctypes.c_void_p]
_lib.yousj_text.restype = ctypes.c_void_p
_lib.yousj_links.argtypes = [ctypes.c_void_p]
_lib.yousj_links.restype = ctypes.c_void_p
_lib.yousj_anchors.argtypes = [ctypes.c_void_p]
_lib.yousj_anchors.restype = ctypes.c_void_p
_lib.yousj_dom_tree.argtypes = [ctypes.c_void_p]
_lib.yousj_dom_tree.restype = ctypes.c_void_p

_lib.yousj_forms.argtypes = [ctypes.c_void_p]
_lib.yousj_forms.restype = ctypes.c_void_p

_lib.yousj_interactive.argtypes = [ctypes.c_void_p]
_lib.yousj_interactive.restype = ctypes.c_void_p

_lib.yousj_scripts.argtypes = [ctypes.c_void_p]
_lib.yousj_scripts.restype = ctypes.c_void_p

_lib.yousj_layout.argtypes = [ctypes.c_void_p, ctypes.c_int]
_lib.yousj_layout.restype = ctypes.c_void_p

_lib.yousj_free_str.argtypes = [ctypes.c_void_p]
_lib.yousj_free_str.restype = None

# Optional: only present when the engine was built with --features js
# (see build-js.sh). Missing symbol -> AttributeError -> clear message.
try:
    _lib.yousj_run_js.argtypes = [ctypes.c_void_p, ctypes.c_char_p,
                                  ctypes.c_size_t]
    _lib.yousj_run_js.restype = ctypes.c_void_p
    _HAS_JS = True
except AttributeError:
    _HAS_JS = False

# Optional: fetch-enabled JS entry point (same build flag).
# The callback receives the URL as c_char_p and returns JSON
# {"status":200,"body":"..."} or {"error":"..."} as c_char_p.
_FETCH_CB = ctypes.CFUNCTYPE(ctypes.c_char_p, ctypes.c_char_p)
try:
    _lib.yousj_run_js_with_fetch.argtypes = [ctypes.c_void_p, ctypes.c_char_p,
                                             ctypes.c_size_t, _FETCH_CB]
    _lib.yousj_run_js_with_fetch.restype = ctypes.c_void_p
    _HAS_JS_FETCH = True
except AttributeError:
    _HAS_JS_FETCH = False


# Optional: debug-enabled JS entry point (phase 8, same build flag).
# Takes a JSON array of 1-based breakpoint line numbers; returns JSON with
# an extra "debug_hits" array of pause snapshots.
try:
    _lib.yousj_run_js_debug.argtypes = [ctypes.c_void_p, ctypes.c_char_p,
                                       ctypes.c_size_t, ctypes.c_char_p,
                                       ctypes.c_size_t]
    _lib.yousj_run_js_debug.restype = ctypes.c_void_p
    _HAS_JS_DEBUG = True
except AttributeError:
    _HAS_JS_DEBUG = False


def _fetch_via_net_get(url_ptr) -> bytes:
    """C callback: fetch a URL via yousj.net.get (SSRF-checked, fail-closed).

    Returns JSON {"status":200,"body":...} on success (net.get returns the
    body directly; success implies 200), {"error":...} on failure.
    The returned bytes stay alive on this frame while Rust copies them.
    """
    from yousj import net as _net
    url = ctypes.cast(url_ptr, ctypes.c_char_p).value.decode("utf-8", errors="replace")
    try:
        body = _net.get(url)
        return json.dumps({"status": 200, "body": body}).encode("utf-8")
    except Exception as e:  # noqa: BLE001 - surfaced to JS as rejection
        return json.dumps({"error": "%s: %s" % (type(e).__name__, e)}).encode("utf-8")


def _take_str(ptr) -> str:
    if not ptr:
        return ""
    s = ctypes.cast(ptr, ctypes.c_char_p).value or b""
    _lib.yousj_free_str(ptr)
    return s.decode("utf-8", errors="replace")


class Document:
    """A parsed HTML document. Backed by the Rust engine."""

    def __init__(self, ptr):
        if not ptr:
            raise ValueError("engine failed to parse document")
        self._ptr = ptr
        # Source URL when obtained via yousj.fetch() (used to resolve
        # relative form actions). None for parse(html) from a string.
        self.url = None

    def __del__(self):
        ptr, self._ptr = self._ptr, None
        if ptr:
            _lib.yousj_free_doc(ptr)

    def title(self) -> str:
        return _take_str(_lib.yousj_title(self._ptr))

    def text(self) -> str:
        return _take_str(_lib.yousj_text(self._ptr))

    def links(self):
        raw = _take_str(_lib.yousj_links(self._ptr))
        return [l for l in raw.split("\n") if l]

    def anchors(self):
        """List of (href, anchor_text) in document order."""
        raw = _take_str(_lib.yousj_anchors(self._ptr))
        out = []
        for line in raw.split("\n"):
            if not line.strip():
                continue
            href, _, text = line.partition("\t")
            out.append((href, text))
        return out

    def dom_tree(self) -> str:
        """Indented DOM tree text (Elements panel)."""
        return _take_str(_lib.yousj_dom_tree(self._ptr))

    def forms(self):
        """All <form> elements as :class:`yousj.forms.Form` objects.

        Example::

            form = doc.forms()[0]
            result = form.fill({"user": "alice"}).submit()
        """
        from . import forms as _forms
        return _forms.forms(self)

    def interactive_elements(self, viewport_width: int = 1280,
                             with_boxes: bool = True):
        """All interactive elements as a numbered list of dicts.

        See :mod:`yousj.interact` for the item schema. Numbers are
        stable within this document (document order); refer to
        elements by number in :meth:`activate`. Each item also carries
        ``box`` (``{"x","y","width","height"}`` pixel coordinates from
        :mod:`yousj.layout`, or None) and ``touchable`` (bool) plus
        ``touchable_reason`` (why), unless ``with_boxes=False`` (then
        ``box`` is None and no layout pass runs).
        """
        from . import interact as _interact
        return _interact.interactive_elements(
            self, viewport_width=viewport_width, with_boxes=with_boxes)

    def format_elements(self) -> str:
        """AI-readable text of :meth:`interactive_elements`."""
        from . import interact as _interact
        return _interact.format_elements(self.interactive_elements())

    def layout(self, viewport_width: int = 1280) -> dict:
        """Pixel layout of this document (see :mod:`yousj.layout`)."""
        from . import layout as _layout
        return _layout.layout(self, viewport_width=viewport_width)

    def element_box(self, element_id: int, viewport_width: int = 1280):
        """Pixel box of interactive element ``element_id``, or None.

        See :mod:`yousj.layout` for the join rule and the None cases.
        """
        from . import layout as _layout
        return _layout.element_box(self, element_id,
                                   viewport_width=viewport_width)

    def screenshot(self, path=None, format: str = "png",
                   viewport_width: int = 1280, viewport_height: int = 800,
                   full_page: bool = False, mouse_pos=None,
                   highlight_box=None):
        """Wireframe screenshot of this document (see :mod:`yousj.screenshot`).

        Returns the :func:`yousj.screenshot.capture` dict (format /
        width / height / bytes / path / note); writes to ``path`` when
        given.
        """
        from . import screenshot as _shot
        return _shot.capture(self, path=path, format=format,
                             viewport_width=viewport_width,
                             viewport_height=viewport_height,
                             full_page=full_page, mouse_pos=mouse_pos,
                             highlight_box=highlight_box)

    def activate(self, element_id, timeout: int = 15, pace: bool = True,
                 viewport_width: int = 1280):
        """Activate interactive element ``element_id`` (1-based).

        See :mod:`yousj.interact` for the structured result schema
        (action / new_url / new_title / changed / note).
        """
        from . import interact as _interact
        return _interact.activate(self, element_id, timeout=timeout,
                                  pace=pace, viewport_width=viewport_width)

    def run_js(self, js: str, fetch: bool = False, debug: bool = False,
               breakpoints=None) -> dict:
        """Run JavaScript against this document's live DOM.

        Mutations (e.g. ``document.getElementById('t').textContent = 'hi'``)
        apply immediately; re-read via :meth:`text` / :meth:`dom_tree`.

        ``fetch=True`` wires the JS ``fetch()`` global to :mod:`yousj.net`
        (via a C callback); without it ``fetch()`` returns a rejected
        promise ("fetch not implemented").

        ``debug=True`` attaches the phase-8 debugger: ``breakpoints`` is a
        list of 1-based line numbers; the result gains a ``"debug_hits"``
        list of pause snapshots
        (``{"line", "col", "stack": [...], "vars": {...}}``).

        Returns ``{"console": [...], "error": None | "message"}``
        (plus ``"debug_hits"`` when ``debug=True``).
        Raises RuntimeError when the engine wasn't built with the ``js``
        feature.
        """
        if not _HAS_JS:
            raise RuntimeError(
                "engine built without `js` feature; rebuild with: "
                "cargo build --release --features js (see build-js.sh)")
        data = js.encode("utf-8")
        if debug:
            if not _HAS_JS_DEBUG:
                raise RuntimeError(
                    "engine built without debug-enabled JS entry point; "
                    "rebuild with build-js.sh")
            bps = json.dumps(list(breakpoints or [])).encode("utf-8")
            raw = _take_str(_lib.yousj_run_js_debug(
                self._ptr, data, len(data), bps, len(bps)))
            if not raw:
                return {"console": [], "error": "no output from engine",
                        "debug_hits": []}
            return json.loads(raw)
        if fetch:
            if not _HAS_JS_FETCH:
                raise RuntimeError(
                    "engine built without fetch-enabled JS entry point; "
                    "rebuild with build-js.sh")
            cb = _FETCH_CB(_fetch_via_net_get)
            raw = _take_str(_lib.yousj_run_js_with_fetch(
                self._ptr, data, len(data), cb))
        else:
            raw = _take_str(_lib.yousj_run_js(self._ptr, data, len(data)))
        if not raw:
            return {"console": [], "error": "no output from engine"}
        return json.loads(raw)


def parse(html: str) -> Document:
    data = html.encode("utf-8")
    ptr = _lib.yousj_parse(data, len(data))
    return Document(ptr)
