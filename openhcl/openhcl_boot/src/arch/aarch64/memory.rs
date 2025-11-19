// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Setting up memory

use crate::AddressSpaceManager;
use crate::PartitionInfo;
use crate::ShimParams;
use crate::hvcall;
use aarch64defs::IntermPhysAddrSize;

pub fn setup_vtl2_memory(
    _shim_params: &ShimParams,
    _partition_info: &PartitionInfo,
    _address_space: &mut AddressSpaceManager,
) {
    // TODO: memory acceptance isn't currently supported in the boot shim for aarch64.
    let _ = _shim_params.bounce_buffer;

    // Enable VTL protection so that vtl 2 protections can be applied. All other config
    // should be set by the user mode
    let vsm_config = hvdef::HvRegisterVsmPartitionConfig::new()
        .with_default_vtl_protection_mask(0xF)
        .with_enable_vtl_protection(true);

    hvcall()
        .set_register(
            hvdef::HvArm64RegisterName::VsmPartitionConfig.into(),
            hvdef::HvRegisterValue::from(u64::from(vsm_config)),
        )
        .expect("setting vsm config shouldn't fail");
}

pub fn physical_address_bits(_isolation: crate::IsolationType) -> u8 {
    let mut mmfr0: u64;
    // SAFETY: Reading a system register into u64 allocated on the stack, single-threaded.
    unsafe {
        core::arch::asm!("mrs {0}, ID_AA64MMFR0_EL1", out(reg) mmfr0);
    }

    let mmfr0 = aarch64defs::MmFeatures0El1::from(mmfr0);
    match mmfr0.pa_range() {
        IntermPhysAddrSize::IPA_32_BITS_4_GB => 32,
        IntermPhysAddrSize::IPA_36_BITS_64_GB => 36,
        IntermPhysAddrSize::IPA_40_BITS_1_TB => 40,
        IntermPhysAddrSize::IPA_42_BITS_4_TB => 42,
        IntermPhysAddrSize::IPA_44_BITS_16_TB => 44,
        IntermPhysAddrSize::IPA_48_BITS_256_TB => 48,
        IntermPhysAddrSize::IPA_52_BITS_4_PB => 52,
        IntermPhysAddrSize::IPA_56_BITS_64_PB => 56,
        _ => 32,
    }
}

pub fn verify_imported_regions_hash(_shim_params: &ShimParams) {
    // TODO: memory acceptance isn't currently supported in the boot shim for aarch64, which means
    // hashing is unsupported as well.
}
