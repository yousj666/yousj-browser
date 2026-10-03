"""HTTP fetching for the Yousj browser (Python layer, stdlib only)."""
import gzip
import urllib.request


UA = "YousjBrowser/0.1 (headless; for AI agents)"


def get(url: str, timeout: int = 15) -> str:
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        raw = r.read()
        if r.headers.get("Content-Encoding") == "gzip":
            raw = gzip.decompress(raw)
        charset = r.headers.get_content_charset() or "utf-8"
        try:
            return raw.decode(charset, errors="replace")
        except (LookupError, ValueError):
            return raw.decode("utf-8", errors="replace")
