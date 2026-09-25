"""AUDIT INSTRUMENT: does a backticked name in a CODE COMMENT still exist?

`scripts/doc_symbols_check.py` asks this of every .md file. Nothing asks it of the ~52,000 comment
lines in this tree, and comments rot faster than docs because they sit next to the thing that
changed and nobody re-reads them while changing it.

FIRST VERSION OF THIS WAS BLIND, and the way it was caught is the point. It reported 0 findings, a
planted probe naming two dead symbols was not detected, and the reason was that `load_haystack`
read every file INCLUDING the comments being scanned - so every name in a comment "existed" because
the comment itself was in the haystack. A symbol exists if it appears in CODE, config or prose,
never merely in a comment. Rust comments are stripped from the haystack now.

Deliberately conservative - this is an audit, not a gate, and a false positive costs a human a
minute. So it only reports a name that:
  - appears inside a `//`, `///` or `//!` comment, in backticks
  - looks like a Rust identifier or a path (snake_case, CamelCase, or dotted/slashed form)
  - appears NOWHERE in the tree's non-comment source, config or docs
"""
import io, os, re, sys, collections

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCAN = [os.path.join(ROOT, d) for d in ('services', 'sdk', 'stdlib', 'kernel', 'osdev')]

COMMENT = re.compile(r'^\s*(?://!|///|//)(.*)$')
TICKED = re.compile(r'`([^`\n]{2,80})`')
NAMEY = re.compile(r'^[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\.|/)[A-Za-z0-9_.]+)*$')
RS_COMMENT = re.compile(r'^\s*(?://!|///|//)')

STOP = set('''
true false none some ok err self super crate str u8 u16 u32 u64 i8 i16 i32 i64 usize isize bool
char f32 f64 vec box dyn impl fn let mut pub mod use ref as in if is it a an the and or not no yes
q all one two new get set add del put run cmd arg env dir cd ls cat rm cp mv pwd echo help exit
gsh main type name size len ptr val num idx pos end top low high on off up down left right
'''.split())


def is_interesting(tok):
    base = tok.split('::')[0].split('.')[0].split('/')[0]
    if len(tok) < 4:
        return False
    if tok.lower() in STOP or base.lower() in STOP:
        return False
    if not NAMEY.match(tok):
        return False
    if tok.isdigit():
        return False
    if re.match(r'^[a-z]+$', tok):     # a bare lowercase word is prose, not a symbol
        return False
    return True


def load_haystack():
    """Non-comment source + config + docs. Comments are EXCLUDED - see the module docstring."""
    parts = []
    for base, _dirs, files in os.walk(ROOT):
        norm = base.replace('\\', '/')
        # audits/, milestones/ and bugs/ are DATED EVIDENCE, not a statement of what exists
        # now - and an audit that lists dead names would otherwise make them resolve, which is
        # exactly what happened when Audit 7 was written. Second time this instrument fooled
        # itself by reading its own output; the first was reading the comments under test.
        if any(s in norm for s in ('/target', '/.git', '/build', '/__pycache__',
                                   '/audits', '/milestones', '/bugs')):
            continue
        for fn in files:
            if not fn.endswith(('.rs', '.md', '.toml', '.py', '.gsh', '.json', '.conf', '.txt')):
                continue
            try:
                with io.open(os.path.join(base, fn), encoding='utf-8', errors='ignore') as fh:
                    text = fh.read()
            except OSError:
                continue
            if fn.endswith('.rs'):
                text = '\n'.join(l for l in text.split('\n') if not RS_COMMENT.match(l))
            parts.append(text)
    return '\n'.join(parts)


def main():
    hay = load_haystack()
    misses = collections.defaultdict(list)
    scanned = 0
    for root in SCAN:
        for base, _dirs, files in os.walk(root):
            if '/target' in base.replace('\\', '/'):
                continue
            for fn in files:
                if not fn.endswith('.rs'):
                    continue
                path = os.path.join(base, fn)
                rel = os.path.relpath(path, ROOT).replace('\\', '/')
                try:
                    with io.open(path, encoding='utf-8', errors='ignore') as fh:
                        lines = fh.readlines()
                except OSError:
                    continue
                for n, line in enumerate(lines, 1):
                    m = COMMENT.match(line)
                    if not m:
                        continue
                    scanned += 1
                    for tok in TICKED.findall(m.group(1)):
                        tok = tok.strip()
                        if not is_interesting(tok):
                            continue
                        leaf = re.split(r'::|/', tok)[-1].split('(')[0]
                        if len(leaf) < 4:
                            continue
                        if leaf in hay:
                            continue
                        misses[rel].append((n, tok))

    total = sum(len(v) for v in misses.values())
    print('comment-symbol audit: %d comment lines scanned, %d name(s) resolve to nothing\n'
          % (scanned, total))
    for rel in sorted(misses, key=lambda r: -len(misses[r])):
        print('%s  (%d)' % (rel, len(misses[rel])))
        for n, tok in misses[rel][:40]:
            print('    %s:%d  `%s`' % (rel, n, tok))
    return 0


sys.exit(main())
