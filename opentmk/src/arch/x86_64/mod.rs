pub mod hypercall;
#[cfg(feature = "nightly")]
pub mod interrupt;
#[cfg(feature = "nightly")]
mod interrupt_handler_register;
mod io;
pub mod rtc;
pub mod serial;
pub mod tpm;
