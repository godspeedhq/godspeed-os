#!/usr/bin/env python3
"""mdBook preprocessor: link acronyms to the glossary and give them a hover tooltip.

The docs are dense with hardware and systems abbreviations (IOMMU, xHCI, TSC, HHDM, ...).
A reader meeting one mid-sentence should not have to leave the page to find out what it
means. This wraps the FIRST occurrence of each glossary acronym on a page as:

    <a href="../glossary.html#gl-iommu"><abbr title="IOMMU - I/O Memory Management Unit ...">IOMMU</abbr></a>

so hovering shows the definition and clicking goes to the full entry.

Why a preprocessor rather than editing the docs: the source of truth (GLOSSARY.md,
docs/*.md, CLAUDE.md) stays plain, readable markdown - it is read in the repo and in a
terminal, not only in a browser - and the vocabulary lives in exactly ONE place. Adding an
entry to GLOSSARY.md automatically makes it hoverable everywhere; nothing can drift out of
sync, because the term list IS the glossary (Commandment III - one truth, derived views).

Scope and safety:
  - Terms come ONLY from the "Hardware and Systems Abbreviations" section of GLOSSARY.md.
    The rest of that file defines single letters (test categories P, F, S, ...) which must
    never be auto-linked.
  - FIRST occurrence per page only. Linking every "DMA" in a driver doc would be noise.
  - Never rewrites inside code fences, inline code, HTML tags, existing links, or headings
    (a heading rewrite would break its anchor).
  - Skips the glossary page itself, where it instead injects the `#gl-*` anchors that these
    links point at.

Configured in book.toml as [preprocessor.godspeed-abbr]; runs after the include/link passes
so {{#include}} content is already expanded.
"""
import json, re, sys, os

if len(sys.argv) > 1 and sys.argv[1] == "supports":
    sys.exit(0)

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GLOSSARY = os.path.join(ROOT, "GLOSSARY.md")
SECTION = "Hardware and Systems Abbreviations"

# Spans we must never rewrite inside: fenced code, inline code, HTML tags, markdown links,
# and ATX headings. Captured so re.split hands them back untouched.
# Only the fenced-code alternatives may span lines. The tag and link forms are deliberately
# newline-free: with re.S a `<[^>]+>` would treat a stray "<" in prose (`< 1 ms`, `for <var> in
# <words>`) as the start of a tag and swallow everything up to the next ">" - hundreds of lines,
# hiding the very text the acronyms live in.
PROTECT = re.compile(
    r"(```.*?```|~~~.*?~~~|`[^`\n]*`|<[^>\n]+>|\[[^\]\n]*\]\([^)\n]*\)|^[ \t]{0,3}#{1,6}[ \t][^\n]*$)",
    re.S | re.M,
)


def slug(term):
    return "gl-" + re.sub(r"[^a-z0-9]+", "-", term.lower()).strip("-")


def load_terms():
    """term -> (anchor, tooltip). Only from the hardware/systems section."""
    try:
        text = open(GLOSSARY, encoding="utf-8").read()
    except OSError:
        return {}
    # Narrow to the section: from its heading to the next same-level heading.
    m = re.search(r"^##\s+" + re.escape(SECTION) + r"\s*$(.*?)(?=^##\s|\Z)", text, re.S | re.M)
    if not m:
        return {}
    body = m.group(1)

    terms = {}
    # "- **TERM** - definition ..." possibly continued on indented lines; "A / B" = two terms.
    for em in re.finditer(r"^-\s+\*\*(.+?)\*\*\s*-\s*(.+?)(?=^-\s+\*\*|\Z)", body, re.S | re.M):
        names, definition = em.group(1), " ".join(em.group(2).split())
        definition = re.sub(r"[*`]", "", definition)
        if len(definition) > 240:
            definition = definition[:237].rsplit(" ", 1)[0] + "..."
        for name in [n.strip() for n in names.split("/")]:
            # Guard: never auto-link something too short to be unambiguous.
            if len(name) >= 3 and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9.-]*", name):
                terms[name] = (slug(names.split("/")[0].strip()), f"{name} - {definition}")
    return terms


TERMS = load_terms()
# Longest first so "MSI-X" wins over "MSI".
ORDER = sorted(TERMS, key=len, reverse=True)


def attr(s):
    """Escape a string for use inside a double-quoted HTML attribute."""
    return s.replace("&", "&amp;").replace('"', "&quot;").replace("<", "&lt;").replace(">", "&gt;")


def link_plain(text, prefix, remaining):
    """Wrap the first occurrence of each still-unused term.

    Matches are collected against the ORIGINAL text and applied in one pass from the end.
    Rewriting iteratively would let a later term match inside an already-inserted tooltip -
    e.g. the IOMMU definition contains "DMA", so DMA would be linked *inside* the IOMMU
    title attribute, nesting tags inside an attribute and corrupting the HTML.
    """
    hits, claimed = [], []
    for term in ORDER:
        if term not in remaining:
            continue
        anchor, tip = TERMS[term]
        pat = re.compile(r"(?<![A-Za-z0-9_-])" + re.escape(term) + r"(?![A-Za-z0-9_-])")
        for m in pat.finditer(text):
            if any(s < m.end() and m.start() < e for s, e in claimed):
                continue  # overlaps a term already claimed (e.g. MSI inside MSI-X)
            hits.append((
                m.start(), m.end(),
                f'<a class="gs-abbr" href="{prefix}glossary.html#{anchor}">'
                f'<abbr title="{attr(tip)}">{term}</abbr></a>',
            ))
            claimed.append((m.start(), m.end()))
            remaining.discard(term)
            break
    for start, end, repl in sorted(hits, reverse=True):
        text = text[:start] + repl + text[end:]
    return text, len(hits)


def anchor_glossary(content):
    """Give each entry an id so the links above can deep-link to it."""
    def add(m):
        names = m.group(1)
        return f'<span id="{slug(names.split("/")[0].strip())}"></span>{m.group(0)}'
    return re.sub(r"^-\s+\*\*(.+?)\*\*", add, content, flags=re.M)


def process(chapter):
    path = chapter.get("path") or ""
    content = chapter.get("content", "")
    if path.endswith("glossary.md"):
        chapter["content"] = anchor_glossary(content)
    elif TERMS:
        prefix = "../" * path.count("/")
        remaining = set(TERMS)
        parts = PROTECT.split(content)
        # Even indices are plain text; odd indices are protected spans.
        for i in range(0, len(parts), 2):
            parts[i], _ = link_plain(parts[i], prefix, remaining)
        chapter["content"] = "".join(parts)
    for sub in chapter.get("sub_items", []):
        if "Chapter" in sub:
            process(sub["Chapter"])


def main():
    # Explicit UTF-8 through the byte streams, exactly as link-fixup.py does: text-mode
    # json.load/dump would use the platform encoding (cp1252 on Windows) and corrupt the
    # book JSON on any non-ASCII character.
    _context, book = json.loads(sys.stdin.buffer.read().decode("utf-8"))
    for item in book.get("sections", []):
        if "Chapter" in item:
            process(item["Chapter"])
    sys.stdout.buffer.write(json.dumps(book, ensure_ascii=False).encode("utf-8"))


main()
