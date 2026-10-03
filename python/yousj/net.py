"""HTTP fetching for the Yousj browser (Python layer, stdlib only)."""
import gzip
import time
import urllib.request


UA = "YousjBrowser/0.2 (headless; for AI agents)"

HEADERS = {
    "User-Agent": UA,
    "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    "Accept-Language": "en-US,en;q=0.9",
}

# Every request made through net.get() is recorded here for the
# Network panel (F12). Each entry: url, method, status, bytes, ms, error.
request_log = []


def clear_log():
    del request_log[:]


def get(url: str, timeout: int = 15) -> str:
    t0 = time.monotonic()
    entry = {"url": url, "method": "GET", "status": None,
             "bytes": 0, "ms": 0.0, "error": None}
    try:
        req = urllib.request.Request(url, headers=HEADERS)
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            if r.headers.get("Content-Encoding") == "gzip":
                raw = gzip.decompress(raw)
            entry["status"] = r.status
            entry["bytes"] = len(raw)
            charset = r.headers.get_content_charset() or "utf-8"
            try:
                return raw.decode(charset, errors="replace")
            except (LookupError, ValueError):
                return raw.decode("utf-8", errors="replace")
    except Exception as e:  # noqa: BLE001 - recorded for the Network panel
        entry["error"] = "%s: %s" % (type(e).__name__, e)
        raise
    finally:
        entry["ms"] = round((time.monotonic() - t0) * 1000, 1)
        request_log.append(entry)
