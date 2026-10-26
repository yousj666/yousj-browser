"""Wiring check v2 (by Muse): SmartRegistration + RealBrowserAdapter + RealBrowser.

Runs the smart-registration flow against the LOCAL test page only, with a
fake SMS code. Two shims (this script only; registration.py and real.py
are untouched):
- mouse_drag(path): RealBrowser has no path-based drag, so translate
  path -> RealMouse.drag(start, end). RealMouse.drag regenerates its own
  human trajectory (same human.human_path generator), so the page still
  sees a human drag.
- slider_geometry(): the local test page's handle has no role=slider and
  no slider text, so the base adapter cannot find it via elements(). This
  override reads #sliderTrack / #sliderHandle boxes via evaluate() and
  returns track-based geometry (press at handle center, drag to the
  track's right zone); falls back to the base adapter for other pages.
"""
import json
import sys

sys.path.insert(0, 'python')

from yousj.real import RealBrowser
from yousj import registration

_SLIDER_JS = (
    "(() => { const t = document.getElementById('sliderTrack');"
    " const h = document.getElementById('sliderHandle');"
    " if (!t || !h) return null;"
    " const tr = t.getBoundingClientRect();"
    " const hr = h.getBoundingClientRect();"
    " if (tr.width <= 0) return null;"
    " return JSON.stringify({track: {x: tr.x, y: tr.y, width: tr.width,"
    " height: tr.height}, handle: {x: hr.x, y: hr.y, width: hr.width,"
    " height: hr.height}}); })()"
)


class RealBrowserShim:
    """Expose adapter-expected mouse_drag(path) on a RealBrowser."""

    def __init__(self, real):
        self._real = real

    def __getattr__(self, name):
        return getattr(self._real, name)

    def mouse_drag(self, path):
        if not path:
            return False
        # Points may be (x, y) or (x, y, delay); index, don't unpack.
        x0, y0 = path[0][0], path[0][1]
        x1, y1 = path[-1][0], path[-1][1]
        return self._real.mouse.drag(int(round(x0)), int(round(y0)),
                                     int(round(x1)), int(round(y1)))


class WiringAdapter(registration.RealBrowserAdapter):
    """Adapter + track-based slider geometry for the local test page."""

    def _elements(self):
        # RealBrowser.elements() does not expose input placeholders, but
        # the adapter matches the SMS-code field by placeholder text
        # ("请输入短信验证码"). Enrich elements with placeholders via
        # evaluate(), joined on html_id. Falls back to unenriched on error.
        els = super()._elements()
        try:
            res = self.real.evaluate(
                "JSON.stringify([...document.querySelectorAll('input')]"
                ".map(el => [el.id, el.placeholder || '']))")
            pairs = json.loads(res) if isinstance(res, str) else res
            if isinstance(pairs, list):
                ph = {pid: p for pid, p in pairs if pid}
                for el in els:
                    hid = el.get("html_id")
                    if hid in ph and ph[hid]:
                        el["placeholder"] = ph[hid]
        except Exception:
            pass
        return els

    def slider_geometry(self):
        try:
            res = self.real.evaluate(_SLIDER_JS)
            if isinstance(res, str):
                res = json.loads(res)
            if isinstance(res, dict) and res.get("track"):
                tr = res["track"]
                hd = res["handle"]
                cx = hd["x"] + hd["width"] / 2.0
                cy = hd["y"] + hd["height"] / 2.0
                x1 = tr["x"] + tr["width"]
                return {"x0": cx, "y0": cy, "x1": x1, "y1": cy,
                        "target": (x1 - 30, x1), "track_y": cy}
        except Exception:
            pass
        return super().slider_geometry()


def fake_sms():
    # Local test only. Fake code, never a real user's code.
    return "123456"


def main():
    task = registration.RegistrationTask(
        site="local-test",
        start_url=("file:///home/hatch/workspace/yousj-browser/tests/"
                   "registration/test_register.html"),
        phone="13800000000",  # fake test number, not a real user phone
        dry_run=False)
    with RealBrowser() as rb:
        driver = WiringAdapter(RealBrowserShim(rb))
        result = registration.SmartRegistration(driver).run(task, fake_sms)
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
