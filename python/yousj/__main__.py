"""Yousj browser CLI.

usage:
  python -m yousj <url>                 fetch & summarize (v0.1 style)
  python -m yousj fetch <url>           fetch & summarize
  python -m yousj search <query...>     web search via our own engine
  python -m yousj devtools <url>        open F12 on a URL
  python -m yousj download <url> [-o file] [--yes]  download a file (SSRF-checked)
                                                   # --yes: skip oversize confirm
  python -m yousj history [n]          show recent visit history
  python -m yousj config list           show all settings
  python -m yousj config get <key>      show one setting
  python -m yousj config set <k> <v>    change a setting
  python -m yousj config engine [name|url]  switch search engine
  python -m yousj config proxy [url|off]    set HTTP(S) proxy
  python -m yousj config max-download-size [MB]  download size cap (default 200)
"""
import os
import sys
import urllib.parse

from . import devtools, fetch, history as _history, net as _net
from . import search as _search, security, settings


def _cli_risky_flow(url):
    """Interactive two-layer risk confirmation. Returns a bypass token,
    or None when the user backs out / stdin isn't interactive."""
    p1 = security.warn_first(url)
    if p1.get("allowed"):
        return None
    print("⚠ " + p1["text"])
    for i, c in enumerate(p1["choices"], 1):
        print("  %d. %s" % (i, c))
    if not sys.stdin.isatty():
        print("(非交互模式：拒绝访问)", file=sys.stderr)
        return None
    if input("选择 [1/%d]: " % len(p1["choices"])).strip() != "2":
        print("已退出，不访问。")
        return None
    p2 = security.warn_second(url)
    print("⚠ " + p2["text"])
    for i, c in enumerate(p2["choices"], 1):
        print("  %d. %s" % (i, c))
    if input("选择 [1/%d]: " % len(p2["choices"])).strip() != "2":
        print("已退出，不访问。")
        return None
    return security.confirm_visit(url)


def _cmd_fetch(url):
    try:
        doc = fetch(url)
    except security.SecurityError:
        token = _cli_risky_flow(url)
        if token is None:
            return 1
        doc = fetch(url, bypass_token=token)
    print("== title ==")
    print(doc.title())
    print("== text (first 2000 chars) ==")
    print(doc.text()[:2000])
    print("== links (first 20) ==")
    for link in doc.links()[:20]:
        print(" -", link)
    return 0


def _cmd_search(query):
    se = settings.get_search_engine()
    print("search engine: %s" % se["name"])
    try:
        results = _search(query)
    except Exception as e:  # noqa: BLE001
        print("search failed: %s" % e, file=sys.stderr)
        return 1
    if not results:
        print("(no results parsed)")
        return 0
    for i, r in enumerate(results, 1):
        print("%d. %s" % (i, r["title"]))
        print("   %s" % r["url"])
    return 0


def _cmd_devtools(url):
    try:
        session = devtools.inspect(url)
    except security.SecurityError:
        token = _cli_risky_flow(url)
        if token is None:
            return 1
        session = devtools.inspect(url, bypass_token=token)
    except Exception as e:  # noqa: BLE001
        print("devtools failed: %s" % e, file=sys.stderr)
        return 1
    print(session.report())
    return 0


def _cmd_config(argv):
    if not argv or argv[0] == "list":
        for k, v in settings.all_settings().items():
            print("%s = %s" % (k, v))
        print("presets: %s" % ", ".join(settings.list_search_engines()))
        return 0
    if argv[0] == "proxy":
        if len(argv) == 1:
            print(settings.get_proxy() or "(direct)")
        elif argv[1] == "off":
            settings.clear_proxy()
            print("proxy cleared (direct connection)")
        else:
            try:
                settings.set_proxy(argv[1])
            except ValueError as e:
                print("error: %s" % e, file=sys.stderr)
                return 1
            print("proxy = %s" % settings.get_proxy())
        return 0
    if argv[0] == "get" and len(argv) == 2:
        print(settings.get(argv[1]))
        return 0
    if argv[0] == "set" and len(argv) == 3:
        settings.set(argv[1], argv[2])
        print("ok: %s = %s" % (argv[1], argv[2]))
        return 0
    if argv[0] == "engine":
        if len(argv) == 1:
            se = settings.get_search_engine()
            print("%s -> %s" % (se["name"], se["url"]))
        else:
            try:
                se = settings.set_search_engine(argv[1])
            except ValueError as e:
                print("error: %s" % e, file=sys.stderr)
                return 1
            print("search engine: %s -> %s" % (se["name"], se["url"]))
        return 0
    if argv[0] == "max-download-size":
        if len(argv) == 1:
            print("%s MB" % settings.get("max_download_size_mb", 200))
        else:
            try:
                mb = float(argv[1])
            except ValueError:
                print("error: MB must be a number", file=sys.stderr)
                return 1
            if mb <= 0:
                print("error: MB must be positive", file=sys.stderr)
                return 1
            settings.set("max_download_size_mb", mb)
            print("max-download-size = %s MB" % mb)
        return 0
    print("usage: config [list|get <k>|set <k> <v>|engine [name|url]|proxy [url|off]|max-download-size [MB]]",
          file=sys.stderr)
    return 2


def _cmd_download(argv):
    if not argv:
        print("usage: download <url> [-o file] [--yes]", file=sys.stderr)
        return 2
    yes = "--yes" in argv
    argv = [a for a in argv if a != "--yes"]
    url = argv[0]
    dest = None
    if "-o" in argv:
        i = argv.index("-o")
        if i + 1 >= len(argv):
            print("error: -o needs a filename", file=sys.stderr)
            return 2
        dest = argv[i + 1]
    if dest is None:
        name = urllib.parse.urlsplit(url).path.rsplit("/", 1)[-1] or "index.html"
        dest = urllib.parse.unquote(name) or "index.html"
    dest = os.path.expanduser(dest)
    # --yes auto-confirms oversize downloads (non-interactive otherwise).
    on_oversize = (lambda *a: True) if yes else None
    try:
        def progress(done, total):
            if total:
                pct = done * 100 // total
                print("\r%d/%d bytes (%d%%)" % (done, total, pct), end="",
                      flush=True)
            else:
                print("\r%d bytes" % done, end="", flush=True)
        info = _net.download(url, dest, progress=progress,
                             on_oversize=on_oversize)
        print("\nsaved: %s (%d bytes)" % (info["path"], info["bytes"]))
    except security.SecurityError:
        token = _cli_risky_flow(url)
        if token is None:
            return 1
        info = _net.download(url, dest, bypass_token=token,
                             on_oversize=on_oversize)
        print("saved: %s (%d bytes)" % (info["path"], info["bytes"]))
    except _net.OversizeDownloadError as e:
        print("\ndownload cancelled: %s" % e, file=sys.stderr)
        return 1
    except Exception as e:  # noqa: BLE001
        print("\ndownload failed: %s" % e, file=sys.stderr)
        return 1
    return 0


def _cmd_history(argv):
    n = 20
    if argv:
        try:
            n = max(1, int(argv[0]))
        except ValueError:
            print("usage: history [n]", file=sys.stderr)
            return 2
    items = _history.recent(n)
    if not items:
        print("(no history yet)")
        return 0
    import datetime
    for it in items:
        ts = datetime.datetime.fromtimestamp(it["ts"]).strftime("%m-%d %H:%M")
        title = it["title"] or "(no title)"
        print("%s  %s\n    %s" % (ts, title, it["url"]))
    return 0


def main(argv=None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if not argv:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    cmd = argv[0]
    if cmd == "fetch" and len(argv) == 2:
        return _cmd_fetch(argv[1])
    if cmd == "search" and len(argv) >= 2:
        return _cmd_search(" ".join(argv[1:]))
    if cmd == "devtools" and len(argv) == 2:
        return _cmd_devtools(argv[1])
    if cmd == "config":
        return _cmd_config(argv[1:])
    if cmd == "download":
        return _cmd_download(argv[1:])
    if cmd == "history":
        return _cmd_history(argv[1:])
    if len(argv) == 1 and "://" in argv[0]:
        return _cmd_fetch(argv[0])  # v0.1 style: bare URL
    print("usage: python -m yousj <url>|fetch|search|devtools|download|history|config ...",
          file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
