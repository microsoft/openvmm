// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module includes `vtpm-util`, a tool to create and manage vTPM blobs.
//! vTPM blobs are used to provide TPM functionality to trusted and confidential VMs.
mod key_import;
mod vtpm_helper;

pub use key_import::create_random_rsa_key_in_tpm2_import_blob_format;

use crate::vtpm_helper::TpmEngineHelper;
use crate::vtpm_helper::create_tpm_engine_helper;
use anyhow::Context;
use base64::Engine;
use crypto::sha_256::sha_256;
use parking_lot::Mutex;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::vec;
use tpm_lib as tpm_helper;
use tpm_protocol::TPM_RSA_SRK_HANDLE;
use tpm_protocol::tpm20proto::AlgId;
use tpm_protocol::tpm20proto::AlgIdEnum;
use tpm_protocol::tpm20proto::TPM20_RH_OWNER;
use tpm_protocol::tpm20proto::protocol::Tpm2bPublic;
use tpm_protocol::tpm20proto::protocol::TpmtPublic;

/// Microsoft TPM reference implementation version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TpmVersion {
    /// TPM reference implementation version 1.38.
    #[default]
    V138,
    /// TPM reference implementation version 1.85.
    V185,
}

fn write_sensitive_file(path: &str, contents: &[u8]) -> std::io::Result<()> {
    write_sensitive_file_with(path, contents, File::write_all)
}

fn write_sensitive_file_with(
    path: &str,
    contents: &[u8],
    write: impl FnOnce(&mut File, &[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let parent = Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    write(file.as_file_mut(), contents)?;
    file.persist_noclobber(path)
        .map(|_| ())
        .map_err(|error| error.error)
}

/// Creates a vTPM blob and writes it to `path`.
pub fn create_vtpm_blob_file(path: &str) -> anyhow::Result<()> {
    create_vtpm_blob_file_with_version(path, TpmVersion::default())
}

/// Creates a vTPM blob using `version` and writes it to `path`.
pub fn create_vtpm_blob_file_with_version(path: &str, version: TpmVersion) -> anyhow::Result<()> {
    tracing::info!("Creating vTPM blob and saving to file: {}", path);
    let (mut tpm_engine_helper, nv_blob_accessor) = create_tpm_engine_helper(version)?;
    tpm_engine_helper
        .initialize_tpm_engine()
        .context("failed to initialize TPM engine")?;

    let state = create_vtpm_blob(tpm_engine_helper, nv_blob_accessor)?;
    tracing::info!("vTPM blob size: {}", state.len());

    write_sensitive_file(path, &state).context("failed to create vTPM state blob file")?;
    tracing::info!("vTPM blob created and saved to file: {}", path);
    Ok(())
}

/// Writes the SRK public key from a vTPM blob in TPM2B format.
pub fn write_srk(vtpm_blob_path: &str, srk_out_path: &str) -> anyhow::Result<()> {
    write_srk_with_version(vtpm_blob_path, srk_out_path, TpmVersion::default())
}

/// Writes the SRK public key from a versioned vTPM blob in TPM2B format.
pub fn write_srk_with_version(
    vtpm_blob_path: &str,
    srk_out_path: &str,
    version: TpmVersion,
) -> anyhow::Result<()> {
    let vtpm_blob_content = fs::read(vtpm_blob_path).context("failed to read vTPM blob file")?;
    let (mut vtpm_engine_helper, _nv_blob_accessor) = create_tpm_engine_helper(version)?;

    vtpm_engine_helper
        .tpm_engine
        .reset(Some(&vtpm_blob_content))
        .context("failed to restore TPM engine from blob")?;
    vtpm_engine_helper
        .initialize_tpm_engine()
        .context("failed to initialize TPM engine")?;

    tracing::info!(
        "write-srk: blob file: {}, SRK out file: {}",
        vtpm_blob_path,
        srk_out_path
    );
    export_vtpm_srk_pub(vtpm_engine_helper, srk_out_path)
}

/// Create vtpm and return its state as a byte vector.
fn create_vtpm_blob(
    mut tpm_engine_helper: TpmEngineHelper,
    nvm_state_blob: Arc<Mutex<Vec<u8>>>,
) -> anyhow::Result<Vec<u8>> {
    // Create a vTPM instance.
    tracing::info!("Initializing TPM engine with deterministic ColdInit for Ubuntu compatibility.");

    // NOTE: We do NOT call refresh_tpm_seeds() as that would randomize the seeds.
    // Ubuntu expects the TPM to use the initial deterministic seeds from ColdInit.

    // Create a primary key: SRK
    let auth_handle = TPM20_RH_OWNER;
    let srk_in_public = tpm_helper::srk_pub_template().context("failed to create SRK template")?;
    let response = tpm_engine_helper
        .create_primary(auth_handle, srk_in_public)
        .context("failed to create SRK primary key")?;
    tracing::info!("SRK handle: {:?}", response.object_handle);
    anyhow::ensure!(
        response.out_public.size.get() != 0,
        "TPM returned an empty SRK public area"
    );
    tracing::trace!("SRK public area: {:?}", response.out_public.public_area);

    // Evict the SRK handle.
    tpm_engine_helper
        .evict_control(TPM20_RH_OWNER, response.object_handle, TPM_RSA_SRK_HANDLE)
        .context("failed to persist SRK")?;

    // DEBUG: retrieve the SRK and print its SHA256 hash and name
    let response = tpm_engine_helper
        .read_public(TPM_RSA_SRK_HANDLE)
        .context("failed to read persisted SRK")?;
    let public_area_hash = sha_256(&response.out_public.public_area.serialize());
    tracing::trace!(
        "SRK public area SHA256 hash: {}",
        hex::encode(public_area_hash)
    );

    // Calculate and print the SRK name (algorithm ID + hash)
    let algorithm_id = response.out_public.public_area.name_alg;
    let mut srk_name = vec![0u8; 2 + public_area_hash.len()];
    srk_name[0] = (algorithm_id.0.get() >> 8) as u8;
    srk_name[1] = (algorithm_id.0.get() & 0xFF) as u8;
    srk_name[2..].copy_from_slice(&public_area_hash);

    let srk_name_hex = srk_name
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();
    tracing::info!("Generated SRK name: {}", srk_name_hex);

    // Get the nv state of the TPM.
    let nv_blob = nvm_state_blob.lock().clone();
    tracing::trace!("Retrieved NV blob size: {}", nv_blob.len());
    Ok(nv_blob)
}

/// Export the vTPM SRK public key to a file in TPM2B format.
fn export_vtpm_srk_pub(
    mut tpm_engine_helper: TpmEngineHelper,
    srk_out_path: &str,
) -> anyhow::Result<()> {
    // Debug: Check if the SRK handle exists
    tracing::trace!("Checking if SRK handle exists...");
    let find_result = tpm_engine_helper.find_object(TPM_RSA_SRK_HANDLE);
    match find_result {
        Ok(Some(_handle)) => tracing::trace!("SRK handle found"),
        Ok(None) => tracing::trace!("SRK handle NOT found"),
        Err(e) => tracing::error!("Error finding SRK handle: {:?}", e),
    }

    // Extract SRK primary key public area.
    let response = tpm_engine_helper
        .read_public(TPM_RSA_SRK_HANDLE)
        .context("failed to read SRK public area")?;
    tracing::trace!("SRK public area: {:?}", response.out_public.public_area);

    // Use the full TPM2B_PUBLIC serialization to match Windows C++ GetSrkPub
    // Windows returns the raw publicArea from ReadPublic.m_pOutPublic->Get(),
    // which is the serialized TPM2B_PUBLIC structure
    let srk_pub = response.out_public.serialize();
    write_sensitive_file(srk_out_path, &srk_pub).context("failed to create SRK output file")?;

    // Calculate and print the SRK name (algorithm ID + hash)
    let public_area_hash = sha_256(&response.out_public.public_area.serialize());
    tracing::trace!(
        "SRK public area SHA256 hash: {}",
        hex::encode(public_area_hash)
    );
    let algorithm_id = response.out_public.public_area.name_alg;
    let mut srk_name = vec![0u8; 2 + public_area_hash.len()];
    srk_name[0] = (algorithm_id.0.get() >> 8) as u8;
    srk_name[1] = (algorithm_id.0.get() & 0xFF) as u8;
    srk_name[2..].copy_from_slice(&public_area_hash);

    let srk_name_hex = srk_name
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();
    tracing::info!("SRK name: {}", srk_name_hex);

    // Compute SHA256 hash of the public area
    let public_area_hash = sha_256(&response.out_public.public_area.serialize());
    tracing::trace!(
        "SRK public area SHA256 hash: {} is written to file {}",
        hex::encode(public_area_hash),
        srk_out_path
    );
    Ok(())
}

/// Print the SRK public key name.
/// Prints the TPM key name of an SRK public key file.
pub fn print_key_name(srkpub_path: &str) -> anyhow::Result<()> {
    let srkpub_content_buf = fs::read(srkpub_path).context("failed to read SRK public key file")?;

    // Deserialize the srkpub to a public area.
    let public_key = Tpm2bPublic::deserialize(&srkpub_content_buf)
        .context("failed to deserialize SRK public key")?;
    let public_area: TpmtPublic = public_key.public_area;
    anyhow::ensure!(
        public_area.name_alg == AlgIdEnum::SHA256.into(),
        "unsupported SRK name algorithm {:?}; only SHA-256 is supported",
        public_area.name_alg
    );
    // Compute SHA256 hash of the public area
    let public_area_hash = sha_256(&public_area.serialize());

    // Compute the key name
    let rsa_key = public_area.unique;
    tracing::trace!("Printing key properties.\n");
    tracing::trace!("Public key type: {:?}", public_area.my_type);
    tracing::trace!("Public hash alg: {:?}", public_area.name_alg);
    tracing::trace!(
        "Public key size in bits: {:?}",
        public_area.parameters.key_bits
    );
    print_sha256_hash(public_area.serialize().as_slice());

    // Compute the key name
    let algorithm_id = public_area.name_alg;
    let mut output_key = vec![0u8; size_of::<AlgId>() + public_area_hash.len()];
    output_key[0] = (algorithm_id.0.get() >> 8) as u8;
    output_key[1] = (algorithm_id.0.get() & 0xFF) as u8;
    for i in 0..public_area_hash.len() {
        output_key[i + 2] = public_area_hash[i];
    }

    let base64_key = base64::engine::general_purpose::STANDARD.encode(&output_key);
    tracing::info!("Key name: {}", base64_key);

    // DEBUG: Print RSA bytes in hex to be able to compare with tpm2_readpublic -c 0x81000001
    let mut rsa_pub_str = String::new();
    for i in 0..tpm_helper::RSA_2K_MODULUS_SIZE {
        rsa_pub_str.push_str(&format!("{:02x}", rsa_key.buffer[i]));
    }
    tracing::trace!("RSA key bytes: {}", rsa_pub_str);
    tracing::info!("\nOperation completed successfully.\n");
    Ok(())
}

/// Print SHA256 hash of the data.
pub(crate) fn print_sha256_hash(data: &[u8]) {
    let hash = sha_256(data);
    let mut hash_str = String::new();
    for i in 0..hash.len() {
        hash_str.push_str(&format!("{:02X}", hash[i]));
    }
    tracing::trace!("SHA256 hash: {}\n", hash_str);
}

#[cfg(test)]
mod tests {
    use super::TpmVersion;
    use super::create_vtpm_blob_file;
    use super::create_vtpm_blob_file_with_version;
    use super::print_key_name;
    use super::write_sensitive_file;
    use super::write_sensitive_file_with;
    use super::write_srk;
    use super::write_srk_with_version;
    use std::fs;
    use std::io::Write;
    use test_with_tracing::test;
    use tpm_protocol::tpm20proto::AlgIdEnum;
    use tpm_protocol::tpm20proto::TpmaObjectBits;
    use tpm_protocol::tpm20proto::protocol::Tpm2bPublic;
    use tpm_protocol::tpm20proto::protocol::TpmsRsaParams;
    use tpm_protocol::tpm20proto::protocol::TpmtPublic;
    use tpm_protocol::tpm20proto::protocol::TpmtRsaScheme;
    use tpm_protocol::tpm20proto::protocol::TpmtSymDefObject;

    #[test]
    fn sensitive_file_does_not_overwrite_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sensitive");
        fs::write(&path, b"existing").unwrap();

        assert!(write_sensitive_file(path.to_str().unwrap(), b"replacement").is_err());
        assert_eq!(fs::read(path).unwrap(), b"existing");
    }

    #[test]
    fn sensitive_file_is_published_only_after_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sensitive");

        write_sensitive_file_with(path.to_str().unwrap(), b"secret", |file, contents| {
            file.write_all(&contents[..1])?;
            assert!(!path.exists());
            file.write_all(&contents[1..])
        })
        .unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"secret");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn sensitive_file_cleans_up_failed_writes_and_allows_retry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sensitive");

        let error =
            write_sensitive_file_with(path.to_str().unwrap(), b"secret", |file, contents| {
                file.write_all(&contents[..1])?;
                Err(std::io::Error::other("injected write failure"))
            })
            .unwrap_err();

        assert_eq!(error.to_string(), "injected write failure");
        assert!(!path.exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);

        write_sensitive_file(path.to_str().unwrap(), b"secret").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"secret");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn sensitive_file_preserves_destination_created_during_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sensitive");

        let error =
            write_sensitive_file_with(path.to_str().unwrap(), b"secret", |file, contents| {
                file.write_all(contents)?;
                fs::write(&path, b"existing")
            })
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"existing");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn print_key_name_rejects_missing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing");

        let error = print_key_name(path.to_str().unwrap()).unwrap_err();

        assert_eq!(error.to_string(), "failed to read SRK public key file");
    }

    #[test]
    fn print_key_name_rejects_malformed_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("malformed");
        fs::write(&path, b"not a TPM2B_PUBLIC").unwrap();

        let error = print_key_name(path.to_str().unwrap()).unwrap_err();

        assert_eq!(error.to_string(), "failed to deserialize SRK public key");
    }

    #[test]
    fn print_key_name_rejects_non_sha256_name_algorithm() {
        let symmetric = TpmtSymDefObject::new(AlgIdEnum::NULL.into(), None, None);
        let scheme = TpmtRsaScheme::new(AlgIdEnum::NULL.into(), None);
        let parameters = TpmsRsaParams::new(symmetric, scheme, 2048, 0);
        let public_area = TpmtPublic::new(
            AlgIdEnum::RSA.into(),
            AlgIdEnum::SHA.into(),
            TpmaObjectBits::new(),
            &[],
            parameters,
            &[0; 256],
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unsupported-name-alg.tpm2b");
        fs::write(&path, Tpm2bPublic::new(public_area).serialize()).unwrap();

        let error = print_key_name(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("only SHA-256 is supported"));
    }

    #[test]
    fn write_srk_rejects_input_output_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let blob_path = directory.path().join("vtpm.blob");
        let alias_path = directory.path().join("vtpm-alias.blob");
        create_vtpm_blob_file(blob_path.to_str().unwrap()).unwrap();
        fs::hard_link(&blob_path, &alias_path).unwrap();
        let original_blob = fs::read(&blob_path).unwrap();

        let error =
            write_srk(blob_path.to_str().unwrap(), blob_path.to_str().unwrap()).unwrap_err();
        assert_eq!(error.to_string(), "failed to create SRK output file");
        assert_eq!(fs::read(&blob_path).unwrap(), original_blob);

        let error =
            write_srk(blob_path.to_str().unwrap(), alias_path.to_str().unwrap()).unwrap_err();
        assert_eq!(error.to_string(), "failed to create SRK output file");
        assert_eq!(fs::read(&blob_path).unwrap(), original_blob);
        assert_eq!(fs::read(&alias_path).unwrap(), original_blob);
    }

    #[test]
    fn tpm_185_blob_can_export_srk() {
        let directory = tempfile::tempdir().unwrap();
        let blob_path = directory.path().join("vtpm-185.blob");
        let srk_path = directory.path().join("srk-185.tpm2b");

        create_vtpm_blob_file_with_version(blob_path.to_str().unwrap(), TpmVersion::V185).unwrap();
        assert_eq!(fs::metadata(&blob_path).unwrap().len(), 128 * 1024);

        write_srk_with_version(
            blob_path.to_str().unwrap(),
            srk_path.to_str().unwrap(),
            TpmVersion::V185,
        )
        .unwrap();
        Tpm2bPublic::deserialize(&fs::read(srk_path).unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn sensitive_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sensitive");

        write_sensitive_file(path.to_str().unwrap(), b"secret").unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
