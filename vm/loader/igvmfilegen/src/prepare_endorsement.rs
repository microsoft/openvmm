// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Prepare one platform's CoRIM without rebuilding or authenticating its ID block.
//!
//! An existing SNP ID block supplies the SVN; without one, an explicit SVN is
//! required. CoRIM operations never change ID blocks. Verification must use the
//! same ID-block state as the preparation baseline.

use crate::snp_id_block;
use anyhow::Context;
use igvm::IgvmFile;
use igvm::IgvmInitializationHeader;
use igvm::IgvmSerializer;
use igvm_defs::IgvmPlatformType;
use serde::Serialize;
use std::io::Write;
use std::path::Path;

struct PreparedEndorsement {
    image: Vec<u8>,
    corim: Vec<u8>,
    platform: IgvmPlatformType,
    compatibility_mask: u32,
    guest_svn: u32,
    launch_digest: Vec<u8>,
}

fn prepare(
    input: &[u8],
    platform: IgvmPlatformType,
    guest_svn: Option<u32>,
) -> anyhow::Result<PreparedEndorsement> {
    let file = IgvmFile::new_from_binary(input, None).context("parsing input IGVM")?;
    let mut serializer = IgvmSerializer::new(&file).context("measuring input IGVM")?;
    let mask = crate::platform_mask::lookup_compatibility_mask(file.platforms(), platform)?;
    let svn =
        match platform {
            IgvmPlatformType::SEV_SNP
                if file.directives().iter().any(|header| {
                    matches!(header, igvm::IgvmDirectiveHeader::SnpIdBlock { .. })
                }) =>
            {
                let identity = snp_id_block::existing_identity(&mut serializer)?;
                anyhow::ensure!(
                    guest_svn.is_none_or(|svn| svn == identity.payload.guest_svn),
                    "--guest-svn must match the existing SNP ID-block SVN ({})",
                    identity.payload.guest_svn
                );
                identity.payload.guest_svn
            }
            IgvmPlatformType::SEV_SNP => guest_svn
                .context("--guest-svn is required for SNP endorsements without an ID block")?,
            IgvmPlatformType::TDX | IgvmPlatformType::VSM_ISOLATION => {
                guest_svn.context("--guest-svn is required for TDX/VBS endorsements")?
            }
            _ => anyhow::bail!("unsupported endorsement platform {platform:?}"),
        };
    anyhow::ensure!(
        !file.initializations().iter().any(|header| {
            matches!(header,
                IgvmInitializationHeader::CorimDocument { compatibility_mask, .. }
                | IgvmInitializationHeader::CorimSignature { compatibility_mask, .. }
                if compatibility_mask & mask != 0)
        }),
        "input already contains CoRIM data for the selected platform; prepare the original image instead"
    );
    let digest = serializer
        .measurement_for(platform)
        .context("input has no measurement for the selected platform")?
        .digest
        .clone();
    let template = crate::build_endorsement_corim(platform, svn)?;
    let corim = serializer
        .add_corim(platform, template.build())
        .context("embedding CoRIM")?
        .to_vec();
    let mut image = Vec::new();
    serializer
        .serialize(&mut image)
        .context("serializing prepared IGVM")?;
    let parsed = IgvmFile::new_from_binary(&image, None).context("reparsing prepared IGVM")?;
    let verified = IgvmSerializer::new(&parsed).context("remeasuring prepared IGVM")?;
    anyhow::ensure!(
        parsed.platforms() == file.platforms()
            && parsed.directives() == file.directives()
            && parsed.initializations().starts_with(file.initializations())
            && parsed.initializations().len() == file.initializations().len() + 1
            && verified
                .measurement_for(platform)
                .context("prepared image lost selected measurement")?
                .digest
                == digest,
        "preparation changed image contents or launch measurement"
    );
    anyhow::ensure!(
        verified.corim_for(platform) == Some(corim.as_slice()),
        "embedded CoRIM differs from signing input"
    );
    Ok(PreparedEndorsement {
        image,
        corim,
        platform,
        compatibility_mask: mask,
        guest_svn: svn,
        launch_digest: digest,
    })
}

#[derive(Serialize)]
struct Artifact<'a> {
    file: &'a str,
    size: usize,
    sha256: String,
}

impl<'a> Artifact<'a> {
    fn new(file: &'a str, bytes: &[u8]) -> Self {
        Self {
            file,
            size: bytes.len(),
            sha256: sha256(bytes),
        }
    }
}

#[derive(Serialize)]
struct PreparationRecord<'a> {
    schema_version: u32,
    tool_sha256: &'a str,
    input_sha256: String,
    platform: String,
    compatibility_mask: u32,
    release_ready: bool,
    signer_trust: &'static str,
    id_authentication: &'static str,
    launch_digest: String,
    guest_svn: u32,
    artifacts: [Artifact<'static>; 2],
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(crypto::sha_256::sha_256(bytes))
}

fn publish(
    output: &Path,
    input: &[u8],
    prepared: &PreparedEndorsement,
    tool_sha256: &str,
) -> anyhow::Result<()> {
    let files = [
        ("image.igvm", prepared.image.as_slice()),
        ("corim.cbor", prepared.corim.as_slice()),
    ];
    let record = PreparationRecord {
        schema_version: 2,
        tool_sha256,
        input_sha256: sha256(input),
        platform: crate::platform_mask::isolation_label(prepared.platform),
        compatibility_mask: prepared.compatibility_mask,
        release_ready: false,
        signer_trust: "not-checked",
        id_authentication: "not-checked",
        launch_digest: hex::encode(&prepared.launch_digest),
        guest_svn: prepared.guest_svn,
        artifacts: files.map(|(name, bytes)| Artifact::new(name, bytes)),
    };
    let record = serde_json::to_vec_pretty(&record).context("encoding preparation manifest")?;
    fs_err::create_dir(output).context("creating new output directory (must not already exist)")?;
    // The manifest is the commit marker. Consumers must not use a directory
    // without it, and must verify all listed hashes before requesting signatures.
    for (name, bytes) in files.into_iter().chain(std::iter::once((
        "endorsement-inputs.json",
        record.as_slice(),
    ))) {
        let write = || -> anyhow::Result<()> {
            let mut file = tempfile::NamedTempFile::new_in(output)?;
            file.write_all(bytes)?;
            file.as_file().sync_all()?;
            file.persist_noclobber(output.join(name))?;
            Ok(())
        };
        write().with_context(|| format!(
            "publishing {name} in {}; preparation is incomplete without endorsement-inputs.json; retry in a new directory",
            output.display(),
        ))?;
    }
    Ok(())
}

pub(crate) fn run(
    input: &Path,
    output: &Path,
    platform: IgvmPlatformType,
    guest_svn: Option<u32>,
) -> anyhow::Result<()> {
    let bytes = fs_err::read(input).with_context(|| format!("reading {}", input.display()))?;
    let prepared = prepare(&bytes, platform, guest_svn)?;
    let tool = fs_err::read(std::env::current_exe().context("locating igvmfilegen executable")?)
        .context("reading executable for preparation provenance")?;
    publish(output, &bytes, &prepared, &sha256(&tool))?;
    tracing::info!(
        path = %output.display(),
        ?platform,
        "Prepared CoRIM signing inputs; ID authentication and signer authorization are not checked"
    );
    Ok(())
}

fn verify_finalized(
    original: &[u8],
    finalized: &[u8],
    platform: IgvmPlatformType,
    guest_svn: Option<u32>,
) -> anyhow::Result<()> {
    let prepared =
        prepare(original, platform, guest_svn).context("reconstructing original signing inputs")?;
    let final_file =
        IgvmFile::new_from_binary(finalized, None).context("parsing finalized IGVM")?;
    let serializer = IgvmSerializer::new(&final_file)?;
    anyhow::ensure!(
        serializer.corim_for(platform) == Some(prepared.corim.as_slice()),
        "final CoRIM differs from the endorsement prepared for this original image"
    );
    let signature = final_file
        .initializations()
        .iter()
        .find_map(|header| match header {
            IgvmInitializationHeader::CorimSignature {
                compatibility_mask,
                signature,
            } if *compatibility_mask == prepared.compatibility_mask => Some(signature),
            _ => None,
        })
        .context("final image has no matching CoRIM signature")?;
    crate::corim_signature::verify_corim_signature(signature, &prepared.corim)?;

    let expected_file = IgvmFile::new_from_binary(&prepared.image, None)?;
    let mut expected = IgvmSerializer::new(&expected_file)?;
    expected.set_corim_signature(platform, signature.clone())?;
    let mut expected_bytes = Vec::new();
    expected.serialize(&mut expected_bytes)?;
    // Require the exact reconstructed output, including every ID authentication
    // byte and all other platforms. Do not normalize the finalized bytes: that
    // could conceal changes to padding or other data discarded by the parser.
    anyhow::ensure!(
        finalized == expected_bytes,
        "final image contains changes beyond the selected CoRIM additions"
    );
    Ok(())
}

pub(crate) fn run_verify(
    original: &Path,
    finalized: &Path,
    resource: crate::firmware_dll::ResourceSelector,
    platform: IgvmPlatformType,
    guest_svn: Option<u32>,
) -> anyhow::Result<()> {
    let original_bytes = fs_err::read(original).context("reading original IGVM")?;
    let finalized_bytes = crate::firmware_dll::read_igvm_image(finalized, resource)
        .context("reading finalized IGVM")?;
    verify_finalized(&original_bytes, &finalized_bytes, platform, guest_svn)?;
    let receipt = serde_json::json!({
        "schema_version": 2,
        "platform": crate::platform_mask::isolation_label(platform),
        "original_sha256": sha256(&original_bytes),
        "finalized_sha256": sha256(&finalized_bytes),
        "corim_signature_verified": true,
        "id_authentication": "not-checked",
        "signer_trust": "not-checked",
    });
    serde_json::to_writer_pretty(std::io::stdout().lock(), &receipt)
        .context("writing verification receipt")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corim_signature;
    use crate::corim_signature::test_helpers::sign_envelope_for;
    use crate::snp_id_block::tests::build_existing_snp_igvm;
    use crate::snp_id_block::tests::build_snp_igvm;
    use crate::snp_id_block::tests::edit_image;
    use crate::snp_id_block::tests::sign_payload;
    use igvm::IgvmDirectiveHeader;
    use test_with_tracing::test;

    #[test]
    fn snp_without_id_block_requires_explicit_svn_and_preserves_directives() {
        let input = build_snp_igvm(4);
        assert!(prepare(&input, IgvmPlatformType::SEV_SNP, None).is_err());
        for svn in [0, 7, u32::MAX] {
            let prepared = prepare(&input, IgvmPlatformType::SEV_SNP, Some(svn)).unwrap();
            assert_eq!(prepared.guest_svn, svn);
            let before = IgvmFile::new_from_binary(&input, None).unwrap();
            let after = IgvmFile::new_from_binary(&prepared.image, None).unwrap();
            assert_eq!(before.directives(), after.directives());
            let (_, comids) = corim::validate::decode_and_validate_at(&prepared.corim, 0).unwrap();
            assert_eq!(
                comids[0]
                    .triples
                    .conditional_endorsement_series
                    .as_ref()
                    .unwrap()[0]
                    .1[0]
                    .1[0]
                    .mval
                    .svn,
                Some(corim::types::measurement::SvnChoice::ExactValue(svn.into()))
            );
            let signature = sign_envelope_for(&prepared.corim, "snp-without-id");
            let output = corim_signature::patch(
                &prepared.image,
                &signature,
                IgvmPlatformType::SEV_SNP,
                Some(&prepared.corim),
            )
            .unwrap();
            verify_finalized(&input, &output, IgvmPlatformType::SEV_SNP, Some(svn)).unwrap();
            assert!(verify_finalized(&input, &output, IgvmPlatformType::SEV_SNP, None).is_err());
            assert!(
                verify_finalized(&input, &output, IgvmPlatformType::SEV_SNP, Some(svn ^ 1))
                    .is_err()
            );
        }
    }

    #[test]
    fn preparation_preserves_identity_and_derives_snp_svn() {
        for signed in [false, true] {
            let input = build_existing_snp_igvm(signed);
            let prepared = prepare(&input, IgvmPlatformType::SEV_SNP, None).unwrap();
            assert_eq!(prepared.guest_svn, 0x1234);
            assert!(prepare(&input, IgvmPlatformType::SEV_SNP, Some(0x1234)).is_ok());
            assert!(prepare(&input, IgvmPlatformType::SEV_SNP, Some(0x1235)).is_err());
            let before = IgvmFile::new_from_binary(&input, None).unwrap();
            let after = IgvmFile::new_from_binary(&prepared.image, None).unwrap();
            assert_eq!(before.directives(), after.directives());
            assert_eq!(before.platforms(), after.platforms());
            assert!(
                before
                    .initializations()
                    .iter()
                    .all(|h| after.initializations().contains(h))
            );
            assert_eq!(
                after.initializations().len(),
                before.initializations().len() + 1
            );
            let serializer = IgvmSerializer::new(&after).unwrap();
            assert_eq!(
                serializer.corim_for(IgvmPlatformType::SEV_SNP),
                Some(prepared.corim.as_slice())
            );
            let (corim, comids) =
                corim::validate::decode_and_validate_at(&prepared.corim, 0).unwrap();
            assert_eq!(corim.id.to_string(), "AMD/SEV-SNP/launch-measurement");
            assert_eq!(comids.len(), 1);
            let references = comids[0].triples.reference_triples.as_ref().unwrap();
            assert_eq!(references.len(), 1);
            assert_eq!(references[0].1.len(), 1);
            let digests = references[0].1[0].mval.digests.as_ref().unwrap();
            assert_eq!(digests.len(), 1);
            assert_eq!(digests[0].1, prepared.launch_digest);
            let series = comids[0]
                .triples
                .conditional_endorsement_series
                .as_ref()
                .unwrap();
            assert_eq!(series.len(), 1);
            assert_eq!(series[0].1.len(), 1);
            assert_eq!(series[0].1[0].0, references[0].1);
            assert_eq!(series[0].1[0].1.len(), 1);
            assert_eq!(
                series[0].1[0].1[0].mval.svn,
                Some(corim::types::measurement::SvnChoice::ExactValue(0x1234))
            );
            assert!(prepare(&prepared.image, IgvmPlatformType::SEV_SNP, None).is_err());
        }
    }

    #[test]
    fn preparation_rejects_missing_stale_and_ambiguous_identity() {
        assert!(prepare(&build_snp_igvm(4), IgvmPlatformType::SEV_SNP, None).is_err());
        let input = build_existing_snp_igvm(true);
        for change in [
            "measurement",
            "no-policy",
            "duplicate-policy",
            "duplicate-block",
            "mask",
            "disjoint-mask",
            "version",
            "reserved",
        ] {
            let changed = edit_image(&input, |init, directives| match change {
                "measurement" => {
                    let IgvmDirectiveHeader::PageData { data, .. } = &mut directives[0] else {
                        unreachable!()
                    };
                    data[0] ^= 1;
                }
                "no-policy" => init.clear(),
                "duplicate-policy" => init.push(init[0].clone()),
                "duplicate-block" => directives.push(directives.last().unwrap().clone()),
                "mask" | "disjoint-mask" => {
                    let IgvmDirectiveHeader::SnpIdBlock {
                        compatibility_mask, ..
                    } = directives.last_mut().unwrap()
                    else {
                        unreachable!()
                    };
                    *compatibility_mask = if change == "mask" { 12 } else { 8 };
                }
                "version" => {
                    let IgvmDirectiveHeader::SnpIdBlock { version, .. } =
                        directives.last_mut().unwrap()
                    else {
                        unreachable!()
                    };
                    *version = 2;
                }
                "reserved" => {
                    let IgvmDirectiveHeader::SnpIdBlock { reserved, .. } =
                        directives.last_mut().unwrap()
                    else {
                        unreachable!()
                    };
                    reserved[0] = 1;
                }
                _ => unreachable!(),
            });
            assert!(
                prepare(&changed, IgvmPlatformType::SEV_SNP, None).is_err(),
                "{change}"
            );
            // An explicit SVN must not turn invalid identity into absent identity.
            assert!(
                prepare(&changed, IgvmPlatformType::SEV_SNP, Some(0x1234)).is_err(),
                "{change}"
            );
        }
    }

    #[test]
    fn preparation_preserves_other_platform_measurements_and_endorsements() {
        let input = build_existing_snp_igvm(true);
        let snp_file = IgvmFile::new_from_binary(&input, None).unwrap();
        let mut platforms = snp_file.platforms().to_vec();
        let igvm::IgvmPlatformHeader::SupportedPlatform(mut vbs) = platforms[0];
        vbs.compatibility_mask = 1;
        vbs.platform_type = IgvmPlatformType::VSM_ISOLATION;
        platforms.push(igvm::IgvmPlatformHeader::SupportedPlatform(vbs));
        let mut directives = snp_file.directives().to_vec();
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
            igvm::IgvmRevision::V1,
            platforms,
            snp_file.initializations().to_vec(),
            directives,
        )
        .unwrap();
        let mut serializer = IgvmSerializer::new(&file).unwrap();
        let vbs_measurement = serializer
            .measurement_for(IgvmPlatformType::VSM_ISOLATION)
            .unwrap()
            .digest
            .clone();
        let template = crate::build_endorsement_corim(IgvmPlatformType::VSM_ISOLATION, 9).unwrap();
        let vbs_corim = serializer
            .add_corim(IgvmPlatformType::VSM_ISOLATION, template.build())
            .unwrap()
            .to_vec();
        let mut input = Vec::new();
        serializer.serialize(&mut input).unwrap();
        let vbs_signature = sign_envelope_for(&vbs_corim, "vbs-test");
        let input = corim_signature::patch(
            &input,
            &vbs_signature,
            IgvmPlatformType::VSM_ISOLATION,
            None,
        )
        .unwrap();
        let before = IgvmFile::new_from_binary(&input, None).unwrap();
        let prepared = prepare(&input, IgvmPlatformType::SEV_SNP, None).unwrap();
        let output = prepared.image.clone();
        let after = IgvmFile::new_from_binary(&output, None).unwrap();
        let serializer = IgvmSerializer::new(&after).unwrap();
        assert_eq!(
            serializer
                .measurement_for(IgvmPlatformType::VSM_ISOLATION)
                .unwrap()
                .digest,
            vbs_measurement
        );
        assert_eq!(
            serializer.corim_for(IgvmPlatformType::VSM_ISOLATION),
            Some(vbs_corim.as_slice())
        );
        assert!(
            before
                .initializations()
                .iter()
                .all(|header| after.initializations().contains(header))
        );
        assert_eq!(before.platforms(), after.platforms());
        assert_eq!(before.directives().len(), after.directives().len());
        for (before, after) in before.directives().iter().zip(after.directives()) {
            if !matches!(before, IgvmDirectiveHeader::SnpIdBlock { .. }) {
                assert_eq!(before, after);
            }
        }
        let signature = sign_envelope_for(&prepared.corim, "snp-test");
        let finalized = corim_signature::patch(
            &output,
            &signature,
            IgvmPlatformType::SEV_SNP,
            Some(&prepared.corim),
        )
        .unwrap();
        verify_finalized(&input, &finalized, IgvmPlatformType::SEV_SNP, None).unwrap();
        let changed_other_platform = edit_image(&finalized, |init, _| {
            for header in init {
                if let IgvmInitializationHeader::CorimSignature {
                    compatibility_mask: 1,
                    signature,
                } = header
                {
                    *signature.last_mut().unwrap() ^= 1;
                }
            }
        });
        assert!(
            verify_finalized(
                &input,
                &changed_other_platform,
                IgvmPlatformType::SEV_SNP,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn end_to_end_external_id_signature_then_corim_signature() {
        for signed in [false, true] {
            let producer = build_existing_snp_igvm(signed);
            let payload = snp_id_block::prepare_payload(&producer).unwrap();
            let (signature, key) = sign_payload(&payload);
            let input =
                snp_id_block::replace_snp_id_block_signed(&producer, &payload, &signature, &key)
                    .unwrap();
            let prepared = prepare(&input, IgvmPlatformType::SEV_SNP, None).unwrap();
            let id_signed = prepared.image.clone();
            let signature = sign_envelope_for(&prepared.corim, "existing-image-test");
            let output = corim_signature::patch(
                &id_signed,
                &signature,
                IgvmPlatformType::SEV_SNP,
                Some(&prepared.corim),
            )
            .unwrap();
            verify_finalized(&input, &output, IgvmPlatformType::SEV_SNP, None).unwrap();
            assert!(verify_finalized(&input, &id_signed, IgvmPlatformType::SEV_SNP, None).is_err());
            assert!(verify_finalized(&producer, &output, IgvmPlatformType::SEV_SNP, None).is_err());
            assert!(
                verify_finalized(&input, &output, IgvmPlatformType::SEV_SNP, Some(0x1235)).is_err()
            );
            let (id_signature, key) = sign_payload(&payload);
            let resigned =
                snp_id_block::replace_snp_id_block_signed(&output, &payload, &id_signature, &key)
                    .unwrap();
            assert!(verify_finalized(&input, &resigned, IgvmPlatformType::SEV_SNP, None).is_err());
            let extra_directive = edit_image(&output, |_, directives| {
                directives.push(IgvmDirectiveHeader::RequiredMemory {
                    gpa: 0x1000,
                    compatibility_mask: 4,
                    number_of_bytes: 4096,
                    vtl2_protectable: false,
                });
            });
            assert!(
                verify_finalized(&input, &extra_directive, IgvmPlatformType::SEV_SNP, None)
                    .is_err()
            );
            let invalid_signature = edit_image(&output, |init, _| {
                for header in init {
                    if let IgvmInitializationHeader::CorimSignature { signature, .. } = header {
                        *signature.last_mut().unwrap() ^= 1;
                    }
                }
            });
            assert!(
                verify_finalized(&input, &invalid_signature, IgvmPlatformType::SEV_SNP, None)
                    .is_err()
            );
            let before = IgvmFile::new_from_binary(&input, None).unwrap();
            let after = IgvmFile::new_from_binary(&output, None).unwrap();
            assert_eq!(before.platforms(), after.platforms());
            let mut serializer = IgvmSerializer::new(&after).unwrap();
            let block = snp_id_block::existing_id_block(&mut serializer).unwrap();
            assert_eq!(block.state, snp_id_block::IdBlockState::Signed);
            assert_eq!(zerocopy::IntoBytes::as_bytes(&block.payload), payload);
            assert_eq!(
                serializer.corim_for(IgvmPlatformType::SEV_SNP),
                Some(prepared.corim.as_slice())
            );
            assert_eq!(before.directives().len(), after.directives().len());
            for (original, finalized) in before.directives().iter().zip(after.directives()) {
                assert_eq!(original, finalized);
            }
            assert_eq!(
                after
                    .directives()
                    .iter()
                    .filter(|h| matches!(h, IgvmDirectiveHeader::SnpIdBlock { .. }))
                    .count(),
                1
            );
            assert_eq!(
                after
                    .initializations()
                    .iter()
                    .filter(|h| matches!(h, IgvmInitializationHeader::CorimDocument { .. }))
                    .count(),
                1
            );
            let signatures: Vec<_> = after
                .initializations()
                .iter()
                .filter_map(|h| {
                    if let IgvmInitializationHeader::CorimSignature { signature, .. } = h {
                        Some(signature)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(signatures, vec![&signature]);
            let wrong_signature = sign_envelope_for(b"another document", "wrong-input");
            assert!(
                corim_signature::patch(
                    &id_signed,
                    &wrong_signature,
                    IgvmPlatformType::SEV_SNP,
                    None
                )
                .is_err()
            );
            assert!(
                corim_signature::patch(
                    &id_signed,
                    &signature,
                    IgvmPlatformType::SEV_SNP,
                    Some(b"another document")
                )
                .is_err()
            );
        }
    }

    #[test]
    fn corim_does_not_require_valid_id_authentication() {
        let original = build_existing_snp_igvm(true);
        for field in [
            "signature",
            "partial-unsigned",
            "algorithm",
            "curve",
            "padding",
            "key-reserved",
        ] {
            let input = edit_image(&original, |_, directives| {
                let IgvmDirectiveHeader::SnpIdBlock {
                    id_key_signature,
                    id_key_algorithm,
                    id_public_key,
                    ..
                } = directives.last_mut().unwrap()
                else {
                    unreachable!()
                };
                match field {
                    "signature" => id_key_signature.r_comp[0] ^= 1,
                    "partial-unsigned" => *id_key_algorithm = 0,
                    "algorithm" => *id_key_algorithm = 2,
                    "curve" => id_public_key.curve = 1,
                    "padding" => id_key_signature.s_comp[48] = 1,
                    "key-reserved" => id_public_key.reserved = 1,
                    _ => unreachable!(),
                }
            });
            assert!(snp_id_block::prepare_payload(&input).is_err(), "{field}");
            let prepared = prepare(&input, IgvmPlatformType::SEV_SNP, None).unwrap();
            let signature = sign_envelope_for(&prepared.corim, "independent-corim");
            let output = corim_signature::patch(
                &prepared.image,
                &signature,
                IgvmPlatformType::SEV_SNP,
                None,
            )
            .unwrap();
            verify_finalized(&input, &output, IgvmPlatformType::SEV_SNP, None).unwrap();
            let changed_auth = edit_image(&output, |_, directives| {
                let IgvmDirectiveHeader::SnpIdBlock {
                    id_key_signature, ..
                } = directives.last_mut().unwrap()
                else {
                    unreachable!()
                };
                id_key_signature.r_comp[0] ^= 1;
            });
            assert!(
                verify_finalized(&input, &changed_auth, IgvmPlatformType::SEV_SNP, None).is_err()
            );
            let changed_svn = edit_image(&output, |_, directives| {
                let IgvmDirectiveHeader::SnpIdBlock { guest_svn, .. } =
                    directives.last_mut().unwrap()
                else {
                    unreachable!()
                };
                *guest_svn += 1;
            });
            assert!(
                verify_finalized(&input, &changed_svn, IgvmPlatformType::SEV_SNP, None).is_err()
            );
        }
    }

    // Include a VBS BSP context, not merely a platform header. TDX measures
    // normal pages and uses its own policy; neither platform needs an ID block.
    fn non_snp_fixture(platform: IgvmPlatformType) -> Vec<u8> {
        let input = build_snp_igvm(4);
        let file = IgvmFile::new_from_binary(&input, None).unwrap();
        let mut platforms = file.platforms().to_vec();
        let igvm::IgvmPlatformHeader::SupportedPlatform(info) = &mut platforms[0];
        info.platform_type = platform;
        let mut directives = file.directives().to_vec();
        if platform == IgvmPlatformType::VSM_ISOLATION {
            directives.push(IgvmDirectiveHeader::X64VbsVpContext {
                vtl: igvm::hv_defs::Vtl::Vtl0,
                registers: vec![igvm::registers::X86Register::Rip(0x1000)],
                compatibility_mask: 4,
            });
        }
        let file = IgvmFile::new(
            igvm::IgvmRevision::V1,
            platforms,
            vec![IgvmInitializationHeader::GuestPolicy {
                policy: 0,
                compatibility_mask: 4,
            }],
            directives,
        )
        .unwrap();
        let mut bytes = Vec::new();
        file.serialize(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn tdx_and_vbs_prepare_sign_verify_require_explicit_svn() {
        for platform in [IgvmPlatformType::TDX, IgvmPlatformType::VSM_ISOLATION] {
            let original = non_snp_fixture(platform);
            assert!(prepare(&original, platform, None).is_err());
            assert!(snp_id_block::prepare_payload(&original).is_err());
            for svn in [0, 7, u32::MAX] {
                let prepared = prepare(&original, platform, Some(svn)).unwrap();
                let (corim, comids) =
                    corim::validate::decode_and_validate_at(&prepared.corim, 0).unwrap();
                assert_eq!(
                    corim.id.to_string(),
                    if platform == IgvmPlatformType::TDX {
                        "Intel/TDX/launch-measurement"
                    } else {
                        "Microsoft/VBS/launch-measurement"
                    }
                );
                let triples = &comids[0].triples;
                let references = triples.reference_triples.as_ref().unwrap();
                assert_eq!(
                    references[0].1[0].mval.digests.as_ref().unwrap()[0].1,
                    prepared.launch_digest
                );
                let series = triples.conditional_endorsement_series.as_ref().unwrap();
                assert_eq!(series[0].1[0].0, references[0].1);
                assert_eq!(
                    series[0].1[0].1[0].mval.svn,
                    Some(corim::types::measurement::SvnChoice::ExactValue(svn.into()))
                );
                assert_eq!(
                    prepared.launch_digest.len(),
                    if platform == IgvmPlatformType::TDX {
                        48
                    } else {
                        32
                    }
                );
                let signature = sign_envelope_for(&prepared.corim, "platform-test");
                let output = corim_signature::patch(
                    &prepared.image,
                    &signature,
                    platform,
                    Some(&prepared.corim),
                )
                .unwrap();
                verify_finalized(&original, &output, platform, Some(svn)).unwrap();
                assert!(verify_finalized(&original, &output, platform, None).is_err());
                assert!(verify_finalized(&original, &output, platform, Some(svn ^ 1)).is_err());
                assert!(
                    verify_finalized(&original, &output, IgvmPlatformType::SEV_SNP, None).is_err()
                );
                let changed = edit_image(&output, |_, directives| {
                    let IgvmDirectiveHeader::PageData { data, .. } = &mut directives[0] else {
                        unreachable!()
                    };
                    data[0] ^= 1;
                });
                assert!(verify_finalized(&original, &changed, platform, Some(svn)).is_err());
                let root = tempfile::tempdir().unwrap();
                let original_path = root.path().join("original.igvm");
                let output_dir = root.path().join("prepared");
                let final_path = root.path().join("final.igvm");
                fs_err::write(&original_path, &original).unwrap();
                run(&original_path, &output_dir, platform, Some(svn)).unwrap();
                assert!(!output_dir.join("snp.idblock").exists());
                let record: serde_json::Value = serde_json::from_slice(
                    &fs_err::read(output_dir.join("endorsement-inputs.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    record["platform"],
                    crate::platform_mask::isolation_label(platform)
                );
                assert_eq!(record["guest_svn"], svn);
                assert_eq!(
                    fs_err::read(output_dir.join("corim.cbor")).unwrap(),
                    prepared.corim
                );
                fs_err::write(&final_path, &output).unwrap();
                run_verify(
                    &original_path,
                    &final_path,
                    Default::default(),
                    platform,
                    Some(svn),
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn publication_records_exact_artifacts_and_refuses_reuse() {
        let root = tempfile::tempdir().unwrap();
        let input = build_existing_snp_igvm(false);
        let input_path = root.path().join("input.igvm");
        fs_err::write(&input_path, &input).unwrap();
        let output = root.path().join("prepared");
        run(&input_path, &output, IgvmPlatformType::SEV_SNP, None).unwrap();
        assert_eq!(fs_err::read(&input_path).unwrap(), input);
        let record_bytes = fs_err::read(output.join("endorsement-inputs.json")).unwrap();
        let record: serde_json::Value = serde_json::from_slice(&record_bytes).unwrap();
        assert_eq!(record["input_sha256"], sha256(&input));
        assert_eq!(record["release_ready"], false);
        assert_eq!(record["schema_version"], 2);
        assert_eq!(record["platform"], "snp");
        assert_eq!(record["signer_trust"], "not-checked");
        assert_eq!(record["id_authentication"], "not-checked");
        assert_eq!(record["guest_svn"], 0x1234);
        assert_eq!(record["tool_sha256"].as_str().unwrap().len(), 64);
        let artifacts = record["artifacts"].as_array().unwrap();
        assert_eq!(artifacts.len(), 2);
        for artifact in artifacts {
            let bytes = fs_err::read(output.join(artifact["file"].as_str().unwrap())).unwrap();
            assert_eq!(artifact["sha256"], sha256(&bytes));
            assert_eq!(artifact["size"], bytes.len());
        }
        let image = fs_err::read(output.join("image.igvm")).unwrap();
        assert!(!output.join("snp.idblock").exists());
        assert_eq!(fs_err::read_dir(&output).unwrap().count(), 3);
        let file = IgvmFile::new_from_binary(&image, None).unwrap();
        let serializer = IgvmSerializer::new(&file).unwrap();
        assert_eq!(
            serializer.corim_for(IgvmPlatformType::SEV_SNP).unwrap(),
            fs_err::read(output.join("corim.cbor")).unwrap()
        );
        let prepared = prepare(&input, IgvmPlatformType::SEV_SNP, None).unwrap();
        assert!(publish(&output, &input, &prepared, "test").is_err());
        assert_eq!(
            fs_err::read(output.join("endorsement-inputs.json")).unwrap(),
            record_bytes
        );
        let incomplete = root.path().join("incomplete");
        fs_err::create_dir(&incomplete).unwrap();
        fs_err::write(incomplete.join("image.igvm"), b"partial").unwrap();
        assert!(publish(&incomplete, &input, &prepared, "test").is_err());
        assert!(!incomplete.join("endorsement-inputs.json").exists());
        assert_eq!(
            fs_err::read(incomplete.join("image.igvm")).unwrap(),
            b"partial"
        );
        assert!(
            publish(
                &root.path().join("missing/child"),
                &input,
                &prepared,
                "test"
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_input_creates_no_output() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input.igvm");
        let output = root.path().join("prepared");
        fs_err::write(&input, build_snp_igvm(4)).unwrap();
        assert!(run(&input, &output, IgvmPlatformType::SEV_SNP, None).is_err());
        assert!(!output.exists());
        fs_err::write(&input, b"not an IGVM").unwrap();
        assert!(run(&input, &output, IgvmPlatformType::SEV_SNP, None).is_err());
        assert!(!output.exists());
    }

    #[test]
    fn publication_accepts_relative_paths_and_rejects_existing_files() {
        let cwd = std::env::current_dir().unwrap();
        let root = tempfile::tempdir_in(&cwd).unwrap();
        let output = root.path().strip_prefix(&cwd).unwrap().join("prepared");
        assert!(!output.is_absolute());
        let input = build_existing_snp_igvm(true);
        let prepared = prepare(&input, IgvmPlatformType::SEV_SNP, None).unwrap();
        publish(&output, &input, &prepared, "test").unwrap();
        assert!(output.join("endorsement-inputs.json").is_file());
        let existing = root.path().join("existing");
        fs_err::write(&existing, b"do not overwrite").unwrap();
        assert!(publish(&existing, &input, &prepared, "test").is_err());
        assert_eq!(fs_err::read(&existing).unwrap(), b"do not overwrite");
    }

    #[test]
    fn concurrent_publication_never_overwrites_another_preparation() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("prepared");
        let input = build_existing_snp_igvm(true);
        let prepared = prepare(&input, IgvmPlatformType::SEV_SNP, None).unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let write = || {
                barrier.wait();
                publish(&output, &input, &prepared, "test")
            };
            let first = scope.spawn(write);
            let second = scope.spawn(write);
            assert_ne!(
                first.join().unwrap().is_ok(),
                second.join().unwrap().is_ok()
            );
        });
        assert_eq!(
            fs_err::read(output.join("image.igvm")).unwrap(),
            prepared.image
        );
        assert!(output.join("endorsement-inputs.json").is_file());
    }
}
