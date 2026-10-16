#!/usr/bin/env python3
"""mdBook preprocessor (book.toml): points links that leave docs/ at GitHub.

The pages under docs/ link to files elsewhere in the repository
(`../ci/e2e-csi.sh`, `../deploy/csi`), which resolve on GitHub but not in
the book, whose site holds only docs/. This rewrites each such relative
inline link to the file on the main branch, keeping its #anchor, and leaves
links within docs/, absolute URLs and fenced code alone.

mdBook runs it as `python3 ci/book-links.py` with the context and the book
as JSON on stdin, and with `supports <renderer>` first to ask whether it
handles a renderer (it handles all).
"""

import json
import posixpath
import re
import sys

BLOB = "https://github.com/mcurcio/agentfs/blob/main/"
SRC = "docs"
LINK = re.compile(r"\]\(([^)\s]+)")


def rewrite(target, page_dir):
    if "://" in target or target.startswith(("#", "mailto:", "/", "<")):
        return target
    path, hash_, anchor = target.partition("#")
    resolved = posixpath.normpath(posixpath.join(page_dir, path))
    if resolved == SRC + "/README.md":
        # The book's front page: mdBook renders docs/README.md as index.html.
        return posixpath.relpath(SRC + "/index.md", page_dir) + hash_ + anchor
    if resolved == SRC or resolved.startswith(SRC + "/") or resolved.startswith(".."):
        return target
    return BLOB + resolved + hash_ + anchor


def content(text, page):
    page_dir = posixpath.join(SRC, posixpath.dirname(page))
    out, fence = [], False
    for line in text.split("\n"):
        if line.lstrip().startswith(("```", "~~~")):
            fence = not fence
        elif not fence:
            line = LINK.sub(lambda m: "](" + rewrite(m.group(1), page_dir), line)
        out.append(line)
    return "\n".join(out)


def walk(items):
    for item in items:
        chapter = item.get("Chapter") if isinstance(item, dict) else None
        if not chapter:
            continue
        if chapter.get("path"):
            chapter["content"] = content(chapter["content"], chapter["path"])
        walk(chapter.get("sub_items", []))


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "supports":
        sys.exit(0)
    _context, book = json.load(sys.stdin)
    walk(book["items"])
    json.dump(book, sys.stdout)


if __name__ == "__main__":
    main()
