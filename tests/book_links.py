#!/usr/bin/env python3
"""Fails if any link or image in the book points at something that is not there.

    python3 tests/book_links.py

mdBook does not check relative links, so a renamed chapter or a moved figure
builds cleanly and only breaks for the reader. This checks every markdown link
target, every heading anchor, and every image path under book/.
"""

import pathlib
import re
import sys

BOOK = pathlib.Path(__file__).resolve().parent.parent / "book"

LINK = re.compile(r"(?<!!)\[[^\]]*\]\(([^)]+)\)")
IMAGE = re.compile(r"!\[[^\]]*\]\(([^)]+)\)")
HEADING = re.compile(r"^#{1,6}\s+(.*)$", re.MULTILINE)


def anchors(path: pathlib.Path) -> set[str]:
    """Heading ids the way mdBook derives them: lowercase, punctuation dropped."""
    out = set()
    for heading in HEADING.findall(path.read_text()):
        text = re.sub(r"`", "", heading.strip().lower())
        text = re.sub(r"[^\w\s-]", "", text)
        out.add(re.sub(r"\s+", "-", text).strip("-"))
    return out


def main() -> int:
    problems = []
    for page in sorted(BOOK.rglob("*.md")):
        body = page.read_text()
        for target in IMAGE.findall(body):
            if not (page.parent / target).exists():
                problems.append(f"{page.relative_to(BOOK)}: missing image {target}")
        for target in LINK.findall(body):
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            path, _, fragment = target.partition("#")
            resolved = (page.parent / path).resolve() if path else page
            if not resolved.exists():
                problems.append(f"{page.relative_to(BOOK)}: missing file {target}")
            elif fragment and fragment not in anchors(resolved):
                problems.append(f"{page.relative_to(BOOK)}: missing anchor {target}")

    for problem in problems:
        print(problem, file=sys.stderr)
    print(f"{'FAIL' if problems else 'ok'}: {len(problems)} broken links in the book")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
