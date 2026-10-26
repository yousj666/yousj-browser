"""Local tests for yousj.registration (fake data only, no network).

Run:  PYTHONPATH=<repo>/python python3 tests/registration/test_registration.py
Exit 0 when every test passes, 1 otherwise.

Nothing here touches a real site or a real account: the stub driver
talks to no network, the fake phone/code below are dummy values used
in-memory only, and this script prints NO full phone number and NO
code value (masked phone + statuses + counts only).
"""

import json
import os
import random
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(
        os.path.abspath(__file__)))), "python"))

from yousj import human, registration  # noqa: E402

# Fake test data only. Never sent anywhere (stub driver, no network).
# NOTE: the fake code must not be a substring of the fake phone, or the
# "code never leaks into results" assertions below would false-positive
# on the phone's own digits.
PHONE = "13000000000"
CODE = "123456"

FAILURES = []


def check(name, cond, detail=""):
    print("%s %s %s" % ("PASS" if cond else "FAIL", name, detail))
    if not cond:
        FAILURES.append(name)


def masked():
    return registration._mask_phone(PHONE)


def run_flow(stub, **task_kw):
    task = registration.RegistrationTask(
        site="local-test",
        start_url="http://localhost/test_register.html",
        phone=PHONE, dry_run=True, **task_kw)
    reg = registration.SmartRegistration(
        stub, dry_run=True, rng=task_kw.get("rng"))
    return reg.run(task, lambda: CODE)


def test_happy_path():
    stub = registration.StubBrowserDriver(slider=True,
                                          require_real_name=False)
    res = run_flow(stub, rng=random.Random(12345))
    check("happy: ok", res["ok"] is True)
    check("happy: status registered", res["status"] == "registered")
    check("happy: needs_real_name False",
          res["needs_real_name"] is False)
    check("happy: account returned", res["account"] == PHONE)
    check("happy: all steps ok",
          all(s["ok"] for s in res["steps"]),
          "steps=%s" % ([s["step"] for s in res["steps"]],))
    check("happy: slider passed", res["slider"].get("passed") is True)
    check("happy: slider points >= 5", res["slider"].get("points", 0) >= 5,
          "points=%s" % (res["slider"].get("points"),))
    lx, ly = stub.last_trajectory[-1][:2]
    check("happy: drag ends in target zone",
          330 <= lx <= 360 and abs(ly - 100) <= 15,
          "end=(%s,%s)" % (lx, ly))
    check("happy: code not in result", CODE not in json.dumps(res))
    check("happy: code not stored on stub",
          CODE not in repr(stub.__dict__))
    check("happy: full phone not in trace",
          PHONE not in json.dumps(res["steps"]),
          "masked=" + masked())
    check("happy: code flag set", stub._code_received is True)


def test_real_name():
    stub = registration.StubBrowserDriver(slider=True,
                                          require_real_name=True)
    res = run_flow(stub, rng=random.Random(99))
    check("realname: status", res["status"] == "registered_needs_real_name")
    check("realname: flag", res["needs_real_name"] is True)


def test_no_slider():
    stub = registration.StubBrowserDriver(slider=False)
    res = run_flow(stub, rng=random.Random(5))
    check("noslider: registered", res["status"] == "registered")
    check("noslider: slider skipped",
          res["slider"].get("passed") is None)
    check("noslider: no drag step",
          not any(s["step"] == "slider_drag" for s in res["steps"]))


def test_bot_drag_rejected():
    stub = registration.StubBrowserDriver(slider=True)
    stub.goto("http://localhost/test_register.html")
    stub.fill_phone(PHONE)
    stub.click_get_code()
    check("bot: single-point teleport rejected",
          stub.drag_trajectory([(100, 100, 0.0)]) is False)
    uniform = [(x, 100, 0.1) for x in range(40, 361, 20)]
    check("bot: uniform straight line rejected",
          stub.drag_trajectory(uniform) is False,
          "reason=" + str(stub.last_drag_reason))
    check("bot: still pending after rejects",
          stub.slider_geometry() is not None)


def test_sms_timeout():
    stub = registration.StubBrowserDriver(slider=True)
    task = registration.RegistrationTask(
        site="local-test", start_url="http://localhost/test_register.html",
        phone=PHONE, dry_run=True, rng=random.Random(3))
    reg = registration.SmartRegistration(stub, dry_run=True,
                                         rng=random.Random(3))
    res = reg.run(task, lambda: None)
    check("timeout: status sms_timeout", res["status"] == "sms_timeout")
    check("timeout: not ok", res["ok"] is False)

    def boom():
        raise RuntimeError("provider down")

    stub2 = registration.StubBrowserDriver(slider=True)
    res2 = registration.SmartRegistration(stub2, dry_run=True,
                                          rng=random.Random(3)).run(task,
                                                                    boom)
    check("timeout: provider error -> sms_timeout",
          res2["status"] == "sms_timeout")


class FakeReal:
    """Stand-in for the RealBrowser backend (assumed interface)."""

    def __init__(self):
        self.calls = []
        self.url = None

    def goto(self, url):
        self.url = url

    def elements(self):
        return [
            {"id": 1, "tag": "input", "type": "tel", "text": "",
             "placeholder": "请输入手机号",
             "box": {"x": 40, "y": 100, "width": 200, "height": 32}},
            {"id": 2, "tag": "button", "type": "button",
             "text": "获取验证码",
             "box": {"x": 250, "y": 100, "width": 100, "height": 32}},
            {"id": 3, "tag": "div", "type": "", "text": "拖动滑块验证",
             "role": "slider",
             "box": {"x": 40, "y": 160, "width": 320, "height": 40}},
            {"id": 4, "tag": "input", "type": "text", "text": "",
             "placeholder": "请输入短信验证码",
             "box": {"x": 40, "y": 220, "width": 200, "height": 32}},
            {"id": 5, "tag": "button", "type": "button", "text": "注册",
             "box": {"x": 40, "y": 270, "width": 200, "height": 40}},
        ]

    def fill(self, el_id, text):
        # Record only (method, id) — never the text (phone/code).
        self.calls.append(("fill", el_id))

    def click(self, el_id):
        self.calls.append(("click", el_id))

    def mouse_drag(self, path):
        self.calls.append(("mouse_drag", len(path)))
        return True

    def read_result(self):
        return {"registered": True, "needs_real_name": False,
                "account": None, "message": "ok"}


def test_adapter_fake_real():
    fake = FakeReal()
    adapter = registration.RealBrowserAdapter(fake)
    task = registration.RegistrationTask(
        site="local-test", start_url="http://localhost/test_register.html",
        phone=PHONE, dry_run=True, rng=random.Random(11))
    res = registration.SmartRegistration(adapter, dry_run=True,
                                         rng=random.Random(11)).run(
                                             task, lambda: CODE)
    check("adapter: ok", res["ok"] is True)
    check("adapter: registered", res["status"] == "registered")
    check("adapter: account falls back to phone",
          res["account"] == PHONE)
    kinds = [c[0] for c in fake.calls]
    check("adapter: phone field filled", ("fill", 1) in fake.calls)
    check("adapter: get-code clicked", ("click", 2) in fake.calls)
    drags = [c[1] for c in fake.calls if c[0] == "mouse_drag"]
    check("adapter: drag played with >= 5 points",
          drags and drags[0] >= 5, "points=%s" % (drags,))
    check("adapter: code field filled", ("fill", 4) in fake.calls)
    check("adapter: submit clicked", ("click", 5) in fake.calls)
    check("adapter: code not in result", CODE not in json.dumps(res))
    check("adapter: call kinds sane",
          kinds.count("fill") == 2 and kinds.count("click") == 2)


def test_human_path_genuine():
    path = human.human_path(40, 100, 345, 100, rng=random.Random(7))
    check("human: >= 5 points", len(path) >= 5, "n=%d" % len(path))
    check("human: ends exactly on target", path[-1][:2] == (345, 100))
    delays = [p[2] for p in path]
    check("human: delays vary", len(set(delays)) >= 2)
    dev = max(abs(y - 100) for x, y, d in path)
    check("human: path wobbles off the straight line", dev > 0,
          "max_dev=%s" % (dev,))


def main():
    print("masked test phone: " + masked())
    test_happy_path()
    test_real_name()
    test_no_slider()
    test_bot_drag_rejected()
    test_sms_timeout()
    test_adapter_fake_real()
    test_human_path_genuine()
    print("----")
    if FAILURES:
        print("FAILED: %d (%s)" % (len(FAILURES), ", ".join(FAILURES)))
        return 1
    print("ALL TESTS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
