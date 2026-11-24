// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Contract tests of the paravirtual kick that a guest sends its own VP, which
//! run a VP on a few bytes of code.

use super::fixture::Fixture;
use anyhow::Context as _;
use futures::executor::block_on;
use hvdef::Vtl;
use std::cell::RefCell;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use virt::BindProcessor as _;
use virt::Partition;
use virt::Processor;
use virt::StopVpSource;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::vp;
use virt::vp::AccessVpState as _;
use virt::x86::SegmentRegister;

/// Where the guest's code starts, in the fixture's page.
const CODE: u16 = 0x400;
/// The port that the guest writes its marks to.
const MARK_PORT: u8 = 0x80;
/// The guest's marks: once it has kicked its VP, and once it runs on.
const KICKED_MARK: u8 = b'K';
const RAN_ON_MARK: u8 = b'R';
/// KVM's hypercall that kicks the VP whose APIC ID is its second argument.
const KVM_HC_KICK_CPU: u32 = 5;
/// How long the halted VP is watched for running on. A VP that KVM wakes runs
/// at once.
const HALTED_FOR: Duration = Duration::from_secs(1);
/// How long to wait for a VP to do what is certain to happen.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The guest's code, in real mode: it kicks its own VP, whose APIC ID is 0,
/// with the hypercall instruction of the host's vendor, keeps the hypercall's
/// result in ESI, marks, marks again when it runs on, and halts with
/// interrupts disabled. The VP does not halt between the kick and the first
/// mark, so KVM holds the kick when the VP stops there.
fn code(amd: bool) -> Vec<u8> {
    let mut code = vec![0x66, 0xb8]; // mov eax, KVM_HC_KICK_CPU
    code.extend(KVM_HC_KICK_CPU.to_le_bytes());
    code.extend([0x66, 0x31, 0xdb]); // xor ebx, ebx
    code.extend([0x66, 0x31, 0xc9]); // xor ecx, ecx
    code.extend(if amd {
        [0x0f, 0x01, 0xd9] // vmmcall
    } else {
        [0x0f, 0x01, 0xc1] // vmcall
    });
    code.extend([0x66, 0x89, 0xc6]); // mov esi, eax
    for mark in [KICKED_MARK, RAN_ON_MARK] {
        code.extend([0xb0, mark, 0xe6, MARK_PORT]); // mov al, mark; out MARK_PORT, al
    }
    code.extend([0xfa, 0xf4]); // cli; hlt
    code
}

/// The guest's side of the VP's I/O: it stops the VP at each mark that the
/// guest writes, and records what else the VP meets, which it does not expect.
struct GuestIo<'a> {
    stop: &'a StopVpSource,
    marks: RefCell<Vec<u8>>,
    unexpected: RefCell<Vec<String>>,
}

impl GuestIo<'_> {
    fn unexpected(&self, what: String) {
        self.unexpected.borrow_mut().push(what);
        self.stop.stop();
    }
}

impl CpuIo for GuestIo<'_> {
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

    async fn write_io(&self, _vp: VpIndex, port: u16, data: &[u8]) {
        match data.first() {
            Some(&mark) if port == u16::from(MARK_PORT) => {
                self.marks.borrow_mut().push(mark);
                self.stop.stop();
            }
            _ => self.unexpected(format!("a write of {data:x?} to port {port:#x}")),
        }
    }

    fn fatal_error(&self, error: Box<dyn std::error::Error + Send + Sync>) -> VpHaltReason {
        self.unexpected(format!("a fatal error: {error:#}"));
        VpHaltReason::PowerOff
    }
}

/// Runs the VP until the guest writes a mark, or until `limit` passes, and
/// returns the marks that the guest wrote.
fn run(
    partition: &impl Partition,
    processor: &mut impl Processor,
    limit: Duration,
) -> anyhow::Result<Vec<u8>> {
    let stop = StopVpSource::new();
    let io = GuestIo {
        stop: &stop,
        marks: RefCell::new(Vec::new()),
        unexpected: RefCell::new(Vec::new()),
    };
    let timed_out = &AtomicBool::new(false);
    let halt = std::thread::scope(|scope| {
        let (done, wait_done) = mpsc::channel::<()>();
        scope.spawn(move || {
            // A VP that KVM keeps halted runs no code, so only a cancelled
            // run stops it.
            if let Err(mpsc::RecvTimeoutError::Timeout) = wait_done.recv_timeout(limit) {
                timed_out.store(true, Ordering::SeqCst);
                partition.request_yield(VpIndex::BSP);
            }
        });
        let mut run = pin!(processor.run_vp(stop.checker(), &io));
        let halt = block_on(poll_fn(|cx| {
            if timed_out.load(Ordering::SeqCst) {
                stop.stop();
            }
            run.as_mut().poll(cx)
        }));
        drop(done);
        halt
    });
    anyhow::ensure!(
        io.unexpected.borrow().is_empty(),
        "the VP met {:?}",
        io.unexpected.borrow()
    );
    match halt {
        Err(VpHaltReason::Stop(_)) => {}
        Err(reason) => anyhow::bail!("the VP halted: {reason:?}"),
        Ok(never) => match never {},
    }
    Ok(io.marks.into_inner())
}

/// Whether `error` is the refusal to halt a VP that holds a kick, which KVM
/// cannot clear.
fn is_kick_refusal<E: std::error::Error + 'static>(error: &E) -> bool {
    matches!(
        (error as &dyn std::error::Error).downcast_ref::<virt_kvm::KvmError>(),
        Some(virt_kvm::KvmError::PvKick { vp_index: 0 })
    )
}

/// The host's Linux release, and whether its KVM clears the kick of a VP that
/// it sets runnable, which it does from Linux 6.15 on.
fn linux_clears_kicks() -> anyhow::Result<(String, bool)> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .context("failed to read the host's Linux release")?;
    let release = release.trim().to_owned();
    let mut numbers = release
        .split(|c: char| !c.is_ascii_digit())
        .map(str::parse::<u32>);
    let (Some(Ok(major)), Some(Ok(minor))) = (numbers.next(), numbers.next()) else {
        anyhow::bail!("unrecognized Linux release {release:?}");
    };
    Ok((release, (major, minor) >= (6, 15)))
}

// A guest can kick a VP, its own included, with KVM's KVM_HC_KICK_CPU, which
// KVM serves unless it checks the guest's CPUID, as it does only for a
// partition with a time ABI, or with an IPI in the remote-read delivery mode.
// KVM holds the kick until it wakes the VP from a halt: it reads a halted VP
// that holds one as running, and runs it on at once. A saved state holds no
// kick, so a restore of a VP that was saved halted, onto a VP that has run and
// holds a kick, must leave the VP halted. KVM clears the kick of a VP that it
// sets runnable from Linux 6.15 on, and nothing clears it before, where the
// restore must refuse the VP instead.
fn a_vp_set_halted_stays_halted_over_its_kick(
    fixture: &Fixture,
    hv: &mut virt_kvm::Kvm,
) -> anyhow::Result<()> {
    let (partition, mut binder) = fixture.build(hv, false)?;
    let amd = partition.caps().vendor.is_amd_compatible();
    fixture
        .memory()
        .write_at(CODE.into(), &code(amd))
        .context("failed to write the guest")?;
    let mut processor = binder.bind()?;
    {
        let mut state = processor.access_state(Vtl::Vtl0);
        let registers = state.registers()?;
        state.set_registers(&vp::Registers {
            cs: SegmentRegister {
                base: 0,
                limit: 0xffff,
                selector: 0,
                attributes: 0x9b,
            },
            rip: CODE.into(),
            ..registers
        })?;
    }

    let marks = run(&partition, &mut processor, TIMEOUT)?;
    anyhow::ensure!(
        marks == [KICKED_MARK],
        "the guest did not mark its kick: it wrote {marks:x?}"
    );
    // The hypercall returns 0 once KVM has sent the kick.
    let result = processor.access_state(Vtl::Vtl0).registers()?.rsi;
    anyhow::ensure!(result == 0, "KVM_HC_KICK_CPU returned {result:#x}");

    let halted = vp::Activity {
        mp_state: vp::MpState::Halted,
        ..Default::default()
    };
    let mut state = processor.access_state(Vtl::Vtl0);
    if let Err(error) = state.set_activity(&halted) {
        let (release, clears) = linux_clears_kicks()?;
        anyhow::ensure!(
            is_kick_refusal(&error) && !clears,
            "setting the VP halted on Linux {release} failed: {error:#}"
        );
        // KVM reads the VP that it holds halted as running.
        let mp_state = state.activity()?.mp_state;
        anyhow::ensure!(
            mp_state == vp::MpState::Running,
            "the VP whose halt was refused reads as {mp_state:?}"
        );
        tracing::info!(%release, "this KVM cannot clear the kick, so the halt was refused");
        return Ok(());
    }
    let activity = state.activity()?;
    drop(state);
    let marks = run(&partition, &mut processor, HALTED_FOR)?;
    anyhow::ensure!(
        activity == halted && marks.is_empty(),
        "the VP that was set halted over its kick reads as {:?}, and it wrote {marks:x?}",
        activity.mp_state
    );

    // The VP does run on once it is set running, so the watch would have seen
    // a VP that the kick woke.
    processor
        .access_state(Vtl::Vtl0)
        .set_activity(&vp::Activity::default())?;
    let marks = run(&partition, &mut processor, TIMEOUT)?;
    anyhow::ensure!(
        marks == [RAN_ON_MARK],
        "the VP that was set running wrote {marks:x?}"
    );
    Ok(())
}

// This contract runs the VP on code that it writes into the fixture's RAM, so
// it registers by hand, with what `backend_test!` registers for KVM.
petri::multitest!(vec![crate::native::kvm::test(
    "kvm::a_vp_set_halted_stays_halted_over_its_kick",
    false,
    || {
        // Evaluate the capabilities, as `backend_test!` does, so that a failed
        // probe fails the test.
        let _ = crate::native::kvm::capabilities(false);
        true
    },
    async |driver, mut hv| {
        a_vp_set_halted_stays_halted_over_its_kick(&Fixture::new(driver).await?, &mut hv)
    },
)]);
