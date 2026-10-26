"""Local-only test for yousj.real (real Chromium backend via CDP).

Serves tests/pages/ on 127.0.0.1 and drives a real Chromium through
goto / elements / fill / activate / screenshot / drag_slider. No
external site is touched, no login is attempted — real-site verification
is Muse's job, not this test's.

Run from the project root:
    PYTHONPATH=python python3 tests/test_real_browser.py
Exit 0 when every check passes, 1 otherwise.
"""
import os
import random
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, os.path.join(ROOT, "python"))

from yousj.real import RealBrowser  # noqa: E402

RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok)))
    print("%s %s %s" % ("PASS" if ok else "FAIL", name, detail))


def main():
    pages = os.path.join(HERE, "pages")
    # file://, not http://127.0.0.1: headless Chrome 152 blocks local
    # HTTP targets with ERR_BLOCKED_BY_LOCAL_NETWORK_ACCESS_CHECKS
    # (--no-proxy-server and --disable-features LNA flags both tried,
    # both failed), while file:// loads fine. Static test pages need
    # no server, so file:// is the correct harness here.
    base = "file://" + pages
    print("testing pages at %s" % base)

    try:
        with RealBrowser() as b:
            # 1. goto + title + url
            r = b.goto(base + "/form.html")
            check("goto-form", r["title"] == "Local form test"
                  and r["url"].endswith("/form.html"), str(r))

            # 2. elements: real boxes, touchable
            els = b.elements()
            by_id = {e.get("html_id"): e for e in els}
            name_el = by_id.get("name")
            go_el = by_id.get("go")
            link_el = by_id.get("tolink")
            check("elements-present", bool(name_el and go_el and link_el),
                  "ids=%s" % sorted(by_id.keys()))
            for label, e in (("input", name_el), ("button", go_el),
                             ("link", link_el)):
                if e is None:
                    continue
                box = e.get("box") or {}
                check("box-%s" % label,
                      box.get("width", 0) > 0 and box.get("height", 0) > 0,
                      str(box))
                check("touchable-%s" % label, e.get("touchable") is True,
                      e.get("touchable_reason", ""))
            print("--- format_elements ---")
            print(b.format_elements(els))
            print("-----------------------")

            # 3. fill (real keyboard path) + activate (real click)
            if name_el is not None and go_el is not None:
                filled = b.fill(name_el["id"], "皮蛋")
                check("fill", filled.get("value_after") == "皮蛋",
                      str(filled))
                act = b.activate(go_el["id"])
                check("activate-changed", act.get("changed") is True
                      and "皮蛋" in (act.get("text_after") or ""),
                      "changed=%s after=%r" % (act.get("changed"),
                                               (act.get("text_after") or "")[:60]))
            else:
                check("fill", False, "input/button element missing, skipped")
                check("activate-changed", False, "input/button missing")

            # 4. real screenshot (PNG magic, non-trivial size)
            shot = b.screenshot(path="/tmp/yousj-real-shot.png")
            with open("/tmp/yousj-real-shot.png", "rb") as f:
                magic = f.read(8)
            check("screenshot-png", magic == b"\x89PNG\r\n\x1a\n"
                  and shot["bytes"] > 1000,
                  "bytes=%d path=%s" % (shot["bytes"], shot["path"]))

            # 5. navigation via real link click
            if link_el is not None:
                act2 = b.activate(link_el["id"])
                check("link-navigates", b.url().endswith("/slider.html")
                      and b.title() == "Local slider test",
                      "url=%s title=%s" % (b.url(), b.title()))
            else:
                check("link-navigates", False, "link element missing")

            # 6. slider: human drag to target zone, page says passed
            els2 = b.elements()
            handle = next((e for e in els2
                           if e.get("html_id") == "handle"), None)
            check("slider-handle-found", handle is not None,
                  "handle=%s" % (handle or ""))
            if handle is not None:
                box0 = handle["box"]
                start_x = box0["x"] + box0["width"] // 2
                target_x = start_x + 340  # handle left 0 -> 340 (zone 330..360)
                res = b.drag_slider(
                    handle_id=handle["id"], target_x=target_x,
                    check_js="document.getElementById('status').dataset.status",
                    rng=random.Random(42), overshoot=True)
                print("drag result: from=%s to=%s points=%d "
                      "box_before=%s box_after=%s state=%r"
                      % (res["from"], res["to"], res["path_points"],
                         res["handle_box_before"], res["handle_box_after"],
                         res["state_after"]))
                check("slider-passed", res["state_after"] == "passed",
                      "state=%r" % (res["state_after"],))
                box1 = res["handle_box_after"] or {}
                check("slider-moved", box1.get("x", 0) > box0["x"],
                      "x %s -> %s" % (box0["x"], box1.get("x")))
                path = res["path"]
                check("drag-path-human", len(path) > 10
                      and path[-1][:2] == res["to"],
                      "points=%d last=%s to=%s"
                      % (len(path), path[-1][:2] if path else None,
                         res["to"]))
                # overshoot forced: some point must pass the target x,
                # then come back to exactly the target.
                xs = [p[0] for p in path]
                check("drag-overshoot", max(xs) > target_x
                      and path[-1][0] == target_x,
                      "max_x=%d target_x=%d" % (max(xs), target_x))
                check("mouse-position", b.mouse.position() == res["to"],
                      "pos=%s to=%s" % (b.mouse.position(), res["to"]))

            # 7. context manager closes the browser process
            proc = b._proc
        check("browser-closed", proc is not None and proc.poll() is not None,
              "poll=%s" % (proc.poll() if proc else None))
    finally:
        pass  # no server to shut down; the context manager closes Chrome

    failed = [n for n, ok in RESULTS if not ok]
    print("=== %d/%d checks passed ===" % (len(RESULTS) - len(failed),
                                            len(RESULTS)))
    if failed:
        print("FAILED:", failed)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
