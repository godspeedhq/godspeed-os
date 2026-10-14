# Utility: `home` - go to your home folder

**Utility:** `home` - go to your home folder
**Status:** Built 2026-10-13, QEMU-verified (`osdev test files`). As-built reference.
**Shape:** shell built-in (see `0_conventions.md` section 2).

---

## 1. Purpose

`home` moves you to your home folder, `/users/default` - where a session starts and where a bare name like
`notes.txt` is written (`docs/filesystem-layout.md` section 6).

```text
gsh> cd /system/services
/system/services
gsh> home
/users/default
```

## 2. Invocation

| Command | Meaning |
|---|---|
| `home` | Go to `/users/default`, and print it. |

`home` takes no argument; given one, it says so and does nothing.

## 3. Why a word, when a bare `cd` already does it

A bare `cd` goes home, and still does. But it means different things on different systems - "go home" on
Unix, "print where I am" on Windows - so a reader who knows one guesses wrong about the other. `home` cannot
be read two ways, and it is the form help advertises. It is a command, not a folder name: `cd home` still
enters a folder called `home`, if there is one. In a script, the home folder as a path is `$home`
(`docs/scripting.md`), so `cd $home/notes` works where `home` alone would not.

When users exist (phase 3 of the layout), home is the signed-in user's folder; `home` goes there unchanged.

## 4. Behaviour

The same as `cd /users/default`: the folder is checked with `fs` and the move is refused loudly if it does not
exist - a disk formatted before the layout has no home folder, and `drives flash` gives it one. An action, not a
pipe source (`0_conventions.md` rule 12).

## 5. Capabilities

- **`fs`**, for the one check that the folder exists, and **console output**.

## 6. Conformance

Conforms: `home help` and `home version` (with a real example, per `0_conventions.md`); listed by the shell's
top-level `help` under **Storage**.
