// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SEV-SNP ID block generation and signing.
//!
//! Adds a signed SNP ID block to an already-built IGVM file as an
//! [`IgvmDirectiveHeader::SnpIdBlock`] directive. Two signing modes are
//! supported:
//!
//! - **Out-of-band (production):** `manifest` emits the ID block signing
//!   payload as `<base>-snp.idblock` -- the raw [`SnpPspIdBlock`] bytes, i.e.
//!   exactly the content the SNP firmware hashes (SHA-384) and validates. A
//!   generic file-content signer (e.g. `openssl dgst -sha384 -sign key -out
//!   sig.der file`) signs those bytes and emits a DER-encoded ECDSA signature
//!   file. That signature, plus the signing public key (X.509 cert or SPKI
//!   PEM), is fed back via [`add_snp_id_block_signed`], which reconstructs the
//!   directive without ever holding a private key.
//! - **Temporary key (development/test):** [`add_snp_id_block_temp_key`]
//!   generates an ephemeral ECDSA P-384 key, signs the block in-process, and
//!   embeds the result. This is for local testing only.
//!
//! Either way, the launch digest embedded in the block is the SNP measurement
//! that the `igvm` crate's [`IgvmSerializer`] computes eagerly at construction
//! time, so the file is measured exactly once. The SNP measurement algorithm
//! only hashes page-data directives, so adding the `SnpIdBlock` directive
//! afterwards does not perturb that launch digest -- the embedded `ld` stays
//! valid. Its presence signals the IGVM loader to set `id_block_en = 1`.

use anyhow::Context;
use der::Decode;
use igvm::IgvmDirectiveHeader;
use igvm::IgvmFile;
use igvm::IgvmInitializationHeader;
use igvm::IgvmSerializer;
use igvm_defs::IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY;
use igvm_defs::IGVM_VHS_SNP_ID_BLOCK_SIGNATURE;
use igvm_defs::IgvmPlatformType;
use x86defs::snp::SnpPspIdBlock;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// SNP family identifier for OpenHCL/Underhill guests.
///
/// This value is baked into externally-consumed SNP ID blocks; changing it
/// alters attestation identity, so any edit must be deliberate.
pub const SNP_FAMILY_ID: [u8; 16] = [
    0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
/// SNP image identifier for OpenHCL/Underhill guests.
pub const SNP_IMAGE_ID: [u8; 16] = *b"underhill\0\0\0\0\0\0\0";

const SHA_384_OUTPUT_SIZE_BYTES: usize = 48;
const SNP_ID_KEY_ALGORITHM_ECDSA_P384_SHA384: u32 = 1;
const SNP_ECDSA_CURVE_P384: u32 = 2;
const SNP_ECC_KEY_SIZE_BYTES: usize = 48;
const SNP_ECC_COMPONENT_SIZE_BYTES: usize = 72;

/// Build the SNP ID block signing payload for an IGVM file.
///
/// Called by `manifest` to emit `<base>-snp.idblock`. The returned bytes are
/// the raw [`SnpPspIdBlock`] -- exactly the content the SNP firmware hashes
/// (SHA-384) and validates. A file-content signer signs these bytes directly
/// (SHA-384 + ECDSA P-384, DER-encoded signature), so the emitted signature is
/// valid for the firmware without any repackaging. `ld` is the SNP launch
/// measurement, `policy` comes from the file's `GuestPolicy`, and `guest_svn`
/// from the manifest.
pub fn id_block_signing_payload(ld: &[u8], guest_svn: u32, policy: u64) -> anyhow::Result<Vec<u8>> {
    let ld: [u8; SHA_384_OUTPUT_SIZE_BYTES] =
        ld.try_into().context("SNP launch digest is not 48 bytes")?;
    let id_block = SnpPspIdBlock {
        ld,
        family_id: SNP_FAMILY_ID,
        image_id: SNP_IMAGE_ID,
        version: 0x1,
        guest_svn,
        policy,
    };
    Ok(id_block.as_bytes().to_vec())
}

/// Read the SNP `GuestPolicy` value for `compatibility_mask` from an IGVM file,
/// if present.
pub fn guest_policy(igvm_file: &IgvmFile, compatibility_mask: u32) -> Option<u64> {
    igvm_file.initializations().iter().find_map(|h| match h {
        IgvmInitializationHeader::GuestPolicy {
            policy,
            compatibility_mask: mask,
        } if mask & compatibility_mask == compatibility_mask => Some(*policy),
        _ => None,
    })
}

/// Add an SNP ID block signed by an ephemeral key (development/test only).
///
/// The SNP launch digest is taken from the measurement that
/// [`IgvmSerializer::new`] computes eagerly, so the file is measured exactly
/// once. A random ECDSA P-384 key signs the block in-process. Production flows
/// should instead use [`add_snp_id_block_signed`] with an out-of-band
/// signature.
///
/// # Arguments
/// * `igvm_data` - Input IGVM file; must contain an SEV-SNP platform header and
///   a matching [`IgvmInitializationHeader::GuestPolicy`].
/// * `guest_svn` - Guest security version number to embed.
///
/// # Errors
/// Returns an error if the file has no SEV-SNP platform, already contains an
/// SNP ID block, lacks an SNP measurement/guest policy, or if signing fails.
pub fn add_snp_id_block_temp_key(igvm_data: &[u8], guest_svn: u32) -> anyhow::Result<Vec<u8>> {
    let igvm_file =
        IgvmFile::new_from_binary(igvm_data, None).context("parsing input IGVM file")?;
    let (compatibility_mask, policy) = snp_context(&igvm_file)?;

    let mut serializer = IgvmSerializer::new(&igvm_file).context("constructing IGVM serializer")?;
    let ld = snp_measurement(&serializer)?;

    let psp_id_block = SnpPspIdBlock {
        ld,
        family_id: SNP_FAMILY_ID,
        image_id: SNP_IMAGE_ID,
        version: 0x1,
        guest_svn,
        policy,
    };
    tracing::info!("SNP ID Block (temporary key) {:x?}", psp_id_block);

    let (signature, public_key) = sign_id_block_with_temp_key(&psp_id_block)?;
    serializer.add_directive(id_block_directive(
        &psp_id_block,
        compatibility_mask,
        signature,
        public_key,
    ));

    finish(serializer, igvm_data.len())
}

/// Add an SNP ID block using an out-of-band signature (production).
///
/// `signing_payload` is the `<base>-snp.idblock` emitted by `manifest` (raw
/// [`SnpPspIdBlock`], see [`id_block_signing_payload`]); `signature_der` is the
/// DER-encoded ECDSA signature a file-content signer produced over those exact
/// bytes; `public_key_pem` is the signer's public key as an X.509 certificate
/// or SPKI public key (PEM or DER). The payload's launch digest and policy are
/// checked against the IGVM file being patched, and the signature is
/// cryptographically verified over the payload bytes with the supplied public
/// key, so that a stale payload, wrong key, or corrupt signature fails at build
/// time rather than only when the guest fails to launch on real hardware.
///
/// # Errors
/// Returns an error if the file has no SEV-SNP platform, already contains an
/// SNP ID block, lacks an SNP measurement/guest policy, if the signing payload
/// is malformed or does not match the file, if the signature/public key cannot
/// be parsed, or if the signature does not verify.
pub fn add_snp_id_block_signed(
    igvm_data: &[u8],
    signing_payload: &[u8],
    signature_der: &[u8],
    public_key_pem: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let igvm_file =
        IgvmFile::new_from_binary(igvm_data, None).context("parsing input IGVM file")?;
    let (compatibility_mask, policy) = snp_context(&igvm_file)?;

    let mut serializer = IgvmSerializer::new(&igvm_file).context("constructing IGVM serializer")?;
    let ld = snp_measurement(&serializer)?;

    // The out-of-band signature was produced over the signing payload's exact
    // bytes, so the directive must be reconstructed from the payload's ID block
    // verbatim. Verify it matches the file we are patching so a stale payload
    // fails clearly instead of yielding a file that will not attest.
    let id_block = parse_signing_payload(signing_payload)?;
    anyhow::ensure!(
        id_block.ld == ld,
        "SNP ID block signing payload launch digest does not match the IGVM \
         file measurement; the payload was generated for a different build"
    );
    anyhow::ensure!(
        id_block.policy == policy,
        "SNP ID block signing payload policy 0x{:X} does not match the IGVM \
         file GuestPolicy 0x{policy:X}",
        id_block.policy,
    );

    let (signature, public_key) =
        signature_and_verify(signing_payload, signature_der, public_key_pem)?;
    tracing::info!("SNP ID Block (out-of-band signature) {:x?}", id_block);
    serializer.add_directive(id_block_directive(
        &id_block,
        compatibility_mask,
        signature,
        public_key,
    ));

    finish(serializer, igvm_data.len())
}

/// Locate the SEV-SNP compatibility mask, reject a pre-existing ID block, and
/// return `(compatibility_mask, guest_policy)`.
fn snp_context(igvm_file: &IgvmFile) -> anyhow::Result<(u32, u64)> {
    let compatibility_mask = crate::platform_mask::lookup_compatibility_mask(
        igvm_file.platforms(),
        IgvmPlatformType::SEV_SNP,
    )?;

    // Refuse to double-add for this compatibility mask: a second ID block for
    // the same mask would make the file ambiguous. Callers wanting to re-sign
    // must start from a file without one for this mask.
    if igvm_file.directives().iter().any(|h| {
        matches!(h, IgvmDirectiveHeader::SnpIdBlock { compatibility_mask: mask, .. } if *mask == compatibility_mask)
    }) {
        anyhow::bail!(
            "IGVM file already contains an SNP ID block for compatibility mask \
             0x{compatibility_mask:X}; refusing to add a second one"
        );
    }

    let policy = guest_policy(igvm_file, compatibility_mask)
        .context("missing SNP GuestPolicy initialization header")?;

    Ok((compatibility_mask, policy))
}

/// Fetch the cached SNP launch measurement (48-byte SHA-384) from a serializer.
fn snp_measurement(
    serializer: &IgvmSerializer<'_>,
) -> anyhow::Result<[u8; SHA_384_OUTPUT_SIZE_BYTES]> {
    serializer
        .measurement_for(IgvmPlatformType::SEV_SNP)
        .context("no SNP launch measurement computed for the IGVM file")?
        .digest
        .as_slice()
        .try_into()
        .context("SNP launch digest is not 48 bytes")
}

/// Serialize the staged serializer to bytes with a trace of the result size.
fn finish(serializer: IgvmSerializer<'_>, input_size: usize) -> anyhow::Result<Vec<u8>> {
    let mut output = Vec::new();
    serializer
        .serialize(&mut output)
        .context("serializing IGVM file with SNP ID block")?;
    tracing::info!(
        input_size,
        output_size = output.len(),
        "Added SNP ID block to IGVM file"
    );
    Ok(output)
}

/// Parse and validate an SNP ID block signing payload (the raw
/// [`SnpPspIdBlock`] bytes emitted by `manifest`).
fn parse_signing_payload(bytes: &[u8]) -> anyhow::Result<SnpPspIdBlock> {
    anyhow::ensure!(
        bytes.len() == size_of::<SnpPspIdBlock>(),
        "SNP ID block signing payload must be exactly {} bytes, got {}",
        size_of::<SnpPspIdBlock>(),
        bytes.len()
    );
    let (id_block, _) = SnpPspIdBlock::read_from_prefix(bytes)
        .map_err(|_| anyhow::anyhow!("SNP ID block signing payload is malformed"))?;
    Ok(id_block)
}

/// Left-pad a big-endian ECC scalar/coordinate to 48 bytes.
fn left_pad_be(field: &str, be: &[u8]) -> anyhow::Result<[u8; SNP_ECC_KEY_SIZE_BYTES]> {
    anyhow::ensure!(
        be.len() <= SNP_ECC_KEY_SIZE_BYTES,
        "{field} is {} bytes, exceeds {SNP_ECC_KEY_SIZE_BYTES}",
        be.len()
    );
    let mut out = [0u8; SNP_ECC_KEY_SIZE_BYTES];
    out[SNP_ECC_KEY_SIZE_BYTES - be.len()..].copy_from_slice(be);
    Ok(out)
}

/// Parse a DER-encoded ECDSA signature (`SEQUENCE { INTEGER r, INTEGER s }`)
/// into its big-endian 48-byte P-384 `(r, s)` components.
fn parse_der_ecdsa_p384(
    der_sig: &[u8],
) -> anyhow::Result<([u8; SNP_ECC_KEY_SIZE_BYTES], [u8; SNP_ECC_KEY_SIZE_BYTES])> {
    #[derive(der::Sequence)]
    struct EcdsaSigDer<'a> {
        r: der::asn1::UintRef<'a>,
        s: der::asn1::UintRef<'a>,
    }
    let sig = EcdsaSigDer::from_der(der_sig).context("parsing DER-encoded ECDSA signature")?;
    Ok((
        left_pad_be("signature r", sig.r.as_bytes())?,
        left_pad_be("signature s", sig.s.as_bytes())?,
    ))
}

/// Extract the signer's ECDSA P-384 public key from a supplied public key.
///
/// Accepts an X.509 certificate or a bare `SubjectPublicKeyInfo`, in PEM or
/// DER form. The actual key parsing is delegated to the `crypto` crate.
fn parse_p384_public_key(public_key: &[u8]) -> anyhow::Result<crypto::ecdsa::EcdsaPublicKey> {
    use crypto::ecdsa::EcdsaPublicKey;
    use crypto::x509::X509Certificate;

    // Extract the ECDSA public key from a DER-encoded X.509 certificate,
    // routing through the `crypto` X.509 parser rather than doing our own DER
    // work here.
    fn key_from_cert_der(der: &[u8]) -> anyhow::Result<EcdsaPublicKey> {
        X509Certificate::from_der(der)
            .context("parsing certificate")?
            .public_key()
            .context("extracting certificate public key")?
            .ecdsa()
            .context("certificate public key is not an ECDSA key")
    }

    if let Ok(text) = std::str::from_utf8(public_key)
        && text.trim_start().starts_with("-----BEGIN")
    {
        let (label, doc) = der::Document::from_pem(text).context("parsing public key PEM")?;
        match label {
            "CERTIFICATE" => key_from_cert_der(doc.as_bytes()),
            "PUBLIC KEY" => EcdsaPublicKey::from_public_key_der(doc.as_bytes())
                .context("parsing SubjectPublicKeyInfo"),
            other => {
                anyhow::bail!("unexpected PEM label {other:?}; expected CERTIFICATE or PUBLIC KEY")
            }
        }
        .context("parsing SNP ID block public key")
    } else {
        // DER: try a certificate first, then a bare SubjectPublicKeyInfo.
        key_from_cert_der(public_key)
            .or_else(|_| {
                EcdsaPublicKey::from_public_key_der(public_key)
                    .context("parsing SubjectPublicKeyInfo")
            })
            .context("parsing SNP ID block public key (DER certificate or SubjectPublicKeyInfo)")
    }
}

/// Parse a DER ECDSA signature + public key, cryptographically verify the
/// signature over `signed_bytes` (the signing payload the signer signed), and
/// return the IGVM ID block signature and public-key structures (big-endian in,
/// PSP little-endian layout out).
///
/// Verifying here catches a wrong key or corrupt signature at build time,
/// rather than only when the guest fails to launch on real hardware.
fn signature_and_verify(
    signed_bytes: &[u8],
    signature_der: &[u8],
    public_key_pem: &[u8],
) -> anyhow::Result<(
    IGVM_VHS_SNP_ID_BLOCK_SIGNATURE,
    IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY,
)> {
    let (r, s) = parse_der_ecdsa_p384(signature_der)?;
    let public_key = parse_p384_public_key(public_key_pem)?;

    let mut sig = Vec::with_capacity(2 * SNP_ECC_KEY_SIZE_BYTES);
    sig.extend_from_slice(&r);
    sig.extend_from_slice(&s);

    // Verify (public key, r||s) over the SHA-384 of the signed content.
    let valid = public_key
        .verify(crypto::HashAlgorithm::Sha384, signed_bytes, &sig)
        .context("verifying SNP ID block signature")?;
    anyhow::ensure!(
        valid,
        "SNP ID block signature does not verify against the supplied public key; \
         the signature, public key, or signing payload do not correspond"
    );

    // Export the verified key as `Qx || Qy` (big-endian, 48 bytes each) for the
    // PSP ID block public-key structure.
    let qxqy = public_key
        .public_key_bytes()
        .context("exporting SNP ID block public key")?;
    anyhow::ensure!(
        qxqy.len() == 2 * SNP_ECC_KEY_SIZE_BYTES,
        "unexpected SNP ID block public key size {}",
        qxqy.len()
    );
    let (qx, qy) = qxqy.split_at(SNP_ECC_KEY_SIZE_BYTES);

    Ok((
        IGVM_VHS_SNP_ID_BLOCK_SIGNATURE {
            r_comp: padded_le_component(&r),
            s_comp: padded_le_component(&s),
        },
        IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY {
            curve: SNP_ECDSA_CURVE_P384,
            reserved: 0,
            qx: padded_le_component(qx),
            qy: padded_le_component(qy),
        },
    ))
}

/// Assemble an [`IgvmDirectiveHeader::SnpIdBlock`] from an ID block plus its
/// signature and public key. Author-key fields are left zeroed (author signing
/// is not used); the directive's presence signals the loader to set
/// `id_block_en = 1`.
fn id_block_directive(
    psp_id_block: &SnpPspIdBlock,
    compatibility_mask: u32,
    id_key_signature: IGVM_VHS_SNP_ID_BLOCK_SIGNATURE,
    id_public_key: IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY,
) -> IgvmDirectiveHeader {
    IgvmDirectiveHeader::SnpIdBlock {
        compatibility_mask,
        author_key_enabled: 0,
        reserved: [0; 3],
        ld: psp_id_block.ld,
        family_id: psp_id_block.family_id,
        image_id: psp_id_block.image_id,
        version: psp_id_block.version,
        guest_svn: psp_id_block.guest_svn,
        id_key_algorithm: SNP_ID_KEY_ALGORITHM_ECDSA_P384_SHA384,
        author_key_algorithm: 0,
        id_key_signature: Box::new(id_key_signature),
        id_public_key: Box::new(id_public_key),
        author_key_signature: Box::new(IGVM_VHS_SNP_ID_BLOCK_SIGNATURE {
            r_comp: [0; SNP_ECC_COMPONENT_SIZE_BYTES],
            s_comp: [0; SNP_ECC_COMPONENT_SIZE_BYTES],
        }),
        author_public_key: Box::new(IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY {
            curve: 0,
            reserved: 0,
            qx: [0; SNP_ECC_COMPONENT_SIZE_BYTES],
            qy: [0; SNP_ECC_COMPONENT_SIZE_BYTES],
        }),
    }
}

/// Zero-pads and reverses a big-endian ECC component into a 72-byte
/// little-endian array as required by the PSP ID block format.
fn padded_le_component(input_be: &[u8]) -> [u8; SNP_ECC_COMPONENT_SIZE_BYTES] {
    let mut out = [0u8; SNP_ECC_COMPONENT_SIZE_BYTES];
    for (dst, src) in out.iter_mut().zip(input_be.iter().rev()) {
        *dst = *src;
    }
    out
}

/// Generate a temporary ECDSA P-384 key pair using the selected `crypto`
/// backend, sign the SHA-384 hash of the ID block, and return the signature
/// + public key in the format expected by `IGVM_VHS_SNP_ID_BLOCK`.
fn sign_id_block_with_temp_key(
    id_block: &SnpPspIdBlock,
) -> anyhow::Result<(
    IGVM_VHS_SNP_ID_BLOCK_SIGNATURE,
    IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY,
)> {
    use crypto::ecdsa::EcdsaCurve;
    use crypto::ecdsa::EcdsaKeyPair;

    // Generate a random P-384 key pair for ECDSA signing.
    let key =
        EcdsaKeyPair::generate(EcdsaCurve::P384).context("generating temporary SNP signing key")?;

    // Hash the ID block with SHA-384.
    let id_block_hash: [u8; SHA_384_OUTPUT_SIZE_BYTES] =
        crypto::sha_384::sha_384(id_block.as_bytes());

    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    tracing::info!("Input Hash Base64: {}", b64.encode(id_block_hash));
    tracing::info!("Using Temporary Signing Key");

    // Sign the ID block bytes; `EcdsaKeyPair::sign` hashes them with SHA-384
    // internally. Returns r || s in big-endian, each 48 bytes for P-384.
    let signature = key
        .sign(crypto::HashAlgorithm::Sha384, id_block.as_bytes())
        .context("signing SNP ID block")?;

    anyhow::ensure!(
        signature.len() == SNP_ECC_KEY_SIZE_BYTES * 2,
        "unexpected SNP ID block signature size {}",
        signature.len()
    );

    let (sig_r_be, sig_s_be) = signature.split_at(SNP_ECC_KEY_SIZE_BYTES);
    let id_key_signature = IGVM_VHS_SNP_ID_BLOCK_SIGNATURE {
        r_comp: padded_le_component(sig_r_be),
        s_comp: padded_le_component(sig_s_be),
    };

    tracing::info!("Signature R Base64: {}", b64.encode(sig_r_be));
    tracing::info!("Signature S Base64: {}", b64.encode(sig_s_be));

    // Export the public key as Qx || Qy in big-endian, each 48 bytes for P-384.
    let public_key = key
        .public_key_bytes()
        .context("exporting temporary SNP public key")?;

    anyhow::ensure!(
        public_key.len() == SNP_ECC_KEY_SIZE_BYTES * 2,
        "unexpected SNP ID block public key size {}",
        public_key.len()
    );

    let (qx_be, qy_be) = public_key.split_at(SNP_ECC_KEY_SIZE_BYTES);

    tracing::info!("Public Key Qx Base64: {}", b64.encode(qx_be));
    tracing::info!("Public Key Qy Base64: {}", b64.encode(qy_be));
    let id_public_key = IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY {
        curve: SNP_ECDSA_CURVE_P384,
        reserved: 0,
        qx: padded_le_component(qx_be),
        qy: padded_le_component(qy_be),
    };

    Ok((id_key_signature, id_public_key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use igvm::IgvmPlatformHeader;
    use igvm::IgvmRevision;
    use igvm_defs::IGVM_VHS_SUPPORTED_PLATFORM;
    use igvm_defs::IgvmPageDataFlags;
    use igvm_defs::IgvmPageDataType;
    use test_with_tracing::test;

    /// Build a minimal, measurable SNP IGVM file: one SEV-SNP platform, a
    /// matching `GuestPolicy`, and a single measured page.
    fn build_snp_igvm(mask: u32) -> Vec<u8> {
        let platforms = vec![IgvmPlatformHeader::SupportedPlatform(
            IGVM_VHS_SUPPORTED_PLATFORM {
                compatibility_mask: mask,
                highest_vtl: 0,
                platform_type: IgvmPlatformType::SEV_SNP,
                platform_version: 1,
                shared_gpa_boundary: 0,
            },
        )];
        let initializations = vec![IgvmInitializationHeader::GuestPolicy {
            policy: 0x30000,
            compatibility_mask: mask,
        }];
        let directives = vec![IgvmDirectiveHeader::PageData {
            gpa: 0,
            compatibility_mask: mask,
            flags: IgvmPageDataFlags::new(),
            data_type: IgvmPageDataType::NORMAL,
            data: vec![0xAB; 4096],
        }];
        let igvm = IgvmFile::new(IgvmRevision::V1, platforms, initializations, directives)
            .expect("valid SNP IgvmFile");
        let mut out = Vec::new();
        igvm.serialize(&mut out).expect("serialize");
        out
    }

    fn count_id_blocks(data: &[u8]) -> usize {
        let igvm = IgvmFile::new_from_binary(data, None).expect("valid IGVM");
        igvm.directives()
            .iter()
            .filter(|h| matches!(h, IgvmDirectiveHeader::SnpIdBlock { .. }))
            .count()
    }

    /// Simulate a file-content signer: sign the SHA-384 of the signing payload
    /// with a fresh P-384 key, returning `(DER ECDSA signature, DER SPKI public
    /// key)` -- exactly what `openssl dgst -sha384 -sign` plus the signer's
    /// public key would provide.
    fn sign_payload(signing_payload: &[u8]) -> (Vec<u8>, Vec<u8>) {
        use crypto::ecdsa::EcdsaCurve;
        use crypto::ecdsa::EcdsaKeyPair;

        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let raw_sig = key
            .sign(crypto::HashAlgorithm::Sha384, signing_payload)
            .unwrap();
        (ecdsa_raw_to_der(&raw_sig), spki_der(&key))
    }

    /// The uncompressed EC point `0x04 || Qx || Qy` for `key`.
    fn uncompressed_point(key: &crypto::ecdsa::EcdsaKeyPair) -> Vec<u8> {
        let pk = key.public_key_bytes().unwrap();
        let mut point = vec![0x04u8];
        point.extend_from_slice(&pk[..SNP_ECC_KEY_SIZE_BYTES]);
        point.extend_from_slice(&pk[SNP_ECC_KEY_SIZE_BYTES..]);
        point
    }

    /// DER-encode `key`'s public key as a `SubjectPublicKeyInfo` (the bare
    /// `PUBLIC KEY` form).
    fn spki_der(key: &crypto::ecdsa::EcdsaKeyPair) -> Vec<u8> {
        use der::Encode;

        let spki = x509_cert::spki::SubjectPublicKeyInfo {
            algorithm: x509_cert::spki::AlgorithmIdentifier {
                oid: der::asn1::ObjectIdentifier::new_unwrap("1.2.840.10045.2.1"),
                parameters: Some(der::asn1::ObjectIdentifier::new_unwrap("1.3.132.0.34")),
            },
            subject_public_key: der::asn1::BitString::from_bytes(&uncompressed_point(key)).unwrap(),
        };
        spki.to_der().unwrap()
    }

    /// DER-encode a raw `r || s` ECDSA signature (each `SNP_ECC_KEY_SIZE_BYTES`
    /// big-endian) as a `SEQUENCE { r INTEGER, s INTEGER }` -- the form emitted
    /// by e.g. `openssl dgst -sha384 -sign`.
    fn ecdsa_raw_to_der(raw_sig: &[u8]) -> Vec<u8> {
        use der::Encode;

        fn strip_lz(b: &[u8]) -> &[u8] {
            let mut i = 0;
            while i + 1 < b.len() && b[i] == 0 {
                i += 1;
            }
            &b[i..]
        }
        #[derive(der::Sequence)]
        struct EcdsaSigDer<'a> {
            r: der::asn1::UintRef<'a>,
            s: der::asn1::UintRef<'a>,
        }
        let (r_be, s_be) = raw_sig.split_at(SNP_ECC_KEY_SIZE_BYTES);
        EcdsaSigDer {
            r: der::asn1::UintRef::new(strip_lz(r_be)).unwrap(),
            s: der::asn1::UintRef::new(strip_lz(s_be)).unwrap(),
        }
        .to_der()
        .unwrap()
    }

    /// A minimal self-signed `BuilderProfile` (issuer == subject, no
    /// extensions).
    struct SelfSignedProfile {
        name: x509_cert::name::Name,
    }

    impl x509_cert::builder::profile::BuilderProfile for SelfSignedProfile {
        fn get_issuer(&self, _subject: &x509_cert::name::Name) -> x509_cert::name::Name {
            self.name.clone()
        }

        fn get_subject(&self) -> x509_cert::name::Name {
            self.name.clone()
        }

        fn build_extensions(
            &self,
            _spk: x509_cert::spki::SubjectPublicKeyInfoRef<'_>,
            _issuer_spk: x509_cert::spki::SubjectPublicKeyInfoRef<'_>,
            _tbs: &x509_cert::TbsCertificate,
        ) -> x509_cert::builder::Result<Vec<x509_cert::ext::Extension>> {
            Ok(Vec::new())
        }
    }

    /// `signature`/`spki` adapter over a P-384 [`crypto::ecdsa::EcdsaKeyPair`]
    /// so the `x509-cert` builder can produce a self-signed ECDSA certificate.
    /// Only the embedded `SubjectPublicKeyInfo` is consumed by
    /// [`parse_p384_public_key`]; the self-signature is real but its
    /// verification is not part of this path.
    struct EcdsaCertSigner<'a> {
        key: &'a crypto::ecdsa::EcdsaKeyPair,
        point: Vec<u8>,
    }

    #[derive(Clone)]
    struct EcdsaVerifyingKey(Vec<u8>);

    struct EcdsaDerSignature(Vec<u8>);

    impl signature::Keypair for EcdsaCertSigner<'_> {
        type VerifyingKey = EcdsaVerifyingKey;

        fn verifying_key(&self) -> Self::VerifyingKey {
            EcdsaVerifyingKey(self.point.clone())
        }
    }

    impl x509_cert::spki::DynSignatureAlgorithmIdentifier for EcdsaCertSigner<'_> {
        fn signature_algorithm_identifier(
            &self,
        ) -> x509_cert::spki::Result<x509_cert::spki::AlgorithmIdentifierOwned> {
            Ok(x509_cert::spki::AlgorithmIdentifierOwned {
                // ecdsa-with-SHA384
                oid: der::asn1::ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.3"),
                parameters: None,
            })
        }
    }

    impl signature::Signer<EcdsaDerSignature> for EcdsaCertSigner<'_> {
        fn try_sign(&self, msg: &[u8]) -> Result<EcdsaDerSignature, signature::Error> {
            let raw = self
                .key
                .sign(crypto::HashAlgorithm::Sha384, msg)
                .map_err(|_| signature::Error::new())?;
            Ok(EcdsaDerSignature(ecdsa_raw_to_der(&raw)))
        }
    }

    impl x509_cert::spki::EncodePublicKey for EcdsaVerifyingKey {
        fn to_public_key_der(&self) -> x509_cert::spki::Result<der::Document> {
            use der::Encode;

            let spki = x509_cert::spki::SubjectPublicKeyInfoOwned {
                algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
                    oid: der::asn1::ObjectIdentifier::new_unwrap("1.2.840.10045.2.1"),
                    parameters: Some(der::Any::from(der::asn1::ObjectIdentifier::new_unwrap(
                        "1.3.132.0.34",
                    ))),
                },
                subject_public_key: der::asn1::BitString::from_bytes(&self.0)?,
            };
            Ok(der::Document::try_from(spki.to_der()?)?)
        }
    }

    impl x509_cert::spki::SignatureBitStringEncoding for EcdsaDerSignature {
        fn to_bitstring(&self) -> der::Result<der::asn1::BitString> {
            der::asn1::BitString::from_bytes(&self.0)
        }
    }

    /// Build a self-signed P-384 certificate (DER) whose
    /// `SubjectPublicKeyInfo` carries `key`'s public point.
    fn self_signed_p384_cert_der(key: &crypto::ecdsa::EcdsaKeyPair) -> Vec<u8> {
        use core::str::FromStr;
        use der::Encode;
        use x509_cert::builder::Builder;

        let point = uncompressed_point(key);
        let spki = x509_cert::spki::SubjectPublicKeyInfoOwned {
            algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
                oid: der::asn1::ObjectIdentifier::new_unwrap("1.2.840.10045.2.1"),
                parameters: Some(der::Any::from(der::asn1::ObjectIdentifier::new_unwrap(
                    "1.3.132.0.34",
                ))),
            },
            subject_public_key: der::asn1::BitString::from_bytes(&point).unwrap(),
        };

        let name = x509_cert::name::Name::from_str("CN=snp-id-block-test").unwrap();
        let serial = x509_cert::serial_number::SerialNumber::from(1u32);
        let validity = x509_cert::time::Validity::new(
            der::asn1::GeneralizedTime::from_unix_duration(std::time::Duration::from_secs(0))
                .unwrap()
                .into(),
            x509_cert::time::Time::INFINITY,
        );

        let builder = x509_cert::builder::CertificateBuilder::new(
            SelfSignedProfile { name },
            serial,
            validity,
            spki,
        )
        .unwrap();

        let signer = EcdsaCertSigner { key, point };
        builder.build(&signer).unwrap().to_der().unwrap()
    }

    /// Pin the SNP ID block identity constants to their exact byte values.
    /// These are baked into externally-consumed SNP ID blocks, so any change
    /// must be a deliberate, reviewed edit.
    #[test]
    fn snp_id_block_constants_byte_identity() {
        assert_eq!(
            SNP_FAMILY_ID,
            [
                0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00,
            ]
        );
        assert_eq!(SNP_IMAGE_ID, *b"underhill\0\0\0\0\0\0\0");
        assert_eq!(SNP_FAMILY_ID.len(), 16);
        assert_eq!(SNP_IMAGE_ID.len(), 16);
    }

    /// The temporary signing path must produce correctly-sized signature and
    /// public-key components in the PSP little-endian layout.
    #[test]
    fn temp_signing_produces_expected_sizes() {
        let id_block = SnpPspIdBlock {
            ld: [0x11; 48],
            family_id: SNP_FAMILY_ID,
            image_id: SNP_IMAGE_ID,
            version: 0x1,
            guest_svn: 3,
            policy: 0x30000,
        };
        let (sig, pubkey) = sign_id_block_with_temp_key(&id_block).expect("temp signing succeeds");
        // r/s each occupy 48 significant bytes zero-padded into a 72-byte field.
        assert_eq!(sig.r_comp.len(), SNP_ECC_COMPONENT_SIZE_BYTES);
        assert_eq!(sig.s_comp.len(), SNP_ECC_COMPONENT_SIZE_BYTES);
        assert_eq!(pubkey.curve, SNP_ECDSA_CURVE_P384);
        assert_eq!(pubkey.qx.len(), SNP_ECC_COMPONENT_SIZE_BYTES);
        assert_eq!(pubkey.qy.len(), SNP_ECC_COMPONENT_SIZE_BYTES);
        // The top (most-significant) bytes beyond the 48-byte component must
        // be zero padding.
        assert!(sig.r_comp[SNP_ECC_KEY_SIZE_BYTES..].iter().all(|&b| b == 0));
        assert!(pubkey.qx[SNP_ECC_KEY_SIZE_BYTES..].iter().all(|&b| b == 0));
    }

    /// End-to-end: adding an ID block yields a valid file with exactly one
    /// `SnpIdBlock`, and its embedded `ld` equals the measurement of the
    /// original (ID-block-free) file -- proving the ID block does not perturb
    /// the launch digest.
    #[test]
    fn add_snp_id_block_preserves_measurement() {
        let igvm_data = build_snp_igvm(0x1);

        // Reference digest computed on the original file.
        let original = IgvmFile::new_from_binary(&igvm_data, None).unwrap();
        let ref_ld = IgvmSerializer::new(&original)
            .unwrap()
            .measurement_for(IgvmPlatformType::SEV_SNP)
            .unwrap()
            .digest
            .clone();

        let out = add_snp_id_block_temp_key(&igvm_data, 7).expect("add SNP ID block");
        assert_eq!(count_id_blocks(&out), 1);

        let parsed = IgvmFile::new_from_binary(&out, None).expect("valid output");
        let id_block = parsed
            .directives()
            .iter()
            .find_map(|h| match h {
                IgvmDirectiveHeader::SnpIdBlock { ld, guest_svn, .. } => Some((*ld, *guest_svn)),
                _ => None,
            })
            .expect("id block present");
        assert_eq!(id_block.0.as_slice(), ref_ld.as_slice());
        assert_eq!(id_block.1, 7);

        // The measurement of the patched file must be unchanged.
        let after_ld = IgvmSerializer::new(&parsed)
            .unwrap()
            .measurement_for(IgvmPlatformType::SEV_SNP)
            .unwrap()
            .digest
            .clone();
        assert_eq!(after_ld, ref_ld);
    }

    /// Adding a second ID block must be refused.
    #[test]
    fn add_snp_id_block_rejects_double_add() {
        let igvm_data = build_snp_igvm(0x1);
        let once = add_snp_id_block_temp_key(&igvm_data, 1).expect("first add");
        let err = add_snp_id_block_temp_key(&once, 1).unwrap_err();
        assert!(
            format!("{err:#}").contains("already contains an SNP ID block"),
            "unexpected error: {err:#}"
        );
    }

    /// A file with no SEV-SNP platform must be rejected.
    #[test]
    fn add_snp_id_block_requires_snp_platform() {
        let platforms = vec![IgvmPlatformHeader::SupportedPlatform(
            IGVM_VHS_SUPPORTED_PLATFORM {
                compatibility_mask: 0x1,
                highest_vtl: 0,
                platform_type: IgvmPlatformType::VSM_ISOLATION,
                platform_version: 1,
                shared_gpa_boundary: 0,
            },
        )];
        let directives = vec![IgvmDirectiveHeader::PageData {
            gpa: 0,
            compatibility_mask: 0x1,
            flags: IgvmPageDataFlags::new(),
            data_type: IgvmPageDataType::NORMAL,
            data: vec![0xCD; 4096],
        }];
        let igvm = IgvmFile::new(IgvmRevision::V1, platforms, vec![], directives).unwrap();
        let mut data = Vec::new();
        igvm.serialize(&mut data).unwrap();

        let err = add_snp_id_block_temp_key(&data, 1).unwrap_err();
        assert!(format!("{err:#}").to_lowercase().contains("platform"));
    }

    /// End-to-end out-of-band flow: `manifest` emits the signing payload, a
    /// file-content signer produces a DER signature + public key, and
    /// `add_snp_id_block_signed` reconstructs a valid directive whose
    /// `ld`/`guest_svn` come from the payload and whose launch digest matches
    /// the file measurement.
    #[test]
    fn add_snp_id_block_signed_round_trip() {
        let igvm_data = build_snp_igvm(0x1);

        // Reference measurement + policy of the original file.
        let original = IgvmFile::new_from_binary(&igvm_data, None).unwrap();
        let ref_ld = IgvmSerializer::new(&original)
            .unwrap()
            .measurement_for(IgvmPlatformType::SEV_SNP)
            .unwrap()
            .digest
            .clone();

        // manifest-side: emit the signing payload from the measurement.
        let signing_payload = id_block_signing_payload(&ref_ld, 11, 0x30000).expect("payload");
        // signer-side: produce the DER signature + public key.
        let (sig_der, spki_der) = sign_payload(&signing_payload);

        // add-side: reconstruct and attach.
        let out = add_snp_id_block_signed(&igvm_data, &signing_payload, &sig_der, &spki_der)
            .expect("signed add");
        assert_eq!(count_id_blocks(&out), 1);

        let parsed = IgvmFile::new_from_binary(&out, None).expect("valid output");
        let (ld, svn) = parsed
            .directives()
            .iter()
            .find_map(|h| match h {
                IgvmDirectiveHeader::SnpIdBlock { ld, guest_svn, .. } => Some((*ld, *guest_svn)),
                _ => None,
            })
            .expect("id block present");
        assert_eq!(ld.as_slice(), ref_ld.as_slice());
        assert_eq!(svn, 11);
    }

    /// A malformed DER signature is rejected.
    #[test]
    fn add_snp_id_block_signed_rejects_malformed_signature() {
        let igvm_data = build_snp_igvm(0x1);
        let original = IgvmFile::new_from_binary(&igvm_data, None).unwrap();
        let ref_ld = IgvmSerializer::new(&original)
            .unwrap()
            .measurement_for(IgvmPlatformType::SEV_SNP)
            .unwrap()
            .digest
            .clone();
        let signing_payload = id_block_signing_payload(&ref_ld, 1, 0x30000).unwrap();
        let (_sig_der, spki_der) = sign_payload(&signing_payload);

        let err = add_snp_id_block_signed(&igvm_data, &signing_payload, b"not-der", &spki_der)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("DER-encoded ECDSA signature"),
            "unexpected error: {err:#}"
        );
    }

    /// A valid signature under the WRONG public key is rejected by the
    /// build-time cryptographic verification.
    #[test]
    fn add_snp_id_block_signed_rejects_wrong_public_key() {
        let igvm_data = build_snp_igvm(0x1);
        let ref_ld = IgvmSerializer::new(&IgvmFile::new_from_binary(&igvm_data, None).unwrap())
            .unwrap()
            .measurement_for(IgvmPlatformType::SEV_SNP)
            .unwrap()
            .digest
            .clone();
        let signing_payload = id_block_signing_payload(&ref_ld, 1, 0x30000).unwrap();
        // Signature from one key, public key from a different key.
        let (sig_der, _spki_a) = sign_payload(&signing_payload);
        let (_sig_b, spki_b) = sign_payload(&signing_payload);

        let err =
            add_snp_id_block_signed(&igvm_data, &signing_payload, &sig_der, &spki_b).unwrap_err();
        assert!(
            format!("{err:#}").contains("does not verify"),
            "unexpected error: {err:#}"
        );
    }

    /// A signing payload whose launch digest does not match the file is
    /// rejected before the signature is even consulted.
    #[test]
    fn add_snp_id_block_signed_rejects_stale_payload() {
        let igvm_data = build_snp_igvm(0x1);
        // Signing payload built for a *different* launch digest.
        let signing_payload = id_block_signing_payload(&[0x22; 48], 1, 0x30000).unwrap();
        let (sig_der, spki_der) = sign_payload(&signing_payload);

        let err =
            add_snp_id_block_signed(&igvm_data, &signing_payload, &sig_der, &spki_der).unwrap_err();
        assert!(
            format!("{err:#}").contains("does not match the IGVM file"),
            "unexpected error: {err:#}"
        );
    }

    /// A wrong-length signing payload (not the raw SnpPspIdBlock) is rejected.
    #[test]
    fn add_snp_id_block_signed_rejects_wrong_size_payload() {
        let igvm_data = build_snp_igvm(0x1);
        let (sig_der, spki_der) =
            sign_payload(&id_block_signing_payload(&[0u8; 48], 1, 0x30000).unwrap());
        let err =
            add_snp_id_block_signed(&igvm_data, b"too-short", &sig_der, &spki_der).unwrap_err();
        assert!(
            format!("{err:#}").contains("must be exactly"),
            "unexpected error: {err:#}"
        );
    }

    /// End-to-end with the X.509 certificate signer input (rather than a bare
    /// `SubjectPublicKeyInfo`): the signer's public key is supplied as a P-384
    /// certificate in both DER and PEM form, and `add_snp_id_block_signed`
    /// accepts it and validates the signature. This exercises the certificate
    /// parsing branch of `parse_p384_public_key`.
    #[test]
    fn add_snp_id_block_signed_accepts_x509_certificate() {
        use crypto::ecdsa::EcdsaCurve;
        use crypto::ecdsa::EcdsaKeyPair;

        let igvm_data = build_snp_igvm(0x1);
        let ref_ld = IgvmSerializer::new(&IgvmFile::new_from_binary(&igvm_data, None).unwrap())
            .unwrap()
            .measurement_for(IgvmPlatformType::SEV_SNP)
            .unwrap()
            .digest
            .clone();
        let signing_payload = id_block_signing_payload(&ref_ld, 5, 0x30000).unwrap();

        // Sign the payload and wrap the signer's public key in a self-signed
        // P-384 certificate, using the same key for both.
        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let sig_der = ecdsa_raw_to_der(
            &key.sign(crypto::HashAlgorithm::Sha384, &signing_payload)
                .unwrap(),
        );
        let cert_der = self_signed_p384_cert_der(&key);
        let cert_pem = der::Document::from_der(&cert_der)
            .unwrap()
            .to_pem("CERTIFICATE", der::pem::LineEnding::LF)
            .unwrap();

        for public_key in [cert_der.clone(), cert_pem.into_bytes()] {
            let out = add_snp_id_block_signed(&igvm_data, &signing_payload, &sig_der, &public_key)
                .expect("certificate signer input accepted");
            assert_eq!(count_id_blocks(&out), 1);
            let (ld, svn) = IgvmFile::new_from_binary(&out, None)
                .unwrap()
                .directives()
                .iter()
                .find_map(|h| match h {
                    IgvmDirectiveHeader::SnpIdBlock { ld, guest_svn, .. } => {
                        Some((*ld, *guest_svn))
                    }
                    _ => None,
                })
                .expect("id block present");
            assert_eq!(ld.as_slice(), ref_ld.as_slice());
            assert_eq!(svn, 5);
        }
    }
}
