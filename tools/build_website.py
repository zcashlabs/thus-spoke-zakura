#!/usr/bin/env python3
"""Combine the static website and rendered book for deployment to Vercel."""

import pathlib
import shutil
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "site" / "deploy"


def main() -> int:
    book = ROOT / "site" / "public"
    if not (book / "index.html").is_file() or not (book / "book.md").is_file():
        print("run `mdbook build && python3 tools/build_site.py` first", file=sys.stderr)
        return 1

    if OUT.exists():
        shutil.rmtree(OUT)
    shutil.copytree(ROOT / "website", OUT / "public",
                    ignore=shutil.ignore_patterns(".*", "tests", "README.md", "vercel.json"))
    shutil.copytree(book, OUT / "public" / "book")
    shutil.copy2(ROOT / "website" / "vercel.json", OUT / "vercel.json")
    print("site/deploy: website at /, book at /book/")
    return 0


if __name__ == "__main__":
    sys.exit(main())
