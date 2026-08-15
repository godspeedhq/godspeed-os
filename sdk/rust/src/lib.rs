// SPDX-License-Identifier: Apache-2.0
//! GodspeedOS service SDK.
//!
//! All userspace services link against this crate. It provides the typed
//! wrappers around kernel syscalls so service code never issues raw syscalls.

// `no_std` for the real (target) build; under `cargo test` we build for the host with
// `std` so the pure-logic modules (e.g. `hid`) can have unit tests.
#![cfg_attr(not(test), no_std)]

pub mod adversarial;
pub mod capability;
pub mod dma;
pub mod hid;
pub mod ipc;
pub mod mmio;
pub mod record;
pub mod service_context;
pub(crate) mod syscall;

pub use capability::{CapHandle, CapError};
pub use dma::Dma;
pub use ipc::{Message, IpcError};
pub use mmio::Mmio;
pub use record::{Table, Value, RecordSink, parse_predicate};
pub use service_context::{ServiceContext, TaskStat, CapInfo, Datetime, ClockSource, ReqOutcome, DeadlineOutcome,
                          USB_DISK_BUSY, USB_DISK_ABSENT};

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Cap(CapError),
    Ipc(IpcError),
    NotFound,
    InvalidArgument,
}

impl From<CapError> for Error {
    fn from(e: CapError) -> Self { Error::Cap(e) }
}

impl From<IpcError> for Error {
    fn from(e: IpcError) -> Self { Error::Ipc(e) }
}

/// A panicking service must DIE, not spin.
///
/// This was `loop {}`, and that one line quietly disabled the whole recovery model. `panic = "abort"`
/// means a panic reaches here; spinning in ring 3 means the task never dies, and everything downstream
/// depends on death:
///
///   - the endpoint's generation is never bumped, so a peer blocked in `call` NEVER receives
///     `ReplyDead` (8.6) - the failure-truth Commandment VIII exists to deliver simply never arrives;
///   - the kernel never notifies the supervisor, so the service is NEVER restarted, however carefully
///     it was added to the watched set;
///   - nothing prints, so an operator sees a service that is "running" and answering nothing;
///   - the liveness watchdog cannot help - the task is preempted normally, so the CORE is fine.
///
/// A panic in `fs`, `block-driver` or a USB driver therefore converted the entire storage chain into a
/// permanent silent hang. That is the Rule Above The Rules inverted: the one thing nothing above the
/// kernel may do is wedge the machine, and this made a panic do exactly that.
///
/// So: say what happened, then FAULT deliberately. There is no self-terminate syscall (a service
/// cannot ask to be killed), and a ring-3 fault is the sanctioned way a task ends - the kernel kills
/// it, reclaims its frames, bumps its endpoint generation and notifies the supervisor, which is
/// precisely the path a page-faulting service already takes. `adversarial::fault_null_read` exists for
/// this exact purpose and carries the SAFETY argument, so the unsafe stays in the module 18.1 permits
/// it in rather than appearing here.
///
/// The log comes first and is deliberately short: `log` truncates at 256 bytes, and a panic message
/// with a long file path can exceed that.
#[cfg(not(test))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // `ServiceContext` is a zero-sized handle (`_private: ()`) whose methods read the kernel-written
    // context block, so one can be made here without threading it through the panic path.
    let ctx = crate::service_context::ServiceContext::for_panic();
    ctx.log_fmt(format_args!("PANIC in service: {}", info));
    ctx.log("service panicked - faulting so the kernel kills and the supervisor restarts it");
    crate::adversarial::fault_null_read();
    // Unreachable on a conforming kernel: the fault above kills this task. If a kernel ever failed to,
    // spinning is still wrong but it is what is left - and the two lines above already said so.
    loop {}
}
