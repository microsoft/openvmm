// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of a restore of a VP's whole saved state.
//!
//! A debug build of `restore_all` reads back each element that it restores and
//! asserts that it is the one that it set, up to the normalization that the
//! element accepts for the backend.

use super::native::backend_test;
use anyhow::Context as _;
use hvdef::Vtl;
use mesh::payload::Protobuf;
use virt::Partition;
use virt::Processor;
use virt::vp;
use virt::vp::AccessVpState as _;

/// The elements of a VP's saved state that these tests craft, under the field
/// numbers that the saved state gives them.
#[derive(Default, Protobuf)]
struct Crafted {
    #[mesh(1)]
    registers: Option<vp::Registers>,
    #[mesh(2)]
    activity: Option<vp::Activity>,
    #[mesh(9)]
    msrs: Option<vp::VirtualMsrs>,
}

impl Crafted {
    fn into_saved_state(self) -> anyhow::Result<vp::VpSavedState> {
        mesh::payload::decode(&mesh::payload::encode(self))
            .context("the crafted state is not a saved VP state")
    }
}

// Without KVM_CAP_EXCEPTION_PAYLOAD, KVM reports an exception that it has
// queued but not injected yet as pending and injected both, as a save recorded
// it, and restores it once, as injected, so that it reads back as the
// pending interruption alone.
backend_test!(restore_accepts_a_queued_exception_that_a_save_recorded_twice, kvm_only, requires: []);
fn restore_accepts_a_queued_exception_that_a_save_recorded_twice(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    for error_code in [None, Some(0)] {
        let queued = vp::Activity {
            pending_event: Some(vp::PendingEvent::Exception {
                vector: 13,
                error_code,
                parameter: 0,
            }),
            pending_interruption: Some(vp::PendingInterruption::Exception {
                vector: 13,
                error_code,
            }),
            ..Default::default()
        };
        let saved = Crafted {
            activity: Some(queued),
            ..Default::default()
        }
        .into_saved_state()?;
        state
            .restore_all(&saved)
            .with_context(|| format!("restoring an exception with error code {error_code:?}"))?;

        let injected = vp::Activity {
            pending_interruption: Some(vp::PendingInterruption::Exception {
                vector: 13,
                error_code,
            }),
            ..Default::default()
        };
        let restored = state.activity()?;
        anyhow::ensure!(
            restored == injected,
            "the restored exception reads back as {restored:?}"
        );
    }
    Ok(())
}

// KVM keeps of a segment's attributes only the fields of its segment format, so
// reserved bits read back clear. A CPU with VMX that runs the guest
// unrestricted has it set the accessed bit of a usable segment, and an SVM CPU's
// reads set it of a data segment that is present, while the backend clears it
// of a segment that is not present. KVM keeps SYSENTER_EIP and SYSENTER_ESP
// canonical, and SYSENTER_CS in 32 bits on a CPU with VMX.
backend_test!(restore_accepts_the_state_that_kvm_normalizes, kvm_only, requires: []);
fn restore_accepts_the_state_that_kvm_normalizes(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    // The fixture's CPUID is the host's, so an AMD CPU, which has SVM, is a
    // guest whose CPU is AMD-compatible.
    let vmx = !partition.caps().vendor.is_amd_compatible();
    let mut state = processor.access_state(Vtl::Vtl0);

    let mut registers = state.registers()?;
    // Reserved bits 8-11, and a data segment whose accessed bit is clear.
    registers.es.attributes = 0x0f93;
    registers.ds.attributes = 0x0092;
    // A segment that is not present, whose accessed bit an older save held,
    // since KVM set it.
    registers.gs.attributes = 0x4011;

    let mut msrs = state.virtual_msrs()?;
    // An address that is canonical at no width, and a SYSENTER_CS wider than
    // VMX keeps.
    msrs.sysenter_eip = 0x8000_0000_0000_1000;
    msrs.sysenter_esp = 0x8000_0000_0000_2000;
    msrs.sysenter_cs = 0x1_0000_0010;

    let saved = Crafted {
        registers: Some(registers),
        msrs: Some(msrs),
        ..Default::default()
    }
    .into_saved_state()?;
    state
        .restore_all(&saved)
        .context("restoring state that KVM normalizes failed")?;

    // KVM did change it, as the restore's check allows. A CPU with VMX sets
    // the data segment's accessed bit only when it runs the guest unrestricted.
    let registers = state.registers()?;
    let ds_attributes: &[u16] = if vmx { &[0x0092, 0x0093] } else { &[0x0093] };
    anyhow::ensure!(
        registers.es.attributes == 0x0093
            && ds_attributes.contains(&registers.ds.attributes)
            && registers.gs.attributes == 0x4010,
        "the segment attributes read back as {:#x}, {:#x}, and {:#x}",
        registers.es.attributes,
        registers.ds.attributes,
        registers.gs.attributes
    );
    let msrs = state.virtual_msrs()?;
    anyhow::ensure!(
        (msrs.sysenter_eip, msrs.sysenter_esp) == (0x1000, 0x2000),
        "SYSENTER_EIP and SYSENTER_ESP read back as {:#x} and {:#x}",
        msrs.sysenter_eip,
        msrs.sysenter_esp
    );
    let sysenter_cs = if vmx { 0x10 } else { 0x1_0000_0010 };
    anyhow::ensure!(
        msrs.sysenter_cs == sysenter_cs,
        "SYSENTER_CS reads back as {:#x}, not {sysenter_cs:#x}",
        msrs.sysenter_cs
    );
    Ok(())
}

// A save names the kind of an interrupt shadow only for an STI shadow, and a
// restore of a shadow that is given no kind sets a MOV SS shadow, so the shadow
// of an earlier save, which has no kind, and an STI shadow read back as they
// were saved, and the restore's check accepts them as they are. An SVM CPU has
// one kind of shadow, which a save holds no kind for.
backend_test!(restore_reads_back_an_interrupt_shadow_as_saved, kvm_only, requires: []);
fn restore_reads_back_an_interrupt_shadow_as_saved(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut kinds = vec![None];
    if partition.caps().vendor.is_intel_compatible() {
        kinds.push(Some(vp::InterruptShadowKind::Sti));
    }
    let mut state = processor.access_state(Vtl::Vtl0);
    for interrupt_shadow_kind in kinds {
        let activity = || vp::Activity {
            interrupt_shadow: true,
            interrupt_shadow_kind,
            ..Default::default()
        };
        let saved = Crafted {
            activity: Some(activity()),
            ..Default::default()
        }
        .into_saved_state()?;
        state.restore_all(&saved).with_context(|| {
            format!("restoring an interrupt shadow of kind {interrupt_shadow_kind:?}")
        })?;

        let restored = state.activity()?;
        anyhow::ensure!(
            restored == activity(),
            "the restored interrupt shadow reads back as {restored:?}"
        );
    }
    Ok(())
}
