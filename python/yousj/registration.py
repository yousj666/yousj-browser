"""Smart registration backend for the Yousj browser (independent module).

A registration job driven by AI, acting like a human: fill the phone
number, tap "get code", drag a slider with a human trajectory, wait for
the caller to supply the SMS code, submit, and report the account.

This module is INDEPENDENT of the real-Chromium rendering backend (that
is a separate backend, built by another worker; its API is not settled
yet). The browser side here is a pluggable driver
(:class:`BaseBrowserDriver`):

- :class:`StubBrowserDriver` — a local state machine that mirrors a
  registration page (phone -> get code -> slider -> SMS code -> submit).
  Used to run the whole flow locally with fake data, no real site.
- :class:`RealBrowserAdapter` — translates driver calls into calls on a
  real rendering browser object (the "RealBrowser" backend). It is
  written against an ASSUMED RealBrowser interface (documented below);
  once that backend lands, swap the stub for
  ``RealBrowserAdapter(real_browser)`` and re-run the same flow.

Privacy (hard rules, not suggestions):
- The SMS code is supplied by the CALLER through a provider callable.
  This module uses it once to fill the code field, then drops it
  (``code = None`` in a ``finally`` block). It is never stored on any
  object, never logged, never written to disk, never put in a result.
- The phone number lives in memory only for the duration of the run
  (the flow needs it to fill the field). Traces and logs carry only a
  masked phone (first 3 + ``****`` + last 2). The full phone is returned
  in the result dict (``account``) for the caller to use — the caller
  decides what to tell the user; this module persists nothing.
- Test data is fake and never sent anywhere (the stub talks to no
  network; the local test page is pure local HTML/JS).

Human verification:
- Slider drags use :func:`yousj.human.human_path` (variable speed,
  wobble, pauses, overshoot-and-pullback — never a straight uniform
  line). The driver receives the full ``[(x, y, delay), ...]`` path and
  plays it; a single-point teleport or a perfectly uniform path is a
  robot signature and is rejected by the stub (and should be rejected
  by any honest slider consumer).
- The orchestrator retries a failed slider drag ONCE with a fresh
  trajectory, then gives up (no hammering).

Assumed RealBrowser interface (for :class:`RealBrowserAdapter`):
- ``goto(url)``
- ``elements()`` -> list of element dicts like :func:`yousj.interact.
  interactive_elements` (``id``, ``tag``, ``type``, ``text``, ``box``,
  ``touchable``; ``placeholder``/``name``/``role`` when available)
- ``fill(element_id, text)`` (fallback: ``type(element_id, text)``)
- ``click(element_id)`` (fallback: ``activate(element_id)``)
- ``mouse_drag(path)`` (fallbacks: ``drag(path)``,
  ``mouse_drag_points(path)``) — plays ``[(x, y, delay), ...]`` with
  real pointer events
- ``read_result()`` (optional) and/or ``text()`` for result inference
- ``close()`` / ``disconnect()`` (optional cleanup)

If a method is missing, the adapter raises a clear error naming it —
it never silently pretends to have acted.
"""

import random

from . import human as _human


# ------------------------------------------------------------------ privacy
def _mask_phone(phone):
    """Mask a phone number for traces: first 3 + **** + last 2."""
    if not phone:
        return "****"
    s = str(phone)
    if len(s) < 7:
        return "****"
    return s[:3] + "****" + s[-2:]


# ------------------------------------------------------------------ task
class RegistrationTask:
    """One registration job.

    ``site`` is a label (e.g. "douyin" for real use, "local-test" for
    tests). ``start_url`` is the registration page. ``phone`` is the
    phone number to register with. ``method`` is currently only
    "phone_sms" (phone + SMS code). ``sms_timeout_s`` is advisory: the
    SMS provider is expected to return None on timeout (see
    :meth:`SmartRegistration.run`). ``dry_run`` skips pacing sleeps
    (tests). ``rng`` is a ``random.Random`` for deterministic
    trajectories in tests (default: global ``random``).
    """

    def __init__(self, site, start_url, phone, method="phone_sms",
                 area_code="+86", sms_timeout_s=120.0, dry_run=False,
                 rng=None):
        if not site or not start_url or not phone:
            raise ValueError("site, start_url and phone are required")
        if method != "phone_sms":
            raise ValueError(
                "unsupported method %r (only 'phone_sms' for now)"
                % (method,))
        self.site = site
        self.start_url = start_url
        self.phone = str(phone)
        self.method = method
        self.area_code = area_code
        self.sms_timeout_s = sms_timeout_s
        self.dry_run = dry_run
        self.rng = rng

    def masked_phone(self):
        """Masked phone for traces (never the full number)."""
        return _mask_phone(self.phone)


# ------------------------------------------------------------------ driver API
class BaseBrowserDriver:
    """Pluggable browser side of registration.

    Every method returns True/False (or a documented value) and never
    raises for an expected page state (e.g. "no slider"); it raises
    only for programmer errors (e.g. a missing RealBrowser method in
    the adapter). No method may log or persist a phone number or an
    SMS code.
    """

    def goto(self, url):
        """Open the registration page. Returns True on success."""
        raise NotImplementedError

    def find_phone_field(self):
        """Return a truthy handle for the phone input, or None."""
        raise NotImplementedError

    def fill_phone(self, phone):
        """Fill the phone field. Returns True on success."""
        raise NotImplementedError

    def click_get_code(self):
        """Tap 'get verification code'. Returns True if triggered."""
        raise NotImplementedError

    def slider_geometry(self):
        """Return None when no slider is present, else a dict::

            {"x0": handle start x, "y0": handle start y,
             "x1": track end x, "y1": handle start y,
             "target": (target_x0, target_x1), "track_y": y}
        """
        raise NotImplementedError

    def drag_trajectory(self, path):
        """Play a ``[(x, y, delay), ...]`` drag path on the slider.

        Returns True when the slider accepts the drag, False otherwise.
        """
        raise NotImplementedError

    def fill_sms_code(self, code):
        """Fill the SMS code field. Returns True on success.

        Must not store the code anywhere (use it, drop it).
        """
        raise NotImplementedError

    def submit(self):
        """Submit the registration form. Returns True on success."""
        raise NotImplementedError

    def read_result(self):
        """Return ``{"registered": bool, "needs_real_name": bool,
        "account": str|None, "message": str}``."""
        raise NotImplementedError

    def close(self):
        """Optional cleanup; default no-op."""
        return None


# ------------------------------------------------------------------ stub driver
class StubBrowserDriver(BaseBrowserDriver):
    """Local registration-page state machine (tests only, no network).

    Mirrors a typical phone+SMS registration page::

        page -> phone_filled -> code_requested -> [slider] ->
        code_filled -> submitted -> registered

    The slider check is honest, not rigged: a drag passes only when
    (a) the path ends inside the target zone, (b) it has at least 5
    points, and (c) it is not perfectly uniform (identical step sizes
    AND identical delays = robot signature). A human trajectory from
    :func:`yousj.human.human_path` passes; a single-point teleport or
    a uniform straight line fails.

    The SMS code is never stored: :meth:`fill_sms_code` records only
    a boolean flag and drops the value.
    """

    def __init__(self, slider=True, require_real_name=False,
                 track_x0=40, track_x1=360, track_y=100,
                 target_zone=(330, 360)):
        self.slider = slider
        self.require_real_name = require_real_name
        self.track_x0 = track_x0
        self.track_x1 = track_x1
        self.track_y = track_y
        self.target_zone = target_zone
        self.state = "start"
        self._slider_pending = False
        self._phone = None
        self._code_received = False
        self._submitted = False
        self.last_error = None
        self.last_trajectory = None
        self.last_drag_ok = None
        self.last_drag_reason = None

    def goto(self, url):
        self.state = "page"
        self.last_error = None
        return True

    def find_phone_field(self):
        return "phone" if self.state != "start" else None

    def fill_phone(self, phone):
        if self.state != "page" or not phone:
            self.last_error = "bad_order"
            return False
        # In-memory only, for this run; never persisted. (Test data.)
        self._phone = str(phone)
        self.state = "phone_filled"
        return True

    def click_get_code(self):
        if self.state != "phone_filled":
            self.last_error = "bad_order"
            return False
        self.state = "code_requested"
        self._slider_pending = bool(self.slider)
        return True

    def slider_geometry(self):
        if not self._slider_pending:
            return None
        return {"x0": self.track_x0, "y0": self.track_y,
                "x1": self.track_x1, "y1": self.track_y,
                "target": self.target_zone, "track_y": self.track_y}

    def drag_trajectory(self, path):
        if not self._slider_pending:
            self.last_error = "no_slider"
            return False
        pts = [(p[0], p[1]) for p in path]
        delays = [p[2] for p in path if len(p) > 2]
        self.last_trajectory = list(path)
        tx0, tx1 = self.target_zone
        lx, ly = pts[-1]
        if not (tx0 <= lx <= tx1 and abs(ly - self.track_y) <= 15):
            self.last_drag_ok = False
            self.last_drag_reason = "endpoint outside target zone"
            return False
        if len(pts) < 5:
            self.last_drag_ok = False
            self.last_drag_reason = "too few points (teleport?)"
            return False
        steps = [((pts[i + 1][0] - pts[i][0]) ** 2
                  + (pts[i + 1][1] - pts[i][1]) ** 2) ** 0.5
                 for i in range(len(pts) - 1)]
        uniform_steps = all(abs(s - steps[0]) < 0.5 for s in steps)
        uniform_delays = (
            len(delays) >= 2
            and all(abs(d - delays[0]) < 1e-9 for d in delays))
        if uniform_steps and uniform_delays:
            self.last_drag_ok = False
            self.last_drag_reason = "uniform robot path"
            return False
        self.last_drag_ok = True
        self.last_drag_reason = None
        self._slider_pending = False
        self.state = "slider_passed"
        return True

    def fill_sms_code(self, code):
        if self.state not in ("code_requested", "slider_passed"):
            self.last_error = "bad_order"
            return False
        if self._slider_pending:
            self.last_error = "slider_not_passed"
            return False
        if not code:
            self.last_error = "empty_code"
            return False
        # Deliberately NOT stored: only a boolean flag survives.
        self._code_received = True
        self.state = "code_filled"
        return True

    def submit(self):
        if self.state != "code_filled":
            self.last_error = "bad_order"
            return False
        self.state = "submitted"
        self._submitted = True
        return True

    def read_result(self):
        if self.state == "submitted":
            return {"registered": True,
                    "needs_real_name": bool(self.require_real_name),
                    "account": self._phone,
                    "message": "registered" + (
                        "; real-name verification required"
                        if self.require_real_name else "")}
        return {"registered": False, "needs_real_name": False,
                "account": None,
                "message": "not submitted (state=%s)" % (self.state,)}


# ------------------------------------------------------------------ RealBrowser adapter
_PHONE_WORDS = ("手机号", "手机", "phone", "tel", "mobile")
_CODE_WORDS = ("验证码", "短信码", "sms", "verification code", "code")
_GET_CODE_TEXTS = ("获取验证码", "发送验证码", "get code", "send code",
                   "get verification code")
_SUBMIT_TEXTS = ("注册", "提交", "register", "submit", "sign up")
_SLIDER_WORDS = ("滑块", "slider", "drag", "拖动")


def _searchable(el):
    """Lowercased text pooled from the element's text-ish fields."""
    parts = []
    for key in ("text", "placeholder", "name", "type", "tag", "role"):
        v = el.get(key)
        if v:
            parts.append(str(v))
    return " ".join(parts).lower()


def _has_any(haystack, words):
    return any(w.lower() in haystack for w in words)


class RealBrowserAdapter(BaseBrowserDriver):
    """Translate driver calls into RealBrowser calls (assumed API).

    The RealBrowser backend is built by another worker and its API is
    not settled yet, so this adapter is defensive: it looks up each
    method with ``getattr`` and tries documented fallbacks, and raises
    a clear ``RuntimeError`` naming the missing method instead of
    silently pretending. When the real backend lands, construct
    ``RealBrowserAdapter(real_browser)`` and run the same
    :class:`SmartRegistration` flow; adjust ``locators`` only if a
    site's fields need different keywords.
    """

    def __init__(self, real, locators=None):
        self.real = real
        self.locators = dict(
            phone_words=_PHONE_WORDS,
            code_words=_CODE_WORDS,
            get_code_texts=_GET_CODE_TEXTS,
            submit_texts=_SUBMIT_TEXTS,
            slider_words=_SLIDER_WORDS,
        )
        if locators:
            self.locators.update(locators)
        self._phone_id = None
        self._code_id = None

    def _elements(self):
        fn = getattr(self.real, "elements", None)
        if fn is None:
            raise RuntimeError(
                "RealBrowser has no elements() method; cannot list fields")
        return fn()

    def _find(self, predicate):
        for el in self._elements():
            if predicate(el):
                return el
        return None

    def _click(self, el):
        el_id = el["id"]
        for name in ("click", "activate"):
            fn = getattr(self.real, name, None)
            if fn is not None:
                fn(el_id)
                return True
        raise RuntimeError(
            "RealBrowser has neither click(id) nor activate(id)")

    def _fill(self, el, text):
        el_id = el["id"]
        for name in ("fill", "type"):
            fn = getattr(self.real, name, None)
            if fn is not None:
                fn(el_id, text)
                return True
        raise RuntimeError(
            "RealBrowser has neither fill(id, text) nor type(id, text)")

    def goto(self, url):
        fn = getattr(self.real, "goto", None)
        if fn is None:
            raise RuntimeError("RealBrowser has no goto(url) method")
        fn(url)
        return True

    def find_phone_field(self):
        el = self._find(
            lambda e: e.get("tag") == "input" and (
                e.get("type") in ("tel", "phone")
                or _has_any(_searchable(e), self.locators["phone_words"])))
        self._phone_id = el["id"] if el else None
        return self._phone_id

    def fill_phone(self, phone):
        el_id = self._phone_id
        if el_id is None:
            self.find_phone_field()
            el_id = self._phone_id
        if el_id is None:
            return False
        el = self._find(lambda e: e.get("id") == el_id) or {"id": el_id}
        self._fill(el, phone)
        return True

    def click_get_code(self):
        el = self._find(
            lambda e: _has_any(str(e.get("text", "")).lower(),
                               self.locators["get_code_texts"]))
        if el is None:
            return False
        self._click(el)
        return True

    def slider_geometry(self):
        el = self._find(
            lambda e: str(e.get("role", "")).lower() == "slider"
            or _has_any(_searchable(e), self.locators["slider_words"]))
        box = el.get("box") if el else None
        if not box:
            return None
        x0 = box["x"]
        y = box["y"] + box["height"] // 2
        x1 = box["x"] + box["width"]
        return {"x0": x0, "y0": y, "x1": x1, "y1": y,
                "target": (x1 - 30, x1), "track_y": y}

    def drag_trajectory(self, path):
        for name in ("mouse_drag", "drag", "mouse_drag_points"):
            fn = getattr(self.real, name, None)
            if fn is not None:
                res = fn(path)
                if isinstance(res, dict):
                    return bool(res.get("ok", True))
                return True if res is None else bool(res)
        raise RuntimeError(
            "RealBrowser has no mouse_drag(path)/drag(path)/"
            "mouse_drag_points(path) method; cannot play the trajectory")

    def fill_sms_code(self, code):
        el = self._find(
            lambda e: e.get("tag") == "input"
            and _has_any(_searchable(e), self.locators["code_words"])
            and e.get("id") != self._phone_id)
        if el is None:
            return False
        self._code_id = el["id"]
        self._fill(el, code)
        return True

    def submit(self):
        el = self._find(
            lambda e: _has_any(str(e.get("text", "")).lower(),
                               self.locators["submit_texts"]))
        if el is None:
            return False
        self._click(el)
        return True

    def read_result(self):
        fn = getattr(self.real, "read_result", None)
        if fn is not None:
            res = fn()
            return {"registered": bool(res.get("registered", False)),
                    "needs_real_name": bool(res.get("needs_real_name",
                                                   False)),
                    "account": res.get("account"),
                    "message": str(res.get("message", ""))}
        text_fn = getattr(self.real, "text", None)
        if text_fn is None:
            return {"registered": False, "needs_real_name": False,
                    "account": None,
                    "message": "indeterminate: RealBrowser exposes neither "
                               "read_result() nor text()"}
        text = str(text_fn()).lower()
        registered = any(w in text for w in ("注册成功", "registered",
                                             "success", "欢迎"))
        needs = any(w in text for w in ("实名", "real-name",
                                        "verify identity", "实名认证"))
        return {"registered": registered, "needs_real_name": needs,
                "account": None,
                "message": "inferred from page text"}

    def close(self):
        for name in ("close", "disconnect"):
            fn = getattr(self.real, name, None)
            if fn is not None:
                fn()
                return None
        return None


# ------------------------------------------------------------------ orchestrator
class SmartRegistration:
    """Run a registration task through a driver, like a human.

    ``sms_code_provider`` is a callable supplied by the caller: it is
    invoked once, when the SMS code is needed, and must return the code
    string, or None on timeout. The orchestrator fills the code and
    drops it immediately; it never stores, logs, or returns it.
    """

    def __init__(self, driver, dry_run=False, rng=None):
        self.driver = driver
        self.dry_run = dry_run
        self.rng = rng
        self._pacer = _human.Pacer()

    def _pace(self):
        if not self.dry_run:
            self._pacer.pace()

    def run(self, task, sms_code_provider):
        """Run ``task``; return a result dict (never contains the code).

        Result keys: ``ok``, ``status``, ``account`` (phone when
        registered, else None), ``needs_real_name``, ``steps`` (masked
        trace), ``slider`` (trajectory stats), ``error``, ``site``.
        """
        if not callable(sms_code_provider):
            raise TypeError(
                "sms_code_provider must be callable returning the SMS "
                "code (or None on timeout)")
        steps = []

        def record(step, ok, detail=""):
            steps.append({"step": step, "ok": bool(ok), "detail": detail})

        def fail(status, slider_info=None):
            return {"ok": False, "status": status, "account": None,
                    "needs_real_name": False, "steps": steps,
                    "slider": slider_info or {}, "error": status,
                    "site": task.site}

        # 1. Open the registration page.
        if not self.driver.goto(task.start_url):
            record("goto", False, task.start_url)
            return fail("failed:goto")
        record("goto", True, task.start_url)
        self._pace()

        # 2. Fill the phone number (masked in the trace).
        if not self.driver.find_phone_field():
            record("find_phone_field", False)
            return fail("failed:phone_field")
        if not self.driver.fill_phone(task.phone):
            record("fill_phone", False, "phone " + task.masked_phone())
            return fail("failed:fill_phone")
        record("fill_phone", True, "phone " + task.masked_phone())
        self._pace()

        # 3. Request the SMS code.
        if not self.driver.click_get_code():
            record("get_code", False)
            return fail("failed:get_code")
        record("get_code", True)
        self._pace()

        # 4. Human-verification slider, if one appears (one retry).
        slider_info = {"points": 0, "attempts": 0, "passed": None}
        geom = self.driver.slider_geometry()
        if geom is not None:
            tx0, tx1 = geom["target"]
            target_x = (tx0 + tx1) // 2
            target_y = geom.get("track_y", geom["y0"])
            passed = False
            path = []
            for attempt in (1, 2):
                path = _human.human_path(geom["x0"], geom["y0"],
                                         target_x, target_y, rng=self.rng)
                ok = self.driver.drag_trajectory(path)
                record("slider_drag", ok,
                       "attempt %d, %d points, end (%d,%d)"
                       % (attempt, len(path), path[-1][0], path[-1][1]))
                slider_info = {"points": len(path), "attempts": attempt,
                               "passed": bool(ok)}
                if ok:
                    passed = True
                    break
                self._pace()
            if not passed:
                return fail("slider_failed", slider_info)
        self._pace()

        # 5. SMS code: ask the caller once, fill it, drop it at once.
        try:
            code = sms_code_provider()
        except Exception as exc:  # provider failure == no code
            record("sms_code", False, "provider error: %s"
                   % (type(exc).__name__,))
            return fail("sms_timeout", slider_info)
        if not code:
            record("sms_code", False, "no code (timeout)")
            return fail("sms_timeout", slider_info)
        try:
            ok = self.driver.fill_sms_code(code)
        finally:
            code = None  # dropped immediately; never stored/logged
        if not ok:
            record("fill_sms_code", False, "code received")
            return fail("failed:fill_sms_code", slider_info)
        record("fill_sms_code", True, "code received")
        self._pace()

        # 6. Submit.
        if not self.driver.submit():
            record("submit", False)
            return fail("submit_failed", slider_info)
        record("submit", True)
        self._pace()

        # 7. Read the result.
        res = self.driver.read_result()
        registered = bool(res.get("registered", False))
        needs = bool(res.get("needs_real_name", False))
        account = res.get("account") or (task.phone if registered else None)
        status = ("registered_needs_real_name"
                  if registered and needs else
                  "registered" if registered else "failed:submit_result")
        record("read_result", registered, res.get("message", ""))
        return {"ok": registered, "status": status, "account": account,
                "needs_real_name": needs, "steps": steps,
                "slider": slider_info,
                "error": None if registered else status,
                "site": task.site}


def run_registration(task, driver, sms_code_provider):
    """Convenience: run ``task`` through ``driver`` (dry_run from task)."""
    return SmartRegistration(driver, dry_run=task.dry_run,
                             rng=task.rng).run(task, sms_code_provider)
