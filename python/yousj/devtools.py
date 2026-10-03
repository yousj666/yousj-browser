"""F12 developer tools for the Yousj browser — built for AI agents, not humans.

Panels: Network, Elements, Console, Sources, Performance.

Typical AI usage::

    from yousj import devtools
    s = devtools.inspect("https://example.com")  # opens a Session (like F12)
    s.network.table()      # every HTTP request the engine made
    s.elements.tree()      # indented DOM tree
    s.console.messages()   # engine logs / warnings
    s.sources.html[:500]   # raw fetched HTML
    s.performance.summary()# fetch + parse timings

The Console panel also works standalone (``devtools.Console.log(...)``) so
any layer of the engine can report there.
"""
import time

from . import engine as _engine
from . import net as _net


class Console:
    """Log sink for engine messages. Levels: log / warn / error."""
    _messages = []

    @classmethod
    def _push(cls, level, text):
        cls._messages.append({"t": round(time.time(), 3), "level": level,
                              "text": str(text)})

    @classmethod
    def log(cls, *args):
        cls._push("log", " ".join(map(str, args)))

    @classmethod
    def warn(cls, *args):
        cls._push("warn", " ".join(map(str, args)))

    @classmethod
    def error(cls, *args):
        cls._push("error", " ".join(map(str, args)))

    @classmethod
    def messages(cls, level=None):
        return [m for m in cls._messages
                if level is None or m["level"] == level]

    @classmethod
    def clear(cls):
        del cls._messages[:]


class Network:
    """Read-only view of every HTTP request made through ``yousj.net``."""

    @staticmethod
    def entries():
        return list(_net.request_log)

    @staticmethod
    def clear():
        _net.clear_log()

    @staticmethod
    def table():
        lines = ["%-6s %-4s %6s %8s  %s"
                 % ("STATUS", "TIME", "BYTES", "MS", "URL")]
        for e in _net.request_log:
            status = e["status"] if e["status"] is not None else "ERR"
            lines.append("%-6s %-4s %6d %8.1f  %s"
                         % (status, e["method"], e["bytes"], e["ms"],
                            e["url"][:100]))
            if e["error"]:
                lines.append("         ! %s" % e["error"])
        return "\n".join(lines)

    @staticmethod
    def summary():
        es = _net.request_log
        ok = [e for e in es if e["status"] == 200]
        return {"requests": len(es),
                "ok": len(ok),
                "failed": len(es) - len(ok),
                "bytes": sum(e["bytes"] for e in es),
                "ms": round(sum(e["ms"] for e in es), 1)}


class Elements:
    """DOM inspection (backed by the Rust engine)."""

    def __init__(self, doc):
        self._doc = doc

    def tree(self, max_lines=120):
        lines = self._doc.dom_tree().splitlines()
        if len(lines) > max_lines:
            lines = lines[:max_lines] + ["... (%d more lines)"
                                         % (len(lines) - max_lines)]
        return "\n".join(lines)

    def title(self):
        return self._doc.title()

    def text(self, limit=2000):
        return self._doc.text()[:limit]

    def links(self, limit=50):
        return self._doc.links()[:limit]

    def anchors(self, limit=50):
        return self._doc.anchors()[:limit]


class Sources:
    """Raw source of the fetched page."""

    def __init__(self, html):
        self.html = html

    def head(self, n=800):
        return self.html[:n]


class Performance:
    """Fetch + parse timings for one session."""

    def __init__(self, fetch_ms, parse_ms, html_bytes):
        self.fetch_ms = fetch_ms
        self.parse_ms = parse_ms
        self.html_bytes = html_bytes

    def summary(self):
        return {"fetch_ms": round(self.fetch_ms, 1),
                "parse_ms": round(self.parse_ms, 1),
                "total_ms": round(self.fetch_ms + self.parse_ms, 1),
                "html_bytes": self.html_bytes}


class Session:
    """One inspected page load — the F12 window for a URL."""

    def __init__(self, url, html, doc, fetch_ms, parse_ms):
        self.url = url
        self.network = Network()
        self.elements = Elements(doc)
        self.console = Console
        self.sources = Sources(html)
        self.performance = Performance(fetch_ms, parse_ms, len(html))
        self.doc = doc

    @classmethod
    def open(cls, url, timeout=15):
        """Fetch + parse a URL, like opening DevTools on a fresh page load."""
        Console.log("navigating to %s" % url)
        t0 = time.monotonic()
        try:
            html = _net.get(url, timeout=timeout)
        except Exception as e:  # noqa: BLE001
            Console.error("fetch failed: %s" % e)
            raise
        fetch_ms = (time.monotonic() - t0) * 1000
        t1 = time.monotonic()
        doc = _engine.parse(html)
        parse_ms = (time.monotonic() - t1) * 1000
        Console.log("parsed %d bytes in %.1f ms" % (len(html), parse_ms))
        return cls(url, html, doc, fetch_ms, parse_ms)

    def report(self):
        """One-screen F12 summary for the terminal."""
        p = self.performance.summary()
        n = self.network.summary()
        out = ["=== Yousj F12 : %s ===" % self.url,
               "[Performance] fetch %.1f ms | parse %.1f ms | %d bytes"
               % (p["fetch_ms"], p["parse_ms"], p["html_bytes"]),
               "[Network] %d requests, %d ok, %d bytes total"
               % (n["requests"], n["ok"], n["bytes"]),
               self.network.table(),
               "[Elements] title: %s" % self.elements.title(),
               self.elements.tree(max_lines=40),
               "[Console] %d messages" % len(self.console.messages())]
        for m in self.console.messages()[-5:]:
            out.append("  [%s] %s" % (m["level"], m["text"][:120]))
        return "\n".join(out)


def inspect(url, timeout=15):
    """Open F12 on a URL. Returns a Session."""
    return Session.open(url, timeout=timeout)
