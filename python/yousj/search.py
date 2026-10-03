"""Web search through the Yousj engine.

The search engine is configured in ``yousj.settings`` (swappable by humans
and AI agents)::

    from yousj import search, settings
    settings.set_search_engine("google")
    for r in search.search("Yousj browser"):
        print(r["title"], r["url"])

Each result is ``{"title": ..., "url": ...}`` in the engine's ranking order.
The search request itself goes through ``yousj.net``, so it shows up in the
F12 Network panel like any other page load.
"""
import base64
import urllib.parse

from . import devtools as _devtools
from . import engine as _engine
from . import net as _net
from . import settings as _settings


def _unwrap_bing(href: str):
    """Unwrap Bing's /ck/a redirect links: ``u=a1<base64url>`` -> real URL."""
    try:
        q = urllib.parse.parse_qs(urllib.parse.urlparse(href).query)
        u = q.get("u", [""])[0]
        if not u.startswith("a1"):
            return None
        raw = u[2:]
        raw += "=" * (-len(raw) % 4)
        url = base64.urlsafe_b64decode(raw).decode("utf-8", "replace")
        return url if url.startswith(("http://", "https://")) else None
    except Exception:  # noqa: BLE001
        return None


def _resolve_engine(engine_name: str) -> dict:
    """Resolve a preset name or custom URL template without persisting."""
    key = engine_name.strip()
    lowered = key.lower()
    if lowered in _settings.PRESET_ENGINES:
        return {"name": lowered, "url": _settings.PRESET_ENGINES[lowered]}
    if key.startswith(("http://", "https://")) and "{q}" in key:
        return {"name": "custom", "url": key}
    raise ValueError("unknown engine %r" % engine_name)


def _clean_title(title: str, url: str) -> str:
    """Drop leading site-chip tokens, e.g.
    "Lightpanda lightpanda.io Lightpanda | The headless browser"
    -> "Lightpanda | The headless browser".
    """
    host = urllib.parse.urlparse(url).netloc.lower()
    parts = host.split(".")
    bare = ".".join(parts[-2:]) if len(parts) >= 2 else host
    tokens = title.split()
    for i, tok in enumerate(tokens):
        t = tok.strip().lower()
        if t == host or t == bare or t == "www." + bare:
            rest = " ".join(tokens[i + 1:]).strip()
            if rest:
                return rest
    return title


def search(query, max_results=10, engine_name=None, timeout=15):
    """Search the web and return results parsed by our own HTML engine.

    ``engine_name`` is a one-off override (preset name or ``{q}`` URL
    template); it does not change the saved setting. Use
    ``yousj.settings.set_search_engine()`` to switch persistently.
    """
    if engine_name is not None:
        se = _resolve_engine(engine_name)
    else:
        se = _settings.get_search_engine()
    url = se["url"].replace("{q}", urllib.parse.quote_plus(query))
    _devtools.Console.log("search [%s]: %s" % (se["name"], query))
    html = _net.get(url, timeout=timeout)
    doc = _engine.parse(html)
    engine_host = urllib.parse.urlparse(se["url"]).netloc

    results = []
    seen = set()
    for href, text in doc.anchors():
        # Unwrap engine redirect links (Bing wraps results in /ck/a).
        if engine_host in ("www.bing.com", "bing.com") and "/ck/a" in href:
            unwrapped = _unwrap_bing(href)
            if unwrapped:
                href = unwrapped
        if not href.startswith(("http://", "https://")):
            continue
        host = urllib.parse.urlparse(href).netloc
        # Skip the search engine's own navigation links.
        if host == engine_host or host.endswith("." + engine_host):
            continue
        title = _clean_title(" ".join(text.split()), href)
        # Skip site chips / breadcrumbs ("example.com https:// ... › ...").
        if not title or "http://" in title or "https://" in title \
                or "\u203a" in title:
            continue
        if href in seen:
            continue
        seen.add(href)
        results.append({"title": title, "url": href})
        if len(results) >= max_results:
            break
    _devtools.Console.log("search returned %d results" % len(results))
    return results
