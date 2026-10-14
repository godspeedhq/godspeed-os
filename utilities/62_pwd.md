# Utility: `pwd` - print the folder you are in

**Utility:** `pwd` - print the folder you are in
**Status:** Built 2026-10-13, QEMU-verified (`osdev test files`). As-built reference.
**Shape:** shell built-in (see `0_conventions.md` section 2).

---

## 1. Purpose

`pwd` prints the whole path of the current folder - the one a bare name like `notes.txt` means, and the
one `cd` moves. The prompt is a plain `gsh> ` and does not show it, so this is how you ask.

```text
gsh> pwd
/users/default
gsh> cd /system/services
/system/services
gsh> pwd
/system/services
```

A session starts in its home, `/users/default` (`docs/filesystem-layout.md` section 6), and a bare `cd`
goes back there.

## 2. Invocation

| Command | Meaning |
|---|---|
| `pwd` | Print the current folder's whole path. |
| `pwd \| write here.txt` | It is a text producer, so it pipes like `echo`. |

`pwd` takes no argument; given one, it says so and does nothing.

## 3. Why the Unix word

Godspeed's words are its own (`dir`, `read`, `delete`), and the shell answers a Unix word with a hint
rather than running it (`FOREIGN_HINTS`, CLAUDE.md Appendix B.4). `pwd` is the deliberate exception, for
the reason `cd` already is one: `cd` was kept because everyone knows it (`17_cd.md` section 1), and `pwd` is
its other half - anyone who knows one knows the other. Its earlier stand-in, `cd .`, asked a reader to know
that `.` means "this folder", which is a Unix convention too, and one that names nothing.

## 4. Behaviour

Prints the session's current folder, which the shell holds itself (`17_cd.md` section 3). It asks `fs`
nothing, so it answers at once - with or without a disk - and it cannot say whether the folder still
exists: a folder deleted from under the session is reported by the next command that uses it.

## 5. Capabilities

- **Console output** only.

## 6. Conformance

Conforms: `pwd help` and `pwd version` (with a real example, per `0_conventions.md`); listed by the shell's
top-level `help` under **Storage**; a pipe source (rule 12).
