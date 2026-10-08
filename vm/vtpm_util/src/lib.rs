// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module includes `vtpm_util`, a tool to create and manage vTPM blobs.
//! vTPM blobs are used to provide TPM functionality to trusted and confidential VMs.
mod key_import;
mod marshal;
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
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::sync::Arc;
use std::vec;
use tpm_lib as tpm_helper;
use tpm_protocol::TPM_RSA_SRK_HANDLE;
use tpm_protocol::tpm20proto::AlgId;
use tpm_protocol::tpm20proto::TPM20_RH_OWNER;
use tpm_protocol::tpm20proto::protocol::Tpm2bPublic;
use tpm_protocol::tpm20proto::protocol::TpmtPublic;

fn write_sensitive_file(path: &str, contents: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    file.write_all(contents)
}

/// Creates a vTPM blob and writes it to `path`.
pub fn create_vtpm_blob_file(path: &str) -> anyhow::Result<()> {
    tracing::info!("Creating vTPM blob and saving to file: {}", path);
    let (mut tpm_engine_helper, nv_blob_accessor) = create_tpm_engine_helper();
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
    let vtpm_blob_content = fs::read(vtpm_blob_path).context("failed to read vTPM blob file")?;
    let (mut vtpm_engine_helper, _nv_blob_accessor) = create_tpm_engine_helper();

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

    // Write the SRK pub to a file.
    let mut srk_pub_file =
        File::create(srk_out_path).context("failed to create SRK output file")?;

    // Use the full TPM2B_PUBLIC serialization to match Windows C++ GetSrkPub
    // Windows returns the raw publicArea from ReadPublic.m_pOutPublic->Get(),
    // which is the serialized TPM2B_PUBLIC structure
    let srk_pub = response.out_public.serialize();
    srk_pub_file
        .write_all(&srk_pub)
        .context("failed to write SRK output file")?;

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
pub fn print_key_name(srkpub_path: &str) {
    let mut srk_pub_file = OpenOptions::new()
        .write(false)
        .read(true)
        .open(srkpub_path)
        .expect("failed to open file");

    let mut srkpub_content_buf = Vec::new();
    srk_pub_file
        .read_to_end(&mut srkpub_content_buf)
        .expect("failed to read file");

    // Deserialize the srkpub to a public area.
    let public_key =
        Tpm2bPublic::deserialize(&srkpub_content_buf).expect("failed to deserialize srkpub");
    let public_area: TpmtPublic = public_key.public_area;
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
    use super::write_sensitive_file;
    use std::fs;
    use test_with_tracing::test;

    #[test]
    fn sensitive_file_does_not_overwrite_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sensitive");
        fs::write(&path, b"existing").unwrap();

        assert!(write_sensitive_file(path.to_str().unwrap(), b"replacement").is_err());
        assert_eq!(fs::read(path).unwrap(), b"existing");
    }

    #[cfg(unix)]
    #[test]
    fn sensitive_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sensitive");

        write_sensitive_file(path.to_str().unwrap(), b"secret").unwrap();

        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
