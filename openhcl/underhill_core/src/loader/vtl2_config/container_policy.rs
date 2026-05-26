// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runtime parser for the optional measured [`ContainerPolicy`] payload.
//!
//! The wire format and the runtime decoded representation are
//! intentionally the same type — `loader_defs::paravisor::ContainerPolicy`
//! (re-exported here for convenience). Each `ContainerPolicy` variant
//! identifies a container product on the wire via its `#[mesh(N)]` tag;
//! adding a new product means adding a new variant in `loader_defs` and
//! writing the consumer code in OpenHCL that reads it.
//!
//! The payload is appended in-place after
//! [`loader_defs::paravisor::ParavisorMeasuredVtl2Config`] on the same
//! measured config region, starting at byte
//! [`loader_defs::paravisor::CONTAINER_POLICY_INLINE_OFFSET`]. Its
//! byte length is recorded in the struct's
//! `container_policy_size` field; a length of zero — including the
//! all-zero trailing bytes of pre-feature IGVMs — means absent.
//!
//! All structural errors must propagate via `Result`; OpenHCL refuses
//! to boot when an attested policy fails to decode.

use loader_defs::paravisor::ContainerPolicy;
use loader_defs::paravisor::decode_container_policy_page;

/// Decode the mesh-encoded bytes following
/// [`ParavisorMeasuredVtl2Config`] on the measured config region into a
/// [`ContainerPolicy`].
///
/// `bytes` is exactly the byte range
/// `[CONTAINER_POLICY_INLINE_OFFSET .. CONTAINER_POLICY_INLINE_OFFSET +
///   container_policy_size]`. Callers must arrange the slice so it
/// contains the policy body and nothing more; the build records the
/// exact byte length in
/// [`ParavisorMeasuredVtl2Config::container_policy_size`].
///
/// Returns `Err(_)` on any mesh decode failure. The boot path must
/// propagate this so an attested-but-corrupted policy refuses to start.
///
/// [`ParavisorMeasuredVtl2Config`]: loader_defs::paravisor::ParavisorMeasuredVtl2Config
/// [`ParavisorMeasuredVtl2Config::container_policy_size`]: loader_defs::paravisor::ParavisorMeasuredVtl2Config::container_policy_size
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
    fn default_cwcow_round_trips() {
        let policy = ContainerPolicy::Cwcow(CwcowPolicy::default());
        let bytes = encode_container_policy_page(&policy);
        let decoded = read_container_policy(&bytes).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn garbage_bytes_are_rejected() {
        let bytes = [0xFFu8, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0xF9, 0xF8];
        assert!(read_container_policy(&bytes).is_err());
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
        // Empty bytes cannot be decoded into a variant: the caller
        // promised at least one byte by passing a non-zero
        // container_policy_size. A zero size MUST NOT reach this
        // function (the caller checks first); if it does, we error.
        assert!(read_container_policy(&[]).is_err());
    }

    #[test]
    fn does_not_panic_on_random_inputs() {
        // Small no-panic table: 16 deterministic "random" inputs of
        // varying length must produce a Result (no unwinding).
        for seed in 0u32..16 {
            let mut buf = Vec::new();
            let len = (seed * 7) as usize;
            for i in 0..len {
                buf.push((seed.wrapping_add(i as u32) & 0xFF) as u8);
            }
            let _ = read_container_policy(&buf);
        }
    }
}
