// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor support for Microsoft hypervisor-backed partitions.

use crate::HypervisorBacked;
use crate::UhProcessor;
use hcl::GuestVtl;
use thiserror::Error;

pub mod arm64;
mod tlb_lock;
pub mod x64;

#[derive(Debug, Error)]
#[error("failed to run")]
struct MshvRunVpError(#[source] hcl::ioctl::Error);

#[derive(Default, inspect::Inspect)]
pub(crate) struct VbsIsolatedVtl1State {
    #[inspect(hex, with = "|flags| flags.map(u32::from)")]
    default_vtl_protections: Option<hvdef::HvMapGpaFlags>,
    enable_vtl_protection: bool,
}

impl UhProcessor<'_, HypervisorBacked> {
    fn deliver_synic_messages(&mut self, vtl: GuestVtl, sints: u16) {
        let pending_sints =
            self.inner.message_queues[vtl].post_pending_messages(sints, |sint, message| {
                self.partition.hcl.post_message_direct(
                    self.inner.vp_info.base.vp_index.index(),
                    sint,
                    message,
                )
            });

        self.request_sint_notifications(vtl, pending_sints);
    }

    /// Sets the startup suspend state for VTL0 of this VP.
    ///
    /// If `startup_suspend` is `true`, hold the VP in the startup suspend state by setting
    /// the internal activity register. In the opposie case, clear the startup suspend state
    /// thus letting the VP run.
    fn set_vtl0_startup_suspend(
        &mut self,
        startup_suspend: bool,
    ) -> Result<(), hcl::ioctl::register::SetRegError> {
        let reg = u64::from(
            hvdef::HvInternalActivityRegister::new().with_startup_suspend(startup_suspend),
        );
        // Non-VTL0 VPs should never be in startup suspend, so
        // we only need to handle VTL0.
        self.runner.set_vp_register(
            GuestVtl::Vtl0,
            hvdef::HvAllArchRegisterName::InternalActivityState.into(),
            reg.into(),
        )
    }
}
