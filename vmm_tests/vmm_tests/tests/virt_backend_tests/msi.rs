// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the interrupts of a reserved delivery mode.

use super::native::backend_test;
use hvdef::Vtl;
use virt::Processor;
use virt::X86Partition;
use virt::irqcon::DeliveryMode;
use virt::irqcon::MsiRequest;
use virt::vp;
use virt::vp::AccessVpState as _;
use x86defs::apic::ApicBase;

/// The vector of the interrupts of the reserved delivery mode, which must not
/// arrive, and the vectors of the fixed interrupts that follow them by each
/// path, which must.
const RESERVED_VECTOR: u8 = 0x40;
const MSI_VECTOR: u8 = 0x41;
const IOAPIC_VECTOR: u8 = 0x42;

/// The IO-APIC input that the test routes.
const IRQ: u8 = 5;

/// An MSI to the VP, whose APIC ID is 0, with an edge trigger.
fn msi(mode: DeliveryMode, vector: u8) -> MsiRequest {
    MsiRequest::new_x86(mode, 0, false, vector, false)
}

/// Whether the APIC requests `vector`.
fn requests(apic: &vp::Apic, vector: u8) -> bool {
    apic.registers().irr[usize::from(vector / 32)] & (1 << (vector % 32)) != 0
}

/// Software-enables the VP's APIC, since a disabled one drops a fixed
/// interrupt, and halts the VP, which only a backend that holds the halted
/// state of a VP that has not run keeps.
fn halt_with_apic_enabled(processor: &mut impl Processor) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    let apic = state.apic()?;
    let mut registers = *apic.registers();
    registers.svr = 0x1ff;
    state.set_apic(&vp::Apic::new(
        ApicBase::from(apic.apic_base).with_enable(true),
        registers,
        apic.auto_eoi,
    ))?;
    state.set_activity(&vp::Activity {
        mp_state: vp::MpState::Halted,
        ..Default::default()
    })?;
    Ok(())
}

/// Reads the activity and the APIC of the VP, which are what an interrupt
/// changes in a save of it.
fn read(processor: &mut impl Processor) -> anyhow::Result<(vp::Activity, vp::Apic)> {
    let mut state = processor.access_state(Vtl::Vtl0);
    Ok((state.activity()?, state.apic()?))
}

/// Checks that after an interrupt of the reserved delivery mode and then the
/// fixed interrupt `vector`, sent by `path`, the VP's activity is still
/// `before`, and that the APIC requests the fixed interrupt but not the
/// reserved one.
fn ensure_only_fixed_arrived(
    processor: &mut impl Processor,
    before: &vp::Activity,
    path: &str,
    vector: u8,
) -> anyhow::Result<()> {
    let (activity, apic) = read(processor)?;
    anyhow::ensure!(
        activity == *before && requests(&apic, vector) && !requests(&apic, RESERVED_VECTOR),
        "after {path}, the VP is {activity:?} and requests {:x?}",
        apic.registers().irr
    );
    Ok(())
}

// Delivery mode 011 is reserved for an MSI and for an IO-APIC redirection
// entry, both of which a guest can program. KVM takes it as its paravirtual
// kick, which sets a wake-up of the target VP that the saved state cannot
// hold: KVM reads a halted VP as running, and a running VP's next HLT returns
// at once. So no backend may act on an interrupt of that mode that the VMM
// hands over as an MSI or as an IO-APIC input. The VP is halted, which is
// where KVM reports the wake-up. MSHV reads the halted state of a VP that has
// not run as running, so the test checks a running VP there; WHP's hypervisor
// reads it so too, but its backend reports the halted state that it restored
// until the VP runs. The fixed interrupt after each interrupt of the reserved
// mode shows that the path delivers.
backend_test!(a_reserved_delivery_mode_delivers_nothing, requires: []);
fn a_reserved_delivery_mode_delivers_nothing(
    partition: &impl X86Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    halt_with_apic_enabled(processor)?;
    let (before, apic) = read(processor)?;
    anyhow::ensure!(
        apic.registers().irr == [0; 8],
        "a new VP requests {:x?}",
        apic.registers().irr
    );
    let reserved = msi(DeliveryMode::REMOTE_READ, RESERVED_VECTOR);

    partition.request_msi(Vtl::Vtl0, reserved);
    partition.request_msi(Vtl::Vtl0, msi(DeliveryMode::FIXED, MSI_VECTOR));
    ensure_only_fixed_arrived(processor, &before, "an MSI", MSI_VECTOR)?;

    let ioapic = partition.ioapic_routing();
    ioapic.set_irq_route(IRQ, Some(reserved));
    ioapic.assert_irq(IRQ);
    ioapic.set_irq_route(IRQ, Some(msi(DeliveryMode::FIXED, IOAPIC_VECTOR)));
    ioapic.assert_irq(IRQ);
    ensure_only_fixed_arrived(processor, &before, "an IO-APIC input", IOAPIC_VECTOR)?;
    ioapic.set_irq_route(IRQ, None);
    Ok(())
}

// An irqfd injects its MSI when its event is signaled, without the VMM. Only
// KVM owes this contract through one: WHP has no irqfd, and the MSHV driver
// can inject the vector of an irqfd's route by itself, without looking at the
// route's delivery mode, on a host that lets it.
#[cfg(target_os = "linux")]
mod irqfd {
    use super::RESERVED_VECTOR;
    use super::ensure_only_fixed_arrived;
    use super::halt_with_apic_enabled;
    use super::msi;
    use super::read;
    use super::requests;
    use crate::native::backend_test;
    use anyhow::Context as _;
    use std::time::Duration;
    use std::time::Instant;
    use virt::Partition;
    use virt::Processor;
    use virt::irqcon::DeliveryMode;

    const IRQFD_VECTOR: u8 = 0x43;

    /// How long to wait for an interrupt that an irqfd injects.
    const IRQFD_TIMEOUT: Duration = Duration::from_secs(10);

    // KVM can inject for an irqfd after the signal returns, so the fixed
    // interrupt's event follows the reserved one's, and the check waits for the
    // fixed interrupt to arrive.
    backend_test!(a_reserved_delivery_mode_by_irqfd_delivers_nothing, kvm_only, requires: []);
    fn a_reserved_delivery_mode_by_irqfd_delivers_nothing(
        partition: &impl Partition,
        processor: &mut impl Processor,
    ) -> anyhow::Result<()> {
        let irqfd = partition.irqfd().context("KVM has no irqfd")?;
        halt_with_apic_enabled(processor)?;
        let (before, _) = read(processor)?;

        let reserved_route = irqfd.new_irqfd_route().context("no irqfd route")?;
        let fixed_route = irqfd.new_irqfd_route().context("no second irqfd route")?;
        let reserved = msi(DeliveryMode::REMOTE_READ, RESERVED_VECTOR);
        reserved_route.enable(reserved.address, reserved.data, None);
        let fixed = msi(DeliveryMode::FIXED, IRQFD_VECTOR);
        fixed_route.enable(fixed.address, fixed.data, None);
        reserved_route.event().signal();
        fixed_route.event().signal();

        let deadline = Instant::now() + IRQFD_TIMEOUT;
        while !requests(&read(processor)?.1, IRQFD_VECTOR) {
            anyhow::ensure!(
                Instant::now() < deadline,
                "a fixed interrupt did not arrive by irqfd"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        ensure_only_fixed_arrived(processor, &before, "an irqfd", IRQFD_VECTOR)
    }
}
