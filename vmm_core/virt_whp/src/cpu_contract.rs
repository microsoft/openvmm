// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPUID policy that keeps unsupported host enlightenments from reaching the
//! guest.

/// The L0 pinning requirement describes memory owned by OpenVMM, not memory
/// owned by OpenVMM's guest. OpenVMM does not expose the GPA pin/unpin
/// hypercalls, so this leaf keeps the enlightenment from being passed through
/// to the guest.
pub(crate) fn mask_gpa_pinning_enlightenment() -> virt::CpuidLeaf {
    let mask = hvdef::HvEnlightenmentInformation::new()
        .with_use_gpa_pinning_hypercall(true)
        .into_bits();
    virt::CpuidLeaf::new(
        hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION,
        [0; 4],
    )
    .masked([
        mask as u32,
        (mask >> 32) as u32,
        (mask >> 64) as u32,
        (mask >> 96) as u32,
    ])
}

#[cfg(test)]
mod tests {
    use super::mask_gpa_pinning_enlightenment;

    #[test]
    fn l0_gpa_pinning_enlightenment_is_not_exposed() {
        let enlightenment = hvdef::HvEnlightenmentInformation::new()
            .with_use_gpa_pinning_hypercall(true)
            .with_nested(true);
        let bits = enlightenment.into_bits();
        let mut result = [
            bits as u32,
            (bits >> 32) as u32,
            (bits >> 64) as u32,
            (bits >> 96) as u32,
        ];

        mask_gpa_pinning_enlightenment().apply(&mut result);

        let result = hvdef::HvEnlightenmentInformation::from(
            result[0] as u128
                | (result[1] as u128) << 32
                | (result[2] as u128) << 64
                | (result[3] as u128) << 96,
        );
        assert!(!result.use_gpa_pinning_hypercall());
        assert!(result.nested());
    }
}
