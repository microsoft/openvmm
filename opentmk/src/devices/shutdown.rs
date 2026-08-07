// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ACPI S5 (soft-off) shutdown via the PM1 Control Register or HW-reduced
//! sleep control register.
//!
//! After `exit_boot_services()`, UEFI runtime services are unavailable for
//! shutdown. This module writes directly to the appropriate ACPI register
//! to trigger an S5 power-off.
//!
//! Two mechanisms are supported:
//!
//! - **Legacy ACPI**: Write `(SLP_TYP << 10) | SLP_EN` to the PM1a/PM1b
//!   control block I/O ports from the FADT.
//! - **HW-reduced ACPI** (e.g. Hyper-V Gen2): Write `(SLP_TYP << 2) | SLP_EN`
//!   to the `sleep_control_reg` from the FADT, which may be an I/O port or
//!   an MMIO address.

use crate::tmkdefs::AcpiError;
use crate::tmkdefs::TmkError;
use crate::tmkdefs::TmkResult;

// --- Legacy PM1 constants ---

/// PM1 Control Register: SLP_EN (sleep enable) bit 13.
#[cfg(target_os = "uefi")]
const PM1_CNT_SLP_EN: u16 = 1 << 13;

/// PM1 Control Register: SLP_TYP shift bits 10..12.
#[cfg(target_os = "uefi")]
const PM1_CNT_SLP_TYP_SHIFT: u16 = 10;

// --- HW-reduced sleep control register constants ---

/// HW-reduced sleep control register: SLP_EN bit 5.
#[cfg(target_os = "uefi")]
const HW_REDUCED_SLP_EN: u8 = 1 << 5;

/// HW-reduced sleep control register: SLP_TYP shift bits 2..4.
#[cfg(target_os = "uefi")]
const HW_REDUCED_SLP_TYP_SHIFT: u8 = 2;

/// Well-known SLP_TYP values for S5 (soft-off) across different
/// platforms. The correct value is defined in the DSDT `\_S5` AML
/// object, which we cannot parse in a no_std baremetal environment.
/// We try these values in order until one triggers a shutdown.
///
/// - `0`: Hyper-V Gen2
/// - `5`: QEMU, PIIX4/ICH chipsets
/// - `7`: Some OEM firmware
#[cfg(target_os = "uefi")]
const SLP_TYP_S5_CANDIDATES: &[u16] = &[0, 5, 7];

/// Number of spin iterations to wait after a shutdown write before
/// concluding it did not take effect. Each iteration executes a PAUSE
/// instruction (~140 cycles on modern x86). At 3 GHz this yields
/// roughly 200-250 ms per attempt -- long enough for any hypervisor
/// to process a synchronous shutdown, yet short enough to retry
/// alternate SLP_TYP values in under a second total.
#[cfg(target_os = "uefi")]
const SHUTDOWN_SPIN_WAIT_ITERS: u32 = 5_000_000;

/// Trigger an ACPI S5 shutdown by writing to the appropriate ACPI
/// register (legacy PM1 or HW-reduced sleep control).
///
/// Automatically detects the platform mechanism from the FADT and
/// tries well-known SLP_TYP values for S5 since the actual value
/// requires AML interpretation.
///
/// On success, the VM powers off and this function never returns.
/// If all SLP_TYP candidates are exhausted without effect, enters
/// an infinite spin loop as a last resort.
///
/// # Errors
///
/// Returns `TmkError::AcpiError(AcpiError::ShutdownFailed)` if the shutdown parameters
/// cannot be retrieved from the FADT (non-UEFI targets always error).
#[cfg(target_os = "uefi")]
pub fn shutdown() -> TmkResult<()> {
    use crate::uefi::acpi_wrap::AcpiSleepMechanism;
    use crate::uefi::acpi_wrap::AcpiTableContext;

    let mechanism = AcpiTableContext::get_sleep_mechanism().map_err(|e| {
        log::error!("Failed to get ACPI sleep mechanism: {:?}", e);
        TmkError::AcpiError(AcpiError::ShutdownFailed)
    })?;

    // WARNING: Trying multiple SLP_TYP values is inherently racy -- a
    // previous write may have initiated platform shutdown logic that has
    // not yet completed. The spin wait between attempts is sized to give
    // the platform enough time to act on each write before we move on.
    for &slp_typ in SLP_TYP_S5_CANDIDATES {
        log::info!("Attempting ACPI shutdown with SLP_TYP={}", slp_typ);
        match &mechanism {
            AcpiSleepMechanism::Legacy {
                pm1a_cnt_blk,
                pm1b_cnt_blk,
            } => shutdown_legacy(slp_typ, *pm1a_cnt_blk, *pm1b_cnt_blk),
            AcpiSleepMechanism::HwReduced { sleep_reg } => shutdown_hw_reduced(slp_typ, sleep_reg),
        }
        // If we reach here, the shutdown write did not take effect.
        // Try the next SLP_TYP candidate.
    }

    // None of the candidates worked.
    log::error!("All SLP_TYP candidates exhausted, ACPI shutdown failed");
    loop {
        core::hint::spin_loop();
    }
}

/// Write the legacy PM1 shutdown value and wait briefly for it to
/// take effect. Returns so the caller can try the next SLP_TYP
/// candidate if the write did not trigger a power-off.
#[cfg(all(target_arch = "x86_64", target_os = "uefi"))] // xtask-fmt allow-target-arch sys-crate
fn shutdown_legacy(slp_typ: u16, pm1a_port: u16, pm1b_port: u16) {
    use crate::arch::io::outw;

    let value = (slp_typ << PM1_CNT_SLP_TYP_SHIFT) | PM1_CNT_SLP_EN;

    log::info!(
        "ACPI legacy shutdown: writing 0x{:04x} to PM1a port 0x{:x}",
        value,
        pm1a_port
    );
    outw(pm1a_port, value);

    if pm1b_port != 0 {
        log::info!(
            "ACPI legacy shutdown: writing 0x{:04x} to PM1b port 0x{:x}",
            value,
            pm1b_port
        );
        outw(pm1b_port, value);
    }

    // Brief delay to let the hypervisor process the shutdown request.
    for _ in 0..SHUTDOWN_SPIN_WAIT_ITERS {
        core::hint::spin_loop();
    }
}

/// Legacy ACPI shutdown is not available on aarch64.
#[cfg(all(target_arch = "aarch64", target_os = "uefi"))] // xtask-fmt allow-target-arch sys-crate
fn shutdown_legacy(_slp_typ: u16, _pm1a: u16, _pm1b: u16) {
    log::error!("Legacy PM1 shutdown is not supported on aarch64");
}

/// HW-reduced ACPI shutdown via the sleep control register.
///
/// The register may be in SystemIO or SystemMemory address space.
///
/// On Hyper-V Gen2 x64, the `sleep_control_reg` points at the high
/// byte of the PM1 control word (PM1_CNT + 1 = 0x405). The hypervisor
/// only responds to a **word-width** write at the aligned PM1_CNT base
/// address (0x404), so we try a word write at `port & !1` first, then
/// fall back to a byte write at the exact register address.
#[cfg(target_os = "uefi")]
fn shutdown_hw_reduced(slp_typ: u16, sleep_reg: &acpi_spec::fadt::GenericAddress) {
    use acpi_spec::fadt::AddressSpaceId;
    use acpi_spec::fadt::GenericAddress;

    let byte_value = ((slp_typ as u8) << HW_REDUCED_SLP_TYP_SHIFT) | HW_REDUCED_SLP_EN;
    let word_value = (slp_typ << PM1_CNT_SLP_TYP_SHIFT) | PM1_CNT_SLP_EN;
    // Copy from packed struct to avoid unaligned reference.
    let reg_addr = sleep_reg.address;
    // Read the raw address space byte to avoid UB from constructing an
    // AddressSpaceId enum with an invalid discriminant (firmware data is
    // untrusted). AddressSpaceId is #[repr(u8)] so we read the first
    // byte of the packed GenericAddress struct directly.
    //
    // SAFETY: sleep_reg points to a valid GenericAddress in FADT memory.
    // We read a single u8 (the addr_space_id field at offset 0), which
    // has no alignment requirements.
    let addr_space_raw: u8 =
        unsafe { core::ptr::read_unaligned(sleep_reg as *const GenericAddress as *const u8) };

    const ADDR_SPACE_SYSTEM_MEMORY: u8 = AddressSpaceId::SystemMemory as u8;
    const ADDR_SPACE_SYSTEM_IO: u8 = AddressSpaceId::SystemIo as u8;

    match addr_space_raw {
        ADDR_SPACE_SYSTEM_IO => {
            #[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch sys-crate
            {
                let port = reg_addr as u16;

                // Hyper-V workaround: the sleep_control_reg often points one
                // byte into PM1_CNT (e.g. 0x405 when PM1_CNT is at 0x404).
                // The hypervisor only handles word-width writes at the aligned
                // base, so mask off the low bit and do a 16-bit outw.
                let pm1_cnt_port = port & !1;
                log::info!(
                    "ACPI HW-reduced shutdown: writing word 0x{:04x} to PM1_CNT port 0x{:x}",
                    word_value,
                    pm1_cnt_port
                );
                crate::arch::io::outw(pm1_cnt_port, word_value);

                // Fall back to a byte write at the exact register address
                // for platforms that handle byte-granularity access.
                log::info!(
                    "ACPI HW-reduced shutdown: writing byte 0x{:02x} to I/O port 0x{:x}",
                    byte_value,
                    port
                );
                crate::arch::io::outb(port, byte_value);
            }
            #[cfg(not(target_arch = "x86_64"))] // xtask-fmt allow-target-arch sys-crate
            {
                log::error!("SystemIO sleep control register not supported on this arch");
            }
        }
        ADDR_SPACE_SYSTEM_MEMORY => {
            let addr = reg_addr as usize;
            if addr < 0x1000 {
                log::error!(
                    "ACPI HW-reduced shutdown: rejecting suspicious MMIO addr 0x{:x}",
                    addr
                );
                return;
            }
            // Try a word-width write first (matches legacy PM1_CNT format)
            // for platforms where the MMIO register requires word access.
            let aligned_addr = addr & !1;
            log::info!(
                "ACPI HW-reduced shutdown: writing word 0x{:04x} to MMIO addr 0x{:x} (byte-wise)",
                word_value,
                aligned_addr
            );
            // SAFETY: The sleep_control_reg address comes from the FADT, which
            // resides in EfiACPIReclaimMemory. ACPI control registers are in
            // EfiMemoryMappedIO regions that remain mapped after
            // exit_boot_services per the UEFI specification. The aligned
            // address stays within the same register word. We use byte-wise
            // writes to avoid alignment faults on architectures that require
            // naturally aligned memory access (e.g. aarch64).
            let word_bytes = word_value.to_le_bytes();
            unsafe {
                core::ptr::write_volatile(aligned_addr as *mut u8, word_bytes[0]);
                core::ptr::write_volatile((aligned_addr + 1) as *mut u8, word_bytes[1]);
            }

            // Also try a byte write at the exact address.
            log::info!(
                "ACPI HW-reduced shutdown: writing byte 0x{:02x} to MMIO addr 0x{:x}",
                byte_value,
                addr
            );
            // SAFETY: Same as above -- the address is a valid ACPI control
            // register that remains mapped after exit_boot_services.
            unsafe {
                core::ptr::write_volatile(addr as *mut u8, byte_value);
            }
        }
        other => {
            log::error!(
                "Unsupported sleep_control_reg address space: 0x{:02x}",
                other
            );
        }
    }

    // Brief delay to let the hypervisor process the shutdown request.
    for _ in 0..SHUTDOWN_SPIN_WAIT_ITERS {
        core::hint::spin_loop();
    }
}

/// Stub for non-UEFI targets.
#[cfg(not(target_os = "uefi"))]
pub fn shutdown() -> TmkResult<()> {
    log::error!("ACPI shutdown is only supported in UEFI environments");
    Err(TmkError::AcpiError(AcpiError::ShutdownFailed))
}
