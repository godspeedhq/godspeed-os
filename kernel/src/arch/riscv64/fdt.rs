// SPDX-License-Identifier: GPL-2.0-only
//! Flattened Device Tree reader - how this arch learns what machine it is on.
//!
//! **This is the file that keeps RISC-V from being special.** Every board-specific number the port
//! needs - the UART's base and register stride, where RAM starts and how much there is, the PLIC,
//! the timer's frequency, which harts actually exist - is a question the machine already answers, in
//! a blob the firmware hands us in `a1`. Reading it lets `arch::imp` report facts to the neutral
//! kernel without anything above the seam learning that a JH7110 exists. Hard-coding the same
//! numbers would work just as well on this board and teach the kernel a board, which is the thing
//! being avoided.
//!
//! It removes a real bug too, not only a smell. The DTB shipped in the vendor image declares
//! `/memory@40000000` as 4 GiB; the board has 8, and U-Boot patches the memory node from what the
//! SPL detected before passing it on. A parser reading the runtime pointer learns the truth.
//! Constants copied from that file, or from a boot log, would have sized RAM at half the machine and
//! been PLAUSIBLY wrong, which is worse than being obviously wrong.
//!
//! ONE `unsafe`, AT THE BOUNDARY. `from_ptr` turns the firmware's pointer into a `&[u8]` of exactly
//! the length the header declares; everything after is ordinary slice indexing, so a malformed or
//! hostile tree yields `None` rather than a read outside the blob. Same shape as `bootcon`, which
//! takes a `&'static mut [u8]` from the arch and is bounds-checked thereafter.
//!
//! NO HEAP, NO PANICS, BOUNDED WALKS (§26.6.1, §26.6). The parser holds a slice and a cursor; the
//! only state that grows is a fixed cell-size stack, and every walk is capped so a corrupt tree
//! cannot spin the boot forever. Every accessor returns `Option`; there is no `unwrap` and no
//! indexing that can panic.

/// Big-endian u32 at `off`, or `None` if that would read past the blob.
fn be32(blob: &[u8], off: usize) -> Option<u32> {
    let b = blob.get(off..off.checked_add(4)?)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn be64(blob: &[u8], off: usize) -> Option<u64> {
    let hi = be32(blob, off)? as u64;
    let lo = be32(blob, off.checked_add(4)?)? as u64;
    Some((hi << 32) | lo)
}

/// A NUL-terminated string starting at `off`, bounded by the blob.
fn cstr(blob: &[u8], off: usize) -> Option<&str> {
    let rest = blob.get(off..)?;
    let end = rest.iter().position(|&c| c == 0)?;
    core::str::from_utf8(&rest[..end]).ok()
}

const FDT_MAGIC: u32 = 0xd00d_feed;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;

/// Hard ceiling on tokens walked. A well-formed tree for this class of board is a few thousand; the
/// cap exists so a CORRUPT one fails as a `None` instead of hanging the boot before anything has
/// been printed. Bounded, or it becomes undefined under a fault (§26.6).
const MAX_TOKENS: u32 = 200_000;
/// Deepest nesting tracked for `#address-cells` / `#size-cells`. Real trees are far shallower.
const MAX_DEPTH: usize = 24;

pub struct Fdt<'a> {
    blob: &'a [u8],
    off_struct: usize,
    off_strings: usize,
}

/// What a `reg` property decodes to, in the cells its parent declared.
#[derive(Clone, Copy)]
pub struct Reg {
    pub base: u64,
    pub size: u64,
}

impl<'a> Fdt<'a> {
    /// Wrap the firmware's device-tree pointer.
    ///
    /// # Safety
    /// `p` must be the pointer the boot protocol supplied in `a1`, or null. The header is read first
    /// and its magic checked before a slice of the declared length is formed, so a pointer to
    /// something that is not a device tree is rejected rather than trusted - but those first eight
    /// bytes must themselves be readable.
    pub unsafe fn from_ptr(p: *const u8) -> Option<Fdt<'a>> {
        if p.is_null() {
            return None;
        }
        // SAFETY: reads the 8-byte FDT header at the firmware-supplied pointer. Nothing is trusted
        // until the magic matches, and the length used below comes from the header itself - the only
        // way the blob's true extent can be known.
        let (magic, total) = unsafe {
            (
                u32::from_be((p as *const u32).read_volatile()),
                u32::from_be((p as *const u32).add(1).read_volatile()),
            )
        };
        if magic != FDT_MAGIC || total < 40 {
            return None;
        }
        // SAFETY: the header declares the blob's size and the firmware placed it there, so that many
        // bytes are mapped. From here the parser only indexes this slice, which makes every later
        // read bounds-checked by the compiler rather than by argument.
        let blob = unsafe { core::slice::from_raw_parts(p, total as usize) };
        let off_struct = be32(blob, 8)? as usize;
        let off_strings = be32(blob, 12)? as usize;
        if off_struct >= blob.len() || off_strings >= blob.len() {
            return None;
        }
        Some(Fdt { blob, off_struct, off_strings })
    }

    /// The memory reservation block: spans the firmware says must NOT be reused.
    ///
    /// This is not decoration. OpenSBI runs in M-mode from RAM and lists ITSELF here - on QEMU at
    /// 0x8000_0000, on the JH7110 at 0x4000_0000 - and nothing else in the tree marks it. A frame
    /// allocator handed that memory would allocate the firmware it is running under, and the failure
    /// would surface far from the write as an SBI call into rewritten code.
    ///
    /// Format is a list of (address, size) big-endian u64 pairs, terminated by a zero pair. Bounded
    /// by `max` entries and by the blob, so a corrupt list cannot spin.
    pub fn reservations(&self, out: &mut [Reg]) -> usize {
        let Some(off) = be32(self.blob, 16) else { return 0 };
        let mut off = off as usize;
        let mut n = 0;
        while n < out.len() {
            let (Some(addr), Some(size)) = (be64(self.blob, off), be64(self.blob, off + 8)) else {
                break;
            };
            if addr == 0 && size == 0 {
                break; // the terminator
            }
            out[n] = Reg { base: addr, size };
            n += 1;
            off += 16;
        }
        n
    }

    pub fn total_size(&self) -> usize {
        self.blob.len()
    }

    /// The tree's own view of which hart booted, for cross-checking against `a0`.
    pub fn boot_cpuid(&self) -> Option<u32> {
        be32(self.blob, 28)
    }

    /// Walk every property once, as `(node_name, prop_name, value, addr_cells, size_cells)`.
    /// Returning `true` from `f` stops the walk.
    ///
    /// A single bounded pass is the whole engine: every lookup below is this walk with a different
    /// filter, which keeps token decoding in one place rather than repeated per query. The cells
    /// handed to `f` are the ones in force for the node's PARENT, since that is what `reg` is
    /// decoded with, carried on a fixed-depth stack rather than re-derived.
    // The callback's references borrow the BLOB, not the call, so a caller may keep a node name
    // past the closure body - which `find_compatible` and `usable_harts` both need in order to
    // remember which node they are inside across successive properties.
    fn walk<F: FnMut(&'a str, &'a str, &'a [u8], u32, u32) -> bool>(&self, mut f: F) {
        let mut off = self.off_struct;
        let mut depth: usize = 0;
        let mut cells = [(2u32, 1u32); MAX_DEPTH];
        let mut names: [&'a str; MAX_DEPTH] = [""; MAX_DEPTH];
        let mut tokens: u32 = 0;

        loop {
            tokens += 1;
            if tokens > MAX_TOKENS {
                return;
            }
            let Some(tok) = be32(self.blob, off) else { return };
            off += 4;
            match tok {
                BEGIN_NODE => {
                    let Some(name) = cstr(self.blob, off) else { return };
                    off += (name.len() + 4) & !3;
                    if depth < MAX_DEPTH {
                        names[depth] = name;
                        cells[depth] = if depth > 0 { cells[depth - 1] } else { (2, 1) };
                    }
                    depth += 1;
                }
                END_NODE => depth = depth.saturating_sub(1),
                PROP => {
                    let Some(len) = be32(self.blob, off) else { return };
                    let Some(noff) = be32(self.blob, off + 4) else { return };
                    off += 8;
                    let Some(val) = self.blob.get(off..off + len as usize) else { return };
                    off += ((len as usize) + 3) & !3;
                    let Some(pname) = cstr(self.blob, self.off_strings + noff as usize) else {
                        return;
                    };
                    let d = depth.saturating_sub(1);
                    if d < MAX_DEPTH {
                        // `#address-cells` on a node governs its CHILDREN, so it is recorded here and
                        // read from the parent slot when a child's `reg` is decoded.
                        if pname == "#address-cells" {
                            if let Some(v) = be32(val, 0) {
                                cells[d].0 = v;
                            }
                        } else if pname == "#size-cells" {
                            if let Some(v) = be32(val, 0) {
                                cells[d].1 = v;
                            }
                        }
                        let parent = if d > 0 { cells[d - 1] } else { (2, 1) };
                        if f(names[d], pname, val, parent.0, parent.1) {
                            return;
                        }
                    }
                }
                NOP => {}
                END => return,
                _ => return, // a token we do not know means a corrupt tree: stop rather than guess
            }
        }
    }

    /// The first node whose `compatible` list contains `compat`: its `reg`, plus the extra u32
    /// properties named in `want` (written to `out`, same order, left `None` where absent).
    ///
    /// Matching on `compatible` rather than on a node NAME is deliberate. A name encodes an address
    /// (`serial@10000000`) and would smuggle a board fact into the kernel; `compatible` names the
    /// PROGRAMMING MODEL, which is the thing a driver actually depends on.
    pub fn find_compatible(
        &self,
        compat: &str,
        want: &[&str],
        out: &mut [Option<u32>],
    ) -> Option<Reg> {
        let mut target: Option<&'a str> = None;
        self.walk(|node, prop, val, _, _| {
            if prop == "compatible" && val.split(|&c| c == 0).any(|s| s == compat.as_bytes()) {
                target = Some(node);
                return true;
            }
            false
        });
        let target = target?;

        // Two passes rather than one, so the walk stays a plain filter with no lookahead. The tree
        // is small and read once at boot, so the second pass costs nothing worth complicating for.
        let mut reg: Option<Reg> = None;
        self.walk(|node, prop, val, ac, sc| {
            if node != target {
                return false;
            }
            if prop == "reg" && reg.is_none() {
                let base = if ac == 2 { be64(val, 0) } else { be32(val, 0).map(u64::from) };
                let size = match sc {
                    0 => Some(0),
                    2 => be64(val, (ac as usize) * 4),
                    _ => be32(val, (ac as usize) * 4).map(u64::from),
                };
                if let (Some(base), Some(size)) = (base, size) {
                    reg = Some(Reg { base, size });
                }
            }
            for (i, w) in want.iter().enumerate() {
                if prop == *w {
                    if let Some(slot) = out.get_mut(i) {
                        if slot.is_none() {
                            *slot = be32(val, 0);
                        }
                    }
                }
            }
            false
        });
        reg
    }

    /// The raw bytes of one property of the first node matching `compat`.
    ///
    /// Raw, because some properties are not a number: a PCI host's `ranges` is a list of
    /// seven-cell triplets whose meaning depends on flag bits inside the first cell, and decoding it
    /// belongs with the code that knows what a PCI window is rather than in the tree reader.
    pub fn find_compatible_prop(&self, compat: &str, prop_name: &str) -> Option<&'a [u8]> {
        let mut target: Option<&'a str> = None;
        self.walk(|node, prop, val, _, _| {
            if prop == "compatible" && val.split(|&c| c == 0).any(|s| s == compat.as_bytes()) {
                target = Some(node);
                return true;
            }
            false
        });
        let target = target?;
        let mut found: Option<&'a [u8]> = None;
        self.walk(|node, prop, val, _, _| {
            if node == target && prop == prop_name && found.is_none() {
                found = Some(val);
            }
            false
        });
        found
    }

    /// Where RAM starts, and how much there is in total.
    ///
    /// SUMMED across every `/memory` node rather than taken from the first: a machine may describe
    /// RAM in several banks, and using only the first would silently lose the rest.
    pub fn memory(&self) -> Option<Reg> {
        let mut base: Option<u64> = None;
        let mut total: u64 = 0;
        self.walk(|node, prop, val, ac, sc| {
            if prop == "reg" && (node == "memory" || node.starts_with("memory@")) {
                let b = if ac == 2 { be64(val, 0) } else { be32(val, 0).map(u64::from) };
                let s = if sc == 2 {
                    be64(val, (ac as usize) * 4)
                } else {
                    be32(val, (ac as usize) * 4).map(u64::from)
                };
                if let (Some(b), Some(s)) = (b, s) {
                    if base.is_none() {
                        base = Some(b);
                    }
                    total = total.saturating_add(s);
                }
            }
            false
        });
        base.map(|base| Reg { base, size: total })
    }

    /// The rate the machine's timer counts at.
    pub fn timebase_frequency(&self) -> Option<u32> {
        let mut hz = None;
        self.walk(|node, prop, val, _, _| {
            if prop == "timebase-frequency"
                && hz.is_none()
                && (node == "cpus" || node.starts_with("cpu@"))
            {
                hz = be32(val, 0);
            }
            false
        });
        hz
    }

    /// The ids of every hart the tree says is USABLE, written into `out`, returning how many.
    ///
    /// The IDS, not a count and a maximum. Starting a hart needs its number, and on this board the
    /// numbers are not `0..n`: hart 0 is an S7 monitor core marked `disabled` and the four U74s are
    /// 1 through 4. Deriving ids from a count would try to start hart 0 - a different core design,
    /// which the tree explicitly excludes - and skip hart 4 entirely. "Do not assume hart 0" stops
    /// being a rule to remember only if the ids are read rather than inferred.
    ///
    /// Bounded by the caller's array: a machine reporting more harts than fit gets the first `out.len()`
    /// and the count says how many were taken, so nothing is allocated and nothing overflows.
    pub fn usable_hart_ids(&self, out: &mut [u32]) -> usize {
        let mut n = 0usize;
        let mut cur_id: Option<u32> = None;
        let mut cur_ok = true;
        let mut cur_node = "";

        // A `cpu@` node's properties arrive one at a time, so an id is only known to be usable once
        // the NEXT node begins (or the walk ends) - which is what "banking" the previous one means.
        let mut bank = |id: Option<u32>, ok: bool, n: &mut usize, out: &mut [u32]| {
            if let (Some(id), true) = (id, ok) {
                if *n < out.len() {
                    out[*n] = id;
                    *n += 1;
                }
            }
        };

        self.walk(|node, prop, val, _, _| {
            if !node.starts_with("cpu@") {
                return false;
            }
            if node != cur_node {
                bank(cur_id, cur_ok, &mut n, out);
                cur_node = node;
                cur_id = None;
                cur_ok = true;
            }
            if prop == "reg" {
                cur_id = be32(val, 0);
            } else if prop == "status" {
                cur_ok = val.split(|&c| c == 0).next() == Some(b"okay");
            }
            false
        });
        bank(cur_id, cur_ok, &mut n, out);
        n
    }

    /// How many harts the tree says are USABLE, and the highest id among them.
    ///
    /// A `cpu@` node whose `status` is not "okay" is skipped, and that one rule is what keeps this
    /// arch from needing to know about the JH7110 at all: its hart 0 is an S7 monitor core marked
    /// `disabled`, so an honest reader excludes it without being told to. "Do not assume hart 0"
    /// stops being a special case to remember and becomes a consequence of reading the machine.
    pub fn usable_harts(&self) -> (u32, u32) {
        let mut count = 0u32;
        let mut max_id = 0u32;
        let mut cur_id: Option<u32> = None;
        let mut cur_ok = true;
        let mut cur_node = "";

        let mut bank = |id: Option<u32>, ok: bool, count: &mut u32, max_id: &mut u32| {
            if let Some(id) = id {
                if ok {
                    *count += 1;
                    if id > *max_id {
                        *max_id = id;
                    }
                }
            }
        };

        self.walk(|node, prop, val, _, _| {
            if !node.starts_with("cpu@") {
                return false;
            }
            if node != cur_node {
                bank(cur_id, cur_ok, &mut count, &mut max_id);
                cur_node = node;
                cur_id = None;
                cur_ok = true;
            }
            if prop == "reg" {
                cur_id = be32(val, 0);
            } else if prop == "status" {
                cur_ok = val.split(|&c| c == 0).next() == Some(b"okay");
            }
            false
        });
        bank(cur_id, cur_ok, &mut count, &mut max_id);
        (count, max_id)
    }
}
