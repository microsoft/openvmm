// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_arch = "x86_64")]

use super::bitops;
use super::bitops_base;
use super::WHV_CPUID_OUTPUT;
use super::WHV_MEMORY_ACCESS_TYPE;
use super::WHV_PROCESSOR_FEATURES;
use super::WHV_PROCESSOR_FEATURES1;
use super::WHV_REGISTER_NAME;
use super::WHV_RUN_VP_EXIT_REASON;
use super::WHV_UINT128;

pub const WHvX64RegisterRax: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000000);
pub const WHvX64RegisterRcx: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000001);
pub const WHvX64RegisterRdx: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000002);
pub const WHvX64RegisterRbx: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000003);
pub const WHvX64RegisterRsp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000004);
pub const WHvX64RegisterRbp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000005);
pub const WHvX64RegisterRsi: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000006);
pub const WHvX64RegisterRdi: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000007);
pub const WHvX64RegisterR8: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000008);
pub const WHvX64RegisterR9: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000009);
pub const WHvX64RegisterR10: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000000A);
pub const WHvX64RegisterR11: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000000B);
pub const WHvX64RegisterR12: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000000C);
pub const WHvX64RegisterR13: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000000D);
pub const WHvX64RegisterR14: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000000E);
pub const WHvX64RegisterR15: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000000F);
pub const WHvX64RegisterRip: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000010);
pub const WHvX64RegisterRflags: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000011);
pub const WHvX64RegisterEs: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000012);
pub const WHvX64RegisterCs: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000013);
pub const WHvX64RegisterSs: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000014);
pub const WHvX64RegisterDs: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000015);
pub const WHvX64RegisterFs: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000016);
pub const WHvX64RegisterGs: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000017);
pub const WHvX64RegisterLdtr: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000018);
pub const WHvX64RegisterTr: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000019);
pub const WHvX64RegisterIdtr: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000001A);
pub const WHvX64RegisterGdtr: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000001B);
pub const WHvX64RegisterCr0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000001C);
pub const WHvX64RegisterCr2: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000001D);
pub const WHvX64RegisterCr3: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000001E);
pub const WHvX64RegisterCr4: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000001F);
pub const WHvX64RegisterCr8: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000020);
pub const WHvX64RegisterDr0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000021);
pub const WHvX64RegisterDr1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000022);
pub const WHvX64RegisterDr2: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000023);
pub const WHvX64RegisterDr3: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000024);
pub const WHvX64RegisterDr6: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000025);
pub const WHvX64RegisterDr7: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000026);
pub const WHvX64RegisterXCr0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000027);
pub const WHvX64RegisterVirtualCr0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000028);
pub const WHvX64RegisterVirtualCr3: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00000029);
pub const WHvX64RegisterVirtualCr4: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000002A);
pub const WHvX64RegisterVirtualCr8: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000002B);
pub const WHvX64RegisterXmm0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001000);
pub const WHvX64RegisterXmm1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001001);
pub const WHvX64RegisterXmm2: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001002);
pub const WHvX64RegisterXmm3: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001003);
pub const WHvX64RegisterXmm4: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001004);
pub const WHvX64RegisterXmm5: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001005);
pub const WHvX64RegisterXmm6: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001006);
pub const WHvX64RegisterXmm7: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001007);
pub const WHvX64RegisterXmm8: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001008);
pub const WHvX64RegisterXmm9: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001009);
pub const WHvX64RegisterXmm10: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000100A);
pub const WHvX64RegisterXmm11: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000100B);
pub const WHvX64RegisterXmm12: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000100C);
pub const WHvX64RegisterXmm13: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000100D);
pub const WHvX64RegisterXmm14: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000100E);
pub const WHvX64RegisterXmm15: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000100F);
pub const WHvX64RegisterFpMmx0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001010);
pub const WHvX64RegisterFpMmx1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001011);
pub const WHvX64RegisterFpMmx2: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001012);
pub const WHvX64RegisterFpMmx3: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001013);
pub const WHvX64RegisterFpMmx4: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001014);
pub const WHvX64RegisterFpMmx5: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001015);
pub const WHvX64RegisterFpMmx6: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001016);
pub const WHvX64RegisterFpMmx7: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001017);
pub const WHvX64RegisterFpControlStatus: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001018);
pub const WHvX64RegisterXmmControlStatus: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00001019);
pub const WHvX64RegisterTsc: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002000);
pub const WHvX64RegisterEfer: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002001);
pub const WHvX64RegisterKernelGsBase: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002002);
pub const WHvX64RegisterApicBase: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002003);
pub const WHvX64RegisterPat: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002004);
pub const WHvX64RegisterSysenterCs: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002005);
pub const WHvX64RegisterSysenterEip: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002006);
pub const WHvX64RegisterSysenterEsp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002007);
pub const WHvX64RegisterStar: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002008);
pub const WHvX64RegisterLstar: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002009);
pub const WHvX64RegisterCstar: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000200A);
pub const WHvX64RegisterSfmask: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000200B);
pub const WHvX64RegisterInitialApicId: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000200C);
pub const WHvX64RegisterMsrMtrrCap: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000200D);
pub const WHvX64RegisterMsrMtrrDefType: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000200E);
pub const WHvX64RegisterMsrMtrrPhysBase0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002010);
pub const WHvX64RegisterMsrMtrrPhysBase1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002011);
pub const WHvX64RegisterMsrMtrrPhysBase2: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002012);
pub const WHvX64RegisterMsrMtrrPhysBase3: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002013);
pub const WHvX64RegisterMsrMtrrPhysBase4: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002014);
pub const WHvX64RegisterMsrMtrrPhysBase5: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002015);
pub const WHvX64RegisterMsrMtrrPhysBase6: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002016);
pub const WHvX64RegisterMsrMtrrPhysBase7: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002017);
pub const WHvX64RegisterMsrMtrrPhysBase8: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002018);
pub const WHvX64RegisterMsrMtrrPhysBase9: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002019);
pub const WHvX64RegisterMsrMtrrPhysBaseA: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000201A);
pub const WHvX64RegisterMsrMtrrPhysBaseB: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000201B);
pub const WHvX64RegisterMsrMtrrPhysBaseC: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000201C);
pub const WHvX64RegisterMsrMtrrPhysBaseD: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000201D);
pub const WHvX64RegisterMsrMtrrPhysBaseE: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000201E);
pub const WHvX64RegisterMsrMtrrPhysBaseF: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000201F);

pub const WHvX64RegisterMsrMtrrPhysMask0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002040);
pub const WHvX64RegisterMsrMtrrPhysMask1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002041);
pub const WHvX64RegisterMsrMtrrPhysMask2: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002042);
pub const WHvX64RegisterMsrMtrrPhysMask3: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002043);
pub const WHvX64RegisterMsrMtrrPhysMask4: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002044);
pub const WHvX64RegisterMsrMtrrPhysMask5: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002045);
pub const WHvX64RegisterMsrMtrrPhysMask6: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002046);
pub const WHvX64RegisterMsrMtrrPhysMask7: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002047);
pub const WHvX64RegisterMsrMtrrPhysMask8: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002048);
pub const WHvX64RegisterMsrMtrrPhysMask9: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002049);
pub const WHvX64RegisterMsrMtrrPhysMaskA: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000204A);
pub const WHvX64RegisterMsrMtrrPhysMaskB: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000204B);
pub const WHvX64RegisterMsrMtrrPhysMaskC: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000204C);
pub const WHvX64RegisterMsrMtrrPhysMaskD: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000204D);
pub const WHvX64RegisterMsrMtrrPhysMaskE: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000204E);
pub const WHvX64RegisterMsrMtrrPhysMaskF: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000204F);

pub const WHvX64RegisterMsrMtrrFix64k00000: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002070);
pub const WHvX64RegisterMsrMtrrFix16k80000: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002071);
pub const WHvX64RegisterMsrMtrrFix16kA0000: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002072);
pub const WHvX64RegisterMsrMtrrFix4kC0000: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002073);
pub const WHvX64RegisterMsrMtrrFix4kC8000: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002074);
pub const WHvX64RegisterMsrMtrrFix4kD0000: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002075);
pub const WHvX64RegisterMsrMtrrFix4kD8000: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002076);
pub const WHvX64RegisterMsrMtrrFix4kE0000: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002077);
pub const WHvX64RegisterMsrMtrrFix4kE8000: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002078);
pub const WHvX64RegisterMsrMtrrFix4kF0000: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002079);
pub const WHvX64RegisterMsrMtrrFix4kF8000: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000207A);

pub const WHvX64RegisterTscAux: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000207B);
pub const WHvX64RegisterBndcfgs: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000207C);
pub const WHvX64RegisterMCount: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000207E);
pub const WHvX64RegisterACount: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000207F);
pub const WHvX64RegisterSpecCtrl: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002084);
pub const WHvX64RegisterPredCmd: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002085);
pub const WHvX64RegisterTscVirtualOffset: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002087);
pub const WHvX64RegisterTsxCtrl: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002088);
pub const WHvX64RegisterXss: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000208B);
pub const WHvX64RegisterUCet: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000208C);
pub const WHvX64RegisterSCet: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000208D);
pub const WHvX64RegisterSsp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000208E);
pub const WHvX64RegisterPl0Ssp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000208F);
pub const WHvX64RegisterPl1Ssp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002090);
pub const WHvX64RegisterPl2Ssp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002091);
pub const WHvX64RegisterPl3Ssp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002092);
pub const WHvX64RegisterInterruptSspTableAddr: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002093);

pub const WHvX64RegisterTscDeadline: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002095);
pub const WHvX64RegisterTscAdjust: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002096);
pub const WHvX64RegisterUnwaitControl: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002098);
pub const WHvX64RegisterXfd: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00002099);
pub const WHvX64RegisterXfdErr: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000209A);
pub const WHvX64RegisterApicId: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00003002);
pub const WHvX64RegisterApicVersion: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00003003);

pub const WHvX64RegisterHypercall: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00005001);
pub const WHvRegisterPendingInterruption: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x80000000);
pub const WHvRegisterInterruptState: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x80000001);
pub const WHvX64RegisterPendingDebugException: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x80000006);

pub const WHvRegisterSint0: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004000);
pub const WHvRegisterSint1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004001);
pub const WHvRegisterSint2: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004002);
pub const WHvRegisterSint3: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004003);
pub const WHvRegisterSint4: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004004);
pub const WHvRegisterSint5: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004005);
pub const WHvRegisterSint6: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004006);
pub const WHvRegisterSint7: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004007);
pub const WHvRegisterSint8: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004008);
pub const WHvRegisterSint9: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004009);
pub const WHvRegisterSint10: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000400A);
pub const WHvRegisterSint11: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000400B);
pub const WHvRegisterSint12: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000400C);
pub const WHvRegisterSint13: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000400D);
pub const WHvRegisterSint14: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000400E);
pub const WHvRegisterSint15: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x0000400F);
pub const WHvRegisterScontrol: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004010);
pub const WHvRegisterSversion: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004011);
pub const WHvRegisterSiefp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004012);
pub const WHvRegisterSimp: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004013);
pub const WHvRegisterEom: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00004014);
pub const WHvRegisterVpRuntime: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00005000);
pub const WHvRegisterGuestOsId: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00005002);
pub const WHvRegisterVpAssistPage: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00005013);
pub const WHvRegisterReferenceTsc: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x00005017);
pub const WHvRegisterPendingEvent: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x80000002);
pub const WHvRegisterPendingEvent1: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x80000003);
pub const WHvRegisterDeliverabilityNotifications: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x80000004);
pub const WHvRegisterInternalActivityState: WHV_REGISTER_NAME = WHV_REGISTER_NAME(0x80000005);

pub const WHvRunVpExitReasonNone: WHV_RUN_VP_EXIT_REASON = WHV_RUN_VP_EXIT_REASON(0);
pub const WHvRunVpExitReasonMemoryAccess: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x00000001);
pub const WHvRunVpExitReasonX64IoPortAccess: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x00000002);
pub const WHvRunVpExitReasonUnrecoverableException: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x00000004);
pub const WHvRunVpExitReasonInvalidVpRegisterValue: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x00000005);
pub const WHvRunVpExitReasonUnsupportedFeature: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x00000006);
pub const WHvRunVpExitReasonX64InterruptWindow: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x00000007);
pub const WHvRunVpExitReasonX64Halt: WHV_RUN_VP_EXIT_REASON = WHV_RUN_VP_EXIT_REASON(0x00000008);
pub const WHvRunVpExitReasonX64ApicEoi: WHV_RUN_VP_EXIT_REASON = WHV_RUN_VP_EXIT_REASON(0x00000009);
pub const WHvRunVpExitReasonSynicSintDeliverable: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x0000000A);
pub const WHvRunVpExitReasonX64MsrAccess: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x00001000);
pub const WHvRunVpExitReasonX64Cpuid: WHV_RUN_VP_EXIT_REASON = WHV_RUN_VP_EXIT_REASON(0x00001001);
pub const WHvRunVpExitReasonException: WHV_RUN_VP_EXIT_REASON = WHV_RUN_VP_EXIT_REASON(0x00001002);
pub const WHvRunVpExitReasonX64Rdtsc: WHV_RUN_VP_EXIT_REASON = WHV_RUN_VP_EXIT_REASON(0x00001003);
pub const WHvRunVpExitReasonX64ApicSmiTrap: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x00001004);
pub const WHvRunVpExitReasonHypercall: WHV_RUN_VP_EXIT_REASON = WHV_RUN_VP_EXIT_REASON(0x00001005);
pub const WHvRunVpExitReasonX64ApicInitSipiTrap: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x00001006);
pub const WHvRunVpExitReasonX64ApicWriteTrap: WHV_RUN_VP_EXIT_REASON =
    WHV_RUN_VP_EXIT_REASON(0x00001007);
pub const WHvRunVpExitReasonCanceled: WHV_RUN_VP_EXIT_REASON = WHV_RUN_VP_EXIT_REASON(0x00002001);

#[repr(C)]
#[derive(Copy, Clone)]
pub union WHV_RUN_VP_EXIT_CONTEXT_u {
    pub MemoryAccess: WHV_MEMORY_ACCESS_CONTEXT,
    pub CancelReason: WHV_RUN_VP_CANCELED_CONTEXT,
    pub Hypercall: WHV_HYPERCALL_CONTEXT,
    pub SynicSintDeliverable: WHV_SYNIC_SINT_DELIVERABLE_CONTEXT,
    pub IoPortAccess: WHV_X64_IO_PORT_ACCESS_CONTEXT,
    pub MsrAccess: WHV_X64_MSR_ACCESS_CONTEXT,
    pub CpuidAccess: WHV_X64_CPUID_ACCESS_CONTEXT,
    pub VpException: WHV_VP_EXCEPTION_CONTEXT,
    pub InterruptWindow: WHV_X64_INTERRUPTION_DELIVERABLE_CONTEXT,
    pub UnsupportedFeature: WHV_X64_UNSUPPORTED_FEATURE_CONTEXT,
    pub ApicEoi: WHV_X64_APIC_EOI_CONTEXT,
    pub ReadTsc: WHV_X64_RDTSC_CONTEXT,
    pub ApicSmi: WHV_X64_APIC_SMI_CONTEXT,
    pub ApicInitSipi: WHV_X64_APIC_INIT_SIPI_CONTEXT,
    pub ApicWrite: WHV_X64_APIC_WRITE_CONTEXT,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_RUN_VP_CANCELED_CONTEXT {
    pub CancelReason: u32,
}

#[derive(Debug, Copy, Clone, Default)]
pub struct WHV_X64_MSR_EXIT_BITMAP(pub u64);
bitops!(WHV_X64_MSR_EXIT_BITMAP);

impl WHV_X64_MSR_EXIT_BITMAP {
    pub const UnhandledMsrs: Self = Self(1 << 0);
    pub const TscMsrWrite: Self = Self(1 << 1);
    pub const TscMsrRead: Self = Self(1 << 2);
    pub const ApicBaseMsrWrite: Self = Self(1 << 3);
    pub const MiscEnableMsrRead: Self = Self(1 << 4);
    pub const McUpdatePatchLevelMsrRead: Self = Self(1 << 5);
}

impl WHV_PROCESSOR_FEATURES {
    pub const Sse3Support: Self = Self(1 << 0);
    pub const LahfSahfSupport: Self = Self(1 << 1);
    pub const Ssse3Support: Self = Self(1 << 2);
    pub const Sse4_1Support: Self = Self(1 << 3);
    pub const Sse4_2Support: Self = Self(1 << 4);
    pub const Sse4ASupport: Self = Self(1 << 5);
    pub const XopSupport: Self = Self(1 << 6);
    pub const PopCntSupport: Self = Self(1 << 7);
    pub const Cmpxchg16BSupport: Self = Self(1 << 8);
    pub const Altmovcr8Support: Self = Self(1 << 9);
    pub const LzcntSupport: Self = Self(1 << 10);
    pub const MisAlignSseSupport: Self = Self(1 << 11);
    pub const MmxExtSupport: Self = Self(1 << 12);
    pub const Amd3DNowSupport: Self = Self(1 << 13);
    pub const ExtendedAmd3DNowSupport: Self = Self(1 << 14);
    pub const Page1GbSupport: Self = Self(1 << 15);
    pub const AesSupport: Self = Self(1 << 16);
    pub const PclmulqdqSupport: Self = Self(1 << 17);
    pub const PcidSupport: Self = Self(1 << 18);
    pub const Fma4Support: Self = Self(1 << 19);
    pub const F16CSupport: Self = Self(1 << 20);
    pub const RdRandSupport: Self = Self(1 << 21);
    pub const RdWrFsGsSupport: Self = Self(1 << 22);
    pub const SmepSupport: Self = Self(1 << 23);
    pub const EnhancedFastStringSupport: Self = Self(1 << 24);
    pub const Bmi1Support: Self = Self(1 << 25);
    pub const Bmi2Support: Self = Self(1 << 26);

    pub const MovbeSupport: Self = Self(1 << 29);
    pub const Npiep1Support: Self = Self(1 << 30);
    pub const DepX87FpuSaveSupport: Self = Self(1 << 31);
    pub const RdSeedSupport: Self = Self(1 << 32);
    pub const AdxSupport: Self = Self(1 << 33);
    pub const IntelPrefetchSupport: Self = Self(1 << 34);
    pub const SmapSupport: Self = Self(1 << 35);
    pub const HleSupport: Self = Self(1 << 36);
    pub const RtmSupport: Self = Self(1 << 37);
    pub const RdtscpSupport: Self = Self(1 << 38);
    pub const ClflushoptSupport: Self = Self(1 << 39);
    pub const ClwbSupport: Self = Self(1 << 40);
    pub const ShaSupport: Self = Self(1 << 41);
    pub const X87PointersSavedSupport: Self = Self(1 << 42);
    pub const InvpcidSupport: Self = Self(1 << 43);
    pub const IbrsSupport: Self = Self(1 << 44);
    pub const StibpSupport: Self = Self(1 << 45);
    pub const IbpbSupport: Self = Self(1 << 46);
    pub const Reserved2: Self = Self(1 << 47);
    pub const SsbdSupport: Self = Self(1 << 48);
    pub const FastShortRepMovSupport: Self = Self(1 << 49);
    pub const Reserved3: Self = Self(1 << 50);
    pub const RdclNo: Self = Self(1 << 51);
    pub const IbrsAllSupport: Self = Self(1 << 52);
    pub const Reserved4: Self = Self(1 << 53);
    pub const SsbNo: Self = Self(1 << 54);
    pub const RsbANo: Self = Self(1 << 55);
    pub const Reserved5: Self = Self(1 << 56);
    pub const RdPidSupport: Self = Self(1 << 57);
    pub const UmipSupport: Self = Self(1 << 58);
    pub const MdsNoSupport: Self = Self(1 << 59);
    pub const MdClearSupport: Self = Self(1 << 60);
    pub const TaaNoSupport: Self = Self(1 << 61);
    pub const TsxCtrlSupport: Self = Self(1 << 62);
}

impl WHV_PROCESSOR_FEATURES1 {
    pub const ACountMCountSupport: Self = Self(1 << 0);
    pub const Reserved1: Self = Self(1 << 1);
    pub const ClZeroSupport: Self = Self(1 << 2);
    pub const RdpruSupport: Self = Self(1 << 3);

    pub const NestedVirtSupport: Self = Self(1 << 6);
    pub const PsfdSupport: Self = Self(1 << 7);
    pub const CetSsSupport: Self = Self(1 << 8);
    pub const CetIbtSupport: Self = Self(1 << 9);
    pub const VmxExceptionInjectSupport: Self = Self(1 << 10);

    pub const UmwaitTpauseSupport: Self = Self(1 << 12);
    pub const MovdiriSupport: Self = Self(1 << 13);
    pub const Movdir64bSupport: Self = Self(1 << 14);
    pub const CldemoteSupport: Self = Self(1 << 15);
    pub const SerializeSupport: Self = Self(1 << 16);
    pub const TscDeadlineTmrSupport: Self = Self(1 << 17);
    pub const TscAdjustSupport: Self = Self(1 << 18);
    pub const FzlRepMovsb: Self = Self(1 << 19);
    pub const FsRepStosb: Self = Self(1 << 20);
    pub const FsRepCmpsb: Self = Self(1 << 21);
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_PROCESSOR_PERFMON_FEATURES(pub u64);
bitops!(WHV_PROCESSOR_PERFMON_FEATURES);

impl WHV_PROCESSOR_PERFMON_FEATURES {
    pub const PmuSupport: Self = Self(1 << 0);
    pub const LbrSupport: Self = Self(1 << 1);
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct WHV_VP_EXIT_CONTEXT {
    pub ExecutionState: WHV_X64_VP_EXECUTION_STATE,
    pub InstructionLengthAndCr8: u8,
    pub Reserved: u8,
    pub Reserved2: u32,
    pub Cs: WHV_X64_SEGMENT_REGISTER,
    pub Rip: u64,
    pub Rflags: u64,
}

impl WHV_VP_EXIT_CONTEXT {
    pub fn InstructionLength(&self) -> u8 {
        self.InstructionLengthAndCr8 & 0xf
    }
    pub fn Cr8(&self) -> u8 {
        self.InstructionLengthAndCr8 >> 4
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct WHV_X64_VP_EXECUTION_STATE(pub u16);

impl WHV_X64_VP_EXECUTION_STATE {
    pub fn Cpl(self) -> u8 {
        (self.0 & 0x3) as u8
    }
    pub fn Cr0Pe(self) -> bool {
        (self.0 & 0x4) != 0
    }
    pub fn Cr0Am(self) -> bool {
        (self.0 & 0x8) != 0
    }
    pub fn EferLma(self) -> bool {
        (self.0 & 0x10) != 0
    }
    pub fn DebugActive(self) -> bool {
        (self.0 & 0x20) != 0
    }
    pub fn InterruptionPending(self) -> bool {
        (self.0 & 0x40) != 0
    }
    pub fn InterruptShadow(self) -> bool {
        (self.0 & 0x1000) != 0
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_MEMORY_ACCESS_CONTEXT {
    // Context of the virtual processor
    pub InstructionByteCount: u8,
    pub Reserved: [u8; 3],
    pub InstructionBytes: [u8; 16],

    // Memory access info
    pub AccessInfo: WHV_MEMORY_ACCESS_INFO,
    pub Gpa: u64,
    pub Gva: u64,
    #[cfg(target_arch = "aarch64")]
    pub Syndrome: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_MEMORY_ACCESS_INFO(pub u32);

impl WHV_MEMORY_ACCESS_INFO {
    pub fn AccessType(self) -> WHV_MEMORY_ACCESS_TYPE {
        WHV_MEMORY_ACCESS_TYPE(self.0 & 3)
    }

    pub fn GpaUnmapped(self) -> bool {
        (self.0 & 4) != 0
    }

    pub fn GvaValid(self) -> bool {
        (self.0 & 8) != 0
    }
}

#[repr(C, align(8))]
#[derive(Debug, Copy, Clone)]
pub struct WHV_INTERRUPT_CONTROL {
    pub Type: u8, // WHV_INTERRUPT_TYPE
    pub Modes: WHV_INTERRUPT_CONTROL_MODES,
    pub Reserved: [u8; 6],
    pub Destination: u32,
    pub Vector: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_INTERRUPT_TYPE(pub u32);

pub const WHvX64InterruptTypeFixed: WHV_INTERRUPT_TYPE = WHV_INTERRUPT_TYPE(0);
pub const WHvX64InterruptTypeLowestPriority: WHV_INTERRUPT_TYPE = WHV_INTERRUPT_TYPE(1);
pub const WHvX64InterruptTypeNmi: WHV_INTERRUPT_TYPE = WHV_INTERRUPT_TYPE(4);
pub const WHvX64InterruptTypeInit: WHV_INTERRUPT_TYPE = WHV_INTERRUPT_TYPE(5);
pub const WHvX64InterruptTypeSipi: WHV_INTERRUPT_TYPE = WHV_INTERRUPT_TYPE(6);
pub const WHvX64InterruptTypeLocalInt1: WHV_INTERRUPT_TYPE = WHV_INTERRUPT_TYPE(9);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_INTERRUPT_DESTINATION_MODE(pub u8);

pub const WHvX64InterruptDestinationModePhysical: WHV_INTERRUPT_DESTINATION_MODE =
    WHV_INTERRUPT_DESTINATION_MODE(0);
pub const WHvX64InterruptDestinationModeLogical: WHV_INTERRUPT_DESTINATION_MODE =
    WHV_INTERRUPT_DESTINATION_MODE(1);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_INTERRUPT_TRIGGER_MODE(pub u8);

pub const WHvX64InterruptTriggerModeEdge: WHV_INTERRUPT_TRIGGER_MODE =
    WHV_INTERRUPT_TRIGGER_MODE(0);
pub const WHvX64InterruptTriggerModeLevel: WHV_INTERRUPT_TRIGGER_MODE =
    WHV_INTERRUPT_TRIGGER_MODE(1);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_INTERRUPT_CONTROL_MODES(pub u8);

impl WHV_INTERRUPT_CONTROL_MODES {
    pub fn new(dest: WHV_INTERRUPT_DESTINATION_MODE, trigger: WHV_INTERRUPT_TRIGGER_MODE) -> Self {
        Self(dest.0 | trigger.0 << 4)
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_IO_PORT_ACCESS_INFO(pub u32);

impl WHV_X64_IO_PORT_ACCESS_INFO {
    pub fn IsWrite(self) -> bool {
        (self.0 & 1) != 0
    }
    pub fn AccessSize(self) -> u8 {
        ((self.0 >> 1) & 0x7) as u8
    }
    pub fn StringOp(self) -> bool {
        (self.0 & 0x10) != 0
    }
    pub fn RepPrefix(self) -> bool {
        (self.0 & 0x20) != 0
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_IO_PORT_ACCESS_CONTEXT {
    // Context of the virtual processor
    pub InstructionByteCount: u8,
    pub Reserved: [u8; 3],
    pub InstructionBytes: [u8; 16],

    pub AccessInfo: WHV_X64_IO_PORT_ACCESS_INFO,
    pub PortNumber: u16,
    pub Reserved2: [u16; 3],
    pub Rax: u64,
    pub Rcx: u64,
    pub Rsi: u64,
    pub Rdi: u64,
    pub Ds: WHV_X64_SEGMENT_REGISTER,
    pub Es: WHV_X64_SEGMENT_REGISTER,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_MSR_ACCESS_INFO(pub u32);

impl WHV_X64_MSR_ACCESS_INFO {
    pub fn IsWrite(self) -> bool {
        (self.0 & 1) != 0
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_MSR_ACCESS_CONTEXT {
    pub AccessInfo: WHV_X64_MSR_ACCESS_INFO,
    pub MsrNumber: u32,
    pub Rax: u64,
    pub Rdx: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_CPUID_ACCESS_CONTEXT {
    pub Rax: u64,
    pub Rcx: u64,
    pub Rdx: u64,
    pub Rbx: u64,
    pub DefaultResultRax: u64,
    pub DefaultResultRcx: u64,
    pub DefaultResultRdx: u64,
    pub DefaultResultRbx: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_VP_EXCEPTION_INFO(pub u32);

impl WHV_VP_EXCEPTION_INFO {
    pub fn ErrorCodeValid(self) -> bool {
        (self.0 & 1) != 0
    }
    pub fn SoftwareException(self) -> bool {
        (self.0 & 2) != 0
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct WHV_EXCEPTION_TYPE(pub u8);

pub const WHvX64ExceptionTypeDivideErrorFault: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x0);
pub const WHvX64ExceptionTypeDebugTrapOrFault: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x1);
pub const WHvX64ExceptionTypeBreakpointTrap: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x3);
pub const WHvX64ExceptionTypeOverflowTrap: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x4);
pub const WHvX64ExceptionTypeBoundRangeFault: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x5);
pub const WHvX64ExceptionTypeInvalidOpcodeFault: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x6);
pub const WHvX64ExceptionTypeDeviceNotAvailableFault: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x7);
pub const WHvX64ExceptionTypeDoubleFaultAbort: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x8);
pub const WHvX64ExceptionTypeInvalidTaskStateSegmentFault: WHV_EXCEPTION_TYPE =
    WHV_EXCEPTION_TYPE(0x0A);
pub const WHvX64ExceptionTypeSegmentNotPresentFault: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x0B);
pub const WHvX64ExceptionTypeStackFault: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x0C);
pub const WHvX64ExceptionTypeGeneralProtectionFault: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x0D);
pub const WHvX64ExceptionTypePageFault: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x0E);
pub const WHvX64ExceptionTypeFloatingPointErrorFault: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x10);
pub const WHvX64ExceptionTypeAlignmentCheckFault: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x11);
pub const WHvX64ExceptionTypeMachineCheckAbort: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x12);
pub const WHvX64ExceptionTypeSimdFloatingPointFault: WHV_EXCEPTION_TYPE = WHV_EXCEPTION_TYPE(0x13);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_VP_EXCEPTION_CONTEXT {
    pub InstructionByteCount: u8,
    pub Reserved: [u8; 3],
    pub InstructionBytes: [u8; 16],

    // Exception info
    pub ExceptionInfo: WHV_VP_EXCEPTION_INFO,
    pub ExceptionType: WHV_EXCEPTION_TYPE,
    pub Reserved2: [u8; 3],
    pub ErrorCode: u32,
    pub ExceptionParameter: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WHV_X64_UNSUPPORTED_FEATURE_CODE(pub u32);

pub const WHvUnsupportedFeatureIntercept: WHV_X64_UNSUPPORTED_FEATURE_CODE =
    WHV_X64_UNSUPPORTED_FEATURE_CODE(1);
pub const WHvUnsupportedFeatureTaskSwitchTss: WHV_X64_UNSUPPORTED_FEATURE_CODE =
    WHV_X64_UNSUPPORTED_FEATURE_CODE(2);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_UNSUPPORTED_FEATURE_CONTEXT {
    pub FeatureCode: WHV_X64_UNSUPPORTED_FEATURE_CODE,
    pub Reserved: u32,
    pub FeatureParameter: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WHV_X64_PENDING_INTERRUPTION_TYPE(pub u32);

pub const WHvX64PendingInterrupt: WHV_X64_PENDING_INTERRUPTION_TYPE =
    WHV_X64_PENDING_INTERRUPTION_TYPE(0);
pub const WHvX64PendingNmi: WHV_X64_PENDING_INTERRUPTION_TYPE =
    WHV_X64_PENDING_INTERRUPTION_TYPE(2);
pub const WHvX64PendingException: WHV_X64_PENDING_INTERRUPTION_TYPE =
    WHV_X64_PENDING_INTERRUPTION_TYPE(3);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_INTERRUPTION_DELIVERABLE_CONTEXT {
    pub DeliverableType: WHV_X64_PENDING_INTERRUPTION_TYPE,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_APIC_EOI_CONTEXT {
    pub InterruptVector: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_RDTSC_CONTEXT {
    pub TscAux: u64,
    pub VirtualOffset: u64,
    pub Tsc: u64,
    pub ReferenceTime: u64,
    pub RdtscInfo: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_APIC_SMI_CONTEXT {
    pub ApicIcr: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_APIC_INIT_SIPI_CONTEXT {
    pub ApicIcr: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_APIC_WRITE_CONTEXT {
    pub Type: u32,
    pub Reserved: u32,
    pub WriteValue: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct WHV_HYPERCALL_CONTEXT {
    pub Rax: u64,
    pub Rbx: u64,
    pub Rcx: u64,
    pub Rdx: u64,
    pub R8: u64,
    pub Rsi: u64,
    pub Rdi: u64,
    pub Reserved0: u64,
    pub XmmRegisters: [WHV_UINT128; 6],
    pub Reserved1: [u64; 2],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_CPUID_RESULT {
    pub Function: u32,
    pub Reserved: [u32; 3],
    pub Eax: u32,
    pub Ebx: u32,
    pub Ecx: u32,
    pub Edx: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct WHV_X64_LOCAL_APIC_EMULATION_MODE(pub u32);

pub const WHvX64LocalApicEmulationModeNone: WHV_X64_LOCAL_APIC_EMULATION_MODE =
    WHV_X64_LOCAL_APIC_EMULATION_MODE(0);
pub const WHvX64LocalApicEmulationModeXApic: WHV_X64_LOCAL_APIC_EMULATION_MODE =
    WHV_X64_LOCAL_APIC_EMULATION_MODE(1);
pub const WHvX64LocalApicEmulationModeX2Apic: WHV_X64_LOCAL_APIC_EMULATION_MODE =
    WHV_X64_LOCAL_APIC_EMULATION_MODE(2);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_CPUID_RESULT2_FLAGS(pub u32);
bitops!(WHV_X64_CPUID_RESULT2_FLAGS);

pub const WHvX64CpuidResult2FlagSubleafSpecific: WHV_X64_CPUID_RESULT2_FLAGS =
    WHV_X64_CPUID_RESULT2_FLAGS(0x00000001);
pub const WHvX64CpuidResult2FlagVpSpecific: WHV_X64_CPUID_RESULT2_FLAGS =
    WHV_X64_CPUID_RESULT2_FLAGS(0x00000002);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_CPUID_RESULT2 {
    Function: u32,
    Index: u32,
    VpIndex: u32,
    Flags: WHV_X64_CPUID_RESULT2_FLAGS,
    Output: WHV_CPUID_OUTPUT,
    Mask: WHV_CPUID_OUTPUT,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_X64_SEGMENT_REGISTER {
    pub Base: u64,
    pub Limit: u32,
    pub Selector: u16,
    pub Attributes: u16,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct WHV_X64_TABLE_REGISTER {
    pub Pad: [u16; 3],
    pub Limit: u16,
    pub Base: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_SYNIC_SINT_DELIVERABLE_CONTEXT {
    pub DeliverableSints: u16,
    pub Reserved1: u16,
    pub Reserved2: u32,
}
