// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! GodspeedOS service SDK.
//!
//! All userspace services link against this crate. It provides the typed
//! wrappers around kernel syscalls so service code never issues raw syscalls.

#![no_std]

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
pub use service_context::{ServiceContext, TaskStat, CapInfo, Datetime};
pub use service_context::{
    REGISTRY_OP_REGISTER, REGISTRY_OP_LOOKUP, REGISTRY_FOUND, REGISTRY_NOT_FOUND,
    REGISTRY_NAME_MAX,
};

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

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
