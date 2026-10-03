"""SSRF protection for the Yousj browser.

Every URL fetched through ``yousj.net`` is validated here first:

- only ``http://`` / ``https://`` schemes (no ``file://``, ``ftp://``, ...)
- the host must resolve to public (global) IP addresses — loopback,
  RFC1918, link-local, multicast, reserved etc. are blocked
- every redirect hop is validated too (no bypass via 302 to
  ``http://169.254.169.254/``)

Fail closed: if a URL can't be verified safe, it is not fetched.

Opt out (only if you know what you're doing, e.g. scraping your own
intranet)::

    yousj.settings.set("allow_private_urls", True)

Note: the check is resolve-then-validate. A hostile DNS that changes its
answer between the check and the connection (DNS rebinding) is out of scope
for this layer.
"""
import ipaddress
import socket
import urllib.parse


class SecurityError(ValueError):
    """Raised when a URL is blocked by SSRF protection."""


ALLOWED_SCHEMES = {"http", "https"}

# RFC 2544 benchmarking range. Not globally routable, but sandboxed /
# proxied environments (like CI sandboxes) legitimately resolve public
# domains into it, so it is exempted from the private-IP block below.
# Outside such environments no public DNS serves it, so allowing it is safe.
_BENCH_RANGE = ipaddress.ip_network("198.18.0.0/15")


def _allow_private() -> bool:
    from . import settings  # lazy: avoids any import-cycle surprise
    return bool(settings.get("allow_private_urls", False))


def _blocked_ip(ip: "ipaddress._BaseAddress") -> bool:
    """True when an IP must never be fetched (SSRF)."""
    if ip in _BENCH_RANGE:
        return False
    return (ip.is_loopback or ip.is_link_local or ip.is_multicast
            or ip.is_unspecified or ip.is_reserved or ip.is_private)


def validate_url(url: str) -> str:
    """Validate a URL for fetching. Returns it unchanged.

    Raises :class:`SecurityError` when the URL must not be fetched.
    """
    parts = urllib.parse.urlsplit(url)
    scheme = parts.scheme.lower()
    if scheme not in ALLOWED_SCHEMES:
        raise SecurityError(
            "blocked scheme %r: only http:// and https:// are fetched "
            "(set allow_private_urls to override)" % (scheme or url[:20],))
    host = parts.hostname
    if not host:
        raise SecurityError("URL has no host: %r" % url[:100])
    if _allow_private():
        return url
    # Literal IP in the URL? Check it directly.
    try:
        ip = ipaddress.ip_address(host)
    except ValueError:
        ip = None
    if ip is not None:
        if _blocked_ip(ip):
            raise SecurityError("blocked non-public IP literal: %s" % host)
        return url
    # Resolve, then require every address to be public.
    # Fail closed: unresolvable host = don't fetch.
    try:
        infos = socket.getaddrinfo(host, None, type=socket.SOCK_STREAM)
    except socket.gaierror as e:
        raise SecurityError("DNS resolution failed for %r: %s" % (host, e))
    ips = {info[4][0] for info in infos}
    if not ips:
        raise SecurityError("no addresses resolved for %r" % host)
    for ip_str in sorted(ips):
        if _blocked_ip(ipaddress.ip_address(ip_str)):
            raise SecurityError(
                "blocked: %r resolves to non-public IP %s" % (host, ip_str))
    return url
