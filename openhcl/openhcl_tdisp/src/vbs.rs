// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VBS implementation of [`TdispResourceValidationInterface`].

use crate::TdispResourceValidationInterface;
use crate::TdispTdiState;
use anyhow::Context;
use guid::Guid;
use hcl::ioctl::MshvHvcall;
use hvdef::Vtl;
use std::future::Future;
use std::pin::Pin;
use tdisp::devicereport::TdiReportStruct;

/// VBS implementation of [`TdispResourceValidationInterface`].
///
/// On VBS the hypervisor denies DMA from every device assigned to the
/// partition until it's authorized through the vTSM (virtual TSM). Each
/// validator is created for one device, so unblocking DMA authorizes that
/// device, blocking DMA unauthorizes it, and the `device_id` arguments are
/// unused. The other operations do nothing, as VBS has no other resources to
/// validate.
pub struct TdispVbsResourceValidator {
    hvcall: MshvHvcall,
    logical_device_id: u64,
}

impl TdispVbsResourceValidator {
    /// Creates a validator for the device on the VPCI bus with `instance_id`.
    pub fn new(instance_id: Guid) -> anyhow::Result<Self> {
        let hvcall = MshvHvcall::new().context("failed to open mshv_hvcall device")?;
        hvcall.set_allowed_hypercalls(&[hvdef::HypercallCode::HvCallVbsVmCallAuthorizeDeviceIo]);
        Ok(Self {
            hvcall,
            logical_device_id: logical_device_id(&instance_id),
        })
    }

    /// Authorizes the device's DMA, or unauthorizes it if `authorize` is false.
    fn authorize_device_io(&self, authorize: bool) -> anyhow::Result<()> {
        self.hvcall
            .authorize_device_io(self.logical_device_id, authorize)
            .with_context(|| {
                format!(
                    "failed to {} device I/O for logical device {:#x}",
                    if authorize {
                        "authorize"
                    } else {
                        "unauthorize"
                    },
                    self.logical_device_id
                )
            })
    }
}

/// The hypervisor's logical ID for function 0 on the VPCI bus with
/// `instance_id`. The guest's VPCI driver derives it the same way.
fn logical_device_id(instance_id: &Guid) -> u64 {
    u64::from(instance_id.data2) << 16 | u64::from(instance_id.data3 & 0xfff8)
}

impl TdispResourceValidationInterface for TdispVbsResourceValidator {
    fn on_pre_bind(&self, _target_vtl: Vtl, _device_id: u16) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_pre_start(&self, _target_vtl: Vtl, _device_id: u16) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_post_start(&self, _target_vtl: Vtl, _device_id: u16) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_tsm_tdi_state(
        &self,
        _target_vtl: Vtl,
        _device_id: u16,
    ) -> anyhow::Result<Option<TdispTdiState>> {
        Ok(None)
    }

    fn tdisp_set_tdi_report(&self, _device_id: u16, _report: &TdiReportStruct) {}

    fn tdisp_clear_tdi_report(&self, _device_id: u16) {}

    fn tdisp_unblock_mmio<'a>(
        &'a self,
        _target_vtl: Vtl,
        _device_id: u16,
        _base_gpa: u64,
        _base_offset: u32,
        _length_in_bytes: u64,
        _range_id: u16,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + Sync + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn tdisp_unblock_dma(&self, _target_vtl: Vtl, _device_id: u16) -> anyhow::Result<()> {
        self.authorize_device_io(true)
    }

    fn tdisp_block_mmio<'a>(
        &'a self,
        _target_vtl: Vtl,
        _device_id: u16,
        _base_gpa: u64,
        _base_offset: u32,
        _length_in_bytes: u64,
        _range_id: u16,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + Sync + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn tdisp_block_dma(&self, _target_vtl: Vtl, _device_id: u16) -> anyhow::Result<()> {
        self.authorize_device_io(false)
    }
}

#[cfg(test)]
mod tests {
    /// The ID is bytes 4-7 of the bus's instance ID with the function bits
    /// cleared, as the guest's VPCI driver derives it. All three function bits
    /// are set, so a mask that clears only some of them fails. The expected
    /// value is the ID a host assigned to a bus whose instance ID differs only
    /// in those bits.
    #[test]
    fn logical_device_id_clears_the_function_bits() {
        let instance_id = guid::guid!("c849acf7-18a6-4dbf-b9da-1aff87e1a12a");
        assert_eq!(super::logical_device_id(&instance_id), 0x18a6_4db8);
    }
}
