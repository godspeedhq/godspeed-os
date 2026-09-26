<!-- SPDX-License-Identifier: GPL-2.0-only -->
# What `conform` says when something is wrong

**GENERATED - do not edit.** Regenerate with:

    py scripts/conform.py --gallery

The catalogue of what a contributor actually SEES, one entry per rule, produced by planting a
real violation and capturing the output. Generated from the same `tests/conformance/ui/*.case`
corpus that `--selftest` verifies, so the catalogue cannot drift from the tested behaviour: one
corpus, two views.

Why it exists: `CLAUDE.md` 22.7 says **a gate that fires with an unhelpful message is a finding,
not a pass**. That is a claim about rendered text, and the only way to hold it is to READ the
text - so it is written down, reviewable in a diff, and regenerated rather than remembered.

## a backlog entry with no status

26.7 says a limitation that cannot be closed is RECORDED. A record nobody can find is not a record, and a record whose STATE is unclear is worse: two hand surveys of that folder disagreed with each other before the status line was made mandatory and mechanical - one read only the first four lines, one put three closed items in the open pile.

*Planted in `backlog/99-a-conform-probe-entry.md` (`create`), caught by `scripts/backlog_check.py`.*

```
error[GS0410]: a backlog entry has no status line, or is not linked from the index
   --> backlog/99-a-conform-probe-entry.md
    |
    = rule: CLAUDE.md 26.7
    = why: 26.7 says a limitation that cannot be closed is RECORDED. A record nobody can find
           is not a record: two hand surveys of that folder disagreed with each other before
           the status line was made mandatory and mechanical.
    = help: Put `**Status:` in the first 12 lines carrying OPEN or CLOSED, and add a row to
            `backlog/README.md`. An entry also owes its evidence, what is RULED OUT, and the
            next concrete step.
    = note: `py scripts/conform.py --explain GS0410` for the long form

  backlog_check.py reported:
  | backlog check: NOT INDEXED: backlog/99-a-conform-probe-entry.md is not linked from backlog/README.md
  | backlog check: NO STATUS: backlog/99-a-conform-probe-entry.md has no "**Status:" line carrying CLOSED/OPEN/RESOLVED/FIXED (capitals) in its first 12 lines
  | (1 more line of explanation, which the frame above covers - `py scripts/backlog_check.py` for all of it)
```

## a doc missing from the index

An index that calls itself the index while files are invisible to it is worse than no index. Two documents sat unlisted in `docs/`, and six backlog entries before that - and a reader picks from the index, so a file nobody can reach is a file nobody reads. `mode: create` exists for this shape of rule: it can only be tripped by a file that does not exist yet.

*Planted in `docs/zz-conform-probe.md` (`create`), caught by `scripts/docs_index_check.py`.*

```
error[GS0408]: a file in docs/ is not reachable from the docs index
   --> docs/CLAUDE.md
    |
    = rule: CLAUDE.md 5
    = why: An index that calls itself the index while files are invisible to it is worse than
           no index. Two documents sat unlisted, and six backlog entries before that.
    = help: Add a row to `docs/CLAUDE.md` saying what the file is FOR. A reader picks from
            the index; a one-word entry does not help them choose.
    = note: `py scripts/conform.py --explain GS0408` for the long form

  docs_index_check.py reported:
  | DOCS INDEX CHECK FAILED: 1 file(s) in docs/ are named nowhere in docs/CLAUDE.md
  |   docs/zz-conform-probe.md
  | CLAUDE.md 5 designates `docs/CLAUDE.md` as the index for this directory. A document
  | (3 more lines of explanation, which the frame above covers - `py scripts/docs_index_check.py` for all of it)
```

## a documented invocation that does not run

Names resolving and numbers matching is not enough - nothing asked whether a documented PROMPT actually runs. `dir long /` lists a directory NAMED `long` and discards the path, which is a WRONG ANSWER rather than an error, and it shipped. The accepted words are read from the shell's own SUBCMD_FIRST, so this cannot drift from what the shell really takes.

*Planted in `docs/pipes.md` (`append`), caught by `scripts/doc_command_check.py`.*

```
error[GS0407]: a documented invocation does not work
   --> docs/pipes.md:271
    |
    = rule: CLAUDE.md 26.7
    = why: Names resolving and numbers matching is not enough: nothing asked whether a
           documented PROMPT runs. `dir long /` lists a directory NAMED `long` and discards
           the path, which is a WRONG ANSWER rather than an error, and it shipped.
    = help: Run it and paste what it does. The accepted words are read from the shell's
            `SUBCMD_FIRST` and osdev's own `match suite`, so this cannot drift from either.
    = note: `py scripts/conform.py --explain GS0407` for the long form

  doc_command_check.py reported:
  | doc commands: 1 documented invocation(s) the code does not answer
  |   docs/pipes.md:271
  | (5 more lines of explanation, which the frame above covers - `py scripts/doc_command_check.py` for all of it)
```

## a line citation that rotted

A line number is the fastest-rotting citation in the repository - every edit above it moves it. Audit 7 found 7 of 11 live citations wrong, with FOUR documents citing one dead line because the citation had been copied rather than checked. `audits/`, `milestones/` and `bugs/` are exempt: a line number correct on the day an audit ran is a true record of what was seen. The check is ANCHORED - it looks for a distinctive word from the citing sentence near the cited line - so a citation must carry a real word to be judged. The first draft cited a line past the end of a file in a sentence with nothing to anchor on, and was blessed to "no finding".

*Planted in `docs/pipes.md` (`append`), caught by `scripts/line_ref_check.py`.*

```
error[GS0404]: a `path:line` citation no longer points at what it claims
   --> docs/pipes.md:271
    |
    = rule: CLAUDE.md 26.7
    = why: A line number is the fastest-rotting citation in the repository: every edit above
           it moves it. Audit 7 found 7 of 11 live citations wrong, with four documents
           citing ONE dead line because the citation had been copied rather than checked.
    = help: Re-point it, or cite the FUNCTION or the distinctive comment instead - those
            survive editing and a reader can grep for them. `audits/`, `milestones/` and
            `bugs/` are exempt: a line number correct on the day an audit ran is a true
            record of what was seen.
    = note: `py scripts/conform.py --explain GS0404` for the long form

  line_ref_check.py reported:
  | line refs: 1 citation(s) no longer point at what they claim
  |   docs/pipes.md:271
  |       cites services/shell/src/main.rs:12 - nothing within 10 lines matches the citing sentence
  | (4 more lines of explanation, which the frame above covers - `py scripts/line_ref_check.py` for all of it)
```

## a posix word used as a command

The shell's vocabulary is fresh - `dir`, `read`, `delete`, `copy`, `match`, `count` - and a foreign word is a HINT, never an alias: `ls` does not run, it answers ``try `dir` ``. The `ls` to `dir` rename reached the shell, the specs and the help text and missed TEN worked examples, which is why this is gated from the shell's own FOREIGN_HINTS rather than a list.

*Planted in `docs/pipes.md` (`append`), caught by `scripts/foreign_word_check.py`.*

```
error[GS0406]: a document shows a POSIX or DOS word being used as a command
   --> docs/pipes.md:271
    |
    = rule: CLAUDE.md Appendix B.4
    = why: The shell's vocabulary is fresh - `dir`, `read`, `delete`, `copy`, `match`,
           `count` - and a foreign word is a HINT, never an alias: `ls` does not run, it
           answers ``try `dir` ``. The `ls` to `dir` rename reached the shell, the specs and
           the help text, and missed TEN worked examples.
    = help: Use the Godspeed word. The list this checks is read from the shell's own
            `FOREIGN_HINTS`, so it cannot drift from what the shell actually refuses.
    = note: `py scripts/conform.py --explain GS0406` for the long form

  foreign_word_check.py reported:
  | foreign words: 1 example(s) use a word the shell REFUSES
  |   docs/pipes.md:271
  | (5 more lines of explanation, which the frame above covers - `py scripts/foreign_word_check.py` for all of it)
```

## an em dash in prose

The DECIDABLE class, and what it must look like: one line, not a frame. A fixable violation rendered as a full diagnostic whose help says "conform fixes this" is the least interesting problem taking the most space, which trains a reader to skim past the ones that matter. This case pins the compact form, and it only holds because the fixer claimed the file - a fixable rule that fails on a file the fixer cannot reach keeps its frame.

*Planted in `examples/00-hello/CLAUDE.md` (`append`), caught by `scripts/dash_check.py`.*

```
error[GS0101]: an em-dash or en-dash appears in a tracked text file
    = note: 1 file, listed above; `py scripts/conform.py` fixes it
```

## comment names a deleted symbol

The single highest-value case of the 2026-09-26 sweep. A plausible-looking name for a thing that no longer exists, in a comment a contributor reads BEFORE any document because it sits beside the code being changed. The message has to carry the reason AND both escape routes, because the right fix is sometimes the baseline rather than the comment.

*Planted in `examples/00-hello/src/main.rs` (`append`), caught by `scripts/comment_symbol_check.py`.*

```
error[GS0403]: a Rust comment names something that exists nowhere in the code
   --> examples/00-hello/src/main.rs:53
    |
    = rule: CLAUDE.md 26.7, 26.14
    = why: A comment is read BEFORE any document, because it sits beside the code being
           changed. There are 27,000 doc-comment lines here and until 2026-09-26 nothing
           checked one of them.
    = help: Name what does the job now - or, if it names something OUTSIDE this tree on
            purpose (a hardware register, an SBI call, a Linux function cited per 26.14), add
            it to `scripts/COMMENT-SYMBOLS.baseline.txt` with which kind it is. A comment
            that says "X was deleted" is RIGHT to name X: that is a record, and it belongs in
            the baseline.
    = note: `py scripts/conform.py --explain GS0403` for the long form

  comment_symbol_check.py reported:
  | comment symbols: 1 name(s) in Rust comments name nothing in the code:
  |     `reclaim_view_state`  examples/00-hello/src/main.rs:53
  | (4 more lines of explanation, which the frame above covers - `py scripts/comment_symbol_check.py` for all of it)
```

## comment records a removal

THE CASE THAT MUST STAY QUIET, and it matters as much as the ones that fire. Half of what the 2026-09-26 comment sweep found was comments correctly RECORDING a removal - `pci.rs` has nine such sites and `services/console` ten, one of which literally opens "REQ_SCROLL IS GONE". A checker that fired on those would be unusable, and the first break attempted while building `--selftest` accidentally proved it: it used `paint_view`, which is baselined as exactly this kind of record, so nothing fired. Correct behaviour, wrong test. This case pins that on purpose.

*Planted in `examples/00-hello/src/main.rs` (`append`), caught by `scripts/comment_symbol_check.py`.*

**No finding, and that is the point.** This case exists to prove the gate stays QUIET here.

## contract claims authority nothing grants

A COMMANDMENT violation, which is the case the frame exists for - and the case it rendered WORST before this fixture existed. `conform` treated commandments.py as one checker with one code and the title "a Commandment check failed", so a Commandment IV failure printed "= commandment: all ten". Naming which of the Ten a violation breaks is the whole point of the frame, and for the checker covering all ten it was doing it least well. The violation itself is the one CLAUDE.md 13.6 was written for: a weak model was asked to let a service restart a peer, added `service_control = true` to its CONTRACT, and reported that the kernel would grant it. It compiled, `osdev validate` passed, and every call is denied at runtime while a log line printed BEFORE the call asserts the restart happened. So the message has to say that the contract is not read at runtime, and name both real fixes.

*Planted in `examples/counter/contracts/counter.toml` (`append`), caught by `scripts/commandments.py`.*

```
error[GS0004]: a contract's claim of authority matches what is actually granted
   --> examples/counter/contracts/counter.toml
    |
    = commandment: IV - Thou shalt honor service contracts.
    = why: declares `service_control = true`, and NOTHING GRANTS IT. The contract is not read
           at runtime (13.6): authority comes from the supervisor's spawn row and the
           kernel's `service_config`. A service claiming an authority it is never given
           loads, runs, and has every such call DENIED - and a denial is easy to discard, so
           it fails silently. Either grant SERVICE_CONTROL in services/supervisor/src/main.rs
           (and pin it under [kernel.service_grants]), or delete the claim.
    = help: `COMMANDMENTS.md` is the law and `docs/anti-patterns.md` has the correct pattern
            for this category. An exemption is legitimate ONLY if a CLAUDE.md amendment
            already accepts it - not a baseline entry.
    = note: `py scripts/conform.py --explain GS0004` for the long form
```

## crlf in a boot config

Also decidable, and gated because the failure LOOKS like success. `.gitattributes` declares `boot/**` as `eol=lf` since U-Boot reads a trailing CR as part of every FILENAME: every entry fails while the menu renders perfectly, because a trailing CR in a display string just returns the cursor. Two card reflashes and two wrong theories (backlog/26). The plant must DIFFER from what is on disk - writing the same bytes back changes nothing, which is how the first draft got blessed to "no finding".

*Planted in `boot/pi2/config.txt` (`write`), caught by `scripts/line_ending_check.py`.*

```
error[GS0102]: a tracked text file carries CRLF line endings
    |
    = rule: backlog/26
    = why: A CRLF in a boot config boots NOTHING while showing a perfect menu: U-Boot reads
           the trailing CR as part of every filename. It cost two reflashes before it was
           gated.
    = help: `conform` fixes this by rewriting the file with LF endings.
    = note: `py scripts/conform.py --explain GS0102` for the long form

  line_ending_check.py reported:
  | CARRIAGE RETURNS in files that must be LF:
  | (7 more lines of explanation, which the frame above covers - `py scripts/line_ending_check.py` for all of it)
```

## dash in prose decidable

The DECIDABLE class. One right answer, no reader needed - so `conform` fixes it and reports one line rather than a full frame. A fixable violation rendered as a full diagnostic whose help says "conform fixes this" is the least interesting problem taking the most space, which trains a reader to skim past the ones that matter.

*Planted in `examples/00-hello/CLAUDE.md` (`append`), caught by `scripts/dash_check.py`.*

```
error[GS0101]: an em-dash or en-dash appears in a tracked text file
    = note: 1 file, listed above; `py scripts/conform.py` fixes it
```

## doc names a symbol that is gone

A rename breaks prose SILENTLY, because the sentence still reads correctly. Four had rotted when this gate was written, one of them in CLAUDE.md pointing at a file an amendment in the same document had deleted. The legitimate escape is the baseline, with a reason on the line - a deliberate mention of an external symbol is not a defect.

*Planted in `docs/pipes.md` (`append`), caught by `scripts/doc_symbols_check.py`.*

```
error[GS0402]: a document names a symbol that does not exist in the source
   --> docs/pipes.md
    |
    = rule: CLAUDE.md 26.7
    = why: A rename breaks prose SILENTLY, because the sentence still reads correctly. Four
           had rotted when this was written, one of them in CLAUDE.md pointing at a file an
           amendment in the same document had deleted.
    = help: Name what does the job now. If the mention is deliberate - an external symbol, or
            a proposal that was never built - add it to `scripts/DOC-SYMBOLS.baseline.txt`
            with the reason on the line. The baseline may shrink freely; it may not grow
            silently.
    = note: `py scripts/conform.py --explain GS0402` for the long form

  doc_symbols_check.py reported:
  | DOC SYMBOL CHECK FAILED: 1 backticked name(s) in current-tense docs match nothing in the source.
  |   pipe_thread_the_needle             docs/pipes.md
  | (5 more lines of explanation, which the frame above covers - `py scripts/doc_symbols_check.py` for all of it)
```

## doc points at a missing file

A citation of a file that was deleted sends a reader after nothing. A citation of a BACKLOG entry that was never written is worse: it reads as though the limitation has been recorded, which is the opposite of what 26.7 asks. This gate caught exactly that while `conform` itself was being written - a `backlog/58` cited one commit before it existed.

*Planted in `docs/pipes.md` (`append`), caught by `scripts/doc_refs.py`.*

```
error[GS0401]: a document points at a path that does not exist
   --> docs/pipes.md
    |
    = rule: CLAUDE.md 26.7
    = why: A citation of a file that was deleted sends a reader after nothing. A citation of
           a backlog entry that was never written is worse: it reads as though the limitation
           HAS been recorded, which is the opposite of what 26.7 asks.
    = help: Re-point it, or write the entry you cited. If the target is genuinely gone, say
            so where the citation was rather than deleting the sentence.
    = note: `py scripts/conform.py --explain GS0401` for the long form

  doc_refs.py reported:
  | doc refs: 1 reference(s) point at files that do not exist
  |   docs/pipes.md
  |       -> docs/pipes-internals-that-do-not-exist.md
  | (3 more lines of explanation, which the frame above covers - `py scripts/doc_refs.py` for all of it)
```

## neutral kernel names an isa

The claim a port depends on: you write `arch/<isa>/` and NOTHING else in the kernel changes. Neutral code reaches hardware only through the `arch::imp` seam, so a named ISA or inline asm outside `arch/` is the boundary leaking. The fix is never to special-case the call site - it is to add the missing primitive, because the fault is a MISSING primitive.

*Planted in `kernel/src/ipc/message.rs` (`append`), caught by `scripts/arch_boundary_check.py`.*

```
error[GS0202]: neutral kernel code names an ISA, or contains inline assembly
   --> kernel/src/ipc/message.rs:154
    |
    = commandment: I - CLAUDE.md 4.1
    = why: A port is bounded to `arch/<isa>/`: you write that directory and nothing else in
           the kernel changes. Neutral code reaches hardware only through the `arch::imp`
           seam. Also: use `portable_atomic::AtomicU64`, never `core`'s - 32-bit RISC-V has
           no 64-bit atomic.
    = help: Add an `arch::imp` primitive and call that, rather than special-casing your arch
            at the call site. The fault is a MISSING primitive, not a stubborn call site.
    = note: `py scripts/conform.py --explain GS0202` for the long form

  arch_boundary_check.py reported:
  | Arch-boundary check - FAILURES (arch-specific code leaked into a neutral kernel layer):
  |   kernel/src/ipc/message.rs:154: names `arch::x86_64::` directly - use `arch::imp::` (the seam) so a new arch stays a drop-in
  |   kernel/src/ipc/message.rs:154: uses `core::arch::x86_64::` intrinsics in a neutral file - wrap it in an `arch::imp` primitive in kernel/src/arch/
  | 2 violation(s). The neutral layers must reach hardware only through the `arch::imp` seam (docs/aarch64.md); add an `arch::imp` primitive rather than inlining asm or naming a specific arch. This keeps the NEXT port BOUNDED.
```

## python newer than the declared floor

README.md tells a contributor they need Python 3.8. A hand-measured number is right on the day it is taken and silently wrong afterwards, and the drift arrives as a SyntaxError FROM A CHECKER - the worst first experience this repository can offer. So the floor is held mechanically, and raising it means changing `FLOOR` and the README together, deliberately.

*Planted in `scripts/test_report.py` (`append`), caught by `scripts/python_floor_check.py`.*

```
error[GS0103]: a script uses a Python feature newer than the declared floor
   --> scripts/test_report.py:154
    |
    = rule: README.md, Requirements
    = why: `README.md` tells a contributor they need Python 3.8. That number was measured by
           hand, and a hand-measured number is right on the day it is taken and silently
           wrong afterwards. A contributor on the floor version would meet the drift as a
           SyntaxError from a CHECKER, which is the worst first experience this repository
           can offer.
    = help: Rewrite it to work on the floor, or RAISE the floor deliberately - `FLOOR` in
            `scripts/python_floor_check.py` and the Requirements line in `README.md`,
            together. Never let the number drift upward by accident.
    = note: `py scripts/conform.py --explain GS0103` for the long form

  python_floor_check.py reported:
  | python floor: 1 use(s) of a feature newer than the declared floor (3.8):
  |   scripts/test_report.py:154 needs Python 3.10 - a `match` statement
  | (5 more lines of explanation, which the frame above covers - `py scripts/python_floor_check.py` for all of it)
```

## static mut in a service

Commandment VI, and the reason it is absolute: unowned global mutable state in a service is state nobody owns, which invariants 8 and 9 forbid outright. A service keeps its state in its own bounded structures, and anything that must outlive a restart is persisted and reconstructed - because the service WILL be restarted (Commandment IX), and a static does not survive that. Note the check is about SERVICES: the first draft planted this in `examples/` and was blessed to "no finding", which is the `--bless` hazard doing exactly what it was documented to do.

*Planted in `services/observe/src/main.rs` (`append`), caught by `scripts/commandments.py`.*

```
error[GS0006]: no unowned global mutable state in services
   --> services/observe/src/main.rs:416
    |
    = commandment: VI - Thou shalt not introduce shared mutable state.
    = why: unowned global mutable state: give it an owner, or pass it explicitly
    = help: `COMMANDMENTS.md` is the law and `docs/anti-patterns.md` has the correct pattern
            for this category. An exemption is legitimate ONLY if a CLAUDE.md amendment
            already accepts it - not a baseline entry.
    = note: `py scripts/conform.py --explain GS0006` for the long form
```

## unsafe in a service

Commandment X and CLAUDE.md 18.2: a service may not write `unsafe`, full stop. The SDK's `Mmio`/`Dma` wrappers exist precisely so a DRIVER needs none either, and `services/` is at zero mechanically. The compiler refuses it too (`#![deny(unsafe_code)]`), which is the stronger half; this holds the audit inventory to the source. First written against `examples/00-hello` and BLESSED to "no finding", because `unsafe_check` scans `services/` and `sdk/`. A green case that proves nothing is worse than a missing one.

*Planted in `services/observe/src/main.rs` (`append`), caught by `scripts/unsafe_check.py`.*

```
error[GS0201]: the unsafe inventory does not match the source
   --> services/observe/src/main.rs
    |
    = commandment: X - CLAUDE.md 18
    = why: Unsafe is permitted only in arch/, memory/, capability/ and smp/, plus the SDK's
           audited hardware/ABI layer, and every block carries a SAFETY comment. The
           grandfathered counts may FALL freely and may rise only by a recorded 18.5
           amendment.
    = help: If you added an `unsafe` block, add it to `audits/unsafe-audit.md` in the same
            commit. If you removed one, lower the frozen count. If a service needs `unsafe`,
            it does not: go through the SDK's `Mmio`/`Dma` wrappers.
    = note: `py scripts/conform.py --explain GS0201` for the long form

  unsafe_check.py reported:
  | Unsafe audit - FAILURES:
  |   FAIL  services/observe/src/main.rs: 1 unsafe line(s) - §18.2 forbids `unsafe` in a userspace service; move it behind a safe SDK wrapper (§18.1, e.g. sdk `adversarial`/`mmio`/`dma`)
  | 1 violation(s). See audits/unsafe-audit.md and §18 of CLAUDE.md for the policy.
```

