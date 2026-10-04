"""Persistent settings for the Yousj browser.

The search engine is swappable, by humans (CLI: ``python -m yousj config``)
and by AI agents (the functions below — no UI needed)::

    from yousj import settings
    settings.set_search_engine("google")                       # preset
    settings.set_search_engine("https://example.com/s?q={q}")  # custom
    settings.get_search_engine()  # -> {"name": ..., "url": ...}
"""
import json
import os

# {q} is replaced with the URL-encoded query.
PRESET_ENGINES = {
    "duckduckgo": "https://html.duckduckgo.com/html/?q={q}",
    "google": "https://www.google.com/search?q={q}",
    "bing": "https://www.bing.com/search?q={q}",
    "brave": "https://search.brave.com/search?q={q}",
}

DEFAULTS = {
    # brave's server-rendered results page is the most bot-friendly
    # of the presets (duckduckgo serves a bot challenge, google serves
    # a consent page to simple clients). Switch anytime in settings.
    "search_engine": "brave",
    # HTTP(S) proxy URL, e.g. "http://127.0.0.1:8080". None = direct.
    # CLI: python -m yousj config proxy http://host:port
    # (env vars http_proxy/https_proxy are honored when unset here)
    "proxy": None,
    # Download size cap in megabytes (V5.1: CUE review — no unlimited
    # downloads filling the disk). CLI:
    # python -m yousj config max-download-size 500
    "max_download_size_mb": 200,
}


def _path() -> str:
    return os.path.join(os.path.expanduser("~"), ".config", "yousj",
                        "settings.json")


def _load() -> dict:
    try:
        with open(_path(), encoding="utf-8") as f:
            data = json.load(f)
            return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def _save(data: dict) -> None:
    p = _path()
    os.makedirs(os.path.dirname(p), exist_ok=True)
    tmp = p + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=2)
    os.replace(tmp, p)


def get(key: str, default=None):
    return _load().get(key, DEFAULTS.get(key, default))


def set(key: str, value) -> None:
    data = _load()
    data[key] = value
    _save(data)


def all_settings() -> dict:
    data = dict(DEFAULTS)
    data.update(_load())
    data["search_engine_url"] = get_search_engine()["url"]
    return data


def list_search_engines() -> dict:
    """Name -> URL template of built-in presets."""
    return dict(PRESET_ENGINES)


def set_search_engine(name_or_url: str) -> dict:
    """Point the browser at a search engine.

    Accepts a preset name (``duckduckgo`` / ``google`` / ``bing`` / ``brave``)
    or a custom URL template containing ``{q}``, e.g.
    ``https://search.example.com/?q={q}``.
    Returns the active engine ``{"name": ..., "url": ...}``.
    """
    key = name_or_url.strip()
    lowered = key.lower()
    if lowered in PRESET_ENGINES:
        set("search_engine", lowered)
    elif key.startswith(("http://", "https://")) and "{q}" in key:
        set("search_engine", "custom")
        set("search_engine_custom_url", key)
    else:
        raise ValueError(
            "unknown engine %r: use a preset name %s or a URL template "
            "containing {q}" % (name_or_url, sorted(PRESET_ENGINES)))
    return get_search_engine()


def get_search_engine() -> dict:
    """Current engine as ``{"name": ..., "url": ...}``."""
    name = get("search_engine", "duckduckgo")
    if name == "custom":
        url = get("search_engine_custom_url", PRESET_ENGINES["duckduckgo"])
        return {"name": "custom", "url": url}
    return {"name": name,
            "url": PRESET_ENGINES.get(name, PRESET_ENGINES["duckduckgo"])}


def set_proxy(url: str) -> str:
    """Set the HTTP(S) proxy, e.g. ``set_proxy("http://127.0.0.1:8080")``.

    Takes effect on the next request (``yousj.net`` rebuilds its opener).
    """
    url = (url or "").strip()
    if url and not url.startswith(("http://", "https://")):
        raise ValueError("proxy URL must start with http:// or https://")
    set("proxy", url or None)
    return get("proxy")


def clear_proxy() -> None:
    """Back to direct connection (env vars still honored if set)."""
    set("proxy", None)


def get_proxy():
    """Configured proxy URL, or None."""
    return get("proxy")
