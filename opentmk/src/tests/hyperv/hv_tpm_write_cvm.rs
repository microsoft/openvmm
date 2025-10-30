// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ops::Range;

use hvdef::Vtl;

use crate::arch::tpm::Tpm;
use crate::context::InterruptPlatformTrait;
use crate::context::SecureInterceptPlatformTrait;
use crate::context::VirtualProcessorPlatformTrait;
use crate::context::VpExecToken;
use crate::context::VtlPlatformTrait;
use crate::devices::tpm::{TpmDevice, TpmUtil};
use crate::tmk_assert;

/// Executes a series of tests to validate TPM write violation in a Hyper-V environment.
pub fn exec<T>(ctx: &mut T)
where
    T: InterruptPlatformTrait
        + SecureInterceptPlatformTrait
        + VtlPlatformTrait
        + VirtualProcessorPlatformTrait<T>,
{
    let mut _tpm = Tpm::new();
    let protocol_version = Tpm::get_tcg_protocol_version();
    log::warn!("TPM protocol version: 0x{:x}", protocol_version);
    let tpm_gpa: u32 = Tpm::get_mapped_shared_memory();
    log::warn!("TPM CMD buffer from vTPM Device: 0x{:x}", tpm_gpa);
    let tpm_ptr = (tpm_gpa as u64) as *mut u8;

    // build slice from pointer
    // SAFETY: we trust the address set by UEFI is valid
    let tpm_command = unsafe { core::slice::from_raw_parts_mut(tpm_ptr, 4096) };
    // SAFETY: we trust the address set by UEFI is valid
    let tpm_response = unsafe { core::slice::from_raw_parts_mut(tpm_ptr.add(4096), 4096) };

    _tpm.set_command_buffer(tpm_command);
    _tpm.set_response_buffer(tpm_response);

    let result = TpmUtil::exec_self_test(&mut _tpm);

    log::warn!("TPM self test result: {:?}", result);
    tmk_assert!(result.is_ok(), "TPM self test is successful");

    let vp_count = ctx.get_vp_count();
    tmk_assert!(vp_count.is_ok(), "get_vp_count should succeed");
    let vp_count = vp_count.unwrap();
    tmk_assert!(vp_count == 4, "vp count should be 4");
    let r = ctx.setup_interrupt_handler();
    tmk_assert!(r.is_ok(), "setup_interrupt_handler should succeed");
    log::info!("set intercept handler successfully!");
    let r = ctx.setup_partition_vtl(Vtl::Vtl1);
    tmk_assert!(r.is_ok(), "setup_partition_vtl should succeed");

    let response_rage = Range {
        start: tpm_gpa as u64 + 4096,
        end: tpm_gpa as u64 + 4096 * 2,
    };

    let _r = ctx.start_on_vp(VpExecToken::new(0, Vtl::Vtl1).command(move |ctx: &mut T| {
        log::info!("successfully started running VTL1 on vp0.");
        let r = ctx.setup_secure_intercept(0x30);
        tmk_assert!(r.is_ok(), "setup_secure_intercept should succeed");

        let r = ctx.setup_vtl_protection();
        tmk_assert!(r.is_ok(), "setup_vtl_protection should succeed");

        log::info!("enabled vtl protections for the partition.");

        let r = ctx.apply_vtl_protection_for_memory(response_rage, Vtl::Vtl1);
        tmk_assert!(r.is_ok(), "apply_vtl_protection_for_memory should succeed");

        log::info!("moving to vtl0 to attempt to read the heap memory");

        ctx.switch_to_low_vtl();
    }));

    let r = ctx.set_interrupt_idx(18, |_ctx| {
        log::warn!("successfully intercepted interrupt 18");
        panic!("MC should cause a system abort");
    });
    tmk_assert!(r.is_ok(), "set_interrupt_idx should succeed");

    let cmd = TpmUtil::get_self_test_cmd();

    _tpm.copy_to_command_buffer(&cmd);
    log::warn!("TPM self test command copied to buffer");
    log::warn!("about to execute TPM self test command..");
    Tpm::execute_command_no_check();
    log::warn!("TPM self test command executed");
}
