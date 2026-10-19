<!-- SPDX-License-Identifier: GPL-2.0-only -->
# Developer services: building a service from outside the tree into an image

**Status: PROPOSAL, 2026-10-15. Nothing here is built and nothing here is approved.** Written for the
`godspeed` developer CLI (the separate `godspeed-cli` repository), whose contract says its first half -
create, build, test, run in QEMU - "can be built now". Measured against this tree it cannot, without
the CLI editing supervisor source, and that is the boundary the CLI's own contract forbids. This
document is the three OS-side changes that would make it true, and what each one deliberately does
not do. Section 9 (2026-10-16) records what the CLI still needs beyond them, and section 10 the native
`godspeed` utility on a Godspeed machine. Also 2026-10-16: 2.1 and 3.5 are corrected against the kernel
verification (`audits/2026-10-16-kernel-verification.md`). Section 11 (2026-10-17) proposes that each
OS release publish the facts the CLI builds against as one file, so the CLI stops reading this
repository's build files.

No kernel change. No MISCIS change. No new syscall. No image loaded from disk.

---

## 0. In one paragraph

A developer's service crate, living anywhere on their machine, can be named to the OS build
(`--with <crate-dir>`); the build validates its contract with the same validator every in-tree
contract meets, runs the per-crate gates on it, compiles it with the pinned toolchain, and **generates**
its spawn row from the contract under a narrow, fixed developer grant policy that refuses every
privilege. The supervisor carries those rows in a third table beside `IMAGES` and the USB rows, starts
and restarts them, and says at boot that the image carries services built outside the tree. For that
generation to be possible, the contract has to be able to say everything a spawn row says (change 2).
And `osdev new` / `osdev publish` are removed, because the developer-project workflow now has an owner
(change 3).

---

## 1. The problem, measured

### 1.1 Adding a service is seven hand edits to OS source

`GETTING_STARTED.md` tells a newcomer to copy `examples/00-hello` and then:

| # | edit | where |
|---|------|-------|
| 1 | add the crate to the workspace | root `Cargo.toml` members |
| 2 | add it to the host build list | osdev `SERVICE_CRATES` (and the board scripts' lists for the other three ISAs) |
| 3 | add it to the embedded list | `services/supervisor/build.rs`, `EMBEDDED` or a per-arch list |
| 4 | embed the ELF | an `include_bytes!` static in `services/supervisor/src/main.rs` |
| 5 | write its spawn row | `IMAGES` in the same file |
| 6 | start it at boot | an `ensure_mapped` / `ensure_wired` call |
| 7 | have it restarted | `MANAGED`, a fixed array whose length `MANAGED_N` must be bumped |

Plus its contract, which must agree with the row of step 5 or `scripts/contract_check.py` fails the
build. Every one of those is reasonable for a service that **ships with the OS**. None of them can be
done by a tool on public interfaces: a CLI that performed them would be patching the supervisor, which
is exactly the "CLI needs a private interface" signal the CLI's contract says to stop at.

### 1.2 The crate itself is welded to the tree

- `examples/00-hello/build.rs` finds `services/user.ld` as `CARGO_MANIFEST_DIR/../..`. Moved anywhere
  else, it does not link.
- The stdlib is a relative path dependency (`../../stdlib/rust`), and the stdlib's own dependency on
  the SDK is relative too. A git dependency on a tag would resolve both (Cargo checks out the whole
  repository), but nothing documents that as supported.
- Every gate a new crate must pass - `one_way_check.py`, `unsafe_check.py`, `spdx_check.py`,
  `contract_check.py`, `stack_fit_check.py` - scans fixed directories (`services/`, `examples/`). A
  crate outside them is not held to zero raw SDK calls; it is simply never looked at.

### 1.3 The contract cannot describe a spawn

Section 4 has the table. The short form: of the nine columns of an `IMAGES` row, the contract covers
four (memory limit, core, send peers, device class) and part of a fifth (privileges). The receive
endpoint, eight of the fifteen privilege bits, `probe_mode`, `dma_pages`, and whether the supervisor
restarts the service exist only in supervisor source. So a row cannot be generated from a contract
today even if we wanted to.

Concrete evidence that the receive endpoint is not derivable: `stdlib-hello`'s row carries
`SPAWN_FLAG_REQ_RECV` (so `fs`'s replies have somewhere to land), while its contract declares no
`ipc_receive` - correctly, because it serves nothing. `pong` declares `ipc_receive` and carries the
same flag for a different reason. One flag, two meanings, neither stated in the contract.

### 1.4 Two tools claim the same verbs

`osdev new` and `osdev publish` exist as subcommands whose bodies are `todo!()` (`cmd_new`,
`cmd_publish`; CLAUDE.md 17 lists them as not implemented). The `godspeed` CLI's contract assigns
`new`, `package` and `publish` to itself. Two half-owners of one verb is the drift `backlog/35` records
for the service list, arriving early.

---

## 2. What this proposal does not do

Stated first, because each of these is the obvious overreach.

- **It does not let a contract grant authority at runtime.** CLAUDE.md 13.6 is unchanged: the kernel
  mints from the spawn request and never reads TOML. What changes is that, **for developer services
  only**, the spawn row is produced from the contract by the build instead of by hand - under a policy
  that refuses everything a hand-written row could give and a TOML edit should not (2.1).
- **It does not load images from disk.** That is step 2 of `docs/service-ownership.md`, with signing,
  and it stays there. A developer service is compiled into the image like every other service.
- **It is not installation.** A developer image is a development build that happens to carry a
  service from elsewhere. It says so at boot (3.5), and the CLI is obliged by its own contract to say
  so too.
- **It does not design the local grant policy.** Who may decide that a package gets more than the
  floor is the open question the CLI's contract lists as "not designed". This proposal answers it
  only for the build-time development case, and only with "nobody, yet".
- **It does not change how in-tree services are declared.** Their rows stay hand-written. Generating
  those too is recorded as an option it enables (4.4), not proposed.

### 2.1 Why generation is safe here and not in general

The 13.6 amendment records a contributor adding `service_control = true` to a contract and believing
it granted. If contracts generated rows across the tree, that edit **would** grant - a TOML edit would
become an authority change that reads like documentation. For a developer service the generator is
not a pass-through: it has a fixed ceiling (3.3), and anything above the ceiling is a build refusal,
not a grant. The kernel's own check that the supervisor cannot delegate what it lacks
(`SUPERVISOR_DELEGATABLE`) still applies underneath, so the generator cannot widen authority even if
it is wrong; it can only narrow it.

**Corrected 2026-10-16: that backstop covers the privilege word and nothing else.** The kernel
verification (`audits/2026-10-16-kernel-verification.md`, D22) found that `SpawnImage` checks only the
request's privilege bits against what the spawner holds (`privileges_caller_lacks`). The request FLAGS
are minted without any holdings check: `SPAWN_FLAG_REQ_CONSOLE` gives the child CONSOLE_READ, which the
supervisor does not itself hold (`syscall/dispatch.rs:1150-1160`, `task/mod.rs:1791-1792`), and
`SPAWN_FLAG_PEERS_GRANT` makes the name-wired peer caps grantable. Neither is in `SUPERVISOR_DELEGATABLE`.
So for those two flags, and for the send peers themselves, **the generator's ceiling is the only check**:
a generator that set them wrongly would grant, and the kernel would not stop it. That is why 3.3 refuses
both flags by name, and why the generator's refusals are an authority boundary, not a convenience.

---

## 3. Change 1: developer services in a build

### 3.1 The input

One repeatable flag, the same on every build front door: `osdev build`, `osdev run`, `osdev image`,
`osdev test`, and `scripts/board.py` for the other three ISAs.

    cargo run -p osdev -- run --with ../hello
    python scripts/board.py pi4 --with ../hello

The front door turns the list into one generated file the supervisor's `build.rs` reads. The file's
format is internal to the OS build; the flag is the interface. (The CLI invokes the flag. It never
writes the file.)

### 3.2 What the build does with each crate, in order

Each step fails with what failed, why, and what to do next - the CLI relays the message, so the
message has to be worth relaying.

1. **Contract.** `<crate>/contracts/<name>.toml` must exist and pass schema validation (the same
   validator `osdev validate` uses - 4.3 puts it on the build path).
2. **Name.** It must not collide with an embedded service or another developer service. A developer
   service called `fs` is refused, not shadowed.
3. **Gates.** The per-crate gates run on it: no `unsafe` beyond the `service_main` entry, zero raw
   SDK calls with a `gs` equivalent (the one-way rule, with no baseline to hide behind), SPDX tags, the
   print-authority check, the stack-fit check. Those scripts gain a "this crate, at this path" mode.
4. **Compile.** With the OS's pinned toolchain, for the image's target. A crate that pins a different
   toolchain is refused with both versions named, not silently built with the OS's.
5. **Row.** Generated from the contract under the developer grant policy (3.3).
6. **Embed.** The ELF goes into the supervisor exactly as an in-tree service's does.

### 3.3 The developer grant policy

A developer service may be given:

| contract says | row gets |
|---|---|
| `log_write = true` | the log |
| `ipc_send = [names]` | send caps to those names, **from the public set only** (below) |
| `ipc_receive = [name]` or an explicit endpoint (4.2) | its own endpoint |
| `resources.memory.limit` | that limit, as written: there is no ceiling (decided 2026-10-16; question 1 in 7) |
| `placement.core` | a preference, never strict |

**A send peer is authority, not plumbing.** On Godspeed a service holds no file or socket until a server
hands it one, so what a send cap is worth is what its server will do for any client that asks - and
today two of them will do a great deal:

- **`fs` serves the whole disk.** It checks a client's request, not the client's reach: folder
  capabilities (`docs/filesystem-layout.md`, phase 2) are designed and not built, so a send cap to `fs`
  is read and write over every file, `/system/` included.
- **`net-stack` serves the network.** A send cap to it opens sockets to anywhere.

So the floor's send peers are a **public set**, named in this document and in the generator, of servers
whose answers to an arbitrary client cost the machine nothing. **Read against their code, that set is
empty today**, and each obvious candidate shows why:

| peer | what any client may ask it today | so |
|---|---|---|
| `events` | to record a trace event or a metric under any name, and to read back the whole IPC trace ring (`TRACE_OP_DUMP`), every metric (`TRACE_OP_METRICS`) and the recent log lines of every service (`TRACE_OP_LOGS`) | not at the floor: a developer service could read what every other service did and logged, and write events that look like theirs |
| `time` | to raise the clock floor (`OP_FLOOR_SET`), which only ever rises and is persisted to disk, and to start a network fetch (`OP_SYNC`) | not at the floor: one message could push the machine's clock years ahead for good |
| `fs` | read and write over the whole disk | not at the floor until folder capabilities exist |
| `net-stack` | sockets to anywhere | not at the floor until there is a design for what an untrusted program may reach |

So **a developer service starts with no send peers**: the log, its own endpoint, its memory and its
core. That is still a working service - the shell, which may reach any service, can talk to it; it can
answer what it is asked; and the headless verdict (3.7) reads its log - and it is the honest floor until
the servers can tell one client from another. A peer joins the public set by an amendment (5), when its
server answers an arbitrary client safely:

- **`events`** when its read ops are gated or scoped to the asker, and its write ops attribute
  by the kernel's badge rather than the name a client supplies;
- **`time`** when the floor is set by a holder of storage alone, the way it is written and read on the way
  down and up (its own comment calls it "persisted by whoever holds storage"). Being done (2026-10-16,
  branch `fix/time-floor`): `OP_FLOOR_SET` is retired and the floor is `time`'s alone. Once that merges,
  `time` answers only `OP_NOW`, `OP_FLOOR_GET` and `OP_SYNC`, and joining the set is an amendment that
  weighs `OP_SYNC` (a client can make it send an NTP query);
- **`fs`** with folder capabilities, as a grant of the service's own `/system/services/<name>/` and
  nothing wider;
- **`net-stack`** with a design for what an untrusted program may reach on the network.

A request for any peer outside the set is refused with the reason:

    Cannot build developer service hello
    Reason:
      contracts/hello.toml sends to fs. A developer service is given no send peers yet: fs serves
      the whole disk to any client until folder capabilities are built.
    Next:
      Remove fs from ipc_send, or build the service in the OS tree, where its spawn row is reviewed.

**Two findings this check turned up in the OS itself, recorded here rather than in `backlog/`** (5 in
section 7): any service holding a send cap to `time` can raise the persisted clock floor without bound,
and any service holding a send cap to `events` can read every service's traces and log lines. Neither is
reachable by a developer service under this floor. Who holds those caps today, and whether either is a
defect for them, is for the operator to decide.

**Everything else is refused at build time**: every privilege bit, every `hw_*` key, `console_push`,
`resource_mint`, `service_control`, `pci_cfg`, `cpu_clock`, `power_off`, a non-zero `probe_mode`, any
DMA pages, and the two spawn flags the kernel does not check, `[spawn] console` and `[spawn]
peers_grant` (4.2; 2.1 says why these two matter most). The refusal names the key and says why:

    Cannot build developer service hello
    Reason:
      contracts/hello.toml requests service_control. A developer service built outside the tree is
      granted no privilege; what an installed package's requests become is not designed yet.
    Next:
      Remove the request, or build the service in the OS tree, where its spawn row is reviewed.

This is also why the CLI's `driver` template stays unavailable: a driver needs a device grant, and
this floor has none. That is the honest answer until the local grant policy exists.

### 3.4 The supervisor

- A third image table, generated, chained into `spawn_by_image` after `IMAGES` and the USB rows. The
  lookup is by name and names cannot collide (3.2 step 2), so order carries no meaning.
- Developer services start **after** every managed service is up, so a developer service that fails
  at boot cannot delay `fs`, the shell or the console. It is wired to its declared peers the same way
  (`ensure_wired`).
- They are restarted on death, on a separate bounded list beside `MANAGED` (a fixed maximum - four
  proposed - so the no-heap rule holds and `MANAGED_N` is untouched). `is_watched` covers them.
- A developer service that dies repeatedly is given up on with the same backoff and the same visible
  "given up" state as a managed one. It never takes the supervisor down.

### 3.5 Saying what the image is

The supervisor logs, once, before the first developer service spawns:

    supervisor: development image: 1 service built outside this tree: hello 0.1.0

and `status` marks the service as built outside the tree. A release image (the release workflow)
refuses `--with` outright, so no published artefact can carry one.

**Hardware follows from the floor.** Because a developer service reaches no other service, no disk, no
network and no device (3.3), a developer image on a real board can harm nothing a QEMU run could not: the machine's
disk is out of its reach. So a developer image may be flashed (`godspeed run --target hardware`) on the
same terms as QEMU, and the boot line above is what tells anyone at that machine what it is carrying.
**If the floor ever widens - `fs` joining the public set is the first likely case - this is decided
again**, because a developer service with a send cap to `fs` on a real board touches a real disk.

**What the floor does not bound (added 2026-10-16).** The floor limits a developer service's AUTHORITY.
It does not limit what every task can already do, and the kernel verification found two things there
(`audits/2026-10-16-kernel-verification.md`), both open in `backlog/90`:

- **Ungated queries.** Any task may call the ungated `InspectKernel` queries. On x86, query 21 pops a byte
  of the COM2 operator channel, so a floor service can read and delete the commands meant for `control`
  (D23); queries 11 and 17 race on the unlocked CMOS index port and can return a date years ahead.
- **Kernel defects reachable by any task.** A service that faults exercises the self-kill path, which has
  a double-free window (D8), a page-table-root leak (D9, an L1 arena slot on the Pi 2) and a slot-reuse
  race (D10). The restart loop in 3.4 repeats that path for a service that crashes on start, which is why
  its backoff and "given up" state matter: they bound how often a broken developer service drives it.

Neither changes the answer above: an in-tree service has the same exposure, and none of it reaches a
disk, a network or a device. But "can harm nothing a QEMU run could not" is a statement about authority,
and these are the places it is not the whole story. They are fixed in the kernel, not worked around here.

### 3.6 Untying the crate from the tree

- **Linker script.** The stdlib crate (`godspeed`) ships the linker script inside its own package and
  exports its path through Cargo's `links` metadata, so a service's `build.rs` is three lines that
  read it and contain no relative path. `services/user.ld` stays the in-tree copy, or the in-tree
  services read the same metadata - one file, not two.
- **Dependency.** A git dependency on a release tag is documented as the supported way to depend on
  the stdlib from outside, and `osdev test examples` gains one example built that way, so the support
  is tested rather than asserted.
- **Toolchain.** Unchanged: the OS's `rust-toolchain.toml` is the toolchain. A developer crate copies
  it, and step 4 of 3.2 refuses a mismatch.

The linker script and the toolchain pin are two of the seven facts section 11 publishes per release.
Shipping the linker script in the stdlib's package and publishing it in the release file are compatible,
because both carry the same file from the same commit.

### 3.7 A headless run with a verdict

`godspeed test` and CI need to boot an image, watch a developer service, and get a distinct answer.
The `run` front door gains a non-interactive mode: boot, collect the service's log lines, stop at the
first of (every expected line seen / a forbidden line seen / timeout), and exit with **different**
codes for: passed; an expected line never appeared; the service died; the service was never spawned;
the image did not boot; timeout; QEMU unavailable. The expected and forbidden lines come from a small
file in the developer's crate. This is the `WatchSerial` shape `validator.rs` already uses for the
identity suite, made available to one crate.

---

## 4. Change 2: the contract says everything a spawn row says

### 4.1 Row column against contract key, today

| row column | contract key | today |
|---|---|---|
| name | `name` | covered |
| image | - | a build output, correctly not in the contract |
| spawn flags: receive endpoint | `ipc_receive` (partly) | **not derivable** (1.3) |
| spawn flags: console, peers-grant | - | **missing** |
| spawn flags: watched, core-strict | - | supervisor-decided at runtime; correctly not in the contract |
| memory limit | `resources.memory.limit` | covered, reconciled |
| core | `placement.core` | covered, reconciled |
| send peers | `ipc_send` | covered, reconciled |
| privileges | `service_control`, `resource_mint`, `pci_cfg`, `cpu_clock`, `power_off`, `console_push` | **partial**: `ACQUIRE_ANY`, `INTROSPECT`, `FIRE_IRQ`, `REBOOT`, `GPIO`, `SET_CLOCK_FLOOR`, `SET_CLOCK` and `NET_DEVICE` have no key |
| mode | - | **missing** (`probe_mode`) |
| hw class | `hw_device`, `hw_pci_class`, `hw_pci_bar`, `hw_pci_irq` | covered, reconciled |
| DMA pages | - | **missing**: a `match` on the service's name inside `spawn_by_image` |
| restarted by the supervisor | - | **missing**: membership of `MANAGED` |

### 4.2 The additions

All additive, so all within schema v1 (13.5: widening is permissive, narrowing is a v2):

- one boolean per privilege bit that has no key, named like the existing ones
  (`capabilities.acquire_any`, `capabilities.introspect`, and so on);
- `[spawn]` with `endpoint = true` (its own receive endpoint, independent of serving a name - the
  `stdlib-hello` case), `console = true`, `peers_grant = true` and `mode = <integer>`;
- `resources.dma.pages = <integer>`;
- `[lifecycle]` with `restart = true` (the supervisor restarts it).

Each new key is documented in the schema with which row field it corresponds to, so the next reader
does not repeat the 13.6 mistake of reading a declaration as a mechanism.

### 4.3 Reconciled, and validated on every build

- `contract_check.py` reconciles every new key against the hand-written rows, the way it already does
  memory and peers. Its docstring today says "structural fields (elf, probe_mode, has_recv_endpoint)
  are kernel-only and not reconciled"; after this they are reconciled, and the DMA `match` and
  `MANAGED` become checked facts instead of unchecked ones.
- Schema validation moves onto the build path. Today `osdev validate` "is still on no developer build
  path" (the schema's own 2026-10-09 note); a contract that fails the schema can be built and booted.
- **One source of truth for the contract format, and it stays the schema.** `osdev validate` checks a
  contract against `contracts/schema/service.schema.json` with the `jsonschema` crate, which needs `std`
  and the heap. A hand-written `no_std`, allocation-free validator beside it would be a second
  implementation of the same rules, free to drift from the schema - the very double declaration 13.6
  and `contract_check.py` exist to catch. So:
  - **the host side keeps the full validator.** It moves out of `osdev/src/validator.rs` into a small
    host crate in this repository, which osdev and the `godspeed` CLI's `check` both call - one
    implementation on the host, so "manifests are validated the same way everywhere" is true there by
    construction;
  - **the `no_std` side gets only what a Godspeed machine needs, generated, never hand-written.** The
    native package manager does not decide whether a contract is well formed - that was settled when
    the package was built and published. It needs the typed fields and their bounds to read a
    contract it has verified. Those types, and the few range checks they carry, are generated at build
    time from the schema by the crate's `build.rs`, so the schema stays the one place a rule is written;
  - **a test holds them together:** every contract in this tree, and a set of deliberately invalid
    ones, must get the same verdict from both sides.

  The contract format stays this repository's; the CLI depends on it and never forks it.

### 4.4 What this enables and does not decide

Once a contract can say everything a row says, the hand-written rows **could** be generated from
contracts for in-tree services too, which would retire `contract_check.py`'s reconciliation and one of
the copies `backlog/35` counts. That is not proposed here: it moves authority review from a Rust row
to a TOML file across the whole tree, and 2.1 is the reason to decide it separately and deliberately.
Phase A (keys plus reconciliation) is useful on its own and changes no behaviour.

---

## 5. Change 3: `osdev new` and `osdev publish`

- **Remove both subcommands** and their `todo!()` bodies. The developer-project workflow - scaffold,
  package, publish - belongs to the `godspeed` CLI, built on this repository's public contracts.
- **Amend CLAUDE.md 17.** The amendment states, in these terms:
  - osdev is the OS's own build, test and flash tool. Scaffolding, packaging and publishing a project
    are not its work; they are the `godspeed` CLI's, built on this repository's public contracts.
  - osdev exposes **exactly three interfaces for outside tooling**, and names them:
    1. **`--with <crate-dir>`** on every build front door (3.1), with the developer grant floor and its
       public peer set (3.3) as part of its meaning;
    2. **the headless run verdict** (3.7): its input file and its **exit codes**, each one listed;
    3. **the contract validator crate and the generated `no_std` types** (4.3).
  - Those three change **only with a documented migration**: a version, the change, and what a tool
    built on the old form must do - the rule the contract schema already has (13.5). Everything else in
    osdev may change freely, and outside tooling that reaches past the three is relying on an
    implementation detail.
  - Widening the developer floor (3.3) - a privilege, a peer added to the public set - is an authority
    change, and is recorded as an amendment with its reason, never made by editing the generator alone.
- **`GETTING_STARTED.md` step 4** (the seven edits) becomes `--with`, for a service that does not need to
  ship with the OS. The seven edits remain the documented path for one that does.
- `osdev status` and `osdev caps`, the other two stubs, are out of scope: the shell already answers
  both, and nothing here depends on them.

---

## 6. Order, cost, and what each step unblocks

| step | work | unblocks in the CLI |
|---|---|---|
| 1 | change 3 (remove stubs, amend 17) | nothing technical; removes the overlap before both sides build |
| 2 | change 2 phase A: new keys, reconciliation, validation on the build path | nothing yet; makes generation possible |
| 3 | the validator crate | `godspeed check` validating exactly as the build does |
| 4 | 3.6: linker script via `links`, git-tag dependency tested | `godspeed new service` generating a project that builds outside the tree |
| 5 | change 1: `--with`, gates by path, generated rows, supervisor table, boot banner | `godspeed run --target qemu` and `--arch` on all four ISAs |
| 6 | headless verdict (3.7) | `godspeed test` with distinct outcomes |

Steps 1 to 4 are small and independent. Step 5 is the real work and touches the supervisor; it needs
the usual hardware cards, since the supervisor is on every board.

---

## 7. Questions for the operator

1. ~~The memory ceiling for a developer service: 64 MiB, or something else?~~ **Answered 2026-10-16:
   none.** The developer declares the limit they want. A limit is a budget, not a reservation (nothing is
   set aside at spawn), so a large one costs nothing until it is used. A service whose image and stack
   cannot be given memory is not started, which is what happens today and is logged. A running developer
   service that asks for more than its limit is **told no and keeps running** (`AllocDenied`), exactly as
   CLAUDE.md 10.4 defines for every service (decided 2026-10-16, after weighing killing it instead): the
   refusal is a returned error, so it is loud, and the service decides whether to degrade or stop. No
   amendment. **What no ceiling costs, recorded rather than hidden:** budgets are not reservations and are
   not summed against RAM, so one developer service with a very large limit can take every free frame, and
   then other services' allocations and the supervisor's spawns fail too. Services mostly use no heap
   (26.6.1), which keeps this small in practice, and nothing here claims more than that.
2. The maximum number of developer services in one image: four?
3. ~~May a developer image be flashed to hardware?~~ **Proposed answer: yes, on the floor as it stands**,
   because that floor reaches no disk, network or device (3.5). Reopened if the floor widens.
4. ~~`ipc_send` to any service, or a named public set?~~ **Proposed answer: a public set, and today it is
   empty.** Read against their code, `fs` is the whole disk, `net-stack` the network, `events` every
   service's traces and logs, and `time` a clock floor any client can push forward for good (3.3). Each
   joins when its server answers an arbitrary client safely.
5. ~~Backlog items or this document?~~ **Proposed answer: this document stays the single record until
   approved**; nothing is opened in `backlog/` for it.

---

## 8. Ruled out

- **The CLI editing supervisor source.** It works, and it is the CLI acquiring private knowledge of the
  supervisor's tables - the leak the CLI's Core Principle exists to stop. Every supervisor refactor would
  break every developer.
- **Copying or symlinking the developer's crate into `examples/`.** It gets the gates for free and
  makes the developer's project silently part of the OS's workspace, `Cargo.lock` and baselines. A build
  that edits the tree it builds is not reproducible.
- **A runtime loader now.** That is step 2 of `docs/service-ownership.md`, and loading unsigned images
  from disk before that design exists would be exactly the trust shortcut it refuses.
- **Letting a developer contract request anything and granting it.** 2.1. A build tool that turns a
  request into a grant is the "manifest grants" mistake the CLI's contract names first.

---

## 9. Beyond this proposal: what the CLI still needs from the OS

**Added 2026-10-16, for the record. Not proposed here, not approved, and none of it is built.** Changes
1 to 3 make `godspeed run` work for a service. The CLI (`godspeed-cli` 0.1.0, released) still refuses
five more things, each because the OS does not provide what it would need. They are listed here so that
the operator has one place to see the CLI's whole dependency on the OS. Each is the OS's to design, under
its own constitution. The CLI follows; it never works around one.

| the CLI refuses | because the OS lacks | where the OS already says so |
|---|---|---|
| `godspeed run` | developer services in a build | this document, changes 1 to 3 |
| `godspeed install` (on a Godspeed machine) | images loaded from `fs`, signed, with a local grant policy | `docs/service-ownership.md`, step 2; the native side's own needs are section 10 |
| `godspeed new utility` | a terminating task | `docs/stdlib-design.md` 1, option B |
| `godspeed new driver` | device grants for a driver built outside the tree | the supervisor's spawn rows, by device kind |
| `godspeed new app` | public application interfaces | nowhere yet |
| anything but Rust | a stable, documented ABI below `gs` | CLAUDE.md Appendix B (non-normative) |

### 9.1 What the floor (3.3) runs, measured against the CLI's own examples

`godspeed-cli/examples/` holds three services, built for all four ISAs in that repository's CI. Against
the floor as it stands, with the public send set empty:

| example | asks for | under the floor |
|---|---|---|
| `heartbeat` | `log_write` | runs |
| `shout` | `log_write`, `ipc_receive = ["shout"]` | runs; but no developer service can be its client, since `shout` is not in the public set |
| `bootcount` | `log_write`, `ipc_send = ["fs"]` | refused by the build: `fs` is not at the floor until folder capabilities exist |

That is the floor working as intended, not a defect. When the build refuses `bootcount`, the CLI will
report the refusal as the build states it, with the reason this document gives.

### 9.2 Installing: images from `fs`, signed

This is step 2 of `docs/service-ownership.md`, and it is what turns a published package into something a
Godspeed machine can run without being rebuilt. It needs three things. The CLI side of the first is
already built.

- **A verifier at spawn.** The package format is defined and versioned: `manifest.toml`, `manifest.sig`,
  `contract.toml` and one ELF per ISA (the CLI repository's docs/package-format.md). So is the registry's trust
  metadata: root, namespaces with active, retired and revoked keys, snapshot and timestamp
  (docs/trust-metadata.md there, modelled on TUF). Both are verified by `godspeed-package`, a crate
  that is `no_std` and allocator-free and is built for all four targets in CI, so the native package
  manager and a loader can share the exact code the registry and the CLI use. Whether the loader checks
  the publisher's signature, the registry's trust chain, or both, is the OS's decision.
- **A loader:** the supervisor (or a service it trusts) reading an image from `fs` and spawning it.
  `docs/service-ownership.md` is explicit that nothing reads an image from disk today.
- **A local grant policy:** who decides what an installed package's requested capabilities become, and
  how that decision is shown. It is not designed (2, last point but one). The developer grant policy
  (3.3) is the build-time answer for one case. An installed package needs the runtime one.

Then: `godspeed install`, `update` and `uninstall` on a Godspeed machine, and the Stranger Test's second
half.

### 9.3 `utility`: a terminating task

`docs/stdlib-design.md` 1 already sets this out and recommends **option B**: an `Exit(status)` syscall
and one supervisor arm, as a separate reviewed change with a CLAUDE.md amendment (8.2's `CallDeadline` is
the precedent). For the CLI to ship a `utility` template, these are needed on top of the syscall:

- **an entry point that may return:** `fn main()`, or whatever form `gs` settles on, with a status type;
- **supervisor policy:** a finished task is not respawned, and its status goes to whoever started it;
- **a way to start one on demand:** today the utilities are functions inside `services/shell`, so a
  separately built utility needs the shell (or a launcher) to start an image by name, pass it
  arguments, attach its output, and report its status. No argument-passing mechanism exists.

Then: `godspeed new utility`, and `godspeed run` showing the utility's output and exit status. A
terminating task also gives `godspeed test` a natural verdict for a service's tests: they run, end and
report.

### 9.4 `driver`: device grants from outside the tree

`gs::driver` exists, but a device is granted only by the supervisor's compiled spawn rows, by device
kind. The contract's hardware keys (`hw_device`, `hw_pci_class`, `hw_pci_irq`, ...) are stated for
review, and nothing grants from them. The developer floor (3.3) refuses every one of them, rightly. A
`driver` template needs a design for:

- **who approves** a device going to a driver built outside the tree: the operator, explicitly, never a
  contract;
- **exclusivity:** one driver per device, and what happens to the in-tree driver it displaces;
- **discovery:** how such a driver is matched to what `hw-enumerator` found, without the kernel
  resolving anything from a name.

Then: `godspeed new driver`. It is security-sensitive, so the design is the work. The code is small.

### 9.5 `app`: public application interfaces

Nothing exists to build on, and this is the largest of the five. The `console` service owns the
framebuffer, and `console_push` is the keyboard ring the shell reads. An app needs, at minimum:

- a surface it may draw on, shared or arbitrated with the console;
- input delivered to whichever app has focus, rather than to the shell's ring;
- a lifecycle: started, foregrounded, backgrounded, ended. That depends on 9.3.

The first step is deciding what an "app" is on Godspeed. The interfaces come after.

Then: `godspeed new app`.

### 9.6 Other languages

CLAUDE.md Appendix B.2 says any language producing a freestanding ELF "and call via that ABI" can run as
a service, and that C "would need a `sdk/c/` of thin syscall wrappers". Two things stand in the way:

- **The ABI is not documented as stable.** B.2 still says "System V AMD64", but `sdk/rust/src/syscall.rs`
  documents four per-ISA conventions (x86-64 traps with `ud2`, rax plus rdi/rsi/rdx; aarch64 uses
  `svc #0` with x16; riscv64 `ecall` with a7; armv7 `svc #0`, r0-r3). A non-Rust service would also
  have to reproduce the service-context page and its magic, the capability-slot conventions, and the
  `user.ld` layout, none of which is a public contract.
- **A C interface to `gs` itself** (a header and a static library) would be safer than raw syscall
  wrappers: it keeps the stdlib's error handling and outcome-unknown semantics, which raw calls lose.

B.2 rules Go (and other languages with substantial runtimes) out of scope as multi-month ports. The CLI
agrees, and its documentation says so. Then: a C or C++ build backend in the CLI. Packages and publishing
do not change, because the package format does not care what compiled the ELF.

### 9.7 Suggested order

1. **This document, changes 1 to 3:** `godspeed run` for services. Everything below needs something
   that runs.
2. **`Exit` (9.3):** utilities. It is the smallest kernel change on this list, and the most useful.
3. **Images from `fs`, signed (9.2):** installing. The verifier exists.
4. **Device grants (9.4):** drivers.
5. **Application interfaces (9.5):** apps. Design first.
6. **A documented ABI (9.6):** whenever a second language is wanted. It is independent of the rest.

For each step, the CLI's side is small once the OS's exists: a template, its CI build, and `run` or
`install` support. A kind the OS cannot run stays refused until then, with the reason and a pointer here.

---

## 10. The native `godspeed` utility (the other half of the vocabulary)

**Added 2026-10-16. A design note for the next piece of work, not a proposal: nothing here is approved or
built.** The CLI's contract splits the `godspeed` vocabulary in two (the CLI repository's CLAUDE.md, "Host CLI and
Native Package Manager"): the host CLI creates, builds, packages and publishes on a workstation, and the
**native package manager** runs on a Godspeed machine and owns

    godspeed search <query>     godspeed info <package>     godspeed list
    godspeed install <package>  godspeed update <package>   godspeed uninstall <package>

It is "an ordinary Godspeed program on public interfaces: no privileged kernel knowledge, no private
supervisor protocol", and installing "never grants a requested capability by itself". The CLI side of
the format is done: `godspeed-package` (`no_std`, no allocator, built for all four targets) defines the
package, the manifest, the registry index and the trust metadata, and verifies all of them. This section
is what the OS has to settle before the native side can be built, and what it can do before 9.2 exists.

### 10.1 What it can do on today's OS, and what it cannot

| command | needs | today |
|---|---|---|
| `search`, `info` | to read the registry's index and trust metadata, and verify them | transport (10.3) and the clock (10.4) are built; what remains is reading and verifying the metadata (10.6); no kernel change |
| download and verify a package | the above, plus a place to keep it (10.5) | the same; no kernel change |
| `list`, `uninstall` | a record of what is kept, and deleting it | possible once something is kept |
| `install`, `update` (running the package) | a loader and a local grant policy (9.2) | blocked: nothing reads an image from disk |

So a first version can be **fetch, verify, keep, list and remove**, and it must say plainly that a kept
package is not installed: the machine cannot run it until 9.2 exists. The CLI's contract already forbids
presenting a development convenience as installation; the same holds here.

**Built, 2026-10-16: `godspeed registry` (with `add`, `use` and `remove`), `godspeed config`, `godspeed
config set` and `godspeed check`** (`services/godspeed-pm`, `utilities/61_godspeed.md`), QEMU-verified and,
for `check`, hardware-verified the same day on the T630 and the Wyse 5070, and on 2026-10-17 on the Pi 2,
the Pi 4 and the VisionFive 2 Lite. The settings of 10.8 and the transport of 10.3, end to end: the file written on first start, a checked `set` that keeps the rest of the file, and the registry
reached over TLS verified to the roots folder. Three things were missing on the way and were added: an outbound TCP
connection held as a capability (`net-stack` op 23, `docs/tcp-design.md`); `gs::task::wall_clock` and
`gs::task::hw_random`; and RDRAND behind x86's random seam, which had answered `None` - so on a PC whose
CPU has no RDRAND, TLS refuses to run, and says so.

**Built, 2026-10-17 (branch `feat/packages`): `godspeed search`, `info`, `fetch`, `list available | fetched |
installed | updates`, `verify`, `remove` and `trust`** (the operator's names, 2026-10-17: `list` with what to list, `fetched` for a verified
copy that is not installed, an empty list an answer rather than a refusal, `uninstall` kept for when install
exists) -
10.1's first version, fetch, verify, keep, list and remove. Every answer is verified on the machine by the
shared crate's own checks (10.6), in the CLI's order: root 1 by the pin, each rotation by the root before
it, then timestamp, snapshot, namespaces, the package's index, its manifest by digest, the manifest's
signature by a key the namespaces vouch for at the release's time, and the contract and this machine's
binary by digest and size. A fetched package is KEPT under `/system/packages/` (10.5) and says it is not
installed; `godspeed install` refuses with 9.2's reason. QEMU-verified against the real registry and its
package `godspeed/hello` (`osdev test godspeed`, 76/0); no hardware card yet. `utilities/61_godspeed.md` 4a
is the as-built reference. What the work needed, recorded so it is not rediscovered:

- **One kept-alive TLS session per command.** A fetch is a dozen requests; a handshake per request would be
  a dozen handshakes, which is seconds each on a Pi 2. Each answer is read to its exact end
  (`Content-Length` or chunked, `services/godspeed-pm/src/http.rs`, host-tested), and the body is streamed to
  a sink, so a binary goes to disk as it arrives, checked as it goes, and never sits in memory.
- **The stack, not the heap.** Every service has a 256 KiB stack and no heap, and the first QEMU run of
  `info` overflowed it: the trust documents, the TLS session and the command's own buffers measured about
  258 KiB on the deepest path. Each buffer is now sized to what it holds (the snapshot 16 KiB, the root 4 KiB,
  the TLS output 4 KiB), about 216 KiB on `fetch`'s path, and a document past its size is refused by name.
- **The machine's architecture** comes from the target triple the program is built for, whose first word is
  the package architecture's name, so no `cfg` was added.
- **What this machine has seen** is kept per registry under `/system/services/godspeed/trust/<registry>/`,
  so a registry cannot show it an older view than one it accepted; a pin changed since first contact is
  refused until that folder is removed (10.8's rule, made concrete).

### 10.2 Where it lives

Every utility today is a function inside `services/shell` (9.3). This one should not be: it downloads,
hashes and verifies files that can be megabytes, holds the trust metadata while it works, and needs send
caps to `fs` and to `net-stack`, while the shell's stack is already tight on long paths. The natural shape
is **its own in-tree service**, `godspeed`, driven by a thin `godspeed` verb in the shell, the way `ask`
is shaped (`services/ask`, `utilities/60_ask.md`). As an in-tree service its spawn row is hand-written and reviewed (1.1), so its
send caps to `fs` and `net-stack` are an ordinary reviewed grant, not the developer floor's question. It
obeys `utilities/0_conventions.md` like any utility: `help` at every depth, `[q] quit` on every wait over
2 s, `list` and `search` as record producers that pipe, and the actions refusing in a pipe with a sentence.

### 10.3 Transport: the registry is HTTPS, and Godspeed has no TLS

The registry is static files read over HTTPS (the CLI repository's docs/publishing-design.md). Godspeed has TCP
(`net-stack`, the `tcp` and `serve` utilities) and no TLS.

**Decided 2026-10-16 by the operator: TLS, not plain HTTP.** The signed trust metadata would protect
integrity and freshness over plain HTTP, but what a machine fetches would be visible to anyone on the path,
and the operator ruled that out. So the native package manager carries a TLS client.

**What it takes.** None of it is a kernel change; the TLS client lives in the `godspeed` service (10.2):

- **TCP** from `net-stack`, which exists;
- **randomness** for the key exchange: InspectKernel query 19, the hardware random word, which exists;
- **a trustworthy date** for certificate validity, which is 10.4;
- **root certificates**, a handful, built into the service (the registry's custom domain is served by
  Cloudflare, whose certificates chain to one of a few public roots), and a way to update them with the
  utility itself. *As built, they are not compiled in: they are files in a folder, `tls-roots` (10.8),
  given ISRG Root X2 on first start, so adding or removing a root is adding or deleting a file;*
- **fixed buffers**: a TLS record is up to 16 KiB, so about 16 to 20 KiB per connection plus room for the
  server's certificate chain, which fits the no-heap rule (26.6.1) as static arrays.

**The library, as surveyed 2026-10-16:**

| candidate | fits the no-heap rule | the catch |
|---|---|---|
| `embedded-tls` (0.19.0, June 2026) | yes: `no_std`, no allocator except for its `rsa` feature; TLS 1.3 client; verifies the chain with `rustls-webpki` | **Apache-2.0 only**, and this tree is GPL-2.0-only, a combination the FSF considers incompatible. Its verifier checks one trusted root, so a few roots mean a few attempts |
| `rustls` | needs a heap, as far as was checked (its `no_std` mode still uses `alloc`; not confirmed in its source) | the CLI's own TLS; licensing is fine (Apache-2.0 OR ISC OR MIT) |
| BearSSL | yes, by design: no allocation at all | C, TLS 1.2 only, last released 2018; a C toolchain on every port's build |
| a small TLS 1.3 client written here on RustCrypto primitives (MIT) | yes | one cipher suite, X25519, ECDSA P-256/P-384 and the X.509 checking the registry needs: security-critical code nobody has audited |

The licence is the deciding question, and it is the operator's: ask `embedded-tls` for an MIT dual licence
(the cheapest good outcome), accept the cost of writing the client here, or take on C. **Decided
2026-10-16: written here** - the operator asked for "ours", after how Linux does it (below).

**How Linux does it, and the shape that suggests (proposed 2026-10-16, not decided).** On Linux, TLS is a
userspace library: OpenSSL, GnuTLS, BoringSSL or rustls does the handshake and checks the certificates
against a root store the distribution ships (the `ca-certificates` package). The kernel's part is
optional and narrow: kTLS can take over encrypting and decrypting records once a library has finished the
handshake, and since 6.5 a kernel that needs TLS for itself (NFS over TLS) asks a userspace daemon,
`tlshd`, to do the handshake for it. The lesson is the split, and it matches this kernel's scope: TLS is a
library in a service, and the kernel learns nothing about it. What is borrowed is that shape; the code is
not, because OpenSSL and GnuTLS are Apache-2.0 and LGPL and are written for a heap (26.14). The authority
is the specifications: RFC 8446 (TLS 1.3), RFC 5280 (certificates) and RFC 8448, which gives complete
example TLS 1.3 handshakes, byte for byte, to test against.

So the proposed client is **ours, in three parts**:

- **the handshake and the record layer, written here** from RFC 8446: client only, TLS 1.3 only,
  the cipher suite RFC 8446 makes mandatory, TLS_AES_128_GCM_SHA256, and X25519, with fixed buffers;
- **certificate checking by `rustls-webpki`**, the verifier `rustls` itself uses: `alloc` and `std` are
  default features it can be built without, its crypto is pluggable, and its licence is ISC (confirmed
  in its crate metadata, as is its `untrusted` dependency's), which GPL-2.0-only can include;
- **the primitives from RustCrypto** (AES-GCM, SHA-256, HKDF, X25519, ECDSA P-256 and P-384), taken under
  their MIT licence.

ECDSA only, because RSA verification without a heap is the hard part, and an ECDSA-only client limits the
registry to an ECDSA certificate chain. **Checked 2026-10-16 against the live registry**
(`godspeed-registry.bankole-422.workers.dev`, the temporary address the CLI's README names), with
`openssl s_client`, offering exactly what this client would - TLS 1.3, TLS_AES_128_GCM_SHA256, X25519 and
ECDSA signatures only - and trusting ISRG Root X2 alone:

| depth | certificate | key | signed with |
|---|---|---|---|
| 0 | `bankole-422.workers.dev` (valid to 2027-01-14) | ECDSA P-256 | ECDSA SHA-384, by YE2 |
| 1 | Let's Encrypt YE2 | ECDSA P-384 | ECDSA SHA-384, by Root YE |
| 2 | ISRG Root YE | ECDSA P-384 | ECDSA SHA-384, by ISRG Root X2 |
| 3 | ISRG Root X2, self-signed, the trust anchor (valid to 2040-09-17) | ECDSA P-384 | - |

The handshake completed and the chain verified, and the server proved its key with ecdsa_secp256r1_sha256.
The server also sends ISRG Root X2 cross-signed by the RSA root X1; with X2 trusted directly that RSA
signature is never checked. So the client needs ECDSA verification on P-256 (the handshake) and P-384 (the
chain), SHA-256 and SHA-384, and no RSA at all. **Two caveats:** the registry moves to a custom domain,
whose certificate Cloudflare may issue from a different authority, so this is checked again when it does
(and the root list holds whatever anchors that chain needs); and a Let's Encrypt leaf lasts 90 days, which
is fine because the client pins roots, never leaves. Writing the handshake here makes it security-critical code
that nobody has audited, so it is tested against RFC 8448's traces, fuzzed as a parser, and kept to the one
peer it serves.

**Built, 2026-10-16: `sdk/tls` (`godspeed-tls`), a library, no service uses it yet.** The three parts
above, as proposed. `no_std`, no heap, no `unsafe`; the caller owns the socket and every buffer, and hands
the client one record at a time. What it was checked against:

- **RFC 8448 section 3, byte for byte**: every secret and key, and the client's Finished, application
  record and close_notify come out as the RFC's own records.
- **Refusals**, each shown to fire: a ServerHello that picks anything not offered, a HelloRetryRequest, a
  TLS 1.2 server, an extension never offered, handshake bytes after a key change, an altered Finished, an
  altered certificate the verifier accepted (the Finished still catches it), a CertificateRequest, an
  oversized message, application data before the handshake.
- **The verifier, on the registry's real chain** (captured 2026-10-16) pinned to ISRG Root X2: accepted,
  and refused for a wrong name, an expired or not-yet-valid clock, another root, a missing intermediate,
  a flipped signature bit; a CA used as the server, an RSA certificate, a P-256 key under the P-384
  scheme.
- **Mutation**: 20,000 mutated server handshakes, resealed so each reaches the parsers, and 20,000
  mutated encrypted records. None panics and none that changed a byte connects.
- **Live, from a host harness** (not committed): against `openssl s_server` 3.5.7, which counts the
  handshake as finished; and against the registry itself, verified for real - chain to ISRG Root X2,
  CertificateVerify by the leaf's key - answering HTTP 200. Pinned to the wrong root it is refused, and a
  wrong name is refused with an alert openssl receives (alert 42).
- **Device builds**: all four targets; the emitted code of every crypto and certificate crate holds no
  SIMD instruction, because the x86 kernel saves no SSE or AVX state (`.cargo/config.toml`).

**Not yet**: a live KeyUpdate (unit-tested only; openssl's Windows server would not send one), revocation
(no CRL or OCSP: both need a second fetch and a heap; the leaves this client meets last 90 days), and
the service that uses it, which also measures its stack, since webpki builds the path recursively.

### 10.4 Time: verification needs a trustworthy date

The timestamp role expires daily (the CLI repository's docs/trust-metadata.md), so verifying anything needs the
wall clock to be right. Three facts from this tree bear on that:

- the date comes from the `time` service, whose floor only ever rises and is persisted;
- **any client of `time` could raise that floor without bound** (`OP_FLOOR_SET`, 3.3): one message could
  push the machine's date years ahead for good, and every timestamp (and every certificate, 10.3) would
  then look expired. That fails closed (no install, no forgery), but it is a lasting denial of the whole
  package manager. **Being fixed (the operator agreed, 2026-10-16) on the branch `fix/time-floor`:**
  `OP_FLOOR_SET` is retired, the floor is set by `time` alone from its own file and its own readings, and
  `date sync` has `time` re-read that file when it never loaded. Not merged yet;
- on x86 the RTC read behind `time` races across cores and can return a date years ahead
  (`audits/2026-10-16-kernel-verification.md`, D23), which a lock around the CMOS pair would fix.

A clock that is wrong in the other direction cannot replay an old index, because versions only increase.

**Machines with no RTC already take the time from the network.** `time` runs the NTP exchange itself (every
20 s until it has the time, then hourly), and the Pi 2, which has no clock of its own, gets its date that
way. A machine that can reach the registry can reach a time server, so the package manager gets a correct
date wherever it works at all. What NTP is not is authenticated: it is plain UDP, so something on the path
could send a wrong time. Two things already bound the damage: the floor (a clock can never go back past
the last time this machine recorded), and that a wrong date fails CLOSED in both checks that use it (an
expired-looking timestamp or certificate refuses an install; it never accepts a forged one). Two ways to
do better, for when it matters: NTS (RFC 8915), NTP whose keys come from a TLS handshake, which the TLS
client above would make possible; or treating the registry's signed timestamp as an authenticated lower
bound on the date.

### 10.5 Where a kept package goes

`docs/filesystem-layout.md` 10 reserves package storage for phase 4, "only when package management needs
it". This is that moment. **Built 2026-10-17, as proposed here:** `/system/packages/<publisher>/<name>/
<version>/` holding the verified files as published (this machine's binary only, its manifest, signature
and contract, and an `origin` file), with the package manager the owner (L2) and nothing else writing
there; the layout doc's 10 records it. Uninstall then deletes that folder and keeps the package's own data under
`/system/services/<name>/` and `/users/`, as the CLI's contract requires; a `delete data` is separate and
says what it destroys first. Until folder capabilities (phase 2) exist, the package manager's send cap to
`fs` reaches the whole disk like every other `fs` client; that is acceptable for a reviewed in-tree service
and is the reason it must not be a developer service.

### 10.6 The shared crate

`godspeed-package` is a library, not a program and not a package: it is the code that defines the package
format and verifies it - the manifest, its signature, the file digests, the registry index and the trust
metadata. The packages themselves are what contributors publish to the registry (the files in R2); the
`godspeed` utility is the program on the machine. The CLI already uses this library to sign and check what
it publishes, and the native utility would compile it in to verify what it downloads, so the same code
does both. It lives in the CLI's repository (`crates/godspeed-package` there). Where it should live once
this repository compiles it too, three options, the operator's to choose:

1. a git dependency on a tagged release of `godspeed-cli`, the pattern 3.6 proposes for the stdlib in the
   other direction;
2. the crate moves to its own repository, depended on by both;
3. the crate moves into this repository, beside the contract validator of 4.3, and the CLI depends on it.

Option 3 matches 4.3's rule that the format a Godspeed machine verifies is defined where the machine is;
option 1 is the smallest change. Whichever is chosen, `godspeed-package`'s own open question is answered
here: "how the native side reads the text without a heap" (the CLI repository's docs/package-format.md, open question 1)
is decided when the native package manager is, and that is now.

**As built, 2026-10-17: option 1**, the smallest, taken to build the first version and easy to change: a
git dependency on the CLI repository at tag `cli-v0.2.0` (`services/godspeed-pm/Cargo.toml`). The choice
between the three is still the operator's; moving to option 2 or 3 changes one line of that file. Two
things found doing it:

- **Cargo reads every `Cargo.toml` in a git dependency's repository**, and the CLI's two templates hold
  `name = "{{name}}"`, so every build of this repository printed two `error:` lines (harmless: the build
  finishes). The CLI side renames them to `Cargo.toml.template` on its branch `feat/platform-file`; the
  lines stop once this dependency points at a CLI release with that change.
- **The open question's answer:** the native side reads the index and the manifest with its own heapless
  line reader (`services/godspeed-pm/src/toml_lines.rs`, host-tested) into the crate's own borrowed types,
  and runs the crate's own `validate()` on them, so the rules stay the crate's. The trust documents need
  nothing: the crate's `trust` module already reads them without a heap.

### 10.7 What it needs from the kernel

Nothing, for 10.1's first version: no syscall, no capability, no MISCIS change. It is a client of `fs`,
`net-stack` and `time` like any other service. `install` proper needs 9.2, and 9.2 is where the kernel and
supervisor questions are. The suggested order in 9.7 gains a step between 1 and 3: **the native package
manager's fetch, verify, keep, list and remove**, once 10.3 and 10.4 are decided.

### 10.8 Configuration: nothing that can change is compiled in

**Decided 2026-10-16 by the operator.** The registry's address, and every other parameter that can change,
lives in a file on the machine, so changing one never means rebuilding a binary or an image.

**The file** is `/system/services/godspeed/config`, in the service's own folder (`docs/filesystem-layout.md`,
L2): one `key value` per line, `#` for a comment. The utility reads it at the start of every command, so an
edit takes effect at the next one; there is no reload step to forget. A missing file is written out with
the defaults on first start, so the file always shows what is in effect, and `godspeed config` prints every
value with where it came from (the file, or the default because the key is absent). Nothing is a hidden
default (26.4).

| key | default | what it is |
|---|---|---|
| `registry` | `https://godspeed-registry.bankole-422.workers.dev` | where packages are read. `https://` only: anything else is refused with the reason, because the transport is TLS (10.3) |
| `trust-root` | `sha256:d769d1a64c3105c8da0ffcc2f54aba1c3a16ce4ae707a6d206556f08824a146d` | the registry's trust root, pinned on first contact (the CLI README's own pin). After first contact the root the machine reached is remembered, and root rotation happens in-band, signed |
| `tls-roots` | `/system/services/godspeed/roots/`, holding ISRG Root X2 | the certificate authorities TLS accepts, one file per root. Adding a CA is adding a file (10.3's table says why X2 suffices today) |
| `timeout-secs` | 30 | how long a request may go with NOTHING moving on its connection before it is abandoned and said so (`[q] quit` still applies). A download that is still arriving is never cut off by it, whatever its size |
| `max-package-mib` | 64 | the largest package the utility will download, so a hostile index cannot fill the disk |

**Changed 2026-10-16 by the operator: several registries, exactly one in use.** The `registry` and
`trust-root` rows above became one line per registry - `registry <name> <url> <sha256:root>` - and a
`use <name>` line, managed by `godspeed registry` (`add`, `use`, `remove`) rather than `config set`.
There is no priority between registries, because only the one in use is ever asked; each keeps its own
signing root, so switching never trusts one registry with another's key; and an installed package will
remember the registry it came from, so switching does not change where it updates from
(`utilities/61_godspeed.md` 2). A file in the form above still reads, as one registry called `default`,
and is rewritten in the new form by the first registry change. `trust-root` as a key survives only in
that old form.

**Edited through the utility**, not only by hand: `godspeed config set <key> <value>` checks the value first
(a URL that parses, a fingerprint of the right form, a folder that exists) and refuses with the reason, so a
typo is caught when it is made rather than at the next install. The file stays plain text, and `edit` works
on it too.

**What a hostile edit could do, recorded rather than hidden.** Until folder capabilities exist
(`docs/filesystem-layout.md` phase 2), any service with a send cap to `fs` can write this file. Changing
`registry` gains it nothing: the pinned trust root refuses metadata the registry did not sign. Adding a TLS
root lets it read what the machine fetches (confidentiality), not change it, for the same reason.
Changing `trust-root` is the real one: it is equivalent to trusting another registry, so
`godspeed config set trust-root` asks `[y/N]` and shows both fingerprints, and the utility refuses a file
whose pin disagrees with the root it has already reached until the operator confirms. **As built
(2026-10-16), `config set trust-root` is refused outright** and the key is changed by editing the file:
nothing reads the pin until the metadata check exists, and a `[y/N]` needs the console, which a foreground
program's answer table does not have. The confirmation is built with the check that gives it a meaning. The full answer is
folder capabilities, where only the package manager can write its own folder.

**The same rule for the parameters other services compiled in.** `time`'s NTP server was a constant
(162.159.200.123, `time.cloudflare.com`) in `services/time/src/main.rs`. Under this rule it
moves to `/system/services/time/ntp`, read by `time` the way it reads its floor, with the constant as the
default. That is a change to a running service, so it is its own reviewed change (QEMU first, one per
flash), not part of this document. **Built 2026-10-16** (`services/time/src/ntp_file.rs`,
`utilities/2_date.md` 4.1): one IPv4 address, re-read at every `date sync`, a missing file written with
the default, and a file that does not name one address refused by line rather than replaced by the default.

**The TLS library takes all of this from its caller.** The server name, the root certificates and the
current time are arguments, never constants, so the library has no address in it to change.

---

## 11. The platform file: what a release says to developer tooling

**Added 2026-10-17 as a proposal; approved the same day (11.5), and the OS side is built:**
`scripts/platform_file.py` writes the file in the release workflow and checks it on every `osdev build`
(11.4). The first release to carry it is the next one; the CLI's side is its own repository's to release
after that (its `backlog/01`). The
`godspeed` CLI no longer needs a clone of this repository (`godspeed-cli` 0.2.0, the CLI repository's
docs/no-checkout.md). The first time it needs a release, it fetches six files from the release's tag,
resolved first to one commit, and caches them with their digests. That works today with nothing changed
here, but it reads this repository's own build inputs at their paths. Moving `services/user.ld`, or
renaming a key in `.cargo/config.toml`, would break every installed CLI. Those files are not a contract,
and this section proposes the contract. One fact the CLI needs is in none of the six files at all, so
it can only be hard-coded today (the last row of 11.1).

### 11.1 What the CLI reads, and from where

| fact | read today from | why the CLI needs it |
|---|---|---|
| the OS version | `Cargo.toml`, `[workspace.package] version` | the stdlib tag a new project depends on |
| the stdlib crate exists and is named `godspeed` | `stdlib/rust/Cargo.toml` | the generated dependency |
| the toolchain pin and the targets the OS builds | `rust-toolchain.toml` | `--arch`, `--all`, and the project's own pin |
| per-target compiler flags | `.cargo/config.toml` | building a service the way the OS builds its own |
| the service linker script | `services/user.ld` | the same, at link time |
| the contract schema | `contracts/schema/service.schema.json` | `godspeed check` |
| the entry symbol and the link arguments | each service's own `build.rs` (`-T` the linker script, `--entry=service_main`) | linking at all: the kernel starts a service at `service_main` |

The last row is the reason a file is better than a list of paths. No file holds it: every service's
`build.rs` repeats it, so the CLI cannot fetch it and must hard-code `service_main`. Renaming the entry
point here would build and boot every service in this repository and break every service the CLI builds,
with nothing on either side to say so.

Only the targets `rust-toolchain.toml` lists are platform targets. `.cargo/config.toml` also carries
flags for the stub ISAs (`riscv32`, `loongarch64`, `s390x`), which no release builds; they are the
arch-boundary demarcation test, not something a developer can target, and the file leaves them out.

### 11.2 The proposal

The release workflow, which already builds the images and writes `SHA256SUMS-<tag>`, also writes and
attaches **`godspeed-platform.toml`**: one versioned TOML file carrying the seven facts above. The linker
script and the schema are carried as strings, not as paths. Its digest goes in `SHA256SUMS-<tag>` like
every other asset, and the workflow's existing rule applies to it: a published release's assets are
never replaced.

    format = 1
    os = "0.28.0"
    stdlib = { crate = "godspeed", tag = "v0.28.0" }
    toolchain = { channel = "nightly-2026-05-16", targets = ["x86_64-unknown-none", ...] }

    [link]
    entry = "service_main"
    script = "user.ld"          # a key of [files]

    [rustflags]
    "x86_64-unknown-none" = ["..."]

    [files]
    "user.ld" = """..."""
    "service.schema.json" = """..."""

Illustrative only: the exact keys are decided when it is built. `format` gets the compatibility policy
the package format already has. A new key is ignored by an older CLI, and a removed or renamed one is a
new format number.

**It is generated from the files it describes, never written by hand**, in the same workflow step and
from the same commit that built the images. There is still one source for each fact, the file the OS
builds from, and no second declaration to drift from it. That is CLAUDE.md 13.6's rule about contracts
applied to the platform. The entry symbol is the one fact with no single source today; the generator
reads it from the services' `build.rs` files and refuses to write the file unless every one of them
names the same symbol and the same linker script, so a disagreement there fails the release rather than
being published as whichever service was read first.

**What it does not do.** It grants nothing and describes no service. It needs no kernel, MISCIS,
supervisor or syscall change. It changes no file it describes, and the OS's own build reads them exactly
as before. It is not signed beyond the release's checksums; signing OS releases is a separate decision,
and this file would be covered by it.

### 11.3 Old releases

v0.28.0 and every earlier release have no platform file, and their assets are not changed after
publication. So the CLI uses the file when a release has one, and otherwise falls back to the six files
at the tag, as it does today. It drops the fallback when no supported release lacks the file. That is
the CLI's own change, behind its src/kit.rs, and needs nothing further here.

### 11.4 What it unblocks, and what it costs

- **The CLI** reads one published file per release instead of six private ones. This repository can
  move or reshape its build files freely, as long as the generator keeps producing the same facts.
- **Other tools**, such as an editor plugin or a second CLI, get the same facts the same way.
- **The cost:** one script, `scripts/platform_file.py`, with two modes. The release workflow runs it to
  write the TOML. With `--check` it writes nothing and fails if any fact cannot be found, or if the
  services disagree about the entry symbol or the linker script; that mode goes in `osdev`'s
  `EXTRA_CHECKS`, so a rename fails the next local build rather than a developer's machine. Not in
  `build.yml`: that workflow is manual-only and paused, so a check there would run when someone
  remembers it, which is how the documentation checkers once ran only at release (CLAUDE.md 5).
  Small, and independent of changes 1 to 3: it can land first.

### 11.5 Question for the operator

1. Should the release workflow publish `godspeed-platform.toml` (11.2), starting with the next release?

   *Recommended: yes, with the entry symbol (11.1) and the `--check` mode in `EXTRA_CHECKS` (11.4). It
   needs no kernel change, grants nothing, and is the one item here the CLI is already waiting on.*

   **Decided 2026-10-17 by the operator: yes, as recommended.** Built the same day.

### 11.6 As built

The file, as `scripts/platform_file.py` writes it for 0.28.0 (the two `[files]` strings shortened here):

    format = 1
    os = "0.28.0"
    commit = "<the full commit the release was built from>"

    [stdlib]
    crate = "godspeed"
    tag = "v0.28.0"

    [toolchain]
    channel = "nightly-2026-05-16"
    targets = ["x86_64-unknown-none", "armv7a-none-eabi", "aarch64-unknown-none", "riscv64imac-unknown-none-elf"]

    [link]
    entry = "service_main"
    script = "user.ld"          # a key of [files]

    [rustflags]
    "x86_64-unknown-none" = ["-C", "relocation-model=static", "--cfg", "aes_force_soft", ...]
    ...                         # one line per target above, and no others

    [files]
    "user.ld" = "..."
    "service.schema.json" = "..."

- **Refusals**, each proven by `--selftest`: a `build.rs` naming another entry symbol or linker script
  (42 build.rs files, under `services/` and `examples/`, are read); a listed target with no rustflags; a
  missing file; a linker script not called `user.ld`, since the CLI carries it under that name and a
  rename is a new format, not a quiet change.
- **Read back before it is written:** the rendered text is parsed again (`tomllib`, or the repository's
  subset parser on an older Python) and compared with the facts it was made from.
- **Strings are TOML basic strings**, escaped, so the linker script and the schema survive any character
  they contain.
- **Where it runs:** `osdev build` (`EXTRA_CHECKS`), and the release workflow's Package step, into `dist/`
  before `SHA256SUMS-<tag>` is written.

## 12. Installing: a fetched service started by the supervisor (decided and built 2026-10-17)

The operator's four decisions, asked when `godspeed install` was next: **the supervisor loads it**; **the operator
approves peers**, never a privilege; **utilities get option B** (9.3: an `Exit(status)` syscall); **drivers and
apps stay design only** (9.4, 9.5) until a design is approved. This section is the first of them, which is 9.2.

### 12.1 What happens

1. `godspeed install <pkg>` - `godspeed-pm` checks the fetched copy from the disk (`check.rs`, the code `verify` runs)
   and answers the send peers its contract requests.
2. The shell asks once per package, naming all of them and that no privilege is given; `yes` answers in advance,
   `grant <peer>,...` gives a subset.
3. `godspeed-pm` writes the answer to the package's `grant` file, adds `publisher/name version` to
   `/system/packages/installed`, and sends the supervisor `supcmd::INSTALL`.
4. The supervisor runs the same `check.rs` again (by `#[path]`), reading the binary ONCE into a `Region` as it is
   hashed, narrows the grant to the contract, and starts those bytes with `SpawnImage`: privilege 0, the approved
   peers, `SPAWN_FLAG_WATCHED`, the contract's memory limit up to 64 MiB, under the name `publisher/name`.
5. At boot it starts what the list names, the same way, after `supervisor: ready`; a respawned supervisor adopts
   what is still running. A death is restarted from the disk up to 5 times a boot.

`upgrade` is fetch-then-install of the newest release; `uninstall` takes it off the list and `supcmd::UNINSTALL`
stops it, forgotten first so its death notice restarts nothing. The fetched copy stays.

### 12.2 What it needed

- **`Region`** (`sdk/rust/src/memory.rs`), the task's own `AllocMem` pages as a slice: a fourth permitted SDK
  module (CLAUDE.md 18.1 amendment). One 1 MiB region, asked for at the first install and reused; no free exists.
- **Three supervisor commands** (`supcmd::INSTALL`, `UNINSTALL`, `INSTALLED`) and `supervisor` as a peer of
  `godspeed-pm`.
- **`check.rs`** shared by both, so "intact" means one thing; `godspeed verify` uses it too, and now also checks the
  signing key's standing in the trust record, which it did not before. `fetch` records the release's publication
  date in `origin`, so a key retired since still vouches for a release published before.
- **String arrays** in the heapless TOML reader, for a contract's `ipc_send = [...]`.
- No kernel change and no syscall. The CLAUDE.md 14.1 amendment records the widening: the supervisor now starts code
  read from the disk, signed.

### 12.3 What is not closed

The trust record and the grant file live on `/system`, which any `fs` holder can write (folder capabilities are
`docs/filesystem-layout.md` phase 2, not built). So the check proves the image is what a key IN THIS MACHINE'S RECORD
signed, and the grant is narrowed to what the signed contract requests - but a writer to `/system` can change the
record or widen a grant up to that. The supervisor cannot tell an operator's answer from a forged one; it can only
refuse anything beyond what the publisher signed. Also unchecked offline: whether the registry has since revoked the
key - `godspeed list updates` and every fetch refresh the record, and a revoked key is refused once it is there.

### 12.4 Utilities (built 2026-10-17)

Option B of 9.3, as the operator chose: `Exit(status)` (syscall 57, CLAUDE.md 14.5 amendment) ends the caller
by the self-kill path, and the death notice says `E` and the status. `gs::program` is what a utility is written
on: `args` (the command line, the first message on its endpoint), `Output` (a `fmt::Write` sent to the shell a
message at a time) and `finish(status)` (the end byte, then `Exit`). The package format gained the kind
`utility` (the CLI crate, format 1), and the CLI a `utility` template.

- **Install** checks a utility and records it, and starts nothing; boot does the same.
- **Running it**: an unknown command, or a pipe's first stage, that names an installed utility is started
  through the supervisor's SPAWN, which checks the copy from the disk and spawns it with the shell first and
  its grant after. The shell feeds it its arguments, drains it as any stage, gives it a second to end itself,
  and reads how it ended from the supervisor's death record (`supcmd::DEATH`, which keeps 32 bytes of a name
  now, not 16, so `publisher/name`s do not collide).
- **Never restarted**, whether it exited or was killed; one at a time.
- `godspeed/hello-utility` is the first, published 2026-10-17. The registry was redeployed with the crate that
  knows the kind, with the operator's approval.

## 13. Drivers and apps from packages: a design for approval (2026-10-17, not built)

The operator's fourth decision was **design only** for these two kinds: no template, no package, no OS change
until this is approved. What follows is a proposal. 9.4 and 9.5 named what each needs; this answers them using
what install (12) already built.

### 13.1 `driver`: a package granted one device, by the operator, by class

**What a driver is given.** Everything a service is, plus ONE device the operator chose. Never an address, an
IRQ or a BDF: the package's contract names a device CLASS, exactly as an in-tree driver's spawn row does
(`hwclass`: a PCI class code, or a USB vendor and product for a USB device). The kernel resolves the window,
the arena and the vector against its own bus scan, as it does today (14.1's step C amendment), so a driver
package can never name a register the kernel did not find.

**Who approves, and how it is shown.** `godspeed install` for a driver adds one question after the peers:

```
vendor/rtl8139 0.1.0 asks for the device: PCI class 02:00 (network), found here at 00:03.0
  now driven by: nic-driver. Installing it stops nic-driver for that device. Grant it? [y/N]
```

`yes` answers it like the rest: what is approved is what was named. A driver whose class matches nothing on this
machine installs and stays down, said so.

**Exclusivity.** One driver per device. The supervisor keeps, per device, which driver holds it; a package
driver granted a device REPLACES the in-tree driver for that device only - the supervisor stops it and does not
restart it while the package holds the device - and `uninstall` gives the device back, restarting the in-tree
driver. Nothing about the in-tree roster changes for any other device.

**Trust posture, stated as 6.4 does.** A driver granted a DMA arena is trust-critical on a machine that cannot
confine it, and only `xhci` and `audio-driver` are confined today, x86 only. So the install question says it,
from the same fact the boot line reports: "no IOMMU confines this device: a driver for it can reach all of
memory" - and on such a machine a DMA-class package is refused unless the operator answers a second question.
A register-only class (no arena) carries no such warning.

**What it needs, and costs:**
- A contract key the supervisor reads (`[device] class = "02:00"`, or `usb = "0bda:8179"`), validated by the
  schema and narrowed like peers: the grant is never wider than the contract.
- The supervisor: a per-device holder, and a spawn path that fills `hw_flags`/`bdf` from the approved class
  (the fields `spawn_by_image` already sets for in-tree drivers). No kernel change: the kernel already grants by
  class from the request, and refuses raw addresses.
- The CLI: `godspeed new driver` on `gs::driver`, refused until this ships, as `utility` was.
- Open: what a USB-class driver's host does (it must hand the device to the package, as `dwc2`/`xhci` hand the
  dongle to `wifi-usb` today - that protocol, `usbfn`, is the precedent).

### 13.2 `app`: a program with a screen of its own, while it has focus

**What an app is.** A utility that, while it runs, owns the console - the screen and the keyboard - and gives
them back when it ends. That is exactly what `edit` and `observe` do today as built-ins, and what the
`ConsoleForeground` syscall (40) already provides: a task may claim the input ring and the display, and the
shell gets them back when it releases or dies. So an app is the utility mechanism (12.4) plus one capability.

**The one new grant.** `console` (contract key `[capabilities] console = true`): a send cap to the `console`
service for output, and the right to claim the foreground. Asked at install like a peer, with the reason:
"it will read the keyboard while it runs". A keyboard is a command channel (6.4, SEC-2), so this is the grant
that matters, and it is never given silently.

**Lifecycle.** Started by typing its name, as a utility, and in the foreground until it ends. The shell's
`[q]` cannot reach a program that owns the keyboard, so the way out is the app's own (its quit key) or from
outside it: the shell regains the console when the app's task dies, and `kill` from a second console (the
control channel) ends it.
Recorded rather than solved: an app that hangs holding the keyboard is ended only from outside it, which is
true of `edit` today.

**What it needs:** `gs::app` (claim, draw a grid through `console`, read keys, release - the shapes `edit`
uses, lifted out of the shell), the contract key, the install question, and `godspeed new app`. No kernel
change. Not in scope: windows, more than one app on screen, a graphics surface beyond the console's grid.

### 13.3 Questions for the operator

1. Driver: replace the in-tree driver per device (proposed), or refuse a device an in-tree driver holds?
2. Driver: refuse a DMA-class package on a machine with no IOMMU unless asked twice (proposed), or refuse it?
3. App: is "a utility that owns the console while it runs" the right definition of an app for now?


### 13.4 Four kinds, decided (2026-10-17)

Service, utility, app and driver stay four kinds. Merging app into utility was considered - today an app is a
utility whose grant names the console - and rejected by the operator, because the line between the kinds is not how
long a program runs but who governs its life:

| Kind | Started by | Ended by | Holds |
|------|------------|----------|-------|
| service | the system, at boot | nobody: restarted if it dies | its peers |
| utility | the operator, by typing it | itself, when the job is done | a stream: what it writes can be piped |
| app | the operator, by typing it | the operator, by closing it | the screen and the keyboard |
| driver | the system, at boot | nobody: restarted if it dies | its peers and one device |

An office application runs for as long as it is open and is still an app, not a utility. What it will need and a
utility never does - being backgrounded with its work open and brought back, focus among several, closing with "you
have unsaved changes" - is the app lifecycle 9.5 names, not built yet. That is where the two kinds part, so merging them
now would mean splitting them again, with every app published in between carrying the wrong kind. Underneath, they
share one mechanism (an app is started as a utility is, with the console flag); that is an implementation detail the
package format does not have to say.

## 14. Live handover: a running service replaced without stopping (built 2026-10-17)

The operator asked for Erlang's upgrade-in-place. CLAUDE.md 2.5 forbids changing code inside a running program, and
still does; the 2.5 amendment of this date allows the part a user sees - the service is a new version and never
stopped - built from two isolated programs and an explicit handover.

### 14.1 The sequence

1. `godspeed upgrade` (or `install <name>@<version>` with another version running) asks the supervisor for a handover
   (`supcmd::HANDOVER`).
2. The supervisor checks the new version from the disk, as for every start, and takes a send capability to the RUNNING
   version's endpoint by its name.
3. It starts the new version with `SPAWN_FLAG_HANDOVER` and the handover spawn mode (`gs::handover::MODE`). The kernel
   lets it start beside the one running instance; the name now answers to the new one.
4. It sends the running version the OFFER, with the new version's endpoint capability attached.
5. The running version answers what is already in its queue (`hand_over`'s drain), sends its state (one message), and
   ends with status 0. The supervisor sees the name still alive and does not restart anything.
6. The new version, started in the handover mode, waits up to 10 s for the state (`take_over`), keeping up to two
   ordinary requests that arrive first, and carries on from it.

The same sequence as a picture: time runs down, and the name `godspeed/hello-service` answers to whoever holds the
arrow on the left.

```
 operator      godspeed-pm       supervisor               kernel                v1 (running)        v2 (new)
    |               |                 |                       |                     |                   |
    | upgrade ----->|                 |                       |                     |  serving, key K1  |
    |               |-- HANDOVER ---->|                       |                     |                   |
    |               |                 | check v2 from disk    |                     |                   |
    |               |                 | acquire cap to v1 --->|                     |                   |
    |               |                 | SpawnImage + HANDOVER |                     |                   |
    |               |                 |   mode(K2, successor)>|-- start beside v1 ------------------->|
    |               |                 |                       | name -> v2          |                   | take_over: waits
 name ===========================================================================================> v2   |  (keeps 2 early)
    |               |                 |-- OFFER [K1, K2] + cap(v2) -------------->|                   |
    |               |                 |                       |                     | key == K1? yes    |
    |               |                 |                       |                     | drain own queue   |
    |               |                 |                       |                     |-- STATE [K2,..] ->|
    |               |                 |                       |                     | exit(0)           | key == K2? yes
    |               |                 |<-- death notice: v1 exited ----------------|                   | carries on from
    |               |                 | name still alive (v2): no restart          x                   | v1's state
    |               |<-- [OK, 'H'] ---|                       |                                         |
    |<-- "replaced: live, with no restart"                    |                                         |
```

A wrong key at either step is refused and counted; after three, that side takes no more (14.3). Where either contract
lacks `handover`, the supervisor answers `[OK, 'R']` after an ordinary stop and start instead.

### 14.2 When it falls back

A restart, said in the answer (`replaced: stopped and started`), when the running version's contract or the new one's
does not declare `handover`, when there is no hardware random source for a key, or when nothing is running. The
direction does not decide it: a downgrade between two versions that both declare `handover` is live, and an upgrade to a
version that drops it is a restart. A successor that will not start leaves the running version running. A state that
never arrives (the old version does not answer the offer) is said by the successor, which starts fresh.

**A restart checks before it stops.** The new version is checked from the disk - signature, key, contract, binary -
before the running version is stopped (`Installed::verify`), so a package that would not start leaves the running one
untouched instead of costing a stop, a failed start and a restore. The start checks again, as every start does. Between
the stop and the new version registering its name nothing answers it; a client sees `EndpointDead` and reacquires (14.3).
A restart is never side by side: two instances of a service not written for it could write the same files at once, and
declaring `handover` is how a service says it can overlap with its successor.

**Only a service is replaced live.** A driver never is: a handover runs two instances for a moment, and two drivers
would both program one device (`backlog/91` records how a driver could be, if it is ever needed). A utility runs for one
command, so its next run is simply the new version. An app is reloaded by its user, since a new version may bring a new
interface. So `handover` is the one key a contributor sets, and only on a service: `godspeed check` in the CLI refuses it
on any other kind before it is built, and the shared check refuses it at every start, so such a package is never started
and `godspeed verify` says why.

**An app's notice, recorded and not built.** An app could be told a newer version is installed, and offer to reload as
Chrome does - automatic, nothing to declare. Nothing can deliver it yet: `godspeed upgrade` is typed at the shell, and
the shell waits while an app holds the console, so no upgrade can happen while an app is open. It is pulled in by the
first thing that can upgrade while an app is open - an automatic update, a second session, an upgrade over the network.

### 14.3 Why an offer is checked

The offer arrives on the service's public endpoint, which every client can send to, and the kernel does not say who sent
a message. Unchecked, anyone could make a service hand its state to them and end (found by the commit's security review).
So every handover-capable start carries a random 30-bit KEY in its spawn mode, which only the supervisor can set (only it
calls `SpawnImage`); the offer carries the running version's key and names the successor's; the state carries the
successor's. Each side refuses a wrong one and, after three, stops listening, saying so. A key cannot be guessed in three
tries; a sender who keeps trying can only make a later upgrade fall back to a stop and start, which is loud and no worse
than before handovers.

### 14.4 What it does not do

Capabilities do not travel. A request sent to the old version after its drain is lost with it, reported to its caller as
`OutcomeUnknown`. The state is one message (4088 bytes); more stays on disk. Apps are not covered: an app's handover
would move the screen with the state, and is not built. The kernel's part is one flag (2.5 amendment).

### 14.5 Shown

`godspeed/hello-service` 0.5.0 and 0.6.0 count the seconds they run (0.3.0 and 0.4.0 speak the earlier, unchecked offer, and are refused); upgraded from one to the other in QEMU, the new
version logs `took over, live, at N seconds` with the old version's count, and nothing died or restarted.

## 15. Offline packages: the `.gsp` file (approved 2026-10-18, built; QEMU-verified)

A package can travel as one file and install on a machine with no network: copied to a USB stick, shared
directly, kept for a machine that is never online. The operator asked for it, and for the file to be
trusted through the registry rather than through a new kind of trust.

### 15.1 What a `.gsp` is

`.gsp` stands for GodspeedOS Package. **A slice of the registry, in a file.** It holds the very documents a fetch reads, under the very paths the
registry serves them at:

```text
trust/root/1.toml  trust/root/1.sig   ... every root up to the current one
trust/timestamp.toml  trust/timestamp.sig
trust/snapshot.toml   trust/snapshot.sig
trust/namespaces.toml trust/namespaces.sig
index/<publisher>/<name>.toml
packages/<publisher>/<name>/<version>/manifest.toml
packages/<publisher>/<name>/<version>/manifest.sig
packages/<publisher>/<name>/<version>/contract.toml
packages/<publisher>/<name>/<version>/<arch>.elf     one per architecture it carries
```

So installing from one runs the same code as fetching from the registry. godspeed-pm reads the registry
through a `Source`; the network is one source and a `.gsp` is another, answering each path from the file.
Nothing about what is checked, or in what order, differs between the two.

The layout is a header line, then each entry as a line naming its size and path followed by exactly that
many bytes, then an end line. It is read in one pass for its table of entries (at most 64), and an entry is
read by its offset. Defined once, in the shared `godspeed-package` crate, so the CLI that writes it and
godspeed-pm that reads it cannot disagree.

### 15.2 Why it can be trusted

Every GodspeedOS image pins the default registry's root key. The file carries the chain that root
starts:

```text
pinned root (in the OS image)
  -> later roots, each signed by the one before it
  -> the timestamp, naming the snapshot
  -> the snapshot, naming the namespaces and the package's index
  -> the namespaces: this publisher's key is active
  -> the index: this version was published
  -> the manifest's signature, by that key
  -> the contract and the binary, by digest
```

A machine that has never been online already trusts the first link, so it can check every other one from
the file. No new authority is introduced: the registry vouches for the publisher exactly as it does online.

### 15.3 Freshness: what a file cannot know

The documents are as of the day the file was made. The registry's timestamp is valid for a day, so a file
is checked AS OF the moment its chain was valid - just before its timestamp's expiry - and its age is
measured from there. What that cannot show is a key revoked after the file was made. Three rules:

1. **A newer record on this machine wins.** If the machine has been online since, it holds a newer record
   of the publishers' keys, and that one decides: a revocation it has seen refuses the file.
2. **A recent file installs** and says it was checked offline, with the date.
3. **An old one asks** (older than `offline-max-age-days`, 30 by default, in
   `/system/services/godspeed/config`), and so does any file on a machine whose clock is not set - the Pis
   and the VisionFive have no clock chip:

   ```text
   godspeed/hello 0.3.0 was checked against the registry 41 days ago (2026-09-07).
   A key revoked since then would not show. Install it anyway? [y/N]
   ```

   `yes` does not answer this question, so a script that says `yes` does not accept the risk by accident;
   the word `accept-old` does. An acceptance is recorded with the package and shown on its card
   (`accepted offline, checked as of 2026-09-07`).

**Only freshness is a choice.** A bad signature, a digest that does not match, or a key this machine knows
is revoked is refused with no question.

The next time the machine is online, packages installed from a file are checked against the current
registry; one whose key has since been revoked is said, loudly, and left running - stopping it is the
operator's decision, not the package manager's.

### 15.4 Commands

The name of a package file is never typed when it is made, as nobody names a `.deb` when making one: the flag says
what the file is FOR, and the file is named from the release, `<name>-<version>.gsp`, so a second one never
overwrites the first.

```text
godspeed fetch <pkg>[@version] --offline [--arch <arch>|all]   the CLI, on a workstation: a published release
godspeed package --local                                        the CLI: an UNPUBLISHED one, signed by your key (15.6)
godspeed fetch <pkg>[@version] offline                          on a Godspeed machine, into the current folder
godspeed install <file>.gsp [yes] [accept-old]                  on a Godspeed machine
godspeed inspect <file>.gsp | <pkg>[@version]                   either side: every check installing makes, nothing kept
```

`inspect` takes a file or a package name. Given a name it is the same check against the registry, this machine's
binary streamed through its digest - what `info`, which reads only the registry's metadata, cannot say.

### 15.5 What does not change

The supervisor never reads a `.gsp`. godspeed-pm unpacks it into `/system/packages/<publisher>/<name>/<version>/`,
the layout a fetch leaves, checking as it goes, and from there installing is what it always was: the
supervisor checks the copy on disk at install, at every boot and at every restart.
### 15.6 An unpublished package: a key the operator trusts

A package built with no network has never been published, so no registry has vouched for its key, and 15.2's
chain does not exist. `godspeed package --local` writes the file anyway - its manifest, signature, contract and
binaries, and no registry documents - and the machine installs it only if its OPERATOR trusts the signing key for
that publisher:

```text
godspeed trust add <publisher> <key> [yes]     asks first, saying what it means
godspeed trust remove <publisher> <key>
```

The key is kept in its own list (`/system/services/godspeed/keys`), apart from every registry's record, so neither
can stand in for the other; `godspeed trust` shows it as its own card. A copy installed this way records `trusted
operator` in its origin and is listed as coming from `(operator)`, a name no registry can have.

What it does not have, said wherever it shows: **nothing tells this machine if that key leaks.** No registry can
revoke it; only `godspeed trust remove` does, and the supervisor's check at every start reads the list again
(`check.rs`, `operator_trusts`), so a key removed stops what it signed at its next start. A key trusted for one
publisher vouches for that publisher alone. The list is on `/system`, exactly as protected as every other trust
record (12); the residual there is unchanged.

`godspeed inspect` reads an unpublished file too, on the machine and in the CLI, and says that no registry vouches
for it.

## 16. Dependencies: a package that needs another (approved 2026-10-19; building on `feat/gs-driver`)

The operator asked for this: a service that needs a driver, an app that needs a service, and the package manager
and the CLI both knowing it. Until now a contract named the PEERS a package talks to, and the operator approved
each one, but nothing said a peer had to be INSTALLED. An app naming `alice/notes-store` installed whether or not
that package existed, and started with no capability to it. The operator approved this design the same day.

### 16.1 What a dependency is, and is not

- **A package that another package needs installed and running.** `alice/notes` requires `alice/notes-store`.
- **Not a built-in service.** `fs`, `net-stack`, `block-driver`, `nic-driver` and the rest come with the OS
  release, which a package already names (`[os] release`, "built for 0.29.0"). A package does not depend on them;
  it talks to them, as now.
- **Never hardware.** A package that needs the network depends on nothing, because `net-stack` is built in. A
  package never names a NIC driver: which driver serves a device is the machine's business, decided by the
  device's class.
- **In the manifest, not the contract.** The contract says what a package may DO (its peers, its device class),
  and the operator approves it. The manifest says what it NEEDS, and approves nothing. Kept apart, a requirement
  can never become a grant: installing a dependency still asks about that dependency's own peers.

### 16.2 The format

```toml
format = 2

[[requires]]
name = "alice/notes-store"
version = "^1.2"
```

- **`format = 2` exactly when there is a requirement.** A manifest with none stays format 1. So every package
  published so far, and every new one without dependencies, installs on a 0.29.0 machine unchanged. A reader that
  predates this refuses a format 2 manifest as "a manifest format this machine does not read", which is the true
  reason, rather than as a table it does not know.
- **At most 8 requirements**, with no duplicates and never the package itself.
- **A range is one of three forms**, read by one function in the shared `godspeed-package` crate:
  - `^1.2.0` (`^1.2` and `^1` mean the same): the same major version, and at least this one. For a `0.x`
    version, the same minor (`^0.3` is `>= 0.3.0, < 0.4.0`); for `0.0.z`, exactly that version. That is the usual
    reading, because before 1.0 a minor change is allowed to break.
  - `=1.2.3`: exactly this version.
  - `>=1.2.0`: this version or any later one, later majors included.
- **What may be required: a service or a driver**, something that runs and is talked to. A utility ends and an
  app belongs to the operator, so needing one installed means nothing. A requirement naming one is refused when it
  is resolved, with that reason.
- **A peer that is a package must be required.** Every peer in the contract whose name is `publisher/name` (a
  built-in has no `/`) must be in `[[requires]]`. The CLI's `check` refuses the package otherwise, and the machine
  refuses to install it. A requirement need not be a peer: a service may need a driver running that it never
  sends to.

### 16.3 Resolution, on the machine

Before it asks anything, `godspeed install <pkg>` works out every package the install needs, from the verified
snapshot:

- for each requirement, the newest release that satisfies its range;
- an INSTALLED version that satisfies it is kept, and not fetched again;
- an installed version that does not satisfy it is refused, naming both. A plan never upgrades something the
  operator did not ask to upgrade;
- the requirements of each package in turn, to a depth of 4 and at most 8 packages in one plan. A cycle is
  refused, naming it;
- one version of each package on a machine, so two requirements that no single version satisfies are refused,
  naming both ranges.

Every refusal is a sentence that names the package and the range. Nothing is fetched or installed when the plan
cannot be made.

### 16.4 One question

The operator is asked once, for the whole plan, dependencies first:

```text
gsh> godspeed install alice/notes
alice/notes 1.0.0 needs 1 more package:
  alice/notes-store 1.2.4   a service, starts at every boot   asks for: fs        (needed by alice/notes)
  alice/notes 1.0.0         an app                            asks for: console, alice/notes-store
Install all 2 and grant them? [y/N]:
```

`yes` on the command line answers it, as it does for one package. No installs nothing.

### 16.5 Start order

- **At install**, each dependency is installed and started before the package that needs it, in the plan's order.
  A dependency that does not start stops the plan there: what was installed before it stays installed, the rest is
  not started, and the line says which.
- **At boot**, the supervisor starts installed packages in dependency order, read from their manifests, which it
  checks at every start anyway. A package whose dependency is not running is not started, and says which one.

### 16.6 Uninstall, upgrade, cleanup

- **`uninstall`** refuses a package that another installed package requires, naming that package. Uninstalling
  the dependent first is the way through.
- **`upgrade`** refuses a version that would break an installed dependent's range, naming the dependent and the
  range. Upgrading the dependent itself may bring in a dependency, through the same one question.
- **`cleanup` and `uninstall ... purge`** never delete the fetched copy of a package an installed one requires.

### 16.7 Offline

`fetch <pkg> --offline` (the CLI) writes the dependencies' releases into the same `.gsp`, so one file installs a
package and everything it needs. Installing from a file resolves within the file and what is already installed; a
requirement that neither satisfies is refused, naming it. Nothing in section 15 changes: every release in the file
is checked as a fetched one is.

### 16.8 The CLI

- `godspeed.toml` gains `[requires]`, written the way a developer would: `"alice/notes-store" = "^1.2"`.
  `package` turns it into `[[requires]]` and format 2.
- `check` applies 16.2: the forms, the bound, and that peers which are packages are required.
- `publish` refuses a requirement the registry has no release of that satisfies its range.
- `inspect` and `fetch` say what a release requires.

### 16.9 The order it lands in

The manifest reader is strict, so a format 2 manifest is refused by every reader that predates it: the
registry's Worker, CLI 0.3.0 and GodspeedOS 0.29.0. So, in order: the crate, then the registry (redeployed, which
needs the operator's approval), then the CLI release and the OS release. No package is published with
requirements before all of them read it.

### 16.10 What does not change

No kernel change and no new authority. A requirement grants nothing; peers are still approved one by one, and a
package still gets no privilege (14.1). The supervisor already starts what is installed; it learns an order.
