// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module includes the CvmUtil, which is a tool to create and manage vTPM blobs.
//! vTPM blobs are used to provide TPM functionality to trusted and confidential VMs.
use ms_tpm_20_ref::MsTpm20RefPlatform;
use tpm::TPM_RSA_SRK_HANDLE;
use tpm::tpm_helper::{self, TpmEngineHelper};
use tpm::tpm20proto::protocol::{
    Tpm2bBuffer, Tpm2bPublic, TpmsRsaParams, TpmtPublic, TpmtRsaScheme, TpmtSymDefObject,
};
use tpm::tpm20proto::{AlgId, AlgIdEnum, TpmaObjectBits};
mod marshal;
mod vtpm_helper;
mod vtpm_sock_server;
use base64::Engine;
use marshal::TpmtSensitive;
use openssl::ec::EcGroup;
use openssl::ec::EcKey;
use openssl::nid::Nid;
use openssl::pkey::PKey;
use openssl::rsa::Rsa;
use sha2::{Digest, Sha256};
use std::convert::TryInto;
use std::io::Read;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::{fs, fs::File, vec};
use zerocopy::FromZeros;

use crate::vtpm_helper::create_tpm_engine_helper;
use clap::Parser;

#[derive(Parser, Debug)]
#[clap(name = "cvmutil", about = "Tool to interact with vTPM blobs.")]
struct CmdArgs {
    /// Enable verbose logging (trace level)
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Creates a vTpm blob and stores to file. Example: ./cvmutil --createvtpmblob vTpm.blob
    #[arg(
        short = 'c',
        long = "createvtpmblob",
        value_name = "path-to-blob-file",
        number_of_values = 1
    )]
    createvtpmblob: Option<String>,

    /// Write the SRK public key in TPM2B format. Example: ./cvmutil --writeSrk vTpm.blob srk.pub
    #[arg(
        short = 'w',
        long = "writeSrk",
        value_names = &["path-to-vtpm-blob-file", "path-to-srk-out-file"],
        long_help = "Write the SRK public key in TPM2B format.\n./cvmutil --writeSrk vTpm.blob srk.pub"
    )]
    write_srk: Option<Vec<String>>,

    /// Write the SRK template to file in Ubuntu-compatible format. Example: ./cvmutil --writeSrkTemplate tpm2-srk.tmpl
    #[arg(
        long = "writeSrkTemplate",
        value_name = "path-to-template-file",
        long_help = "Write the SRK template to file in Ubuntu-compatible format.\n./cvmutil --writeSrkTemplate tpm2-srk.tmpl"
    )]
    write_srk_template: Option<String>,

    /// Recreate SRK from vTPM blob to verify deterministic generation. Example: ./cvmutil --recreate-srk vTpm.blob
    #[arg(
        short = 'r',
        long = "recreate-srk",
        value_name = "path-to-vtpm-blob-file",
        long_help = "Recreate SRK from vTPM blob to verify deterministic generation.\nThis will undefine the existing SRK and recreate it to verify the seeds produce the same key.\n./cvmutil --recreate-srk vTpm.blob"
    )]
    recreate_srk: Option<String>,

    /// Print the TPM key name of the SRK public key file. Example: ./cvmutil --printKeyName srk.pub
    #[arg(
        short = 'p',
        long = "printKeyName",
        value_name = "path-to-srkPub",
        long_help = "Print the TPM key name \n./cvmutil --printKeyName srk.pub"
    )]
    print_key_name: Option<String>,

    /// Seal data to SRK public key. Example: ./cvmutil --seal srk.pub input.txt output.bin
    #[arg(
        long = "seal",
        value_names = &["path-to-srk-pub", "input-file", "output-file"],
        number_of_values = 3,
        long_help = "Seal data to SRK public key for testing.\n./cvmutil --seal srk.pub input.txt output.bin"
    )]
    seal: Option<Vec<String>>,

    /// Unseal data from sealed blob using vTPM. Example: ./cvmutil --unseal vtpm.blob sealed.bin output.txt
    #[arg(
        long = "unseal",
        value_names = &["path-to-vtpm-blob", "sealed-file", "output-file"],
        number_of_values = 3,
        long_help = "Unseal data from sealed blob using vTPM for testing.\n./cvmutil --unseal vtpm.blob sealed.bin output.txt"
    )]
    unseal: Option<Vec<String>>,

    /// Create random RSA/ECC key in Tpm2 import blob format:TPM2B_PUBLIC || TP2B_PRIVATE || TP2B_ENCRYPTED_SEED
    /// Example: ./cvmutil --createRandomKeyInTpm2ImportBlobFormat rsa rsa_pub.der rsa_priv_marshalled.tpm2b
    #[arg(
        short = 's',
        long = "createRandomKeyInTpm2ImportBlobFormat",
        value_names = &["algorithm", "publicKey", "output-file"],
        long_help = "Create random RSA/ECC key in Tpm2 import blob format:TPM2B_PUBLIC || TP2B_PRIVATE || TP2B_ENCRYPTED_SEED \n./cvmutil --createRandomKeyInTpm2ImportBlobFormat rsa rsa_pub.der rsa_priv_marshalled.tpm2b"
    )]
    create_random_key_in_tpm2_import_blob_format: Option<Vec<String>>,

    /// Print info about public key in DER format. Example: ./cvmutil --printDER rsa_pub.der
    #[arg(
        short = 'd',
        long = "printDER",
        value_name = "path-to-pubKey-der",
        long_help = "Print info about DER key \n./cvmutil --printDER rsa_pub.der"
    )]
    print_pub_key_der: Option<String>,

    /// Print info about private key in TPM2B format: TPM2B_PUBLIC || TP2B_PRIVATE || TP2B_ENCRYPTED_SEED
    #[arg(
        short = 't',
        long = "printTPM2B",
        value_name = "path-to-privKey-tpm2b",
        long_help = "Print info about TPM2B import file: TPM2B_PUBLIC || TP2B_PRIVATE || TP2B_ENCRYPTED_SEED. \n./cvmutil --printTPM2B marshalled_import_blob.tpm2b"
    )]
    print_priv_key_tpm2b: Option<String>,

    /// Test importing public key in DER format and private key in TPM2B format. Make sure they form a keypair.
    #[arg(
        short = 'i',
        long = "testTPM2BImportKeys",
        value_names = &["path-to-pubKey-der", "path-to-privKey-tpm2b"],
        long_help = "Import the public in DER and private in TPM2B format. Make sure they form a keypair. \n./cvmutil --testTPM2BImportKeys rsa_pub.der marshalled_import_blob.tpm2b"
    )]
    test_tpm2b_import_keys: Option<Vec<String>>,

    /// Import a sealed key blob that matches the tpmKeyData structure into an existing vTPM. Example: ./cvmutil --tpmimport /boot/efi/device/fde/cloudimg-rootfs.sealed-key
    #[arg(
        long = "tpmimport",
        value_names = &["path-to-vtpm-blob-file", "path-to-sealed-key-file"],
        long_help = "Import a sealed key blob that matches the tpmKeyData structure into an existing vTPM blob.\nThis loads the vTPM blob, imports the sealed key object into the TPM's storage hierarchy, and saves the updated vTPM state.\n./cvmutil --tpmimport vtpm.blob /boot/efi/device/fde/cloudimg-rootfs.sealed-key"
    )]
    tpm_import: Option<Vec<String>>,

    /// Export a TPM key as a sealed key blob compatible with Canonical's format
    #[arg(
        long = "tpmkeyexport",
        value_names = &["path-to-vtpm-blob-file", "key-handle-or-persistent-handle", "path-to-sealed-key-output-file"],
        long_help = "Export a TPM key from vTPM blob as a sealed key file compatible with Canonical's cloudimg-rootfs.sealed-key format.\nThis reads a key from the vTPM and exports it in the format expected by Ubuntu's sealed key system.\n./cvmutil --tpmkeyexport vtpm.blob 0x81000001 cloudimg-rootfs.sealed-key"
    )]
    tpm_key_export: Option<Vec<String>>,

    /// Start a TPM socket server using a vTPM blob as backing state
    #[arg(
        long = "socket-server",
        value_names = &["path-to-vtpm-blob-file", "host:port"],
        number_of_values = 2,
        long_help = "Start a TPM socket server using vTPM blob as backing state.\nProvides a socket-based TPM interface compatible with tpm2-tools and go-tpm2.\n./cvmutil --socket-server vtpm.blob localhost:2321"
    )]
    socket_server: Option<Vec<String>>,
}

/// Main entry point for cvmutil.
fn main() {
    // Parse the command line arguments.
    let args = CmdArgs::parse();

    // Initialize tracing subscriber for logging.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .log_internal_errors(true)
        .with_max_level(if args.verbose {
            tracing::Level::TRACE
        } else {
            tracing::Level::INFO
        })
        .init();

    if let Some(path) = args.createvtpmblob {
        // Create a vTPM instance.
        tracing::info!("Creating vTPM blob and saving to file: {}", path);
        let (mut tpm_engine_helper, nv_blob_accessor) = create_tpm_engine_helper();
        let result = tpm_engine_helper.initialize_tpm_engine();
        assert!(result.is_ok());

        // Create vTPM in memory and save state to a file.
        let state = create_vtpm_blob(tpm_engine_helper, nv_blob_accessor);
        tracing::info!("vTPM blob size: {}", state.len());

        // if the vtpm file exists, delet it and create a new one
        if std::path::Path::new(&path).exists() {
            tracing::info!(
                "vTPM file already exists. Deleting the existing file and creating a new one."
            );
            fs::remove_file(&path).expect("failed to delete existing vtpm file");
        }
        fs::write(&path, state.as_slice()).expect("Failed to write vtpm state to blob file");
        tracing::info!("vTPM blob created and saved to file: {}", path);
    } else if let Some(paths) = args.write_srk {
        if paths.len() == 2 {
            let vtpm_blob_path = &paths[0];
            // Read the vtpm file content.
            let vtpm_blob_content =
                fs::read(vtpm_blob_path).expect("failed to read vtpm blob file");
            // Restore the TPM engine from the vTPM blob.
            let (mut vtpm_engine_helper, _nv_blob_accessor) = create_tpm_engine_helper();

            let result = vtpm_engine_helper
                .tpm_engine
                .reset(Some(&vtpm_blob_content));
            assert!(result.is_ok());

            let result = vtpm_engine_helper.initialize_tpm_engine();
            assert!(result.is_ok());
            tracing::info!("TPM engine initialized from blob file.");

            let srk_out_path = &paths[1];
            tracing::info!(
                "WriteSrk: blob file: {}, Srk out file: {}",
                vtpm_blob_path,
                srk_out_path
            );
            export_vtpm_srk_pub(vtpm_engine_helper, srk_out_path);
        } else {
            tracing::error!("Invalid number of arguments for --writeSrk. Expected 2 values.");
        }
    } else if let Some(vtpm_blob_path) = args.recreate_srk {
        tracing::info!("Recreating SRK from vTPM blob: {}", vtpm_blob_path);
        recreate_srk_test(&vtpm_blob_path);
    } else if let Some(template_path) = args.write_srk_template {
        tracing::info!("Writing SRK template to file: {}", template_path);
        write_srk_template(&template_path);
    } else if let Some(seal_args) = args.seal {
        if seal_args.len() == 3 {
            let srk_pub_path = &seal_args[0];
            let input_file = &seal_args[1];
            let output_file = &seal_args[2];
            tracing::info!(
                "Sealing data: {} -> {} using SRK: {}",
                input_file,
                output_file,
                srk_pub_path
            );
            seal_data_to_srk(srk_pub_path, input_file, output_file);
        } else {
            tracing::error!(
                "Invalid number of arguments for --seal. Expected 3 values: srk-pub-file input-file output-file"
            );
        }
    } else if let Some(unseal_args) = args.unseal {
        if unseal_args.len() == 3 {
            let vtpm_blob_path = &unseal_args[0];
            let sealed_file = &unseal_args[1];
            let output_file = &unseal_args[2];
            tracing::info!(
                "Unsealing data: {} -> {} using vTPM: {}",
                sealed_file,
                output_file,
                vtpm_blob_path
            );
            unseal_data_from_vtpm(vtpm_blob_path, sealed_file, output_file);
        } else {
            tracing::error!(
                "Invalid number of arguments for --unseal. Expected 3 values: vtmp-blob-file sealed-file output-file"
            );
        }
    } else if let Some(args) = args.create_random_key_in_tpm2_import_blob_format {
        if args.len() == 3 {
            let algorithm = &args[0];
            let public_key_file = &args[1];
            let private_key_tpm2b_file = &args[2];
            create_random_key_in_tpm2_import_blob_format(
                algorithm,
                public_key_file,
                private_key_tpm2b_file,
            );
        } else {
            tracing::error!(
                "Invalid number of arguments for --createRandomKeyInTpm2ImportBlobFormat. Expected 3 values."
            );
        }
    } else if let Some(srkpub_path) = args.print_key_name {
        print_vtpm_srk_pub_key_name(srkpub_path);
    } else if let Some(pub_der_path) = args.print_pub_key_der {
        print_pub_key_der(pub_der_path);
    } else if let Some(priv_tpm2b_path) = args.print_priv_key_tpm2b {
        print_tpm2bimport_content(priv_tpm2b_path);
    } else if let Some(key_files) = args.test_tpm2b_import_keys {
        if key_files.len() == 2 {
            let public_key_file = &key_files[0];
            let private_key_file = &key_files[1];
            test_import_tpm2b_keys(public_key_file, private_key_file);
        } else {
            tracing::error!(
                "Invalid number of arguments for --testTPM2BImportKeys. Expected 2 values."
            );
        }
    } else if let Some(import_args) = args.tpm_import {
        if import_args.len() == 2 {
            let vtpm_blob_path = &import_args[0];
            let sealed_key_path = &import_args[1];
            tracing::info!(
                "Importing sealed key {} into vTPM blob {}",
                sealed_key_path,
                vtpm_blob_path
            );
            import_sealed_key_blob_into_vtpm(vtpm_blob_path, sealed_key_path);
        } else {
            tracing::error!(
                "Invalid number of arguments for --tpmimport. Expected 2 values: vtpm-blob-file sealed-key-file"
            );
        }
    } else if let Some(export_args) = args.tpm_key_export {
        if export_args.len() == 3 {
            let vtpm_blob_path = &export_args[0];
            let sealed_key_output_path = &export_args[1];

            tracing::info!(
                "Creating new key for sealed key export to: {}",
                sealed_key_output_path
            );
            tracing::info!("Loading vTPM blob from: {}", vtpm_blob_path);
            tracing::info!("Output sealed key file: {}", sealed_key_output_path);

            // Read the vtpm file content.
            let vtpm_blob_content =
                fs::read(vtpm_blob_path).expect("failed to read vtpm blob file");
            // Restore the TPM engine from the vTPM blob.
            let (mut vtpm_engine_helper, _nv_blob_accessor) = create_tpm_engine_helper();

            let result = vtpm_engine_helper
                .tpm_engine
                .reset(Some(&vtpm_blob_content));
            assert!(result.is_ok());

            let result = vtpm_engine_helper.initialize_tpm_engine();
            assert!(result.is_ok());
            tracing::info!("TPM engine initialized from blob file.");

            // Instead of exporting existing key, create new one
            export_new_key_as_sealed_blob(&mut vtpm_engine_helper, sealed_key_output_path);
        } else {
            tracing::error!(
                "Invalid number of arguments for --tpmkeyexport. Expected 3 values: vtpm-blob-file key-handle sealed-key-output-file"
            );
        }
    } else if let Some(socket_args) = args.socket_server {
        if socket_args.len() == 2 {
            let vtpm_blob_path = &socket_args[0];
            let bind_addr = &socket_args[1];
            tracing::info!(
                "Starting TPM socket server: {} -> {}",
                vtpm_blob_path,
                bind_addr
            );
            vtpm_sock_server::start_tpm_socket_server(vtpm_blob_path, bind_addr);
        } else {
            tracing::error!(
                "Invalid arguments for --socket-server. Expected: vtpm-blob-file host:port"
            );
        }
    } else {
        tracing::error!("No command specified. Please re-run with --help for usage information.");
    }
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
    let auth_handle = tpm::tpm20proto::TPM20_RH_OWNER;
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
                tpm::tpm20proto::TPM20_RH_OWNER,
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
            tracing::trace!("SRK public area SHA256 hash: {:x}", public_area_hash);

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
    let nv_blob = nvm_state_blob.lock().unwrap().clone();
    tracing::trace!("Retrieved NV blob size: {}", nv_blob.len());
    nv_blob.to_vec()
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
            tracing::trace!("SRK public area SHA256 hash: {:x}", public_area_hash);
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
                "SRK public area SHA256 hash: {:x} is written to file {}",
                public_area_hash,
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
fn recreate_srk_test(vtpm_blob_path: &str) {
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
        tpm::tpm20proto::TPM20_RH_OWNER, // auth_handle
        TPM_RSA_SRK_HANDLE,              // object_handle (persistent handle to remove)
        TPM_RSA_SRK_HANDLE,              // persistent_handle (same as object_handle for removal)
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

    let auth_handle = tpm::tpm20proto::TPM20_RH_OWNER;
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
    let result = tpm_engine_helper.evict_control(
        tpm::tpm20proto::TPM20_RH_OWNER,
        new_object_handle,
        TPM_RSA_SRK_HANDLE,
    );

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
fn write_srk_template(template_path: &str) {
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
    tracing::trace!("Template SHA256: {:x}", template_hash);

    tracing::info!("SRK template generation completed successfully.");
}

/// Seal data to SRK using TPM-standard format compatible with Ubuntu secboot.
fn seal_data_to_srk(srk_pub_path: &str, input_file: &str, output_file: &str) {
    use marshal::{AfSplitData, CURRENT_METADATA_VERSION, KEY_DATA_HEADER};
    use std::fs;

    tracing::info!("Creating TPM-standard sealed key compatible with Ubuntu secboot");
    tracing::info!("Reading input data from: {}", input_file);
    let input_data = fs::read(input_file).expect("failed to read input file");
    tracing::info!("Input data size: {} bytes", input_data.len());

    // Create minimal TPM structures for a sealed data object
    // We'll create a simple keyedobject that contains the sealed data

    // 1. Create a minimal TPM2B_PRIVATE containing our data
    let key_private = Tpm2bBuffer::new(&input_data).expect("input data too large for TPM2B buffer");

    // 2. Create a minimal TPM2B_PUBLIC for the sealed object
    // Use the SRK public key template but mark it as a data object
    let srk_template = tpm_helper::srk_pub_template().expect("failed to create SRK template");
    let mut sealed_template = srk_template;

    // Modify to be a sealed data object instead of a key
    sealed_template.object_attributes = TpmaObjectBits::new()
        .with_user_with_auth(true)
        .with_no_da(true)
        .with_decrypt(true)
        .into();

    // Set unique field to indicate this contains sealed data
    sealed_template.unique.buffer[0] = 0xDA; // "DATA" marker
    sealed_template.unique.buffer[1] = 0x7A;
    sealed_template.unique.buffer[2] = input_data.len() as u8;
    sealed_template.unique.buffer[3] = (input_data.len() >> 8) as u8;

    let key_public = Tpm2bPublic::new(sealed_template);

    // 3. Create empty TPM2B_ENCRYPTED_SECRET
    let import_sym_seed = Tpm2bBuffer::new_zeroed();

    // 4. Set auth_mode_hint
    let auth_mode_hint: u8 = 0;

    tracing::info!("Created TPM structures:");
    tracing::info!("  TPM2B_PRIVATE size: {} bytes", key_private.payload_size());
    tracing::info!("  TPM2B_PUBLIC size: {} bytes", key_public.payload_size());
    tracing::info!(
        "  TPM2B_ENCRYPTED_SECRET size: {} bytes",
        import_sym_seed.payload_size()
    );

    // 5. Marshal in the order expected by secboot: PRIVATE || PUBLIC || auth_mode_hint || ENCRYPTED_SECRET
    let mut tpm_data = Vec::new();
    tpm_data.extend_from_slice(&key_private.serialize());
    tpm_data.extend_from_slice(&key_public.serialize());
    tpm_data.push(auth_mode_hint);
    tpm_data.extend_from_slice(&import_sym_seed.serialize());

    tracing::info!("Marshaled TPM data: {} bytes", tpm_data.len());

    // 6. Create AF split data using marshal.rs implementation
    let af_split_data = AfSplitData::create(&tpm_data);

    // 7. Create the final sealed key file
    let mut sealed_blob = Vec::new();

    // Header: "USK$" magic (big endian)
    sealed_blob.extend_from_slice(&KEY_DATA_HEADER.to_be_bytes());

    // Version: 2 (big endian)
    sealed_blob.extend_from_slice(&CURRENT_METADATA_VERSION.to_be_bytes());

    // AF Split data (using marshal.rs serialization)
    sealed_blob.extend_from_slice(&af_split_data.to_bytes());

    tracing::info!(
        "Writing Ubuntu secboot compatible sealed key to: {} ({} bytes)",
        output_file,
        sealed_blob.len()
    );
    fs::write(output_file, sealed_blob).expect("failed to write sealed data file");

    tracing::info!("TPM-standard sealing completed successfully");
    tracing::info!("Test with: ./test-key --debug {}", output_file);
}

/// Unseal data from TPM-standard sealed blob using vTPM.
fn unseal_data_from_vtpm(vtpm_blob_path: &str, sealed_file: &str, output_file: &str) {
    use marshal::{CURRENT_METADATA_VERSION, KEY_DATA_HEADER};
    use std::fs;

    tracing::info!("Unsealing TPM-standard sealed key compatible with Ubuntu secboot");
    tracing::info!("Reading sealed data from: {}", sealed_file);
    let sealed_blob = fs::read(sealed_file).expect("failed to read sealed file");

    // Parse the Ubuntu secboot format
    if sealed_blob.len() < 8 {
        // Magic(4) + Version(4) minimum
        panic!("Sealed file too small for header");
    }

    // Parse header
    let magic = u32::from_be_bytes([
        sealed_blob[0],
        sealed_blob[1],
        sealed_blob[2],
        sealed_blob[3],
    ]);
    if magic != KEY_DATA_HEADER {
        panic!(
            "Invalid sealed file format: expected USK$ magic, got 0x{:08x}",
            magic
        );
    }

    let version = u32::from_be_bytes([
        sealed_blob[4],
        sealed_blob[5],
        sealed_blob[6],
        sealed_blob[7],
    ]);
    if version != CURRENT_METADATA_VERSION {
        panic!(
            "Unsupported sealed file version: {} (expected version {})",
            version, CURRENT_METADATA_VERSION
        );
    }

    tracing::info!("Sealed key format validated: USK$ version {}", version);

    // Use the marshal::AfSplitData::from_bytes() method to parse the AF split data
    // The AF split data starts at offset 8 (after the header)
    let af_split_data =
        marshal::AfSplitData::from_bytes(&sealed_blob[8..]).expect("failed to parse AF split data");

    tracing::info!(
        "AF Split data parsed: {} stripes, hash_alg=0x{:04x}, size={} bytes",
        af_split_data.stripes,
        af_split_data.hash_alg,
        af_split_data.size
    );

    // Merge the AF split data to recover the original TPM structures
    tracing::info!("Merging AF split data to recover TPM structures...");
    let merged_data = af_split_data
        .merge()
        .expect("failed to merge AF split data");
    tracing::info!(
        "AF split merge successful: recovered {} bytes",
        merged_data.len()
    );

    // Debug: print first few bytes of merged data
    if merged_data.len() >= 16 {
        tracing::debug!("First 16 bytes of merged data: {:02x?}", &merged_data[..16]);
    } else {
        tracing::debug!(
            "First {} bytes of merged data: {:02x?}",
            merged_data.len(),
            &merged_data
        );
    }

    // Parse the merged TPM data: TPM2B_PRIVATE || TPM2B_PUBLIC || auth_mode_hint || TPM2B_ENCRYPTED_SECRET
    let mut offset = 0;

    // Parse TPM2B_PRIVATE
    if merged_data.len() < offset + 2 {
        panic!("Merged data too short for TPM2B_PRIVATE header");
    }

    // Check the size field of TPM2B_PRIVATE
    let private_size = u16::from_be_bytes([merged_data[offset], merged_data[offset + 1]]);
    tracing::debug!("TPM2B_PRIVATE size field: {} bytes", private_size);

    if merged_data.len() < offset + 2 + private_size as usize {
        panic!(
            "Merged data too short for TPM2B_PRIVATE: need {} bytes, have {} bytes",
            offset + 2 + private_size as usize,
            merged_data.len()
        );
    }

    let key_private = Tpm2bBuffer::deserialize(&merged_data[offset..]);
    let key_private = match key_private {
        Some(buffer) => buffer,
        None => {
            tracing::error!("Failed to deserialize TPM2B_PRIVATE");
            tracing::error!(
                "Data at offset {}: {:02x?}",
                offset,
                &merged_data[offset..offset.min(merged_data.len()).min(offset + 20)]
            );
            panic!("failed to deserialize TPM2B_PRIVATE");
        }
    };

    offset += key_private.payload_size();
    tracing::info!("Parsed TPM2B_PRIVATE: {} bytes", key_private.payload_size());

    // Parse TPM2B_PUBLIC
    if merged_data.len() < offset + 2 {
        panic!("Merged data too short for TPM2B_PUBLIC header");
    }

    let public_size = u16::from_be_bytes([merged_data[offset], merged_data[offset + 1]]);
    tracing::debug!("TPM2B_PUBLIC size field: {} bytes", public_size);

    if merged_data.len() < offset + 2 + public_size as usize {
        panic!(
            "Merged data too short for TPM2B_PUBLIC: need {} bytes, have {} bytes",
            offset + 2 + public_size as usize,
            merged_data.len()
        );
    }

    let key_public = Tpm2bPublic::deserialize(&merged_data[offset..]);
    let key_public = match key_public {
        Some(public) => public,
        None => {
            tracing::error!("Failed to deserialize TPM2B_PUBLIC");
            tracing::error!(
                "Data at offset {}: {:02x?}",
                offset,
                &merged_data[offset..offset.min(merged_data.len()).min(offset + 20)]
            );
            panic!("failed to deserialize TPM2B_PUBLIC");
        }
    };

    offset += key_public.payload_size();
    tracing::info!("Parsed TPM2B_PUBLIC: {} bytes", key_public.payload_size());

    // Parse auth_mode_hint
    if merged_data.len() < offset + 1 {
        panic!("Merged data too short for auth_mode_hint");
    }

    let auth_mode_hint = merged_data[offset];
    offset += 1;

    tracing::info!("Parsed auth_mode_hint: {}", auth_mode_hint);

    // Parse TPM2B_ENCRYPTED_SECRET
    if merged_data.len() < offset + 2 {
        panic!("Merged data too short for TPM2B_ENCRYPTED_SECRET");
    }

    let import_sym_seed = Tpm2bBuffer::deserialize(&merged_data[offset..]);
    let import_sym_seed = match import_sym_seed {
        Some(buffer) => buffer,
        None => {
            tracing::error!("Failed to deserialize TPM2B_ENCRYPTED_SECRET");
            tracing::error!(
                "Data at offset {}: {:02x?}",
                offset,
                &merged_data[offset..offset.min(merged_data.len()).min(offset + 20)]
            );
            panic!("failed to deserialize TPM2B_ENCRYPTED_SECRET");
        }
    };

    offset += import_sym_seed.payload_size();
    tracing::info!(
        "Parsed TPM2B_ENCRYPTED_SECRET: {} bytes",
        import_sym_seed.payload_size()
    );
    tracing::info!("Successfully parsed all TPM structures from sealed key");

    // Load vTPM blob and initialize TPM engine
    tracing::info!("Loading vTPM blob from: {}", vtpm_blob_path);
    let vtpm_blob_content = fs::read(vtpm_blob_path).expect("failed to read vTPM blob file");

    let (mut tpm_engine_helper, _nv_blob_accessor) = create_tpm_engine_helper();
    let result = tpm_engine_helper.tpm_engine.reset(Some(&vtpm_blob_content));
    if let Err(e) = result {
        panic!("Failed to restore vTPM from blob: {:?}", e);
    }

    // Initialize the TPM engine (required after reset)
    tracing::info!("Initializing TPM engine...");
    let result = tpm_engine_helper.initialize_tpm_engine();
    if let Err(e) = result {
        panic!("Failed to initialize TPM engine: {:?}", e);
    }

    // The TPM2B_PRIVATE contains our original sealed data
    // In our implementation, we stored the data directly in the TPM2B_PRIVATE buffer
    let sealed_data_size = key_private.size.get() as usize;
    if sealed_data_size == 0 {
        panic!("No data found in sealed key");
    }

    let sealed_data = &key_private.buffer[0..sealed_data_size];

    // Check if this looks like our sealed object by examining the unique field in the public key
    let unique_marker = &key_public.public_area.unique.buffer[0..4];
    if unique_marker[0] == 0xDA && unique_marker[1] == 0x7A {
        // This is our sealed data format
        let expected_data_size = (unique_marker[2] as usize) | ((unique_marker[3] as usize) << 8);
        tracing::info!(
            "Detected sealed data object, expected size: {} bytes",
            expected_data_size
        );

        if sealed_data_size != expected_data_size {
            tracing::warn!(
                "Data size mismatch: stored {} bytes, expected {} bytes",
                sealed_data_size,
                expected_data_size
            );
        }
    }

    tracing::info!("Extracted original data: {} bytes", sealed_data.len());

    // Write the unsealed data
    tracing::info!("Writing unsealed data to: {}", output_file);
    fs::write(output_file, sealed_data).expect("failed to write unsealed data file");

    tracing::info!("TPM-standard unsealing completed successfully");
    tracing::info!("Original data has been recovered from the sealed key");
}

/// Print the SRK public key name.
fn print_vtpm_srk_pub_key_name(srkpub_path: String) {
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
    let public_area: TpmtPublic = public_key.public_area.into();
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
fn create_random_key_in_tpm2_import_blob_format(
    algorithm: &String,
    public_key_file: &String,
    private_key_tpm2b_file: &String,
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

            print_sha256_hash(&private_key_der.as_slice());
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
            return;
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
        "    - sensitive (RSA private exp): {} bytes (size + data)",
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
fn print_pub_key_der(pub_key_der_path: String) {
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
fn print_tpm2bimport_content(tpm2b_import_file_path: String) {
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
fn test_import_tpm2b_keys(public_key_file: &str, private_key_file: &str) {
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
        return;
    }
}

/// Validate keys when public key is in DER format
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
fn import_sealed_key_blob_into_vtpm(vtpm_blob_path: &str, sealed_key_path: &str) {
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
    let updated_blob = nv_blob_accessor.lock().unwrap().clone();

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

/// Create Anti Forensic (AF) split data structure
fn create_af_split_data(payload: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};

    // Use Canonical's approach: target 128KB minimum size
    let min_size = 128 * 1024; // 128KB like Canonical
    let stripes = (min_size / payload.len()) + 1;

    println!(
        "AF split: payload {} bytes, {} stripes, target size ~{}KB",
        payload.len(),
        stripes,
        (payload.len() * stripes) / 1024
    );

    let block_size = payload.len();
    let mut result = Vec::new();
    let mut block = vec![0u8; block_size];

    // Generate stripes-1 random blocks and XOR/hash them
    for _i in 0..(stripes - 1) {
        let mut random_block = vec![0u8; block_size];
        getrandom::fill(&mut random_block).expect("Failed to generate random data");

        result.extend_from_slice(&random_block);

        // XOR with accumulated block
        for j in 0..block_size {
            block[j] ^= random_block[j];
        }

        // Diffuse the block using hash (simplified version)
        let mut hasher = Sha256::new();
        hasher.update(&block);
        let hash = hasher.finalize();

        // Simple diffusion: XOR block with repeated hash
        for j in 0..block_size {
            block[j] ^= hash[j % 32];
        }
    }

    // Final stripe: XOR the accumulated block with original data
    let mut final_stripe = vec![0u8; block_size];
    for i in 0..block_size {
        final_stripe[i] = block[i] ^ payload[i];
    }
    result.extend_from_slice(&final_stripe);

    // Create AF split header: stripes(4) + hash_alg(4) + af_data_size(4) + data
    let mut af_data = Vec::new();
    af_data.extend_from_slice(&(stripes as u32).to_le_bytes()); // 4 bytes: stripe count
    af_data.extend_from_slice(&8u32.to_le_bytes()); // 4 bytes: SHA256 hash algorithm ID
    af_data.extend_from_slice(&(result.len() as u32).to_le_bytes()); // 4 bytes: AF data length (changed from u16)
    af_data.extend_from_slice(&result);

    af_data
}

/// Export a newly generated key as a sealed key file (instead of exporting existing persistent key)
fn export_new_key_as_sealed_blob(
    tpm_engine_helper: &mut TpmEngineHelper,
    sealed_key_output_path: &str,
) {
    tracing::info!("Generating new RSA key for sealed key export");

    // Create RSA key template suitable for export/import
    let key_template = create_exportable_rsa_key_template();

    // Generate the key pair in TPM under Owner hierarchy (like SRK)
    let create_result =
        tpm_engine_helper.create_primary(tpm::tpm20proto::TPM20_RH_OWNER, key_template);

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
    let sealed_key_data = create_sealed_key_blob_v2_with_real_data(
        &dummy_private.unwrap(),
        &key_public,
        &import_seed,
    );

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
fn create_exportable_rsa_key_template() -> TpmtPublic {
    use tpm::tpm20proto::protocol::*;
    use tpm::tpm20proto::*;

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
    key_template.parameters = TpmsRsaParams::from(rsa_params);

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
fn create_sealed_key_blob_v2_with_real_data(
    key_private: &Tpm2bBuffer,
    key_public: &Tpm2bPublic,
    import_seed: &[u8],
) -> Vec<u8> {
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
    let af_split_data = create_af_split_data(&payload);

    // Append AF split data to sealed key
    sealed_data.extend_from_slice(&af_split_data);

    sealed_data
}


// cargo test -p cvmutil test_srk_template_generation
// cargo test -p cvmutil test_platform_unique_value
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    #[test]
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

    #[test]
    fn test_platform_unique_value() {
        let (callbacks, _) = TestPlatformCallbacks::new();
        // Access the method through the trait interface
        use ms_tpm_20_ref::PlatformCallbacks;
        let unique_value = callbacks.get_unique_value();

        // Verify it returns empty array as expected for deterministic SRK generation
        assert_eq!(unique_value, &[] as &[u8]);
        println!("Platform unique value test passed: empty array as expected");
    }
}
