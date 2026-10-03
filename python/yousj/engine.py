"""ctypes bindings to the Yousj engine (Rust cdylib + C helpers)."""
import ctypes
import os


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

_lib.yousj_free_str.argtypes = [ctypes.c_void_p]
_lib.yousj_free_str.restype = None


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


def parse(html: str) -> Document:
    data = html.encode("utf-8")
    ptr = _lib.yousj_parse(data, len(data))
    return Document(ptr)
