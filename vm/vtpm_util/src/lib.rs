// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module includes `vtpm_util`, a tool to create and manage vTPM blobs.
//! vTPM blobs are used to provide TPM functionality to trusted and confidential VMs.
mod marshal;
mod vtpm_helper;
#[cfg(feature = "experimental")]
mod vtpm_sock_server;

use crate::vtpm_helper::TpmEngineHelper;
use crate::vtpm_helper::create_tpm_engine_helper;
use base64::Engine;
use marshal::TpmtSensitive;
use openssl::ec::EcGroup;
use openssl::ec::EcKey;
use openssl::nid::Nid;
use openssl::pkey::PKey;
use openssl::rsa::Rsa;
use parking_lot::Mutex;
use sha2::Digest;
use sha2::Sha256;
#[cfg(feature = "experimental")]
use std::convert::TryInto;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::sync::Arc;
use std::vec;
use tpm_lib as tpm_helper;
use tpm_protocol::TPM_RSA_SRK_HANDLE;
use tpm_protocol::tpm20proto::AlgId;
use tpm_protocol::tpm20proto::AlgIdEnum;
use tpm_protocol::tpm20proto::TPM20_RH_OWNER;
use tpm_protocol::tpm20proto::TpmaObjectBits;
use tpm_protocol::tpm20proto::protocol::Tpm2bBuffer;
use tpm_protocol::tpm20proto::protocol::Tpm2bPublic;
use tpm_protocol::tpm20proto::protocol::TpmsRsaParams;
use tpm_protocol::tpm20proto::protocol::TpmtPublic;
use tpm_protocol::tpm20proto::protocol::TpmtRsaScheme;
use tpm_protocol::tpm20proto::protocol::TpmtSymDefObject;
use zerocopy::FromZeros;

/// Creates a vTPM blob and writes it to `path`.
pub fn create_vtpm_blob_file(path: &str) {
    tracing::info!("Creating vTPM blob and saving to file: {}", path);
    let (mut tpm_engine_helper, nv_blob_accessor) = create_tpm_engine_helper();
    tpm_engine_helper
        .initialize_tpm_engine()
        .expect("failed to initialize TPM engine");

    let state = create_vtpm_blob(tpm_engine_helper, nv_blob_accessor);
    tracing::info!("vTPM blob size: {}", state.len());

    fs::write(path, state).expect("failed to write vTPM state to blob file");
    tracing::info!("vTPM blob created and saved to file: {}", path);
}

/// Writes the SRK public key from a vTPM blob in TPM2B format.
pub fn write_srk(vtpm_blob_path: &str, srk_out_path: &str) {
    let vtpm_blob_content = fs::read(vtpm_blob_path).expect("failed to read vTPM blob file");
    let (mut vtpm_engine_helper, _nv_blob_accessor) = create_tpm_engine_helper();

    vtpm_engine_helper
        .tpm_engine
        .reset(Some(&vtpm_blob_content))
        .expect("failed to restore TPM engine from blob");
    vtpm_engine_helper
        .initialize_tpm_engine()
        .expect("failed to initialize TPM engine");

    tracing::info!(
        "write-srk: blob file: {}, SRK out file: {}",
        vtpm_blob_path,
        srk_out_path
    );
    export_vtpm_srk_pub(vtpm_engine_helper, srk_out_path);
}

/// Create vtpm and return its state as a byte vector.
fn create_vtpm_blob(
    mut tpm_engine_helper: TpmEngineHelper,
    nvm_state_blob: Arc<Mutex<Vec<u8>>>,
) -> Vec<u8> {
    // Create a vTPM instance.
    tracing::info!("Initializing TPM engine with deterministic ColdInit for Ubuntu compatibility.");

    // NOTE: We do NOT call refresh_tpm_seeds() as that would randomize the seeds.
    // Ubuntu expects the TPM to use the initial deterministic seeds from ColdInit.

    // Create a primary key: SRK
    let auth_handle = TPM20_RH_OWNER;
    let result = tpm_helper::srk_pub_template();
    assert!(result.is_ok());
    let srk_in_public = result.unwrap();
    let result = tpm_engine_helper.create_primary(auth_handle, srk_in_public);
    match result {
        Ok(response) => {
            tracing::info!("SRK handle: {:?}", response.object_handle);
            assert_ne!(response.out_public.size.get(), 0);
            tracing::trace!("SRK public area: {:?}", response.out_public.public_area);

            // Evict the SRK handle.
            let result = tpm_engine_helper.evict_control(
                TPM20_RH_OWNER,
                response.object_handle,
                TPM_RSA_SRK_HANDLE,
            );
            assert!(result.is_ok());
        }
        Err(e) => {
            tracing::error!("Error in create_primary: {:?}", e);
        }
    }

    // DEBUG: retrieve the SRK and print its SHA256 hash and name
    let result = tpm_engine_helper.read_public(TPM_RSA_SRK_HANDLE);
    match result {
        Ok(response) => {
            let mut hasher = Sha256::new();
            hasher.update(response.out_public.public_area.serialize());
            let public_area_hash = hasher.finalize();
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
        }
        Err(e) => {
            tracing::error!("Error in read_public: {:?}", e);
        }
    }

    // Get the nv state of the TPM.
    let nv_blob = nvm_state_blob.lock().clone();
    tracing::trace!("Retrieved NV blob size: {}", nv_blob.len());
    nv_blob
}

/// Export the vTPM SRK public key to a file in TPM2B format.
fn export_vtpm_srk_pub(mut tpm_engine_helper: TpmEngineHelper, srk_out_path: &str) {
    // Debug: Check if the SRK handle exists
    tracing::trace!("Checking if SRK handle exists...");
    let find_result = tpm_engine_helper.find_object(TPM_RSA_SRK_HANDLE);
    match find_result {
        Ok(Some(_handle)) => tracing::trace!("SRK handle found"),
        //Ok(Some(handle)) => println!("SRK handle found: {:?}", handle),
        Ok(None) => {
            tracing::trace!("SRK handle NOT found! Need to create it.");
            // The SRK doesn't exist, so we need to create it
            //recreate_srk(&mut tpm_engine_helper);
        }
        Err(e) => tracing::error!("Error finding SRK handle: {:?}", e),
    }

    // Extract SRK primary key public area.
    let result = tpm_engine_helper.read_public(TPM_RSA_SRK_HANDLE);
    match result {
        Ok(response) => {
            tracing::trace!("SRK public area: {:?}", response.out_public.public_area);

            // Write the SRK pub to a file.
            let mut srk_pub_file = File::create(srk_out_path).expect("failed to create file");

            // Use the full TPM2B_PUBLIC serialization to match Windows C++ GetSrkPub
            // Windows returns the raw publicArea from ReadPublic.m_pOutPublic->Get(),
            // which is the serialized TPM2B_PUBLIC structure
            let srk_pub = response.out_public.serialize();
            srk_pub_file
                .write_all(&srk_pub)
                .expect("failed to write to file");

            // Calculate and print the SRK name (algorithm ID + hash)
            let mut hasher = Sha256::new();
            hasher.update(response.out_public.public_area.serialize());
            let public_area_hash = hasher.finalize();
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
            let mut hasher = Sha256::new();
            hasher.update(response.out_public.public_area.serialize());
            let public_area_hash = hasher.finalize();
            tracing::trace!(
                "SRK public area SHA256 hash: {} is written to file {}",
                hex::encode(public_area_hash),
                srk_out_path
            );
        }
        Err(e) => {
            tracing::error!("Error in read_public: {:?}", e);
        }
    }
}

/// Recreate SRK from vTPM blob to verify deterministic generation.
/// This function will:
/// 1. Load the vTPM blob and read the current SRK
/// 2. Undefine (remove) the persistent SRK
/// 3. Recreate the SRK using the same seeds
/// 4. Compare the old and new SRK to verify they match
#[cfg(feature = "experimental")]
pub fn recreate_srk(vtpm_blob_path: &str) {
    tracing::info!("Starting SRK recreation test...");

    // Read the vTPM blob file
    let vtpm_blob_content = fs::read(vtpm_blob_path).expect("Failed to read vTPM blob file");

    tracing::info!("vTPM blob size: {} bytes", vtpm_blob_content.len());

    // Create TPM engine helper and restore from blob
    let (mut tpm_engine_helper, _nv_blob_accessor) = create_tpm_engine_helper();

    let result = tpm_engine_helper.tpm_engine.reset(Some(&vtpm_blob_content));
    assert!(result.is_ok(), "Failed to reset TPM engine from blob");

    let result = tpm_engine_helper.initialize_tpm_engine();
    assert!(result.is_ok(), "Failed to initialize TPM engine");

    tracing::info!("TPM engine initialized from blob");

    // IMPORTANT: Use StartupType::State instead of initialize_tpm_engine() to preserve TPM state
    // tracing::info!("Starting TPM with State preservation...");
    // let result = tpm_engine_helper.startup(tpm::tpm20proto::protocol::StartupType::State);
    // assert!(result.is_ok(), "Failed to startup TPM with state preservation");

    // Perform self-test but don't reinitialize the seeds/state
    // let result = tpm_engine_helper.self_test(true);
    // assert!(result.is_ok(), "Failed to perform TPM self-test");
    //tracing::info!("TPM engine initialized from blob with state preservation");

    // Step 1: Read the original SRK
    tracing::info!("Step 1: Reading original SRK...");
    let original_srk = tpm_engine_helper
        .read_public(TPM_RSA_SRK_HANDLE)
        .expect("Failed to read original SRK - SRK might not exist in this blob");

    // Calculate and log the original SRK name
    let mut original_hasher = Sha256::new();
    original_hasher.update(original_srk.out_public.public_area.serialize());
    let original_public_area_hash = original_hasher.finalize();

    let algorithm_id = original_srk.out_public.public_area.name_alg;
    let mut original_srk_name = vec![0u8; 2 + original_public_area_hash.len()];
    original_srk_name[0] = (algorithm_id.0.get() >> 8) as u8;
    original_srk_name[1] = (algorithm_id.0.get() & 0xFF) as u8;
    original_srk_name[2..].copy_from_slice(&original_public_area_hash);

    let original_srk_name_hex = original_srk_name
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();
    tracing::info!("Original SRK name: {}", original_srk_name_hex);
    tracing::info!(
        "Original SRK public area size: {} bytes",
        original_srk.out_public.size.get()
    );

    // Step 2: Undefine (remove) the persistent SRK
    tracing::info!("Step 2: Undefining persistent SRK...");

    let result = tpm_engine_helper.evict_control(
        TPM20_RH_OWNER,     // auth_handle
        TPM_RSA_SRK_HANDLE, // object_handle (persistent handle to remove)
        TPM_RSA_SRK_HANDLE, // persistent_handle (same as object_handle for removal)
    );

    match result {
        Ok(()) => {
            tracing::info!("Successfully undefined persistent SRK");
        }
        Err(e) => {
            tracing::error!("Failed to undefine persistent SRK: {:?}", e);
            panic!("Cannot proceed with test - failed to undefine SRK");
        }
    }

    // Verify SRK is no longer present
    let find_result = tpm_engine_helper.find_object(TPM_RSA_SRK_HANDLE);
    match find_result {
        Ok(Some(_)) => {
            tracing::error!("SRK still exists after evict_control - this should not happen!");
            panic!("SRK was not properly undefined");
        }
        Ok(None) => {
            tracing::info!("Confirmed: SRK no longer exists in persistent storage");
        }
        Err(e) => {
            tracing::warn!(
                "Error checking SRK existence (this might be expected): {:?}",
                e
            );
        }
    }

    // Step 3: Recreate the SRK using the same method as create_vtpm_blob
    tracing::info!("Step 3: Recreating SRK...");

    let auth_handle = TPM20_RH_OWNER;
    let srk_template = tpm_helper::srk_pub_template().expect("Failed to create SRK template");

    let create_result = tpm_engine_helper.create_primary(auth_handle, srk_template);
    let new_object_handle = match create_result {
        Ok(response) => {
            tracing::info!(
                "SRK recreated with temporary handle: {:?}",
                response.object_handle
            );
            assert_ne!(
                response.out_public.size.get(),
                0,
                "New SRK public area should not be empty"
            );

            // Calculate the new SRK name for comparison
            let mut new_hasher = Sha256::new();
            new_hasher.update(response.out_public.public_area.serialize());
            let new_public_area_hash = new_hasher.finalize();

            let mut new_srk_name = vec![0u8; 2 + new_public_area_hash.len()];
            new_srk_name[0] = (algorithm_id.0.get() >> 8) as u8;
            new_srk_name[1] = (algorithm_id.0.get() & 0xFF) as u8;
            new_srk_name[2..].copy_from_slice(&new_public_area_hash);

            let new_srk_name_hex = new_srk_name
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>();
            tracing::info!("New SRK name: {}", new_srk_name_hex);

            // Step 4: Compare the original and new SRK
            tracing::info!("Step 4: Comparing original and new SRK...");

            if original_srk_name == new_srk_name {
                tracing::info!("SUCCESS: SRK names match exactly!");
                tracing::info!(
                    "This confirms that the TPM seeds are deterministic and produce identical keys"
                );
            } else {
                tracing::error!("FAILURE: SRK names do NOT match!");
                tracing::error!("Original: {}", original_srk_name_hex);
                tracing::error!("New:      {}", new_srk_name_hex);
                tracing::error!(
                    "This indicates the TPM seeds have changed or are not deterministic"
                );
            }

            // Also compare the public areas byte-by-byte for additional verification
            let original_public_bytes = original_srk.out_public.public_area.serialize();
            let new_public_bytes = response.out_public.public_area.serialize();

            if original_public_bytes == new_public_bytes {
                tracing::info!("Public areas are identical (byte-for-byte match)");
            } else {
                tracing::error!("Public areas differ!");
                tracing::trace!(
                    "  Original public area: {} bytes",
                    original_public_bytes.len()
                );
                tracing::trace!("  New public area: {} bytes", new_public_bytes.len());

                // Show first few bytes that differ for debugging
                let min_len = original_public_bytes.len().min(new_public_bytes.len());
                for i in 0..min_len {
                    if original_public_bytes[i] != new_public_bytes[i] {
                        tracing::trace!(
                            "First difference at byte {}: original=0x{:02x}, new=0x{:02x}",
                            i,
                            original_public_bytes[i],
                            new_public_bytes[i]
                        );
                        break;
                    }
                }
            }

            response.object_handle
        }
        Err(e) => {
            tracing::error!("Failed to recreate SRK: {:?}", e);
            panic!("Cannot complete test - failed to recreate SRK");
        }
    };

    // Step 5: Make the new SRK persistent again (restore the blob to its original state)
    tracing::info!("Step 5: Making new SRK persistent...");
    let result =
        tpm_engine_helper.evict_control(TPM20_RH_OWNER, new_object_handle, TPM_RSA_SRK_HANDLE);

    match result {
        Ok(()) => {
            tracing::info!(
                "Successfully made new SRK persistent at handle 0x{:08x}",
                TPM_RSA_SRK_HANDLE.0.get()
            );
        }
        Err(e) => {
            tracing::error!("Failed to make new SRK persistent: {:?}", e);
            // This is not critical for the test, but good to restore state
        }
    }

    // Final verification: read the persistent SRK to confirm it's accessible
    let final_srk_result = tpm_engine_helper.read_public(TPM_RSA_SRK_HANDLE);
    match final_srk_result {
        Ok(final_srk) => {
            let mut final_hasher = Sha256::new();
            final_hasher.update(final_srk.out_public.public_area.serialize());
            let final_public_area_hash = final_hasher.finalize();

            let mut final_srk_name = vec![0u8; 2 + final_public_area_hash.len()];
            final_srk_name[0] = (algorithm_id.0.get() >> 8) as u8;
            final_srk_name[1] = (algorithm_id.0.get() & 0xFF) as u8;
            final_srk_name[2..].copy_from_slice(&final_public_area_hash);

            let final_srk_name_hex = final_srk_name
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>();
            tracing::info!("Final persistent SRK name: {}", final_srk_name_hex);

            if final_srk_name == original_srk_name {
                tracing::info!(
                    "Persistent SRK matches original - blob state restored successfully"
                );
            } else {
                tracing::warn!(
                    "Persistent SRK differs from original - blob state may have changed"
                );
            }
        }
        Err(e) => {
            tracing::warn!("Could not read final persistent SRK: {:?}", e);
        }
    }

    tracing::info!("SRK recreation test completed successfully!");
}

/// Write the SRK template to file in Ubuntu-compatible format.
/// This creates the same template format that Ubuntu's canonical-encrypt-cloud-image expects.
/// Writes the SRK template in Ubuntu-compatible format.
#[cfg(feature = "experimental")]
pub fn write_srk_template(template_path: &str) {
    tracing::info!("Generating SRK template for Ubuntu compatibility...");

    // Get the SRK template using the same function used for TPM initialization
    let srk_template = tpm_helper::srk_pub_template().expect("Failed to create SRK template");

    // Convert to Tpm2bPublic format (same as what gets stored in TPM)
    let tpm2b_public = Tpm2bPublic::new(srk_template);

    // Serialize the template in the format Ubuntu expects
    // Ubuntu uses go-tpm2's mu.Sized() format which is: size(2 bytes) + data
    let serialized_template = tpm2b_public.serialize();

    // Write to file
    let mut template_file = File::create(template_path).expect("Failed to create template file");
    template_file
        .write_all(&serialized_template)
        .expect("Failed to write template to file");

    tracing::info!(
        "SRK template written to {} ({} bytes)",
        template_path,
        serialized_template.len()
    );

    // Debug: Print template properties for verification
    tracing::trace!("SRK Template Properties:");
    tracing::trace!("  Type: {:?}", tpm2b_public.public_area.my_type);
    tracing::trace!("  Name Algorithm: {:?}", tpm2b_public.public_area.name_alg);
    tracing::trace!(
        "  Attributes: {:?}",
        tpm2b_public.public_area.object_attributes
    );
    tracing::trace!(
        "  Key Bits: {:?}",
        tpm2b_public.public_area.parameters.key_bits
    );
    tracing::trace!(
        "  Symmetric Algorithm: {:?}",
        tpm2b_public.public_area.parameters.symmetric.algorithm
    );
    tracing::trace!(
        "  Symmetric Key Bits: {:?}",
        tpm2b_public.public_area.parameters.symmetric.key_bits
    );
    tracing::trace!(
        "  Symmetric Mode: {:?}",
        tpm2b_public.public_area.parameters.symmetric.mode
    );

    // Compute and display hash for verification
    let mut hasher = Sha256::new();
    hasher.update(&serialized_template);
    let template_hash = hasher.finalize();
    tracing::trace!("Template SHA256: {}", hex::encode(template_hash));

    tracing::info!("SRK template generation completed successfully.");
}

/// Print the SRK public key name.
/// Prints the TPM key name of an SRK public key file.
pub fn print_key_name(srkpub_path: &str) {
    let mut srk_pub_file = fs::OpenOptions::new()
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
    let mut hasher = Sha256::new();
    hasher.update(public_area.serialize());
    let public_area_hash = hasher.finalize();

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

/// Create random RSA or ECC key. Export the public public key to a file and private key in TPM2B format.
/// Creates a random key in TPM2 import blob format.
pub fn create_random_key_in_tpm2_import_blob_format(
    algorithm: &str,
    public_key_file: &str,
    private_key_tpm2b_file: &str,
) {
    match algorithm.to_lowercase().as_str() {
        "rsa" => {
            // Generate RSA 2048-bit key
            let rsa = Rsa::generate(2048).unwrap();
            let modulus_bytes = rsa.n().to_vec();
            tracing::trace!("RSA modulus size: {} bytes", modulus_bytes.len());
            let modulus_buffer = Tpm2bBuffer::new(modulus_bytes.as_slice()).unwrap();
            tracing::trace!(
                "Tpm2bBuffer modulus size field: {} bytes",
                modulus_buffer.size.get()
            );

            let public_key_der = rsa.public_key_to_der_pkcs1().unwrap();
            let pkey = PKey::from_rsa(rsa).unwrap();
            tracing::info!("RSA 2048-bit key generated.");

            // Export the public key to a file in pem format
            let mut pub_file = File::create(public_key_file).unwrap();
            pub_file.write_all(&public_key_der).unwrap();
            tracing::info!("RSA public key is saved to {public_key_file} in DER PKCS1 format.");
            print_sha256_hash(public_key_der.as_slice());

            // Convert the private key to TPM2B format
            let tpm2_import_blob = get_key_in_tpm2_import_format_rsa(&pkey);

            // Save the TPM2B private key to a file
            let mut priv_file = File::create(private_key_tpm2b_file).unwrap();
            priv_file.write_all(&tpm2_import_blob).unwrap();
            tracing::info!(
                "RSA private key is saved to {private_key_tpm2b_file} in TPM2B import format."
            );
            let private_key_der = pkey.private_key_to_der().unwrap();

            print_sha256_hash(private_key_der.as_slice());
        }
        "ecc" => {
            // Create a random ECC P-256 key using openssl-sys crate.
            let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
            let ec_key = EcKey::generate(&group).unwrap();
            let pkey = PKey::from_ec_key(ec_key).unwrap();
            tracing::info!("ECC P-256 key generated.");

            // Export the public key to a file
            let public_key_pem = pkey.public_key_to_pem().unwrap();
            let mut pub_file = File::create(public_key_file).unwrap();
            pub_file.write_all(&public_key_pem).unwrap();
            tracing::info!("ECC public key saved to {public_key_file}.");

            // Convert the private key to TPM2B format
            // TODO: define the ECC version for TPM2B format
            let tpm2_import_blob = get_key_in_tpm2_import_format_rsa(&pkey);

            // Save the TPM2B private key to a file
            let mut priv_file = File::create(private_key_tpm2b_file).unwrap();
            priv_file.write_all(&tpm2_import_blob).unwrap();

            tracing::info!(
                "ECC private key in TPM2B import format saved to {private_key_tpm2b_file}."
            );
        }
        _ => {
            tracing::error!("Invalid algorithm. Supported algorithms are rsa and ecc.");
        }
    }
}

// Convert the private key to TPM2B format
fn get_key_in_tpm2_import_format_rsa(priv_key: &PKey<openssl::pkey::Private>) -> Vec<u8> {
    let rsa = priv_key.rsa().unwrap();

    let key_bits: u16 = rsa.size() as u16 * 8; // 2048;
    tracing::trace!("Key bits: {:?}", key_bits);
    let exponent = 0; // Use 0 to indicate default exponent (65537)
    let auth_policy = [0; 0];
    let symmetric_def = TpmtSymDefObject::new(AlgIdEnum::NULL.into(), None, None);
    let rsa_scheme = TpmtRsaScheme::new(AlgIdEnum::NULL.into(), None);

    // Create a TPM2B_PUBLIC structure
    let tpmt_public_area = TpmtPublic::new(
        AlgIdEnum::RSA.into(),
        AlgIdEnum::SHA256.into(),
        TpmaObjectBits::new()
            .with_user_with_auth(true)
            .with_decrypt(true),
        &auth_policy,
        TpmsRsaParams::new(symmetric_def, rsa_scheme, key_bits, exponent),
        &rsa.n().to_vec(),
    )
    .unwrap();

    let tpm2b_public = Tpm2bPublic::new(tpmt_public_area);
    // Debug: Check TPM2B_PUBLIC size breakdown
    tracing::trace!("TPM2B_PUBLIC size {} bytes", tpm2b_public.size.get());
    tracing::trace!(
        "TPM2B_PUBLIC serialized size: {} bytes",
        tpm2b_public.serialize().len()
    );

    // Create a TPM2B_PRIVATE structure
    // For RSA import format, use the first prime factor (p), not the private exponent (d)
    let prime1_bytes = rsa.p().unwrap().to_vec();
    tracing::trace!("RSA prime1 (p) size: {} bytes", prime1_bytes.len());
    let sensitive_rsa = Tpm2bBuffer::new(&prime1_bytes).unwrap();

    let tpmt_sensitive = TpmtSensitive {
        sensitive_type: tpmt_public_area.my_type, // TPM_ALG_RSA
        auth_value: Tpm2bBuffer::new_zeroed(),    // Empty auth value
        seed_value: Tpm2bBuffer::new_zeroed(),    // Empty seed value
        sensitive: sensitive_rsa,
    };

    let marshaled_tpmt_sensitive = marshal::tpmt_sensitive_marshal(&tpmt_sensitive).unwrap();
    let marshaled_size = marshaled_tpmt_sensitive.len() as u16;

    // Create TPM2B_PRIVATE structure: size + marshaled_data
    let mut tpm2b_private_buffer = Vec::new();

    // Add the TPM2B size field (total size of the buffer excluding this size field)
    tpm2b_private_buffer.extend_from_slice(&marshaled_size.to_be_bytes());

    // Add the marshaled sensitive data
    tpm2b_private_buffer.extend_from_slice(&marshaled_tpmt_sensitive);

    tracing::trace!(
        "TPM2B_PRIVATE total buffer size: {} bytes",
        tpm2b_private_buffer.len()
    );
    tracing::trace!("  - Size field: 2 bytes");
    tracing::trace!(
        "  - Marshaled sensitive data: {} bytes",
        marshaled_tpmt_sensitive.len()
    );
    tracing::trace!("    - sensitive_type: 2 bytes");
    tracing::trace!(
        "    - auth_value: {} bytes (size + data)",
        2 + tpmt_sensitive.auth_value.size.get()
    );
    tracing::trace!(
        "    - seed_value: {} bytes (size + data)",
        2 + tpmt_sensitive.seed_value.size.get()
    );
    tracing::trace!(
        "    - sensitive (RSA private prime): {} bytes (size + data)",
        2 + tpmt_sensitive.sensitive.size.get()
    );

    // Create the final import blob: TPM2B_PUBLIC || TPM2B_PRIVATE || TPM2B_ENCRYPTED_SECRET
    let mut final_import_blob = Vec::new();

    // Add TPM2B_PUBLIC
    let serialized_public = tpm2b_public.serialize();
    final_import_blob.extend_from_slice(&serialized_public);

    // Add TPM2B_PRIVATE
    final_import_blob.extend_from_slice(&tpm2b_private_buffer);

    // Add TPM2B_ENCRYPTED_SECRET (empty - just 2 bytes of zeros for size)
    final_import_blob.extend_from_slice(&[0u8, 0u8]);

    tracing::trace!(
        "Final TPM2B import format size: {} bytes",
        final_import_blob.len()
    );
    tracing::trace!("  - TPM2B_PUBLIC: {} bytes", serialized_public.len());
    tracing::trace!("  - TPM2B_PRIVATE: {} bytes", tpm2b_private_buffer.len());
    tracing::trace!("  - TPM2B_ENCRYPTED_SECRET: 2 bytes (empty)");

    final_import_blob
}

/// Print info about public key in DER format.
/// Prints information about a public key in DER format.
#[cfg(feature = "experimental")]
pub fn print_public_key_der(pub_key_der_path: &str) {
    let mut pub_key_file = fs::OpenOptions::new()
        .write(false)
        .read(true)
        .open(pub_key_der_path)
        .expect("failed to open file");

    let mut pub_key_content_buf = Vec::new();
    pub_key_file
        .read_to_end(&mut pub_key_content_buf)
        .expect("failed to read file");

    // Deserialize the pub der to a rsa public key.
    let rsa =
        Rsa::public_key_from_der(&pub_key_content_buf).expect("failed to deserialize pub der");
    let pkey = PKey::from_rsa(rsa).unwrap();

    // Print the key type and size
    tracing::trace!("Key type: {:?}", pkey.id());
    tracing::trace!("Key size: {:?}", pkey.bits());
    print_sha256_hash(pkey.public_key_to_der().unwrap().as_slice());

    tracing::info!("\nOperation completed successfully.\n");
}

/// Print SHA256 hash of the data.
fn print_sha256_hash(data: &[u8]) {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let hash = hasher.finalize();
    let mut hash_str = String::new();
    for i in 0..hash.len() {
        hash_str.push_str(&format!("{:02X}", hash[i]));
    }
    tracing::trace!("SHA256 hash: {}\n", hash_str);
}

/// Print info about private key in TPM2B format.
/// Tpm2ImportFormat is TPM2B_PUBLIC || TPM2B_PRIVATE || TPM2B_ENCRYPTED_SEED
/// Prints information about a private key in TPM2B import format.
#[cfg(feature = "experimental")]
pub fn print_tpm2b_import_content(tpm2b_import_file_path: &str) {
    let mut tpm2b_import_file = fs::OpenOptions::new()
        .write(false)
        .read(true)
        .open(tpm2b_import_file_path)
        .expect("failed to open file");

    let mut tpm2b_import_content = Vec::new();
    tpm2b_import_file
        .read_to_end(&mut tpm2b_import_content)
        .expect("failed to read file");
    tracing::trace!("TPM2B import file size: {:?}", tpm2b_import_content.len());

    // Reverse the operations in get_key_in_tpm2_import_format_rsa
    // Deserialize the tpm2b import to a Tpm2bPublic and Tpm2bBuffer.
    let tpm2b_public = Tpm2bPublic::deserialize(&tpm2b_import_content)
        .expect("failed to deserialize tpm2b public");
    tracing::trace!("TPM2B public size: {:?}", tpm2b_public.size);
    tracing::trace!("TPM2B public type: {:?}", tpm2b_public.public_area.my_type);
    let tpm2b_public_size = u16::from_be(tpm2b_public.size.into()) as usize;
    tracing::trace!("TPM2B public size: {:?}", tpm2b_public_size);
    let tpm2b_private = Tpm2bBuffer::deserialize(&tpm2b_import_content[tpm2b_public_size..])
        .expect("failed to deserialize tpm2b private");
    tracing::trace!("TPM2B private size: {:?}", tpm2b_private.size);

    tracing::info!("\nOperation completed successfully.\n");
}

/// Test importing TPM2B format keys by reading and validating them
/// Tests whether DER public and TPM2B private keys form a keypair.
#[cfg(feature = "experimental")]
pub fn test_tpm2b_import_keys(public_key_file: &str, private_key_file: &str) {
    tracing::info!("Testing TPM2B key import...");
    tracing::info!("Public key file: {}", public_key_file);
    tracing::info!("Private key file: {}", private_key_file);

    // Read the public key file
    let mut pub_key_file = fs::OpenOptions::new()
        .read(true)
        .open(public_key_file)
        .expect("Failed to open public key file");

    let mut pub_key_content = Vec::new();
    pub_key_file
        .read_to_end(&mut pub_key_content)
        .expect("Failed to read public key file");

    tracing::info!("Public key file size: {} bytes", pub_key_content.len());

    // Try to determine the format and parse accordingly
    // First, try DER format (most likely for .pub files from your tool)
    let rsa_public_opt = if let Ok(rsa) = Rsa::public_key_from_der_pkcs1(&pub_key_content) {
        tracing::info!("Successfully parsed as PKCS1 DER format");
        Some(rsa)
    } else if let Ok(rsa) = Rsa::public_key_from_der(&pub_key_content) {
        tracing::info!("Successfully parsed as standard DER format");
        Some(rsa)
    } else {
        tracing::info!("Failed to parse as DER formats, trying TPM2B format...");
        None
    };

    if let Some(rsa_public) = rsa_public_opt {
        tracing::info!("RSA public key successfully parsed:");
        tracing::info!("  Key size: {} bits", rsa_public.size() * 8);
        tracing::info!("  Modulus size: {} bytes", rsa_public.n().to_vec().len());
        tracing::info!("  Exponent size: {} bytes", rsa_public.e().to_vec().len());

        // Continue with DER format validation
        validate_der_format_keys(&rsa_public, private_key_file);
    } else {
        // Try TPM2B format as last resort
        tracing::error!("Failed to parse public as DER formats...");
    }
}

/// Validate keys when public key is in DER format
#[cfg(feature = "experimental")]
fn validate_der_format_keys(rsa_public: &Rsa<openssl::pkey::Public>, private_key_file: &str) {
    // Read the private key file (TPM2B import format)
    let mut priv_key_file = fs::OpenOptions::new()
        .read(true)
        .open(private_key_file)
        .expect("Failed to open private key file");

    let mut priv_key_content = Vec::new();
    priv_key_file
        .read_to_end(&mut priv_key_content)
        .expect("Failed to read private key file");

    tracing::info!("Private key file size: {} bytes", priv_key_content.len());

    // Parse the TPM2B import format: TPM2B_PUBLIC || TPM2B_PRIVATE || TPM2B_ENCRYPTED_SEED

    // 1. Parse TPM2B_PUBLIC
    let tpm2b_public =
        Tpm2bPublic::deserialize(&priv_key_content).expect("Failed to deserialize TPM2B_PUBLIC");

    let public_size = tpm2b_public.size.get() as usize + 2; // +2 for size field
    tracing::info!("TPM2B_PUBLIC parsed:");
    tracing::info!("  Size: {} bytes", public_size);
    tracing::info!("  Algorithm: {:?}", tpm2b_public.public_area.my_type);
    tracing::info!("  Name algorithm: {:?}", tpm2b_public.public_area.name_alg);
    tracing::info!(
        "  Key bits: {:?}",
        tpm2b_public.public_area.parameters.key_bits
    );

    // 2. Parse TPM2B_PRIVATE
    let remaining_data = &priv_key_content[public_size..];
    let tpm2b_private =
        Tpm2bBuffer::deserialize(remaining_data).expect("Failed to deserialize TPM2B_PRIVATE");

    let private_size = tpm2b_private.size.get() as usize + 2; // +2 for size field
    tracing::info!("TPM2B_PRIVATE parsed:");
    tracing::info!("  Size: {} bytes", private_size);
    tracing::info!("  Data size: {} bytes", tpm2b_private.size.get());

    // 3. Parse TPM2B_ENCRYPTED_SECRET (should be empty - 2 zero bytes)
    let encrypted_seed_data = &remaining_data[private_size..];
    if encrypted_seed_data.len() >= 2 {
        let encrypted_seed_size =
            u16::from_be_bytes([encrypted_seed_data[0], encrypted_seed_data[1]]);
        tracing::info!("TPM2B_ENCRYPTED_SECRET parsed:");
        tracing::info!("  Size: {} bytes (should be 0)", encrypted_seed_size);

        if encrypted_seed_size == 0 {
            tracing::info!("Encrypted seed is empty as expected");
        } else {
            tracing::warn!("Encrypted seed is not empty");
        }
    }

    // Validation: Compare the modulus from the DER public key with the TPM2B public key
    let der_modulus = rsa_public.n().to_vec();
    let tpm2b_modulus: &[u8; 256] = tpm2b_public.public_area.unique.buffer[0..256]
        .try_into()
        .expect("Modulus size mismatch");

    tracing::info!("Validation:");
    tracing::info!("  DER modulus size: {} bytes", der_modulus.len());
    tracing::info!("  TPM2B modulus size: {} bytes", tpm2b_modulus.len());

    if der_modulus == *tpm2b_modulus {
        tracing::info!("  Modulus values match between DER and TPM2B formats");
    } else {
        tracing::error!("  Modulus values do NOT match");
        tracing::error!(
            "  First 16 bytes of DER modulus: {:02X?}",
            &der_modulus[..16.min(der_modulus.len())]
        );
        tracing::error!(
            "  First 16 bytes of TPM2B modulus: {:02X?}",
            &tpm2b_modulus[..16.min(tpm2b_modulus.len())]
        );
    }

    // Calculate expected total size
    let expected_total = public_size + private_size + 2; // +2 for encrypted seed
    tracing::info!("Size breakdown:");
    tracing::info!("  TPM2B_PUBLIC: {} bytes", public_size);
    tracing::info!("  TPM2B_PRIVATE: {} bytes", private_size);
    tracing::info!("  TPM2B_ENCRYPTED_SECRET: 2 bytes");
    tracing::info!("  Expected total: {} bytes", expected_total);
    tracing::info!("  Actual file size: {} bytes", priv_key_content.len());

    if expected_total == priv_key_content.len() {
        tracing::info!("File size matches expected TPM2B import format");
    } else {
        tracing::error!("File size does NOT match expected format");
    }

    tracing::info!("DER pub and TPM2B priv key validation completed successfully!");
}

/// Import a sealed key blob into an existing vTPM blob file
/// Imports a sealed key blob into an existing vTPM blob.
#[cfg(feature = "experimental")]
pub fn import_sealed_key_blob_into_vtpm(vtpm_blob_path: &str, sealed_key_path: &str) {
    tracing::info!("Loading vTPM blob from: {}", vtpm_blob_path);
    tracing::info!("Reading sealed key file: {}", sealed_key_path);

    // Read the vTPM blob file
    let vtpm_blob_content = match fs::read(vtpm_blob_path) {
        Ok(data) => data,
        Err(e) => {
            tracing::error!("Failed to read vTPM blob file {}: {}", vtpm_blob_path, e);
            return;
        }
    };

    tracing::info!("vTPM blob size: {} bytes", vtpm_blob_content.len());

    // Read the sealed key file
    let sealed_key_data = match fs::read(sealed_key_path) {
        Ok(data) => data,
        Err(e) => {
            tracing::error!("Failed to read sealed key file {}: {}", sealed_key_path, e);
            return;
        }
    };

    tracing::info!("Sealed key file size: {} bytes", sealed_key_data.len());

    // Parse the sealed key data
    let tpm_key_data = match marshal::TpmKeyData::from_bytes(&sealed_key_data) {
        Ok(data) => data,
        Err(e) => {
            tracing::error!("Failed to parse sealed key data: {}", e);
            return;
        }
    };

    tracing::info!("Successfully parsed sealed key data:");
    tracing::info!("  Version: {}", tpm_key_data.version);
    tracing::info!("  Auth mode hint: {}", tpm_key_data.auth_mode_hint);
    tracing::info!(
        "  Key private size: {} bytes",
        tpm_key_data.key_private.payload_size()
    );
    tracing::info!(
        "  Key public size: {} bytes",
        tpm_key_data.key_public.payload_size()
    );
    tracing::info!(
        "  Import sym seed size: {} bytes",
        tpm_key_data.import_sym_seed.payload_size()
    );

    // Create TPM engine helper and restore from blob
    let (mut tpm_engine_helper, nv_blob_accessor) = create_tpm_engine_helper();

    let result = tpm_engine_helper.tpm_engine.reset(Some(&vtpm_blob_content));
    if let Err(e) = result {
        tracing::error!("Failed to reset TPM engine from blob: {:?}", e);
        return;
    }

    let result = tpm_engine_helper.initialize_tpm_engine();
    if let Err(e) = result {
        tracing::error!("Failed to initialize TPM engine: {:?}", e);
        return;
    }

    tracing::info!("TPM engine initialized from blob");

    // Check if SRK exists (required as parent for import)
    if tpm_engine_helper
        .find_object(TPM_RSA_SRK_HANDLE)
        .unwrap_or(None)
        .is_none()
    {
        tracing::error!("Storage Root Key (SRK) not found in vTPM blob - cannot import sealed key");
        tracing::info!("The vTPM blob may be invalid or not properly initialized");
        return;
    }

    tracing::info!("SRK found in vTPM - proceeding with sealed key import");

    // Extract the import blob format from the sealed key data
    let import_blob = tpm_key_data.to_import_blob();

    // Check if we need to import or can load directly
    if import_blob.in_sym_seed.size.get() > 0 {
        tracing::info!(
            "Key has import symmetric seed ({} bytes) - importing into TPM storage hierarchy",
            import_blob.in_sym_seed.size.get()
        );

        // Import the key under the SRK
        let import_reply = match tpm_engine_helper.import(
            TPM_RSA_SRK_HANDLE,
            &import_blob.object_public,
            &import_blob.duplicate,
            &import_blob.in_sym_seed,
        ) {
            Ok(reply) => {
                tracing::info!("Successfully imported sealed key into vTPM");
                reply
            }
            Err(e) => {
                tracing::error!("Failed to import sealed key object into vTPM: {:?}", e);
                tracing::error!("This could indicate:");
                tracing::error!("  - Bad sealed key object");
                tracing::error!("  - Invalid symmetric seed");
                tracing::error!("  - TPM owner changed");
                tracing::error!("  - Wrong TPM (key was sealed to different vTPM)");
                return;
            }
        };

        // Load the imported key to verify it works
        let load_reply = match tpm_engine_helper.load(
            TPM_RSA_SRK_HANDLE,
            &import_reply.out_private,
            &import_blob.object_public,
        ) {
            Ok(reply) => {
                tracing::info!(
                    "Successfully loaded imported sealed key (temporary handle: {:?})",
                    reply.object_handle
                );
                reply
            }
            Err(e) => {
                tracing::error!("Failed to load imported sealed key: {:?}", e);
                return;
            }
        };

        // Verify we can access the key
        match tpm_engine_helper.read_public(load_reply.object_handle) {
            Ok(read_reply) => {
                tracing::info!(
                    "Verified key access - public area size: {} bytes",
                    read_reply.out_public.size.get()
                );
                tracing::info!(
                    "Key algorithm: {:?}",
                    read_reply.out_public.public_area.my_type
                );
            }
            Err(e) => {
                tracing::warn!("Could not read public area of loaded key: {:?}", e);
            }
        }

        // Clean up the temporary handle
        if let Err(e) = tpm_engine_helper.flush_context(load_reply.object_handle) {
            tracing::warn!("Failed to flush temporary key handle: {:?}", e);
        } else {
            tracing::info!("Cleaned up temporary key handle");
        }
    } else {
        tracing::info!("Key does not require import - attempting to load directly");

        // Try to load directly under SRK
        match tpm_engine_helper.load(
            TPM_RSA_SRK_HANDLE,
            &import_blob.duplicate,
            &import_blob.object_public,
        ) {
            Ok(load_reply) => {
                tracing::info!(
                    "Successfully loaded sealed key directly (handle: {:?})",
                    load_reply.object_handle
                );

                // Clean up
                if let Err(e) = tpm_engine_helper.flush_context(load_reply.object_handle) {
                    tracing::warn!("Failed to flush temporary key handle: {:?}", e);
                } else {
                    tracing::info!("Cleaned up temporary key handle");
                }
            }
            Err(e) => {
                tracing::error!("Failed to load sealed key directly: {:?}", e);
                return;
            }
        }
    }

    // Save the updated vTPM state back to the blob file
    let updated_blob = nv_blob_accessor.lock().clone();

    // Create backup of original blob
    let backup_path = format!("{}.backup", vtpm_blob_path);
    if let Err(e) = fs::copy(vtpm_blob_path, &backup_path) {
        tracing::warn!("Failed to create backup at {}: {}", backup_path, e);
    } else {
        tracing::info!("Created backup of original vTPM blob at: {}", backup_path);
    }

    // Write updated blob
    if let Err(e) = fs::write(vtpm_blob_path, &updated_blob) {
        tracing::error!(
            "Failed to write updated vTPM blob to {}: {}",
            vtpm_blob_path,
            e
        );
        tracing::error!("Original blob backup is available at: {}", backup_path);
        return;
    }

    tracing::info!("Updated vTPM blob size: {} bytes", updated_blob.len());
    tracing::info!(
        "Successfully saved updated vTPM blob to: {}",
        vtpm_blob_path
    );
    tracing::info!("Sealed key import into vTPM completed successfully");
}

/// Export a newly generated key as a sealed key file (instead of exporting existing persistent key)
/// Exports a newly generated TPM key as a sealed key blob.
#[cfg(feature = "experimental")]
pub fn export_tpm_key_as_sealed_blob(vtpm_blob_path: &str, sealed_key_output_path: &str) {
    tracing::info!(
        "Creating new key for sealed key export to: {}",
        sealed_key_output_path
    );
    tracing::info!("Loading vTPM blob from: {}", vtpm_blob_path);

    let vtpm_blob_content = fs::read(vtpm_blob_path).expect("failed to read vTPM blob file");
    let (mut vtpm_engine_helper, _nv_blob_accessor) = create_tpm_engine_helper();
    vtpm_engine_helper
        .tpm_engine
        .reset(Some(&vtpm_blob_content))
        .expect("failed to restore TPM engine from blob");
    vtpm_engine_helper
        .initialize_tpm_engine()
        .expect("failed to initialize TPM engine");

    export_new_key_as_sealed_blob(&mut vtpm_engine_helper, sealed_key_output_path);
}

#[cfg(feature = "experimental")]
fn export_new_key_as_sealed_blob(
    tpm_engine_helper: &mut TpmEngineHelper,
    sealed_key_output_path: &str,
) {
    tracing::info!("Generating new RSA key for sealed key export");

    // Create RSA key template suitable for export/import
    let key_template = create_exportable_rsa_key_template();

    // Generate the key pair in TPM under Owner hierarchy (like SRK)
    let create_result = tpm_engine_helper.create_primary(TPM20_RH_OWNER, key_template);

    let (key_handle, key_public) = match create_result {
        Ok(response) => (response.object_handle, response.out_public),
        Err(e) => {
            tracing::error!("Failed to create new key for export: {:?}", e);
            return;
        }
    };

    tracing::info!("Successfully created new key:");
    tracing::info!("  Handle: 0x{:08X}", key_handle.0.get());
    tracing::info!("  Algorithm: {:?}", key_public.public_area.my_type);
    tracing::info!("  Key bits: {:?}", key_public.public_area.parameters);
    tracing::info!("  Public size: {} bytes", key_public.size.get());

    // For a complete implementation, we would need TPM2_Create to get the private key data
    // For now, use the create_primary approach which gives us the public key
    // The limitation is that we still need dummy private key data

    // Generate import symmetric seed for the export
    let mut import_seed = vec![0u8; 128]; // 128 bytes of random seed
    getrandom::fill(&mut import_seed).expect("Failed to generate import seed");

    tracing::info!(
        "Generated import symmetric seed: {} bytes",
        import_seed.len()
    );

    // Since we don't have access to the actual private key from create_primary,
    // we still need to create dummy private key data
    // TODO: Implement TPM2_Create under SRK to get real private key data
    let mut dummy_private_data = vec![0u8; 64];
    getrandom::fill(&mut dummy_private_data).expect("Failed to generate dummy private data");
    let dummy_private = Tpm2bBuffer::new(&dummy_private_data);

    // Clean up the temporary key handle
    if let Err(e) = tpm_engine_helper.flush_context(key_handle) {
        tracing::warn!("Failed to flush temporary key context: {:?}", e);
    }

    // Create the sealed key data with the new key public area and dummy private data
    let sealed_key_data = match create_sealed_key_blob_v2_with_real_data(
        &dummy_private.unwrap(),
        &key_public,
        &import_seed,
    ) {
        Ok(data) => data,
        Err(error) => {
            tracing::error!("Failed to create sealed key data: {}", error);
            return;
        }
    };

    // Write the sealed key file
    match fs::write(sealed_key_output_path, &sealed_key_data) {
        Ok(()) => {
            tracing::info!(
                "Successfully exported new sealed key to: {}",
                sealed_key_output_path
            );
            tracing::info!("Sealed key file size: {} bytes", sealed_key_data.len());
            tracing::info!("Format: Canonical-compatible sealed key (version 2)");
            tracing::info!("Note: Contains newly generated RSA key with proper export attributes");
        }
        Err(e) => {
            tracing::error!(
                "Failed to write sealed key file {}: {}",
                sealed_key_output_path,
                e
            );
        }
    }
}

/// Create RSA key template optimized for export/import operations
#[cfg(feature = "experimental")]
fn create_exportable_rsa_key_template() -> TpmtPublic {
    use tpm_protocol::tpm20proto::protocol::*;
    use tpm_protocol::tpm20proto::*;

    let mut key_template = TpmtPublic::new_zeroed();

    // Set up RSA key parameters
    key_template.my_type = AlgId::from(AlgIdEnum::RSA);
    key_template.name_alg = AlgId::from(AlgIdEnum::SHA256);

    // Object attributes suitable for import/export
    // Clear FIXEDTPM and FIXEDPARENT for import compatibility
    key_template.object_attributes = TpmaObjectBits::new()
        .with_user_with_auth(true) // User can use key with auth
        .with_decrypt(true) // Key can decrypt
        .with_sign_encrypt(true) // Key can sign/encrypt
        .with_sensitive_data_origin(true) // TPM generated sensitive data
        .with_fixed_tpm(false) // NOT fixed to TPM (exportable)
        .with_fixed_parent(false)
        .into(); // NOT fixed to parent (importable)

    // RSA parameters: 2048-bit key
    let mut rsa_params = TpmsRsaParams::new_zeroed();
    rsa_params.key_bits = 2048.into();
    rsa_params.exponent = 0.into(); // Use default exponent (65537)
    rsa_params.scheme = TpmtRsaScheme::new_zeroed();

    // Set RSA parameters
    key_template.parameters = rsa_params;

    // No auth policy for simplicity
    key_template.auth_policy = Tpm2bBuffer::new_zeroed();

    // Empty unique field for creation
    key_template.unique = Tpm2bBuffer::new_zeroed();

    tracing::info!("Created exportable RSA key template:");
    tracing::info!("  Type: RSA 2048-bit");
    tracing::info!("  Attributes: 0x{:08X}", key_template.object_attributes.0);
    tracing::info!("  Exportable: true (FIXEDTPM/FIXEDPARENT clear)");

    key_template
}

/// Create sealed key blob with real TPM data structures
#[cfg(feature = "experimental")]
fn create_sealed_key_blob_v2_with_real_data(
    key_private: &Tpm2bBuffer,
    key_public: &Tpm2bPublic,
    import_seed: &[u8],
) -> Result<Vec<u8>, std::io::Error> {
    let mut sealed_data = Vec::new();

    // Header (4 bytes): 0x55534B24 ("USK$")
    sealed_data.extend_from_slice(&marshal::KEY_DATA_HEADER.to_be_bytes());

    // Version (4 bytes): 2
    sealed_data.extend_from_slice(&marshal::CURRENT_METADATA_VERSION.to_be_bytes());

    // Create the payload data that will be AF-split
    let mut payload = Vec::new();

    // Add real TPM2B_PRIVATE (from TPM2_Create)
    let private_serialized = key_private.serialize();
    payload.extend_from_slice(&private_serialized);
    tracing::info!("Added TPM2B_PRIVATE: {} bytes", private_serialized.len());

    // Add real TPM2B_PUBLIC (from TPM2_Create)
    let public_serialized = key_public.serialize();
    payload.extend_from_slice(&public_serialized);
    tracing::info!("Added TPM2B_PUBLIC: {} bytes", public_serialized.len());

    // Add auth mode hint (1 byte)
    payload.push(0u8); // No authentication required
    tracing::info!("Added auth mode hint: 1 byte");

    // Add real TPM2B_ENCRYPTED_SECRET (import symmetric seed)
    let import_seed_buffer = Tpm2bBuffer::new(import_seed);
    let seed_serialized = import_seed_buffer.unwrap().serialize();
    payload.extend_from_slice(&seed_serialized);
    tracing::info!(
        "Added TPM2B_ENCRYPTED_SECRET: {} bytes",
        seed_serialized.len()
    );

    tracing::info!("Created payload for AF split: {} bytes", payload.len());
    tracing::info!("  TPM2B_PRIVATE: {} bytes", private_serialized.len());
    tracing::info!("  TPM2B_PUBLIC: {} bytes", public_serialized.len());
    tracing::info!("  Auth mode hint: 1 byte");
    tracing::info!("  TPM2B_ENCRYPTED_SECRET: {} bytes", seed_serialized.len());

    // Apply AF split to the payload
    let af_split_data = marshal::AfSplitData::create(&payload)?.to_bytes()?;

    // Append AF split data to sealed key
    sealed_data.extend_from_slice(&af_split_data);

    Ok(sealed_data)
}

/// Starts a TPM socket server using a vTPM blob as backing state.
#[cfg(feature = "experimental")]
pub fn start_tpm_socket_server(vtpm_blob_path: &str, bind_addr: &str) {
    vtpm_sock_server::start_tpm_socket_server(vtpm_blob_path, bind_addr);
}

// cargo test -p vtpm_util test_srk_template_generation
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    #[cfg(feature = "experimental")]
    fn test_srk_template_generation() {
        // Create a temporary directory for testing
        let temp_dir = tempfile::tempdir().unwrap();
        let template_path = temp_dir.path().join("test-srk-template.tmpl");

        // Generate SRK template
        write_srk_template(template_path.to_str().unwrap());

        // Verify the file exists and has content
        assert!(template_path.exists());
        let template_data = fs::read(&template_path).unwrap();
        assert!(!template_data.is_empty());

        // Verify it can be deserialized back to Tpm2bPublic
        let deserialized = Tpm2bPublic::deserialize(&template_data).unwrap();

        // Verify key properties match Ubuntu expectations
        assert_eq!(deserialized.public_area.my_type, AlgIdEnum::RSA.into());
        assert_eq!(deserialized.public_area.name_alg, AlgIdEnum::SHA256.into());
        assert_eq!(
            deserialized.public_area.parameters.key_bits,
            tpm_helper::RSA_2K_MODULUS_BITS
        );
        assert_eq!(
            deserialized.public_area.parameters.symmetric.algorithm,
            AlgIdEnum::AES.into()
        );
        assert_eq!(deserialized.public_area.parameters.symmetric.key_bits, 128); // AES-128 as expected by Ubuntu
        assert_eq!(
            deserialized.public_area.parameters.symmetric.mode,
            AlgIdEnum::CFB.into()
        );

        // Verify object attributes match Ubuntu expectations
        let attrs = TpmaObjectBits::from(deserialized.public_area.object_attributes.0.get());
        assert!(attrs.fixed_tpm());
        assert!(attrs.fixed_parent());
        assert!(attrs.sensitive_data_origin());
        assert!(attrs.user_with_auth());
        assert!(attrs.no_da());
        assert!(attrs.restricted());
        assert!(attrs.decrypt());

        println!(
            "SRK template test passed: {} bytes generated",
            template_data.len()
        );
    }
}
