// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Crate for interacting with the hypervisor via the /dev/mshv_vtl device and
//! related kernel functionality.

#![cfg(target_os = "linux")]
#![warn(missing_docs)]
// UNSAFETY: Calling ioctls.
#![expect(unsafe_code)]

use hvdef::hypercall::HvInputVtl;
use hvdef::Vtl;
use inspect::Inspect;
use thiserror::Error;

pub mod ioctl;
mod mapped_page;
pub mod protocol;
pub mod stats;
pub mod vmbus;
pub mod vmsa;

/// The VTL, exclusive of the paravisor VTL (VTL2).
///
/// This is useful to use instead of [`Vtl`] to statically ensure that the VTL
/// is not VTL2.
#[derive(Copy, Clone, Debug, Inspect, PartialEq, Eq, PartialOrd, Ord)]
pub enum GuestVtl {
    /// VTL0
    Vtl0 = 0,
    /// VTL1
    Vtl1 = 1,
}

impl From<GuestVtl> for HvInputVtl {
    fn from(value: GuestVtl) -> Self {
        Vtl::from(value).into()
    }
}

impl From<GuestVtl> for u8 {
    fn from(value: GuestVtl) -> Self {
        Vtl::from(value).into()
    }
}

impl From<GuestVtl> for Vtl {
    fn from(value: GuestVtl) -> Self {
        match value {
            GuestVtl::Vtl0 => Vtl::Vtl0,
            GuestVtl::Vtl1 => Vtl::Vtl1,
        }
    }
}

/// The specified VTL is not supported in the current context.
#[derive(Debug, Error)]
#[error("unsupported guest VTL")]
pub struct UnsupportedGuestVtl(pub u8);

impl TryFrom<Vtl> for GuestVtl {
    type Error = UnsupportedGuestVtl;

    fn try_from(value: Vtl) -> Result<Self, Self::Error> {
        Ok(match value {
            Vtl::Vtl0 => GuestVtl::Vtl0,
            Vtl::Vtl1 => GuestVtl::Vtl1,
            _ => return Err(UnsupportedGuestVtl(value.into())),
        })
    }
}

impl TryFrom<u8> for GuestVtl {
    type Error = UnsupportedGuestVtl;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => GuestVtl::Vtl0,
            1 => GuestVtl::Vtl1,
            _ => return Err(UnsupportedGuestVtl(value)),
        })
    }
}
