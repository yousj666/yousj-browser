"""Simulated mouse control for AI browsing (pixel-based).

The engine has no rendered window and no real OS pointer, so this
module SIMULATES a mouse: a :class:`Mouse` tracks a virtual cursor
position, and move / click / drag produce human-like paths (see
:func:`yousj.human.human_path`) plus a record of what happened. A
browser AI drives a Mouse to aim at layout boxes; the Mouse itself
never touches a real screen, and it dispatches NO DOM pointer events
(mousemove / mousedown / mouseup) to page JS — the engine has no
pointer-event dispatch. So a Mouse drag does not by itself move a page
slider; it produces the path a slider would see, and the caller
interprets it (see the slider test in this package's README).

What a Mouse holds:
- ``position()``: the current virtual cursor ``(x, y)`` (starts at
  ``(0, 0)`` — the document origin, top-left), or the last position set.
- ``history``: a list of action records (move / click / drag), for
  inspection and debugging.

Methods:
- :meth:`Mouse.move_to` — walk a human path to (x, y).
- :meth:`Mouse.click` — optionally move, then record a click.
- :meth:`Mouse.drag` — move to start, press, walk a human path to end,
  release.
- :meth:`Mouse.move_to_element` / :meth:`Mouse.click_element` /
  :meth:`Mouse.drag_element` — resolve an interactive element's layout
  box and aim at a jittered point INSIDE it (never dead-center twice).
- :meth:`Mouse.hit_test` — which interactive element's box contains a
  pixel point (smallest box wins; approximate — real hit-testing needs
  paint order and z-index).

``dry_run=True`` skips all sleeping (paths are still generated and the
position still updates) — use it in tests. ``human=False`` moves in a
single straight jump (bot-obvious, only for tests).

A browser AI moves the mouse ITSELF through this API: it picks a target
(element number or pixel coordinate), calls ``move_to`` / ``click`` /
``drag``, and reads back the path + final position. It does not wait
for an external driver to move anything.
"""
import random
import time

from . import human as _human


class Mouse:
    """A simulated (virtual) mouse cursor."""

    def __init__(self, x: int = 0, y: int = 0):
        self._pos = (int(x), int(y))
        self.history = []

    def position(self):
        """Current virtual cursor position ``(x, y)``."""
        return self._pos

    def reset(self):
        """Clear history and park the cursor at the origin (0, 0)."""
        self._pos = (0, 0)
        self.history = []

    def _walk(self, path, dry_run: bool):
        """Sleep each point's delay (unless dry_run) and update position."""
        for x, y, delay in path:
            if not dry_run and delay > 0:
                time.sleep(delay)
            self._pos = (x, y)
        return path

    def move_to(self, x, y, human: bool = True, dry_run: bool = False,
                rng=None, overshoot=None):
        """Move the cursor to ``(x, y)``. Returns the path taken.

        ``human=True`` (default) walks :func:`yousj.human.human_path`
        (variable speed, wobble, pauses, possible overshoot-pullback);
        ``human=False`` jumps straight there in one step (bot-obvious).
        ``dry_run=True`` skips sleeping. The path is a list of
        ``(x, y, delay)``; the last point is exactly ``(x, y)``.
        """
        x, y = int(x), int(y)
        start = self._pos
        if human:
            path = _human.human_path(start[0], start[1], x, y, rng=rng,
                                     overshoot=overshoot)
        else:
            path = [(x, y, 0.0)]
        self._walk(path, dry_run)
        self.history.append({"action": "move", "from": start,
                             "to": (x, y), "points": len(path),
                             "human": human})
        return path

    def click(self, x=None, y=None, human: bool = True,
              dry_run: bool = False, rng=None, overshoot=None):
        """Click at the current position, or move to (x, y) first.

        Returns ``{"at": (x, y), "path": [...]}`` where ``path`` is the
        move path (or ``[(x, y, 0.0)]`` when no move was needed). A
        click is only a RECORD here: it dispatches no DOM event. Pair
        it with ``doc.activate(element_id)`` for the DOM effect (see
        :meth:`click_element` with ``activate=True``).
        """
        path = []
        if x is not None and y is not None:
            path = self.move_to(x, y, human=human, dry_run=dry_run,
                                rng=rng, overshoot=overshoot)
        else:
            path = [(self._pos[0], self._pos[1], 0.0)]
        if human and not dry_run:
            time.sleep(random.uniform(0.05, 0.15))  # press+release dwell
        self.history.append({"action": "click", "at": self._pos,
                             "path_points": len(path)})
        return {"at": self._pos, "path": path}

    def drag(self, x0, y0, x1, y1, human: bool = True,
             dry_run: bool = False, rng=None, overshoot=None):
        """Drag from (x0, y0) to (x1, y1): move, press, walk, release.

        Returns ``{"from", "to", "path", "points"}``. The path is the
        human drag trajectory (grab pause before moving, release pause
        after arriving, both slept unless ``dry_run``). Like
        :meth:`click`, this produces a path + record only — it moves no
        page element by itself, because the engine dispatches no pointer
        events. A slider consumer reads ``path`` and decides success
        (e.g. "did the path reach the target zone and end there").
        """
        r = rng or random
        x0, y0, x1, y1 = int(x0), int(y0), int(x1), int(y1)
        if self._pos != (x0, y0):
            self.move_to(x0, y0, human=human, dry_run=dry_run, rng=r)
        if human and not dry_run:
            time.sleep(r.uniform(0.1, 0.3))  # grab pause before moving
        if human:
            path = _human.human_path(x0, y0, x1, y1, rng=r,
                                     overshoot=overshoot)
        else:
            path = [(x1, y1, 0.0)]
        self._walk(path, dry_run)
        if human and not dry_run:
            time.sleep(r.uniform(0.05, 0.2))  # release pause
        self.history.append({"action": "drag", "from": (x0, y0),
                             "to": (x1, y1), "points": len(path),
                             "human": human})
        return {"from": (x0, y0), "to": (x1, y1), "path": path,
                "points": len(path)}

    def _element_target(self, doc, element_id, viewport_width, rng):
        """(box, target) for an interactive element: box + jittered point
        inside it (never dead-center twice). Raises ValueError when the
        element has no number or no layout box."""
        from . import interact as _interact
        from . import layout as _layout
        els = _interact.interactive_elements(doc, with_boxes=False)
        el = next((e for e in els if e["id"] == element_id), None)
        if el is None:
            raise ValueError("no interactive element with id %r"
                             % (element_id,))
        box = _layout.element_box(doc, element_id,
                                  viewport_width=viewport_width)
        if box is None:
            raise ValueError(
                "element %d (<%s>) has no layout box (nested inline "
                "element, display:none, or layout failure); aim by "
                "pixels instead" % (element_id, el["tag"]))
        r = rng or random
        jx = r.uniform(-0.3, 0.3) * box["width"]
        jy = r.uniform(-0.3, 0.3) * box["height"]
        tx = int(round(box["x"] + box["width"] / 2.0 + jx))
        ty = int(round(box["y"] + box["height"] / 2.0 + jy))
        return el, box, (tx, ty)

    def move_to_element(self, doc, element_id, viewport_width: int = 1280,
                        human: bool = True, dry_run: bool = False,
                        rng=None, overshoot=None):
        """Move the cursor onto interactive element ``element_id``.

        Returns ``{"element", "box", "target", "path"}``. The target is
        the box center plus a random jitter inside the box.
        """
        el, box, target = self._element_target(doc, element_id,
                                               viewport_width, rng)
        path = self.move_to(target[0], target[1], human=human,
                            dry_run=dry_run, rng=rng, overshoot=overshoot)
        return {"element": element_id, "tag": el["tag"], "box": box,
                "target": target, "path": path}

    def click_element(self, doc, element_id, viewport_width: int = 1280,
                      human: bool = True, dry_run: bool = False,
                      rng=None, overshoot=None, activate: bool = False,
                      timeout: int = 15):
        """Move onto element ``element_id`` and click it.

        Returns ``{"element", "box", "target", "path", "at", "activate"}``.
        With ``activate=False`` (default) this is pointer-only (a record,
        no DOM effect). With ``activate=True`` it additionally calls
        ``doc.activate(element_id)`` and includes that result under
        ``"activate"`` — the honest split: the mouse moves the pointer,
        ``activate()`` performs the DOM effect.
        """
        moved = self.move_to_element(doc, element_id,
                                     viewport_width=viewport_width,
                                     human=human, dry_run=dry_run, rng=rng,
                                     overshoot=overshoot)
        clicked = self.click(human=human, dry_run=dry_run, rng=rng)
        out = {"element": element_id, "tag": moved["tag"],
               "box": moved["box"], "target": moved["target"],
               "path": moved["path"], "at": clicked["at"],
               "activate": None}
        if activate:
            out["activate"] = doc.activate(element_id, timeout=timeout,
                                           pace=False,
                                           viewport_width=viewport_width)
        return out

    def drag_element(self, doc, element_id, x1, y1,
                     viewport_width: int = 1280, human: bool = True,
                     dry_run: bool = False, rng=None, overshoot=None):
        """Drag interactive element ``element_id`` (from its box center)
        to pixel ``(x1, y1)`` — the slider gesture.

        Returns ``{"element", "box", "from", "to", "path", "points"}``.
        """
        el, box, start = self._element_target(doc, element_id,
                                              viewport_width, rng)
        res = self.drag(start[0], start[1], x1, y1, human=human,
                        dry_run=dry_run, rng=rng, overshoot=overshoot)
        res["element"] = element_id
        res["tag"] = el["tag"]
        res["box"] = box
        return res

    def hit_test(self, doc, x, y, viewport_width: int = 1280):
        """Interactive element whose layout box contains pixel (x, y).

        Smallest containing box wins (a nested element's box sits inside
        its parent's, so the smallest is the most specific target).
        Returns the element dict (with ``box``), or None. Approximate:
        real hit-testing also needs paint order and z-index, which this
        minimal layout does not model.
        """
        from . import interact as _interact
        els = _interact.interactive_elements(
            doc, viewport_width=viewport_width, with_boxes=True)
        best = None
        best_area = None
        for el in els:
            b = el.get("box")
            if not b:
                continue
            if b["x"] <= x < b["x"] + b["width"] and \
                    b["y"] <= y < b["y"] + b["height"]:
                area = b["width"] * b["height"]
                if best_area is None or area < best_area:
                    best, best_area = el, area
        return best
