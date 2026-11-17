// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Contract tests of the suspend state that a restore keeps.
//!
//! A VP that was saved halted, or idle, runs no instruction when it is
//! restored, until something wakes it, and then resumes after the instruction
//! that suspended it. The guest of these tests is code that suspends its own
//! VP, in real mode, and in 64-bit mode with paging for one case, since a
//! hypervisor does not keep a suspend state that the VMM writes, and a VP that
//! was never run has none of its own.

use super::fixture::Fixture;
use anyhow::Context as _;
use futures::executor::block_on;
use hvdef::Vtl;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;
use virt::BindProcessor as _;
use virt::Hypervisor;
use virt::Partition;
use virt::PartitionMemoryMapper;
use virt::Processor;
use virt::StopVpSource;
use virt::VpHaltReason;
use virt::io::CpuIo;
use virt::irqcon::DeliveryMode;
use virt::irqcon::MsiRequest;
use virt::vp;
use virt::vp::AccessVpState as _;
use virt::x86::SegmentRegister;
use vm_topology::processor::VpIndex;
use x86defs::apic::ApicBase;

/// The port that the guest writes its marks to.
const MARK_PORT: u16 = 0x80;
/// Where the guest starts, and the top of its stack, in the fixture's page.
const CODE: u16 = 0x400;
const STACK: u64 = 0x800;
/// The vector of the interrupt that wakes a VP with interrupts enabled.
const VECTOR: u8 = 0x30;
/// The handlers of the NMI and of that interrupt. Each marks and returns.
const NMI_HANDLER: u16 = 0x500;
const VECTOR_HANDLER: u16 = 0x510;

/// How long a restored VP is watched for running a guest instruction. A VP
/// that the hypervisor woke runs one at once.
const SUSPENDED_FOR: Duration = Duration::from_secs(1);
/// How long to wait for a VP to do what is certain to happen.
const TIMEOUT: Duration = Duration::from_secs(10);

/// What a restored VP's guest does, in the order of its marks.
const SUSPEND_MARK: u8 = b'H';
const WOKEN_MARK: u8 = b'W';
const NMI_MARK: u8 = b'N';
const VECTOR_MARK: u8 = b'I';

/// How a guest suspends its VP.
#[derive(Clone, Copy, Debug)]
enum Suspend {
    /// `hlt`, with the interrupts disabled or enabled.
    Halt { interrupts: bool },
    /// `rdmsr` of the Hyper-V guest idle MSR.
    Idle,
}

impl Suspend {
    fn mp_state(self) -> vp::MpState {
        match self {
            Self::Halt { .. } => vp::MpState::Halted,
            Self::Idle => vp::MpState::Idle,
        }
    }

    /// The guest's code: it marks, suspends, marks again once something has
    /// woken the VP, and halts with the interrupts disabled.
    fn code(self) -> Vec<u8> {
        let mark = |mark: u8| [0xb0, mark, 0xe6, MARK_PORT as u8]; // mov al, mark; out MARK_PORT, al
        let mut code = mark(SUSPEND_MARK).to_vec();
        match self {
            Self::Halt { interrupts } => {
                code.push(if interrupts { 0xfb } else { 0xfa }); // sti or cli
                code.push(0xf4); // hlt
            }
            Self::Idle => {
                code.extend([0x66, 0xb9]); // mov ecx, HV_X64_MSR_GUEST_IDLE
                code.extend(hvdef::HV_X64_MSR_GUEST_IDLE.to_le_bytes());
                code.extend([0x0f, 0x32]); // rdmsr
            }
        }
        code.extend(mark(WOKEN_MARK));
        code.extend([0xfa, 0xf4]); // cli; hlt
        code
    }
}

/// The mode that the guest's code runs in.
#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Real mode, with paging off, as a VP starts.
    Real,
    /// 64-bit mode with paging on, through page tables that are the fixture's
    /// page alone: its first entry maps the page at every level, so that
    /// translating the address of the code walks four levels of tables.
    Long,
}

/// What wakes the restored VP.
#[derive(Clone, Copy, Debug)]
enum Wake {
    Nmi,
    /// The fixed interrupt, which a VP with interrupts disabled holds back.
    Vector,
    /// Nothing, as the guest of the 64-bit mode has no handlers: the VP is only
    /// watched for running on.
    Never,
}

impl Wake {
    fn request(self, partition: &impl Partition) {
        let (mode, vector) = match self {
            Self::Nmi => (DeliveryMode::NMI, 0),
            Self::Vector => (DeliveryMode::FIXED, VECTOR),
            Self::Never => return,
        };
        partition.request_msi(
            Vtl::Vtl0,
            MsiRequest::new_x86(mode, 0, false, vector, false),
        );
    }

    /// The marks of a VP that the wake-up resumed after its suspending
    /// instruction: the handler's, and the guest's.
    fn marks(self) -> Option<[u8; 2]> {
        match self {
            Self::Nmi => Some([NMI_MARK, WOKEN_MARK]),
            Self::Vector => Some([VECTOR_MARK, WOKEN_MARK]),
            Self::Never => None,
        }
    }
}

/// Where the saved state is restored.
#[derive(Clone, Copy, Debug)]
enum Target {
    /// The VP that was saved, which suspended itself.
    SameVp,
    /// A VP of a new partition, as a snapshot restore gives it.
    FreshPartition,
}

/// When the wake-up reaches the restored VP.
#[derive(Clone, Copy, Debug)]
enum When {
    /// After the VP ran, and has suspended itself again.
    AfterRun,
    /// Before the VP runs, so that it is pending when the VP first runs, as
    /// the vector of a LAPIC timer that expired during the downtime is.
    BeforeRun,
}

/// One case: the mode of the guest, how it suspends its VP, where the saved
/// state is restored, and what wakes the restored VP, when.
#[derive(Clone, Copy, Debug)]
struct Case {
    mode: Mode,
    suspend: Suspend,
    target: Target,
    wake: Wake,
    when: When,
}

/// The guest's side of the VP's I/O: the marks that it writes.
struct GuestIo {
    marks: mpsc::Sender<u8>,
}

/// The marks that the guest has written, as the test watches them.
struct Marks {
    receiver: mpsc::Receiver<u8>,
    seen: Vec<u8>,
}

fn guest_io() -> (GuestIo, Marks) {
    let (sender, receiver) = mpsc::channel();
    (
        GuestIo { marks: sender },
        Marks {
            receiver,
            seen: Vec::new(),
        },
    )
}

impl Marks {
    /// Waits until `done` holds of the marks, or `timeout` passes, and returns
    /// the marks.
    fn wait(&mut self, timeout: Duration, done: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        while !done(&self.seen) {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match self.receiver.recv_timeout(left) {
                Ok(mark) => self.seen.push(mark),
                Err(_) => break,
            }
        }
        self.seen.clone()
    }
}

impl CpuIo for GuestIo {
    fn is_mmio(&self, _address: u64) -> bool {
        false
    }

    fn acknowledge_pic_interrupt(&self) -> Option<u8> {
        None
    }

    fn handle_eoi(&self, _irq: u32) {}

    async fn read_mmio(&self, _vp: VpIndex, _address: u64, data: &mut [u8]) {
        data.fill(!0);
    }

    async fn write_mmio(&self, _vp: VpIndex, _address: u64, _data: &[u8]) {}

    async fn read_io(&self, _vp: VpIndex, _port: u16, data: &mut [u8]) {
        data.fill(!0);
    }

    async fn write_io(&self, _vp: VpIndex, port: u16, data: &[u8]) {
        if let (MARK_PORT, Some(&mark)) = (port, data.first()) {
            // The test stops watching once it has what it waits for.
            let _ = self.marks.send(mark);
        }
    }

    fn fatal_error(&self, error: Box<dyn std::error::Error + Send + Sync>) -> VpHaltReason {
        panic!("fatal VP error: {error:#}");
    }
}

/// Runs the VP on this thread while `watch` runs on another one, and stops the
/// VP once `watch` returns, with the partition's time running only while it
/// runs.
fn run_while<T: Send>(
    partition: &impl Partition,
    processor: &mut impl Processor,
    io: &GuestIo,
    watch: impl FnOnce() -> T + Send,
) -> anyhow::Result<T> {
    let time = partition
        .supports_time_control()
        .context("the backend cannot freeze time, which the VP's state access needs")?;
    let stop = StopVpSource::new();
    let stopped = AtomicBool::new(false);
    time.thaw_time();
    let (halt, watched) = std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            let watched = watch();
            stopped.store(true, Ordering::SeqCst);
            // The VP can be suspended in the hypervisor, which only a
            // cancelled run leaves.
            partition.request_yield(VpIndex::BSP);
            watched
        });
        let mut run = pin!(processor.run_vp(stop.checker(), io));
        let halt = block_on(poll_fn(|cx| {
            if stopped.load(Ordering::SeqCst) {
                stop.stop();
            }
            run.as_mut().poll(cx)
        }));
        (halt, watcher.join())
    });
    time.freeze_time();
    match halt {
        Err(VpHaltReason::Stop(_)) => {}
        Err(reason) => anyhow::bail!("the VP halted: {reason:?}"),
        Ok(never) => match never {},
    }
    watched.map_err(|_| anyhow::anyhow!("the watcher of the VP panicked"))
}

/// Writes the guest into the fixture's page: the code of `case`, and the
/// handlers of the NMI and of the interrupt, which real-mode vectors point at,
/// and, for the 64-bit mode, the page tables.
fn write_guest(fixture: &Fixture, case: Case) -> anyhow::Result<()> {
    let memory = fixture.memory();
    let vector = |vector: u8, handler: u16| {
        let [low, high] = handler.to_le_bytes();
        memory.write_at(u64::from(vector) * 4, &[low, high, 0, 0])
    };
    vector(2, NMI_HANDLER)?;
    vector(VECTOR, VECTOR_HANDLER)?;
    for (handler, mark) in [(NMI_HANDLER, NMI_MARK), (VECTOR_HANDLER, VECTOR_MARK)] {
        // mov al, mark; out MARK_PORT, al; iret
        memory.write_at(handler.into(), &[0xb0, mark, 0xe6, MARK_PORT as u8, 0xcf])?;
    }
    memory.write_at(CODE.into(), &case.suspend.code())?;
    if let Mode::Long = case.mode {
        // Present and writable, and the page itself at every level.
        memory.write_at(0, &0x3u64.to_le_bytes())?;
    }
    Ok(())
}

/// Points the VP at the guest's code, in the guest's mode, and software-enables
/// its APIC, since a disabled one drops the fixed interrupt.
fn start_guest(processor: &mut impl Processor, mode: Mode) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    let registers = state.registers()?;
    state.set_registers(&match mode {
        Mode::Real => vp::Registers {
            cs: SegmentRegister {
                base: 0,
                limit: 0xffff,
                selector: 0,
                attributes: 0x9b,
            },
            rip: CODE.into(),
            rsp: STACK,
            ..registers
        },
        Mode::Long => vp::Registers {
            // A 64-bit code segment, which no descriptor table holds, since
            // nothing reloads it.
            cs: SegmentRegister {
                base: 0,
                limit: !0,
                selector: 8,
                attributes: 0xa09b,
            },
            cr0: x86defs::X64_CR0_PG
                | x86defs::X64_CR0_NE
                | x86defs::X64_CR0_ET
                | x86defs::X64_CR0_PE,
            cr3: 0,
            cr4: x86defs::X64_CR4_PAE,
            efer: x86defs::X64_EFER_LME | x86defs::X64_EFER_LMA,
            rip: CODE.into(),
            rsp: STACK,
            ..registers
        },
    })?;
    let apic = state.apic()?;
    let mut registers = *apic.registers();
    registers.svr = 0x1ff;
    state.set_apic(&vp::Apic::new(
        ApicBase::from(apic.apic_base).with_enable(true),
        registers,
        apic.auto_eoi,
    ))?;
    Ok(())
}

/// Runs the guest until its VP suspends itself, and stops it, in the state
/// that `suspend` gives it.
fn run_until_suspended(
    partition: &impl Partition,
    processor: &mut impl Processor,
    suspend: Suspend,
) -> anyhow::Result<()> {
    let (io, mut watch) = guest_io();
    let marks = run_while(partition, processor, &io, || {
        watch.wait(TIMEOUT, |marks| marks == [SUSPEND_MARK])
    })?;
    anyhow::ensure!(
        marks == [SUSPEND_MARK],
        "the guest marked {marks:?}, not {SUSPEND_MARK:?}"
    );
    // The guest suspends itself within a few instructions of its mark, which
    // the stop may not have waited for.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let activity = processor.access_state(Vtl::Vtl0).activity()?;
        if activity.mp_state == suspend.mp_state() {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the VP is {:?}, not {:?}, after its guest marked",
            activity.mp_state,
            suspend.mp_state()
        );
        run_while(partition, processor, &io, || {
            std::thread::sleep(Duration::from_millis(10))
        })?;
    }
}

/// Restores `saved` into the VP, which must read back as suspended, and checks
/// that the guest runs nothing until the wake-up, and then resumes after its
/// suspending instruction.
fn check_restored(
    partition: &impl Partition,
    processor: &mut impl Processor,
    saved: &vp::VpSavedState,
    case: Case,
) -> anyhow::Result<()> {
    let expected = case.suspend.mp_state();
    let restored = {
        let mut state = processor.access_state(Vtl::Vtl0);
        state
            .restore_all(saved)
            .context("restoring the saved state failed")?;
        state.activity()?
    };
    anyhow::ensure!(
        restored.mp_state == expected,
        "the VP reads {:?} after the restore, not {expected:?}",
        restored.mp_state
    );

    if let When::BeforeRun = case.when {
        case.wake.request(partition);
    }
    let (io, mut watch) = guest_io();
    let Some(woken) = case.wake.marks() else {
        let marks = run_while(partition, processor, &io, || {
            watch.wait(SUSPENDED_FOR, |marks| !marks.is_empty())
        })?;
        anyhow::ensure!(
            marks.is_empty(),
            "the VP marked {:?} after {case:?}: it was running past its suspending instruction",
            String::from_utf8_lossy(&marks)
        );
        return Ok(());
    };
    let marks = run_while(partition, processor, &io, || {
        if let When::AfterRun = case.when {
            let marks = watch.wait(SUSPENDED_FOR, |marks| !marks.is_empty());
            if !marks.is_empty() {
                return marks;
            }
            case.wake.request(partition);
        }
        watch.wait(TIMEOUT, |marks| marks == woken)
    })?;
    anyhow::ensure!(
        marks == woken,
        "the VP marked {:?} after {case:?}, not {:?}: it was {}",
        String::from_utf8_lossy(&marks),
        String::from_utf8_lossy(&woken),
        if marks.starts_with(&[WOKEN_MARK]) {
            "running past its suspending instruction before the wake-up"
        } else {
            "not resumed after its suspending instruction by the wake-up"
        }
    );
    Ok(())
}

fn run_case<H: Hypervisor>(
    fixture: &Fixture,
    hv: &mut H,
    hv1: bool,
    case: Case,
) -> anyhow::Result<()>
where
    H::Partition: Partition + PartitionMemoryMapper,
{
    write_guest(fixture, case)?;

    let (partition, mut binder) = fixture.build(hv, hv1)?;
    let mut processor = binder.bind()?;
    start_guest(&mut processor, case.mode)?;
    run_until_suspended(&partition, &mut processor, case.suspend)?;
    let saved = processor
        .access_state(Vtl::Vtl0)
        .save_all()
        .context("saving the suspended VP failed")?;

    match case.target {
        Target::SameVp => check_restored(&partition, &mut processor, &saved, case),
        Target::FreshPartition => {
            drop(processor);
            drop(binder);
            drop(partition);
            let (partition, mut binder) = fixture.build(hv, hv1)?;
            let mut processor = binder.bind()?;
            check_restored(&partition, &mut processor, &saved, case)
        }
    }
}

fn supported() -> bool {
    // Evaluate the capabilities, which the test reads, as `backend_test!` does,
    // so that a failed probe fails the test.
    let _ = crate::native::whp::capabilities(false);
    true
}

fn supported_hv1() -> bool {
    let _ = crate::native::whp::capabilities(true);
    true
}

/// A WHP test of `case`. These register by hand, since a case builds a
/// second partition, and runs a guest.
fn whp_test(name: &'static str, hv1: bool, case: Case) -> petri::TestCase {
    crate::native::whp::test(
        name,
        hv1,
        if hv1 { supported_hv1 } else { supported },
        async move |driver, mut hv| run_case(&Fixture::new(driver).await?, &mut hv, hv1, case),
    )
}

// With the hypervisor's APIC, WHP does not keep a suspend state that the VMM
// writes once it has written more of the VP's state, as a restore does: the
// restored VP ran past its `hlt`, though it read back as halted. A guest that
// halts with interrupts disabled, as Linux does an offlined processor, never
// expects that. A restored VP must stay suspended, whether the restore is into
// the VP that was saved or into a new partition's, as a snapshot's is, until
// something wakes it, and then resume after its suspending instruction. That
// holds when the wake-up is already pending as the VP first runs too, as the
// vector of a LAPIC timer that expired during the downtime is: taken before
// the `hlt`, it would return to it and leave the VP suspended with nothing to
// wake it. The cases register for WHP alone; KVM and MSHV have none yet. One
// case runs the guest in 64-bit mode with paging, as Linux does, so that
// finding the instruction before RIP takes a walk of page tables.
petri::multitest!(vec![
    whp_test(
        "whp::halted_vp_with_interrupts_off_stays_halted_in_a_fresh_partition",
        false,
        Case {
            mode: Mode::Real,
            suspend: Suspend::Halt { interrupts: false },
            target: Target::FreshPartition,
            wake: Wake::Nmi,
            when: When::AfterRun,
        },
    ),
    whp_test(
        "whp::halted_vp_with_interrupts_on_stays_halted_in_a_fresh_partition",
        false,
        Case {
            mode: Mode::Real,
            suspend: Suspend::Halt { interrupts: true },
            target: Target::FreshPartition,
            wake: Wake::Vector,
            when: When::AfterRun,
        },
    ),
    whp_test(
        "whp::halted_vp_with_interrupts_off_stays_halted_when_restored_onto_itself",
        false,
        Case {
            mode: Mode::Real,
            suspend: Suspend::Halt { interrupts: false },
            target: Target::SameVp,
            wake: Wake::Nmi,
            when: When::AfterRun,
        },
    ),
    whp_test(
        "whp::halted_vp_with_interrupts_on_stays_halted_when_restored_onto_itself",
        false,
        Case {
            mode: Mode::Real,
            suspend: Suspend::Halt { interrupts: true },
            target: Target::SameVp,
            wake: Wake::Vector,
            when: When::AfterRun,
        },
    ),
    whp_test(
        "whp::halted_vp_takes_a_pending_nmi_after_its_hlt",
        false,
        Case {
            mode: Mode::Real,
            suspend: Suspend::Halt { interrupts: false },
            target: Target::FreshPartition,
            wake: Wake::Nmi,
            when: When::BeforeRun,
        },
    ),
    whp_test(
        "whp::halted_vp_takes_a_pending_interrupt_after_its_hlt",
        false,
        Case {
            mode: Mode::Real,
            suspend: Suspend::Halt { interrupts: true },
            target: Target::FreshPartition,
            wake: Wake::Vector,
            when: When::BeforeRun,
        },
    ),
    whp_test(
        "whp::halted_vp_in_64_bit_mode_with_paging_stays_halted_in_a_fresh_partition",
        false,
        Case {
            mode: Mode::Long,
            suspend: Suspend::Halt { interrupts: false },
            target: Target::FreshPartition,
            wake: Wake::Never,
            when: When::AfterRun,
        },
    ),
    whp_test(
        "whp::idle_vp_stays_idle_in_a_fresh_partition_hv1",
        true,
        Case {
            mode: Mode::Real,
            suspend: Suspend::Idle,
            target: Target::FreshPartition,
            wake: Wake::Nmi,
            when: When::AfterRun,
        },
    ),
    whp_test(
        "whp::idle_vp_takes_a_pending_nmi_after_its_rdmsr_hv1",
        true,
        Case {
            mode: Mode::Real,
            suspend: Suspend::Idle,
            target: Target::FreshPartition,
            wake: Wake::Nmi,
            when: When::BeforeRun,
        },
    ),
]);
