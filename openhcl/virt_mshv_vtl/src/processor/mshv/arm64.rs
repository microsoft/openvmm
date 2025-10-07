// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Aarch64 Processor support for Microsoft hypervisor-backed partitions.

#![cfg(guest_arch = "aarch64")]

type VpRegisterName = HvArm64RegisterName;

use super::super::BackingParams;
use super::super::BackingPrivate;
use super::super::signal_mnf;
use super::super::vp_state;
use super::super::vp_state::UhVpStateAccess;
use super::MshvRunVpError;
use super::VbsIsolatedVtl1State;
use crate::BackingShared;
use crate::Error;
use crate::GuestVsmState;
use crate::UhPartitionNewParams;
use crate::processor::BackingSharedParams;
use crate::processor::UhEmulationState;
use crate::processor::UhHypercallHandler;
use crate::processor::UhProcessor;
use aarch64defs::Cpsr64;
use aarch64defs::SystemOff2Code;
use aarch64defs::SystemReset2Code;
use aarch64emu::AccessCpuState;
use aarch64emu::InterceptState;
use hcl::GuestVtl;
use hcl::ioctl;
use hcl::ioctl::aarch64::MshvArm64;
use hv1_emulator::hv::ProcessorVtlHv;
use hvdef::HvAarch64PendingEvent;
use hvdef::HvArm64RegisterName;
use hvdef::HvArm64ResetType;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvMapGpaFlags;
use hvdef::HvMessageType;
use hvdef::HvRegisterValue;
use hvdef::Vtl;
use hvdef::hypercall;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use parking_lot::RwLock;
use virt::EmulatorMonitorSupport;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::aarch64::vp;
use virt::aarch64::vp::AccessVpState;
use virt::io::CpuIo;
use virt::state::HvRegisterState;
use virt::state::StateElement;
use virt_support_aarch64emu::emulate;
use virt_support_aarch64emu::emulate::EmuCheckVtlAccessError;
use virt_support_aarch64emu::emulate::EmuTranslateError;
use virt_support_aarch64emu::emulate::EmuTranslateResult;
use virt_support_aarch64emu::emulate::EmulatorSupport;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// A backing for hypervisor-backed partitions (non-isolated and
/// software-isolated).
#[derive(InspectMut)]
pub struct HypervisorBackedArm64 {
    #[inspect(hex, with = "|&x| u64::from(x)")]
    deliverability_notifications: HvDeliverabilityNotificationsRegister,
    #[inspect(hex, with = "|&x| u64::from(x)")]
    next_deliverability_notifications: HvDeliverabilityNotificationsRegister,
    stats: ProcessorStatsArm64,
}

/// Partition-wide shared data for hypervisor backed VMs.
#[derive(Inspect)]
pub struct HypervisorBackedArm64Shared {
    pub(crate) guest_vsm: RwLock<GuestVsmState<VbsIsolatedVtl1State>>,
}

impl HypervisorBackedArm64Shared {
    /// Creates a new partition-shared data structure for hypervisor backed VMs.
    pub(crate) fn new(
        _partition_params: &UhPartitionNewParams<'_>,
        params: BackingSharedParams<'_>,
    ) -> Result<Self, Error> {
        Ok(Self {
            guest_vsm: RwLock::new(GuestVsmState::from_availability(params.guest_vsm_available)),
        })
    }
}

#[derive(Inspect, Default)]
struct ProcessorStatsArm64 {
    mmio: Counter,
    unaccepted_gpa: Counter,
    hypercall: Counter,
    synic_deliverable: Counter,
}

#[derive(Debug, Error)]
#[error("vendor-specific system reset with code {0:#x}")]
struct VendorSpecificSystemReset(u32);

#[expect(private_interfaces)]
impl BackingPrivate for HypervisorBackedArm64 {
    type HclBacking<'mshv> = MshvArm64<'mshv>;
    type EmulationCache = UhCpuStateCache;
    type Shared = HypervisorBackedArm64Shared;

    fn shared(shared: &BackingShared) -> &Self::Shared {
        let BackingShared::Hypervisor(shared) = shared;
        shared
    }

    fn new(
        params: BackingParams<'_, '_, Self>,
        _shared: &HypervisorBackedArm64Shared,
    ) -> Result<Self, Error> {
        vp::Registers::at_reset(&params.partition.caps, params.vp_info);
        // TODO: reset the registers in the CPU context.
        let _ = params.runner;
        Ok(Self {
            deliverability_notifications: Default::default(),
            next_deliverability_notifications: Default::default(),
            stats: Default::default(),
        })
    }

    fn init(this: &mut UhProcessor<'_, Self>) {
        // The hypervisor initializes startup suspend to false. Set it to the
        // architectural default.
        if !this.vp_index().is_bsp() {
            this.set_vtl0_startup_suspend(true).unwrap();
        }
    }

    type StateAccess<'p, 'a>
        = UhVpStateAccess<'a, 'p, Self>
    where
        Self: 'a + 'p,
        'p: 'a;

    fn access_vp_state<'a, 'p>(
        this: &'a mut UhProcessor<'p, Self>,
        vtl: GuestVtl,
    ) -> Self::StateAccess<'p, 'a> {
        UhVpStateAccess::new(this, vtl)
    }

    async fn run_vp(
        this: &mut UhProcessor<'_, Self>,
        dev: &impl CpuIo,
        _stop: &mut virt::StopVp<'_>,
    ) -> Result<(), VpHaltReason> {
        if this.backing.deliverability_notifications
            != this.backing.next_deliverability_notifications
        {
            let notifications = this.backing.next_deliverability_notifications;
            tracing::trace!(?notifications, "setting notifications");
            this.runner
                .set_vp_register(
                    // TODO GUEST VSM
                    GuestVtl::Vtl0,
                    VpRegisterName::DeliverabilityNotifications,
                    u64::from(notifications).into(),
                )
                .expect("requesting deliverability is not a fallable operation");
            this.backing.deliverability_notifications =
                this.backing.next_deliverability_notifications;
        }

        this.unlock_tlb_lock(Vtl::Vtl2);
        let intercepted = this
            .runner
            .run()
            .map_err(|e| dev.fatal_error(MshvRunVpError(e).into()))?;

        if intercepted {
            let stat = match this.runner.exit_message().header.typ {
                HvMessageType::HvMessageTypeUnmappedGpa
                | HvMessageType::HvMessageTypeGpaIntercept => {
                    this.handle_mmio_exit(dev).await?;
                    &mut this.backing.stats.mmio
                }
                HvMessageType::HvMessageTypeUnacceptedGpa => {
                    this.handle_unaccepted_gpa_intercept(dev).await?;
                    &mut this.backing.stats.unaccepted_gpa
                }
                HvMessageType::HvMessageTypeHypercallIntercept => {
                    this.handle_hypercall_exit(dev)?;
                    &mut this.backing.stats.hypercall
                }
                HvMessageType::HvMessageTypeSynicSintDeliverable => {
                    this.handle_synic_deliverable_exit();
                    &mut this.backing.stats.synic_deliverable
                }
                HvMessageType::HvMessageTypeArm64ResetIntercept => {
                    let message = this
                        .runner
                        .exit_message()
                        .as_message::<hvdef::HvArm64ResetInterceptMessage>();
                    match message.reset_type {
                        HvArm64ResetType::POWER_OFF => {
                            return Err(VpHaltReason::PowerOff);
                        }
                        HvArm64ResetType::REBOOT => {
                            return Err(VpHaltReason::Reset);
                        }
                        HvArm64ResetType::HIBERNATE => match SystemOff2Code(message.reset_code) {
                            SystemOff2Code::DEFAULT | SystemOff2Code::HIBERNATE_OFF => {
                                return Err(VpHaltReason::Hibernate);
                            }
                            _ => {
                                tracing::error!(
                                    reset_code = message.reset_code,
                                    "received unknown architectural hibernate reset code, treating as hibernate"
                                );
                                return Err(VpHaltReason::Hibernate);
                            }
                        },
                        HvArm64ResetType::SYSTEM_RESET => {
                            // High bit set indicates vendor-specific reset code.
                            if message.reset_code & 0x80000000 != 0 {
                                tracing::error!(
                                    reset_code = message.reset_code,
                                    "received vendor-specific system_reset2 reset code, treating as fatal error"
                                );
                                return Err(dev.fatal_error(
                                    VendorSpecificSystemReset(message.reset_code).into(),
                                ));
                            }
                            match SystemReset2Code(message.reset_code) {
                                SystemReset2Code::WARM_RESET => return Err(VpHaltReason::Reset),
                                _ => {
                                    tracing::error!(
                                        reset_code = message.reset_code,
                                        "received unknown architectural system_reset2 reset code, treating as reset"
                                    );
                                    return Err(VpHaltReason::Reset);
                                }
                            }
                        }
                        ty => {
                            unreachable!(
                                "unknown reset type: {:#x?}, {:#x}",
                                ty, message.reset_code
                            )
                        }
                    }
                }
                reason => unreachable!("unknown exit reason: {:#x?}", reason),
            };
            stat.increment();
        }
        Ok(())
    }

    fn poll_apic(_this: &mut UhProcessor<'_, Self>, _vtl: GuestVtl, _scan_irr: bool) {}

    fn process_interrupts(
        _this: &mut UhProcessor<'_, Self>,
        _scan_irr: hv1_structs::VtlArray<bool, 2>,
        _first_scan_irr: &mut bool,
        _dev: &impl CpuIo,
    ) -> bool {
        false
    }

    fn request_extint_readiness(this: &mut UhProcessor<'_, Self>) {
        this.backing
            .next_deliverability_notifications
            .set_interrupt_notification(true);
    }

    fn request_untrusted_sint_readiness(this: &mut UhProcessor<'_, Self>, sints: u16) {
        this.backing
            .next_deliverability_notifications
            .set_sints(this.backing.next_deliverability_notifications.sints() | sints);
    }

    fn inspect_extra(_this: &mut UhProcessor<'_, Self>, _resp: &mut inspect::Response<'_>) {}

    fn hv(&self, _vtl: GuestVtl) -> Option<&ProcessorVtlHv> {
        None
    }

    fn hv_mut(&mut self, _vtl: GuestVtl) -> Option<&mut ProcessorVtlHv> {
        None
    }

    fn handle_vp_start_enable_vtl_wake(_this: &mut UhProcessor<'_, Self>, _vtl: GuestVtl) {
        unimplemented!()
    }

    fn vtl1_inspectable(_this: &UhProcessor<'_, Self>) -> bool {
        // TODO: Use the VsmVpStatus register to query the hypervisor for
        // whether VTL 1 is enabled on the vp (this can be cached).
        false
    }
}

#[derive(Debug, Error)]
#[error("guest accessed unaccepted gpa {0}")]
struct UnacceptedMemoryAccess(u64);

impl UhProcessor<'_, HypervisorBackedArm64> {
    fn intercepted_vtl(message_header: &hvdef::HvArm64InterceptMessageHeader) -> GuestVtl {
        message_header.execution_state.vtl().try_into().unwrap()
    }

    fn handle_synic_deliverable_exit(&mut self) {
        let message = self
            .runner
            .exit_message()
            .as_message::<hvdef::HvArm64SynicSintDeliverableMessage>();

        tracing::trace!(
            deliverable_sints = message.deliverable_sints,
            "sint deliverable"
        );

        self.backing.deliverability_notifications.set_sints(
            self.backing.deliverability_notifications.sints() & !message.deliverable_sints,
        );

        // This is updated by `deliver_synic_messages below`, so clear it here.
        self.backing.next_deliverability_notifications.set_sints(0);

        // These messages are always VTL0, as VTL1 does not own any VMBUS channels.
        self.deliver_synic_messages(GuestVtl::Vtl0, message.deliverable_sints);
    }

    fn handle_hypercall_exit(&mut self, bus: &impl CpuIo) -> Result<(), VpHaltReason> {
        let message = self
            .runner
            .exit_message()
            .as_message::<hvdef::HvArm64HypercallInterceptMessage>();

        tracing::trace!(msg = %format_args!("{:x?}", message), "hypercall");

        let intercepted_vtl = Self::intercepted_vtl(&message.header);
        let guest_memory = &self.partition.gm[intercepted_vtl];
        let smccc_convention = message.immediate == 0;

        let handler = UhHypercallHandler {
            vp: self,
            bus,
            trusted: false,
            intercepted_vtl,
        };
        UhHypercallHandler::MSHV_DISPATCHER.dispatch(
            guest_memory,
            hv1_hypercall::Arm64RegisterIo::new(handler, false, smccc_convention),
        );
        Ok(())
    }

    async fn handle_mmio_exit(&mut self, dev: &impl CpuIo) -> Result<(), VpHaltReason> {
        let message = self
            .runner
            .exit_message()
            .as_message::<hvdef::HvArm64MemoryInterceptMessage>();
        // tracing::trace!(msg = %format_args!("{:x?}", message), "mmio");

        let intercept_state = InterceptState {
            instruction_bytes: message.instruction_bytes,
            instruction_byte_count: message.instruction_byte_count,
            gpa: Some(message.guest_physical_address),
            syndrome: aarch64defs::EsrEl2::from(message.syndrome),
            interruption_pending: message.header.execution_state.interruption_pending(),
        };

        let intercepted_vtl = Self::intercepted_vtl(&message.header);

        // Fast path for monitor page writes.
        if Some(message.guest_physical_address & !(hvdef::HV_PAGE_SIZE - 1))
            == self.partition.monitor_page.gpa()
            && message.header.intercept_access_type == hvdef::HvInterceptAccessType::WRITE
            && message.instruction_byte_count == 4
        {
            let gpa = message.guest_physical_address;
            let guest_memory = &self.partition.gm[intercepted_vtl];
            if let Some(mut bitmask) = emulate::emulate_mnf_write_fast_path(
                u32::from_ne_bytes(message.instruction_bytes),
                &mut UhEmulationState {
                    vp: &mut *self,
                    interruption_pending: intercept_state.interruption_pending,
                    devices: dev,
                    vtl: intercepted_vtl,
                    cache: UhCpuStateCache::default(),
                },
                guest_memory,
                dev,
            ) {
                let bit_offset = (gpa & (hvdef::HV_PAGE_SIZE - 1)) as u32 * 8;
                while bitmask != 0 {
                    let bit = 63 - bitmask.leading_zeros();
                    bitmask &= !(1 << bit);
                    if let Some(connection_id) =
                        self.partition.monitor_page.write_bit(bit_offset + bit)
                    {
                        signal_mnf(dev, connection_id);
                    }
                }
                return Ok(());
            }
        }

        let cache = UhCpuStateCache::default();
        self.emulate(dev, &intercept_state, intercepted_vtl, cache)
            .await
    }

    async fn handle_unaccepted_gpa_intercept(
        &mut self,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        let gpa = self
            .runner
            .exit_message()
            .as_message::<hvdef::HvArm64MemoryInterceptMessage>()
            .guest_physical_address;

        if self.partition.is_gpa_lower_vtl_ram(gpa) {
            // The host may have moved the page to an unaccepted state, so fail
            // here. This does not apply to VTL 2 memory - for unaccepted pages,
            // the intercept goes to host VTL0.
            //
            // Note: SGX memory should be included in this check, so if SGX is
            // no longer included in the lower_vtl_memory_layout, make sure the
            // appropriate changes are reflected here.
            Err(dev.fatal_error(UnacceptedMemoryAccess(gpa).into()))
        } else {
            // TODO: for hardware isolation, if the intercept is due to a guest
            // error, inject a machine check
            self.handle_mmio_exit(dev).await
        }
    }
}

#[derive(Default)]
pub struct UhCpuStateCache {
    pc: Option<(u64, bool)>,
    sp: Option<(u64, bool)>,
    x18: Option<(u64, bool)>,

    cpsr: Option<Cpsr64>,
}

impl<T: CpuIo> AccessCpuState for UhEmulationState<'_, '_, T, HypervisorBackedArm64> {
    fn commit(&mut self) {
        let mut expensive_regs = Vec::with_capacity(3);
        if let Some((x18, true)) = self.cache.x18 {
            expensive_regs.push((HvArm64RegisterName::X18, x18));
        }
        if let Some((pc, true)) = self.cache.pc {
            expensive_regs.push((HvArm64RegisterName::XPc, pc));
        }
        if let Some((sp, true)) = self.cache.sp {
            expensive_regs.push((HvArm64RegisterName::XSp, sp));
        }
        self.vp
            .runner
            .set_vp_registers(self.vtl, expensive_regs)
            .unwrap();
    }

    fn x(&mut self, index: u8) -> u64 {
        assert!(index < 31);
        if index == 18 {
            self.cache
                .x18
                .get_or_insert_with(|| {
                    (
                        self.vp
                            .runner
                            .get_vp_register(self.vtl, HvArm64RegisterName::X18)
                            .expect("register query should not fail")
                            .as_u64(),
                        false,
                    )
                })
                .0
        } else {
            self.vp.runner.cpu_context().x[index as usize]
        }
    }

    fn update_x(&mut self, index: u8, data: u64) {
        assert!(index < 31);
        self.vp.runner.cpu_context_mut().x[index as usize] = data;
        if index == 18 {
            self.cache.x18 = Some((data, true));
        }
    }

    fn q(&self, index: u8) -> u128 {
        assert!(index < 32);
        self.vp.runner.cpu_context().q[index as usize]
    }

    fn update_q(&mut self, index: u8, data: u128) {
        assert!(index < 32);
        self.vp.runner.cpu_context_mut().q[index as usize] = data;
    }

    fn d(&self, index: u8) -> u64 {
        (self.q(index) & 0xffffffff_ffffffff) as u64
    }

    fn update_d(&mut self, index: u8, data: u64) {
        self.update_q(index, data as u128);
    }

    fn h(&self, index: u8) -> u32 {
        (self.d(index) & 0xffffffff) as u32
    }

    fn update_h(&mut self, index: u8, data: u32) {
        self.update_q(index, data as u128);
    }

    fn s(&self, index: u8) -> u16 {
        (self.h(index) & 0xffff) as u16
    }

    fn update_s(&mut self, index: u8, data: u16) {
        self.update_q(index, data as u128);
    }

    fn b(&self, index: u8) -> u8 {
        (self.s(index) & 0xff) as u8
    }

    fn update_b(&mut self, index: u8, data: u8) {
        self.update_q(index, data as u128);
    }

    fn sp(&mut self) -> u64 {
        self.cache
            .sp
            .get_or_insert_with(|| {
                (
                    self.vp
                        .runner
                        .get_vp_register(self.vtl, HvArm64RegisterName::XSp)
                        .expect("register query should not fail")
                        .as_u64(),
                    false,
                )
            })
            .0
    }

    fn update_sp(&mut self, data: u64) {
        self.cache.sp = Some((data, true));
    }

    fn fp(&mut self) -> u64 {
        self.x(29)
    }

    fn update_fp(&mut self, data: u64) {
        self.update_x(29, data)
    }

    fn lr(&mut self) -> u64 {
        self.x(30)
    }

    fn update_lr(&mut self, data: u64) {
        self.update_x(30, data)
    }

    fn pc(&mut self) -> u64 {
        self.cache
            .pc
            .get_or_insert_with(|| {
                (
                    self.vp
                        .runner
                        .get_vp_register(self.vtl, HvArm64RegisterName::XPc)
                        .expect("register query should not fail")
                        .as_u64(),
                    false,
                )
            })
            .0
    }

    fn update_pc(&mut self, data: u64) {
        self.cache.pc = Some((data, true));
    }

    fn cpsr(&mut self) -> Cpsr64 {
        *self.cache.cpsr.get_or_insert_with(|| {
            self.vp
                .runner
                .get_vp_register(self.vtl, HvArm64RegisterName::Cpsr)
                .expect("register query should not fail")
                .as_u64()
                .into()
        })
    }
}

impl<T: CpuIo> EmulatorSupport for UhEmulationState<'_, '_, T, HypervisorBackedArm64> {
    fn vp_index(&self) -> VpIndex {
        self.vp.vp_index()
    }

    fn physical_address(&self) -> Option<u64> {
        let message = self.vp.runner.exit_message();
        match message.header.typ {
            HvMessageType::HvMessageTypeGpaIntercept
            | HvMessageType::HvMessageTypeUnmappedGpa
            | HvMessageType::HvMessageTypeUnacceptedGpa => Some(
                message
                    .as_message::<hvdef::HvArm64MemoryInterceptMessage>()
                    .guest_physical_address,
            ),
            _ => None,
        }
    }

    fn initial_gva_translation(&mut self) -> Option<emulate::InitialTranslation> {
        if (self.vp.runner.exit_message().header.typ != HvMessageType::HvMessageTypeGpaIntercept)
            && (self.vp.runner.exit_message().header.typ != HvMessageType::HvMessageTypeUnmappedGpa)
            && (self.vp.runner.exit_message().header.typ
                != HvMessageType::HvMessageTypeUnacceptedGpa)
        {
            return None;
        }

        let message = self
            .vp
            .runner
            .exit_message()
            .as_message::<hvdef::HvArm64MemoryInterceptMessage>();

        if !message.memory_access_info.gva_gpa_valid() {
            tracing::trace!(?message.guest_virtual_address, ?message.guest_physical_address, "gva gpa not valid {:?}", self.vp.runner.exit_message().payload());
            return None;
        }

        let translate_mode = emulate::TranslateMode::try_from(message.header.intercept_access_type)
            .expect("unexpected intercept access type");

        let translation = emulate::InitialTranslation {
            gva: message.guest_virtual_address,
            gpa: message.guest_physical_address,
            translate_mode,
        };

        tracing::trace!(?translation, "initial translation");

        // If we have a valid translation, the hypervisor must have set the TLB lock
        // so the translation remains valid for the duration of this exit.
        // Update our local cache appropriately.
        self.vp.mark_tlb_locked(Vtl::Vtl2, self.vtl);

        Some(translation)
    }

    fn interruption_pending(&self) -> bool {
        self.interruption_pending
    }

    fn check_vtl_access(
        &mut self,
        gpa: u64,
        mode: emulate::TranslateMode,
    ) -> Result<(), EmuCheckVtlAccessError> {
        // Underhill currently doesn't set VTL 2 protections against execute exclusively, it removes
        // all permissions from a page. So for VTL 1, no need to check the permissions; if VTL 1
        // doesn't have permissions to a page, Underhill should appropriately fail when it tries
        // to read or write to that page on VTL 1's behalf.
        //
        // For VTL 0, the alias map guards for read and write permissions, so only check VTL execute
        // permissions. Because VTL 2 will not restrict execute exclusively, only VTL 1 execute
        // permissions need to be checked and therefore only check permissions if VTL 1 is allowed.
        //
        // Note: the restriction to VTL 1 support also means that for WHP, which doesn't support VTL 1
        // the HvCheckSparseGpaPageVtlAccess hypercall--which is unimplemented in whp--will never be made.
        if mode == emulate::TranslateMode::Execute
            && self.vtl == GuestVtl::Vtl0
            && !matches!(
                *self.vp.shared.guest_vsm.read(),
                GuestVsmState::NotPlatformSupported,
            )
        {
            // Should always be called after translate gva with the tlb lock flag
            // or with an initial translation.
            debug_assert!(self.vp.is_tlb_locked(Vtl::Vtl2, self.vtl));

            let cpsr: Cpsr64 = self
                .vp
                .runner
                .get_vp_register(self.vtl, HvArm64RegisterName::SpsrEl2)
                .unwrap()
                .as_u64()
                .into();

            let flags = if cpsr.el() == 0 {
                HvMapGpaFlags::new().with_user_executable(true)
            } else {
                HvMapGpaFlags::new().with_kernel_executable(true)
            };

            let access_result = self
                .vp
                .partition
                .hcl
                .check_vtl_access(gpa, self.vtl, flags)
                .unwrap();

            if let Some(ioctl::CheckVtlAccessResult { vtl, denied_flags }) = access_result {
                return Err(EmuCheckVtlAccessError::AccessDenied { vtl, denied_flags });
            };
        }

        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        mode: emulate::TranslateMode,
    ) -> Result<EmuTranslateResult, EmuTranslateError> {
        let mut control_flags = hypercall::TranslateGvaControlFlagsArm64::new();
        match mode {
            emulate::TranslateMode::Read => control_flags.set_validate_read(true),
            emulate::TranslateMode::Write => {
                control_flags.set_validate_read(true);
                control_flags.set_validate_write(true);
            }
            emulate::TranslateMode::Execute => control_flags.set_validate_execute(true),
        };

        // The translation will be used, so set the appropriate page table bits
        // (the access/dirty bit).
        //
        // Prevent flushes in order to make sure that translation of this GVA
        // remains usable until the VP is resumed back to direct execution.
        control_flags.set_set_page_table_bits(true);
        control_flags.set_tlb_flush_inhibit(true);

        // In case we're not running ring 0, check privileges against VP state
        // as of when the original intercept came in - since the emulator
        // doesn't support instructions that change ring level, the ring level
        // will remain the same as it was in the VP state as of when the
        // original intercept came in. The privilege exempt flag should
        // not be set.
        assert!(!control_flags.pan_clear());

        // Do the translation using the current VTL.
        control_flags.set_input_vtl(self.vtl.into());

        match self
            .vp
            .partition
            .hcl
            .translate_gva_to_gpa(gva, control_flags)
            .unwrap()
        {
            Ok(ioctl::TranslateResult {
                gpa_page,
                overlay_page,
            }) => {
                self.vp.mark_tlb_locked(Vtl::Vtl2, self.vtl);
                Ok(EmuTranslateResult {
                    gpa: (gpa_page << hvdef::HV_PAGE_SHIFT) + (gva & (hvdef::HV_PAGE_SIZE - 1)),
                    overlay_page: Some(overlay_page),
                })
            }
            Err(ioctl::aarch64::TranslateErrorAarch64 { code }) => Err(EmuTranslateError {
                code: hypercall::TranslateGvaResultCode(code),
                event_info: None,
            }),
        }
    }

    fn inject_pending_event(&mut self, event_info: HvAarch64PendingEvent) {
        let regs = [(
            HvArm64RegisterName::PendingEvent0,
            u128::from_ne_bytes(event_info.as_bytes().try_into().unwrap()),
        )];

        self.vp
            .runner
            .set_vp_registers_hvcall(self.vtl.into(), regs)
            .expect("set_vp_registers hypercall for setting pending event should not fail");
    }

    fn monitor_support(&self) -> Option<&dyn EmulatorMonitorSupport> {
        Some(self)
    }

    fn is_gpa_mapped(&self, gpa: u64, write: bool) -> bool {
        self.vp.partition.is_gpa_mapped(gpa, write)
    }
}

impl<T: CpuIo> UhHypercallHandler<'_, '_, T, HypervisorBackedArm64> {
    const MSHV_DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [
            hv1_hypercall::HvPostMessage,
            hv1_hypercall::HvSignalEvent,
            hv1_hypercall::HvRetargetDeviceInterrupt,
            hv1_hypercall::HvGetVpIndexFromApicId,
        ]
    );
}

impl<T: CpuIo> hv1_hypercall::RetargetDeviceInterrupt
    for UhHypercallHandler<'_, '_, T, HypervisorBackedArm64>
{
    fn retarget_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        params: hv1_hypercall::HvInterruptParameters<'_>,
    ) -> hvdef::HvResult<()> {
        self.retarget_virtual_interrupt(
            device_id,
            address,
            data,
            params.vector,
            params.multicast,
            params.target_processors,
        )
    }
}

trait ToVpRegisterName: 'static + Copy + std::fmt::Debug {
    fn to_vp_reg_name(self) -> VpRegisterName;
}

impl ToVpRegisterName for VpRegisterName {
    fn to_vp_reg_name(self) -> VpRegisterName {
        self
    }
}

impl UhVpStateAccess<'_, '_, HypervisorBackedArm64> {
    fn set_register_state<T, R: ToVpRegisterName, const N: usize>(
        &mut self,
        regs: &T,
    ) -> Result<(), vp_state::Error>
    where
        T: HvRegisterState<R, N>,
    {
        let names = regs.names().map(|r| r.to_vp_reg_name());
        let mut values = [HvRegisterValue::new_zeroed(); N];
        regs.get_values(values.iter_mut());
        self.vp
            .runner
            .set_vp_registers(self.vtl, names.iter().copied().zip(values))
            .map_err(vp_state::Error::SetRegisters)?;
        Ok(())
    }

    fn get_register_state<T, R: ToVpRegisterName, const N: usize>(
        &mut self,
    ) -> Result<T, vp_state::Error>
    where
        T: HvRegisterState<R, N>,
    {
        let mut regs = T::default();
        let names = regs.names().map(|r| r.to_vp_reg_name());
        let mut values = [HvRegisterValue::new_zeroed(); N];
        self.vp
            .runner
            .get_vp_registers(self.vtl, &names, &mut values)
            .map_err(vp_state::Error::GetRegisters)?;

        regs.set_values(values.into_iter());
        Ok(regs)
    }

    /// Get the system VP registers on the current VP.
    fn get_system_registers(&mut self) -> Result<vp::SystemRegisters, vp_state::Error> {
        self.get_register_state()
    }

    /// Set the system VP registers on the current VP.
    fn set_system_registers(&mut self, regs: &vp::SystemRegisters) -> Result<(), vp_state::Error> {
        self.set_register_state(regs)
    }
}

impl AccessVpState for UhVpStateAccess<'_, '_, HypervisorBackedArm64> {
    type Error = vp_state::Error;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.vp.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn registers(&mut self) -> Result<vp::Registers, Self::Error> {
        self.get_register_state()
    }

    fn set_registers(&mut self, value: &vp::Registers) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn system_registers(&mut self) -> Result<vp::SystemRegisters, Self::Error> {
        self.get_system_registers()
    }

    fn set_system_registers(&mut self, value: &vp::SystemRegisters) -> Result<(), Self::Error> {
        self.set_system_registers(value)
    }
}

mod save_restore {
    use super::HypervisorBackedArm64;
    use super::UhProcessor;
    use anyhow::anyhow;
    use hcl::GuestVtl;
    use hvdef::HvArm64RegisterName;
    use hvdef::HvInternalActivityRegister;
    use virt::Processor;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "underhill.partition")]
        pub struct ProcessorSavedState {
            #[mesh(1)]
            pub(super) x: [u64; 31],
            #[mesh(2)]
            pub(super) q: [u128; 32],
            #[mesh(3)]
            pub(super) startup_suspend: bool,
        }
    }

    impl SaveRestore for UhProcessor<'_, HypervisorBackedArm64> {
        type SavedState = state::ProcessorSavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            // Ensure all async requests are reflected in the saved state.
            self.flush_async_requests();

            let internal_activity = self
                .runner
                // Non-VTL0 VPs should never be in startup suspend, so we only need to check VTL0.
                // The hypervisor handles halt and idle for us.
                .get_vp_register(GuestVtl::Vtl0, HvArm64RegisterName::InternalActivityState)
                .map_err(|err| {
                    SaveError::Other(anyhow!("unable to query startup suspend: {}", err))
                })?;
            let startup_suspend =
                HvInternalActivityRegister::from(internal_activity.as_u64()).startup_suspend();
            let state = state::ProcessorSavedState {
                x: self.runner.cpu_context().x,
                q: self.runner.cpu_context().q,
                startup_suspend,
            };

            Ok(state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            self.runner.cpu_context_mut().x = state.x;
            self.runner.cpu_context_mut().q = state.q;
            self.set_vtl0_startup_suspend(state.startup_suspend)
                .expect("setting startup suspend should not fail");
            Ok(())
        }
    }
}
