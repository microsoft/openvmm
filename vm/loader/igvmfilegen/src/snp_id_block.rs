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
//! time. SNP measurement includes page information, parameter inserts, and VP
//! context, but not the ID-block directive. Adding or re-signing the block does
//! not perturb the launch digest. Its presence signals the IGVM loader to set
//! `id_block_en = 1`.

use anyhow::Context;
use crypto::sha_384::SHA_384_OUTPUT_SIZE_BYTES;
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

/// SNP family identifier for OpenHCL guests.
///
/// Layout convention:
/// - `byte[3] == 0x01` identifies OpenHCL.
/// - all other bytes are reserved and must remain `0x00`.
///
/// This value is baked into externally-consumed SNP ID blocks; changing it
/// alters attestation identity, so any edit must be deliberate.
pub const SNP_FAMILY_ID: [u8; 16] = [
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
/// SNP image identifier for OpenHCL guests.
pub const SNP_IMAGE_ID: [u8; 16] = *b"openhcl\0\0\0\0\0\0\0\0\0";

const SNP_ID_KEY_ALGORITHM_ECDSA_P384_SHA384: u32 = 1;
const SNP_ECDSA_CURVE_P384: u32 = 2;
const SNP_ECC_COMPONENT_SIZE_BYTES: usize = 72;

/// Identity fields included in an SNP ID block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SnpImageIdentity {
    family_id: [u8; 16],
    image_id: [u8; 16],
}

impl SnpImageIdentity {
    /// The OpenHCL SNP image identity.
    pub(crate) const OPENHCL: Self = Self::new(SNP_FAMILY_ID, SNP_IMAGE_ID);

    /// The identity used by the direct-Linux SNP test image.
    pub(crate) const LINUX_DIRECT: Self = Self::new(*b"OpenVMM SNP test", *b"linux-direct\0\0\0\0");

    const fn new(family_id: [u8; 16], image_id: [u8; 16]) -> Self {
        Self {
            family_id,
            image_id,
        }
    }
}

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
    id_block_signing_payload_with_identity(ld, guest_svn, policy, SnpImageIdentity::OPENHCL)
}

/// Build an SNP ID block signing payload with an explicit image identity.
pub(crate) fn id_block_signing_payload_with_identity(
    ld: &[u8],
    guest_svn: u32,
    policy: u64,
    identity: SnpImageIdentity,
) -> anyhow::Result<Vec<u8>> {
    let ld: [u8; SHA_384_OUTPUT_SIZE_BYTES] =
        ld.try_into().context("SNP launch digest is not 48 bytes")?;
    let id_block = SnpPspIdBlock {
        ld,
        family_id: identity.family_id,
        image_id: identity.image_id,
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
/// * `identity` - Family and image identifiers to embed.
///
/// # Errors
/// Returns an error if the file has no SEV-SNP platform, already contains an
/// SNP ID block, lacks an SNP measurement/guest policy, or if signing fails.
pub(crate) fn add_snp_id_block_temp_key(
    igvm_data: &[u8],
    guest_svn: u32,
    identity: SnpImageIdentity,
) -> anyhow::Result<Vec<u8>> {
    let igvm_file =
        IgvmFile::new_from_binary(igvm_data, None).context("parsing input IGVM file")?;
    let (compatibility_mask, policy) = snp_context(&igvm_file)?;

    let mut serializer = IgvmSerializer::new(&igvm_file).context("constructing IGVM serializer")?;
    check_corim_guest_svn(&serializer, guest_svn)?;
    let ld = snp_measurement(&serializer)?;

    let psp_id_block = SnpPspIdBlock {
        ld,
        family_id: identity.family_id,
        image_id: identity.image_id,
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
    check_corim_guest_svn(&serializer, id_block.guest_svn)?;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IdBlockState {
    Signed,
    Unsigned,
}

pub(crate) struct ExistingIdBlock {
    pub payload: SnpPspIdBlock,
    pub compatibility_mask: u32,
    pub state: IdBlockState,
}

pub(crate) struct ExistingIdentity {
    pub payload: SnpPspIdBlock,
    pub compatibility_mask: u32,
}

/// Validate identity, layout, policy and measurement, independently of ID-key
/// authentication. CoRIM endorses this identity, not its signature or signer.
/// Stages an identity-preserving no-op replacement to reuse the library's
/// ambiguity, policy, and measurement checks.
pub(crate) fn existing_identity(
    serializer: &mut IgvmSerializer<'_>,
) -> anyhow::Result<ExistingIdentity> {
    let file = serializer.file();
    let mask = crate::platform_mask::lookup_compatibility_mask(
        file.platforms(),
        IgvmPlatformType::SEV_SNP,
    )?;
    let policy = guest_policy(file, mask).context("missing SNP GuestPolicy")?;
    let directive = file
        .directives()
        .iter()
        .find(|h| {
            matches!(h, IgvmDirectiveHeader::SnpIdBlock { compatibility_mask, .. }
            if compatibility_mask & mask != 0)
        })
        .context("existing-image signing requires an SNP ID block")?
        .clone();
    serializer
        .replace_snp_id_block(directive.clone(), policy)
        .context("validating existing SNP ID block")?;

    let IgvmDirectiveHeader::SnpIdBlock {
        reserved,
        ld,
        family_id,
        image_id,
        version,
        guest_svn,
        author_key_algorithm,
        author_key_signature,
        author_public_key,
        ..
    } = &directive
    else {
        unreachable!("selected SNP ID block");
    };
    anyhow::ensure!(*version == 1, "unsupported SNP ID block version {version}");
    anyhow::ensure!(
        *reserved == [0; 3]
            && *author_key_algorithm == 0
            && author_key_signature.as_bytes().iter().all(|b| *b == 0)
            && author_public_key.as_bytes().iter().all(|b| *b == 0),
        "existing SNP ID block has nonzero reserved or unused author-key fields"
    );
    let payload = SnpPspIdBlock {
        ld: *ld,
        family_id: *family_id,
        image_id: *image_id,
        version: *version,
        guest_svn: *guest_svn,
        policy,
    };
    Ok(ExistingIdentity {
        payload,
        compatibility_mask: mask,
    })
}

/// Additionally require a valid existing signature or canonical unsigned
/// placeholder for the standalone ID-block signing/replacement workflow.
pub(crate) fn existing_id_block(
    serializer: &mut IgvmSerializer<'_>,
) -> anyhow::Result<ExistingIdBlock> {
    let identity = existing_identity(serializer)?;
    let IgvmDirectiveHeader::SnpIdBlock {
        id_key_algorithm,
        id_key_signature,
        id_public_key,
        ..
    } = serializer
        .file()
        .directives()
        .iter()
        .find(|h| {
            matches!(h, IgvmDirectiveHeader::SnpIdBlock { compatibility_mask, .. }
            if *compatibility_mask == identity.compatibility_mask)
        })
        .context("missing validated SNP ID block")?
    else {
        unreachable!("selected SNP ID block")
    };
    let payload = identity.payload;
    let state = if *id_key_algorithm == 0
        && id_key_signature.as_bytes().iter().all(|b| *b == 0)
        && id_public_key.as_bytes().iter().all(|b| *b == 0)
    {
        IdBlockState::Unsigned
    } else {
        anyhow::ensure!(
            *id_key_algorithm == SNP_ID_KEY_ALGORITHM_ECDSA_P384_SHA384
                && id_public_key.curve == SNP_ECDSA_CURVE_P384
                && id_public_key.reserved == 0,
            "existing SNP ID block has unsupported authentication fields"
        );
        let signature = crypto::ecdsa::EcdsaSignature::from_components(
            crypto::ecdsa::EcdsaCurve::P384,
            &crypto::ecdsa::EcdsaSignatureComponents {
                r: unpadded_be_component(&id_key_signature.r_comp)?,
                s: unpadded_be_component(&id_key_signature.s_comp)?,
            },
        )
        .context("parsing existing SNP ID signature")?;
        let public_key = crypto::ecdsa::EcdsaPublicKeyComponents {
            qx: unpadded_be_component(&id_public_key.qx)?,
            qy: unpadded_be_component(&id_public_key.qy)?,
        };
        let key = crypto::ecdsa::EcdsaPublicKey::from_public_key_bytes(
            crypto::ecdsa::EcdsaCurve::P384,
            &public_key,
        )
        .context("parsing existing SNP ID public key")?;
        anyhow::ensure!(
            key.verify(
                payload.as_bytes(),
                &signature,
                crypto::HashAlgorithm::Sha384
            )
            .context("verifying existing SNP ID signature")?,
            "existing SNP ID block signature does not verify"
        );
        IdBlockState::Signed
    };
    Ok(ExistingIdBlock {
        payload,
        compatibility_mask: identity.compatibility_mask,
        state,
    })
}

/// Export the validated existing identity without adding any CoRIM data.
/// Sign this payload, replace the ID block, and only then prepare the CoRIM;
/// verification must use that ID-signed image as its original baseline.
pub(crate) fn prepare_payload(input: &[u8]) -> anyhow::Result<Vec<u8>> {
    let file = IgvmFile::new_from_binary(input, None).context("parsing input IGVM")?;
    let mut serializer = IgvmSerializer::new(&file).context("measuring input IGVM")?;
    let existing = existing_id_block(&mut serializer)?;
    tracing::info!(state = ?existing.state, "Prepared SNP ID signing payload; signer trust is not checked");
    Ok(existing.payload.as_bytes().to_vec())
}

pub(crate) fn run_prepare(
    input: &std::path::Path,
    output: &std::path::Path,
    dump: bool,
) -> anyhow::Result<()> {
    let bytes = fs_err::read(input).context("reading input IGVM")?;
    let payload = prepare_payload(&bytes)?;
    crate::write_igvm_file_atomic(output, &payload, "writing SNP ID signing payload")?;
    if dump {
        dump_payload(&payload, std::io::stdout().lock())?;
    }
    Ok(())
}

/// Decode the exact exported signing bytes, not the IGVM authentication fields.
fn dump_payload(payload: &[u8], mut output: impl std::io::Write) -> anyhow::Result<()> {
    let block = SnpPspIdBlock::read_from_bytes(payload)
        .map_err(|_| anyhow::anyhow!("invalid SNP ID-block signing payload size"))?;
    let decoded = serde_json::json!({
        "ld": hex::encode(block.ld),
        "family_id": hex::encode(block.family_id),
        "image_id": hex::encode(block.image_id),
        "version": block.version,
        "guest_svn": block.guest_svn,
        "policy": format!("0x{:016x}", block.policy),
    });
    serde_json::to_writer_pretty(&mut output, &decoded).context("writing decoded SNP ID block")?;
    writeln!(output).context("writing decoded SNP ID-block newline")?;
    Ok(())
}

fn unpadded_be_component(
    component: &[u8; SNP_ECC_COMPONENT_SIZE_BYTES],
) -> anyhow::Result<Vec<u8>> {
    let key_size = crypto::ecdsa::EcdsaCurve::P384.key_size();
    anyhow::ensure!(
        component[key_size..].iter().all(|b| *b == 0),
        "SNP P-384 component has nonzero padding"
    );
    let mut be = component[..key_size].to_vec();
    be.reverse();
    Ok(be)
}

/// Apply a verified external signature without changing an existing image's
/// identity. Neither the supplied key nor the original embedded key is trusted
/// merely because its signature verifies.
pub(crate) fn replace_snp_id_block_signed(
    igvm_data: &[u8],
    signing_payload: &[u8],
    signature_der: &[u8],
    public_key: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let file = IgvmFile::new_from_binary(igvm_data, None).context("parsing input IGVM file")?;
    let mut serializer = IgvmSerializer::new(&file).context("constructing IGVM serializer")?;
    let existing = existing_id_block(&mut serializer)?;
    check_corim_guest_svn(&serializer, existing.payload.guest_svn)?;
    anyhow::ensure!(
        signing_payload == existing.payload.as_bytes(),
        "replacement signing payload differs from the existing identity, policy, or launch digest"
    );
    let (signature, key) = signature_and_verify(signing_payload, signature_der, public_key)?;
    let replacement = id_block_directive(
        &existing.payload,
        existing.compatibility_mask,
        signature,
        key,
    );
    serializer
        .replace_snp_id_block(replacement, existing.payload.policy)
        .context("replacing SNP ID block")?;
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
    if igvm_file
        .directives()
        .iter()
        .any(|h| matches!(h, IgvmDirectiveHeader::SnpIdBlock { .. }))
    {
        anyhow::bail!(
            "IGVM file already contains an SNP ID block for compatibility mask \
             0x{compatibility_mask:X}; refusing to add a second one"
        );
    }

    let policy = guest_policy(igvm_file, compatibility_mask)
        .context("missing SNP GuestPolicy initialization header")?;

    Ok((compatibility_mask, policy))
}

/// Require an existing SNP launch endorsement to agree with the new ID block.
/// Decode the document only: signature authentication and signer trust are
/// independent of extracting the endorsed SVN. Reject unknown/ambiguous shapes
/// rather than searching arbitrary measurements for an SVN-like value.
fn check_corim_guest_svn(serializer: &IgvmSerializer<'_>, guest_svn: u32) -> anyhow::Result<()> {
    use corim::types::comid::ComidTag;
    use corim::types::corim::ConciseTagChoice;
    use corim::types::corim::CorimMap;
    use corim::types::measurement::SvnChoice;

    let platform = IgvmPlatformType::SEV_SNP;
    let mask =
        crate::platform_mask::lookup_compatibility_mask(serializer.file().platforms(), platform)?;
    let mut documents =
        serializer
            .file()
            .initializations()
            .iter()
            .filter_map(|header| match header {
                IgvmInitializationHeader::CorimDocument {
                    compatibility_mask, ..
                } if compatibility_mask & mask != 0 => Some(*compatibility_mask),
                _ => None,
            });
    let Some(document_mask) = documents.next() else {
        anyhow::ensure!(
            !serializer
                .file()
                .initializations()
                .iter()
                .any(|header| matches!(header,
                IgvmInitializationHeader::CorimSignature { compatibility_mask, .. }
                if compatibility_mask & mask != 0)),
            "SNP CoRIM signature has no document from which to extract a guest SVN"
        );
        return Ok(());
    };
    anyhow::ensure!(
        document_mask == mask && documents.next().is_none(),
        "ambiguous SNP CoRIM documents or mismatched compatibility mask"
    );
    let document = serializer
        .corim_for(platform)
        .context("missing SNP CoRIM document")?;
    anyhow::ensure!(
        document.len() <= corim::validate::MAX_PAYLOAD_SIZE,
        "SNP CoRIM document is too large"
    );
    let tagged: corim::cbor::value::Tagged<CorimMap> =
        corim::cbor::decode(document).context("decoding SNP CoRIM document")?;
    anyhow::ensure!(
        tagged.tag == corim::types::tags::TAG_CORIM,
        "invalid SNP CoRIM tag"
    );
    let [ConciseTagChoice::Comid(bytes)] = tagged.value.tags.as_slice() else {
        anyhow::bail!("SNP CoRIM must contain exactly one launch-measurement CoMID");
    };
    let comid: ComidTag = corim::cbor::decode(bytes).context("decoding SNP launch CoMID")?;
    let series = comid
        .triples
        .conditional_endorsement_series
        .as_deref()
        .unwrap_or_default();
    let [series] = series else {
        anyhow::bail!("SNP CoRIM must contain exactly one conditional endorsement series");
    };
    let [record] = series.1.as_slice() else {
        anyhow::bail!("SNP CoRIM must contain exactly one conditional endorsement record");
    };
    let [addition] = record.1.as_slice() else {
        anyhow::bail!("SNP CoRIM must endorse exactly one guest SVN");
    };
    let Some(SvnChoice::ExactValue(svn)) = addition.mval.svn else {
        anyhow::bail!("SNP CoRIM must endorse an exact guest SVN");
    };
    let svn = u32::try_from(svn).context("SNP CoRIM guest SVN exceeds u32")?;

    // Compare the entire triple structure against the serializer's launch
    // profile, including the reference digest, CES environment and selection.
    // This also rejects extra SVN claims in unrelated measurements/triples.
    let mut expected = IgvmSerializer::new(serializer.file())?;
    let template = crate::build_endorsement_corim(platform, svn)?;
    let bytes = expected.add_corim(platform, template.build())?;
    let (expected_corim, expected_comids) = corim::validate::decode_and_validate_at(bytes, 0)?;
    anyhow::ensure!(
        tagged.value.id == expected_corim.id
            && tagged.value.profile == expected_corim.profile
            && expected_comids.len() == 1
            && comid.triples == expected_comids[0].triples,
        "SNP CoRIM does not have the expected launch-measurement endorsement structure"
    );
    anyhow::ensure!(
        svn == guest_svn,
        "SNP ID-block guest SVN ({guest_svn}) differs from existing SNP CoRIM guest SVN ({svn})"
    );
    Ok(())
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
    let sig =
        crypto::ecdsa::EcdsaSignature::from_der(crypto::ecdsa::EcdsaCurve::P384, signature_der)
            .context("parsing DER-encoded ECDSA signature")?;
    let public_key = parse_p384_public_key(public_key_pem)?;

    // Verify the signature over the SHA-384 of the signed content.
    let valid = public_key
        .verify(signed_bytes, &sig, crypto::HashAlgorithm::Sha384)
        .context("verifying SNP ID block signature")?;
    anyhow::ensure!(
        valid,
        "SNP ID block signature does not verify against the supplied public key; \
         the signature, public key, or signing payload do not correspond"
    );

    // Export the verified key's coordinates (big-endian, 48 bytes each) for the
    // PSP ID block public-key structure.
    let coordinates = public_key
        .public_key_bytes()
        .context("exporting SNP ID block public key")?;
    let signature = sig
        .to_components()
        .context("exporting SNP ID block signature")?;

    Ok((
        IGVM_VHS_SNP_ID_BLOCK_SIGNATURE {
            r_comp: padded_le_component(&signature.r),
            s_comp: padded_le_component(&signature.s),
        },
        IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY {
            curve: SNP_ECDSA_CURVE_P384,
            reserved: 0,
            qx: padded_le_component(&coordinates.qx),
            qy: padded_le_component(&coordinates.qy),
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
    // internally. Export big-endian r and s, each 48 bytes for P-384.
    let signature = key
        .sign(id_block.as_bytes(), crypto::HashAlgorithm::Sha384)
        .context("signing SNP ID block")?
        .to_components()
        .context("exporting temporary SNP signature")?;

    let id_key_signature = IGVM_VHS_SNP_ID_BLOCK_SIGNATURE {
        r_comp: padded_le_component(&signature.r),
        s_comp: padded_le_component(&signature.s),
    };

    tracing::info!("Signature R Base64: {}", b64.encode(&signature.r));
    tracing::info!("Signature S Base64: {}", b64.encode(&signature.s));

    // Export the public key's big-endian coordinates, each 48 bytes for P-384.
    let public_key = key
        .public_key_bytes()
        .context("exporting temporary SNP public key")?;

    tracing::info!("Public Key Qx Base64: {}", b64.encode(&public_key.qx));
    tracing::info!("Public Key Qy Base64: {}", b64.encode(&public_key.qy));
    let id_public_key = IGVM_VHS_SNP_ID_BLOCK_PUBLIC_KEY {
        curve: SNP_ECDSA_CURVE_P384,
        reserved: 0,
        qx: padded_le_component(&public_key.qx),
        qy: padded_le_component(&public_key.qy),
    };

    Ok((id_key_signature, id_public_key))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use der::Decode;
    use igvm::IgvmPlatformHeader;
    use igvm::IgvmRevision;
    use igvm_defs::IGVM_VHS_SUPPORTED_PLATFORM;
    use igvm_defs::IgvmPageDataFlags;
    use igvm_defs::IgvmPageDataType;
    use test_with_tracing::test;

    /// Build a minimal, measurable SNP IGVM file: one SEV-SNP platform, a
    /// matching `GuestPolicy`, and a single measured page.
    pub(crate) fn build_snp_igvm(mask: u32) -> Vec<u8> {
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
    pub(crate) fn sign_payload(signing_payload: &[u8]) -> (Vec<u8>, Vec<u8>) {
        use crypto::ecdsa::EcdsaCurve;
        use crypto::ecdsa::EcdsaKeyPair;

        let key = EcdsaKeyPair::generate(EcdsaCurve::P384).unwrap();
        let signature = key
            .sign(signing_payload, crypto::HashAlgorithm::Sha384)
            .unwrap();
        (
            signature.to_der().unwrap(),
            key.to_public_key_der().unwrap(),
        )
    }

    pub(crate) fn build_existing_snp_igvm(signed: bool) -> Vec<u8> {
        let bytes = build_snp_igvm(4);
        let file = IgvmFile::new_from_binary(&bytes, None).unwrap();
        let mut serializer = IgvmSerializer::new(&file).unwrap();
        let payload = SnpPspIdBlock {
            ld: snp_measurement(&serializer).unwrap(),
            family_id: [0x12; 16],
            image_id: [0x34; 16],
            version: 1,
            guest_svn: 0x1234,
            policy: 0x30000,
        };
        let (signature, key) = sign_id_block_with_temp_key(&payload).unwrap();
        let mut block = id_block_directive(&payload, 4, signature, key);
        if !signed {
            clear_authentication(&mut block);
        }
        serializer.add_directive(block);
        let mut bytes = Vec::new();
        serializer.serialize(&mut bytes).unwrap();
        bytes
    }

    fn clear_authentication(block: &mut IgvmDirectiveHeader) {
        if let IgvmDirectiveHeader::SnpIdBlock {
            id_key_algorithm,
            id_key_signature,
            id_public_key,
            ..
        } = block
        {
            *id_key_algorithm = 0;
            id_key_signature.as_mut_bytes().fill(0);
            id_public_key.as_mut_bytes().fill(0);
        }
    }

    pub(crate) fn signing_payload_from_image(bytes: &[u8]) -> Vec<u8> {
        let file = IgvmFile::new_from_binary(bytes, None).unwrap();
        existing_id_block(&mut IgvmSerializer::new(&file).unwrap())
            .unwrap()
            .payload
            .as_bytes()
            .to_vec()
    }

    pub(crate) fn edit_image(
        input: &[u8],
        edit: impl FnOnce(&mut Vec<IgvmInitializationHeader>, &mut Vec<IgvmDirectiveHeader>),
    ) -> Vec<u8> {
        let file = IgvmFile::new_from_binary(input, None).unwrap();
        let mut init = file.initializations().to_vec();
        let mut directives = file.directives().to_vec();
        edit(&mut init, &mut directives);
        let file = IgvmFile::new(
            IgvmRevision::V1,
            file.platforms().to_vec(),
            init,
            directives,
        )
        .unwrap();
        let mut bytes = Vec::new();
        file.serialize(&mut bytes).unwrap();
        bytes
    }

    pub(crate) fn with_snp_corim(input: &[u8], svn: u32) -> Vec<u8> {
        let file = IgvmFile::new_from_binary(input, None).unwrap();
        let mut serializer = IgvmSerializer::new(&file).unwrap();
        serializer
            .add_corim(
                IgvmPlatformType::SEV_SNP,
                crate::build_endorsement_corim(IgvmPlatformType::SEV_SNP, svn)
                    .unwrap()
                    .build(),
            )
            .unwrap();
        let mut output = Vec::new();
        serializer.serialize(&mut output).unwrap();
        output
    }

    #[test]
    fn id_block_creation_and_replacement_require_matching_corim_svn() {
        for existing in [false, true] {
            let original = if existing {
                build_existing_snp_igvm(false)
            } else {
                build_snp_igvm(4)
            };
            let payload = if existing {
                signing_payload_from_image(&original)
            } else {
                let file = IgvmFile::new_from_binary(&original, None).unwrap();
                id_block_signing_payload(
                    &snp_measurement(&IgvmSerializer::new(&file).unwrap()).unwrap(),
                    0x1234,
                    0x30000,
                )
                .unwrap()
            };
            let (signature, key) = sign_payload(&payload);
            for svn in [0x1234, 0x1235] {
                let input = with_snp_corim(&original, svn);
                // SVN consistency is independent of CoRIM signature validity.
                let input = edit_image(&input, |init, _| {
                    init.push(IgvmInitializationHeader::CorimSignature {
                        compatibility_mask: 4,
                        signature: b"not an authenticated signature".to_vec(),
                    })
                });
                let result = if existing {
                    replace_snp_id_block_signed(&input, &payload, &signature, &key)
                } else {
                    add_snp_id_block_signed(&input, &payload, &signature, &key)
                };
                if svn == 0x1234 {
                    let output = result.unwrap();
                    assert_eq!(signing_payload_from_image(&output), payload);
                    assert_eq!(
                        IgvmFile::new_from_binary(&input, None)
                            .unwrap()
                            .initializations(),
                        IgvmFile::new_from_binary(&output, None)
                            .unwrap()
                            .initializations()
                    );
                } else {
                    assert!(
                        format!("{:#}", result.unwrap_err())
                            .contains("differs from existing SNP CoRIM guest SVN")
                    );
                }
                if !existing {
                    for identity in [SnpImageIdentity::OPENHCL, SnpImageIdentity::LINUX_DIRECT] {
                        let result = add_snp_id_block_temp_key(&input, 0x1234, identity);
                        assert_eq!(result.is_ok(), svn == 0x1234);
                        if let Ok(output) = result {
                            assert_eq!(
                                IgvmFile::new_from_binary(&input, None)
                                    .unwrap()
                                    .initializations(),
                                IgvmFile::new_from_binary(&output, None)
                                    .unwrap()
                                    .initializations()
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn id_block_mutations_reject_missing_malformed_and_ambiguous_corim_svn() {
        use corim::types::comid::ComidTag;
        use corim::types::corim::ConciseTagChoice;
        use corim::types::corim::CorimMap;
        use corim::types::measurement::SvnChoice;

        for existing in [false, true] {
            let original = if existing {
                build_existing_snp_igvm(false)
            } else {
                build_snp_igvm(4)
            };
            let payload = if existing {
                signing_payload_from_image(&original)
            } else {
                let file = IgvmFile::new_from_binary(&original, None).unwrap();
                id_block_signing_payload(
                    &snp_measurement(&IgvmSerializer::new(&file).unwrap()).unwrap(),
                    0x1234,
                    0x30000,
                )
                .unwrap()
            };
            let (signature, key) = sign_payload(&payload);
            let input = with_snp_corim(&original, 0x1234);
            for change in [
                "no-svn",
                "min-svn",
                "overflow",
                "no-series",
                "duplicate-series",
                "no-record",
                "duplicate-record",
                "no-addition",
                "duplicate-addition",
                "reference-svn",
                "selection",
                "condition",
                "duplicate-comid",
                "wrong-platform",
                "mask",
                "malformed",
            ] {
                let changed = edit_image(&input, |init, _| {
                    let index = init
                        .iter()
                        .position(|h| matches!(h, IgvmInitializationHeader::CorimDocument { .. }))
                        .unwrap();
                    let IgvmInitializationHeader::CorimDocument {
                        document,
                        compatibility_mask,
                    } = &mut init[index]
                    else {
                        unreachable!()
                    };
                    if change == "mask" {
                        *compatibility_mask |= 8;
                        return;
                    }
                    if change == "malformed" {
                        *document = vec![0xff];
                        return;
                    }
                    let mut tagged: corim::cbor::value::Tagged<CorimMap> =
                        corim::cbor::decode(document).unwrap();
                    let ConciseTagChoice::Comid(bytes) = &tagged.value.tags[0] else {
                        unreachable!()
                    };
                    let mut comid: ComidTag = corim::cbor::decode(bytes).unwrap();
                    let series = comid
                        .triples
                        .conditional_endorsement_series
                        .as_mut()
                        .unwrap();
                    match change {
                        "no-svn" => {
                            // Keep mval nonempty so this is valid CBOR/CoMID,
                            // but not an endorsement of any guest SVN.
                            series[0].1[0].1[0].mval.svn = None;
                            series[0].1[0].1[0].mval.digests =
                                series[0].1[0].0[0].mval.digests.clone();
                        }
                        "min-svn" => {
                            series[0].1[0].1[0].mval.svn = Some(SvnChoice::MinValue(0x1234))
                        }
                        "overflow" => {
                            series[0].1[0].1[0].mval.svn =
                                Some(SvnChoice::ExactValue(u64::from(u32::MAX) + 1))
                        }
                        "no-series" => series.clear(),
                        "duplicate-series" => series.push(series[0].clone()),
                        "no-record" => series[0].1.clear(),
                        "duplicate-record" => {
                            let record = series[0].1[0].clone();
                            series[0].1.push(record);
                        }
                        "no-addition" => series[0].1[0].1.clear(),
                        "duplicate-addition" => {
                            let addition = series[0].1[0].1[0].clone();
                            series[0].1[0].1.push(addition);
                        }
                        "reference-svn" => {
                            comid.triples.reference_triples.as_mut().unwrap()[0].1[0]
                                .mval
                                .svn = Some(SvnChoice::ExactValue(0x1234))
                        }
                        "selection" => {
                            series[0].1[0].0[0].mval.digests.as_mut().unwrap()[0].1[0] ^= 1
                        }
                        "condition" => {
                            series[0].0.environment.class.as_mut().unwrap().model =
                                Some("TDX".into())
                        }
                        "duplicate-comid" => {}
                        "wrong-platform" => {
                            tagged.value.id = corim::types::corim::CorimId::Text(
                                "Intel/TDX/launch-measurement".into(),
                            )
                        }
                        _ => unreachable!(),
                    }
                    tagged.value.tags[0] =
                        ConciseTagChoice::Comid(corim::cbor::encode(&comid).unwrap());
                    if change == "duplicate-comid" {
                        tagged.value.tags.push(tagged.value.tags[0].clone());
                    }
                    *document = corim::cbor::encode(&tagged).unwrap();
                });
                let result = if existing {
                    replace_snp_id_block_signed(&changed, &payload, &signature, &key)
                } else {
                    add_snp_id_block_signed(&changed, &payload, &signature, &key)
                };
                let error = format!("{:#}", result.unwrap_err());
                assert!(
                    error.contains("CoRIM") || error.contains("CoMID"),
                    "{change}: {error}"
                );
                if !existing {
                    assert!(
                        add_snp_id_block_temp_key(&changed, 0x1234, SnpImageIdentity::OPENHCL)
                            .is_err(),
                        "{change}"
                    );
                }
            }
        }
    }

    #[test]
    fn id_block_mutations_preserve_other_platform_endorsements() {
        for existing in [false, true] {
            let original = if existing {
                build_existing_snp_igvm(false)
            } else {
                build_snp_igvm(4)
            };
            let snp = IgvmFile::new_from_binary(&original, None).unwrap();
            let mut platforms = snp.platforms().to_vec();
            let IgvmPlatformHeader::SupportedPlatform(mut vbs) = platforms[0];
            vbs.compatibility_mask = 1;
            vbs.platform_type = IgvmPlatformType::VSM_ISOLATION;
            platforms.push(IgvmPlatformHeader::SupportedPlatform(vbs));
            let mut directives = snp.directives().to_vec();
            let mut page = directives[0].clone();
            let IgvmDirectiveHeader::PageData {
                compatibility_mask, ..
            } = &mut page
            else {
                unreachable!()
            };
            *compatibility_mask = 1;
            directives.push(page);
            let file = IgvmFile::new(
                IgvmRevision::V1,
                platforms,
                snp.initializations().to_vec(),
                directives,
            )
            .unwrap();
            let mut serializer = IgvmSerializer::new(&file).unwrap();
            serializer
                .add_corim(
                    IgvmPlatformType::VSM_ISOLATION,
                    crate::build_endorsement_corim(IgvmPlatformType::VSM_ISOLATION, 99)
                        .unwrap()
                        .build(),
                )
                .unwrap();
            let mut input = Vec::new();
            serializer.serialize(&mut input).unwrap();
            // A different platform's SVN (and signature) must not constrain SNP.
            let input = edit_image(&input, |init, _| {
                init.push(IgvmInitializationHeader::CorimSignature {
                    compatibility_mask: 1,
                    signature: b"other platform signature".to_vec(),
                })
            });
            for input in [input.clone(), with_snp_corim(&input, 0x1234)] {
                let file = IgvmFile::new_from_binary(&input, None).unwrap();
                let serializer = IgvmSerializer::new(&file).unwrap();
                let payload = if existing {
                    signing_payload_from_image(&input)
                } else {
                    id_block_signing_payload(
                        &snp_measurement(&serializer).unwrap(),
                        0x1234,
                        0x30000,
                    )
                    .unwrap()
                };
                let (signature, key) = sign_payload(&payload);
                let output = if existing {
                    replace_snp_id_block_signed(&input, &payload, &signature, &key).unwrap()
                } else {
                    add_snp_id_block_signed(&input, &payload, &signature, &key).unwrap()
                };
                let mut outputs = vec![output];
                if !existing {
                    outputs.push(
                        add_snp_id_block_temp_key(&input, 0x1234, SnpImageIdentity::LINUX_DIRECT)
                            .unwrap(),
                    );
                }
                for output in outputs {
                    let after = IgvmFile::new_from_binary(&output, None).unwrap();
                    assert_eq!(file.initializations(), after.initializations());
                    assert_eq!(file.platforms(), after.platforms());
                    let after_serializer = IgvmSerializer::new(&after).unwrap();
                    for measurement in serializer.measurements() {
                        assert_eq!(
                            measurement.digest,
                            after_serializer
                                .measurement_for(measurement.platform)
                                .unwrap()
                                .digest
                        );
                    }
                    assert_eq!(file.directives()[0], after.directives()[0]);
                    assert_eq!(
                        file.directives().last(),
                        if existing {
                            after.directives().last()
                        } else {
                            after.directives().get(file.directives().len() - 1)
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn existing_id_block_validates_signed_and_unsigned_inputs() {
        for signed in [false, true] {
            let input = build_existing_snp_igvm(signed);
            let file = IgvmFile::new_from_binary(&input, None).unwrap();
            let mut serializer = IgvmSerializer::new(&file).unwrap();
            let existing = existing_id_block(&mut serializer).unwrap();
            assert_eq!(
                existing.state,
                if signed {
                    IdBlockState::Signed
                } else {
                    IdBlockState::Unsigned
                }
            );
            assert_eq!(existing.compatibility_mask, 4);
            assert_eq!(existing.payload.family_id, [0x12; 16]);
            assert_eq!(existing.payload.image_id, [0x34; 16]);
            assert_eq!(existing.payload.guest_svn, 0x1234);
            assert_eq!(existing.payload.as_bytes().len(), 96);
            assert_eq!(
                prepare_payload(&input).unwrap(),
                existing.payload.as_bytes()
            );
        }
    }

    #[test]
    fn decoded_payload_matches_exported_signing_fields() {
        for signed in [false, true] {
            let input = build_existing_snp_igvm(signed);
            let payload = prepare_payload(&input).unwrap();
            let mut output = Vec::new();
            dump_payload(&payload, &mut output).unwrap();
            assert!(output.ends_with(b"\n"));
            let decoded: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(
                decoded,
                serde_json::json!({
                    "ld": hex::encode(&payload[..48]),
                    "family_id": "12".repeat(16),
                    "image_id": "34".repeat(16),
                    "version": 1,
                    "guest_svn": 0x1234,
                    "policy": "0x0000000000030000",
                })
            );
        }
    }

    #[test]
    fn decoded_payload_rejects_bad_size_and_reports_output_failure() {
        let payload = prepare_payload(&build_existing_snp_igvm(false)).unwrap();
        let mut output = Vec::new();
        assert!(dump_payload(&payload[..payload.len() - 1], &mut output).is_err());
        assert!(output.is_empty());
        assert!(dump_payload(&payload, &mut [][..]).is_err());
    }

    #[test]
    fn standalone_preparation_exports_only_payload_and_is_atomic_on_failure() {
        let root = tempfile::tempdir().unwrap();
        let input_path = root.path().join("input.igvm");
        let output = root.path().join("payload.idblock");
        for signed in [false, true] {
            let input = build_existing_snp_igvm(signed);
            fs_err::write(&input_path, &input).unwrap();
            fs_err::write(&output, b"old payload").unwrap();
            run_prepare(&input_path, &output, false).unwrap();
            let without_dump = fs_err::read(&output).unwrap();
            run_prepare(&input_path, &output, true).unwrap();
            assert_eq!(fs_err::read(&output).unwrap(), without_dump);
            assert_eq!(
                fs_err::read(&output).unwrap(),
                signing_payload_from_image(&input)
            );
            assert_eq!(fs_err::read(&input_path).unwrap(), input);
            assert_eq!(fs_err::read_dir(root.path()).unwrap().count(), 2);
        }
        let good = fs_err::read(&output).unwrap();
        fs_err::write(&input_path, b"not an IGVM").unwrap();
        assert!(run_prepare(&input_path, &output, false).is_err());
        assert_eq!(fs_err::read(&output).unwrap(), good);
        let absent = root.path().join("absent");
        assert!(run_prepare(&input_path, &absent, true).is_err());
        assert!(!absent.exists());
        fs_err::write(&input_path, build_existing_snp_igvm(false)).unwrap();
        let directory = root.path().join("directory");
        fs_err::create_dir(&directory).unwrap();
        assert!(run_prepare(&input_path, &directory, true).is_err());
        assert!(directory.is_dir());
        assert!(run_prepare(&input_path, &root.path().join("missing/child"), false).is_err());
    }

    #[test]
    fn standalone_preparation_rejects_missing_stale_and_ambiguous_identity() {
        assert!(prepare_payload(b"not an IGVM").is_err());
        assert!(prepare_payload(&build_snp_igvm(4)).is_err());
        let input = build_existing_snp_igvm(true);
        for change in [
            "no-policy",
            "duplicate-policy",
            "duplicate-block",
            "measurement",
            "mask",
        ] {
            let changed = edit_image(&input, |init, directives| match change {
                "no-policy" => init.clear(),
                "duplicate-policy" => init.push(init[0].clone()),
                "duplicate-block" => directives.push(directives.last().unwrap().clone()),
                "measurement" => {
                    let IgvmDirectiveHeader::PageData { data, .. } = &mut directives[0] else {
                        unreachable!()
                    };
                    data[0] ^= 1;
                }
                "mask" => {
                    let IgvmDirectiveHeader::SnpIdBlock {
                        compatibility_mask, ..
                    } = directives.last_mut().unwrap()
                    else {
                        unreachable!()
                    };
                    *compatibility_mask |= 8;
                }
                _ => unreachable!(),
            });
            assert!(prepare_payload(&changed).is_err(), "{change}");
        }
    }

    #[test]
    fn existing_id_block_rejects_malformed_authentication_and_identity() {
        let input = build_existing_snp_igvm(true);
        for field in [
            "signature",
            "signature_padding",
            "key_padding",
            "algorithm",
            "curve",
            "key_reserved",
            "version",
            "reserved",
            "author",
            "author_algorithm",
            "author_signature",
            "author_key",
            "partial_unsigned",
            "ld",
        ] {
            let changed = edit_image(&input, |_, directives| {
                let IgvmDirectiveHeader::SnpIdBlock {
                    id_key_signature,
                    id_public_key,
                    id_key_algorithm,
                    version,
                    reserved,
                    author_key_enabled,
                    author_key_algorithm,
                    author_key_signature,
                    author_public_key,
                    ld,
                    ..
                } = directives.last_mut().unwrap()
                else {
                    unreachable!()
                };
                match field {
                    "signature" => id_key_signature.r_comp[0] ^= 1,
                    "signature_padding" => id_key_signature.s_comp[48] = 1,
                    "key_padding" => id_public_key.qx[48] = 1,
                    "algorithm" => *id_key_algorithm = 2,
                    "curve" => id_public_key.curve = 1,
                    "key_reserved" => id_public_key.reserved = 1,
                    "version" => *version = 2,
                    "reserved" => reserved[0] = 1,
                    "author" => *author_key_enabled = 1,
                    "author_algorithm" => *author_key_algorithm = 1,
                    "author_signature" => author_key_signature.r_comp[0] = 1,
                    "author_key" => author_public_key.qx[0] = 1,
                    "partial_unsigned" => *id_key_algorithm = 0,
                    "ld" => ld[0] ^= 1,
                    _ => unreachable!(),
                }
            });
            let file = IgvmFile::new_from_binary(&changed, None).unwrap();
            assert!(
                existing_id_block(&mut IgvmSerializer::new(&file).unwrap()).is_err(),
                "{field}"
            );
            assert!(prepare_payload(&changed).is_err(), "{field}");
        }
    }

    #[test]
    fn replacement_preserves_identity_and_rejects_stale_signed_payloads() {
        for signed in [false, true] {
            let input = build_existing_snp_igvm(signed);
            let payload = signing_payload_from_image(&input);
            let (signature, key) = sign_payload(&payload);
            let output = replace_snp_id_block_signed(&input, &payload, &signature, &key).unwrap();
            assert_eq!(signing_payload_from_image(&output), payload);
            assert_eq!(count_id_blocks(&output), 1);
            let file = IgvmFile::new_from_binary(&output, None).unwrap();
            assert_eq!(
                existing_id_block(&mut IgvmSerializer::new(&file).unwrap())
                    .unwrap()
                    .state,
                IdBlockState::Signed
            );
            assert!(add_snp_id_block_signed(&input, &payload, &signature, &key).is_err());
            assert!(replace_snp_id_block_signed(&input, &payload, b"bad signature", &key).is_err());
            let (_, other_key) = sign_payload(&payload);
            assert!(replace_snp_id_block_signed(&input, &payload, &signature, &other_key).is_err());
            for offset in [0, 48, 64, 80, 84, 88] {
                let mut stale = payload.clone();
                stale[offset] ^= 1;
                let (signature, key) = sign_payload(&stale);
                assert!(
                    replace_snp_id_block_signed(&input, &stale, &signature, &key).is_err(),
                    "changed payload at {offset}"
                );
            }
        }
    }

    /// Pin the SNP ID block identity constants to their exact byte values.
    /// These are baked into externally-consumed SNP ID blocks, so any change
    /// must be a deliberate, reviewed edit.
    #[test]
    fn snp_id_block_constants_byte_identity() {
        assert_eq!(
            SNP_FAMILY_ID,
            [
                0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00,
            ]
        );
        assert_eq!(SNP_IMAGE_ID, *b"openhcl\0\0\0\0\0\0\0\0\0");
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
        let key_size = crypto::ecdsa::EcdsaCurve::P384.key_size();
        assert!(sig.r_comp[key_size..].iter().all(|&b| b == 0));
        assert!(pubkey.qx[key_size..].iter().all(|&b| b == 0));
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

        let out = add_snp_id_block_temp_key(&igvm_data, 7, SnpImageIdentity::LINUX_DIRECT)
            .expect("add SNP ID block");
        assert_eq!(count_id_blocks(&out), 1);

        let parsed = IgvmFile::new_from_binary(&out, None).expect("valid output");
        let id_block = parsed
            .directives()
            .iter()
            .find_map(|h| match h {
                IgvmDirectiveHeader::SnpIdBlock {
                    ld,
                    family_id,
                    image_id,
                    guest_svn,
                    ..
                } => Some((*ld, *family_id, *image_id, *guest_svn)),
                _ => None,
            })
            .expect("id block present");
        assert_eq!(id_block.0.as_slice(), ref_ld.as_slice());
        assert_eq!(id_block.1, SnpImageIdentity::LINUX_DIRECT.family_id);
        assert_eq!(id_block.2, SnpImageIdentity::LINUX_DIRECT.image_id);
        assert_eq!(id_block.3, 7);

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
        let once =
            add_snp_id_block_temp_key(&igvm_data, 1, SnpImageIdentity::OPENHCL).expect("first add");
        let err = add_snp_id_block_temp_key(&once, 1, SnpImageIdentity::OPENHCL).unwrap_err();
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

        let err = add_snp_id_block_temp_key(&data, 1, SnpImageIdentity::OPENHCL).unwrap_err();
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
        use crypto::x509::X509Certificate;
        use crypto::x509::X509KeyPair;

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
        let sig_der = key
            .sign(&signing_payload, crypto::HashAlgorithm::Sha384)
            .unwrap()
            .to_der()
            .unwrap();
        let cert_der =
            X509Certificate::build_self_signed(X509KeyPair::Ecdsa(&key), &Default::default())
                .unwrap()
                .to_der()
                .unwrap();
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
