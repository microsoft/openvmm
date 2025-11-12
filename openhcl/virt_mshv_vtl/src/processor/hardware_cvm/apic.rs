// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::UhProcessor;
use crate::processor::HardwareIsolatedBacking;
use cvm_tracing::CVM_ALLOWED;
use hcl::GuestVtl;
use virt::Processor;
use virt::vp::MpState;
use virt::x86::SegmentRegister;
use virt_support_apic::ApicWork;

pub(crate) trait ApicBacking<'b, B: HardwareIsolatedBacking> {
    fn vp(&mut self) -> &mut UhProcessor<'b, B>;

    fn handle_init(&mut self, vtl: GuestVtl) {
        let vp_info = self.vp().inner.vp_info;
        let mut access = self.vp().access_state(vtl.into());
        virt::vp::x86_init(&mut access, &vp_info).unwrap();
    }

    fn handle_sipi(&mut self, vtl: GuestVtl, cs: SegmentRegister);
    fn handle_nmi(&mut self, vtl: GuestVtl);
    fn handle_interrupt(&mut self, vtl: GuestVtl, vector: u8);

    fn handle_extint(&mut self, vtl: GuestVtl) {
        tracelimit::warn_ratelimited!(CVM_ALLOWED, ?vtl, "extint not supported");
    }
}

pub(crate) fn poll_apic_core<'b, B: HardwareIsolatedBacking, T: ApicBacking<'b, B>>(
    apic_backing: &mut T,
    vtl: GuestVtl,
    scan_irr: bool,
) {
    // Check for interrupt requests from the host and kernel offload.
    if vtl == GuestVtl::Vtl0 {
        if let Some(irr) = apic_backing.vp().runner.proxy_irr_vtl0() {
            // We can't put the interrupts directly into offload (where supported) because we might need
            // to clear the tmr state. This can happen if a vector was previously used for a level
            // triggered interrupt, and is now being used for an edge-triggered interrupt.
            apic_backing.vp().backing.cvm_state_mut().lapics[vtl]
                .lapic
                .request_fixed_interrupts(irr);
        }
    }

    let vp = apic_backing.vp();
    let ApicWork {
        init,
        extint,
        sipi,
        nmi,
        interrupt,
    } = vp.backing.cvm_state_mut().lapics[vtl]
        .lapic
        .scan(&mut vp.vmtime, scan_irr);

    // Check VTL permissions inside each block to avoid taking a lock on the hot path,
    // INIT and SIPI are quite cold.
    if init {
        if !apic_backing
            .vp()
            .cvm_partition()
            .is_lower_vtl_startup_denied()
        {
            apic_backing.handle_init(vtl);
        }
    }

    if let Some(vector) = sipi {
        if apic_backing.vp().backing.cvm_state_mut().lapics[vtl].activity == MpState::WaitForSipi {
            if !apic_backing
                .vp()
                .cvm_partition()
                .is_lower_vtl_startup_denied()
            {
                let base = (vector as u64) << 12;
                let selector = (vector as u16) << 8;
                apic_backing.handle_sipi(
                    vtl,
                    SegmentRegister {
                        base,
                        limit: 0xffff,
                        selector,
                        attributes: 0x9b,
                    },
                );
            }
        }
    }

    // Interrupts are ignored while waiting for SIPI.
    let lapic = &mut apic_backing.vp().backing.cvm_state_mut().lapics[vtl];
    if lapic.activity != MpState::WaitForSipi {
        if nmi || lapic.nmi_pending {
            lapic.nmi_pending = true;
            apic_backing.handle_nmi(vtl);
        }

        if let Some(vector) = interrupt {
            apic_backing.handle_interrupt(vtl, vector);
        }

        if extint {
            apic_backing.handle_extint(vtl);
        }
    }
}
