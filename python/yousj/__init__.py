"""Yousj browser — a from-scratch browser engine (headless, built for AI).

v0.1: fetch a URL, parse HTML into a DOM (Rust+C engine), extract
title / text / links.
"""
from .engine import Document, parse
from . import net


def fetch(url: str, timeout: int = 15) -> Document:
    """Fetch a URL and return a parsed Document."""
    html = net.get(url, timeout=timeout)
    return parse(html)


__all__ = ["fetch", "parse", "Document"]
__version__ = "0.1.0"
