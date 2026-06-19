# Utility: `fcap`

**Utility:** `fcap` — file-as-capability self-check
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`fcap` answers **is a file *really* a capability?** — it is the executable proof of the
file-as-capability model (§7.10, the P2 amendment; `docs/persistence.md` §7.2). In GodspeedOS
a file is not a path you have permission to, nor a handle the filesystem trusts you to use
honestly: opening a file mints a **real, kernel-minted capability** for it, and every read or
write is a validated *invocation* of that capability — the same unforgeable, non-escalating,
revocable machinery as any IPC-endpoint cap.

`fcap` is a **diagnostic, not a file tool.** It creates its own throwaway file, exercises every
property the capability model promises against it, prints one line per check, then deletes the
file. It never touches a file of yours and takes no path argument — so it is safe to run any
time as a "does the file-cap model still hold end-to-end?" probe.

## 2. Invocation

| Command | Meaning |
|---|---|
| `fcap` | Run the self-check against an internal throwaway file. |
| `fcap help` | Print usage. |
| `fcap version` | Print the version (uniform across utilities). |

`fcap` takes **no path** — passing one is refused (`fcap: takes no argument …`). It uses its own
hidden file (`/.fcap-selftest`) and removes it on exit, so it is leak-free and re-runnable.

## 3. Output

```
gsh> fcap
fcap: opened rw (file cap)
fcap: write via cap OK
fcap: read via cap OK
fcap: ro-cap write rejected by kernel (non-escalation)
fcap: fs refused write under read right (op<=right)
fcap: forged handle rejected
fcap: cap revoked after close
fcap: cap revoked after rename
fcap: all file-capability checks passed
```

Each line is one verified property. Any `FAIL` line (and a non-zero result) means the
file-capability model is broken — a constitutional regression, not a cosmetic one.

## 4. What it verifies

The checks map one-to-one onto the §7.3 capability properties, applied to a *file*:

| Check | Property pinned |
|---|---|
| read / write **through** the cap | A file **is** a capability — operated by invoking it, not by naming a path the fs trusts (§7.10). |
| read-only cap's write rejected **by the kernel** | **Non-escalation** at the kernel layer — the cap lacks `WRITE`, so the invocation never reaches `fs` (`CapInsufficientRights`). |
| `fs` refuses a write op sent under a read-validated badge | **Non-escalation** at the `fs` layer — `op ≤ right`; even a correctly-validated read cap can't smuggle a write (`FS_DENIED`). |
| fabricated handle rejected | **Unforgeable** — only the kernel mints valid caps; a made-up handle is not one. |
| cap stale after **close** | **Revocable** — closing the file revokes the resource (generation bump); the next use is stale. |
| cap stale after **rename** | **Revocable** on path rebinding — renaming revokes a still-open cap so it can never silently rebind to a different file later created at the old path (confused-deputy avoidance, §7.10). |

The unforgeable-badge detail: the right that authorises a file-cap invocation is carried to `fs`
in an **unforgeable, kernel-set `Message` field**, not in the client's payload — so a client
cannot fake a file-cap operation over its ordinary `fs` send cap.

## 5. Capabilities

- **A narrow `SEND` cap to `fs`** (the shell already holds it for `read`/`write`/`drives`) —
  `fcap` mints and invokes file caps through `fs`; `fs` enforces all disk authority.
- **`RESOURCE_MINT` is *not* held by the shell** — only `fs` mints delegated resource caps
  (§3.1, §7.10). `fcap` receives the minted file cap from `fs` on open; it cannot mint its own.
- **Console output** to print the per-check lines.

## 6. Non-goals

- **Not a file editor or viewer.** `fcap` never reads, writes, or deletes a file of yours —
  use `read` / `write` / `delete` for that. It only touches its own throwaway file.
- **No options or paths.** One behaviour, no flags (§ `0_conventions.md`); it is a self-check,
  not a configurable tool.

## 7. Conformance

Conforms: own `fcap help` (usage with real examples per `0_conventions.md`) and `fcap version`
(number + creator credit), listed by the shell's top-level `help`. Pinned end-to-end by
`osdev test file-cap` (§22 **Test 14** — every line above is asserted), and run in-memory by the
shell's `selfcheck` suite. Hardware-proven on the HP T630 (§23.3). See `0_conventions.md` §3,
`CLAUDE.md` §7.10 / §22 Test 14, and `docs/persistence.md` §7.2.
