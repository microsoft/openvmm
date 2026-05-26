// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runtime parser for the measured [`ContainerPolicy`] payload.
//!
//! The payload follows [`ParavisorMeasuredVtl2Config`] in the same
//! measured config region; `container_policy_size == 0` means absent.
//!
//! [`ParavisorMeasuredVtl2Config`]: loader_defs::paravisor::ParavisorMeasuredVtl2Config

use loader_defs::paravisor::ContainerPolicy;
use loader_defs::paravisor::decode_container_policy_page;

/// Decode the body selected by
/// `ParavisorMeasuredVtl2Config::container_policy_size`. Callers must
/// pass exactly the policy bytes (no padding) and treat a zero size as
/// absent before calling.
pub fn read_container_policy(bytes: &[u8]) -> anyhow::Result<ContainerPolicy> {
    decode_container_policy_page(bytes)
        .map_err(|e| anyhow::Error::new(e).context("container policy decode"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loader_defs::paravisor::CwcowPolicy;
    use loader_defs::paravisor::encode_container_policy_page;

    fn sample_cwcow() -> ContainerPolicy {
        ContainerPolicy::Cwcow(CwcowPolicy {
            vmgs_read_only: true,
            require_secure_boot: true,
            require_secure_boot_vars: true,
            require_bcd_integrity: true,
            require_secure_avic: false,
            custom_uefi_json: vec![1, 2, 3, 4, 5],
        })
    }

    #[test]
    fn known_cwcow_variant_round_trips() {
        let policy = sample_cwcow();
        let bytes = encode_container_policy_page(&policy);
        let decoded = read_container_policy(&bytes).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn truncated_buffer_is_rejected() {
        let policy = sample_cwcow();
        let mut bytes = encode_container_policy_page(&policy);
        bytes.pop();
        assert!(read_container_policy(&bytes).is_err());
    }

    #[test]
    fn empty_buffer_is_rejected() {
        // Callers must check `container_policy_size != 0` before
        // calling; if they don't, the decode must still error.
        assert!(read_container_policy(&[]).is_err());
    }
}
