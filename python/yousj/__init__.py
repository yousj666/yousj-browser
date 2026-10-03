"""Yousj browser — a from-scratch browser engine (headless, built for AI).

v0.2: fetch a URL, parse HTML into a DOM (Rust+C engine), extract
title / text / links; F12 devtools (Network/Elements/Console/Sources/
Performance); swappable search engine with web search through our own engine.
"""
from .engine import Document, parse
from . import devtools
from . import net
from . import search as _search_mod
from . import settings


def fetch(url: str, timeout: int = 15) -> Document:
    """Fetch a URL and return a parsed Document."""
    html = net.get(url, timeout=timeout)
    return parse(html)


def search(query: str, max_results: int = 10, engine_name=None,
           timeout: int = 15):
    """Search the web using the configured search engine."""
    return _search_mod.search(query, max_results=max_results,
                             engine_name=engine_name, timeout=timeout)


__all__ = ["fetch", "parse", "search", "Document", "devtools", "net",
           "settings"]
__version__ = "0.2.0"
