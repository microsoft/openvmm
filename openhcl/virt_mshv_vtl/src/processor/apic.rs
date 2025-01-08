// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::LapicState;
use super::UhRunVpError;
use hcl::GuestVtl;
use virt::vp::MpState;
use virt_support_apic::ApicWork;

pub(crate) trait ApicBacking {
    fn handle_init(&mut self, vtl: GuestVtl) -> Result<(), UhRunVpError>;
    fn handle_sipi(&mut self, vtl: GuestVtl, vector: u8) -> Result<(), UhRunVpError>;
    fn handle_nmi(&mut self, vtl: GuestVtl) -> Result<(), UhRunVpError>;
    fn handle_interrupt(&mut self, vtl: GuestVtl, vector: u8) -> Result<(), UhRunVpError>;
    fn handle_extint(&mut self, vtl: GuestVtl) -> Result<(), UhRunVpError>;
}

pub(crate) fn poll_apic_core<T: ApicBacking>(
    processor: &mut T,
    scan: impl Fn(&mut T) -> ApicWork,
    proxy_irr: impl Fn(&mut T) -> Option<[u32; 8]>,
    lapic_access: impl Fn(&mut T) -> &mut LapicState,
    vtl1_enabled: impl Fn(&mut T) -> bool,
    vtl: GuestVtl,
) -> Result<(), UhRunVpError> {
    // Check for interrupt requests from the host and kernel offload.
    if vtl == GuestVtl::Vtl0 {
        if let Some(irr) = proxy_irr(processor) {
            // We can't put the interrupts directly into offload (where supported) because we might need
            // to clear the tmr state. This can happen if a vector was previously used for a level
            // triggered interrupt, and is now being used for an edge-triggered interrupt.
            lapic_access(processor).lapic.request_fixed_interrupts(irr);
        }
    }

    let ApicWork {
        init,
        extint,
        sipi,
        nmi,
        interrupt,
    } = scan(processor);

    // An INIT/SIPI targeted at a VP with more than one guest VTL enabled is ignored.
    // Check VTL enablement inside each block to avoid taking a lock on the hot path,
    // INIT and SIPI are quite cold.
    if init {
        if !vtl1_enabled(processor) {
            processor.handle_init(vtl)?;
        }
    }

    if let Some(vector) = sipi {
        if !vtl1_enabled(processor) {
            processor.handle_sipi(vtl, vector)?;
        }
    }

    // Interrupts are ignored while waiting for SIPI.
    let lapic = lapic_access(processor);
    if lapic.activity != MpState::WaitForSipi {
        if nmi || lapic.nmi_pending {
            lapic.nmi_pending = true;
            processor.handle_nmi(vtl)?;
        }

        if let Some(vector) = interrupt {
            processor.handle_interrupt(vtl, vector)?;
        }

        if extint {
            processor.handle_extint(vtl)?;
        }
    }

    Ok(())
}
