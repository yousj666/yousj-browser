"""Visit history (persistent, JSONL).

Each visit records url, title and timestamp. File lives at
``~/.config/yousj/history.jsonl`` with mode 600.
"""
import json
import os
import time

_PATH = os.path.join(os.path.expanduser("~"), ".config", "yousj",
                     "history.jsonl")
_MAX_ENTRIES = 1000


def _ensure():
    d = os.path.dirname(_PATH)
    os.makedirs(d, exist_ok=True)


def record(url: str, title: str = "") -> None:
    """Append one visit. Keeps only the newest _MAX_ENTRIES entries."""
    _ensure()
    entry = {"url": url, "title": title or "",
             "ts": int(time.time())}
    try:
        with open(_PATH, "a", encoding="utf-8") as f:
            f.write(json.dumps(entry, ensure_ascii=False) + "\n")
        os.chmod(_PATH, 0o600)
    except OSError:
        return
    _trim()


def _trim():
    try:
        with open(_PATH, encoding="utf-8") as f:
            lines = f.readlines()
    except OSError:
        return
    if len(lines) > _MAX_ENTRIES * 2:
        try:
            with open(_PATH, "w", encoding="utf-8") as f:
                f.writelines(lines[-_MAX_ENTRIES:])
            os.chmod(_PATH, 0o600)
        except OSError:
            pass


def recent(n: int = 20):
    """Newest-first list of ``{"url", "title", "ts"}`` dicts."""
    try:
        with open(_PATH, encoding="utf-8") as f:
            lines = f.readlines()
    except OSError:
        return []
    out = []
    for line in lines[-n:]:
        try:
            out.append(json.loads(line))
        except ValueError:
            continue
    return out[::-1]


def clear() -> None:
    try:
        os.remove(_PATH)
    except OSError:
        pass
