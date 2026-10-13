# The filesystem layout, folder capabilities and users

**Status: PHASE 1 BUILT 2026-10-13, QEMU-verified, hardware cards pending; phases 2 and 3 are design.**
Phases 2 and 3 are designed here so phase 1 does not paint them into a corner. What phase 1 built, and
the one thing it dropped (moving the old root files, section 4), is section 13. Sections 1 to 7 are the
design as agreed; where the build differs, section 13 says so.

**Phase 1 needs a reformat.** A disk formatted before it has none of the folders and its old root files
are not moved: `drives flash` gives a disk the tree. The operator chose that over a migration, because
every test disk is reflashed anyway (2026-10-13).

**The rule this rests on:** a location expresses organisation, a capability expresses authority, and
neither a pathname nor a user name grants anything by itself. GSFS keeps its tree and its rules in
userspace; the kernel keeps enforcing capabilities and memory isolation and learns nothing new. MISCIS
stays six (CLAUDE.md 4.3).

---

## 1. What is true today

Everything a service keeps lives in the ROOT of the disk, next to whatever the operator wrote there:

| File | Owner | What it holds |
|---|---|---|
| `/wifi.keys` | `wifi-driver`, `wifi-usb` | derived WiFi keys, never passphrases (`utilities/56_wifi.md` 6) |
| `/wifi.radio` | the shell's `wifi hardware use` writes it; the radio drivers read it | which radio is in use |
| `/clock.last` | `time` | the clock floor across reboots |
| `/audio.settings` | `audio-driver`, `pwm-audio` | volume, mute, output |
| `/persist.conf` | the shell's `events persist ... sticky` | the capture that resumes at boot |
| `/.gsh_history` | `shell` | the operator's command history |
| `/.write.tmp`, `/.edit.tmp`, `/.fcap-*` | `shell`, `edit`, `fcap` | scratch |

And **any service that can send to `fs` can read and write every file**, `/wifi.keys` included. There are
no permissions: a file capability (CLAUDE.md 7.10) is the only per-file authority, and only `Open` mints
one. So today's layout is not a security boundary, and moving files into folders will not make it one by
itself - that is what phase 2 is for.

Two GSFS facts the design depends on, both checked:

- **A name is at most 38 bytes** (`NAME_MAX`; a directory record is full, `services/fs/src/main.rs`). Every
  folder and user name below must fit.
- **`Move` is journaled.** `fs` runs it inside a transaction (`txn!(fs.move_path(..))`), so a move is
  replayed or discarded after a power cut, never half-done. Migration (section 4) rests on this.

---

## 2. The layout

```text
/
├── system/                      machine state: what the machine and its services need, whoever uses it
│   ├── services/<service>/      state one service owns       time/, audio-driver/, events/, login/ later
│   ├── utilities/<utility>/     state a standalone utility owns; most need none
│   ├── shared/<what>/           state more than one owner keeps, on purpose: wifi/
│   └── scratch/<owner>/         disposable working files, each owner's own
└── users/
    └── default/                 the operator's files, settings and history
```

**Names are Godspeed words that say what a folder holds**, not `/etc`, `/var`, `/home` or `/tmp`. A familiar
convention is adopted only when it solves a problem here; being Unix's is not a reason (section 9).

### Machine state and personal state

| State | Where | Why |
|---|---|---|
| WiFi keys, the radio in use | `/system/shared/wifi/` | two drivers keep it (section 5) |
| The clock floor | `/system/services/time/` | `time` owns the clock |
| Audio settings | `/system/services/audio-driver/` (and `pwm-audio/` on the Pis) | the driver owns the device |
| The sticky capture | `/system/services/events/` | `events persist` belongs to `events` |
| **Shell history** | **`/users/default/`** | it is about a person, not the machine |
| Editor recovery files | the user's folder | a person's work |
| Temporary working files | `/system/scratch/<owner>/` | disposable |

**The owner of state is the one that keeps it, not the command you typed.** `audio volume 60` is a shell
command, and the volume it sets is `audio-driver`'s state. Most utilities are built into the shell and keep
nothing; `utilities/<name>/` exists for a utility that is its own program AND keeps machine state.

**Scratch for a built-in utility is the shell's.** A utility that runs inside the shell has the shell as its
owner, so its scratch is `/system/scratch/shell/`. Only a utility that is its own program gets its own.

### Why the operator's files are in `/users/default/` from the start

If the operator's files lived in the root until users exist, adding users would mean moving them and
redefining what the root is for. Starting in `/users/default/` avoids both. The shell starts there, so the
longer path costs nothing in practice (section 6). `default` is a label: when users exist, this folder is
associated with an authenticated user, and the name grants nothing.

---

## 3. Who creates the folders

Three creators, each at its own moment:

| When | Creator | What |
|---|---|---|
| Format | `fs` | the standard tree: `/system/{services,utilities,shared,scratch}` and `/users/default` |
| First use | the owner | its own folder, `MkdirP`, and moving its old file |
| Grant (phase 2) | `fs`, asked by an authority that may provision | the folder a capability is granted for, if missing |

**Format.** `drives flash` asks `fs` to format, and `fs` writes the standard tree as part of the format, so a
fresh disk shows the layout from its first boot. The shell's `drives` does not build the tree itself: the
tree is `fs`'s. **`fs` learns two layout names and no more** - `system` and `users`, and the four folders
under `system`. It never learns a service's name: a list of services inside `fs` would drift exactly as the
six service lists `backlog/35` records did.

**First use.** Each owner creates its own folder the first time it writes (`MkdirP`, which exists). This is
what makes disks formatted before the change work without a reformat.

**Grant (phase 2).** When folder capabilities exist, the supervisor asks `fs` for a capability on
`/system/services/time/` as it spawns `time`, and `fs` creates the folder if it is missing. Creating a folder
as part of a grant requires **provisioning authority**, which is distinct from **folder authority** (what a
holder may do inside a granted folder): asking for a capability must not become a way to create arbitrary
paths. Once this exists, a service never needs authority over `/system/services/` to create its own folder.

---

## 4. Moving the old files

Each owner moves its own file, at start, the first time it runs after the change:

1. **Look at the new path first.** If it exists, it is the state, whatever is at the old path.
2. **Otherwise, if the old path exists, `Move` it to the new one.** One journaled operation: after a power
   cut it either happened or did not, and the next start finds one of the two states and finishes the job.
   Old data is never lost before the new copy is committed, because there is never a copy - a move is a
   rename in the tree.
3. **If both exist, change nothing and say so.** Two copies is a conflict this owner cannot resolve by
   guessing, so it uses the new one and logs once that the old one is still there.
4. **Log a successful move once.**

**To check before relying on step 2:** a `Move` across directories under `osdev test fs-journal`'s crash
injection. The journal covers it by construction; the test makes it a fact.

| Old | New | Mover |
|---|---|---|
| `/wifi.keys` | `/system/shared/wifi/keys` | whichever radio driver starts first |
| `/wifi.radio` | `/system/shared/wifi/radio` | whichever reader starts first: a radio driver or the shell |
| `/clock.last` | `/system/services/time/floor` | `time` |
| `/audio.settings` | `/system/services/audio-driver/settings` (`pwm-audio/` on the Pis) | the driver |
| `/persist.conf` | `/system/services/events/sticky` | the shell (it owns `events persist` today) |
| `/.gsh_history` | `/users/default/history` | the shell |
| `/.write.tmp`, `/.edit.tmp`, `/.fcap-*` | `/system/scratch/shell/` | not moved; scratch is recreated |

**Where the paths live.** The WiFi paths are constants in `sdk/wifi/src/keyfile.rs` (`PATH`, `RADIO_PATH`),
which both radio drivers use - and the shell keeps its own copy of the radio path (`RADIO_FILE`), which phase 1
makes the shared constant. The audio path is one constant in `sdk/audio/src/settings.rs`, for both audio
drivers. So each move is one change in one place, plus the move-at-start in each reader.

**Two movers at once** (the WiFi files, which a radio driver and the shell may both reach first): `Move` is atomic and the
second mover finds the old path gone and the new one there - step 1. No coordination is needed beyond the
journal.

---

## 5. WiFi: shared state, and authority later

Phase 1 moves `/wifi.keys` to `/system/shared/wifi/keys`, and both radio drivers keep reading and writing it
directly. No credential service is introduced for the move: a new service needs a demonstrated ownership,
security or lifecycle reason, and a folder change is not one.

Phase 2 grants it narrowly: write to the driver that joins, read to the other. **Shared information does not
need shared write authority.** If narrow direct grants prove insufficient - two drivers that both genuinely
need to write, and must not clobber each other - that is the evidence for a single owner reached over IPC,
and not before.

---

## 6. The shell: where you are

**The shell starts in `/users/default/`.** That changes what a bare name means: `write notes.txt` writes
`/users/default/notes.txt`, where today it writes `/notes.txt`. Absolute paths are unaffected.

**Going home is plain `cd`.** Today a bare `cd` goes to `/`; it will go to the session's home instead. No
`~`: it is a Unix convention you have to already know. In a path, home is the variable **`$home`** - `dir
$home`, `cd $home/scripts` - which the shell sets from the session's home. gsh already has variables, so this
adds no new syntax, and it cannot collide with a real folder named `home` the way a reserved path word would.
GSFS never learns the word.

**The prompt shows where you are**, so no new utility is needed:

```text
/users/default gsh> cd /system/services/time
/system/services/time gsh> cd
/users/default gsh> _
```

- The path goes BEFORE `gsh> `, and `gsh> ` stays exactly as it is: every test harness and run script waits
  for that text, so a prefix breaks none of them.
- It is bounded, because a deep path would push the cursor off a narrow terminal: past about 32 characters it
  is shortened from the left, `.../long/path/here gsh> `, keeping the end, which is the part that says where
  you are. (32 is a starting value, not a measurement; the console's width could set it instead.)
- **The full path is `cd .`**, which already exists: it stays and prints where it is (the shell's answer to
  `pwd`, in `FOREIGN_HINTS`). A separate utility would be a second way to do the same thing.

**What phase 1 must audit, because a bare name changes meaning:** the `selfcheck` parts, every script under
`scripts/`, the worked examples in `utilities/` and `docs/`, the shell's tests (`osdev/src/shell_test.rs`),
and any utility that opens a bare name. `doc_command_check.py` catches a documented command that no longer
runs; a documented command that runs and writes somewhere else needs reading.

---

## 7. Scratch

`/system/scratch/<owner>/` is disposable. **Each owner empties its own when it starts**; `fs` does not empty
scratch at mount, because only the owner knows whether its temporary state is still meaningful. Emptying must
be safe to repeat, survive an interrupted earlier run, and touch nothing outside that owner's folder.

Nothing that must survive a restart goes in scratch. An editor's recovery file is durable state and belongs
in the user's folder.

---

## 8. Phase 2: folder capabilities

**Folder capabilities come before users.** Until `fs` enforces them, a user's folder would be a convention
any service could ignore.

### The mechanism exists; the work is in `fs`

A file capability is a delegated resource (CLAUDE.md 7.10): `fs` mints a resource ID, the kernel validates and
routes every use of it, and `fs` maps the ID to a file. **A folder capability is the same mechanism, with the
ID mapped to a subtree.** No new syscall and no new kernel responsibility.

### Rights: the kernel's three, and `fs`'s own

The kernel's rights are READ, WRITE and GRANT, and they stay that way. Finer rights - create, delete, rename -
live in `fs`'s record for the resource and are checked by `fs`, the way it already refuses a write carried
under a read-validated badge. **`fs`-local rights only narrow what the kernel's allow; they never widen it.**

**Narrowing on hand-off goes through `fs`.** The kernel's `derive_cap` copies a capability and cannot narrow
`fs`'s rights, and `fs` keeps rights per resource, not per holder - so a copy handed on carries the same `fs`
rights. "Give the editor my folder, read-only" is therefore a request to `fs`: mint a NEW resource with a
subset of the rights, and hand that on. A holder can never produce a capability with more `fs` rights than its
own.

### What `fs` must enforce

- Paths resolve relative to the granted folder; `..` and every equivalent cannot leave it.
- `Rename` and `Move` are checked against the authority for BOTH ends.
- Asking for `/` grants nothing a holder does not already have.
- Deleting a folder revokes every capability on it and under it, as deleting a file revokes its file
  capabilities today (`revoke_open_subtree`).
- What an already-open handle under a revoked folder does next is stated, not discovered.

### The hard part: an `fs` restart

A folder capability is a resource `fs` owns, so **when `fs` dies, every folder capability in the system goes
stale at once** - the same rule that already refuses a file capability across a restart (`fcap reuse`, which
checks that a cap minted before an `fs` restart is refused after it). That is correct: a restart must never
restore authority that was revoked.

But it leaves every service holding a dead folder capability, and reacquiring `fs` by name gives a send
capability to `fs`, not a capability on a folder. So phase 2 needs a **re-grant protocol**: after `fs`
restarts, whatever holds provisioning authority (the supervisor, which already rewires services after a
restart) asks `fs` for each service's folder again and hands it over, and each service accepts a replacement
folder capability while it runs. This is new, and it is where most of phase 2's effort goes. It is pinned by
the same kind of test as Test 13 (CLAUDE.md 22): kill `fs`, and every service must reach its folder again,
and nothing it could not reach before.

### A constitution amendment

Phase 2 changes what a spawn request grants an `fs` client - a folder capability in place of a send
capability to the whole filesystem - which is a change to the authority model CLAUDE.md 7.10 and 13.6
describe. It is recorded as an amendment, as `IMAGE_SPAWN` was (14.1). Phase 1 changes no authority and
needs none.

---

## 9. Phase 3: users

Users are a userspace feature, and the kernel learns nothing about them: no user names, passwords, sessions,
administrators, home folders or groups.

**A user is a set of capabilities**, handed out at login:

1. A `login` service checks the credentials, which it keeps in `/system/services/login/`.
2. A session authority decides what that user's session gets, and the supervisor starts their shell with
   exactly that: a folder capability for `/users/<name>/`, and the authorities that user may use (`kill`,
   `reboot`, `hardware`, ...).
3. The kernel enforces those capabilities without knowing whose they are - "capabilities, not identity"
   (invariant 3).

**An administrator is a user whose session was granted more.** There is no root user and no name that confers
anything: administration is a grant of authority, not a property of a name.

**A session's capabilities are not fixed at login.** Temporary grants, revocation, the end of a session,
sharing between users and starting a program with less authority are all expressible as capability operations.

**A program does not inherit its launcher's authority.** An editor started from a shell that may reboot is not
given `reboot`. The launcher grants what the program needs - its files, the console - and nothing else.

---

## 10. Phase 4: later, only if needed

Package storage, versioned images and application data may need namespaces of their own when the work that
needs them arrives (service images from `fs` are step 2 of `docs/service-ownership.md`). Nothing is added to
the tree in advance of that.

---

## 11. Layout rules

Named L1-L10 so they are not confused with the constitution's invariants 1-12 (CLAUDE.md 3).

- **L1 Organisation is not authority.** A location expresses organisation; a capability expresses authority.
- **L2 Ownership is explicit.** Services own machine state, users own personal state, and shared state has a
  defined owner and explicit authority.
- **L3 No authority from a path.** A pathname identifies an object; a capability authorises access to it.
- **L4 No implicit administrator.** Administration is a grant, not a name.
- **L5 No ambient authority for programs.** A program gets what its work needs, not what its launcher holds.
- **L6 No escalation.** `fs`-local rights narrow the kernel's and never widen them; a hand-off with narrower
  rights is a new resource minted by `fs`.
- **L7 No hidden migration failure.** A move is atomic, repeatable, and says when it finds a conflict.
- **L8 No speculative service.** A new service needs an ownership, security or lifecycle reason.
- **L9 No POSIX reflex.** A name is chosen for what it means here; a Unix convention needs its own reason.
- **L10 MISCIS stays six.** Layout, provisioning, authentication and sessions are userspace.

---

## 12. Order of work

1. **Phase 1, the layout.** `fs`'s format writes the tree; each owner moves its file (section 4); the shell
   starts in `/users/default/`, `cd` goes home, `$home` exists, the prompt shows the folder; the audit in
   section 6; the `fs-journal` check on a cross-directory `Move`. No authority changes.
2. **Phase 2, folder capabilities.** Folder resources, `fs`-local rights, provisioning authority, the re-grant
   protocol after an `fs` restart, each service granted only its own folder, and the CLAUDE.md amendment.
3. **Phase 3, users and login.** Authentication, sessions, personal folders, programs started with less.
4. **Phase 4**, only when package management or application spaces need it.

---

## 13. Phase 1 as built (2026-10-13)

**What was built**, each piece where the design put it:

- **`fs` lays down the tree when it formats** (`LAYOUT` and `make_layout` in `services/fs/src/main.rs`): the
  five folders in one transaction after `format_into`, so a power cut leaves all of them or none. A disk
  that formatted but could not take the tree is reported as a failed flash. `osdev mkfs` and
  `osdev script-disk` write the same tree host-side (`gsfs_layout`); the test fixtures keep the bare
  format, because several suites count what is on the disk they built.
- **Each owner makes its own folder before it writes**: the WiFi keys and the radio choice
  (`godspeed_wifi::keyfile::make_dir`, used by both radio drivers and the shell), the clock floor (`time`,
  a `MkdirP` sent ahead of each write), the audio settings (`sdk/audio`'s `Place`, per driver), and the
  sticky capture record (the shell).
- **The paths:**

| Was | Is |
|---|---|
| `/wifi.keys`, `/wifi.radio` | `/system/shared/wifi/keys`, `/system/shared/wifi/radio` |
| `/clock.last` | `/system/services/time/floor` |
| `/audio.settings` | `/system/services/audio-driver/settings`, `/system/services/pwm-audio/settings` |
| `/persist.conf` | `/system/services/events/sticky` |
| `/.gsh_history` | `/users/default/history` |
| `/.write.tmp`, `/.edit.tmp`, `/.fcap-*`, `/.fl<id>~` | `/system/scratch/shell/` |

- **The shell starts at `/users/default`**, without asking `fs` anything - the prompt must come up at once,
  and a home that is missing is said by the first command that uses it. A bare `cd` goes home; `cd .`
  prints the whole path; the prompt is `<folder> gsh> `, shortened from the left past 32 characters.
- **The shell's scratch** is emptied and made at its first use in a session (`ensure_scratch`), not at
  start, for the same reason the start touches no disk.

**What was dropped: moving the old files (section 4).** Nothing reads the old root files any more and
nothing moves them; a disk keeps them as ordinary files until somebody deletes them. Section 4 stays as the
design should a migration ever be wanted - for instance a disk whose files cannot be lost to a reformat.

**Where it differs from the design:**

- **`$home` is a script word only.** It is reserved beside `$args` and `$self` (`let home = ...` is
  refused), but a line typed at the prompt is not expanded - variables are a script feature, and the
  prompt is not a script. At the prompt, plain `cd` goes home.
- **`/sc`, the self-check's own folder, stays at the root.** It is test data written by the operator's
  command, so it is the operator's, and moving it changes nothing the layout is for.

**Verified in QEMU (2026-10-13):** `osdev test files` 266 passed, 0 failed, fourteen of them the layout's
own checks: the tree after a flash, a bare name landing in the home folder, an append through the scratch
folder, `tree /`, the prompt following `cd`, a bare `cd` going home, a long folder shortened in the prompt,
`cd .`, and history in `/users/default/history`. The embedded `selfcheck` ran 592, failed 0, with a layout
part of its own (the script suite, 11/0). `shell` 272/0 (2 skipped, no EHCI), `file-cap` 16/0, `fs-restart`
11/0, `edit` 15/0, `audio` 69/0. On the Pi 2 and RISC-V `virt` a USB stick flashed with `drives flash` showed
the same tree; on the Pi 4 QEMU has no xHCI, so only the prompt (`/users/default gsh> `) and the honest
`cd: storage unavailable` were seen. One defect was found on the way and fixed: `for line in (...)` wrote its
temp file into the scratch folder without making it first, and `selfcheck` failed three statements until it did.

**Hardware cards pending:** every board's data disk is reflashed with `drives flash`, then `tree /` and
`selfcheck`.
