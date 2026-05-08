# Milestone 2 — Memory Management

> Physical frame allocator and page tables working. Kernel can hand out and reclaim memory.

## Frame Allocator

- [ ] Parse memory map from `BootInfo` — identify usable regions
- [ ] `frame::alloc()` returns a free 4 KiB `Frame`
- [ ] `frame::free(frame)` returns a frame to the pool
- [ ] Kernel image and boot data excluded from the free pool

## Page Tables

- [ ] `PageTable::new()` allocates a PML4 frame and pre-maps the kernel region
- [ ] `PageTable::map(virt, phys, flags)` walks/allocates PT levels, sets PTE
- [ ] `PageTable::unmap(virt)` clears PTE and returns the physical frame
- [ ] `PageTable::cr3_value()` returns correct physical address for CR3 load

## Ownership Tracking

- [ ] `memory::ownership` tracks which task owns which frames
- [ ] On task death: all owned frames reclaimed, TLB shootdown issued (§10.5)

## Acceptance

- Kernel can allocate frames and map them into an address space without panicking
- Allocating beyond available memory returns an error, does not corrupt state
