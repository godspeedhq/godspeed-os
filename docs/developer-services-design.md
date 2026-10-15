<!-- SPDX-License-Identifier: GPL-2.0-only -->
# Developer services: building a service from outside the tree into an image

**Status: PROPOSAL, 2026-10-15. Nothing here is built and nothing here is approved.** Written for the
`godspeed` developer CLI (the separate `godspeed-cli` repository), whose contract says its first half -
create, build, test, run in QEMU - "can be built now". Measured against this tree it cannot, without
the CLI editing supervisor source, and that is the boundary the CLI's own contract forbids. This
document is the three OS-side changes that would make it true, and what each one deliberately does
not do.

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
| `resources.memory.limit` | that limit, up to a ceiling (64 MiB proposed - the largest any example takes) |
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
  down and up (its own comment calls it "persisted by whoever holds storage");
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
DMA pages. The refusal names the key and says why:

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

1. The memory ceiling for a developer service: 64 MiB, or something else?
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
