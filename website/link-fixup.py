#!/usr/bin/env python3
"""mdBook preprocessor: rewrite the repo-relative links inside INCLUDED source
docs so they resolve within the book, without editing the source of truth.

The site pulls CLAUDE.md / COMMANDMENTS.md / docs/*.md / utilities/*.md verbatim via
{{#include}}. Those files link with paths relative to where they live in the repository
(`docs/ahci.md` from CLAUDE.md, `../docs/x.md` from milestones/ALMANAC.md, `stdlib-design.md`
from docs/stdlib-brief.md), which 404 in the rendered book. For each link this preprocessor
resolves the path against the INCLUDED file's own directory, then maps:

  - a repo file that IS a book page  -> that page (depth-correct relative URL)
  - a repo file or directory that is not a page -> GitHub (blob or tree)
  - anything that resolves to nothing in the repository -> left untouched

Which repo file is which page is READ from the include stubs under src/, never kept as a
list here: a page is a two-line stub whose include names its source, so adding a page adds
its mapping. (The list this replaced named five root docs and fifteen design stems, and every
other link 404'd: the contributing page alone had nine.)

It leaves ALONE anchors (#...), external URLs, links inside fenced code blocks, and the
links on hand-written pages (no include), which are book-relative and which mdBook's own
link handling resolves. Runs AFTER the built-in `links` preprocessor so {{#include}}
content is already expanded.

Configured in book.toml as [preprocessor.godspeed-links]. Set GODSPEED_REPO_URL to
change the GitHub base for non-page links (defaults to the current repo)."""
import json, sys, re, os

# mdBook asks `supports <renderer>` first; we support everything.
if len(sys.argv) > 1 and sys.argv[1] == "supports":
    sys.exit(0)

REPO = os.environ.get("GODSPEED_REPO_URL", "https://github.com/godspeedhq/godspeed-os").rstrip("/")
BLOB, TREE = REPO + "/blob/main", REPO + "/tree/main"

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = os.path.join(HERE, "src")
ROOT = os.path.dirname(HERE)
INCLUDE = re.compile(r"\{\{#include\s+([^}:\s]+)")
LINK = re.compile(r"\]\(([^)\s]+)\)")
FENCE = re.compile(r"^\s*(```|~~~)")


def repo_rel(path):
    return os.path.relpath(path, ROOT).replace(os.sep, "/")


def read_pages():
    """(repo file -> book page, book chapter -> the repo file it includes)."""
    page_of, source_of = {}, {}
    for dirpath, _, files in os.walk(SRC):
        for f in files:
            if not f.endswith(".md"):
                continue
            stub = os.path.join(dirpath, f)
            chapter = os.path.relpath(stub, SRC).replace(os.sep, "/")
            html = chapter[:-3] + ".html"
            page_of[repo_rel(stub)] = html              # website/src/x.md is page x.html
            with open(stub, encoding="utf-8") as fh:
                m = INCLUDE.search(fh.read())
            if m:
                source = repo_rel(os.path.normpath(os.path.join(dirpath, m.group(1))))
                page_of.setdefault(source, html)        # the first page to include a file wins
                source_of[chapter] = source
    return page_of, source_of


PAGE_OF, SOURCE_OF = read_pages()


def resolve(source, target):
    """The repo path a link in `source` means, or None. A link is relative to its file's
    directory, as GitHub reads it; one written from the repository root (`docs/x.md` inside
    docs/) is accepted too, because the old rewrite read them that way and some still are."""
    for base in (os.path.dirname(source), ""):
        path = os.path.normpath(os.path.join(ROOT, base, target))
        rel = repo_rel(path)
        if rel.startswith("..") or not os.path.exists(path):
            continue
        return rel + ("/" if os.path.isdir(path) else "")
    return None


def rewrite(target, prefix, source):
    if target.startswith(("#", "http://", "https://", "mailto:")):
        return None
    frag = ""
    if "#" in target:
        target, f = target.split("#", 1); frag = "#" + f
    if not target:
        return None
    rel = resolve(source, target)
    if rel is None:
        return None
    if rel.rstrip("/") in PAGE_OF:
        return prefix + PAGE_OF[rel.rstrip("/")] + frag
    if rel.endswith("/"):
        return TREE + "/" + rel.rstrip("/") + frag
    return BLOB + "/" + rel + frag


def process(md, chapter):
    source = SOURCE_OF.get(chapter)
    if source is None:
        return md                                       # hand-written: book-relative links
    prefix = "../" * chapter.count("/")                 # depth of this chapter in the book
    out, fenced = [], False
    for line in md.split("\n"):
        if FENCE.match(line):
            fenced = not fenced
        elif not fenced:
            line = LINK.sub(lambda m: "](" + (rewrite(m.group(1), prefix, source) or m.group(1)) + ")", line)
        out.append(line)
    return "\n".join(out)


def walk(items):
    for it in items:
        ch = it.get("Chapter")
        if not ch:
            continue
        path = (ch.get("source_path") or ch.get("path") or "").replace(os.sep, "/")
        ch["content"] = process(ch.get("content", ""), path)
        if ch.get("sub_items"):
            walk(ch["sub_items"])


# Read/write raw UTF-8 bytes: the docs carry emoji and box-drawing, and Windows
# text-mode stdio would otherwise mangle them into a lone surrogate mdBook rejects.
_context, book = json.loads(sys.stdin.buffer.read().decode("utf-8"))
walk(book["sections"])
sys.stdout.buffer.write(json.dumps(book, ensure_ascii=False).encode("utf-8"))
