"""AI-friendly interactive-element browsing.

Two ideas, both built for AI callers rather than humans:

1. :func:`interactive_elements` — a numbered list of everything on the
   page an AI can act on (links, buttons, inputs, selects, textareas,
   labels, onclick / interactive-role elements). Numbers are stable
   within one :class:`~yousj.engine.Document` (document order); the AI
   refers to elements by number. Each item also carries ``box`` (pixel
   coordinates from :mod:`yousj.layout`, or None) and ``touchable`` /
   ``touchable_reason`` (see below), so the AI can also aim a
   (simulated) mouse by pixels via :mod:`yousj.mouse`.

2. :func:`activate` — "click" an element by number and get back a
   structured result saying where you landed and what changed, so the
   AI can see the effect of its action without re-reading the page.

Example::

    from yousj import fetch
    from yousj import interact
    doc = fetch("https://example.com")
    els = interact.interactive_elements(doc)
    print(interact.format_elements(els))
    result = interact.activate(doc, 1)
    print(result["new_url"], result["new_title"])

Touchable vs untouchable (the user's definition): an element is
"touchable" when pressing it has an observable effect — navigation,
form submission, an action, a state change. Pressing with no effect is
untouchable. Static judgment rules (see :func:`_touchable`):
- disabled controls: untouchable (browsers don't activate them).
- links with href, buttons, form controls, labels with an associated
  control, and elements with an inline onclick handler: touchable.
- an element with only an interactive role (e.g. role=button) and no
  static handler: untouchable *by static judgment* — see the boundary.
- everything else (plain div/span/text with nothing): untouchable.

BOUNDARY, stated honestly: static judgment cannot see handlers
registered by page scripts via ``addEventListener`` (the engine exposes
no listener list, and listeners don't persist across ``run_js`` calls),
and it cannot tell whether a handler actually does anything (a handler
that only calls ``preventDefault()`` or an empty handler looks identical
to a working one). So ``touchable`` is a best-effort static verdict, not
a guarantee. The ground truth is the click itself: :func:`activate`
reports ``changed`` / ``console`` / ``new_url`` after pressing — if
those show nothing, the element was effectively untouchable whatever
the static verdict said. When in doubt, click and check.

Honest limits (see also module docstrings):
- Page <script> code is NOT auto-executed on fetch. For button clicks,
  :func:`activate` runs the page's inline scripts together with the
  click in a single ``run_js`` call (yousj-js listeners do not persist
  across calls, so a click in a separate call could never fire them).
  Page scripts therefore re-run on every button activation; external
  (``src``) scripts are not fetched, and handlers registered on
  DOMContentLoaded never fire (no load event is dispatched here).
  Inline ``onclick="..."`` attributes are reported (``onclick: True``)
  but NOT executed by yousj-js — only ``addEventListener`` handlers
  fire on ``click()``.
- Mouse and pixels are SIMULATED (:mod:`yousj.mouse` on
  :mod:`yousj.layout` boxes): there is no rendered window and no real
  pointer, and the engine dispatches no pointer events (mousemove /
  mousedown / mouseup) to page JS. So a real slider CAPTCHA — which
  reads pointer events and verifies the drag server-side — cannot be
  driven by this engine; the slider test in this package validates the
  drag path + drop logic against a simulated slider only. Real-world
  sliders need a rendered browser.
"""
import json
import re
import urllib.parse

from . import engine as _engine
from . import history as _history
from . import net as _net


def _js_string(s: str) -> str:
    """Escape a Python string for a JS single-quoted literal."""
    return "'" + s.replace("\\", "\\\\").replace("'", "\\'").replace(
        "\n", "\\n").replace("\r", "\\r") + "'"


def _js_click_expr(el: dict) -> str | None:
    """JS expression that clicks this element, or None if not addressable.

    Prefers getElementById (stable, works with any id string once
    escaped); falls back to querySelectorAll(tag)[tag_index]
    (document order, same counting as the Rust walker).
    """
    el_id = el.get("html_id") or ""
    if el_id:
        return "document.getElementById(%s).click()" % _js_string(el_id)
    tag = el.get("tag") or ""
    if not re.match(r"^[a-z][a-z0-9]*$", tag):
        return None
    return "document.querySelectorAll('%s')[%d].click()" % (
        tag, el.get("tag_index", 0))


def _touchable(el: dict):
    """Static touchable verdict: (touchable, reason).

    Follows the user's definition: pressing has an observable effect
    (navigation, submission, action, state change) => touchable;
    pressing with no effect => untouchable. This is STATIC judgment
    from the element's tag/kind/attributes only — it cannot see
    ``addEventListener`` handlers (no listener list in the engine) and
    cannot tell a no-op handler from a working one. See the module
    docstring's boundary note. Callers should verify by clicking
    (:func:`activate` reports ``changed`` / ``console`` / ``new_url``).
    """
    if el.get("disabled"):
        return False, "disabled (browsers do not activate disabled controls)"
    if el.get("onclick"):
        return True, "inline onclick handler"
    kind = el.get("kind")
    if kind == "link":
        href = (el.get("href") or "").strip()
        low = href.lower()
        if low.startswith("javascript:"):
            return True, ("javascript: link (code runs in a real browser; "
                          "this engine reports it as info, does not execute)")
        if low.startswith(("mailto:", "tel:")):
            return True, ("external-scheme link (opens mail/phone app in a "
                          "real browser; this engine reports info only)")
        if low.startswith("#"):
            return True, "in-page anchor (fragment navigation)"
        if href:
            return True, "navigates to %s" % (el.get("url") or href)
        return False, "link without href"
    if kind in ("button", "submit"):
        if kind == "submit" and el.get("form_index") is not None:
            return True, "submits its form"
        return True, ("button (a click handler may be registered by page "
                      "script; verify by clicking)")
    if kind in ("input", "toggle", "select", "textarea"):
        return True, "form control (focus / input / state change)"
    if kind == "label":
        if el.get("label_for") or el.get("wraps_control"):
            return True, "activates its associated control"
        return False, "label without associated control"
    if kind == "clickable":
        # clickable without onclick means an interactive role (e.g.
        # role=button) with no static handler: presented as actionable,
        # but pressing does nothing unless a script registered one.
        return False, ("no static click handler detected (role=%s; a "
                       "script-registered handler is possible; verify by "
                       "clicking)" % (el.get("role") or "interactive"))
    return False, "no link, button, form behavior, or click handler detected"


def interactive_elements(doc, viewport_width: int = 1280,
                         with_boxes: bool = True):
    """All interactive elements of ``doc`` as a numbered list of dicts.

    Each dict has (at least): ``id`` (1-based, stable within this
    document), ``tag``, ``kind`` (link/button/submit/input/toggle/
    select/textarea/label/clickable), ``type``, ``text`` (visible text,
    truncated), ``href``, ``name``, ``html_id`` (the element's HTML id
    attribute, if any), ``value``, ``disabled``, ``onclick``,
    ``role``, ``tag_index``, ``form_index``, ``checked`` (checkbox/
    radio), ``options`` (select), ``label_for`` (label),
    ``wraps_control`` (label), plus ``url`` (absolute URL for links,
    resolved against ``doc.url`` when known), ``box`` (``{"x","y",
    "width","height"}`` pixel coordinates from :mod:`yousj.layout`,
    or None when layout gives the element no box) and ``touchable`` /
    ``touchable_reason`` (static verdict, see :func:`_touchable`).

    ``with_boxes=False`` skips the layout pass (``box`` is None for
    every item); ``viewport_width`` is the layout viewport in CSS
    pixels (default 1280).
    """
    raw = _engine._take_str(_engine._lib.yousj_interactive(doc._ptr))
    items = json.loads(raw) if raw else []
    boxes = {}
    if with_boxes:
        try:
            from . import layout as _layout
            info = _layout.layout(doc, viewport_width=viewport_width)
            for b in info["boxes"]:
                boxes[(b["tag"], b["tag_index"])] = {
                    "x": b["x"], "y": b["y"],
                    "width": b["width"], "height": b["height"]}
        except Exception:  # noqa: BLE001 - boxes are best-effort
            boxes = {}
    out = []
    for i, it in enumerate(items, 1):
        it = dict(it)
        it["id"] = i
        href = it.get("href") or ""
        if href and getattr(doc, "url", None):
            it["url"] = urllib.parse.urljoin(doc.url, href)
        else:
            it["url"] = href
        it["box"] = boxes.get((it.get("tag"), it.get("tag_index")))
        touchable, reason = _touchable(it)
        it["touchable"] = touchable
        it["touchable_reason"] = reason
        out.append(it)
    return out


def format_elements(elements) -> str:
    """Render an interactive-element list as AI-readable text.

    One line per element: ``[id] <tag> type=... 'text' -> url
    @(x,y) WxH ✓touchable/✗untouchable (reason) name=... (flags)``.
    Empty text falls back to the element's value (e.g. a submit
    button's label); flags note disabled / onclick / role / checked /
    selected options. The ``@(x,y) WxH`` part is the layout pixel box
    (absent when the element has no box); ``✓``/``✗`` is the static
    touchable verdict (see :func:`_touchable` — verify by clicking).
    """
    lines = []
    for el in elements:
        parts = ["[%d]" % el["id"], "<%s>" % el["tag"]]
        if el.get("type"):
            parts.append("type=%s" % el["type"])
        label = el.get("text") or ""
        if not label and el.get("value"):
            label = el["value"]
        if label:
            parts.append("'%s'" % label[:80])
        if el.get("kind") == "link" and el.get("url"):
            parts.append("-> %s" % el["url"])
        if el.get("box"):
            b = el["box"]
            parts.append("@(%d,%d) %dx%d" % (
                b["x"], b["y"], b["width"], b["height"]))
        if "touchable" in el:
            if el["touchable"]:
                parts.append("✓touchable")
            else:
                parts.append("✗untouchable (%s)"
                             % el.get("touchable_reason", ""))
        if el.get("html_id"):
            parts.append("html_id=%s" % el["html_id"])
        if el.get("name"):
            parts.append("name=%s" % el["name"])
        if el.get("kind") == "select" and el.get("options"):
            opts = ",".join(o["value"] for o in el["options"][:8])
            parts.append("options=[%s]" % opts)
        if el.get("kind") == "toggle":
            parts.append("checked=%s" % bool(el.get("checked")))
        if el.get("kind") == "label" and el.get("label_for"):
            parts.append("for=%s" % el["label_for"])
        if el.get("disabled"):
            parts.append("(disabled)")
        if el.get("onclick"):
            parts.append("(onclick)")
        if el.get("role"):
            parts.append("role=%s" % el["role"])
        lines.append(" ".join(parts))
    return "\n".join(lines)


def _snip(text: str, n: int = 500) -> str:
    return (text or "")[:n]


def _page_scripts(doc):
    """Inline <script> bodies of ``doc`` in document order.

    External scripts (``src``) are returned as empty code (not fetched).
    Used by :func:`_js_click`: see its note on listener persistence.
    """
    try:
        raw = _engine._take_str(_engine._lib.yousj_scripts(doc._ptr))
        items = json.loads(raw) if raw else []
    except Exception:  # noqa: BLE001 - scripts are best-effort
        return []
    return [it.get("code") or "" for it in items if it.get("code")]


def _js_click(doc, el):
    """Dispatch a DOM click() on ``el`` via yousj-js.

    Returns (console, js_error, before_text, after_text, note).

    CRITICAL engine behavior: yousj-js event listeners live only for
    one ``run_js`` call — they do NOT persist across calls (DOM
    mutations do). A click dispatched in a separate call can therefore
    never fire a handler registered by an earlier call. So this runs
    the page's inline scripts AND the click expression in a SINGLE
    ``run_js`` call. Each script is wrapped in try/catch so one bad
    script doesn't block the click (errors surface via console).
    Consequences, stated honestly:
    - Page scripts re-run on every button activation (load-time side
      effects repeat). Test pages should register handlers only.
    - Handlers registered on DOMContentLoaded never fire (the engine
      dispatches no load event here); top-level registrations work.
    - Inline ``onclick`` attributes are NOT executed by yousj-js (only
      ``addEventListener`` handlers fire); that case is noted, not hidden.
    Raises RuntimeError when the JS engine is unavailable, ValueError
    when the element cannot be addressed from JS.
    """
    expr = _js_click_expr(el)
    if expr is None:
        raise ValueError("cannot address element from JS (no id, unusual tag)")
    scripts = _page_scripts(doc)
    budget = 200_000
    used = 0
    skipped = 0
    parts = []
    for code in scripts:
        if used + len(code) > budget:
            skipped += 1
            continue
        used += len(code)
        parts.append("try{\n%s\n}catch(e){console.log('page script error: '+e);}"
                     % code)
    parts.append(expr + ";")
    before = doc.text()
    res = doc.run_js("\n".join(parts))  # may raise RuntimeError (no js)
    after = doc.text()
    note = ""
    if skipped:
        note = "%d page script(s) skipped (size budget). " % skipped
    if el.get("onclick") and not res.get("console"):
        note += ("element has an inline onclick attribute, which yousj-js "
                 "does not execute (only addEventListener handlers fire "
                 "on click()); no handler ran")
    elif not res.get("console") and before == after:
        note += "no click handler observed (no console output, no text change)"
    return res.get("console", []), res.get("error"), before, after, note


def _js_click_result(doc, el, console, js_error, before, after, note,
                     action="js_click"):
    return {
        "action": action,
        "id": el["id"],
        "tag": el["tag"],
        "kind": el["kind"],
        "box": el.get("box"),
        "new_url": getattr(doc, "url", None),
        "new_title": doc.title(),
        "text_before": _snip(before),
        "text_after": _snip(after),
        "changed": before != after,
        "console": console,
        "js_error": js_error,
        "note": note,
    }


def _nav_result(action, el, new_doc):
    return {
        "action": action,
        "id": el["id"],
        "tag": el["tag"],
        "kind": el["kind"],
        "box": el.get("box"),
        "new_url": getattr(new_doc, "url", None),
        "new_title": new_doc.title(),
        "text_before": None,
        "text_after": _snip(new_doc.text()),
        "changed": True,
        "console": [],
        "js_error": None,
        "note": "",
    }


def _info_result(action, el, note, **extra):
    r = {
        "action": action,
        "id": el["id"],
        "tag": el["tag"],
        "kind": el["kind"],
        "box": el.get("box"),
        "new_url": None,
        "new_title": None,
        "text_before": None,
        "text_after": None,
        "changed": False,
        "console": [],
        "js_error": None,
        "note": note,
    }
    r.update(extra)
    return r


def activate(doc, element_id, timeout: int = 15, pace: bool = True,
             viewport_width: int = 1280):
    """Activate interactive element ``element_id`` (1-based, see
    :func:`interactive_elements`) and report what happened.

    Returns a dict: ``action`` (navigate / js_click / form_submit /
    info / error), ``id``, ``tag``, ``kind``, ``box`` (pixel box of the
    clicked element, or None), ``new_url``,
    ``new_title``, ``text_before``, ``text_after``, ``changed``,
    ``console`` (JS console lines, if any), ``js_error``, ``note``.

    - link: fetches the href (resolved against ``doc.url``) and returns
      the new page's URL/title/snippet. In-page anchors (``#...``),
      ``javascript:`` and ``mailto:``/``tel:`` are NOT fetched; they
      return an ``info`` result explaining why.
    - button / clickable / input-button: dispatches a DOM ``click()``
      through yousj-js and reports console output, JS errors, and
      whether the page text changed. A button with only an inline
      ``onclick`` attribute reports honestly that inline handlers are
      not executed (only ``addEventListener`` handlers fire).
    - submit (button/input): submits the enclosing form (if any) via
      the existing form API and returns the result page summary.
    - toggle (checkbox/radio): dispatches ``click()`` and re-reads the
      element list to report whether ``checked`` actually flipped.
    - input/textarea/select/label: informational — these take values
      or point at a control; they are not "clicked". The note tells the
      caller how to interact (e.g. ``forms().fill(...).submit()``).
    - disabled elements are never activated.

    ``pace=True`` (default) inserts a human-like random delay before
    acting (see :mod:`yousj.human`); pass ``pace=False`` in tests.
    """
    if pace:
        from . import human as _human
        _human.pace()
    els = interactive_elements(doc, viewport_width=viewport_width)
    el = next((e for e in els if e["id"] == element_id), None)
    if el is None:
        return _info_result("error", {"id": element_id, "tag": "", "kind": ""},
                            "no interactive element with id %r "
                            "(have 1..%d)" % (element_id, len(els)))
    if el.get("disabled"):
        return _info_result("info", el,
                            "element is disabled; browsers do not "
                            "activate disabled controls")
    kind = el["kind"]

    if kind == "link":
        href = el.get("href") or ""
        if not href:
            return _info_result("info", el, "link has no href")
        low = href.strip().lower()
        if low.startswith("#"):
            return _info_result(
                "info", el,
                "in-page anchor (%s); no navigation performed" % href)
        if low.startswith("javascript:"):
            return _info_result(
                "info", el,
                "javascript: links are not executed by activate()")
        if low.startswith(("mailto:", "tel:")):
            return _info_result(
                "info", el,
                "external scheme link (%s); not fetched" % href.split(":")[0])
        target = el.get("url") or href
        if not urllib.parse.urlsplit(target).netloc and not getattr(
                doc, "url", None):
            return _info_result(
                "error", el,
                "cannot resolve relative href %r: document has no URL; "
                "set doc.url first" % href)
        try:
            final, _, headers, raw = _net._request("GET", target,
                                                   timeout=timeout)
            body = _net._decode(raw, headers)
        except Exception as e:  # noqa: BLE001 - reported, not raised
            return _info_result("error", el,
                                "fetch failed: %s: %s"
                                % (type(e).__name__, e))
        new_doc = _engine.parse(body)
        new_doc.url = final
        _history.record(final, new_doc.title())
        r = _nav_result("navigate", el, new_doc)
        r["note"] = "followed link -> %s" % final
        return r

    if kind == "submit":
        # Real browsers fire click handlers first, then submit as the
        # default action. The engine's click() only runs listeners (it
        # does not submit, and does not track preventDefault), so we do
        # both steps explicitly and say so.
        try:
            console, js_error, _b, _a, click_note = _js_click(doc, el)
        except (ValueError, RuntimeError) as e:
            return _info_result("error", el, "JS click failed: %s" % e)
        fi = el.get("form_index")
        if fi is None:
            # Submit button outside any form: handlers still fire, but
            # there is no form to submit. (This was previously reported
            # as "nothing to submit" without dispatching the click.)
            return _js_click_result(doc, el, console, js_error, _b, _a,
                                    click_note)
        try:
            forms = doc.forms()
            new_doc = forms[fi].submit(timeout=timeout)
        except Exception as e:  # noqa: BLE001
            return _info_result("error", el,
                                "form submit failed: %s: %s"
                                % (type(e).__name__, e))
        r = _nav_result("form_submit", el, new_doc)
        r["console"] = console
        r["js_error"] = js_error
        r["note"] = ("click dispatched, then form submitted as the "
                     "default action (the engine does not track "
                     "preventDefault); click note: %s" % click_note
                     if click_note else
                     "click dispatched, then form submitted as the "
                     "default action (the engine does not track "
                     "preventDefault)")
        return r

    if kind in ("button", "clickable") or (
            kind == "input" and el.get("type") == "button"):
        try:
            console, js_error, before, after, note = _js_click(doc, el)
        except (ValueError, RuntimeError) as e:
            return _info_result("error", el, "JS click failed: %s" % e)
        return _js_click_result(doc, el, console, js_error, before, after,
                                note)

    if kind == "toggle":
        before_checked = el.get("checked")
        try:
            console, js_error, _b, _a, _n = _js_click(doc, el)
        except (ValueError, RuntimeError) as e:
            return _info_result("error", el, "JS click failed: %s" % e)
        after_els = interactive_elements(doc,
                                         viewport_width=viewport_width)
        after_el = next(
            (e for e in after_els
             if e["tag"] == el["tag"]
             and e["tag_index"] == el["tag_index"]), None)
        after_checked = after_el.get("checked") if after_el else None
        return {
            "action": "js_click",
            "id": el["id"],
            "tag": el["tag"],
            "kind": kind,
            "box": el.get("box"),
            "new_url": getattr(doc, "url", None),
            "new_title": doc.title(),
            "text_before": None,
            "text_after": None,
            "changed": before_checked != after_checked,
            "console": console,
            "js_error": js_error,
            "note": "checked: %s -> %s" % (before_checked, after_checked),
        }

    if kind in ("input", "textarea"):
        return _info_result(
            "info", el,
            "text-entry control (name=%r); set a value via "
            "doc.forms().fill({...}).submit() for form fields, or run JS "
            "to set .value for standalone controls" % (el.get("name") or ""))
    if kind == "select":
        opts = el.get("options") or []
        return _info_result(
            "info", el,
            "select control (name=%r) with options %s; choose via "
            "doc.forms().fill({name: value}).submit()"
            % (el.get("name") or "",
               [o["value"] for o in opts]))
    if kind == "label":
        target = el.get("label_for") or ""
        tgt = next((e for e in els if e.get("html_id") == target), None) \
            if target else None
        return _info_result(
            "info", el,
            "label for control id=%r%s"
            % (target,
               (" (list id %d, <%s>)" % (tgt["id"], tgt["tag"]))
               if tgt else " (target not in this list)"))
    return _info_result("info", el, "kind %r has no activation" % kind)
