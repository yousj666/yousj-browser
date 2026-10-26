"""Pixel layout for AI browsing (minimal box model).

Wraps the Rust ``yousj_layout`` export (see ``engine/src/layout.rs``):
given a document and a viewport width, it returns every laid-out
element's box in CSS pixels relative to the document origin (top-left).

Boxes are ESTIMATES (no real font metrics): CJK chars 1.0em, other
chars 0.5em, line height 1.2em; px-only styles; no float / position /
flex / grid / table layout (those fall back to block); margins do not
collapse. See ``layout.rs`` for the full list of what is and isn't
supported. Coordinates are good enough to point a (simulated) mouse at
an element — they are NOT a rendered browser's exact pixels.

Joining with the interactive list: both the layout walker and the
interactive walker count EVERY element per tag in document order
(skipping the same head/script/style/noscript/template subtrees), so a
box's ``(tag, tag_index)`` matches its
:func:`yousj.interact.interactive_elements` entry. Elements that get no
box (e.g. an inline element nested inside another inline element's box,
or ``display:none``) report ``box=None`` — that means "no coordinates
available", not "not on the page".

Example::

    from yousj import parse
    from yousj import layout
    doc = parse("<html><body><a href='/x'>Hi</a></body></html>")
    info = layout.layout(doc)          # {"viewport_width": ..., "boxes": [...]}
    box = layout.element_box(doc, 1)   # box of interactive element #1, or None
"""
import json

from . import engine as _engine


def layout(doc, viewport_width: int = 1280) -> dict:
    """Layout ``doc`` at ``viewport_width`` CSS pixels.

    Returns ``{"viewport_width": int, "truncated": bool, "boxes": [...]}``
    with one box per laid-out element: ``tag``, ``text`` (truncated),
    ``x``, ``y``, ``width``, ``height`` (ints, document origin),
    ``tag_index``, ``html_id``, ``href``. ``truncated`` is True when
    the box cap (5000) or recursion cap was hit. ``viewport_width`` <= 0
    falls back to 1280.
    """
    raw = _engine._take_str(
        _engine._lib.yousj_layout(doc._ptr, int(viewport_width)))
    if not raw:
        return {"viewport_width": int(viewport_width), "truncated": False,
                "boxes": []}
    return json.loads(raw)


def element_box(doc, element_id: int, viewport_width: int = 1280):
    """Pixel box of interactive element ``element_id`` (1-based), or None.

    Resolves the element via :mod:`yousj.interact` (same document), then
    joins to the layout by ``(tag, tag_index)``. Returns None when the
    element has no number, or when layout gives it no box (nested inline
    element, ``display:none``, or layout failure). A second layout pass
    is run on demand; callers that need many boxes should call
    :func:`layout` once and join themselves.
    """
    from . import interact as _interact
    els = _interact.interactive_elements(doc, with_boxes=False)
    el = next((e for e in els if e["id"] == element_id), None)
    if el is None:
        return None
    info = layout(doc, viewport_width=viewport_width)
    for b in info["boxes"]:
        if b["tag"] == el["tag"] and b["tag_index"] == el["tag_index"]:
            return {"x": b["x"], "y": b["y"], "width": b["width"],
                    "height": b["height"]}
    return None
