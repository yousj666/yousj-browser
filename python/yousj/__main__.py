"""Yousj browser CLI.

usage:
  python -m yousj <url>                 fetch & summarize (v0.1 style)
  python -m yousj fetch <url>           fetch & summarize
  python -m yousj search <query...>     web search via our own engine
  python -m yousj devtools <url>        open F12 on a URL
  python -m yousj config list           show all settings
  python -m yousj config get <key>      show one setting
  python -m yousj config set <k> <v>    change a setting
  python -m yousj config engine [name|url]  switch search engine
"""
import sys

from . import devtools, fetch, search as _search, settings


def _cmd_fetch(url):
    doc = fetch(url)
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
    print("usage: config [list|get <k>|set <k> <v>|engine [name|url]]",
          file=sys.stderr)
    return 2


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
    if len(argv) == 1 and "://" in argv[0]:
        return _cmd_fetch(argv[0])  # v0.1 style: bare URL
    print("usage: python -m yousj <url>|fetch|search|devtools|config ...",
          file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
