// SPDX-License-Identifier: Apache-2.0
//! What can go wrong, and which of those you may safely try again.
//!
//! # This is not a new failure model
//!
//! GodspeedOS already had one. [`ServiceContext::request_with_reply_deadline_outcome`] returns
//! `DeadlineOutcome { Reply, SendFailed, QueueFull, Timeout }`, and those four states are exactly
//! right. This module does not replace them; it carries them to the application unchanged, alongside
//! the filesystem's own status byte, so that one `Error` answers both "what happened" and "what may
//! I do about it".
//!
//! # The distinction everything turns on
//!
//! ```text
//!   you sent a request and did not get a reply. WHY?
//!
//!   the send never left          -> Unreachable      RETRY IS SAFE
//!     (peer restarted, cap stale)                    nothing happened
//!
//!   the peer's queue was full    -> Busy             RETRY IS SAFE
//!     (peer alive, congested)                        nothing happened; pace yourself
//!
//!   the deadline passed in silence -> OutcomeUnknown RETRY IS NOT SAFE
//!     (peer may be slow, or gone,                    it may ALREADY have happened
//!      or may have done the work
//!      and died before replying)
//! ```
//!
//! That last case is the whole reason this type is shaped the way it is. A `delete` that returns
//! `OutcomeUnknown` may have deleted the file. Re-sending it is not "trying again", it is performing
//! a second, different operation whose failure would look like success. `services/copier` says the
//! same thing at its own call site, having learned it the hard way, and five services reached for a
//! longer-named SDK function to recover a distinction the short one threw away.
//!
//! [`Error::retry_is_safe`] is the answer in one call, so nobody has to re-derive it.

/// Everything a Godspeed operation can fail with.
///
/// Deliberately small. Each variant exists because a caller would do something DIFFERENT about it;
/// a distinction nobody would act on belongs in a log line, not in a type.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Error {
    // ---- the service answered, and said no -------------------------------------------------
    /// No such file or directory. The service is healthy; the path is not there.
    NotFound,
    /// The operation needs a right this capability does not carry (§7.3 non-escalation). Asking
    /// again cannot help: rights never widen.
    PermissionDenied,
    /// The volume is not mounted, or holds no GodspeedOS filesystem.
    NoFilesystem,
    /// The storage is present but unreadable. Data may still be intact, so this is NOT an
    /// invitation to reformat.
    Unavailable,
    /// The disk holds a foreign partition table. Refused deliberately rather than overwritten.
    Foreign,
    /// The service tried and failed, and said so. A real failure with a real answer behind it.
    Failed,

    // ---- no answer came, and the three are NOT interchangeable --------------------------------
    /// The request never left this task: the peer's capability is stale because it restarted
    /// (§14.2), or its name does not currently resolve. **Nothing happened.** Retrying after
    /// reacquiring the peer by name reaches the fresh instance, which is what [`crate::call`]
    /// already does for you once.
    Unreachable,
    /// The peer is alive and its queue is full. **Nothing happened.** Congestion is transient by
    /// definition: pace and retry, and do not go looking for a peer that never went anywhere.
    Busy,
    /// The deadline passed with no reply. **The operation may have completed.** The peer may be
    /// slow, or may have done the work and died before answering. This is the one failure you must
    /// not paper over: for anything that changes state, report it or re-read the truth, never
    /// re-send.
    OutcomeUnknown,

    // ---- the answer arrived and made no sense ------------------------------------------------
    /// A reply came back that this library could not parse: too short, wrong tag, or a status byte
    /// outside the protocol. A bug somewhere, not a condition to handle - but it is reported rather
    /// than guessed past, because guessing is how a wrong byte index becomes a wrong answer.
    Malformed,

    /// The caller's buffer is too small for the answer. Nothing was consumed; call again with room.
    BufferTooSmall,

    /// The request could not be built: a path longer than the protocol allows, or a payload past
    /// the message ceiling. It was never sent.
    InvalidInput,
}

impl Error {
    /// **May I simply send this again?**
    ///
    /// `true` only when the request provably never reached the service, so a retry is the same
    /// operation rather than a second one. `false` for `OutcomeUnknown` even though a retry might
    /// work, because "might" is not good enough when the first attempt may already have committed.
    ///
    /// For a read-only operation the caller may retry regardless; this is about what the LIBRARY
    /// can promise without knowing what the request was.
    pub fn retry_is_safe(self) -> bool {
        matches!(self, Error::Unreachable | Error::Busy)
    }

    /// `true` when the service answered. Distinguishes "no" from "no answer", which is the coarse
    /// version of the distinction above and often the only one a caller needs.
    pub fn service_answered(self) -> bool {
        matches!(self,
            Error::NotFound | Error::PermissionDenied | Error::NoFilesystem
            | Error::Unavailable | Error::Foreign | Error::Failed)
    }

    /// A short, stable, human-readable phrase. Present tense, no trailing punctuation, no leading
    /// capital, so it drops into a sentence a utility is already building.
    pub fn as_str(self) -> &'static str {
        match self {
            Error::NotFound         => "not found",
            Error::PermissionDenied => "permission denied",
            Error::NoFilesystem     => "no filesystem on this volume",
            Error::Unavailable      => "storage unavailable",
            Error::Foreign          => "the disk holds a foreign partition table",
            Error::Failed           => "the operation failed",
            Error::Unreachable      => "the service could not be reached (nothing happened)",
            Error::Busy             => "the service is busy (nothing happened)",
            Error::OutcomeUnknown   => "no answer before the deadline - THE OUTCOME IS UNKNOWN",
            Error::Malformed        => "the service sent a reply this library could not parse",
            Error::BufferTooSmall   => "the buffer is too small for the answer",
            Error::InvalidInput     => "the request was not valid and was never sent",
        }
    }
}

/// The filesystem's status byte, as `services/fs` defines it.
///
/// Owned HERE rather than copied into every crate that talks to `fs`. Before this library, `FS_OK`
/// was declared independently in four crates and the shell re-declared 21 of the filesystem's 27
/// opcodes; a guessed opcode in one of them made a command write nothing for twelve seconds and
/// report success.
pub(crate) fn from_fs_status(status: u8) -> Result<(), Error> {
    match status {
        0 => Ok(()),                          // FS_OK
        1 => Err(Error::Failed),              // FS_ERR
        2 => Err(Error::NotFound),            // FS_NOTFOUND
        3 => Err(Error::NoFilesystem),        // FS_NOFS
        4 => Err(Error::Unavailable),         // FS_UNAVAIL
        5 => Err(Error::PermissionDenied),    // FS_DENIED
        6 => Err(Error::Foreign),             // FS_FOREIGN
        _ => Err(Error::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The distinction the whole library exists to preserve. If this test ever goes green with
    /// `OutcomeUnknown` in the safe set, a delete has become a double delete somewhere.
    #[test]
    fn retry_is_safe_only_when_nothing_happened() {
        assert!(Error::Unreachable.retry_is_safe(), "the send never left");
        assert!(Error::Busy.retry_is_safe(), "the queue was full; nothing left either");
        assert!(!Error::OutcomeUnknown.retry_is_safe(), "IT MAY HAVE COMMITTED");

        // A service that answered told us what happened; there is nothing to retry into.
        for e in [Error::NotFound, Error::PermissionDenied, Error::Failed,
                  Error::NoFilesystem, Error::Unavailable, Error::Foreign] {
            assert!(!e.retry_is_safe(), "{e:?} is an answer, not a lost request");
        }
    }

    /// Every status byte `services/fs` can send maps to exactly one error, and an unknown byte is
    /// reported rather than guessed past.
    #[test]
    fn fs_status_bytes_map_exactly() {
        assert_eq!(from_fs_status(0), Ok(()));
        assert_eq!(from_fs_status(1), Err(Error::Failed));
        assert_eq!(from_fs_status(2), Err(Error::NotFound));
        assert_eq!(from_fs_status(3), Err(Error::NoFilesystem));
        assert_eq!(from_fs_status(4), Err(Error::Unavailable));
        assert_eq!(from_fs_status(5), Err(Error::PermissionDenied));
        assert_eq!(from_fs_status(6), Err(Error::Foreign));
        // A byte outside the protocol is a malformed reply, NOT a success and NOT a guess.
        assert_eq!(from_fs_status(7),   Err(Error::Malformed));
        assert_eq!(from_fs_status(255), Err(Error::Malformed));
    }

    #[test]
    fn service_answered_separates_no_from_no_answer() {
        assert!(Error::NotFound.service_answered());
        assert!(Error::PermissionDenied.service_answered());
        assert!(!Error::Unreachable.service_answered());
        assert!(!Error::OutcomeUnknown.service_answered());
        assert!(!Error::Busy.service_answered());
    }

    /// The phrasing is user-facing, so it is pinned: no trailing full stop, and the one that matters
    /// most says so loudly.
    #[test]
    fn messages_are_house_style() {
        for e in [Error::NotFound, Error::Unreachable, Error::OutcomeUnknown, Error::Busy,
                  Error::Failed, Error::Malformed, Error::BufferTooSmall, Error::InvalidInput] {
            let s = e.as_str();
            assert!(!s.is_empty());
            assert!(!s.ends_with('.'), "{s:?} ends with a full stop");
        }
        assert!(Error::OutcomeUnknown.as_str().contains("UNKNOWN"),
                "the unknown-outcome message must not read like an ordinary failure");
    }
}
