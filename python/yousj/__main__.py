"""Yousj browser CLI: python -m yousj <url>"""
import sys

from . import fetch


def main(argv=None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if not argv:
        print("usage: python -m yousj <url>", file=sys.stderr)
        return 2
    doc = fetch(argv[0])
    print("== title ==")
    print(doc.title())
    print("== text (first 2000 chars) ==")
    print(doc.text()[:2000])
    print("== links (first 20) ==")
    for link in doc.links()[:20]:
        print(" -", link)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
