//! Capability rights bitfield — §7.4.

/// Actions a capability may authorise on its target resource.
///
/// Rights are **non-escalating**: a `GRANT` transfer can only narrow rights,
/// never widen them. The kernel enforces this on every cap insertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rights(pub(crate) u8);

impl Rights {
    pub const READ:   Rights = Rights(1 << 0);
    pub const WRITE:  Rights = Rights(1 << 1);
    pub const SEND:   Rights = Rights(1 << 2);
    pub const RECV:   Rights = Rights(1 << 3);
    pub const GRANT:  Rights = Rights(1 << 4);
    pub const REVOKE: Rights = Rights(1 << 5);

    pub const fn empty() -> Self { Rights(0) }
    pub const fn all()   -> Self { Rights(0b0011_1111) }

    pub fn contains(self, other: Rights) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Narrow `self` to the intersection with `mask`. Never widens.
    pub fn narrow(self, mask: Rights) -> Rights {
        Rights(self.0 & mask.0)
    }

    pub fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }
}

impl core::ops::BitOr for Rights {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Rights(self.0 | rhs.0) }
}
