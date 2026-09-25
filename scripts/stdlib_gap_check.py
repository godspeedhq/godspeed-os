#!/usr/bin/env python3
"""The standard library's coverage of the SDK, ratcheted.

WHY THIS EXISTS. `feat/stdlib` shipped claiming the standard library was complete, and it was not:
nine of fifteen examples still had to import `godspeed_sdk` because nothing in `gs` could send a
message. The claim was not dishonest, it was UNMEASURED - it counted what `gs` had GAINED (93 items,
then 130) and never asked the only question that settles completeness: what can userspace still get
ONLY from the SDK? A count of what you added tells you nothing about what is still reachable the old
way, and the same blind spot produced a second round of missing modules a branch later.

So this asks the question in the direction that can fail. For every FREE public item of
`godspeed_sdk`, is there a `gs` route - and where there is not, does an ordinary (non-driver,
non-harness) service use it anyway? That last group is the number this ratchets. It may fall freely.
It may not RISE without someone editing the figure below and saying why, which is the point: adding
SDK surface that ordinary programs need is now a deliberate act with a paper trail, not an oversight
discovered by an audit two branches later.

WHAT IT DELIBERATELY DOES NOT COUNT, each for a reason the constitution already gives:

  - Drivers (18.1). `Mmio`, `Dma`, `Framebuffer`, the HID decoders - the SDK is DESIGNATED for these.
    A `gs::mmio` would be a second name for a module 18.1 names, or a constitutional amendment.
  - Test and harness services (18.1). `adversarial.rs` reaches the raw ABI on purpose.
  - Items nothing uses. Surface nobody has needed is not a gap, it is 26.2 working.

INSTRUMENT NOTE, because the first cut of this measurement was wrong by 13 and would have sent
someone rebuilding things that already worked. A `pub fn` INSIDE an `impl` is a METHOD, and a method
is reachable the moment its type is re-exported - `gs::record::Table` carries `filter`, `select`,
`aggregate`, `decode` and the rest along with it. Only column-0 items are free items needing their
own route. Indentation is the whole discriminator. `--list` prints the current set so the next person
can check the instrument rather than trust this docstring.
"""
import io
import os
import re
import sys

SDK = 'sdk/rust/src'
STD = 'stdlib/rust/src'

# The ratchet. Lower it freely when you close a gap. Raising it needs a line in the commit message
# saying which SDK item an ordinary service now needs and why `gs` does not cover it.
BASELINE = 8

# 18.1 designates the SDK for device work; these crates are its intended callers.
DRIVERS = {'block-driver', 'dwc2', 'ehci', 'nic-driver', 'xhci', 'driver-skeleton', 'console'}
# Harness, adversarial and control surface - also 18.1, also not ordinary programs.
HARNESS = {'probe', 'mem-pressure', 'control', 'chaos'}
# Services that IMPLEMENT a wire protocol `gs` is the client for. `events` is the trace sink, so it
# decodes the opcodes `gs::trace` encodes - the same relationship `fs` has to the filesystem protocol.
# A protocol's server necessarily knows its own wire format; that is not a gap in the client library.
PROTOCOL_IMPL = {'events'}

FREE = re.compile(
    r'^pub (?:const |static )?(fn|struct|enum|trait|type|const|static)\s+([A-Za-z_][A-Za-z0-9_]*)')


def free_items(root):
    """Column-0 public items only. An indented `pub fn` is a method (see the instrument note)."""
    found = {}
    for dirpath, _dirs, files in os.walk(root):
        for fn in sorted(files):
            if not fn.endswith('.rs'):
                continue
            path = os.path.join(dirpath, fn).replace(os.sep, '/')
            with io.open(path, encoding='utf-8', errors='replace') as fh:
                lines = fh.read().splitlines()
            in_test = False
            for line in lines:
                if '#[cfg(test)]' in line:
                    in_test = True
                if in_test:
                    continue
                m = FREE.match(line)
                if m:
                    found.setdefault(m.group(2), (m.group(1), path))
    return found


def main():
    if not os.path.isdir(SDK) or not os.path.isdir(STD):
        print('stdlib gap: run from the repository root')
        return 2

    sdk = free_items(SDK)
    std = free_items(STD)

    std_text = ''
    for dirpath, _d, files in os.walk(STD):
        for fn in files:
            if fn.endswith('.rs'):
                with io.open(os.path.join(dirpath, fn), encoding='utf-8', errors='replace') as fh:
                    std_text += fh.read()

    consumers = {}
    for top in ('services', 'examples'):
        if not os.path.isdir(top):
            continue
        for dirpath, dirs, files in os.walk(top):
            dirs[:] = [d for d in dirs if d != 'target']
            for fn in files:
                if not fn.endswith('.rs'):
                    continue
                path = os.path.join(dirpath, fn).replace(os.sep, '/')
                with io.open(path, encoding='utf-8', errors='replace') as fh:
                    body = fh.read()
                body = re.sub(r'//[^\n]*', '', body)      # prose mentioning a name is not use
                if 'godspeed_sdk' not in body:
                    continue
                crate = path.split('/')[1]
                for name in sdk:
                    if re.search(r'\b%s\b' % re.escape(name), body):
                        consumers.setdefault(name, set()).add(crate)

    ordinary = []
    for name, (kind, path) in sorted(sdk.items()):
        if name in std or re.search(r'\b%s\b' % re.escape(name), std_text):
            continue                                  # `gs` names it: re-export, wrapper or call-through
        users = consumers.get(name, set())
        if not users or users <= (DRIVERS | HARNESS | PROTOCOL_IMPL):
            continue
        ordinary.append((name, kind, path.replace(SDK + '/', ''), sorted(users)))

    n = len(ordinary)
    if '--list' in sys.argv:
        print('SDK free public items: %d; `gs` names %d' % (len(sdk), len(sdk) - n))
        print('')
        print('Used by an ordinary service with no `gs` route (%d):' % n)
        for name, kind, path, users in ordinary:
            print('  %-28s %-7s %-20s %s' % (name, kind, path, ', '.join(users)))
        return 0

    if n > BASELINE:
        print('stdlib gap: %d SDK item(s) are used by an ordinary service with NO `gs` route '
              '(baseline %d).' % (n, BASELINE))
        print('')
        for name, kind, path, users in ordinary:
            print('  %-28s %-7s %-20s %s' % (name, kind, path, ', '.join(users)))
        print('')
        print('An ordinary program should not need `godspeed_sdk`. Either add the route to `gs`, or')
        print('raise BASELINE in this file and say in the commit message which item and why.')
        print('Run with --list for the full set. Drivers, harness and protocol implementers are exempt.')
        return 1

    if n < BASELINE:
        print('stdlib gap: %d (baseline %d) - a gap was CLOSED. Lower BASELINE to %d to keep the '
              'ratchet tight.' % (n, BASELINE, n))
        return 1

    print('stdlib gap: %d SDK item(s) used by an ordinary service with no `gs` route, at the '
          'baseline of %d (%d SDK items scanned; drivers and harness exempt per 18.1)'
          % (n, BASELINE, len(sdk)))
    return 0


if __name__ == '__main__':
    sys.exit(main())
