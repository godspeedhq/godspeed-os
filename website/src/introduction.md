# GodspeedOS

> Small enough to understand. Rigorous enough to trust.

GodspeedOS is a capability-based microkernel operating system written in Rust. Every privileged
action requires an explicit, unforgeable capability. Services are isolated in their own address
spaces. Failures are loud, never silent. Authority is never inherited or ambient - only granted.

The kernel is deliberately tiny: memory isolation, scheduling, IPC routing, capability enforcement,
interrupt routing, and multi-core coordination. Nothing else. Everything above it - drivers, the
filesystem, the shell - is a restartable userspace service. The only unkillable component is the
kernel itself.

## Where to start

This site is a rendered view of the project's own documents. Nothing here is written twice: each
page below pulls the real file straight from the repository, so the site can never drift from the
source (that is [Commandment III](commandments.md) applied to the docs themselves).

- **[The Ten Commandments](commandments.md)** - the human-readable distillation of the whole
  design. Ten laws that bound every decision. Start here.
- **[The Constitution](constitution.md)** - the full specification: the invariants, the capability
  model, IPC, the scheduler, the memory model, the unsafe policy, and the contribution rules. When
  the constitution and the code disagree, the constitution wins.
- **[The Glossary](glossary.md)** - the abbreviations (TLB, DMA, IOMMU, AHCI, BSP, IPI, and the rest).
- **[The Almanac](almanac.md)** - the project's logbook: the days something was learned, in the
  words of the person who learned it. The bugs, the wedges, and the lessons that became commandments.

## How it works, in one breath

A **capability** is a token carrying a resource id, a rights set, and a generation number. Holding
one is the necessary and sufficient authority for the actions it permits. Kill a service and its
endpoint generation is bumped; every outstanding capability to it goes stale at once, and the next
use returns `EndpointDead`. Clients look the service back up by name through the kernel's name
directory, reacquire a fresh capability, and resume - possibly against a new instance on a different
core, which is invisible to them. Identity is stable; location is not.

The **[Design Notes](design/persistence.md)** trace how that model plays out in real subsystems -
the filesystem, the block driver, DMA confinement, naming, the scripting language - and the
**[Forward-Looking](design/networking.md)** notes sketch where it goes next: across a network, and
across a cluster.

## Seeing it run

The **[Gallery](gallery.md)** shows GodspeedOS actually booting and running - captured from the real
system, not mocked up.
