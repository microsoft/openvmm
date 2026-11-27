// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VBS implementation of [`TdispResourceValidationInterface`].

use crate::TdispResourceValidationInterface;
use crate::TdispTdiState;
use anyhow::Context;
use guid::Guid;
use hcl::ioctl::AcceptPagesError;
use hcl::ioctl::MshvHvcall;
use hvdef::Vtl;
use hvdef::hypercall::HostVisibilityType;
use memory_range::MemoryRange;
use std::future::Future;
use std::pin::Pin;
use tdisp::devicereport::TdiReportStruct;

/// VBS implementation of [`TdispResourceValidationInterface`].
///
/// On VBS the hypervisor denies DMA from every device assigned to the
/// partition until it's authorized through the vTSM (virtual TSM). Each
/// validator is created for one device, so unblocking DMA authorizes that
/// device, blocking DMA unauthorizes it, and the `device_id` arguments are
/// unused.
///
/// VTL0 also can't reach the device's BARs until VTL2 accepts them, each time
/// the host maps them. VBS can't make device MMIO private, so BARs are only
/// accepted shared and unblocking MMIO fails. The other operations do nothing.
pub struct TdispVbsResourceValidator {
    hvcall: MshvHvcall,
    logical_device_id: u64,
    vtl0_mmio: Vec<MemoryRange>,
}

impl TdispVbsResourceValidator {
    /// Creates a validator for the device on the VPCI bus with `instance_id`.
    /// The guest chooses the BAR addresses, so only BARs inside `vtl0_mmio`
    /// are accepted.
    pub fn new(instance_id: Guid, vtl0_mmio: &[MemoryRange]) -> anyhow::Result<Self> {
        let hvcall = MshvHvcall::new().context("failed to open mshv_hvcall device")?;
        hvcall.set_allowed_hypercalls(&[
            hvdef::HypercallCode::HvCallVbsVmCallAuthorizeDeviceIo,
            hvdef::HypercallCode::HvCallAcceptGpaPages,
        ]);
        Ok(Self {
            hvcall,
            logical_device_id: logical_device_id(&instance_id),
            vtl0_mmio: vtl0_mmio.to_vec(),
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

/// Accepts the pages of `range` that the host maps, shared, and returns the
/// ones it doesn't. The host emulates those, so they stay unaccepted: VTL0
/// accesses to them trap to VTL2, which forwards them to the host.
fn accept_mapped_pages(
    hvcall: &MshvHvcall,
    range: MemoryRange,
) -> Result<Vec<MemoryRange>, AcceptPagesError> {
    let mut skipped = Vec::<MemoryRange>::new();
    let mut start = range.start();
    while start < range.end() {
        match hvcall.accept_gpa_pages(
            MemoryRange::new(start..range.end()),
            hvdef::hypercall::AcceptMemoryType::ANY,
            HostVisibilityType::SHARED,
        ) {
            Ok(()) => break,
            Err(AcceptPagesError::Hypervisor {
                range: chunk,
                output,
                hv_error,
            }) if hv_error == hvdef::HvError::OperationDenied => {
                let page = chunk.start() + output.elements_processed() as u64 * hvdef::HV_PAGE_SIZE;
                let end = page + hvdef::HV_PAGE_SIZE;
                match skipped.last_mut() {
                    Some(last) if last.end() == page => *last = MemoryRange::new(last.start()..end),
                    _ => skipped.push(MemoryRange::new(page..end)),
                }
                start = end;
            }
            Err(err) => return Err(err),
        }
    }
    Ok(skipped)
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
        range_id: u16,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + Sync + 'a>> {
        Box::pin(async move { anyhow::bail!("VBS can't make BAR {range_id} private") })
    }

    fn tdisp_accept_shared_mmio(
        &self,
        _target_vtl: Vtl,
        base_gpa: u64,
        length_in_bytes: u64,
        range_id: u16,
    ) -> anyhow::Result<()> {
        // Checking the range against VTL0 MMIO also keeps `bounding` from panicking.
        let end = base_gpa.saturating_add(length_in_bytes);
        anyhow::ensure!(
            self.vtl0_mmio
                .iter()
                .any(|r| r.start() <= base_gpa && end <= r.end()),
            "BAR {range_id} at {base_gpa:#x} with length {length_in_bytes:#x} is outside VTL0 MMIO"
        );
        let range = MemoryRange::bounding(base_gpa..end);
        let skipped = accept_mapped_pages(&self.hvcall, range)
            .with_context(|| format!("failed to accept BAR {range_id} at {range}"))?;
        let skipped = skipped.iter().map(|r| r.to_string()).collect::<Vec<_>>();
        tracing::info!(
            bar_id = range_id,
            %range,
            skipped = %skipped.join(", "),
            "accepted device BAR"
        );
        Ok(())
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
