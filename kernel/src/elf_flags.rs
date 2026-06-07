//! Pure ELF-segment → page-permission logic, factored out of `loader.rs` so the
//! W^X decision is unit-testable on the host. `loader.rs` itself is
//! hardware-coupled (allocator, page tables, HHDM) and excluded from the test lib;
//! this module has no dependencies and is included by both the kernel binary and
//! the host-test lib (`lib.rs`).
//!
//! **W^X enforcement (hardening H4a):** a loaded page is executable only if its
//! segment is executable AND not writable. A segment marked both writable and
//! executable (`PF_W|PF_X`) is forced non-executable — the loader *enforces* the
//! invariant rather than *mirroring* the ELF's flags, so a malformed or hostile
//! binary cannot obtain a writable-executable mapping. Service binaries built by
//! the toolchain never produce a W+X segment, so this is defense-in-depth.

/// ELF program-header flag: segment is executable.
pub const PF_X: u32 = 1;
/// ELF program-header flag: segment is writable.
pub const PF_W: u32 = 2;
/// ELF program-header flag: segment is readable.
pub const PF_R: u32 = 4;

/// Whether a segment with these `p_flags` should be mapped writable.
pub fn segment_writable(p_flags: u32) -> bool {
    p_flags & PF_W != 0
}

/// Whether a segment with these `p_flags` must be mapped `NO_EXEC`, enforcing W^X:
/// executable iff (executable AND not writable), so writable OR non-executable
/// segments are non-executable.
pub fn segment_no_exec(p_flags: u32) -> bool {
    let writable = p_flags & PF_W != 0;
    let executable = p_flags & PF_X != 0;
    writable || !executable
}

/// Whether these flags describe a W+X segment (writable AND executable) — the
/// anomaly the loader downgrades to `NO_EXEC` and logs.
pub fn segment_is_wx(p_flags: u32) -> bool {
    p_flags & PF_W != 0 && p_flags & PF_X != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rx_code_is_executable() {
        // R+X (.text): executable, read-only.
        assert!(!segment_no_exec(PF_R | PF_X));
        assert!(!segment_writable(PF_R | PF_X));
        assert!(!segment_is_wx(PF_R | PF_X));
    }

    #[test]
    fn rw_data_is_noexec() {
        // R+W (.data/.bss): writable, never executable.
        assert!(segment_no_exec(PF_R | PF_W));
        assert!(segment_writable(PF_R | PF_W));
        assert!(!segment_is_wx(PF_R | PF_W));
    }

    #[test]
    fn ro_is_noexec() {
        // R (.rodata): not executable.
        assert!(segment_no_exec(PF_R));
        assert!(!segment_writable(PF_R));
    }

    #[test]
    fn wx_is_forced_noexec_and_flagged() {
        // W+X (malformed/hostile): W^X enforced → NO_EXEC, and flagged so the
        // loader logs the downgrade. THIS is the regression the fix prevents: the
        // old code mirrored PF_X, so this would have been executable+writable.
        assert!(segment_no_exec(PF_R | PF_W | PF_X));
        assert!(segment_is_wx(PF_R | PF_W | PF_X));
    }
}
