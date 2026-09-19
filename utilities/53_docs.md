# Utility: `docs` - the manual

**Status:** **Built + QEMU-verified** (`osdev test shell`). A shell built-in, full-screen.
Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`docs` opens the manual: what GodspeedOS is, what the kernel does and does not do, and what you
can rely on. It is a document you read; `help` is a reference you consult.

```
docs 0.1.0 - GodspeedOS

  What this is
  MISCIS - the whole of what the kernel does
  What you can rely on
  Seeing it work

[ 1-22 of 46 ]  [arrows] line  [PgUp/PgDn] page  [t] contents  [a] about  [/] find  [q] quit
```

## 2. Why it is not part of `help`

They were one command for about an hour, and that was the wrong shape.

`help` answers *"what is the word for the thing I am trying to do"* - consulted mid-task, many
times a day. `docs` answers *"what is this system"* - read once, deliberately. Putting the second
inside the first means `help` **opens on prose** when somebody wanted `dir`'s byte-count word, and
it makes `help` the place every explanatory thing accumulates. That is the dumping ground `CLAUDE.md`
§4.4 and §26.2 exist to prevent, and the phrase that gave it away was "help can grow organically".

Split, each has one job. `help`'s first screen names `docs`, so it is still found by typing the
obvious thing.

**One browser, two documents.** The contents, the search, the keys and the pinned section line are
parameterised over the document, not copied. A second implementation of a pager is how the first one
went stale.

## 3. Keys

| key | does |
|---|---|
| arrows | scroll a line |
| PgUp / PgDn, space | a page |
| Home / End | the ends |
| `t` | contents - then a **digit** jumps to that section |
| `a` | about: the boot banner, and **this machine's** live architecture |
| `/` | find; `n` for the next match |
| `q`, Esc | leave |

The **section you are in is pinned** at the top. That is the thing a scrollback buffer structurally
cannot give you: scrolled into the middle of a document you would otherwise have no idea which part
you were reading. It is the same reason `trace` keeps its own pager, which pins a column header.

## 4. `[a] about` is drawn from live state, deliberately

The architecture view asks the kernel what is running, on which core, and lists it. It is not a
drawing.

**A hand-drawn box diagram would be a second copy of `CLAUDE.md` §4.1, and a copy rots.** Nine
utilities had fallen out of `help` with nothing watching, in this same session; a picture of an
idealised system would go the same way and nobody would notice, because a diagram cannot fail to
compile. Derived from live state it cannot be wrong - and it is more useful, because it describes
the machine in front of you rather than the one in the document.

The banner is `include_str!` of `assets/godspeed-banner.txt`, the same file the kernel prints at
boot. One source; a second copy of eight lines of ASCII is still a second copy.

## 5. The conceptual half IS a copy, and is gated

§1's layering and the six MISCIS responsibilities do restate `CLAUDE.md` §4.3, because a manual that
cannot describe the model is not a manual.

So it is checked. `scripts/facts_check.py` (`help_philosophy_problems`) derives the six from the
constitution - using the **same slice** `scripts/commandments.py` uses, so there is one way to read
that list - and fails if this text stops naming one. Removing "SMP routing" from the manual fails the
build, which is what makes the copy affordable rather than merely tempting.

## 6. In a script it prints

`depth > 0` - a script, `run`, `assert` or `selfcheck` - dumps the document and returns. A
full-screen browser waiting for a keypress with nobody there does not degrade, it **hangs the run**.
Same guard `paginate` carries, and the reason paging belongs to things you ask for.

## 7. Implementation

`cmd_docs` and `help_browser` in `services/shell/src/main.rs`; the document is the static `DOCS`
table, the same `HelpRow` type `HELP` uses. Sections are derived from the table itself, so adding a
`Sec(...)` puts it in the contents with nothing else to edit.
