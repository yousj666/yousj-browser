"""BYOK AI for the Yousj browser (written by Muse, not the subagent).

The browser ships no built-in model. The user configures their own AI
in settings: API type, base URL, API key, model name. The browser AI
then runs on that model and obeys instructions from an external AI.

Config lives in ``~/.config/yousj/ai.json``, written with mode 0600.
The key is never printed: ``get_ai_config()`` masks it; only ``chat()``
sends it (as an Authorization / x-api-key header, never in logs).
"""

import json
import os
import urllib.error
import urllib.request

API_TYPES = ("openai", "anthropic")


def _config_path():
    return os.path.join(os.path.expanduser("~"), ".config", "yousj",
                        "ai.json")


def _load():
    try:
        with open(_config_path(), encoding="utf-8") as f:
            data = json.load(f)
            return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def _save(data):
    path = _config_path()
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=2)
    os.replace(tmp, path)
    os.chmod(path, 0o600)


def _mask(key):
    if not key:
        return None
    if len(key) <= 8:
        return "****"
    return key[:4] + "****" + key[-4:]


def set_ai_config(api_type="openai", base_url=None, model=None, api_key=None):
    """Save the user's own AI config (BYOK).

    api_type: "openai" (OpenAI-compatible chat completions) or
    "anthropic" (Messages API). base_url: API base, e.g.
    "https://api.example.com/v1". Returns the masked config.
    """
    api_type = (api_type or "openai").strip().lower()
    if api_type not in API_TYPES:
        raise ValueError("api_type must be one of %s" % (API_TYPES,))
    base_url = (base_url or "").strip().rstrip("/")
    model = (model or "").strip()
    api_key = (api_key or "").strip()
    if not base_url.startswith(("http://", "https://")):
        raise ValueError("base_url must start with http:// or https://")
    if not model:
        raise ValueError("model must not be empty")
    if not api_key:
        raise ValueError("api_key must not be empty")
    _save({"api_type": api_type, "base_url": base_url, "model": model,
           "api_key": api_key})
    return get_ai_config()


def get_ai_config():
    """Masked config for display; the key is never returned in full."""
    data = _load()
    if not data:
        return {}
    return {
        "api_type": data.get("api_type", "openai"),
        "base_url": data.get("base_url"),
        "model": data.get("model"),
        "api_key": _mask(data.get("api_key")),
        "configured": is_configured(),
    }


def is_configured():
    data = _load()
    return bool(data.get("base_url") and data.get("model")
                and data.get("api_key"))


def clear_ai_config():
    """Remove the saved AI config entirely."""
    try:
        os.remove(_config_path())
    except OSError:
        pass


def _get_key():
    # Internal only: the raw key goes to the API header, never to output.
    return _load().get("api_key")


def _post_json(url, headers, payload, timeout=60):
    body = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        url, data=body, method="POST",
        headers=dict(headers, **{"Content-Type": "application/json"}))
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", "replace")[:300]
        raise RuntimeError("AI request failed (HTTP %s): %s" % (e.code, detail))
    except urllib.error.URLError as e:
        raise RuntimeError("AI request failed: %s" % (e.reason,))


def _openai_url(base):
    base = base.rstrip("/")
    if base.endswith("/chat/completions"):
        return base
    if base.endswith("/v1"):
        return base + "/chat/completions"
    return base + "/v1/chat/completions"


def _anthropic_url(base):
    base = base.rstrip("/")
    if base.endswith("/messages"):
        return base
    if base.endswith("/v1"):
        return base + "/messages"
    return base + "/v1/messages"


def chat(messages, model=None, max_tokens=1024):
    """Call the user's configured model; return the reply text."""
    data = _load()
    if not is_configured():
        raise RuntimeError(
            "AI not configured: call set_ai_config(api_type, base_url, "
            "model, api_key) first")
    api_type = data.get("api_type", "openai")
    base = data["base_url"]
    mdl = model or data["model"]
    key = data["api_key"]
    if api_type == "openai":
        out = _post_json(_openai_url(base),
                         {"Authorization": "Bearer " + key},
                         {"model": mdl, "messages": messages})
        try:
            return out["choices"][0]["message"]["content"]
        except (KeyError, IndexError, TypeError):
            raise RuntimeError("unexpected OpenAI response: %r"
                               % (str(out)[:300],))
    if api_type == "anthropic":
        system = None
        msgs = []
        for m in messages:
            if m.get("role") == "system" and system is None:
                system = m.get("content")
            else:
                msgs.append(m)
        payload = {"model": mdl, "max_tokens": max_tokens, "messages": msgs}
        if system:
            payload["system"] = system
        out = _post_json(_anthropic_url(base),
                         {"x-api-key": key,
                          "anthropic-version": "2023-06-01"},
                         payload)
        parts = [b.get("text", "") for b in out.get("content", [])
                 if isinstance(b, dict) and b.get("type") == "text"]
        return "".join(parts)
    raise RuntimeError("unknown api_type %r" % (api_type,))


_SYSTEM = (
    "You are the AI inside the Yousj browser. An external AI gives you a "
    "task; you obey and carry it out with the browser's abilities. "
    "Available actions: goto(url), list_elements, activate(id), "
    "mouse_move(x, y), mouse_click(x, y), mouse_drag(x1, y1, x2, y2), "
    "fill(id, text). Reply with a JSON list of steps, each step an object "
    "with an 'action' key plus its arguments, e.g. "
    '[{"action": "goto", "url": "https://example.com"}, '
    '{"action": "activate", "id": 3}]. No other text.'
)


class BrowserAI:
    """Browser AI powered by the user's configured model.

    ``plan(task)`` asks the model for steps. ``run(task)`` plans and then
    executes each step through ``yousj.interact`` when that module exposes
    an ``execute(step)`` entry; until then it returns the plan plus a note
    that execution wiring is pending (it never pretends to have acted).
    """

    def __init__(self):
        if not is_configured():
            raise RuntimeError(
                "AI not configured: call set_ai_config(api_type, base_url, "
                "model, api_key) first")

    def plan(self, task, page=None):
        user = task
        if page:
            user += "\n\nCurrent page:\n" + str(page)[:4000]
        text = chat([{"role": "system", "content": _SYSTEM},
                     {"role": "user", "content": user}])
        cleaned = text.strip()
        for fence in ("```json", "```"):
            if cleaned.startswith(fence):
                cleaned = cleaned[len(fence):].strip()
            if cleaned.endswith("```"):
                cleaned = cleaned[:-3].strip()
        try:
            steps = json.loads(cleaned)
            if isinstance(steps, list):
                return steps
        except ValueError:
            pass
        return [{"action": "unparsed", "text": text}]

    def run(self, task, page=None):
        steps = self.plan(task, page)
        results = []
        try:
            from yousj import interact
            execute = getattr(interact, "execute", None)
        except Exception as e:  # interact module not ready yet
            execute = None
            results.append({"note": "interact not ready: %s" % (e,)})
        if execute is None:
            results.append({"note": "execution wiring pending: interact."
                           "execute not available yet"})
        else:
            for step in steps:
                try:
                    results.append(execute(step))
                except Exception as e:
                    results.append({"step": step, "error": str(e)})
                    break
        return {"plan": steps, "results": results}
