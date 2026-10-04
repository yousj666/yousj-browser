"""HTTP fetching for the Yousj browser (Python layer, stdlib only).

Every request goes through ``yousj.security`` first (SSRF protection:
no file:// URLs, no intranet/private IPs, every redirect hop validated).
Response bodies are size-capped (DoS protection, gzip bombs included).

V5: cookie persistence (MozillaCookieJar at ~/.config/yousj/cookies.txt,
mode 600), proxy support (``yousj.settings`` ``proxy`` key or
http_proxy/https_proxy env vars), POST, and streaming downloads.
"""
import http.cookiejar
import os
import time
import urllib.parse
import urllib.request
import zlib

from . import security
from . import settings


UA = "YousjBrowser/5.0 (headless; for AI agents)"

HEADERS = {
    "User-Agent": UA,
    "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    "Accept-Language": "en-US,en;q=0.9",
}

_REDIRECTS = {301, 302, 303, 307, 308}

_COOKIE_PATH = os.path.join(os.path.expanduser("~"), ".config", "yousj",
                            "cookies.txt")


# ---------------------------------------------------------------------------
# Cookies: persistent session (login once, reuse next time)
# ---------------------------------------------------------------------------

def _load_jar() -> http.cookiejar.MozillaCookieJar:
    jar = http.cookiejar.MozillaCookieJar(_COOKIE_PATH)
    try:
        jar.load(ignore_discard=True, ignore_expires=True)
    except (OSError, http.cookiejar.LoadError):
        pass
    return jar


_jar = _load_jar()


def _save_jar() -> None:
    """Persist cookies to disk (mode 600 — they may contain session ids)."""
    try:
        os.makedirs(os.path.dirname(_COOKIE_PATH), exist_ok=True)
        _jar.save(ignore_discard=True, ignore_expires=True)
        os.chmod(_COOKIE_PATH, 0o600)
    except OSError:
        pass


def clear_cookies() -> None:
    """Forget the whole session (log out everywhere)."""
    _jar.clear()
    _save_jar()


def cookie_count() -> int:
    return len(_jar)


# ---------------------------------------------------------------------------
# Proxy: settings key ``proxy`` wins, then http_proxy/https_proxy env vars
# ---------------------------------------------------------------------------

def _proxy_url() -> "str | None":
    cfg = settings.get("proxy")
    if cfg:
        return cfg
    for var in ("https_proxy", "http_proxy", "HTTPS_PROXY", "HTTP_PROXY"):
        if os.environ.get(var):
            return os.environ[var]
    return None


class _NoAutoRedirect(urllib.request.HTTPRedirectHandler):
    """Don't follow redirects automatically: _request() walks the chain
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


# Opener is rebuilt when the effective proxy changes (cheap enough to
# cache; the cookie jar object is shared so cookies stay live).
_opener_cache = {}


def _get_opener():
    proxy = _proxy_url()
    if proxy not in _opener_cache:
        handlers = [_NoAutoRedirect(),
                    urllib.request.HTTPCookieProcessor(_jar)]
        if proxy:
            handlers.append(urllib.request.ProxyHandler(
                {"http": proxy, "https": proxy}))
        _opener_cache[proxy] = urllib.request.build_opener(*handlers)
    return _opener_cache[proxy]


def active_proxy():
    """The proxy URL currently in effect (None = direct)."""
    return _proxy_url()


# Every request made through net is recorded here for the
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


def _decode(raw: bytes, headers) -> str:
    charset = headers.get_content_charset() or "utf-8"
    try:
        return raw.decode(charset, errors="replace")
    except (LookupError, ValueError):
        return raw.decode("utf-8", errors="replace")


def _request(method: str, url: str, data: bytes = None,
             headers: dict = None, timeout: int = 15, max_redirects: int = 5,
             bypass_token=None, max_bytes: int = 10_000_000):
    """Shared redirect-walking request core.

    Every hop is validated by ``yousj.security`` (SSRF protection cannot
    be bypassed here). Returns ``(final_url, status, headers, body_bytes)``.
    """
    current = url
    hops = 0
    first = True
    body = data
    while True:
        entry = {"url": current, "method": method, "status": None,
                 "bytes": 0, "ms": 0.0, "error": None}
        t0 = time.monotonic()
        try:
            # The bypass token (if any) only applies to the first hop.
            security.validate_url(current,
                                  bypass_token=bypass_token if first else None)
            first = False
            h = dict(HEADERS)
            if headers:
                h.update(headers)
            req = urllib.request.Request(current, data=body, headers=h,
                                         method=method)
            with _get_opener().open(req, timeout=timeout) as r:
                if r.status in _REDIRECTS:
                    entry["status"] = r.status
                    loc = r.headers.get("Location")
                    if not loc:
                        raise RuntimeError("redirect without Location header")
                    if hops >= max_redirects:
                        raise RuntimeError("too many redirects")
                    current = urllib.parse.urljoin(current, loc)
                    hops += 1
                    # 301/302/303 after POST -> GET (standard); 307/308
                    # repeat the original method + body.
                    if r.status in (301, 302, 303):
                        method, body = "GET", None
                    continue
                raw = _read_limited(r, max_bytes)
                entry["status"] = r.status
                entry["bytes"] = len(raw)
                return current, r.status, r.headers, raw
        except Exception as e:  # noqa: BLE001 - recorded for the Network panel
            entry["error"] = "%s: %s" % (type(e).__name__, e)
            raise
        finally:
            entry["ms"] = round((time.monotonic() - t0) * 1000, 1)
            request_log.append(entry)
            _save_jar()


def get(url: str, timeout: int = 15, max_redirects: int = 5,
        bypass_token=None, max_bytes: int = 10_000_000) -> str:
    """Fetch a URL. ``bypass_token`` comes from
    ``yousj.security.confirm_visit`` after the two-layer risk confirmation.
    ``max_bytes`` caps the response body (10MB default, None = unlimited)."""
    _, _, headers, raw = _request("GET", url, timeout=timeout,
                                  max_redirects=max_redirects,
                                  bypass_token=bypass_token,
                                  max_bytes=max_bytes)
    return _decode(raw, headers)


def post(url: str, data: dict, timeout: int = 15, max_redirects: int = 5,
         bypass_token=None, max_bytes: int = 10_000_000) -> str:
    """POST form-encoded ``data`` dict to a URL. Returns the decoded body."""
    body = urllib.parse.urlencode(data).encode("utf-8")
    _, _, headers, raw = _request(
        "POST", url, data=body,
        headers={"Content-Type": "application/x-www-form-urlencoded"},
        timeout=timeout, max_redirects=max_redirects,
        bypass_token=bypass_token, max_bytes=max_bytes)
    return _decode(raw, headers)


def download(url: str, dest: str, timeout: int = 60, max_redirects: int = 5,
             bypass_token=None, progress=None) -> dict:
    """Download a URL to a file (streamed in chunks, SSRF-checked).

    Unlike :func:`get`, there is no 10MB cap — the body is streamed
    straight to disk so large files don't eat RAM. ``progress`` (optional)
    is called as ``progress(bytes_done, total_or_None)``.

    Returns ``{"path": dest, "bytes": n, "url": final_url}``.
    """
    current = url
    hops = 0
    first = True
    while True:
        entry = {"url": current, "method": "GET", "status": None,
                 "bytes": 0, "ms": 0.0, "error": None}
        t0 = time.monotonic()
        try:
            security.validate_url(current,
                                  bypass_token=bypass_token if first else None)
            first = False
            req = urllib.request.Request(current, headers=HEADERS)
            with _get_opener().open(req, timeout=timeout) as r:
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
                total = r.headers.get("Content-Length")
                total = int(total) if total and total.isdigit() else None
                done = 0
                with open(dest, "wb") as f:
                    while True:
                        chunk = r.read(65536)
                        if not chunk:
                            break
                        f.write(chunk)
                        done += len(chunk)
                        if progress:
                            progress(done, total)
                entry["status"] = r.status
                entry["bytes"] = done
                return {"path": dest, "bytes": done, "url": current}
        except Exception as e:  # noqa: BLE001
            entry["error"] = "%s: %s" % (type(e).__name__, e)
            raise
        finally:
            entry["ms"] = round((time.monotonic() - t0) * 1000, 1)
            request_log.append(entry)
            _save_jar()
