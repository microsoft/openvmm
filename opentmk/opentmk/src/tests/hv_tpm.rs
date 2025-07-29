use alloc::string::String;
use core::{alloc::Layout, ops::Range};

use ::alloc::alloc::alloc;
use hvdef::{HvX64RegisterName, Vtl};
use iced_x86::{DecoderOptions, Formatter, NasmFormatter};

use crate::{
    arch::tpm::Tpm,
    context::{
        InterruptPlatformTrait, SecureInterceptPlatformTrait, VirtualProcessorPlatformTrait,
        VpExecutor, VtlPlatformTrait,
    },
    platform::hypvctx::HvTestCtx,
    tmk_assert,
};

pub fn read_assembly_output(target: u64) -> usize {
    unsafe {
        let target_ptr = target as *const u8;
        let code_bytes = core::slice::from_raw_parts(target_ptr, 0x100);
        let mut decoder = iced_x86::Decoder::with_ip(64, code_bytes, target, DecoderOptions::NONE);

        let mut formatter = NasmFormatter::new();
        let mut output = String::new();
        let mut first_ip_len = 0;
        let mut set = false;
        while decoder.can_decode() {
            let instr = decoder.decode();
            if !set {
                first_ip_len = instr.len();
                set = true;
            }
            formatter.format(&instr, &mut output);
            log::info!("{}:{}", instr.ip(), output);
            output.clear();
        }

        first_ip_len
    }
}

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
    // SAFETY: asuming that memory range is limited to 4GB (addressable by 32-bit)
    let tpm_layout = Layout::from_size_align(4096 * 2, 4096);
    tmk_assert!(tpm_layout.is_ok(), "TPM layout is allocated as expected");
    let tpm_layout = tpm_layout.unwrap();
    let tpm_ptr = unsafe { alloc(tpm_layout) };

    let tpm_gpa = tpm_ptr as u64;
    tmk_assert!(
        tpm_gpa >> 32 == 0,
        "TPM layout is allocated in the first 4GB"
    );

    let tpm_gpa = tpm_gpa as u32;

    let set_tpm_gpa = Tpm::map_shared_memory(tpm_gpa);
    tmk_assert!(
        set_tpm_gpa == tpm_gpa,
        format!(
            "TPM layout is mapped as expected, tpm_gpa: 0x{:x}, set_tpm_gpa: 0x{:x}",
            tpm_gpa, set_tpm_gpa
        )
    );

    // build slice from pointer
    let tpm_command = unsafe { core::slice::from_raw_parts_mut(tpm_ptr, 4096) };
    let tpm_response = unsafe { core::slice::from_raw_parts_mut(tpm_ptr.add(4096), 4096) };

    _tpm.set_command_buffer(tpm_command);
    _tpm.set_response_buffer(tpm_response);

    let result = _tpm.self_test();

    log::warn!("TPM self test result: {:?}", result);
    tmk_assert!(result.is_ok(), "TPM self test is successful");

    let vp_count = ctx.get_vp_count();
    tmk_assert!(vp_count.is_ok(), "get_vp_count should succeed");
    let vp_count = vp_count.unwrap();
    tmk_assert!(vp_count == 4, "vp count should be 8");
    let r = ctx.setup_interrupt_handler();
    tmk_assert!(r.is_ok(), "setup_interrupt_handler should succeed");
    log::info!("set intercept handler successfully!");
    let r = ctx.setup_partition_vtl(Vtl::Vtl1);
    tmk_assert!(r.is_ok(), "setup_partition_vtl should succeed");
    
    let response_rage = Range {
        start: tpm_gpa as u64 + 4096,
        end: tpm_gpa as u64 + 4096 * 2,
    };

    let r= ctx.start_on_vp(VpExecutor::new(0, Vtl::Vtl1).command(move |ctx: &mut T| {
        log::info!("successfully started running VTL1 on vp0.");
        let r = ctx.setup_secure_intercept(0x30);
        tmk_assert!(r.is_ok(), "setup_secure_intercept should succeed");

        let r = ctx.set_interrupt_idx(0x30, move || {
            log::info!("interrupt fired!");
            let mut hv = HvTestCtx::new();
            // expected to get interrupt in VTL1.
            // CVMs dont support hypercalls to get the current VTL from VTL1/0.
            hv.init(Vtl::Vtl1);
            log::info!(
                "current vp from interrupt: {}",
                hv.get_current_vp().unwrap()
            );

            let rip = hvdef::HvX64RegisterName::Rip.0;

            let reg = hv.get_vp_state_with_vtl(rip, Vtl::Vtl0);
            tmk_assert!(reg.is_ok(), "get_vp_state_with_vtl should succeed");

            let reg = reg.unwrap();
            log::info!("rip from vtl0: 0x{:x}", reg);

            log::info!("pring assembly for the current RIP:");
            let size = read_assembly_output(reg);

            let new_rip_value = reg + size as u64;

            log::info!("pring assembly for the updated RIP:");
            read_assembly_output(new_rip_value);

            let r = hv.set_vp_state_with_vtl(HvX64RegisterName::Rip.0, new_rip_value, Vtl::Vtl0);
            tmk_assert!(r.is_ok(), "set_vp_state_with_vtl should succeed");

            let reg = hv.get_vp_state_with_vtl(rip, Vtl::Vtl0);
            tmk_assert!(reg.is_ok(), "get_vp_state_with_vtl should succeed");

            let reg = reg.unwrap();
            log::info!("rip from vtl0 after modification: 0x{:x}", reg);
            tmk_assert!(reg == new_rip_value, "rip should be modified");

            log::info!("pring assembly for the updated RIP after fetch:");
            read_assembly_output(reg);

            log::info!("interrupt handled!");
            hv.print_rbp();
        });
        tmk_assert!(r.is_ok(), "set_interrupt_idx should succeed");

        let r= ctx.setup_vtl_protection();
        tmk_assert!(r.is_ok(), "setup_vtl_protection should succeed");

        log::info!("enabled vtl protections for the partition.");

        let r = ctx.apply_vtl_protection_for_memory(response_rage, Vtl::Vtl1);
        tmk_assert!(r.is_ok(), "apply_vtl_protection_for_memory should succeed");
        
        log::info!("moving to vtl0 to attempt to read the heap memory");

        ctx.switch_to_low_vtl();
    }));

    loop {}
}
