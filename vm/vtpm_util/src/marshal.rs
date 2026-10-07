// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Marshal selected TPM structures used by `vtpm_util`.
//! TPM reference documents such as TPM-Rev-2.0-Part-2-Structures-01.38.pdf are a good source.

use std::io;
use tpm_protocol::tpm20proto::AlgId;
use tpm_protocol::tpm20proto::protocol::Tpm2bBuffer;
use zerocopy::IntoBytes;

// Table 187 -- TPMT_SENSITIVE Structure <I/O>
#[repr(C)]
pub struct TpmtSensitive {
    /// TPMI_ALG_PUBLIC
    pub sensitive_type: AlgId,
    /// `TPM2B_AUTH`
    pub auth_value: Tpm2bBuffer,
    /// `TPM2B_DIGEST`
    pub seed_value: Tpm2bBuffer,
    /// `TPM2B_PRIVATE_KEY_RSA`
    pub sensitive: Tpm2bBuffer,
}

/// Marshals the `TpmtSensitive` structure into a buffer.
pub fn tpmt_sensitive_marshal(source: &TpmtSensitive) -> Result<Vec<u8>, io::Error> {
    let mut buffer = Vec::new();

    buffer.extend_from_slice(source.sensitive_type.as_bytes());
    buffer.extend_from_slice(&source.auth_value.serialize());
    buffer.extend_from_slice(&source.seed_value.serialize());
    buffer.extend_from_slice(&source.sensitive.serialize());

    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::TpmtSensitive;
    use super::tpmt_sensitive_marshal;
    use test_with_tracing::test;
    use tpm_protocol::tpm20proto::AlgIdEnum;
    use tpm_protocol::tpm20proto::protocol::Tpm2bBuffer;

    #[test]
    fn tpmt_sensitive_has_one_size_prefix_per_buffer() {
        let sensitive = TpmtSensitive {
            sensitive_type: AlgIdEnum::RSA.into(),
            auth_value: Tpm2bBuffer::new(&[]).unwrap(),
            seed_value: Tpm2bBuffer::new(&[]).unwrap(),
            sensitive: Tpm2bBuffer::new(&[1, 2, 3]).unwrap(),
        };

        assert_eq!(
            tpmt_sensitive_marshal(&sensitive).unwrap(),
            [0, 1, 0, 0, 0, 0, 0, 3, 1, 2, 3]
        );
    }
}
