#!/usr/bin/env python3
"""mdBook preprocessor: rewrite the repo-relative links inside INCLUDED source
docs so they resolve within the book, without editing the source of truth.

The site pulls CLAUDE.md / COMMANDMENTS.md / docs/*.md verbatim via {{#include}}.
Those files link to each other with repo paths (`./CLAUDE.md`, `docs/ahci.md`,
`examples/`) that 404 in the rendered book. This preprocessor maps:

  - a repo doc that IS a book page   -> the book page URL (depth-correct relative)
  - a repo file/dir that is NOT a page (README.md, examples/, sdk/, ...) -> GitHub

It deliberately leaves ALONE: anchors (#...), external URLs, image assets, and the
book's own lowercase chapter links (commandments.md, design/x.md) which mdBook's
built-in link handling already resolves. Runs AFTER the built-in `links`
preprocessor so {{#include}} content is already expanded.

Configured in book.toml as [preprocessor.godspeed-links]. Set GODSPEED_REPO_URL to
change the GitHub base for non-page links (defaults to the current repo)."""
import json, sys, re, os

# mdBook asks `supports <renderer>` first; we support everything.
if len(sys.argv) > 1 and sys.argv[1] == "supports":
    sys.exit(0)

REPO = os.environ.get("GODSPEED_REPO_URL", "https://github.com/godspeedhq/godspeed-os").rstrip("/")
BLOB, TREE = REPO + "/blob/main", REPO + "/tree/main"

# repo-doc basename -> book page (relative to book root, no leading slash)
PAGE = {
    "CLAUDE.md": "constitution.html",
    "COMMANDMENTS.md": "commandments.html",
    "GLOSSARY.md": "glossary.html",
    "CONTRIBUTING.md": "contributing.html",
    "ALMANAC.md": "almanac.html",
}
# docs/<stem>.md -> design page stem (for robustness; none exist as links today)
DESIGN = {
    "persistence": "persistence", "ahci": "ahci", "iommu": "iommu", "naming-design": "naming",
    "scripting": "scripting", "pipes": "pipes", "records": "records",
    "console-service": "console-service", "drives": "drives",
    "service-control-cap": "service-control", "introspection-capability": "introspection",
    "unsafe-audit": "unsafe-audit", "licensing": "licensing", "networking": "networking",
    "cluster-design": "cluster",
}
NONPAGE_FILES = {"README.md", "LICENSE", "NOTICE"}
KNOWN_DIRS = ("examples/", "sdk/", "contracts/", "milestones/", "tests/", "services/", "kernel/", "osdev/")

LINK = re.compile(r"\]\(([^)]+)\)")


def rewrite(target, prefix):
    if target.startswith(("#", "http://", "https://", "mailto:")):
        return None
    frag = ""
    if "#" in target:
        target, f = target.split("#", 1); frag = "#" + f
    core = re.sub(r"^(\.\.?/)+", "", target)          # strip leading ./ ../
    base = core.rsplit("/", 1)[-1]
    if base in PAGE:                                    # repo doc that is a book page
        return prefix + PAGE[base] + frag
    dm = re.search(r"docs/([A-Za-z0-9_-]+)\.md$", core)
    if dm and dm.group(1) in DESIGN:
        return prefix + "design/" + DESIGN[dm.group(1)] + ".html" + frag
    if base in NONPAGE_FILES:                           # repo file, no page -> GitHub
        return BLOB + "/" + core + frag
    if core.endswith("/"):                              # repo dir -> GitHub tree
        return TREE + "/" + core.rstrip("/") + frag
    if core.startswith(KNOWN_DIRS):                     # repo path -> GitHub blob
        return BLOB + "/" + core + frag
    return None                                         # leave everything else untouched


def process(md, path):
    prefix = "../" * (path.count("/") if path else 0)  # depth of this chapter in the book
    def sub(m):
        new = rewrite(m.group(1), prefix)
        return "](" + new + ")" if new is not None else m.group(0)
    return LINK.sub(sub, md)


def walk(items):
    for it in items:
        ch = it.get("Chapter")
        if not ch:
            continue
        ch["content"] = process(ch.get("content", ""), ch.get("path") or "")
        if ch.get("sub_items"):
            walk(ch["sub_items"])


# Read/write raw UTF-8 bytes: the docs carry emoji and box-drawing, and Windows
# text-mode stdio would otherwise mangle them into a lone surrogate mdBook rejects.
_context, book = json.loads(sys.stdin.buffer.read().decode("utf-8"))
walk(book["sections"])
sys.stdout.buffer.write(json.dumps(book, ensure_ascii=False).encode("utf-8"))
