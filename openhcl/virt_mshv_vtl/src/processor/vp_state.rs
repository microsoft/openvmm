// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Backing;
use super::UhProcessor;
use crate::GuestVtl;
use hcl::ioctl::register::GetRegError;
use hcl::ioctl::register::SetRegError;
use thiserror::Error;

pub struct UhVpStateAccess<'a, 'b, T: Backing> {
    pub(crate) vp: &'a mut UhProcessor<'b, T>,
    pub(crate) vtl: GuestVtl,
}

impl<'a, 'p, T: Backing> UhVpStateAccess<'a, 'p, T> {
    pub(crate) fn new(vp: &'a mut UhProcessor<'p, T>, vtl: GuestVtl) -> Self {
        Self { vp, vtl, }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to set registers")]
    SetRegisters(#[source] SetRegError),
    #[error("failed to get registers")]
    GetRegisters(#[source] GetRegError),
    #[error("the value for setting efer {0} is invalid, {1}")]
    SetEfer(u64, &'static str),
    #[error("'{0}' state is not implemented yet")]
    Unimplemented(&'static str),
    #[error("failed to set apic base MSR")]
    InvalidApicBase(#[source] virt_support_apic::InvalidApicBase),
    // #[error("failed to set registers")]
    // SetRegistersR(#[source] SetRegError),
    // #[error("failed to get registers")]
    // GetRegistersR(#[source] GetRegError),
}

// /// temp - just to get round some error compatibilities
// #[derive(Debug, Error)]
// pub enum RegError {
//     #[error("failed to set registers")]
//     SetRegisters(#[source] SetRegError),
//     #[error("failed to get registers")]
//     GetRegisters(#[source] GetRegError),
// }
