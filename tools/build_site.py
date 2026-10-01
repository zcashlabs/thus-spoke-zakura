#!/usr/bin/env python3
"""Assembles the publishable site from mdBook's two renderers.

    mdbook build && python3 tools/build_site.py

mdBook writes HTML to site/html and markdown to site/markdown. This lays both
out as one tree in site/public, so every page is reachable as HTML for people
and as markdown for anything that would otherwise have to scrape the HTML:

    getting-started.html   the page
    getting-started.md     the same page, no navigation or theme chrome
    book.md                every chapter concatenated, one fetch, ~21k tokens
    llms.txt               an index of the above, per https://llmstxt.org/

Output is deterministic: no timestamps, so a rebuild that changes nothing
produces no commit.
"""

import pathlib
import re
import shutil
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
OUT = SITE / "public"
BASE = "https://amiabix.github.io/thus-spoke-zakura-book"

TITLE = "Thus Spoke Zakura"
SUMMARY = (
    "A disposable Zcash Regtest network for local development: a launcher, a "
    "node, lightwalletd, and a wallet-backed dashboard, all in Docker on "
    "loopback."
)

# One line per page, in the order SUMMARY.md lists them. Kept here rather than
# derived from the prose because a first sentence makes a poor description.
PAGES = {
    "index": ("Introduction",
              "What the environment is, who owns which fact, and how to read the book"),
    "getting-started": ("1. Your first instance",
                        "Start an instance, read its endpoints, and throw it away"),
    "architecture/overview": ("2. Components and state",
                              "Three containers, four volumes, and which one owns each answer"),
    "accounts-and-balances": ("3. Accounts and balances",
                              "Transparent and Orchard pools, and where faucet money comes from"),
    "using-the-dashboard": ("4. Fund and send",
                            "Fund an account and send between pools, from the dashboard or the CLI"),
    "mining-and-sync": ("5. Mining and wallet sync",
                        "Mine a block and follow it from the node into the wallet snapshot"),
    "architecture/operations": ("6. Uncertain operations",
                                "Idempotency keys, what an HTTP response proves, and what a lost one does not"),
    "connect-an-app": ("7. Connect your application",
                       "Choose an interface by who owns the data, then read it from your own code"),
    "cli": ("Appendix A: the ths CLI", "Every command, with its flags and an example"),
    "development": ("Appendix B: develop from source",
                    "Build the images from source and run the checks"),
    "troubleshooting": ("Appendix C: troubleshooting",
                        "Work down the layers, from Docker to an uncertain payment"),
}

REFERENCE = {"cli", "development", "troubleshooting"}


def summary_order() -> list[str]:
    """Page slugs in the order book/SUMMARY.md lists them."""
    text = (ROOT / "book" / "SUMMARY.md").read_text()
    slugs = []
    for target in re.findall(r"\]\(([^)]+\.md)\)", text):
        slug = target[:-3]
        slugs.append("index" if slug == "README" else slug)
    return slugs


def main() -> int:
    html, markdown = SITE / "html", SITE / "markdown"
    if not html.is_dir() or not markdown.is_dir():
        print("run `mdbook build` first: site/html and site/markdown are missing",
              file=sys.stderr)
        return 1

    if OUT.exists():
        shutil.rmtree(OUT)
    shutil.copytree(html, OUT)

    # The markdown twin of each page, beside its HTML.
    for page in markdown.rglob("*.md"):
        destination = OUT / page.relative_to(markdown)
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(page, destination)

    order = summary_order()
    missing = [slug for slug in order if slug not in PAGES]
    if missing:
        print(f"tools/build_site.py has no description for: {missing}", file=sys.stderr)
        return 1

    # One file with the whole book. Relative links between chapters keep
    # working, because the markdown twins sit at those same paths.
    header = (f"# {TITLE}\n\n> {SUMMARY}\n\n"
              f"Every chapter, in reading order. The pages are also published "
              f"separately; see {BASE}/llms.txt")
    # Pages nested under architecture/ link out with "../". In one flat file
    # served from the root that resolves above the site, so strip the hop.
    chapters_md = [re.sub(r"\]\(\.\./", "](", (markdown / f"{slug}.md").read_text().strip())
                   for slug in order]
    (OUT / "book.md").write_text("\n\n---\n\n".join([header] + chapters_md) + "\n")

    chapters = [slug for slug in order if slug not in REFERENCE]
    reference = [slug for slug in order if slug in REFERENCE]
    lines = [f"# {TITLE}\n", f"> {SUMMARY}\n",
             f"- [The whole book in one file]({BASE}/book.md): every chapter "
             f"concatenated as markdown\n", "## Chapters\n"]
    lines += [f"- [{PAGES[s][0]}]({BASE}/{s}.md): {PAGES[s][1]}" for s in chapters]
    lines += ["\n## Reference\n"]
    lines += [f"- [{PAGES[s][0]}]({BASE}/{s}.md): {PAGES[s][1]}" for s in reference]
    (OUT / "llms.txt").write_text("\n".join(lines) + "\n")

    pages = len(list(OUT.rglob("*.md")))
    size = (OUT / "book.md").stat().st_size
    print(f"site/public: {pages} markdown pages, book.md {size:,} bytes "
          f"(~{size // 4:,} tokens), llms.txt")
    return 0


if __name__ == "__main__":
    sys.exit(main())
