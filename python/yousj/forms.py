"""HTML form filling & submission (Python layer).

Simple, AI-friendly API::

    from yousj import fetch
    doc = fetch("https://example.com/login")
    form = doc.forms()[0]
    form.fill({"user": "alice", "pass": "s3cret"})
    result = form.submit()          # -> Document (GET or POST per method)

Form field data comes from the Rust engine (``yousj_forms`` FFI as JSON),
so it sees the live DOM including JS-added fields.
"""
import urllib.parse

from . import engine as _engine
from . import history as _history
from . import net as _net


# Input types whose value is submitted as-is.
_TEXTISH = {
    "text", "password", "hidden", "search", "email", "url", "tel",
    "number", "date", "time", "month", "week", "color", "range",
}


class Form:
    """One <form> element. Fill fields with :meth:`fill`, send with
    :meth:`submit`."""

    def __init__(self, doc, spec: dict):
        self._doc = doc
        self.action = spec.get("action", "")
        self.method = (spec.get("method") or "get").lower()
        # name -> list of field dicts (radio groups share a name)
        self._fields = {}
        for f in spec.get("fields", []):
            if f.get("name"):
                self._fields.setdefault(f["name"], []).append(dict(f))

    def field_names(self):
        """All submittable field names."""
        return list(self._fields)

    def fill(self, values: dict) -> "Form":
        """Set field values by name. Returns self (chainable).

        - text/hidden/textarea/select: the value string
        - checkbox: True/False (or the value string to check it)
        - radio: the value string of the option to select
        - unknown field name -> KeyError; bad select value -> ValueError
        """
        for name, val in values.items():
            if name not in self._fields:
                raise KeyError("no such field: %r (have: %s)"
                               % (name, ", ".join(self._fields)))
            for f in self._fields[name]:
                tag, typ = f["tag"], f.get("type", "text")
                if tag == "select":
                    options = [o["value"] for o in f.get("options", [])]
                    if val not in options:
                        raise ValueError(
                            "bad value %r for select %r (options: %s)"
                            % (val, name, options))
                    for o in f["options"]:
                        o["selected"] = (o["value"] == val)
                elif typ in ("checkbox", "radio"):
                    if isinstance(val, bool):
                        f["checked"] = val
                    else:
                        # a specific value: check the matching option
                        f["checked"] = (f.get("value", "on") == val)
                elif tag in ("input", "textarea", "button"):
                    f["value"] = str(val)
        return self

    def _data(self) -> dict:
        """Collect name -> value pairs per HTML form submission rules."""
        data = {}
        for name, group in self._fields.items():
            vals = []
            for f in group:
                tag, typ = f["tag"], f.get("type", "text")
                if tag == "select":
                    opts = f.get("options", [])
                    sel = next((o["value"] for o in opts if o["selected"]),
                               opts[0]["value"] if opts else None)
                    if sel is not None:
                        vals.append(sel)
                elif typ in ("checkbox", "radio"):
                    if f.get("checked"):
                        vals.append(f.get("value", "on"))
                elif typ in ("button", "reset", "file"):
                    continue  # never submitted (file upload: not supported)
                elif tag == "textarea":
                    vals.append(f.get("value", ""))
                else:
                    # text-ish inputs + submit/image buttons with a name
                    # (treated as the clicked button, like browsers do)
                    vals.append(f.get("value", ""))
            if not vals:
                continue
            data[name] = vals[0] if len(vals) == 1 else vals
        return data

    def _target_url(self) -> str:
        base = getattr(self._doc, "url", None)
        action = (self.action or "").strip()
        if not action:
            if not base:
                raise RuntimeError(
                    "form has no action and the document has no URL; "
                    "set doc.url first")
            return base
        if urllib.parse.urlsplit(action).netloc:
            return action
        if not base:
            raise RuntimeError(
                "form action %r is relative but the document has no URL; "
                "set doc.url first" % action)
        return urllib.parse.urljoin(base, action)

    def submit(self, timeout: int = 15):
        """Submit the form (GET or POST per its method). Returns a new
        parsed :class:`Document` with ``.url`` set to the final URL."""
        url = self._target_url()
        data = self._data()
        if self.method == "post":
            final, _, headers, raw = _net._request(
                "POST", url, data=urllib.parse.urlencode(data).encode(),
                headers={"Content-Type":
                         "application/x-www-form-urlencoded"},
                timeout=timeout)
            body = _net._decode(raw, headers)
        else:
            qs = urllib.parse.urlencode(data)
            sep = "&" if urllib.parse.urlsplit(url).query else "?"
            final, _, headers, raw = _net._request(
                "GET", url + sep + qs if qs else url, timeout=timeout)
            body = _net._decode(raw, headers)
        doc = _engine.parse(body)
        doc.url = final
        _history.record(final, doc.title())
        return doc

    def __repr__(self):
        return "<Form method=%s action=%r fields=%s>" % (
            self.method, self.action, ", ".join(self._fields))


def forms(doc) -> list:
    """All forms in a document as :class:`Form` objects."""
    import json
    raw = _engine._take_str(_engine._lib.yousj_forms(doc._ptr))
    specs = json.loads(raw) if raw else []
    return [Form(doc, s) for s in specs]
