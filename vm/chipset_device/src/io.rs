// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Memory mapped IO (MMIO) and port IO support.

pub mod deferred;

/// An error related to the suitability of the IO request for the device. A
/// device should handle device-specific errors internally, and should return
/// `IoResult::Ok` in these conditions.
#[derive(Copy, Clone, Debug, mesh::MeshPayload)]
pub enum IoError {
    /// The requested device register is not present.
    InvalidRegister,
    /// The access length is invalid for the specified address.
    InvalidAccessSize,
    /// The caller attempted to perform an unaligned access to the device
    /// registers.
    UnalignedAccess,
    /// The device never responded to the IO request.
    NoResponse,
}

/// The result returned by a device IO (memory-mapped IO, port IO, or PCI Config
/// Space) operation, as in methods of [`MmioIntercept`](crate::mmio::MmioIntercept),
/// [`PortIoIntercept`](crate::pio::PortIoIntercept), or
/// [`PciConfigSpace`](crate::pci::PciConfigSpace).
#[derive(Debug)]
#[must_use]
pub enum IoResult {
    /// The IO operation succeeded.
    Ok,
    /// The IO operation failed due to an access error.
    ///
    /// The caller should log the failure, then ignore writes, and fill the
    /// buffer with an appropriate bus-specific error value on reads. e.g. For
    /// port IO or memory-mapped IO this value is typically `!0`, while for PCI
    /// config space the value is typically `0`.
    Err(IoError),
    /// Defer this request until [`deferred::DeferredRead::complete`] or
    /// [`deferred::DeferredWrite::complete`] is called.
    Defer(deferred::DeferredToken),
}

impl IoResult {
    /// Asserts if `self` is not `IoResult::Ok`.
    #[track_caller]
    pub fn unwrap(self) {
        match self {
            IoResult::Ok => {}
            IoResult::Err(_) | IoResult::Defer(_) => panic!("unexpected IO result {:?}", self),
        }
    }

    /// Converts `self` to a `Result<(), IoError>`, panicking if `self` is
    /// `IoResult::Defer`.
    #[track_caller]
    pub fn now_or_never(self) -> Result<(), IoError> {
        match self {
            IoResult::Ok => Ok(()),
            IoResult::Err(e) => Err(e),
            IoResult::Defer(_) => panic!("unexpected IO result {:?}", self),
        }
    }
}
