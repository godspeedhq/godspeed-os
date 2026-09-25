#!/usr/bin/env python3
"""Prove `comment_symbol_audit.py` FIRES. A check never observed failing is not evidence.

Plants a comment naming two symbols that exist nowhere, runs the audit, restores the file, and
reports whether both were caught.

THE NAMES ARE GENERATED AT RUNTIME, and that is not decoration. This fixture has now fooled the
instrument three times in three different ways, every one of them by putting its own output back into
the instrument's input:

  1. the audit read the COMMENTS UNDER TEST, so any name in a comment "existed" (reported 0/52,523)
  2. the audit read the AUDIT DOCUMENT, so every dead name it recorded became resolvable
  3. the audit read THIS FIXTURE, so hard-coded probe names existed as string literals here

A random name cannot be present in a tree that was written before it was generated, which closes the
whole class rather than the three instances. If this ever reports NO, suspect the haystack first.
"""
import io
import os
import secrets
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TARGET = os.path.join(ROOT, 'services', 'shell', 'src', 'main.rs')
AUDIT = os.path.join(ROOT, 'scripts', 'comment_symbol_audit.py')
ANCHOR = 'fn cmd_sock('


def main():
    tag = secrets.token_hex(6)
    fn_name = 'probe_fn_%s' % tag
    ty_name = 'ProbeType%s::method_%s' % (tag[:4], tag[4:])
    probe = '// AUDIT PROBE (temporary): `%s` and `%s`.\n' % (fn_name, ty_name)

    with io.open(TARGET, encoding='utf-8', newline='') as fh:
        original = fh.read()
    if ANCHOR not in original:
        print('anchor %r not found in %s - not planting blind' % (ANCHOR, TARGET))
        return 1

    try:
        with io.open(TARGET, 'w', encoding='utf-8', newline='') as fh:
            fh.write(original.replace(ANCHOR, probe + ANCHOR, 1))
        out = subprocess.run([sys.executable, AUDIT],
                             capture_output=True, text=True).stdout
    finally:
        with io.open(TARGET, 'w', encoding='utf-8', newline='') as fh:
            fh.write(original)

    hit_fn = fn_name in out
    hit_ty = ty_name.split('::')[-1] in out or ty_name in out
    if hit_fn and hit_ty:
        print('GUARD FIRES: YES - both planted names were reported (%s)' % tag)
        return 0

    print('GUARD FIRES: NO - THE INSTRUMENT IS BLIND')
    print('  planted: %s / %s' % (fn_name, ty_name))
    print('  caught : fn=%s type=%s' % (hit_fn, hit_ty))
    print('  the haystack is almost certainly reading something it should not.')
    return 1


if __name__ == '__main__':
    sys.exit(main())
