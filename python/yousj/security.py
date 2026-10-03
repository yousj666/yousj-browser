"""SSRF protection for the Yousj browser.

Every URL fetched through ``yousj.net`` is validated here first:

- only ``http://`` / ``https://`` schemes (no ``file://``, ``ftp://``, ...)
- the host must resolve to public IPs — loopback, RFC1918, link-local
  (e.g. cloud metadata ``169.254.169.254``), multicast and reserved ranges
  are blocked, including literal IPs in the URL
- every redirect hop is validated too (no 302 bypass to an intranet address)
- fail closed: unverifiable URLs are not fetched

If a blocked URL really must be visited, there is a **two-layer risk
confirmation** flow (built for AI agents — plain text plus choices)::

    from yousj import security, net

    p1 = security.warn_first(url)
    # {"text": "此网站危险，不建议访问。原因：…",
    #  "choices": ["退出不访问（推荐）", "访问（不推荐）"]}
    # AI picks "访问（不推荐）" ->
    p2 = security.warn_second(url)
    # {"text": "确定要访问吗？如果出了事概不负责。",
    #  "choices": ["不访问", "访问"]}
    # AI picks "访问" ->
    token = security.confirm_visit(url)   # one-time bypass token
    html = net.get(url, bypass_token=token)

Layers can't be skipped (each step requires the previous one, within
10 minutes). The token is single-use, bound to the exact URL, and expires
after 10 minutes. A redirect hop that is itself blocked needs its own
confirmation.

Global opt out (only if you know what you're doing, e.g. scraping your own
intranet)::

    yousj.settings.set("allow_private_urls", True)

Note: the check is resolve-then-validate. A hostile DNS that changes its
answer between the check and the connection (DNS rebinding) is out of scope
for this layer.
"""
import ipaddress
import secrets
import socket
import time
import urllib.parse


class SecurityError(ValueError):
    """Raised when a URL is blocked by SSRF protection."""


ALLOWED_SCHEMES = {"http", "https"}

# RFC 2544 benchmarking range. Not globally routable, but sandboxed /
# proxied environments (like CI sandboxes) legitimately resolve public
# domains into it, so it is exempted from the private-IP block below.
# Outside such environments no public DNS serves it, so allowing it is safe.
_BENCH_RANGE = ipaddress.ip_network("198.18.0.0/15")

# Two-layer confirmation state: url -> timestamp. 10-minute window.
_CONFIRM_WINDOW = 600
_stage1 = {}
_stage2 = {}
_bypasses = {}  # token -> (url, expires)


def _allow_private() -> bool:
    from . import settings  # lazy: avoids any import-cycle surprise
    return bool(settings.get("allow_private_urls", False))


def _blocked_ip(ip: "ipaddress._BaseAddress") -> bool:
    """True when an IP must never be fetched (SSRF)."""
    if ip in _BENCH_RANGE:
        return False
    return (ip.is_loopback or ip.is_link_local or ip.is_multicast
            or ip.is_unspecified or ip.is_reserved or ip.is_private)


def _assess(url: str):
    """Return (blocked: bool, reason: str). Reason is "" when allowed."""
    parts = urllib.parse.urlsplit(url)
    scheme = parts.scheme.lower()
    if scheme not in ALLOWED_SCHEMES:
        return True, "URL scheme %r 不在允许范围（只允许 http/https）" % (
            scheme or url[:20],)
    host = parts.hostname
    if not host:
        return True, "URL 没有主机名"
    if _allow_private():
        return False, ""
    try:
        ip = ipaddress.ip_address(host)
    except ValueError:
        ip = None
    if ip is not None:
        if _blocked_ip(ip):
            return True, "主机 %s 是非公网 IP" % host
        return False, ""
    try:
        infos = socket.getaddrinfo(host, None, type=socket.SOCK_STREAM)
    except socket.gaierror as e:
        return True, "主机 %r DNS 解析失败：%s" % (host, e)
    ips = {info[4][0] for info in infos}
    if not ips:
        return True, "主机 %r 未解析到任何地址" % host
    for ip_str in sorted(ips):
        if _blocked_ip(ipaddress.ip_address(ip_str)):
            return True, "主机 %r 解析到非公网 IP %s" % (host, ip_str)
    return False, ""


def _prune(stage: dict) -> None:
    now = time.time()
    for k in [k for k, t in stage.items() if now - t > _CONFIRM_WINDOW]:
        del stage[k]


def warn_first(url: str) -> dict:
    """Layer 1: warn that the site is dangerous.

    Returns ``{"allowed": True, ...}`` when the URL isn't blocked (no
    confirmation needed), otherwise ``{"allowed": False, "text": ...,
    "choices": ["退出不访问（推荐）", "访问（不推荐）"]}``.
    """
    blocked, reason = _assess(url)
    if not blocked:
        return {"allowed": True, "text": "该 URL 未被拦截，无需风险确认。"}
    _prune(_stage1)
    _stage1[url] = time.time()
    return {
        "allowed": False,
        "text": "此网站危险，不建议访问。原因：%s" % reason,
        "choices": ["退出不访问（推荐）", "访问（不推荐）"],
    }


def warn_second(url: str) -> dict:
    """Layer 2: final confirmation. Requires :func:`warn_first` first."""
    _prune(_stage1)
    if url not in _stage1:
        raise SecurityError("请先调用 warn_first(url) 完成第一层风险提示")
    _prune(_stage2)
    _stage2[url] = time.time()
    return {
        "text": "确定要访问吗？如果出了事概不负责。",
        "choices": ["不访问", "访问"],
    }


def confirm_visit(url: str):
    """Issue a one-time bypass token after both warning layers.

    Requires :func:`warn_second` first. Returns ``None`` when the URL
    doesn't need a bypass. The token is single-use, bound to the exact
    URL, and expires after 10 minutes.
    """
    _prune(_stage2)
    if url not in _stage2:
        raise SecurityError("请先完成两层风险提示（warn_first → warn_second）")
    blocked, _ = _assess(url)
    if not blocked:
        return None
    token = secrets.token_urlsafe(16)
    _bypasses[token] = (url, time.time() + _CONFIRM_WINDOW)
    return token


def _use_bypass(url: str, token) -> bool:
    if not token:
        return False
    rec = _bypasses.pop(token, None)  # single-use
    if rec is None:
        return False
    want_url, expires = rec
    return want_url == url and expires >= time.time()


def validate_url(url: str, bypass_token=None) -> str:
    """Validate a URL for fetching. Returns it unchanged.

    Raises :class:`SecurityError` when the URL must not be fetched.
    """
    if _use_bypass(url, bypass_token):
        return url
    blocked, reason = _assess(url)
    if blocked:
        raise SecurityError(
            "%s。如坚持访问，请走两层风险确认："
            "yousj.security.warn_first(url) → warn_second(url) → "
            "confirm_visit(url)，再用 token 调用 "
            "net.get(url, bypass_token=token)" % reason)
    return url
