#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""NO SILENT WAIT IN THE SHELL: every wait the shell makes says `[q] quit` past 2 s, or says why it cannot.

The operator, 2026-10-13: "Any utility or blocking action that requires a user to wait should have [q] quit or
[b] background (if it's backgroundable) beyond 2 seconds ... GodspeedOS should always be responsive and should
never trap the user behind a block" (`utilities/0_conventions.md` rule 10, `backlog/fixed/89`).

THE SHELL IS THE OBSERVER (`audits/2026-10-13-quit-audit.md` 7a). A command that is a separate program waits
inside the shell, which owns its clock, its `[q]` and its kill; a built-in starts one command clock at Enter
(`begin_command`) that every wait reads. So the one place a wait can trap the user is a wait in the shell that
does NOT read that clock - and that is what this counts, in `services/shell/src/`:

  - the SDK's plain bounded requests (`request_with_reply`, `_deadline`, `_ms`, `_ms_sifted`, `_call_err`),
    which park the shell in the kernel and read no key;
  - `gs::ipc::recv` / `recv_within_ms` and `gs::call::request` / `request_once` / `request_within`, the same
    waits one layer up;
  - an `Fs` or `Net` handle made without `.watching(` or a notice (`Fs::new`, `Fs::from_tag`, `Net::new`).

NOT counted, because they read the keys: `request_watched`, `request_watched_sifted`, `request_within_notice`,
the SDK's `_keyhint_` waits, and `with_notice` handles. Waits on the USER (`console_read`) are exempt by the
rule itself.

A counted site passes when it says why it cannot trap anyone: a comment line `// quit-ok: <reason>` among the
four lines above it (a marker starts its line, so prose that mentions the word is not one) - its bound is under 2 s, it is one pass of a loop that reads `q`, it runs after a
`q` and is held to the 3 s that leaves, or it is between commands, where there is no command to quit. A
marker records an exception; it does not silence one. The number of markers is held to a baseline that may
FALL and may not RISE, so a new exception is a decision made in a commit (`--bless`, saying why), never a
line added to get a build through.

WHAT IT CANNOT SEE, said rather than implied:
  - a LOOP of sleeps or yields that never reads `q` (the chaos modes once were; they read the watch now). A
    loop's bound is a runtime fact, and `osdev test quit` measures it: the table in `backlog/fixed/89`.
  - a wait reached through a helper of the shell's own that wraps one of the counted calls: the call inside
    the helper is counted, once, and its marker has to be true for every caller.
  - a wait between commands (the history save, `tidy_helpers`, an audio shortcut). Those run with no command
    clock; `ask_watched`'s plain branch is marked for exactly that.

`--report` lists every counted site with its marker. `--bless` rewrites the baseline. `--selftest` proves
the check fires on an unmarked wait and passes a marked one.
"""
import io
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SHELL = os.path.join(ROOT, "services", "shell", "src")
BASELINE = os.path.join(ROOT, "scripts", "QUIT-WAIT.baseline.txt")

# The waits that read no key. Each was read off the SDK or `gs` (`sdk/rust/src/service_context.rs`,
# `stdlib/rust/src/{call,ipc,fs,net}.rs`): the keyhint, notice and watched forms are left out on purpose.
SILENT = re.compile(
    r"\.request_with_reply(?:_deadline|_ms|_ms_sifted|_call_err)?\s*\("
    r"|\bgs::ipc::recv(?:_within_ms)?\s*\("
    r"|\bgs::call::(?:request|request_once|request_within)\s*\("
    r"|\bFs::(?:new|from_tag)\s*\((?![^\n]*\.(?:watching|noticing)\s*\()"
    r"|\bNet::new\s*\("
)
MARK = re.compile(r"^\s*//\s*quit-ok:\s*(\S.{9,})")
WINDOW = 4


def strip_comment(line):
    i = line.find("//")
    return line if i < 0 else line[:i]


def scan_text(text, where):
    """(sites, marks): every counted wait as (where, line, code, reason or None), and the marker count."""
    lines = text.split("\n")
    sites, marks = [], 0
    for i, line in enumerate(lines):
        if MARK.search(line):
            marks += 1
        if not SILENT.search(strip_comment(line)):
            continue
        reason = None
        for j in range(i, max(-1, i - WINDOW - 1), -1):
            m = MARK.search(lines[j])
            if m:
                reason = m.group(1).strip()
                break
        sites.append((where, i + 1, line.strip(), reason))
    return sites, marks


def scan():
    sites, marks = [], 0
    for dirpath, _, files in os.walk(SHELL):
        for f in sorted(files):
            if f.endswith(".rs"):
                p = os.path.join(dirpath, f)
                text = io.open(p, encoding="utf-8", errors="replace").read()
                s, m = scan_text(text, os.path.relpath(p, ROOT).replace(os.sep, "/"))
                sites += s
                marks += m
    return sites, marks


def load_baseline():
    if not os.path.exists(BASELINE):
        return 0
    for line in io.open(BASELINE, encoding="utf-8"):
        line = line.strip()
        if line and not line.startswith("#"):
            return int(line.split()[0])
    return 0


def selftest():
    bad = "fn f(ctx: &ShellCtx) {\n    let r = ctx.request_with_reply_deadline(\"fs\", &m, 8);\n}\n"
    ok = ("fn f(ctx: &ShellCtx) {\n    // quit-ok: one pass of a loop that reads q, bounded at 1 s\n"
          "    let r = ctx.request_with_reply_deadline(\"fs\", &m, 1);\n}\n")
    watched = "fn f(ctx: &ShellCtx) {\n    let r = gs::call::request_watched(ctx, \"fs\", &m, 8, ctx);\n}\n"
    handle = "let fs = gs::fs::Fs::from_tag(&self.inner, t);\nlet w = gs::fs::Fs::from_tag(&self.inner, t).watching(self);\n"
    prose = ("/// built on this, or carries a `// quit-ok:` reason (see the checker)\n"
             "let r = ctx.request_with_reply_ms(\"copier\", &m, 9000);\n")
    far = ("// quit-ok: this marker is too far above to cover the wait below\n\n\n\n\n"
           "let r = ctx.request_with_reply_ms(\"copier\", &m, 9000);\n")
    checks = [
        ("an unmarked plain request is counted and fails", scan_text(bad, "t")[0], [None]),
        ("a marked one passes", [s[3] is not None for s in scan_text(ok, "t")[0]], [True]),
        ("a watched request is not counted", scan_text(watched, "t")[0], []),
        ("an Fs handle without .watching is counted, one with it is not",
         [s[1] for s in scan_text(handle, "t")[0]], [1]),
        ("prose that mentions the marker is not one", [s[3] for s in scan_text(prose, "t")[0]], [None]),
        ("a marker more than four lines up covers nothing", [s[3] for s in scan_text(far, "t")[0]], [None]),
    ]
    failed = 0
    for name, got, want in checks:
        got = [s[3] for s in got] if got and isinstance(got[0], tuple) else got
        if got != want:
            failed += 1
            print(f"quit-wait selftest FAILED: {name} - got {got}, wanted {want}")
    if not failed:
        print(f"quit-wait selftest passed - {len(checks)} probe(s), the check fires where it should.")
    return 1 if failed else 0


def main():
    if "--selftest" in sys.argv:
        return selftest()
    sites, marks = scan()
    if "--report" in sys.argv:
        for where, n, code, reason in sites:
            print(f"{where}:{n}: {code}")
            print(f"      {'quit-ok: ' + reason if reason else 'UNMARKED'}")
        print(f"{len(sites)} counted wait(s), {marks} marker(s).")
        return 0
    if "--bless" in sys.argv:
        with io.open(BASELINE, "w", encoding="utf-8", newline="\n") as fh:
            fh.write("# `// quit-ok:` exceptions in services/shell/src (scripts/quit_wait_check.py). MAY FALL, MAY NOT\n")
            fh.write("# RISE; rewritten by `--bless`, and the commit says why the new exception cannot trap anyone.\n")
            fh.write(f"{marks}  services/shell\n")
        print(f"quit-wait: baseline rewritten - {marks} marker(s); say why in the commit.")
        return 0
    unmarked = [s for s in sites if s[3] is None]
    allowed = load_baseline()
    if unmarked:
        print("QUIT-WAIT CHECK FAILED - a wait in the shell reads no key and says nothing past 2 s:\n")
        for where, n, code, _ in unmarked:
            print(f"  {where}:{n}: {code}")
        print("\nThe shell is the observer (backlog/fixed/89, utilities/0_conventions.md rule 10). Make the wait read the")
        print("command's watch - `ask_watched`, `ask_with_quit_notice`, `gs::call::request_watched(_sifted)`, or")
        print("`ctx.fs()` for files - or, if it truly cannot trap anyone, say why on the line above it:")
        print("`// quit-ok: <reason>` (under 2 s, one pass of a loop that reads q, after a q, or between commands).")
        return 1
    if marks > allowed:
        print(f"QUIT-WAIT CHECK FAILED - `// quit-ok:` exceptions rose from {allowed} to {marks}.\n")
        print("Each one is a wait that reads no key. Prefer the watch; if the new one is right, run")
        print("`python scripts/quit_wait_check.py --bless` and say in the commit why it cannot trap anyone.")
        return 1
    if marks < allowed:
        print(f"quit-wait: exceptions fell {allowed} -> {marks}; run `--bless` to lock the lower count in.")
    print(f"Quit-wait check passed - {len(sites)} plain wait(s) in the shell, each saying why it cannot trap "
          f"anyone ({marks} exception(s), none added).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
