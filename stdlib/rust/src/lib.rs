// SPDX-License-Identifier: Apache-2.0
//! The GodspeedOS standard library.
//!
//! Small, typed wrappers over the mechanisms GodspeedOS already has, so that writing a program does
//! not start with learning a wire protocol.
//!
//! # The whole of it
//!
//! ```ignore
//! use godspeed::{fs, io};
//!
//! #[no_mangle]
//! pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
//!     let mut fs = fs::Fs::new(&ctx);
//!     let mut buf = [0u8; 4096];
//!
//!     match fs.read_into("/data/hello.txt", &mut buf) {
//!         Ok(n)  => io::println(&ctx, core::str::from_utf8(&buf[..n]).unwrap_or("<not utf-8>")),
//!         Err(e) => io::report(&ctx, "read", e),
//!     }
//!     loop { ctx.yield_cpu(); }
//! }
//! ```
//!
//! # What this library will not do for you
//!
//! **It does not grant authority.** Every entry point takes a `&ServiceContext`, because that is
//! where authority lives. `fs::Fs::new(&ctx)` is a convenience, not a capability: a task whose
//! contract never asked for the filesystem gets [`Error::Unreachable`] from it, the same as for any
//! peer it cannot reach. There is no global, no ambient handle, and no `print!` that finds a stdout
//! on its own - a global would be ambient authority wearing a familiar name.
//!
//! **It does not hide failure.** In particular it does not hide the difference between "the request
//! never left" and "no answer came back", because those demand opposite responses and collapsing
//! them is how a retry becomes a second delete. [`Error::retry_is_safe`] is the answer in one call.
//!
//! **It does not pretend there is a heap.** There is not, deliberately (CLAUDE.md §26.6.1), so
//! nothing here returns an allocated `String` or `Vec`. Callers own their buffers and the bound is
//! readable in the source. Making this look like Rust's `std` would mean trading the architecture
//! for a familiar shape, which is the one thing this library is not for.
//!
//! # Why `service_main` and not `fn main`
//!
//! Because GodspeedOS has no terminating task yet. Every runnable thing is a service, entered at
//! `service_main(ctx) -> !`, and of the 52 syscalls the only one that ends a task is `Kill` - which
//! kills by name and is gated behind `service_control`, a capability no application should hold.
//!
//! That gap is real and is written up in `docs/stdlib-design.md` rather than papered over here. It
//! needs one kernel change, reviewed on its own terms; this library is deliberately useful without
//! it, and will not grow a userspace impersonation of it.
//!
//! # Where the unsafe lives
//!
//! Nowhere in this crate. `#![deny(unsafe_code)]` below, with no exception - unlike a service, this
//! is a library and exports no `#[no_mangle]` entry symbol, so it does not need even the one
//! `#[allow]` that services carry.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_code)]

pub mod addr;
pub mod error;

// THE SDK-DEPENDENT HALF, and why it is not in the host test build.
//
// `godspeed_sdk` provides the `panic_handler`, and so does `std`. Any crate that depends on the SDK and is
// then built for the host with `std` hits `duplicate lang item`. The SDK itself sidesteps this with
// `cfg_attr(not(test), no_std)`, which only helps the crate being tested.
//
// So the split follows the pattern `kernel/src/clock.rs` already documents: the PURE logic (the
// error model, which is most of the semantics worth testing) has no SDK import and is unit-tested
// on the host; everything that actually talks to a service is exercised on the TARGET, where it can
// meet a real filesystem that can really be restarted. A mock would only prove this library agrees
// with a mock.
#[cfg(not(test))] pub mod call;
#[cfg(not(test))] pub mod cap;
#[cfg(not(test))] pub mod file;
#[cfg(not(test))] pub mod ipc;
#[cfg(not(test))] pub mod task;
#[cfg(not(test))] pub mod trace;
#[cfg(not(test))] pub mod record;
#[cfg(not(test))] pub mod fs;
#[cfg(not(test))] pub mod io;
#[cfg(not(test))] pub mod net;
#[cfg(not(test))] pub mod resource;

pub use error::Error;

/// The service context, re-exported so an ordinary program never has to name `godspeed_sdk`.
///
/// Every program's entry point takes one, and this library said "you should not need the SDK" while
/// requiring the SDK for the first line of every program. The Stranger Test caught that: a weak
/// model given only the published documentation wrote `use godspeed_sdk::ServiceContext;` and
/// recorded it as the single thing most likely to stop its program compiling.
///
/// It is a re-export rather than a wrapper on purpose - it is the SAME type, so a program that does
/// reach for the SDK later is not holding two different things with one name.
#[cfg(not(test))]
pub use godspeed_sdk::service_context::ServiceContext;

/// Everything a small program usually wants, in one `use`.
///
/// Deliberately tiny. A prelude that pulls in a framework is how a standard library stops being one.
#[cfg(not(test))]
pub mod prelude {
    pub use crate::error::Error;
    pub use crate::file::File;
    pub use crate::fs::Fs;
    pub use crate::net::Net;
    pub use crate::io;
    pub use godspeed_sdk::service_context::ServiceContext;
}
