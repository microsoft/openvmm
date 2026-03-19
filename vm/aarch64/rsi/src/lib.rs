// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arm CCA specific definitions, including for the Realm Service Interface (RSI).
#![allow(unsafe_code)]

// TODO: CCA: A lot of the code in this module depends on who gets to package the RSI calls.
// If OpenVMM is the one that packages the RSI calls, then this module should be
// responsible for defining the RSI calls and their parameters. If the kernel driver is the one
// that packages the RSI calls, then this module should only define the data structures used
// to communicate with the kernel driver, and the RSI calls should be defined in the kernel driver.

/// CCA memory permission index, used to set and get Stage 2 memory access permissions
/// via the RSI interface.
#[allow(missing_docs)]
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CcaMemPermIndex {
    Index0,
    Index1,
    Index2,
    Index3,
    Index4,
    Index5,
    Index6,
    Index7,
    Index8,
    Index9,
    Index10,
    Index11,
    Index12,
    Index13,
    #[default]
    Index14,
}

// TODO: CCA: Similarly to the RsiCall trait below, this bitfield representation could
// be used to model the command identifiers used for SMC calls, but only if the OpenVMM
// turns out to be the layer where we end up issuing the SMC calls from.
// use bitfield_struct::bitfield;
// #[bitfield(u64)]
// #[derive(PartialEq, Eq, Ord, PartialOrd, Hash)]
// pub struct SmcCall {
//     pub number: u16,
//     pub hint: bool,
//     #[bits(7)]
//     pub mbz: u8,
//     #[bits(6)]
//     pub service: u8,
//     pub smc64: bool,
//     pub fast: bool,
//     _pad: u32,
// }

// // TODO: CCA: Add missing commands
// use open_enum::open_enum;
// open_enum! {
//     pub enum RsiCommand: SmcCall {
//         RSI_VERSION = SmcCall(0xC400_0190),
//         FEATURES = SmcCall(0xC400_0191),
//         REALM_CONFIG = SmcCall(0xC400_0196),
//         IPA_STATE_SET = SmcCall(0xC400_0197),
//         IPA_STATE_GET = SmcCall(0xC400_0198),
//         HOST_CALL = SmcCall(0xC400_0199),
//         MEM_GET_PERM_VALUE = SmcCall(0xC400_01A0),
//         MEM_SET_PERM_INDEX = SmcCall(0xC400_01A1),
//         MEM_SET_PERM_VALUE = SmcCall(0xC400_01A2),
//         PLANE_ENTER = SmcCall(0xC400_01A3),
//         PLANE_SYSREG_READ = SmcCall(0xC400_01AE),
//         PLANE_SYSREG_WRITE = SmcCall(0xC400_01AF),
//     }
// }

// TODO: CCA: Same as above :)
// open_enum! {
//     pub enum RsiReturnCode: i32 {
//         SUCCESS = 0,
//         ERROR_INPUT = -1,
//         ERROR_STATE = -2,
//         INCOMPLETE = -3,
//         ERROR_UNKNOWN = -4,
//         ERROR_DEVICE = -5,
//     }
// }

// TODO: CCA: Not sure if this approach would be better. There are two possible ways to implement RSI calls:
// 1. Use a trait that defines the RSI call interface, which can be implemented by different types (e.g., a mock for testing). This is what TDX does IIUC.
// 2. Keep the ioctl interface as the basis for what OpenVMM sees, and implement the RSI calls directly in the kernel driver.
// I started with (1), but moved to (2) because it seems more straightforward for the TMK use case. Also, it was annoying to return Rust-native types from the
// functions below because it created a circular dependency with the `hcl` crate.
// /// Trait to perform RSI calls used by this module.
// pub trait RsiCall {
//     /// Perform a RSI call instruction with the specified inputs.
//     fn rsi_call(&self, input: RsiInput) -> RsiOutput;
// }

// #[derive(Debug)]
// pub struct RsiInput {
//     pub command: RsiCommand,
//     pub regs: [u64; 17],
// }

// #[derive(Debug)]
// pub struct RsiOutput {
//     pub return_code: RsiReturnCode,
//     pub regs: [u64; 17],
// }

// fn rsi_version(call: &impl RsiCall) -> Result<(u64, u64), RsiReturnCode> {
//     let input = RsiInput {
//         command: RsiCommand::RSI_VERSION,
//         regs: [0; 17],
//     };

//     let output = call.rsi_call(input);

//     assert_eq!(
//         output.return_code,
//         RsiReturnCode::SUCCESS,
//         "unexpected nonzero return code {:?} returned by RSI_VERSION call",
//         output.return_code
//     );

//     Ok((output.regs[0], output.regs[1]))
// }

// pub fn rsi_realm_config(call: &impl RsiCall, realm_config_addr: u64) -> Result<(), RsiReturnCode> {
//     let mut input = RsiInput {
//         command: RsiCommand::REALM_CONFIG,
//         regs: [0; 17],
//     };
//     input.regs[0] = realm_config_addr;

//     let output = call.rsi_call(input);

//     assert_eq!(
//         output.return_code,
//         RsiReturnCode::SUCCESS,
//         "unexpected nonzero return code {:?} returned by RSI_VERSION call",
//         output.return_code
//     );

//     // TODO: CCA: this is annoying. this level can't produce Rust-native results
//     // because it doesn't have access to the low-level type.
//     Ok(())
// }

/// Read the CNTFRQ_EL0 system register, which contains the frequency of the
/// system timer in Hz. This is used to determine the frequency of the
/// system timer for the current execution level (EL0).
#[inline]
pub fn read_cntfrq_el0() -> u64 {
    let freq: u64;
    // SAFETY: no safety requirements, just reading an EL0 sysreg
    unsafe {
        core::arch::asm!(
            "mrs {cntfrq}, cntfrq_el0",
            cntfrq = out(reg) freq,
            options(nomem, nostack, preserves_flags)
        );
    };
    freq
}
