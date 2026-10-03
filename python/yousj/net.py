"""HTTP fetching for the Yousj browser (Python layer, stdlib only).

Every request goes through ``yousj.security`` first (SSRF protection:
no file:// URLs, no intranet/private IPs, every redirect hop validated).
Response bodies are size-capped (DoS protection, gzip bombs included).
"""
import time
import urllib.parse
import urllib.request
import zlib

from . import security


UA = "YousjBrowser/0.2 (headless; for AI agents)"

HEADERS = {
    "User-Agent": UA,
    "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    "Accept-Language": "en-US,en;q=0.9",
}

_REDIRECTS = {301, 302, 303, 307, 308}


class _NoAutoRedirect(urllib.request.HTTPRedirectHandler):
    """Don't follow redirects automatically: net.get() walks the chain
    itself so security.validate_url() sees every hop.

    (Returning the raw response object from the http_error_* handlers is
    the correct way to suppress urllib's redirect following; returning
    None from redirect_request just raises HTTPError.)
    """

    def _no_follow(self, req, fp, code, msg, headers):
        return fp

    http_error_301 = _no_follow
    http_error_302 = _no_follow
    http_error_303 = _no_follow
    http_error_307 = _no_follow
    http_error_308 = _no_follow


_opener = urllib.request.build_opener(_NoAutoRedirect)

# Every request made through net.get() is recorded here for the
# Network panel (F12). Each entry: url, method, status, bytes, ms, error.
request_log = []


def clear_log():
    del request_log[:]


def _read_limited(r, max_bytes):
    """Read the response body with a hard size cap (DoS protection).

    Caps both the raw bytes on the wire and the gzip-decompressed size,
    so a 10KB gzip bomb can't expand into gigabytes of RAM.
    Raises ValueError when the cap is exceeded. ``max_bytes=None`` disables
    the cap (only if you know what you're doing).
    """
    if max_bytes is None:
        raw = r.read()
        if r.headers.get("Content-Encoding") == "gzip":
            raw = zlib.decompress(raw, 16 + zlib.MAX_WBITS)
        return raw
    raw = r.read(max_bytes + 1)
    if len(raw) > max_bytes:
        raise ValueError("response body exceeded %d bytes" % max_bytes)
    if r.headers.get("Content-Encoding") == "gzip":
        decomp = zlib.decompressobj(16 + zlib.MAX_WBITS)
        try:
            data = decomp.decompress(raw, max_bytes + 1)
        except zlib.error as e:
            raise ValueError("bad gzip body: %s" % e)
        if len(data) > max_bytes or decomp.unconsumed_tail:
            raise ValueError(
                "decompressed body exceeded %d bytes" % max_bytes)
        raw = data
    return raw


def get(url: str, timeout: int = 15, max_redirects: int = 5,
        bypass_token=None, max_bytes: int = 10_000_000) -> str:
    """Fetch a URL. ``bypass_token`` comes from
    ``yousj.security.confirm_visit`` after the two-layer risk confirmation.
    ``max_bytes`` caps the response body (10MB default, None = unlimited)."""
    current = url
    hops = 0
    first = True
    while True:
        entry = {"url": current, "method": "GET", "status": None,
                 "bytes": 0, "ms": 0.0, "error": None}
        t0 = time.monotonic()
        try:
            # The bypass token (if any) only applies to the first hop.
            security.validate_url(current,
                                  bypass_token=bypass_token if first else None)
            first = False
            req = urllib.request.Request(current, headers=HEADERS)
            with _opener.open(req, timeout=timeout) as r:
                if r.status in _REDIRECTS:
                    entry["status"] = r.status
                    loc = r.headers.get("Location")
                    if not loc:
                        raise RuntimeError("redirect without Location header")
                    if hops >= max_redirects:
                        raise RuntimeError("too many redirects")
                    current = urllib.parse.urljoin(current, loc)
                    hops += 1
                    continue
                raw = _read_limited(r, max_bytes)
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
