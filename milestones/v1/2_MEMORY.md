# Milestone 2 — Memory Management ✅

> Physical frame allocator and page tables working. Kernel can hand out and reclaim memory.

**Status: COMPLETE — 2026-05-09**

## Frame Allocator

- ✅ Parse memory map from `BootInfo` — identify usable regions
- ✅ `frame::alloc()` returns a free 4 KiB `Frame`
- ✅ `frame::free(frame)` returns a frame to the pool
- ✅ Kernel image and boot data excluded from the free pool

## Page Tables

- ✅ `PageTable::new()` allocates a PML4 frame and pre-maps the kernel region
- ✅ `PageTable::map(virt, phys, flags)` walks/allocates PT levels, sets PTE
- ✅ `PageTable::unmap(virt)` clears PTE and returns the physical frame
- ✅ `PageTable::cr3_value()` returns correct physical address for CR3 load

## Ownership Tracking

- ✅ `memory::ownership` tracks which task owns which frames
- ✅ On task death: all owned frames reclaimed, TLB shootdown issued (§10.5)

## Acceptance

- ✅ Kernel reports **510 MiB free** on boot with 4 cores, no panic
- ✅ Allocating beyond available memory returns `AllocDenied`, does not corrupt state

## Serial output

```
memory: frame allocator ready (510 MiB free)
capability: subsystem ready
ipc: routing table ready
kernel: all cores ready
```

## Notes

- limine-rs 0.6.3 uses `entry.type_: u64` with `limine::memmap::MEMMAP_*` constants (not an enum),
  and `Request::response()` not `get_response()`. HHDM offset is `resp.offset` (public field).
- Bitmap is BSS-zero-init (all frames = used) so usable regions must be explicitly freed during init —
  this is the correct conservative default.
- Intermediate page table frames are `mem::forget()`-ed inside `walk_or_alloc`; they are owned by
  `PageTable` and will be freed during table teardown (Milestone 5).
