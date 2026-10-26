"""Yousj browser CLI.

usage:
  python -m yousj <url>                 fetch & summarize (v0.1 style)
  python -m yousj fetch <url>           fetch & summarize
  python -m yousj search <query...>     web search via our own engine
  python -m yousj devtools <url>        open F12 on a URL
  python -m yousj download <url> [-o file] [--yes]  download a file (SSRF-checked)
                                                   # --yes: skip oversize confirm
  python -m yousj history [n]          show recent visit history
  python -m yousj elements <url>       list interactive elements (AI-readable)
  python -m yousj activate <url> <id>  activate element <id>, print JSON result
  python -m yousj layout <url> [--width N]  list layout boxes (pixels)
  python -m yousj mouse move <x> <y> [--dry-run]   move virtual mouse, print path
  python -m yousj mouse click [x y] [--dry-run]    click virtual mouse
  python -m yousj mouse drag <x0> <y0> <x1> <y1> [--dry-run]  drag virtual mouse
  python -m yousj screenshot <url> [-o file] [--svg] [--full-page] [--width N] [--height N]
                                         wireframe screenshot to a file
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


def _cmd_elements(url):
    try:
        doc = fetch(url)
    except security.SecurityError:
        token = _cli_risky_flow(url)
        if token is None:
            return 1
        doc = fetch(url, bypass_token=token)
    except Exception as e:  # noqa: BLE001
        print("elements failed: %s" % e, file=sys.stderr)
        return 1
    print("== %s ==" % (doc.title() or url))
    print(doc.format_elements())
    return 0


def _cmd_activate(argv):
    if len(argv) != 2:
        print("usage: activate <url> <id>", file=sys.stderr)
        return 2
    url, id_s = argv
    try:
        element_id = int(id_s)
    except ValueError:
        print("error: <id> must be a number", file=sys.stderr)
        return 2
    try:
        doc = fetch(url)
    except security.SecurityError:
        token = _cli_risky_flow(url)
        if token is None:
            return 1
        doc = fetch(url, bypass_token=token)
    except Exception as e:  # noqa: BLE001
        print("activate failed: %s" % e, file=sys.stderr)
        return 1
    import json
    print(json.dumps(doc.activate(element_id), ensure_ascii=False, indent=2))
    return 0


def _cmd_layout(argv):
    url = None
    width = 1280
    # Simple parse: url [--width N]
    args = []
    i = 0
    while i < len(argv):
        if argv[i] == "--width" and i + 1 < len(argv):
            try:
                width = int(argv[i + 1])
            except ValueError:
                print("error: --width needs a number", file=sys.stderr)
                return 2
            i += 2
        else:
            args.append(argv[i])
            i += 1
    if len(args) != 1:
        print("usage: layout <url> [--width N]", file=sys.stderr)
        return 2
    url = args[0]
    try:
        doc = fetch(url)
    except security.SecurityError:
        token = _cli_risky_flow(url)
        if token is None:
            return 1
        doc = fetch(url, bypass_token=token)
    except Exception as e:  # noqa: BLE001
        print("layout failed: %s" % e, file=sys.stderr)
        return 1
    info = doc.layout(viewport_width=width)
    print("viewport_width=%d truncated=%s boxes=%d" % (
        info["viewport_width"], info["truncated"], len(info["boxes"])))
    for b in info["boxes"]:
        print("<%s> (%d,%d) %dx%d '%s'" % (
            b["tag"], b["x"], b["y"], b["width"], b["height"],
            b["text"][:60]))
    return 0


def _cmd_mouse(argv):
    # NOTE: the CLI mouse is per-invocation (position starts at (0, 0)
    # every call). A real session holds one Mouse object in-process.
    from . import mouse as _mouse
    dry = "--dry-run" in argv
    argv = [a for a in argv if a != "--dry-run"]
    if not argv:
        print("usage: mouse move <x> <y>|click [x y]|drag <x0> <y0> <x1> <y1> [--dry-run]",
              file=sys.stderr)
        return 2
    m = _mouse.Mouse()
    import json
    try:
        if argv[0] == "move" and len(argv) == 3:
            path = m.move_to(int(argv[1]), int(argv[2]), dry_run=dry)
            print(json.dumps({"to": m.position(), "points": len(path),
                              "path": path}))
        elif argv[0] == "click" and len(argv) in (1, 3):
            if len(argv) == 3:
                res = m.click(int(argv[1]), int(argv[2]), dry_run=dry)
            else:
                res = m.click(dry_run=dry)
            print(json.dumps({"at": res["at"],
                              "path_points": len(res["path"]),
                              "path": res["path"]}))
        elif argv[0] == "drag" and len(argv) == 5:
            res = m.drag(int(argv[1]), int(argv[2]), int(argv[3]),
                         int(argv[4]), dry_run=dry)
            print(json.dumps({"from": res["from"], "to": res["to"],
                              "points": res["points"], "path": res["path"]}))
        else:
            print("usage: mouse move <x> <y>|click [x y]|drag <x0> <y0> <x1> <y1> [--dry-run]",
                  file=sys.stderr)
            return 2
    except ValueError:
        print("error: coordinates must be numbers", file=sys.stderr)
        return 2
    print("note: CLI mouse is per-invocation (starts at (0,0)); "
          "a session holds one Mouse in-process")
    return 0


def _cmd_screenshot(argv):
    svg = "--svg" in argv
    full = "--full-page" in argv
    argv = [a for a in argv if a not in ("--svg", "--full-page")]
    width, height = 1280, 800
    dest = None
    args = []
    i = 0
    while i < len(argv):
        if argv[i] == "-o" and i + 1 < len(argv):
            dest = argv[i + 1]
            i += 2
        elif argv[i] == "--width" and i + 1 < len(argv):
            try:
                width = int(argv[i + 1])
            except ValueError:
                print("error: --width needs a number", file=sys.stderr)
                return 2
            i += 2
        elif argv[i] == "--height" and i + 1 < len(argv):
            try:
                height = int(argv[i + 1])
            except ValueError:
                print("error: --height needs a number", file=sys.stderr)
                return 2
            i += 2
        else:
            args.append(argv[i])
            i += 1
    if len(args) != 1:
        print("usage: screenshot <url> [-o file] [--svg] [--full-page] [--width N] [--height N]",
              file=sys.stderr)
        return 2
    url = args[0]
    try:
        doc = fetch(url)
    except security.SecurityError:
        token = _cli_risky_flow(url)
        if token is None:
            return 1
        doc = fetch(url, bypass_token=token)
    except Exception as e:  # noqa: BLE001
        print("screenshot failed: %s" % e, file=sys.stderr)
        return 1
    fmt = "svg" if svg else "png"
    if dest is None:
        dest = "yousj-screenshot." + fmt
    info = doc.screenshot(path=dest, format=fmt, viewport_width=width,
                          viewport_height=height, full_page=full)
    print("saved: %s (%s %dx%d, %d boxes)" % (
        info["path"], info["format"], info["width"], info["height"],
        info["boxes_drawn"]))
    print("note: %s" % info["note"])
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
    if cmd == "elements" and len(argv) == 2:
        return _cmd_elements(argv[1])
    if cmd == "activate" and len(argv) == 3:
        return _cmd_activate(argv[1:])
    if cmd == "layout" and len(argv) >= 2:
        return _cmd_layout(argv[1:])
    if cmd == "mouse" and len(argv) >= 2:
        return _cmd_mouse(argv[1:])
    if cmd == "screenshot" and len(argv) >= 2:
        return _cmd_screenshot(argv[1:])
    if len(argv) == 1 and "://" in argv[0]:
        return _cmd_fetch(argv[0])  # v0.1 style: bare URL
    print("usage: python -m yousj <url>|fetch|search|devtools|download|history|config|elements|activate|layout|mouse|screenshot ...",
          file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
