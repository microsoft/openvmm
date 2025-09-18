use tpm::tpm20proto::AlgId;
///! Marshal TPM structures: selected ones only for our use case in cvmutil.
///! TPM reference documents such as TPM-Rev-2.0-Part-2-Structures-01.38.pdf is a good source.
use tpm::tpm20proto::protocol::Tpm2bBuffer;
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
pub fn tpmt_sensitive_marshal(source: &TpmtSensitive) -> Result<Vec<u8>, std::io::Error> {
    let mut buffer = Vec::new();

    // Marshal sensitive_type (TPMI_ALG_PUBLIC) - 2 bytes
    let sensitive_type_bytes = source.sensitive_type.as_bytes();
    tracing::trace!(
        "Marshaling sensitive_type: {} bytes = {:02X?}",
        sensitive_type_bytes.len(),
        sensitive_type_bytes
    );
    buffer.extend_from_slice(&sensitive_type_bytes);

    // Marshal auth_value (TPM2B_AUTH) - size + data
    let auth_value_bytes = source.auth_value.serialize();
    tracing::trace!(
        "Marshaling auth_value: {} bytes = {:02X?}",
        auth_value_bytes.len(),
        if auth_value_bytes.len() <= 8 {
            &auth_value_bytes[..]
        } else {
            &auth_value_bytes[..8]
        }
    );
    buffer.extend_from_slice(&auth_value_bytes);

    // Marshal seed_value (TPM2B_DIGEST) - size + data
    let seed_value_bytes = source.seed_value.serialize();
    tracing::trace!(
        "Marshaling seed_value: {} bytes = {:02X?}",
        seed_value_bytes.len(),
        if seed_value_bytes.len() <= 8 {
            &seed_value_bytes[..]
        } else {
            &seed_value_bytes[..8]
        }
    );
    buffer.extend_from_slice(&seed_value_bytes);

    // Marshal sensitive (TPMU_SENSITIVE_COMPOSITE) for RSA
    // Based on C++ TPM2B_PRIVATE_KEY_RSA_Marshal, this should be:
    // 1. uint16_t size (of the buffer data)
    // 2. byte array data (the actual prime data)
    let sensitive_bytes = source.sensitive.serialize();
    tracing::trace!(
        "Marshaling sensitive: {} bytes = {:02X?}",
        sensitive_bytes.len(),
        if sensitive_bytes.len() <= 8 {
            &sensitive_bytes[..]
        } else {
            &sensitive_bytes[..8]
        }
    );
    let data_size = sensitive_bytes.len() as u16;
    buffer.extend_from_slice(&data_size.to_be_bytes());
    buffer.extend_from_slice(&sensitive_bytes);

    tracing::trace!("Total marshaled TPMT_SENSITIVE: {} bytes", buffer.len());
    Ok(buffer)
}
