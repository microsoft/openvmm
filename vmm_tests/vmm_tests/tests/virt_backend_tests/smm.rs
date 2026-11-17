// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Contract tests of the SMI that a guest sends itself, which run a VP on a few
//! bytes of code.

use super::fixture::Fixture;
use anyhow::Context as _;
use hvdef::Vtl;
use std::cell::RefCell;
use virt::BindProcessor as _;
use virt::Partition as _;
use virt::Processor as _;
use virt::StopVpSource;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::irqcon::DeliveryMode;
use virt::irqcon::MsiRequest;
use virt::vp::AccessVpState as _;

/// The RAM that the contract maps: enough for KVM's default SMBASE, 0x30000,
/// and the SMRAM above it.
const RAM_SIZE: u64 = 0x40_0000;
/// Where KVM starts an SMI handler: SMBASE plus 0x8000.
const SMI_HANDLER: u64 = 0x30000 + 0x8000;

/// The devices of a VP that stops at the first write to a port, and records
/// what else it meets, which it does not expect.
struct StopAtPortWrite<'a> {
    stop: &'a StopVpSource,
    port_writes: RefCell<Vec<u16>>,
    unexpected: RefCell<Vec<String>>,
}

impl StopAtPortWrite<'_> {
    fn unexpected(&self, what: String) {
        self.unexpected.borrow_mut().push(what);
        self.stop.stop();
    }
}

impl CpuIo for StopAtPortWrite<'_> {
    fn is_mmio(&self, _address: u64) -> bool {
        false
    }

    fn acknowledge_pic_interrupt(&self) -> Option<u8> {
        None
    }

    fn handle_eoi(&self, irq: u32) {
        self.unexpected(format!("an EOI of {irq}"));
    }

    async fn read_mmio(&self, _vp: VpIndex, address: u64, _data: &mut [u8]) {
        self.unexpected(format!("an MMIO read at {address:#x}"));
    }

    async fn write_mmio(&self, _vp: VpIndex, address: u64, _data: &[u8]) {
        self.unexpected(format!("an MMIO write at {address:#x}"));
    }

    async fn read_io(&self, _vp: VpIndex, port: u16, _data: &mut [u8]) {
        self.unexpected(format!("a read of port {port:#x}"));
    }

    async fn write_io(&self, _vp: VpIndex, port: u16, _data: &[u8]) {
        self.port_writes.borrow_mut().push(port);
        self.stop.stop();
    }

    fn fatal_error(&self, error: Box<dyn std::error::Error + Send + Sync>) -> VpHaltReason {
        self.unexpected(format!("a fatal error: {error:#}"));
        VpHaltReason::PowerOff
    }
}

/// Whether `result` is a save's refusal of a VP that is in SMM or holds an SMI,
/// and not another reason that a save can fail, such as a clear GIF.
fn is_smm_refusal<E: std::error::Error + 'static>(result: &Result<(), E>) -> bool {
    let Err(error) = result else { return false };
    matches!(
        (error as &dyn std::error::Error).downcast_ref::<virt_kvm::KvmError>(),
        Some(virt_kvm::KvmError::SmmState { .. })
    )
}

// A guest can send itself an SMI, as an IPI, an I/O APIC entry, or a device's
// MSI, and KVM takes the VP into SMM when it next runs. KVM saves the state
// that the SMI interrupts at SMBASE, which is in RAM, through its SMM address
// space, and marks the whole VM dead when no RAM is there, which it was while
// OpenVMM mapped RAM in the normal address space alone: the run then failed. So
// the VP must run the guest's handler, at SMBASE in RAM. A VP in SMM holds
// state that a save cannot, so the save must refuse it until its handler
// executes RSM.
async fn a_guest_smi_enters_smm_instead_of_killing_the_vm(
    fixture: &Fixture,
    hv: &mut virt_kvm::Kvm,
) -> anyhow::Result<()> {
    let (partition, mut binder) = fixture.build(hv, false)?;
    // The handler reports itself on port 0x80 and loops: `out 0x80, eax; jmp
    // $-2`, which parks the VP in SMM.
    fixture
        .memory()
        .write_at(SMI_HANDLER, &[0xe7, 0x80, 0xeb, 0xfc])
        .context("failed to write the SMI handler")?;
    let mut processor = binder.bind()?;
    partition.request_msi(
        Vtl::Vtl0,
        MsiRequest::new_x86(DeliveryMode::SMI, 0, false, 0, false),
    );
    // A KVM without SMM drops the SMI, and the VP would run its reset code.
    let held = processor.access_state(Vtl::Vtl0).check_saveable();
    anyhow::ensure!(
        is_smm_refusal(&held),
        "the VP does not hold the SMI that was sent to it, so this host's KVM has no SMM: {held:?}"
    );

    let stop = StopVpSource::new();
    let io = StopAtPortWrite {
        stop: &stop,
        port_writes: RefCell::new(Vec::new()),
        unexpected: RefCell::new(Vec::new()),
    };
    let halt = processor.run_vp(stop.checker(), &io).await;
    anyhow::ensure!(
        io.unexpected.borrow().is_empty(),
        "the VP met {:?} instead of running the SMI's handler",
        io.unexpected.borrow()
    );
    anyhow::ensure!(
        matches!(halt, Err(VpHaltReason::Stop(_))),
        "the VP halted instead of stopping: {halt:?}"
    );
    anyhow::ensure!(
        *io.port_writes.borrow() == [0x80],
        "the VP did not run the SMI's handler: it wrote ports {:x?}",
        io.port_writes.borrow()
    );

    let saved = processor.access_state(Vtl::Vtl0).check_saveable();
    anyhow::ensure!(
        is_smm_refusal(&saved),
        "a VP in the SMI's handler is not refused as a VP in SMM: {saved:?}"
    );
    Ok(())
}

// This contract runs the VP with room for SMBASE in RAM, so it registers by
// hand, with what `backend_test!` registers for KVM.
petri::multitest!(vec![crate::native::kvm::test(
    "kvm::a_guest_smi_enters_smm_instead_of_killing_the_vm",
    false,
    || {
        // Evaluate the capabilities, as `backend_test!` does, so that a failed
        // probe fails the test.
        let _ = crate::native::kvm::capabilities(false);
        true
    },
    async |driver, mut hv| {
        a_guest_smi_enters_smm_instead_of_killing_the_vm(
            &Fixture::with_ram(driver, RAM_SIZE).await?,
            &mut hv,
        )
        .await
    },
)]);
