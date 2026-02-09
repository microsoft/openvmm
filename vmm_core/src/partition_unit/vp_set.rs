// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtual processor state management.

use super::HaltReason;
use super::HaltReasonReceiver;
use super::InternalHaltReason;
#[cfg(feature = "gdb")]
use anyhow::Context as _;
use async_trait::async_trait;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::JoinAll;
use futures::future::TryJoinAll;
use futures::stream::select_with_strategy;
use futures_concurrency::future::Race;
use futures_concurrency::stream::Merge;
use guestmem::GuestMemory;
use hvdef::Vtl;
use inspect::Inspect;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use parking_lot::Mutex;
use slab::Slab;
use std::future::Future;
use std::pin::Pin;
use std::pin::pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use thiserror::Error;
use tracing::instrument;
use virt::InitialRegs;
use virt::Processor;
use virt::StopVp;
use virt::StopVpSource;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::VpStopped;
use virt::io::CpuIo;
use virt::vp::AccessVpState;
use vm_topology::processor::TargetVpInfo;
use vmcore::save_restore::ProtobufSaveRestore;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
#[cfg(feature = "gdb")]
use vmm_core_defs::debug_rpc::DebuggerVpState;

const NUM_VTLS: usize = 3;

/// Trait for controlling a VP on a bound partition.
#[async_trait(?Send)]
trait ControlVp: ProtobufSaveRestore {
    /// Run the VP until `stop` says to stop.
    async fn run_vp(
        &mut self,
        vtl_guest_memory: &[Option<GuestMemory>; NUM_VTLS],
        stop: StopVp<'_>,
    ) -> Result<StopReason, HaltReason>;

    /// Inspect the VP.
    fn inspect_vp(&mut self, gm: &[Option<GuestMemory>; NUM_VTLS], req: inspect::Request<'_>);

    /// Sets the register state at first boot.
    fn set_initial_regs(
        &mut self,
        vtl: Vtl,
        state: &InitialRegs,
        to_set: RegistersToSet,
    ) -> Result<(), RegisterSetError>;

    #[cfg(feature = "gdb")]
    fn debug(&mut self) -> &mut dyn DebugVp;
}

enum StopReason {
    OnRequest(VpStopped),
    Cancel,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RegistersToSet {
    All,
    MtrrsOnly,
}

#[cfg(feature = "gdb")]
trait DebugVp {
    fn set_debug_state(
        &mut self,
        vtl: Vtl,
        state: Option<&virt::x86::DebugState>,
    ) -> anyhow::Result<()>;

    fn set_vp_state(&mut self, vtl: Vtl, state: &DebuggerVpState) -> anyhow::Result<()>;

    fn get_vp_state(&mut self, vtl: Vtl) -> anyhow::Result<Box<DebuggerVpState>>;
}

struct BoundVp<'a, T, U> {
    vp: &'a mut T,
    io: &'a U,
    vp_index: VpIndex,
}

impl<T: ProtobufSaveRestore, U> ProtobufSaveRestore for BoundVp<'_, T, U> {
    fn save(&mut self) -> Result<SavedStateBlob, SaveError> {
        self.vp.save()
    }

    fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        self.vp.restore(state)
    }
}

#[async_trait(?Send)]
impl<T, U> ControlVp for BoundVp<'_, T, U>
where
    T: Processor + ProtobufSaveRestore,
    U: CpuIo,
{
    async fn run_vp(
        &mut self,
        vtl_guest_memory: &[Option<GuestMemory>; NUM_VTLS],
        stop: StopVp<'_>,
    ) -> Result<StopReason, HaltReason> {
        let r = self.vp.run_vp(stop, self.io).await;
        // Convert the inner error type to a generic one.
        match r.unwrap_err() {
            VpHaltReason::Stop(stop) => Ok(StopReason::OnRequest(stop)),
            VpHaltReason::Cancel => Ok(StopReason::Cancel),
            VpHaltReason::PowerOff => Err(HaltReason::PowerOff),
            VpHaltReason::Reset => Err(HaltReason::Reset),
            VpHaltReason::Hibernate => Err(HaltReason::Hibernate),
            VpHaltReason::TripleFault { vtl } => {
                let registers = self.vp.access_state(vtl).registers().ok().map(Arc::new);

                tracing::error!(?vtl, vp = self.vp_index.index(), "triple fault");
                self.trace_fault(
                    vtl,
                    vtl_guest_memory[vtl as usize].as_ref(),
                    registers.as_deref(),
                );
                Err(HaltReason::TripleFault {
                    vp: self.vp_index.index(),
                    registers,
                })
            }
            VpHaltReason::SingleStep => {
                tracing::debug!("single step");
                Err(HaltReason::SingleStep {
                    vp: self.vp_index.index(),
                })
            }
            VpHaltReason::HwBreak(breakpoint) => {
                tracing::debug!(?breakpoint, "hardware breakpoint");
                Err(HaltReason::HwBreakpoint {
                    vp: self.vp_index.index(),
                    breakpoint,
                })
            }
        }
    }

    fn inspect_vp(
        &mut self,
        vtl_guest_memory: &[Option<GuestMemory>; NUM_VTLS],
        req: inspect::Request<'_>,
    ) {
        let mut resp = req.respond();
        resp.merge(&mut *self.vp);
        for (name, vtl) in [
            ("vtl0", Vtl::Vtl0),
            ("vtl1", Vtl::Vtl1),
            ("vtl2", Vtl::Vtl2),
        ] {
            if self.vp.vtl_inspectable(vtl) {
                resp.field_mut(
                    name,
                    &mut inspect::adhoc_mut(|req| {
                        self.inspect_vtl(vtl_guest_memory[vtl as usize].as_ref(), req, vtl)
                    }),
                );
            }
        }
    }

    fn set_initial_regs(
        &mut self,
        vtl: Vtl,
        state: &InitialRegs,
        to_set: RegistersToSet,
    ) -> Result<(), RegisterSetError> {
        let InitialRegs {
            registers,
            #[cfg(guest_arch = "x86_64")]
            mtrrs,
            #[cfg(guest_arch = "x86_64")]
            pat,
            #[cfg(guest_arch = "aarch64")]
            system_registers,
        } = state;
        let mut access = self.vp.access_state(vtl);
        // Only set the registers on the BSP.
        if self.vp_index.is_bsp() && to_set == RegistersToSet::All {
            access
                .set_registers(registers)
                .map_err(|err| RegisterSetError("registers", err.into()))?;

            #[cfg(guest_arch = "aarch64")]
            access
                .set_system_registers(system_registers)
                .map_err(|err| RegisterSetError("system_registers", err.into()))?;
        }

        // Set MTRRs and PAT on all VPs.
        #[cfg(guest_arch = "x86_64")]
        access
            .set_mtrrs(mtrrs)
            .map_err(|err| RegisterSetError("mtrrs", err.into()))?;
        #[cfg(guest_arch = "x86_64")]
        access
            .set_pat(pat)
            .map_err(|err| RegisterSetError("pat", err.into()))?;

        Ok(())
    }

    #[cfg(feature = "gdb")]
    fn debug(&mut self) -> &mut dyn DebugVp {
        self
    }
}

impl<T, U> BoundVp<'_, T, U>
where
    T: Processor + ProtobufSaveRestore,
    U: CpuIo,
{
    fn inspect_vtl(&mut self, gm: Option<&GuestMemory>, req: inspect::Request<'_>, vtl: Vtl) {
        let mut resp = req.respond();
        resp.field("enabled", true)
            .merge(self.vp.access_state(vtl).inspect_all());

        let _ = gm;
        #[cfg(all(guest_arch = "x86_64", feature = "gdb"))]
        if let Some(gm) = gm {
            let registers = self.vp.access_state(vtl).registers();
            if let Ok(registers) = &registers {
                resp.field_with("next_instruction", || {
                    Some(
                        vp_state::next_instruction(gm, self.debug(), vtl, registers).map_or_else(
                            |err| format!("{:#}", err),
                            |(instr, _)| instr.to_string(),
                        ),
                    )
                })
                .field_with("previous_instruction", || {
                    Some(
                        vp_state::previous_instruction(gm, self.debug(), vtl, registers)
                            .map_or_else(|err| format!("{:#}", err), |instr| instr.to_string()),
                    )
                });
            }
        }
    }

    #[cfg(guest_arch = "x86_64")]
    fn trace_fault(
        &mut self,
        vtl: Vtl,
        guest_memory: Option<&GuestMemory>,
        registers: Option<&virt::x86::vp::Registers>,
    ) {
        use cvm_tracing::CVM_CONFIDENTIAL;

        #[cfg(not(feature = "gdb"))]
        let _ = (guest_memory, vtl);

        let Some(registers) = registers else {
            return;
        };

        let virt::x86::vp::Registers {
            rax,
            rcx,
            rdx,
            rbx,
            rsp,
            rbp,
            rsi,
            rdi,
            r8,
            r9,
            r10,
            r11,
            r12,
            r13,
            r14,
            r15,
            rip,
            rflags,
            cs,
            ds,
            es,
            fs,
            gs,
            ss,
            tr,
            ldtr,
            gdtr,
            idtr,
            cr0,
            cr2,
            cr3,
            cr4,
            cr8,
            efer,
        } = *registers;
        tracing::error!(
            CVM_CONFIDENTIAL,
            vp = self.vp_index.index(),
            ?vtl,
            rax,
            rcx,
            rdx,
            rbx,
            rsp,
            rbp,
            rsi,
            rdi,
            r8,
            r9,
            r10,
            r11,
            r12,
            r13,
            r14,
            r15,
            rip,
            rflags,
            "triple fault register state",
        );
        tracing::error!(
            CVM_CONFIDENTIAL,
            ?vtl,
            vp = self.vp_index.index(),
            ?cs,
            ?ds,
            ?es,
            ?fs,
            ?gs,
            ?ss,
            ?tr,
            ?ldtr,
            ?gdtr,
            ?idtr,
            cr0,
            cr2,
            cr3,
            cr4,
            cr8,
            efer,
            "triple fault system register state",
        );

        #[cfg(feature = "gdb")]
        if let Some(guest_memory) = guest_memory {
            if let Ok((instr, bytes)) =
                vp_state::next_instruction(guest_memory, self, vtl, registers)
            {
                tracing::error!(
                    CVM_CONFIDENTIAL,
                    instruction = instr.to_string(),
                    ?bytes,
                    "faulting instruction"
                );
            }
        }
    }

    #[cfg(guest_arch = "aarch64")]
    fn trace_fault(
        &mut self,
        _vtl: Vtl,
        _guest_memory: Option<&GuestMemory>,
        _registers: Option<&virt::aarch64::vp::Registers>,
    ) {
        // TODO
    }
}

#[cfg(feature = "gdb")]
impl<T: Processor, U> DebugVp for BoundVp<'_, T, U> {
    fn set_debug_state(
        &mut self,
        vtl: Vtl,
        state: Option<&virt::x86::DebugState>,
    ) -> anyhow::Result<()> {
        self.vp
            .set_debug_state(vtl, state)
            .context("failed to set debug state")
    }

    #[cfg(guest_arch = "x86_64")]
    fn set_vp_state(&mut self, vtl: Vtl, state: &DebuggerVpState) -> anyhow::Result<()> {
        let mut access = self.vp.access_state(vtl);
        let DebuggerVpState::X86_64(state) = state else {
            anyhow::bail!("wrong architecture")
        };
        let regs = virt::x86::vp::Registers {
            rax: state.gp[0],
            rcx: state.gp[1],
            rdx: state.gp[2],
            rbx: state.gp[3],
            rsp: state.gp[4],
            rbp: state.gp[5],
            rsi: state.gp[6],
            rdi: state.gp[7],
            r8: state.gp[8],
            r9: state.gp[9],
            r10: state.gp[10],
            r11: state.gp[11],
            r12: state.gp[12],
            r13: state.gp[13],
            r14: state.gp[14],
            r15: state.gp[15],
            rip: state.rip,
            rflags: state.rflags,
            cs: state.cs,
            ds: state.ds,
            es: state.es,
            fs: state.fs,
            gs: state.gs,
            ss: state.ss,
            cr0: state.cr0,
            cr2: state.cr2,
            cr3: state.cr3,
            cr4: state.cr4,
            cr8: state.cr8,
            efer: state.efer,
            ..access.registers()?
        };
        let msrs = virt::x86::vp::VirtualMsrs {
            kernel_gs_base: state.kernel_gs_base,
            ..access.virtual_msrs()?
        };
        access.set_registers(&regs)?;
        access.set_virtual_msrs(&msrs)?;
        access.commit()?;
        Ok(())
    }

    #[cfg(guest_arch = "x86_64")]
    fn get_vp_state(&mut self, vtl: Vtl) -> anyhow::Result<Box<DebuggerVpState>> {
        let mut access = self.vp.access_state(vtl);
        let regs = access.registers()?;
        let msrs = access.virtual_msrs()?;
        Ok(Box::new(DebuggerVpState::X86_64(
            vmm_core_defs::debug_rpc::X86VpState {
                gp: [
                    regs.rax, regs.rcx, regs.rdx, regs.rbx, regs.rsp, regs.rbp, regs.rsi, regs.rdi,
                    regs.r8, regs.r9, regs.r10, regs.r11, regs.r12, regs.r13, regs.r14, regs.r15,
                ],
                rip: regs.rip,
                rflags: regs.rflags,
                cr0: regs.cr0,
                cr2: regs.cr2,
                cr3: regs.cr3,
                cr4: regs.cr4,
                cr8: regs.cr8,
                efer: regs.efer,
                kernel_gs_base: msrs.kernel_gs_base,
                es: regs.es,
                cs: regs.cs,
                ss: regs.ss,
                ds: regs.ds,
                fs: regs.fs,
                gs: regs.gs,
            },
        )))
    }

    #[cfg(guest_arch = "aarch64")]
    fn set_vp_state(&mut self, vtl: Vtl, state: &DebuggerVpState) -> anyhow::Result<()> {
        let DebuggerVpState::Aarch64(state) = state else {
            anyhow::bail!("wrong architecture")
        };
        let mut access = self.vp.access_state(vtl);
        let regs = virt::aarch64::vp::Registers {
            x0: state.x[0],
            x1: state.x[1],
            x2: state.x[2],
            x3: state.x[3],
            x4: state.x[4],
            x5: state.x[5],
            x6: state.x[6],
            x7: state.x[7],
            x8: state.x[8],
            x9: state.x[9],
            x10: state.x[10],
            x11: state.x[11],
            x12: state.x[12],
            x13: state.x[13],
            x14: state.x[14],
            x15: state.x[15],
            x16: state.x[16],
            x17: state.x[17],
            x18: state.x[18],
            x19: state.x[19],
            x20: state.x[20],
            x21: state.x[21],
            x22: state.x[22],
            x23: state.x[23],
            x24: state.x[24],
            x25: state.x[25],
            x26: state.x[26],
            x27: state.x[27],
            x28: state.x[28],
            fp: state.x[29],
            lr: state.x[30],
            sp_el0: state.sp_el0,
            sp_el1: state.sp_el1,
            pc: state.pc,
            cpsr: state.cpsr,
        };
        let sregs = virt::aarch64::vp::SystemRegisters {
            sctlr_el1: state.sctlr_el1,
            tcr_el1: state.tcr_el1,
            ttbr0_el1: state.ttbr0_el1,
            ttbr1_el1: state.ttbr1_el1,
            ..access.system_registers()?
        };
        access.set_registers(&regs)?;
        access.set_system_registers(&sregs)?;
        access.commit()?;
        Ok(())
    }

    #[cfg(guest_arch = "aarch64")]
    fn get_vp_state(&mut self, vtl: Vtl) -> anyhow::Result<Box<DebuggerVpState>> {
        let mut access = self.vp.access_state(vtl);
        let regs = access.registers()?;
        let sregs = access.system_registers()?;

        Ok(Box::new(DebuggerVpState::Aarch64(
            vmm_core_defs::debug_rpc::Aarch64VpState {
                x: [
                    regs.x0, regs.x1, regs.x2, regs.x3, regs.x4, regs.x5, regs.x6, regs.x7,
                    regs.x8, regs.x9, regs.x10, regs.x11, regs.x12, regs.x13, regs.x14, regs.x15,
                    regs.x16, regs.x17, regs.x18, regs.x19, regs.x20, regs.x21, regs.x22, regs.x23,
                    regs.x24, regs.x25, regs.x26, regs.x27, regs.x28, regs.fp, regs.lr,
                ],
                sp_el0: regs.sp_el0,
                sp_el1: regs.sp_el1,
                pc: regs.pc,
                cpsr: regs.cpsr,
                sctlr_el1: sregs.sctlr_el1,
                tcr_el1: sregs.tcr_el1,
                ttbr0_el1: sregs.ttbr0_el1,
                ttbr1_el1: sregs.ttbr1_el1,
            },
        )))
    }
}

/// Tracks whether the VP should halt due to a guest-initiated condition (triple
/// fault, etc.).
#[derive(Inspect)]
pub struct Halt {
    #[inspect(flatten)]
    state: Mutex<HaltState>,
    #[inspect(skip)]
    send: mesh::Sender<InternalHaltReason>,
}

#[derive(Default, Inspect)]
struct HaltState {
    halt_count: usize,
    #[inspect(skip)]
    wakers: Slab<Option<Waker>>,
}

impl Halt {
    /// Returns a new halt object, plus a receiver to asynchronously receive the
    /// reason for a halt.
    pub fn new() -> (Self, HaltReasonReceiver) {
        let (send, recv) = mesh::channel();
        (
            Self {
                state: Default::default(),
                send,
            },
            HaltReasonReceiver(recv),
        )
    }

    /// Halts all VPs and sends the halt reason to the receiver returned by
    /// [`Self::new()`].
    ///
    /// After this returns, it's guaranteed that any VPs that try to run again
    /// will instead halt. So if this is called from a VP thread, it will ensure
    /// that that VP will not resume.
    pub fn halt(&self, reason: HaltReason) {
        self.halt_internal(InternalHaltReason::Halt(reason));
    }

    /// Halts all VPs temporarily, resets their variable MTRRs to their initial
    /// state, then resumes the VPs.
    ///
    /// This is used by the legacy BIOS, since it stomps over the variable MTRRs
    /// in undesirable ways and is difficult to fix.
    pub fn replay_mtrrs(&self) {
        self.halt_internal(InternalHaltReason::ReplayMtrrs);
    }

    fn halt_internal(&self, reason: InternalHaltReason) {
        // Set the VP halt state immediately and wake them up.
        let mut inner = self.state.lock();
        inner.halt_count += 1;
        for waker in inner.wakers.iter_mut().filter_map(|x| x.1.take()) {
            waker.wake();
        }

        // Send the halt reason asynchronously.
        self.send.send(reason);
    }

    /// Clears a single halt reason. Must be called for each halt reason that
    /// arrives in order to resume the VM.
    fn clear_halt(&self) {
        let mut inner = self.state.lock();
        inner.halt_count = inner
            .halt_count
            .checked_sub(1)
            .expect("too many halt clears");
    }

    fn is_halted(&self) -> bool {
        self.state.lock().halt_count != 0
    }

    fn halted(&self) -> Halted<'_> {
        Halted {
            halt: self,
            idx: None,
        }
    }
}

struct Halted<'a> {
    halt: &'a Halt,
    idx: Option<usize>,
}

impl Clone for Halted<'_> {
    fn clone(&self) -> Self {
        Self {
            halt: self.halt,
            idx: None,
        }
    }
}

impl Future for Halted<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut halt = self.halt.state.lock();
        if halt.halt_count != 0 {
            return Poll::Ready(());
        }

        if let Some(idx) = self.idx {
            halt.wakers[idx] = Some(cx.waker().clone());
        } else {
            self.idx = Some(halt.wakers.insert(Some(cx.waker().clone())));
        }
        Poll::Pending
    }
}

impl Drop for Halted<'_> {
    fn drop(&mut self) {
        if let Some(idx) = self.idx {
            self.halt.state.lock().wakers.remove(idx);
        }
    }
}

#[derive(Inspect)]
struct Inner {
    #[inspect(flatten)]
    halt: Arc<Halt>,
    #[inspect(skip)]
    vtl_guest_memory: [Option<GuestMemory>; NUM_VTLS],
}

#[derive(Inspect)]
pub struct VpSet {
    #[inspect(flatten)]
    inner: Arc<Inner>,
    #[inspect(rename = "vp", iter_by_index)]
    vps: Vec<Vp>,
    #[inspect(skip)]
    started: bool,
}

#[derive(Inspect)]
struct Vp {
    #[inspect(flatten, send = "|req| VpEvent::State(StateEvent::Inspect(req))")]
    send: mesh::Sender<VpEvent>,
    #[inspect(skip)]
    done: mesh::OneshotReceiver<()>,
    #[inspect(flatten)]
    vp_info: TargetVpInfo,
}

impl VpSet {
    pub fn new(vtl_guest_memory: [Option<GuestMemory>; NUM_VTLS], halt: Arc<Halt>) -> Self {
        let inner = Inner {
            vtl_guest_memory,
            halt,
        };
        Self {
            inner: Arc::new(inner),
            vps: Vec::new(),
            started: false,
        }
    }

    /// Adds a VP and returns its runner.
    pub fn add(&mut self, vp: TargetVpInfo) -> VpRunner {
        assert!(!self.started);
        let (send, recv) = mesh::channel();
        let (done_send, done_recv) = mesh::oneshot();
        self.vps.push(Vp {
            send,
            done: done_recv,
            vp_info: vp,
        });
        let (cancel_send, cancel_recv) = mesh::channel();
        VpRunner {
            recv,
            _done: done_send,
            cancel_recv,
            cancel_send,
            inner: RunnerInner {
                vp: vp.as_ref().vp_index,
                inner: self.inner.clone(),
                state: VpState::Stopped,
            },
        }
    }

    /// Starts all VPs.
    pub fn start(&mut self) {
        if !self.started {
            for vp in &self.vps {
                vp.send.send(VpEvent::Start);
            }
            self.started = true;
        }
    }

    /// Initiates a halt to the VPs.
    #[cfg_attr(not(feature = "gdb"), expect(dead_code))]
    pub fn halt(&mut self, reason: HaltReason) {
        self.inner.halt.halt(reason);
    }

    /// Resets the halt state for all VPs.
    ///
    /// The VPs must be stopped.
    pub fn clear_halt(&mut self) {
        assert!(!self.started);
        self.inner.halt.clear_halt();
    }

    /// Stops all VPs.
    pub async fn stop(&mut self) {
        if self.started {
            self.vps
                .iter()
                .map(|vp| {
                    let (send, recv) = mesh::oneshot();
                    vp.send.send(VpEvent::Stop(send));
                    // Ignore VPs whose runners have been dropped.
                    async { recv.await.ok() }
                })
                .collect::<JoinAll<_>>()
                .await;
            self.started = false;
        }
    }

    pub async fn save(&mut self) -> Result<Vec<(VpIndex, SavedStateBlob)>, SaveError> {
        assert!(!self.started);
        self.vps
            .iter()
            .enumerate()
            .map(async |(index, vp)| {
                let data = vp
                    .send
                    .call(|x| VpEvent::State(StateEvent::Save(x)), ())
                    .await
                    .map_err(|err| SaveError::Other(RunnerGoneError(err).into()))
                    .and_then(|x| x)
                    .map_err(|err| SaveError::ChildError(format!("vp{index}"), Box::new(err)))?;
                Ok((VpIndex::new(index as u32), data))
            })
            .collect::<TryJoinAll<_>>()
            .await
    }

    pub async fn restore(
        &mut self,
        states: impl IntoIterator<Item = (VpIndex, SavedStateBlob)>,
    ) -> Result<(), RestoreError> {
        assert!(!self.started);
        states
            .into_iter()
            .map(|(vp_index, data)| {
                let vp = self.vps.get(vp_index.index() as usize);
                async move {
                    let vp = vp.ok_or_else(|| {
                        RestoreError::UnknownEntryId(format!("vp{}", vp_index.index()))
                    })?;
                    vp.send
                        .call(|x| VpEvent::State(StateEvent::Restore(x)), data)
                        .await
                        .map_err(|err| RestoreError::Other(RunnerGoneError(err).into()))
                        .and_then(|x| x)
                        .map_err(|err| {
                            RestoreError::ChildError(
                                format!("vp{}", vp_index.index()),
                                Box::new(err),
                            )
                        })
                }
            })
            .collect::<TryJoinAll<_>>()
            .await?;

        Ok(())
    }

    /// Tears down the VPs.
    pub async fn teardown(self) {
        self.vps
            .into_iter()
            .map(|vp| vp.done.map(drop))
            .collect::<JoinAll<_>>()
            .await;
    }

    pub async fn set_initial_regs(
        &mut self,
        vtl: Vtl,
        initial_regs: Arc<InitialRegs>,
        to_set: RegistersToSet,
    ) -> Result<(), RegisterSetError> {
        self.vps
            .iter()
            .map(|vp| {
                let initial_regs = initial_regs.clone();
                async move {
                    vp.send
                        .call(
                            |x| VpEvent::State(StateEvent::SetInitialRegs(x)),
                            (vtl, initial_regs, to_set),
                        )
                        .await
                        .map_err(|err| {
                            RegisterSetError("initial_regs", RunnerGoneError(err).into())
                        })?
                }
            })
            .collect::<TryJoinAll<_>>()
            .await?;

        Ok(())
    }
}

/// Error returned when registers could not be set on a VP.
#[derive(Debug, Error)]
#[error("failed to set VP register set {0}")]
pub struct RegisterSetError(&'static str, #[source] anyhow::Error);

#[derive(Debug, Error)]
#[error("the vp runner was dropped")]
struct RunnerGoneError(#[source] RpcError);

#[cfg(feature = "gdb")]
impl VpSet {
    /// Set the debug state for a single VP.
    pub async fn set_debug_state(
        &self,
        vp: VpIndex,
        state: virt::x86::DebugState,
    ) -> anyhow::Result<()> {
        self.vps[vp.index() as usize]
            .send
            .call(
                |x| VpEvent::State(StateEvent::Debug(DebugEvent::SetDebugState(x))),
                Some(state),
            )
            .await
            .map_err(RunnerGoneError)?
    }

    /// Clear the debug state for all VPs.
    pub async fn clear_debug_state(&self) -> anyhow::Result<()> {
        for vp in &self.vps {
            vp.send
                .call(
                    |x| VpEvent::State(StateEvent::Debug(DebugEvent::SetDebugState(x))),
                    None,
                )
                .await
                .map_err(RunnerGoneError)??;
        }
        Ok(())
    }

    pub async fn set_vp_state(
        &self,
        vp: VpIndex,
        state: Box<DebuggerVpState>,
    ) -> anyhow::Result<()> {
        self.vps[vp.index() as usize]
            .send
            .call(
                |x| VpEvent::State(StateEvent::Debug(DebugEvent::SetVpState(x))),
                state,
            )
            .await
            .map_err(RunnerGoneError)?
    }

    pub async fn get_vp_state(&self, vp: VpIndex) -> anyhow::Result<Box<DebuggerVpState>> {
        self.vps[vp.index() as usize]
            .send
            .call(
                |x| VpEvent::State(StateEvent::Debug(DebugEvent::GetVpState(x))),
                (),
            )
            .await
            .map_err(RunnerGoneError)?
    }

    pub async fn read_virtual_memory(
        &self,
        vp: VpIndex,
        gva: u64,
        len: usize,
    ) -> anyhow::Result<Vec<u8>> {
        self.vps[vp.index() as usize]
            .send
            .call(
                |x| VpEvent::State(StateEvent::Debug(DebugEvent::ReadVirtualMemory(x))),
                (gva, len),
            )
            .await
            .map_err(RunnerGoneError)?
    }

    pub async fn write_virtual_memory(
        &self,
        vp: VpIndex,
        gva: u64,
        data: Vec<u8>,
    ) -> anyhow::Result<()> {
        self.vps[vp.index() as usize]
            .send
            .call(
                |x| VpEvent::State(StateEvent::Debug(DebugEvent::WriteVirtualMemory(x))),
                (gva, data),
            )
            .await
            .map_err(RunnerGoneError)?
    }
}

#[derive(Debug)]
enum VpEvent {
    Start,
    Stop(mesh::OneshotSender<()>),
    State(StateEvent),
}

#[derive(Debug)]
enum StateEvent {
    Inspect(inspect::Deferred),
    SetInitialRegs(Rpc<(Vtl, Arc<InitialRegs>, RegistersToSet), Result<(), RegisterSetError>>),
    Save(Rpc<(), Result<SavedStateBlob, SaveError>>),
    Restore(Rpc<SavedStateBlob, Result<(), RestoreError>>),
    #[cfg(feature = "gdb")]
    Debug(DebugEvent),
}

#[cfg(feature = "gdb")]
#[derive(Debug)]
enum DebugEvent {
    SetDebugState(Rpc<Option<virt::x86::DebugState>, anyhow::Result<()>>),
    SetVpState(Rpc<Box<DebuggerVpState>, anyhow::Result<()>>),
    GetVpState(Rpc<(), anyhow::Result<Box<DebuggerVpState>>>),
    ReadVirtualMemory(Rpc<(u64, usize), anyhow::Result<Vec<u8>>>),
    WriteVirtualMemory(Rpc<(u64, Vec<u8>), anyhow::Result<()>>),
}

/// An object used to dispatch a virtual processor.
#[must_use]
pub struct VpRunner {
    recv: mesh::Receiver<VpEvent>,
    cancel_send: mesh::Sender<()>,
    cancel_recv: mesh::Receiver<()>,
    _done: mesh::OneshotSender<()>,
    inner: RunnerInner,
}

/// An object that can cancel a pending call into [`VpRunner::run`].
pub struct RunnerCanceller(mesh::Sender<()>);

impl RunnerCanceller {
    /// Requests that the current or next call to [`VpRunner::run`] return as
    /// soon as possible.
    pub fn cancel(&mut self) {
        self.0.send(());
    }
}

/// Error returned when a VP run is cancelled.
#[derive(Debug)]
pub struct RunCancelled(bool);

impl RunCancelled {
    /// Returns `true` if the run was cancelled by the user, or `false` if it was
    /// cancelled by the VP itself.
    pub fn is_user_cancelled(&self) -> bool {
        self.0
    }
}

struct RunnerInner {
    vp: VpIndex,
    inner: Arc<Inner>,
    state: VpState,
}

#[derive(Copy, Clone, Debug, Inspect, PartialEq, Eq)]
enum VpState {
    Stopped,
    Running,
    Halted,
}

impl VpRunner {
    /// Runs the VP dispatch loop for `vp`, using `io` to handle CPU requests.
    ///
    /// Returns [`RunCancelled`] if [`RunnerCanceller::cancel`] was called, or
    /// if the VP returns [`VpHaltReason::Cancel`]. In this case, the call can
    /// be reissued, with the same or different `vp` object, to continue running
    /// the VP.
    ///
    /// Do not reissue this call if it returns `Ok`. Do not drop this future
    /// without awaiting it to completion.
    pub async fn run(
        &mut self,
        vp: &mut (impl Processor + ProtobufSaveRestore),
        io: &impl CpuIo,
    ) -> Result<(), RunCancelled> {
        let vp_index = self.inner.vp;
        self.run_inner(&mut BoundVp { vp, io, vp_index }).await
    }

    /// Returns an object that can be used to cancel a `run` call.
    pub fn canceller(&self) -> RunnerCanceller {
        RunnerCanceller(self.cancel_send.clone())
    }

    #[instrument(level = "debug", name = "run_vp", skip_all, fields(vp_index = self.inner.vp.index()))]
    async fn run_inner(&mut self, vp: &mut dyn ControlVp) -> Result<(), RunCancelled> {
        loop {
            // Wait for start.
            while self.inner.state != VpState::Running {
                let r = (self.recv.next().map(Ok), self.cancel_recv.next().map(Err))
                    .race()
                    .await
                    .map_err(|_| RunCancelled(true))?;
                match r {
                    Some(VpEvent::Start) => {
                        assert_eq!(self.inner.state, VpState::Stopped);
                        self.inner.state = VpState::Running;
                    }
                    Some(VpEvent::Stop(send)) => {
                        assert_eq!(self.inner.state, VpState::Halted);
                        self.inner.state = VpState::Stopped;
                        send.send(());
                    }
                    Some(VpEvent::State(event)) => self.inner.state_event(vp, event),
                    None => return Ok(()),
                }
            }

            // If the VPs are already halted, wait for the next request without
            // running the VP even once.
            if self.inner.inner.halt.is_halted() {
                self.inner.state = VpState::Halted;
                continue;
            }

            let mut stop_complete = None;
            let mut state_requests = Vec::new();
            let mut cancelled_by_user = None;
            {
                enum Event {
                    Vp(VpEvent),
                    Teardown,
                    Halt,
                    VpStopped(Result<StopReason, HaltReason>),
                    Cancel,
                }

                let stop = StopVpSource::new();

                let run_vp = vp
                    .run_vp(&self.inner.inner.vtl_guest_memory, stop.checker())
                    .into_stream()
                    .map(Event::VpStopped);

                let halt = self
                    .inner
                    .inner
                    .halt
                    .halted()
                    .into_stream()
                    .map(|_| Event::Halt);

                let recv = (&mut self.recv)
                    .map(Event::Vp)
                    .chain(async { Event::Teardown }.into_stream());

                let cancel = (&mut self.cancel_recv).map(|()| Event::Cancel);

                let s = (recv, halt, cancel).merge();

                // Since `run_vp` will block the thread until it receives a
                // cancellation notification, always poll the other sources to
                // exhaustion before polling the future.
                let mut s = pin!(select_with_strategy(s, run_vp, |_: &mut ()| {
                    futures::stream::PollNext::Left
                }));

                // Wait for stop or a VP failure.
                while let Some(event) = s.next().await {
                    match event {
                        Event::Vp(VpEvent::Start) => panic!("vp already started"),
                        Event::Vp(VpEvent::Stop(send)) => {
                            tracing::debug!("stopping VP");
                            stop.stop();
                            stop_complete = Some(send);
                        }
                        Event::Vp(VpEvent::State(event)) => {
                            // Stop the VP so that we can drop the run_vp future
                            // before manipulating state.
                            //
                            // FUTURE: This causes inspection delays during slow
                            // MMIO/PIO exit handling. Fix the backends to support
                            // calling inspect while run_vp is still alive (but
                            // suspended).
                            stop.stop();
                            state_requests.push(event);
                        }
                        Event::Halt => {
                            tracing::debug!("stopping VP due to halt");
                            stop.stop();
                        }
                        Event::Cancel => {
                            tracing::debug!("run cancelled externally");
                            stop.stop();
                            cancelled_by_user = Some(true);
                        }
                        Event::Teardown => {
                            tracing::debug!("tearing down");
                            stop.stop();
                        }
                        Event::VpStopped(r) => {
                            match r {
                                Ok(StopReason::OnRequest(VpStopped { .. })) => {
                                    assert!(stop.is_stopping(), "vp stopped without a reason");
                                    tracing::debug!("VP stopped on request");
                                }
                                Ok(StopReason::Cancel) => {
                                    tracing::debug!("run cancelled internally");
                                    cancelled_by_user = Some(false);
                                }
                                Err(halt_reason) => {
                                    tracing::debug!("VP halted");
                                    self.inner.inner.halt.halt(halt_reason);
                                }
                            }
                            break;
                        }
                    }
                }
            }
            for event in state_requests {
                self.inner.state_event(vp, event);
            }

            if let Some(send) = stop_complete {
                self.inner.state = VpState::Stopped;
                send.send(());
            }

            if let Some(by_user) = cancelled_by_user {
                return Err(RunCancelled(by_user));
            }
        }
    }
}

impl RunnerInner {
    fn state_event(&mut self, vp: &mut dyn ControlVp, event: StateEvent) {
        match event {
            StateEvent::Inspect(deferred) => {
                deferred.respond(|resp| {
                    resp.field("state", self.state)
                        .merge(inspect::adhoc_mut(|req| {
                            vp.inspect_vp(&self.inner.vtl_guest_memory, req)
                        }));
                });
            }
            StateEvent::SetInitialRegs(rpc) => {
                rpc.handle_sync(|(vtl, state, to_set)| vp.set_initial_regs(vtl, &state, to_set))
            }
            StateEvent::Save(rpc) => rpc.handle_sync(|()| vp.save()),
            StateEvent::Restore(rpc) => rpc.handle_sync(|data| vp.restore(data)),
            #[cfg(feature = "gdb")]
            StateEvent::Debug(event) => match event {
                DebugEvent::SetDebugState(rpc) => {
                    rpc.handle_sync(|state| vp.debug().set_debug_state(Vtl::Vtl0, state.as_ref()))
                }
                DebugEvent::SetVpState(rpc) => {
                    rpc.handle_sync(|state| vp.debug().set_vp_state(Vtl::Vtl0, &state))
                }
                DebugEvent::GetVpState(rpc) => {
                    rpc.handle_sync(|()| vp.debug().get_vp_state(Vtl::Vtl0))
                }
                DebugEvent::ReadVirtualMemory(rpc) => rpc.handle_sync(|(gva, len)| {
                    let mut buf = vec![0; len];
                    vp_state::read_virtual_memory(
                        self.inner.vtl_guest_memory[0]
                            .as_ref()
                            .context("no guest memory for vtl0")?,
                        vp.debug(),
                        Vtl::Vtl0,
                        gva,
                        &mut buf,
                    )?;
                    Ok(buf)
                }),
                DebugEvent::WriteVirtualMemory(rpc) => rpc.handle_sync(|(gva, buf)| {
                    vp_state::write_virtual_memory(
                        self.inner.vtl_guest_memory[0]
                            .as_ref()
                            .context("no guest memory for vtl0")?,
                        vp.debug(),
                        Vtl::Vtl0,
                        gva,
                        &buf,
                    )?;
                    Ok(())
                }),
            },
        }
    }
}

#[cfg(feature = "gdb")]
mod vp_state {
    use super::DebugVp;
    use anyhow::Context;
    use guestmem::GuestMemory;
    use hvdef::Vtl;
    use vmm_core_defs::debug_rpc::DebuggerVpState;

    fn translate_gva(
        guest_memory: &GuestMemory,
        debug: &mut dyn DebugVp,
        vtl: Vtl,
        gva: u64,
    ) -> anyhow::Result<u64> {
        let state = debug.get_vp_state(vtl).context("failed to get vp state")?;

        match &*state {
            DebuggerVpState::X86_64(state) => {
                let registers = virt_support_x86emu::translate::TranslationRegisters {
                    cr0: state.cr0,
                    cr4: state.cr4,
                    efer: state.efer,
                    cr3: state.cr3,
                    rflags: state.rflags,
                    ss: state.ss.into(),
                    // For debug translation, don't worry about accidentally reading
                    // page tables from shared memory.
                    encryption_mode: virt_support_x86emu::translate::EncryptionMode::None,
                };
                let flags = virt_support_x86emu::translate::TranslateFlags {
                    validate_execute: false,
                    validate_read: false,
                    validate_write: false,
                    override_smap: false,
                    enforce_smap: false,
                    privilege_check: virt_support_x86emu::translate::TranslatePrivilegeCheck::None,
                    set_page_table_bits: false,
                };
                Ok(virt_support_x86emu::translate::translate_gva_to_gpa(
                    guest_memory,
                    gva,
                    &registers,
                    flags,
                )?
                .gpa)
            }
            DebuggerVpState::Aarch64(state) => {
                let registers = virt_support_aarch64emu::translate::TranslationRegisters {
                    cpsr: state.cpsr.into(),
                    sctlr: state.sctlr_el1.into(),
                    tcr: state.tcr_el1.into(),
                    ttbr0: state.ttbr0_el1,
                    ttbr1: state.ttbr1_el1,
                    syndrome: 0,
                    // For debug translation, don't worry about accidentally reading
                    // page tables from shared memory.
                    encryption_mode: virt_support_aarch64emu::translate::EncryptionMode::None,
                };
                let flags = virt_support_aarch64emu::translate::TranslateFlags {
                    validate_execute: false,
                    validate_read: false,
                    validate_write: false,
                    privilege_check:
                        virt_support_aarch64emu::translate::TranslatePrivilegeCheck::None,
                    set_page_table_bits: false,
                };
                Ok(virt_support_aarch64emu::translate::translate_gva_to_gpa(
                    guest_memory,
                    gva,
                    &registers,
                    flags,
                )?)
            }
        }
    }

    pub(super) fn read_virtual_memory(
        guest_memory: &GuestMemory,
        debug: &mut dyn DebugVp,
        vtl: Vtl,
        gva: u64,
        buf: &mut [u8],
    ) -> anyhow::Result<()> {
        let mut offset = 0;
        while offset < buf.len() {
            let gpa = translate_gva(guest_memory, debug, vtl, gva + offset as u64)
                .context("failed to translate gva")?;
            let this_len = (buf.len() - offset).min(4096 - (gpa & 4095) as usize);
            guest_memory.read_at(gpa, &mut buf[offset..offset + this_len])?;
            offset += this_len;
        }
        Ok(())
    }

    pub(super) fn write_virtual_memory(
        guest_memory: &GuestMemory,
        debug: &mut dyn DebugVp,
        vtl: Vtl,
        gva: u64,
        buf: &[u8],
    ) -> anyhow::Result<()> {
        let mut offset = 0;
        while offset < buf.len() {
            let gpa = translate_gva(guest_memory, debug, vtl, gva + offset as u64)
                .context("failed to translate gva")?;
            let this_len = (buf.len() - offset).min(4096 - (gpa & 4095) as usize);
            guest_memory.write_at(gpa, &buf[offset..offset + this_len])?;
            offset += this_len;
        }
        Ok(())
    }

    #[cfg(guest_arch = "x86_64")]
    fn bits(regs: &virt::x86::vp::Registers) -> u32 {
        if regs.cr0 & x86defs::X64_CR0_PE != 0 {
            if regs.efer & x86defs::X64_EFER_LMA != 0 {
                64
            } else {
                32
            }
        } else {
            16
        }
    }

    #[cfg(guest_arch = "x86_64")]
    fn linear_ip(regs: &virt::x86::vp::Registers, rip: u64) -> u64 {
        if bits(regs) == 64 {
            rip
        } else {
            // 32 or 16 bits
            regs.cs.base.wrapping_add(rip)
        }
    }

    /// Get the previous instruction for debugging purposes.
    #[cfg(guest_arch = "x86_64")]
    pub(super) fn previous_instruction(
        guest_memory: &GuestMemory,
        debug: &mut dyn DebugVp,
        vtl: Vtl,
        regs: &virt::x86::vp::Registers,
    ) -> anyhow::Result<iced_x86::Instruction> {
        let mut bytes = [0u8; 16];
        // Read 16 bytes before RIP.
        let rip = regs.rip.wrapping_sub(16);
        read_virtual_memory(guest_memory, debug, vtl, linear_ip(regs, rip), &mut bytes)
            .context("failed to read memory")?;
        let mut decoder = iced_x86::Decoder::new(bits(regs), &bytes, 0);

        // Try decoding at each byte until we find the instruction right before the current one.
        for offset in 0..16 {
            decoder.set_ip(rip.wrapping_add(offset));
            decoder.try_set_position(offset as usize).unwrap();
            let instr = decoder.decode();
            if !instr.is_invalid() && instr.next_ip() == regs.rip {
                return Ok(instr);
            }
        }
        Err(anyhow::anyhow!("could not find previous instruction"))
    }

    /// Get the next instruction for debugging purposes.
    #[cfg(guest_arch = "x86_64")]
    pub(super) fn next_instruction(
        guest_memory: &GuestMemory,
        debug: &mut dyn DebugVp,
        vtl: Vtl,
        regs: &virt::x86::vp::Registers,
    ) -> anyhow::Result<(iced_x86::Instruction, [u8; 16])> {
        let mut bytes = [0u8; 16];
        read_virtual_memory(
            guest_memory,
            debug,
            vtl,
            linear_ip(regs, regs.rip),
            &mut bytes,
        )
        .context("failed to read memory")?;
        let mut decoder = iced_x86::Decoder::new(bits(regs), &bytes, 0);
        decoder.set_ip(regs.rip);
        Ok((decoder.decode(), bytes))
    }
}

struct VpWaker {
    partition: Arc<dyn RequestYield>,
    vp: VpIndex,
    inner: Waker,
}

impl VpWaker {
    fn new(partition: Arc<dyn RequestYield>, vp: VpIndex, waker: Waker) -> Self {
        Self {
            partition,
            vp,
            inner: waker,
        }
    }
}

impl std::task::Wake for VpWaker {
    fn wake_by_ref(self: &Arc<Self>) {
        self.partition.request_yield(self.vp);
        self.inner.wake_by_ref();
    }

    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }
}

/// Trait for requesting that a VP yield in its [`virt::Processor::run_vp`]
/// call.
pub trait RequestYield: Send + Sync {
    /// Forces the run_vp call to yield to the scheduler (i.e. return
    /// Poll::Pending).
    fn request_yield(&self, vp_index: VpIndex);
}

impl<T: virt::Partition> RequestYield for T {
    fn request_yield(&self, vp_index: VpIndex) {
        self.request_yield(vp_index)
    }
}

/// Blocks on a future, where the future may run a VP (and so the associated
/// waker needs to ask the VP to yield).
pub fn block_on_vp<F: Future>(partition: Arc<dyn RequestYield>, vp: VpIndex, fut: F) -> F::Output {
    let mut fut = pin!(fut);
    pal_async::local::block_on(std::future::poll_fn(|cx| {
        let waker = Arc::new(VpWaker::new(partition.clone(), vp, cx.waker().clone())).into();
        let mut cx = Context::from_waker(&waker);
        fut.poll_unpin(&mut cx)
    }))
}
