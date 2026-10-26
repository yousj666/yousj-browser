"""Wireframe screenshots for AI browsing.

Renders the minimal layout (:mod:`yousj.layout` boxes) as an image, so
an AI can SEE the page: where elements are, what text they carry, where
the mouse is. This is a WIREFRAME, not a real rendering: the engine has
no CSS cascade, no image decoding, no font shaping — boxes are drawn as
labeled rectangles (color by tag category) at estimated positions. It
shows structure faithfully; it does NOT show the page's true visual
design (real colors, images, webfonts, exact text metrics).

Formats:
- PNG (needs Pillow): raster image. Text uses Noto Sans CJK when
  available (so Chinese renders), else DejaVu, else PIL's default font.
- SVG: vector wireframe, full text, no Pillow needed; renders in any
  browser/viewer with system fonts.

API:
- :func:`render_png` -> PNG bytes.
- :func:`render_svg` -> SVG text.
- :func:`capture` -> dict with format / width / height / bytes / path /
  boxes_drawn / note; writes to ``path`` when given.

Options (all renderers):
- ``viewport_width`` / ``viewport_height``: the canvas in CSS pixels.
- ``full_page``: False (default) captures only the viewport rectangle;
  True extends the canvas to the document's full laid-out height
  (capped at 20000 px, noted when capped).
- ``mouse_pos``: ``(x, y)`` to draw the virtual cursor (red marker).
- ``highlight_box``: ``{"x","y","width","height"}`` to outline in red
  (e.g. the element just clicked).
"""
import html as _html

try:
    from PIL import Image, ImageDraw, ImageFont
    _HAS_PIL = True
except Exception:  # noqa: BLE001 - PNG is optional, SVG is not
    _HAS_PIL = False

from . import layout as _layout

MAX_FULL_PAGE_HEIGHT = 20000

# tag -> (outline RGB, fill RGB or None)
_COLORS = {
    "a": ((37, 99, 235), (219, 234, 254)),
    "button": ((22, 163, 74), (220, 252, 231)),
    "input": ((217, 119, 6), (254, 243, 199)),
    "select": ((217, 119, 6), (254, 243, 199)),
    "textarea": ((217, 119, 6), (254, 243, 199)),
    "img": ((147, 51, 234), (243, 232, 255)),
    "video": ((147, 51, 234), (243, 232, 255)),
    "h1": ((75, 85, 99), (243, 244, 246)),
    "h2": ((75, 85, 99), (243, 244, 246)),
    "h3": ((75, 85, 99), (243, 244, 246)),
    "h4": ((75, 85, 99), (243, 244, 246)),
    "h5": ((75, 85, 99), (243, 244, 246)),
    "h6": ((75, 85, 99), (243, 244, 246)),
}
_DEFAULT_COLOR = ((180, 180, 180), None)


def _canvas_size(boxes, viewport_width, viewport_height, full_page):
    """(width, height, capped) for the canvas."""
    w = int(viewport_width)
    h = int(viewport_height)
    capped = False
    if full_page:
        bottom = 0
        for b in boxes:
            bottom = max(bottom, b["y"] + b["height"])
        h = max(h, bottom)
        if h > MAX_FULL_PAGE_HEIGHT:
            h = MAX_FULL_PAGE_HEIGHT
            capped = True
    return w, h, capped


def _fit_text(text, font, max_width, measure):
    """Truncate ``text`` to fit ``max_width`` px (measure: str -> float)."""
    if measure(text) <= max_width:
        return text
    out = ""
    for ch in text:
        if measure(out + ch + "…") > max_width:
            break
        out += ch
    return out + "…" if out else ""


def _pil_font(size=12):
    """Best available PIL font (CJK first, so Chinese renders)."""
    for path in (
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ):
        try:
            return ImageFont.truetype(path, size)
        except Exception:  # noqa: BLE001 - try the next font
            continue
    return ImageFont.load_default()


def render_png(doc, viewport_width: int = 1280,
               viewport_height: int = 800, full_page: bool = False,
               mouse_pos=None, highlight_box=None) -> bytes:
    """Render ``doc`` as PNG bytes (wireframe, see module docstring).

    Raises RuntimeError when Pillow is not installed.
    """
    if not _HAS_PIL:
        raise RuntimeError(
            "Pillow is not installed; install it for PNG screenshots, "
            "or use render_svg() (no dependency)")
    info = _layout.layout(doc, viewport_width=viewport_width)
    boxes = info["boxes"]
    width, height, _capped = _canvas_size(
        boxes, viewport_width, viewport_height, full_page)
    img = Image.new("RGB", (width, height), (255, 255, 255))
    draw = ImageDraw.Draw(img)
    font = _pil_font(12)

    def measure(s):
        return draw.textlength(s, font=font)

    # Boxes are emitted children-before-parent; draw reversed so parents
    # form the background and children paint on top.
    for b in reversed(boxes):
        x, y, w, h = b["x"], b["y"], b["width"], b["height"]
        if w <= 0 or h <= 0:
            continue
        outline, fill = _COLORS.get(b["tag"], _DEFAULT_COLOR)
        draw.rectangle([x, y, x + w - 1, y + h - 1], outline=outline,
                       fill=fill)
        # Labels only for leaf boxes: container boxes (html/body/div)
        # carry all descendant text, which would overlap their children.
        label = b["text"] or b["tag"]
        if b.get("leaf", True) and w >= 24 and h >= 14 and label:
            shown = _fit_text(label, font, w - 6, measure)
            if shown:
                draw.text((x + 3, y + 2), shown, font=font, fill=(20, 20, 20))
    if highlight_box:
        hx = highlight_box
        draw.rectangle([hx["x"], hx["y"],
                        hx["x"] + hx["width"] - 1,
                        hx["y"] + hx["height"] - 1],
                       outline=(220, 0, 0), width=3)
    if mouse_pos is not None:
        mx, my = int(mouse_pos[0]), int(mouse_pos[1])
        r = 8
        draw.ellipse([mx - r, my - r, mx + r, my + r],
                     outline=(220, 0, 0), width=2)
        draw.line([mx - 14, my, mx - r - 2, my], fill=(220, 0, 0), width=2)
        draw.line([mx + r + 2, my, mx + 14, my], fill=(220, 0, 0), width=2)
        draw.line([mx, my - 14, mx, my - r - 2], fill=(220, 0, 0), width=2)
        draw.line([mx, my + r + 2, mx, my + 14], fill=(220, 0, 0), width=2)
        label = "mouse (%d,%d)" % (mx, my)
        lx = mx + 16 if mx + 16 + measure(label) < width else mx - 16 - measure(label)
        draw.text((lx, my - 6), label, font=font, fill=(220, 0, 0))
    import io
    buf = io.BytesIO()
    img.save(buf, format="PNG")
    return buf.getvalue()


def render_svg(doc, viewport_width: int = 1280,
               viewport_height: int = 800, full_page: bool = False,
               mouse_pos=None, highlight_box=None) -> str:
    """Render ``doc`` as an SVG wireframe string (no Pillow needed)."""
    info = _layout.layout(doc, viewport_width=viewport_width)
    boxes = info["boxes"]
    width, height, _capped = _canvas_size(
        boxes, viewport_width, viewport_height, full_page)
    parts = ['<svg xmlns="http://www.w3.org/2000/svg" width="%d" '
             'height="%d" viewBox="0 0 %d %d">' % (width, height, width, height),
             '<rect x="0" y="0" width="%d" height="%d" fill="white"/>' % (width, height)]
    for b in reversed(boxes):
        x, y, w, h = b["x"], b["y"], b["width"], b["height"]
        if w <= 0 or h <= 0:
            continue
        (r, g, bl), fill = _COLORS.get(b["tag"], _DEFAULT_COLOR)
        outline = "#%02x%02x%02x" % (r, g, bl)
        fill_s = ("#%02x%02x%02x" % fill) if fill else "none"
        parts.append('<rect x="%d" y="%d" width="%d" height="%d" '
                     'fill="%s" stroke="%s" stroke-width="1"/>' % (
                         x, y, w, h, fill_s, outline))
        label = b["text"] or b["tag"]
        if b.get("leaf", True) and w >= 24 and h >= 14 and label:
            parts.append('<text x="%d" y="%d" font-family="sans-serif" '
                         'font-size="12" fill="#141414">%s</text>' % (
                             x + 3, y + 13, _html.escape(label[:60])))
    if highlight_box:
        hx = highlight_box
        parts.append('<rect x="%d" y="%d" width="%d" height="%d" '
                     'fill="none" stroke="#dc0000" stroke-width="3"/>' % (
                         hx["x"], hx["y"], hx["width"], hx["height"]))
    if mouse_pos is not None:
        mx, my = int(mouse_pos[0]), int(mouse_pos[1])
        parts.append('<circle cx="%d" cy="%d" r="8" fill="none" '
                     'stroke="#dc0000" stroke-width="2"/>' % (mx, my))
        parts.append('<text x="%d" y="%d" font-family="sans-serif" '
                     'font-size="12" fill="#dc0000">mouse (%d,%d)</text>' % (
                         mx + 16, my - 6, mx, my))
    parts.append('</svg>')
    return "\n".join(parts)


def capture(doc, path=None, format: str = "png",
            viewport_width: int = 1280, viewport_height: int = 800,
            full_page: bool = False, mouse_pos=None,
            highlight_box=None) -> dict:
    """Capture a wireframe screenshot of ``doc``.

    Returns ``{"format", "width", "height", "boxes_drawn", "bytes",
    "path", "note"}``. ``bytes`` is the image data (PNG bytes or SVG
    text encoded as UTF-8); when ``path`` is given, the image is also
    written there and ``path`` echoes it. ``format`` is ``"png"`` or
    ``"svg"`` (anything else raises ValueError).
    """
    if format not in ("png", "svg"):
        raise ValueError('format must be "png" or "svg"')
    info = _layout.layout(doc, viewport_width=viewport_width)
    boxes = info["boxes"]
    width, height, capped = _canvas_size(
        boxes, viewport_width, viewport_height, full_page)
    if format == "png":
        data = render_png(doc, viewport_width=viewport_width,
                          viewport_height=viewport_height,
                          full_page=full_page, mouse_pos=mouse_pos,
                          highlight_box=highlight_box)
    else:
        data = render_svg(doc, viewport_width=viewport_width,
                          viewport_height=viewport_height,
                          full_page=full_page, mouse_pos=mouse_pos,
                          highlight_box=highlight_box).encode("utf-8")
    note = ("wireframe screenshot from the minimal layout (estimated "
            "positions, no CSS styling / images / webfonts — structure, "
            "not a real rendering)")
    if capped:
        note += "; full-page height capped at %d px" % MAX_FULL_PAGE_HEIGHT
    if mouse_pos is not None:
        note += "; mouse at (%d,%d)" % (int(mouse_pos[0]),
                                        int(mouse_pos[1]))
    if path:
        with open(path, "wb") as f:
            f.write(data)
    return {"format": format, "width": width, "height": height,
            "boxes_drawn": len(boxes), "bytes": data, "path": path,
            "note": note}
