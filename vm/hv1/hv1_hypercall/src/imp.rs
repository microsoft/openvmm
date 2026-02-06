// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for individual hypercalls.

use super::support::HypercallDispatch;
use super::support::HypercallParameters;
use super::support::RepHypercall;
use super::support::SimpleHypercall;
use super::support::VariableHypercall;
use super::support::VtlHypercall;
use crate::support::HvRepResult;
use crate::support::VariableRepHypercall;
use hv1_structs::ProcessorSet;
use hvdef::HvError;
use hvdef::HvMessage;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use hvdef::HvResult;
use hvdef::HypercallCode;
use hvdef::Vtl;
use hvdef::hypercall as defs;
use hvdef::hypercall::AcceptPagesAttributes;
use hvdef::hypercall::HostVisibilityType;
use hvdef::hypercall::HvRegisterAssoc;
use hvdef::hypercall::HypercallOutput;
use hvdef::hypercall::VtlPermissionSet;
use zerocopy::IntoBytes;

/// Implements the `HvPostMessage` hypercall.
pub trait PostMessage {
    /// Post a synic message.
    fn post_message(&mut self, connection_id: u32, message: &[u8]) -> HvResult<()>;
}

/// Defines the `HvPostMessage` hypercall.
pub type HvPostMessage =
    SimpleHypercall<defs::PostMessage, (), { HypercallCode::HvCallPostMessage.0 }>;

impl<T: PostMessage> HypercallDispatch<HvPostMessage> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvPostMessage::run(params, |input| {
            self.post_message(
                input.connection_id,
                input
                    .payload
                    .as_bytes()
                    .get(..input.payload_size as usize)
                    .ok_or(HvError::InvalidParameter)?,
            )
        })
    }
}

/// Implements the `HvSignalEvent` hypercall.
pub trait SignalEvent {
    /// Signal synic event.
    fn signal_event(&mut self, connection_id: u32, flag: u16) -> HvResult<()>;
}

/// Defines the `HvSignalEvent` hypercall.
pub type HvSignalEvent =
    SimpleHypercall<defs::SignalEvent, (), { HypercallCode::HvCallSignalEvent.0 }>;

impl<T: SignalEvent> HypercallDispatch<HvSignalEvent> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvSignalEvent::run(params, |input| {
            self.signal_event(input.connection_id, input.flag_number)
        })
    }
}

/// Implements the `HvPostMessageDirect` hypercall.
pub trait PostMessageDirect {
    /// Posts a message directly, without going through a port/connection.
    fn post_message_direct(
        &mut self,
        partition_id: u64,
        target_vtl: Vtl,
        vp: u32,
        sint: u8,
        message: &HvMessage,
    ) -> HvResult<()>;
}

/// Defines the `HvPostMessageDirect` hypercall.
pub type HvPostMessageDirect =
    SimpleHypercall<defs::PostMessageDirect, (), { HypercallCode::HvCallPostMessageDirect.0 }>;

impl<T: PostMessageDirect> HypercallDispatch<HvPostMessageDirect> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvPostMessageDirect::run(params, |input| {
            let message = input.message.get();
            self.post_message_direct(
                input.partition_id,
                Vtl::try_from(input.vtl)?,
                input.vp_index,
                input.sint,
                &message,
            )
        })
    }
}

/// Implements the `HvSignalEventDirect` hypercall.
pub trait SignalEventDirect {
    /// Signal synic event directly, without going through a port/connection.
    fn signal_event_direct(
        &mut self,
        partition_id: u64,
        vtl: Vtl,
        vp: u32,
        sint: u8,
        flag: u16,
    ) -> HvResult<defs::SignalEventDirectOutput>;
}

/// Defines the `HvSignalEventDirect` hypercall.
pub type HvSignalEventDirect = SimpleHypercall<
    defs::SignalEventDirect,
    defs::SignalEventDirectOutput,
    { HypercallCode::HvCallSignalEventDirect.0 },
>;

impl<T: SignalEventDirect> HypercallDispatch<HvSignalEventDirect> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvSignalEventDirect::run(params, |input| {
            self.signal_event_direct(
                input.target_partition,
                Vtl::try_from(input.target_vtl)?,
                input.target_vp,
                input.target_sint,
                input.flag_number,
            )
        })
    }
}

/// Implements the `HvRetargetDeviceInterrupt` hypercall.
pub trait RetargetDeviceInterrupt {
    /// Retargets a device interrupt to a new processor set.
    fn retarget_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        params: HvInterruptParameters<'_>,
    ) -> HvResult<()>;
}

/// Configuration for a hypervisor device interrupt.
pub struct HvInterruptParameters<'a> {
    /// The target interrupt vector.
    pub vector: u32,
    /// Whether this is a multicast interrupt.
    pub multicast: bool,
    /// A target processor list.
    pub target_processors: ProcessorSet<'a>,
}

/// Defines the `HvRetargetDeviceInterrupt` hypercall.
pub type HvRetargetDeviceInterrupt = VariableHypercall<
    defs::RetargetDeviceInterrupt,
    (),
    { HypercallCode::HvCallRetargetDeviceInterrupt.0 },
>;

impl<T: RetargetDeviceInterrupt> HypercallDispatch<HvRetargetDeviceInterrupt> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvRetargetDeviceInterrupt::run(params, |input, var_input| {
            if input.target_header.flags.reserved() != 0 {
                return Err(HvError::InvalidParameter);
            }

            let masks = &[input.target_header.mask_or_format];
            let target_processors = if input.target_header.flags.processor_set() {
                ProcessorSet::from_generic_set(input.target_header.mask_or_format, var_input)
            } else {
                ProcessorSet::from_processor_masks(1, masks)
            }
            .ok_or(HvError::InvalidParameter)?;

            if input.entry.source != defs::HvInterruptSource::MSI {
                return Err(HvError::InvalidParameter);
            }

            self.retarget_interrupt(
                input.device_id,
                input.entry.data[0] as u64,
                input.entry.data[1],
                HvInterruptParameters {
                    vector: input.target_header.vector,
                    multicast: input.target_header.flags.multicast(),
                    target_processors,
                },
            )
        })
    }
}

/// Defines the `HvAssertVirtualInterrupt` hypercall.
pub type HvAssertVirtualInterrupt = SimpleHypercall<
    defs::AssertVirtualInterrupt,
    (),
    { HypercallCode::HvCallAssertVirtualInterrupt.0 },
>;

/// Implements the `HvAssertVirtualInterrupt` hypercall.
pub trait AssertVirtualInterrupt {
    /// Asserts a virtual interrupt.
    fn assert_virtual_interrupt(
        &mut self,
        partition_id: u64,
        interrupt_control: hvdef::HvInterruptControl,
        destination_address: u64,
        requested_vector: u32,
        target_vtl: Vtl,
    ) -> HvResult<()>;
}

impl<T: AssertVirtualInterrupt> HypercallDispatch<HvAssertVirtualInterrupt> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvAssertVirtualInterrupt::run(params, |input| {
            if input.rsvd0 != 0 || input.rsvd1 != 0 {
                return Err(HvError::InvalidParameter);
            }
            self.assert_virtual_interrupt(
                input.partition_id,
                input.interrupt_control,
                input.destination_address,
                input.requested_vector,
                input.target_vtl.try_into()?,
            )
        })
    }
}

/// Defines the `HvStartVirtualProcessor` hypercall for x64.
pub type HvX64StartVirtualProcessor = SimpleHypercall<
    defs::StartVirtualProcessorX64,
    (),
    { HypercallCode::HvCallStartVirtualProcessor.0 },
>;

/// Implements the `HvStartVirtualProcessor` hypercall.
pub trait StartVirtualProcessor<T> {
    /// Starts a virtual processor.
    fn start_virtual_processor(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        target_vtl: Vtl,
        vp_context: &T,
    ) -> HvResult<()>;
}

impl<T: StartVirtualProcessor<defs::InitialVpContextX64>>
    HypercallDispatch<HvX64StartVirtualProcessor> for T
{
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvX64StartVirtualProcessor::run(params, |input| {
            self.start_virtual_processor(
                input.partition_id,
                input.vp_index,
                Vtl::try_from(input.target_vtl)?,
                &input.vp_context,
            )
        })
    }
}

/// Defines the `HvStartVirtualProcessor` hypercall for arm64.
pub type HvArm64StartVirtualProcessor = SimpleHypercall<
    defs::StartVirtualProcessorArm64,
    (),
    { HypercallCode::HvCallStartVirtualProcessor.0 },
>;

impl<T: StartVirtualProcessor<defs::InitialVpContextArm64>>
    HypercallDispatch<HvArm64StartVirtualProcessor> for T
{
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvArm64StartVirtualProcessor::run(params, |input| {
            self.start_virtual_processor(
                input.partition_id,
                input.vp_index,
                Vtl::try_from(input.target_vtl)?,
                &input.vp_context,
            )
        })
    }
}

/// Defines the `HvTranslateVirtualAddress` hypercall.
pub type HvX64TranslateVirtualAddress = SimpleHypercall<
    defs::TranslateVirtualAddressX64,
    defs::TranslateVirtualAddressOutput,
    { HypercallCode::HvCallTranslateVirtualAddress.0 },
>;

/// Implements the `HvTranslateVirtualAddress` hypercall.
pub trait TranslateVirtualAddressX64 {
    /// Translates a GVA to a GPA.
    fn translate_virtual_address(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        control_flags: defs::TranslateGvaControlFlagsX64,
        gva_page: u64,
    ) -> HvResult<defs::TranslateVirtualAddressOutput>;
}

impl<T: TranslateVirtualAddressX64> HypercallDispatch<HvX64TranslateVirtualAddress> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvX64TranslateVirtualAddress::run(params, |input| {
            self.translate_virtual_address(
                input.partition_id,
                input.vp_index,
                input.control_flags,
                input.gva_page,
            )
        })
    }
}

/// Defines the `HvTranslateVirtualAddressEx` hypercall.
pub type HvX64TranslateVirtualAddressEx = SimpleHypercall<
    defs::TranslateVirtualAddressX64,
    defs::TranslateVirtualAddressExOutputX64,
    { HypercallCode::HvCallTranslateVirtualAddressEx.0 },
>;

/// Implements the `HvTranslateVirtualAddressEx` hypercall.
pub trait TranslateVirtualAddressExX64 {
    /// Translates a GVA to a GPA.
    fn translate_virtual_address_ex(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        control_flags: defs::TranslateGvaControlFlagsX64,
        gva_page: u64,
    ) -> HvResult<defs::TranslateVirtualAddressExOutputX64>;
}

impl<T: TranslateVirtualAddressExX64> HypercallDispatch<HvX64TranslateVirtualAddressEx> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvX64TranslateVirtualAddressEx::run(params, |input| {
            self.translate_virtual_address_ex(
                input.partition_id,
                input.vp_index,
                input.control_flags,
                input.gva_page,
            )
        })
    }
}

/// Defines the `HvTranslateVirtualAddressEx` hypercall.
pub type HvAarch64TranslateVirtualAddressEx = SimpleHypercall<
    defs::TranslateVirtualAddressArm64,
    defs::TranslateVirtualAddressExOutputArm64,
    { HypercallCode::HvCallTranslateVirtualAddressEx.0 },
>;

/// Implements the `HvTranslateVirtualAddressEx` hypercall.
pub trait TranslateVirtualAddressExAarch64 {
    /// Translates a GVA to a GPA.
    fn translate_virtual_address_ex(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        control_flags: defs::TranslateGvaControlFlagsArm64,
        gva_page: u64,
    ) -> HvResult<defs::TranslateVirtualAddressExOutputArm64>;
}

impl<T: TranslateVirtualAddressExAarch64> HypercallDispatch<HvAarch64TranslateVirtualAddressEx>
    for T
{
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvAarch64TranslateVirtualAddressEx::run(params, |input| {
            self.translate_virtual_address_ex(
                input.partition_id,
                input.vp_index,
                input.control_flags,
                input.gva_page,
            )
        })
    }
}

/// Defines the `HvGetVpRegisters` hypercall.
pub type HvGetVpRegisters = RepHypercall<
    defs::GetSetVpRegisters,
    HvRegisterName,
    HvRegisterValue,
    { HypercallCode::HvCallGetVpRegisters.0 },
>;

/// Implements the `HvGetVpRegisters` hypercall.
pub trait GetVpRegisters {
    /// Gets the requested registers.
    fn get_vp_registers(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Option<Vtl>,
        registers: &[HvRegisterName],
        output: &mut [HvRegisterValue],
    ) -> HvRepResult;
}

impl<T: GetVpRegisters> HypercallDispatch<HvGetVpRegisters> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvGetVpRegisters::run(params, |header, input, output| {
            self.get_vp_registers(
                header.partition_id,
                header.vp_index,
                header.target_vtl.target_vtl().map_err(|err| (err, 0))?,
                input,
                output,
            )
        })
    }
}

/// Defines the `HvSetVpRegisters` hypercall.
pub type HvSetVpRegisters = RepHypercall<
    defs::GetSetVpRegisters,
    HvRegisterAssoc,
    (),
    { HypercallCode::HvCallSetVpRegisters.0 },
>;

/// Implements the `HvSetVpRegisters` hypercall.
pub trait SetVpRegisters {
    /// Sets the requested registers.
    fn set_vp_registers(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Option<Vtl>,
        registers: &[HvRegisterAssoc],
    ) -> HvRepResult;
}

impl<T: SetVpRegisters> HypercallDispatch<HvSetVpRegisters> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvSetVpRegisters::run(params, |header, input, _output| {
            self.set_vp_registers(
                header.partition_id,
                header.vp_index,
                header.target_vtl.target_vtl().map_err(|err| (err, 0))?,
                input,
            )
        })
    }
}

/// Implements the `HvInstallIntercept` hypercall.
pub trait InstallIntercept {
    /// Post a synic message.
    fn install_intercept(
        &mut self,
        partition_id: u64,
        access_type_mask: u32,
        intercept_type: defs::HvInterceptType,
        intercept_parameters: defs::HvInterceptParameters,
    ) -> HvResult<()>;
}

/// Defines the `HvInstallIntercept` hypercall.
pub type HvInstallIntercept =
    SimpleHypercall<defs::InstallIntercept, (), { HypercallCode::HvCallInstallIntercept.0 }>;

impl<T: InstallIntercept> HypercallDispatch<HvInstallIntercept> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvInstallIntercept::run(params, |input| {
            self.install_intercept(
                input.partition_id,
                input.access_type_mask,
                input.intercept_type,
                input.intercept_parameters,
            )
        })
    }
}

/// Operations required to handle VTL switch hypercalls.
pub trait VtlSwitchOps {
    /// Advances the instruction pointer for a vtl switch operation whose preconditions have been
    /// satisfied, in the context of the initiating vtl.
    fn advance_ip(&mut self);
    /// Injects an invalid opcode fault for a vtl switch operation whose preconditions have been
    /// violated, in the context of the initiating vtl.
    fn inject_invalid_opcode_fault(&mut self);
}

/// Implements the `HvVtlReturn` hypercall.
pub trait VtlReturn {
    /// Checks if a return to a lower vtl is allowed based on current state.
    fn is_vtl_return_allowed(&self) -> bool;

    /// Return to a lower VTL.
    fn vtl_return(&mut self, fast: bool);
}

/// Defines the `HvVtlReturn` hypercall.
pub type HvVtlReturn = VtlHypercall<{ HypercallCode::HvCallVtlReturn.0 }>;

impl<T: VtlReturn + VtlSwitchOps> HypercallDispatch<HvVtlReturn> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvVtlReturn::run(params, |input, control| {
            // Preconditions for a successful vtl return:
            // 1. Control must be 0 except for the opcode.
            // 2. Input must be 0 or 1.
            // 3. VTL state must allow vtl returns.
            if u64::from(control.with_code(0)) == 0
                && (input & !1) == 0
                && self.is_vtl_return_allowed()
            {
                // Advance the instruction pointer and issue the vtl return.
                self.advance_ip();
                self.vtl_return(input & 1 != 0);
            } else {
                // Inject an error.
                self.inject_invalid_opcode_fault();
            }
        })
    }
}

/// Implements the `HvVtlCall` hypercall.
pub trait VtlCall {
    /// Checks if a call to a higher vtl is allowed based on current state.
    fn is_vtl_call_allowed(&self) -> bool;
    /// Calls the higher VTL.
    fn vtl_call(&mut self);
}

/// Defines the `HvVtlCall` hypercall.
pub type HvVtlCall = VtlHypercall<{ HypercallCode::HvCallVtlCall.0 }>;

impl<T: VtlCall + VtlSwitchOps> HypercallDispatch<HvVtlCall> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvVtlCall::run(params, |input, control| {
            // Preconditions for a successful vtl call:
            // 1. Control must be 0 except for the opcode.
            // 2. Input must be 0.
            // 3. VTL state must allow a call to higher VTLs.
            if u64::from(control.with_code(0)) == 0 && input == 0 && self.is_vtl_call_allowed() {
                // Advance the instruction pointer and issue the vtl call.
                self.advance_ip();
                self.vtl_call();
            } else {
                self.inject_invalid_opcode_fault();
            }
        })
    }
}

/// Implements the `HvEnableVpVtl` hypercall.
pub trait EnableVpVtl<T> {
    /// Enable the specified VTL.
    fn enable_vp_vtl(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Vtl,
        vp_context: &T,
    ) -> HvResult<()>;
}

/// Defines the `HvEnableVpVtl` hypercall for x64.
pub type HvX64EnableVpVtl =
    SimpleHypercall<defs::EnableVpVtlX64, (), { HypercallCode::HvCallEnableVpVtl.0 }>;

impl<T: EnableVpVtl<hvdef::hypercall::InitialVpContextX64>> HypercallDispatch<HvX64EnableVpVtl>
    for T
{
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvX64EnableVpVtl::run(params, |input| {
            self.enable_vp_vtl(
                input.partition_id,
                input.vp_index,
                Vtl::try_from(input.target_vtl)?,
                &input.vp_vtl_context,
            )
        })
    }
}

/// Defines the `HvEnableVpVtl` hypercall for arm64.
pub type HvArm64EnableVpVtl =
    SimpleHypercall<defs::EnableVpVtlArm64, (), { HypercallCode::HvCallEnableVpVtl.0 }>;

impl<T: EnableVpVtl<hvdef::hypercall::InitialVpContextArm64>> HypercallDispatch<HvArm64EnableVpVtl>
    for T
{
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvArm64EnableVpVtl::run(params, |input| {
            self.enable_vp_vtl(
                input.partition_id,
                input.vp_index,
                Vtl::try_from(input.target_vtl)?,
                &input.vp_vtl_context,
            )
        })
    }
}

/// Implements the `HvModifyVtlProtectionMask` hypercall.
pub trait ModifyVtlProtectionMask {
    /// Modify the VTL protection mask for the list of pages specified by `gpa_pages`.
    /// `map_flags` represents the desired permissions for VTLs lower than `target_vtl`.
    /// `target_vtl` must be lower or equal to the current VTL. It cannot be VTL0.
    fn modify_vtl_protection_mask(
        &mut self,
        partition_id: u64,
        map_flags: hvdef::HvMapGpaFlags,
        target_vtl: Option<Vtl>,
        gpa_pages: &[u64],
    ) -> HvRepResult;
}

/// Defines the `HvModifyVtlProtectionMask` hypercall.
pub type HvModifyVtlProtectionMask = RepHypercall<
    defs::ModifyVtlProtectionMask,
    u64,
    (),
    { HypercallCode::HvCallModifyVtlProtectionMask.0 },
>;

impl<T: ModifyVtlProtectionMask> HypercallDispatch<HvModifyVtlProtectionMask> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvModifyVtlProtectionMask::run(params, |header, input, _output| {
            self.modify_vtl_protection_mask(
                header.partition_id,
                header.map_flags,
                header.target_vtl.target_vtl().map_err(|err| (err, 0))?,
                input,
            )
        })
    }
}

/// Implements the `HvGetVpIndexFromApicId` hypercall.
pub trait GetVpIndexFromApicId {
    /// Gets a list of VP indices from a list of APIC IDs.
    fn get_vp_index_from_apic_id(
        &mut self,
        partition_id: u64,
        target_vtl: Vtl,
        apic_ids: &[u32],
        vp_indices: &mut [u32],
    ) -> HvRepResult;
}

/// Defines the `HvGetVpIndexFromApicId` hypercall.
pub type HvGetVpIndexFromApicId = RepHypercall<
    defs::GetVpIndexFromApicId,
    u32,
    u32,
    { HypercallCode::HvCallGetVpIndexFromApicId.0 },
>;

impl<T: GetVpIndexFromApicId> HypercallDispatch<HvGetVpIndexFromApicId> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvGetVpIndexFromApicId::run(params, |header, input, output| {
            self.get_vp_index_from_apic_id(
                header.partition_id,
                Vtl::try_from(header.target_vtl).map_err(|err| (err, 0))?,
                input,
                output,
            )
        })
    }
}

/// Implements the `HvAcceptGpaPages` hypercall.
pub trait AcceptGpaPages {
    /// Accepts the described pages.
    fn accept_gpa_pages(
        &mut self,
        partition_id: u64,
        page_attributes: AcceptPagesAttributes,
        vtl_permission_set: VtlPermissionSet,
        gpa_page_base: u64,
        page_count: usize,
    ) -> HvRepResult;
}

/// Defines the `HvAcceptGpaPages` hypercall.
pub type HvAcceptGpaPages =
    RepHypercall<defs::AcceptGpaPages, u64, (), { HypercallCode::HvCallAcceptGpaPages.0 }>;

impl<T: AcceptGpaPages> HypercallDispatch<HvAcceptGpaPages> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvAcceptGpaPages::run(params, |header, input, _output| {
            self.accept_gpa_pages(
                header.partition_id,
                header.page_attributes,
                header.vtl_permission_set,
                header.gpa_page_base,
                input.len(),
            )
        })
    }
}

/// Implements the `HvModifySparseGpaPageHostVisibility` hypercall.
pub trait ModifySparseGpaPageHostVisibility {
    /// Modifies the host page visibility for the listed pages.
    fn modify_gpa_visibility(
        &mut self,
        partition_id: u64,
        visibility: HostVisibilityType,
        gpa_pages: &[u64],
    ) -> HvRepResult;
}

/// Defines the `HvModifySparseGpaPageHostVisibility` hypercall.
pub type HvModifySparseGpaPageHostVisibility = RepHypercall<
    defs::ModifySparsePageVisibility,
    u64,
    (),
    { HypercallCode::HvCallModifySparseGpaPageHostVisibility.0 },
>;

impl<T: ModifySparseGpaPageHostVisibility> HypercallDispatch<HvModifySparseGpaPageHostVisibility>
    for T
{
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvModifySparseGpaPageHostVisibility::run(params, |header, input, _output| {
            self.modify_gpa_visibility(
                header.partition_id,
                header.host_visibility.host_visibility(),
                input,
            )
        })
    }
}

/// Implements the `HvEnablePartitionVtl` hypercall.
pub trait EnablePartitionVtl {
    /// Enables the VTL for the partition.
    fn enable_partition_vtl(
        &mut self,
        partition_id: u64,
        target_vtl: Vtl,
        flags: defs::EnablePartitionVtlFlags,
    ) -> HvResult<()>;
}

/// Defines the `HvEnablePartitionVtl` hypercall.
pub type HvEnablePartitionVtl =
    SimpleHypercall<defs::EnablePartitionVtl, (), { HypercallCode::HvCallEnablePartitionVtl.0 }>;

impl<T: EnablePartitionVtl> HypercallDispatch<HvEnablePartitionVtl> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvEnablePartitionVtl::run(params, |input| {
            self.enable_partition_vtl(
                input.partition_id,
                Vtl::try_from(input.target_vtl).map_err(|_| HvError::AccessDenied)?,
                input.flags,
            )
        })
    }
}

/// Implements the `HvFlushVirtualAddressList` hypercall.
pub trait FlushVirtualAddressList {
    /// Invalidates portions of the virtual TLB.
    fn flush_virtual_address_list(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: defs::HvFlushFlags,
        gva_ranges: &[defs::HvGvaRange],
    ) -> HvRepResult;
}

/// Defines the `HvFlushVirtualAddressList` hypercall.
pub type HvFlushVirtualAddressList = RepHypercall<
    defs::FlushVirtualAddressSpace,
    defs::HvGvaRange,
    (),
    { HypercallCode::HvCallFlushVirtualAddressList.0 },
>;

impl<T: FlushVirtualAddressList> HypercallDispatch<HvFlushVirtualAddressList> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvFlushVirtualAddressList::run(params, |header, input, _output| {
            let masks = &[header.processor_mask];
            let processors = ProcessorSet::from_processor_masks(1, masks)
                .ok_or((HvError::InvalidParameter, 0))?;
            self.flush_virtual_address_list(processors, header.flags, input)
        })
    }
}

/// Implements the `HvFlushVirtualAddressListEx` hypercall.
pub trait FlushVirtualAddressListEx {
    /// Invalidates portions of the virtual TLB.
    fn flush_virtual_address_list_ex(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: defs::HvFlushFlags,
        gva_ranges: &[defs::HvGvaRange],
    ) -> HvRepResult;
}

/// Defines the `HvFlushVirtualAddressListEx` hypercall.
pub type HvFlushVirtualAddressListEx = VariableRepHypercall<
    defs::FlushVirtualAddressSpaceEx,
    defs::HvGvaRange,
    (),
    { HypercallCode::HvCallFlushVirtualAddressListEx.0 },
>;

impl<T: FlushVirtualAddressListEx> HypercallDispatch<HvFlushVirtualAddressListEx> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvFlushVirtualAddressListEx::run(params, |header, variable_input, input, _output| {
            if header.vp_set_format != hvdef::hypercall::HV_GENERIC_SET_SPARSE_4K {
                return Err((HvError::InvalidParameter, 0));
            }

            let processors =
                ProcessorSet::from_processor_masks(header.vp_set_valid_banks_mask, variable_input)
                    .ok_or((HvError::InvalidParameter, 0))?;
            self.flush_virtual_address_list_ex(processors, header.flags, input)
        })
    }
}

/// Implements the `HvFlushVirtualAddressSpace` hypercall.
pub trait FlushVirtualAddressSpace {
    /// Invalidates all virtual TLB entries.
    fn flush_virtual_address_space(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: defs::HvFlushFlags,
    ) -> HvResult<()>;
}

/// Defines the `HvFlushVirtualAddressSpace` hypercall.
pub type HvFlushVirtualAddressSpace = SimpleHypercall<
    defs::FlushVirtualAddressSpace,
    (),
    { HypercallCode::HvCallFlushVirtualAddressSpace.0 },
>;

impl<T: FlushVirtualAddressSpace> HypercallDispatch<HvFlushVirtualAddressSpace> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvFlushVirtualAddressSpace::run(params, |input| {
            let masks = &[input.processor_mask];
            let processors =
                ProcessorSet::from_processor_masks(1, masks).ok_or(HvError::InvalidParameter)?;
            self.flush_virtual_address_space(processors, input.flags)
        })
    }
}

/// Implements the `HvFlushVirtualAddressSpaceEx` hypercall.
pub trait FlushVirtualAddressSpaceEx {
    /// Invalidates all virtual TLB entries.
    fn flush_virtual_address_space_ex(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: defs::HvFlushFlags,
    ) -> HvResult<()>;
}

/// Defines the `HvFlushVirtualAddressSpaceEx` hypercall.
pub type HvFlushVirtualAddressSpaceEx = VariableHypercall<
    defs::FlushVirtualAddressSpaceEx,
    (),
    { HypercallCode::HvCallFlushVirtualAddressSpaceEx.0 },
>;

impl<T: FlushVirtualAddressSpaceEx> HypercallDispatch<HvFlushVirtualAddressSpaceEx> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvFlushVirtualAddressSpaceEx::run(params, |header, input| {
            if header.vp_set_format != hvdef::hypercall::HV_GENERIC_SET_SPARSE_4K {
                return Err(HvError::InvalidParameter);
            }

            let processors =
                ProcessorSet::from_processor_masks(header.vp_set_valid_banks_mask, input)
                    .ok_or(HvError::InvalidParameter)?;
            self.flush_virtual_address_space_ex(processors, header.flags)
        })
    }
}

/// Implements the `HvQuerySparseGpaPageHostVisibility` hypercall.
pub trait QuerySparseGpaPageHostVisibility {
    /// Queries the host page visibility for the listed pages.
    fn query_gpa_visibility(
        &mut self,
        partition_id: u64,
        gpa_pages: &[u64],
        host_visibility: &mut [HostVisibilityType],
    ) -> HvRepResult;
}

/// Defines the `HvQuerySparseGpaPageHostVisibility` hypercall.
pub type HvQuerySparseGpaPageHostVisibility = RepHypercall<
    defs::QuerySparsePageVisibility,
    u64,
    HostVisibilityType,
    { HypercallCode::HvCallQuerySparseGpaPageHostVisibility.0 },
>;

impl<T: QuerySparseGpaPageHostVisibility> HypercallDispatch<HvQuerySparseGpaPageHostVisibility>
    for T
{
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvQuerySparseGpaPageHostVisibility::run(params, |header, input, output| {
            self.query_gpa_visibility(header.partition_id, input, output)
        })
    }
}

/// Implements the `HvExtQueryCapabilities` hypercall.
pub trait ExtendedQueryCapabilities {
    /// Queries extended capabilities.
    fn query_extended_capabilities(&mut self) -> HvResult<u64>;
}

/// Defines the `HvExtQueryCapabilities` hypercall.
pub type HvExtQueryCapabilities =
    SimpleHypercall<(), u64, { HypercallCode::HvExtCallQueryCapabilities.0 }>;

impl<T: ExtendedQueryCapabilities> HypercallDispatch<HvExtQueryCapabilities> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvExtQueryCapabilities::run(params, |()| self.query_extended_capabilities())
    }
}

/// Defines the `HvSendSyntheticClusterIpi` hypercall.
pub type HvSendSyntheticClusterIpi = SimpleHypercall<
    defs::SendSyntheticClusterIpi,
    (),
    { HypercallCode::HvCallSendSyntheticClusterIpi.0 },
>;

/// Implements the `HvSendSyntheticClusterIpi` hypercall.
pub trait SendSyntheticClusterIpi {
    /// Sends an ipi to a synthetic cluster.
    fn send_synthetic_cluster_ipi(
        &mut self,
        target_vtl: Option<Vtl>,
        vector: u32,
        flags: u8,
        processor_set: ProcessorSet<'_>,
    ) -> HvResult<()>;
}

impl<T: SendSyntheticClusterIpi> HypercallDispatch<HvSendSyntheticClusterIpi> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvSendSyntheticClusterIpi::run(params, |input| {
            if input.reserved != 0 {
                return Err(HvError::InvalidParameter);
            }
            self.send_synthetic_cluster_ipi(
                input.target_vtl.target_vtl()?,
                input.vector,
                input.flags,
                ProcessorSet::from_processor_masks(1, &[input.processor_mask])
                    .ok_or(HvError::InvalidParameter)?,
            )
        })
    }
}

/// Defines the `HvSendSyntheticClusterIpi` hypercall.
pub type HvSendSyntheticClusterIpiEx = VariableHypercall<
    defs::SendSyntheticClusterIpiEx,
    (),
    { HypercallCode::HvCallSendSyntheticClusterIpiEx.0 },
>;

/// Implements the `HvSendSyntheticClusterIpi` hypercall.
pub trait SendSyntheticClusterIpiEx {
    /// Sends an ipi to a synthetic cluster.
    fn send_synthetic_cluster_ipi_ex(
        &mut self,
        target_vtl: Option<Vtl>,
        vector: u32,
        flags: u8,
        processor_set: ProcessorSet<'_>,
    ) -> HvResult<()>;
}

impl<T: SendSyntheticClusterIpiEx> HypercallDispatch<HvSendSyntheticClusterIpiEx> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvSendSyntheticClusterIpiEx::run(params, |header, input| {
            if header.reserved != 0 {
                return Err(HvError::InvalidParameter);
            }

            if header.vp_set_format != hvdef::hypercall::HV_GENERIC_SET_SPARSE_4K {
                return Err(HvError::InvalidParameter);
            }

            self.send_synthetic_cluster_ipi_ex(
                header.target_vtl.target_vtl()?,
                header.vector,
                header.flags,
                ProcessorSet::from_processor_masks(header.vp_set_valid_banks_mask, input)
                    .ok_or(HvError::InvalidParameter)?,
            )
        })
    }
}

/// Defines the `HvRestorePartitionTime` hypercall.
pub type HvRestorePartitionTime = SimpleHypercall<
    defs::RestorePartitionTime,
    (),
    { HypercallCode::HvCallRestorePartitionTime.0 },
>;

/// Implements the `HvRestorePartitionTime` hypercall.
pub trait RestorePartitionTime {
    /// Restores the partition time.
    fn restore_partition_time(
        &mut self,
        partition_id: u64,
        tsc_sequence: u32,
        reference_time_in_100_ns: u64,
        tsc: u64,
    ) -> HvResult<()>;
}

impl<T: RestorePartitionTime> HypercallDispatch<HvRestorePartitionTime> for T {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        HvRestorePartitionTime::run(params, |input| {
            if input.reserved != 0 {
                return Err(HvError::InvalidParameter);
            }

            self.restore_partition_time(
                input.partition_id,
                input.tsc_sequence,
                input.reference_time_in_100_ns,
                input.tsc,
            )
        })
    }
}
