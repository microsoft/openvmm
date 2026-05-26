// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runtime parser for the optional measured [`ContainerPolicy`] page.
//!
//! The wire format and the runtime decoded representation are
//! intentionally the same type — `loader_defs::paravisor::ContainerPolicy`
//! (re-exported here for convenience). Each `ContainerPolicy` variant
//! identifies a container product on the wire via its `#[mesh(N)]` tag;
//! adding a new product means adding a new variant in `loader_defs` and
//! writing the consumer code in OpenHCL that reads it.
//!
//! The measured page payload is **framed**: a fixed
//! [`loader_defs::paravisor::CONTAINER_POLICY_LEN_PREFIX_BYTES`]-byte
//! little-endian `u32` length precedes the `mesh_protobuf` body so the
//! runtime can ignore the page-aligned zero padding the IGVM importer
//! produces. Without this framing `mesh_protobuf` would interpret
//! trailing zero bytes as additional fields and error out.
//!
//! All error paths must propagate via `Result`; OpenHCL refuses to boot
//! when an attested policy fails to decode.

use loader_defs::paravisor::ContainerPolicy;
use loader_defs::paravisor::decode_container_policy_page;

/// Decode the framed measured page bytes into a [`ContainerPolicy`].
///
/// `page_bytes` is the full container policy region (length-prefix +
/// mesh-encoded body + zero padding). Any decode failure is reported as
/// an error so the boot path can hard-fail consistently.
pub fn read_container_policy(page_bytes: &[u8]) -> anyhow::Result<ContainerPolicy> {
    decode_container_policy_page(page_bytes)
        .map_err(|e| anyhow::Error::new(e).context("container policy page decode"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hvdef::HV_PAGE_SIZE;
    use loader_defs::paravisor::CwcowPolicy;
    use loader_defs::paravisor::PARAVISOR_MEASURED_VTL2_CONFIG_CONTAINER_POLICY_SIZE_PAGES;
    use loader_defs::paravisor::encode_container_policy_page;

    fn region_buf() -> Vec<u8> {
        vec![
            0u8;
            (PARAVISOR_MEASURED_VTL2_CONFIG_CONTAINER_POLICY_SIZE_PAGES * HV_PAGE_SIZE) as usize
        ]
    }

    fn sample_cwcow() -> ContainerPolicy {
        ContainerPolicy::Cwcow(CwcowPolicy {
            vmgs_read_only: true,
            require_secure_boot: true,
            require_secure_boot_vars: true,
            require_bcd_integrity: true,
            require_secure_avic: false,
            debug_mode: false,
            custom_uefi_json: vec![1, 2, 3, 4, 5],
        })
    }

    /// Helper: build a full region buffer carrying a framed policy plus
    /// page-aligned zero padding, simulating what the IGVM importer
    /// produces.
    fn build_region(policy: &ContainerPolicy) -> Vec<u8> {
        let mut buf = region_buf();
        let encoded = encode_container_policy_page(policy);
        buf[..encoded.len()].copy_from_slice(&encoded);
        buf
    }

    #[test]
    fn known_cwcow_variant_round_trips() {
        let policy = sample_cwcow();
        let buf = build_region(&policy);
        let decoded = read_container_policy(&buf).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn trailing_zero_padding_is_tolerated() {
        // build_region() already pads to the full reserved size — that
        // exercises the typical page-aligned scenario.
        let policy = ContainerPolicy::Cwcow(CwcowPolicy::default());
        let buf = build_region(&policy);
        let decoded = read_container_policy(&buf).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn all_zero_buffer_is_an_error() {
        // An all-zero region (length prefix = 0, empty body) does NOT
        // form a valid encoded ContainerPolicy: a length of zero means
        // no body, but mesh_protobuf cannot reconstruct a variant from
        // zero bytes. This must hard-fail rather than producing a
        // spurious "default" value.
        let buf = region_buf();
        let err = read_container_policy(&buf).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("decode") || msg.contains("Mesh") || msg.contains("policy"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn garbage_bytes_are_rejected() {
        let mut buf = region_buf();
        // Length prefix declaring 8 garbage bytes.
        buf[..4].copy_from_slice(&8u32.to_le_bytes());
        buf[4..12].copy_from_slice(&[0xFFu8, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0xF9, 0xF8]);
        assert!(read_container_policy(&buf).is_err());
    }

    #[test]
    fn truncated_buffer_is_rejected() {
        let policy = ContainerPolicy::Cwcow(CwcowPolicy {
            custom_uefi_json: vec![0u8; 64],
            ..Default::default()
        });
        let mut bytes = encode_container_policy_page(&policy);
        // Drop the last byte of the encoded payload.
        bytes.pop();
        assert!(read_container_policy(&bytes).is_err());
    }

    #[test]
    fn multi_page_payload_round_trips() {
        // Construct a policy whose framed encoding crosses a page boundary.
        let policy = ContainerPolicy::Cwcow(CwcowPolicy {
            require_secure_boot: true,
            custom_uefi_json: vec![0xAB; HV_PAGE_SIZE as usize + 1],
            ..Default::default()
        });
        let buf = build_region(&policy);
        let decoded = read_container_policy(&buf).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn buffer_too_small_for_prefix_is_rejected() {
        // A buffer shorter than CONTAINER_POLICY_LEN_PREFIX_BYTES cannot
        // even hold the length header.
        let buf = vec![0u8, 0u8];
        let err = read_container_policy(&buf).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.to_lowercase().contains("prefix")
                || msg.to_lowercase().contains("too small")
                || msg.to_lowercase().contains("decode"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn declared_length_exceeds_buffer_is_rejected() {
        let mut buf = region_buf();
        // Declare a body length larger than the region itself.
        buf[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_container_policy(&buf).is_err());
    }

    #[test]
    fn does_not_panic_on_random_inputs() {
        // Small no-panic table: 16 deterministic "random" inputs of
        // varying length must produce a Result (no unwinding).
        for seed in 0u32..16 {
            let mut buf = Vec::new();
            // Length prefix drawn from the seed.
            buf.extend_from_slice(&seed.wrapping_mul(0xDEAD_BEEFu32).to_le_bytes());
            // Body bytes: a deterministic stream.
            let len = (seed * 7) as usize;
            for i in 0..len {
                buf.push((seed.wrapping_add(i as u32) & 0xFF) as u8);
            }
            let _ = read_container_policy(&buf);
        }
    }

    #[test]
    fn future_field_addition_is_backward_compatible() {
        // mesh_protobuf treats new fields as optional. Encoding a
        // current `CwcowPolicy` and decoding it back must always
        // succeed — even when (in future) the wire body carries
        // additional `#[mesh(N)]` fields that older readers don't
        // recognise. This test asserts the well-formed-bytes path
        // stays stable; the broader backward-compat contract is a
        // property of mesh_protobuf itself.
        let policy = sample_cwcow();
        let bytes = encode_container_policy_page(&policy);
        let decoded = read_container_policy(&bytes).unwrap();
        assert_eq!(decoded, policy);
    }
}
