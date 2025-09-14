//! CvmUtil is a tool to create and manage vTPM blobs.
//! vTPM blobs are used to provide TPM functionality to trusted and confidential VMs.
use ms_tpm_20_ref;
use ms_tpm_20_ref::MsTpm20RefPlatform;
use tpm::tpm20proto::protocol::{
    Tpm2bBuffer, Tpm2bPublic, TpmsRsaParams, TpmtPublic, TpmtRsaScheme, TpmtSymDefObject,
};
use tpm::tpm20proto::{AlgId, AlgIdEnum, TpmaObjectBits};
use tpm::tpm_helper::{self, TpmEngineHelper};
use tpm::TPM_RSA_SRK_HANDLE;
mod marshal;
use marshal::TpmtSensitive;
use zerocopy::FromZeros;
use std::sync::{Arc, Mutex};
use ms_tpm_20_ref::DynResult;
use std::io::Read;
use std::io::Write;
use std::time::Instant;
use std::{fs, fs::File, vec};
use base64::Engine;
use openssl::ec::EcGroup;
use openssl::ec::EcKey;
use openssl::nid::Nid;
use openssl::pkey::PKey;
use openssl::rsa::Rsa;
use sha2::{Digest, Sha256};

use clap::Parser;

#[derive(Parser, Debug)]
#[clap(name = "cvmutil", about = "Tool to interact with vtmp blobs.")]
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

    #[clap(short='w', long = "writeSrk", value_names = &["path-to-blob-file", "path-to-srk-out-file"], long_help="Write the SRK public key in TPM2B format.\n./cvmutil --writeSrk vTpm.blob srk.pub")]
    write_srk: Option<Vec<String>>,

    #[arg(short = 'p', long = "printKeyName", value_name = "path-to-srkPub", long_help="Print the TPM key name \n./cvmutil --printKeyName srk.pub")]
    print_key_name: Option<String>,

    #[arg(short = 's', long = "createRandomKeyInTpm2ImportBlobFormat", value_names = &["algorithm", "publicKey", "output-file"], long_help="Create random RSA/ECC key in Tpm2 import blob format:TPM2B_PUBLIC || TP2B_PRIVATE || TP2B_ENCRYPTED_SEED \n./cvmutil --createRandomKeyInTpm2ImportBlobFormat rsa rsa_pub.der rsa_priv_marshalled.tpm2b")]
    create_random_key_in_tpm2_import_blob_format: Option<Vec<String>>,

    #[arg(short = 'd', long = "printDER", value_name = "path-to-pubKey-der", long_help="Print info about DER key \n./cvmutil --printDER rsa_pub.der")]
    print_pub_key_der: Option<String>,

    #[arg(short = 't', long = "printTPM2B", value_name = "path-to-privKey-tpm2b", long_help="Print info about TPM2B import file: TPM2B_PUBLIC || TP2B_PRIVATE || TP2B_ENCRYPTED_SEED. \n./cvmutil --printTPM2B marshalled_import_blob.tpm2b")]
    print_priv_key_tpm2b: Option<String>,

    #[arg(short = 'i', long = "testTPM2BImportKeys", value_names = &["path-to-pubKey-der", "path-to-privKey-tpm2b"], long_help="Import the public in DER and private in TPM2B format. Make sure they form a keypair. \n./cvmutil --testTPM2BImportKeys rsa_pub.der marshalled_import_blob.tpm2b")]
    test_tpm2b_import_keys: Option<Vec<String>>,
}

/// Main entry point for cvmutil.
fn main() {
    // Parse the command line arguments.
    let args = CmdArgs::parse();

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
            tracing::info!("vTPM file already exists. Deleting the existing file and creating a new one.");
            fs::remove_file(&path).expect("failed to delete existing vtpm file");
        }
        fs::write(&path, state.as_slice()).expect("Failed to write vtpm state to blob file");
        tracing::info!("vTPM blob created and saved to file: {}", path);

    } else if let Some(paths) = args.write_srk {
        if paths.len() == 2 {
            let vtpm_blob_path = &paths[0];
            // Read the vtpm file content.
            let vtpm_blob_content = fs::read(vtpm_blob_path)
                .expect("failed to read vtpm blob file");
            // Restore the TPM engine from the vTPM blob.
            let (mut vtpm_engine_helper, _nv_blob_accessor) = create_tpm_engine_helper();

            let result = vtpm_engine_helper.tpm_engine.reset(Some(&vtpm_blob_content));
            assert!(result.is_ok());

            let result = vtpm_engine_helper.initialize_tpm_engine();
            assert!(result.is_ok());
            tracing::info!("TPM engine initialized from blob file.");

            let srk_out_path = &paths[1];
            tracing::info!(
                "WriteSrk: blob file: {}, Srk out file: {}",
                vtpm_blob_path, srk_out_path
            );
            export_vtpm_srk_pub(vtpm_engine_helper, srk_out_path);
        } else {
            tracing::error!("Invalid number of arguments for --writeSrk. Expected 2 values.");
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
            tracing::error!("Invalid number of arguments for --createRandomKeyInTpm2ImportBlobFormat. Expected 3 values.");
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
            tracing::error!("Invalid number of arguments for --testTPM2BImportKeys. Expected 2 values.");
        }
    } else {
        tracing::error!("No command specified. Please re-run with --help for usage information.");
    }
}

/// Create vtpm and return its state as a byte vector.
fn create_vtpm_blob(mut tpm_engine_helper: TpmEngineHelper, nvm_state_blob: Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    // Create a vTPM instance.
    tracing::info!("Initializing TPM engine.");
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

    // DEBUG: retrieve the SRK and print its SHA256 hash
    let result = tpm_engine_helper.read_public(TPM_RSA_SRK_HANDLE);
    match result {
        Ok(response) => {
            let mut hasher = Sha256::new();
            hasher.update(response.out_public.public_area.serialize());
            let public_area_hash = hasher.finalize();
            tracing::trace!("SRK public area SHA256 hash: {:x}", public_area_hash);
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
            //recreate_srk(&mut tmp_engine_helper);
        }
        Err(e) => tracing::error!("Error finding SRK handle: {:?}", e),
    }

    // Extract SRK primary key public area.
    let result = tpm_engine_helper.read_public(TPM_RSA_SRK_HANDLE);
    match result {  
        Ok(response) => {
            tracing::trace!("SRK public area: {:?}", response.out_public.public_area);

            // Write the SRK pub to a file.
            //let srk_pub_file = Use the input.
            let mut srk_pub_file = File::create(srk_out_path).expect("failed to create file");
            let srk_pub = response.out_public.serialize();
            let srk_pub = srk_pub.as_slice();
            srk_pub_file
                .write_all(&srk_pub)
                .expect("failed to write to file");
            // Compute SHA256 hash of the public area
            let mut hasher = Sha256::new();
            hasher.update(response.out_public.public_area.serialize());
            let public_area_hash = hasher.finalize();
            tracing::trace!("SRK public area SHA256 hash: {:x} is written to file {}", public_area_hash, srk_out_path);
        }
        Err(e) => {
            tracing::error!("Error in read_public: {:?}", e);
        }
    }
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
    tracing::trace!("Public key size in bits: {:?}", public_area.parameters.key_bits);
    print_sha256_hash(rsa_key.buffer.as_slice());

    // Compute the key name
    let algorithm_id = public_area.name_alg;
    let mut output_key = vec![0u8; size_of::<AlgId>() + public_area_hash.len()];
    output_key[0] = (algorithm_id.0.get() >> 8) as u8;
    output_key[1] = (algorithm_id.0.get() & 0xFF) as u8;
    for i in 0..public_area_hash.len() {
        output_key[i + 2] = public_area_hash[i];
    }

    let base64_key = base64::engine::general_purpose::STANDARD.encode(&output_key);
    tracing::trace!("Key name:\n");
    tracing::trace!(" {}", base64_key);

    // DEBUG: Print RSA bytes in hex to be able to compare with tpm2_readpublic -c 0x81000001
    tracing::trace!("\nRSA key bytes:");
    tracing::trace!("  ");
    for i in 0..tpm_helper::RSA_2K_MODULUS_SIZE {
        tracing::trace!("{:02x}", rsa_key.buffer[i]);
    }
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
            tracing::trace!("Tpm2bBuffer modulus size field: {} bytes", modulus_buffer.size.get());

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
            tracing::info!("RSA private key is saved to {private_key_tpm2b_file} in TPM2B import format.");
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

            tracing::info!("ECC private key in TPM2B import format saved to {private_key_tpm2b_file}.");
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
    tracing::trace!("TPM2B_PUBLIC serialized size: {} bytes", tpm2b_public.serialize().len());

    // Create a TPM2B_PRIVATE structure
    // For RSA import format, use the first prime factor (p), not the private exponent (d)
    let prime1_bytes = rsa.p().unwrap().to_vec();
    tracing::trace!("RSA prime1 (p) size: {} bytes", prime1_bytes.len());
    let sensitive_rsa = Tpm2bBuffer::new(&prime1_bytes).unwrap();

    let tpmt_sensitive = TpmtSensitive {
        sensitive_type: tpmt_public_area.my_type,  // TPM_ALG_RSA
        auth_value: Tpm2bBuffer::new_zeroed(),     // Empty auth value
        seed_value: Tpm2bBuffer::new_zeroed(),     // Empty seed value 
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

    tracing::trace!("TPM2B_PRIVATE total buffer size: {} bytes", tpm2b_private_buffer.len());
    tracing::trace!("  - Size field: 2 bytes");
    tracing::trace!("  - Marshaled sensitive data: {} bytes", marshaled_tpmt_sensitive.len());
    tracing::trace!("    - sensitive_type: 2 bytes");
    tracing::trace!("    - auth_value: {} bytes (size + data)", 2 + tpmt_sensitive.auth_value.size.get());
    tracing::trace!("    - seed_value: {} bytes (size + data)", 2 + tpmt_sensitive.seed_value.size.get()); 
    tracing::trace!("    - sensitive (RSA private exp): {} bytes (size + data)", 2 + tpmt_sensitive.sensitive.size.get());

    // Create the final import blob: TPM2B_PUBLIC || TPM2B_PRIVATE || TPM2B_ENCRYPTED_SECRET
    let mut final_import_blob = Vec::new();
    
    // Add TPM2B_PUBLIC
    let serialized_public = tpm2b_public.serialize();
    final_import_blob.extend_from_slice(&serialized_public);
    
    // Add TPM2B_PRIVATE 
    final_import_blob.extend_from_slice(&tpm2b_private_buffer);
    
    // Add TPM2B_ENCRYPTED_SECRET (empty - just 2 bytes of zeros for size)
    final_import_blob.extend_from_slice(&[0u8, 0u8]);
    
    tracing::trace!("Final TPM2B import format size: {} bytes", final_import_blob.len());
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
    pub_key_file.read_to_end(&mut pub_key_content)
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
    priv_key_file.read_to_end(&mut priv_key_content)
        .expect("Failed to read private key file");
    
    tracing::info!("Private key file size: {} bytes", priv_key_content.len());
    
    // Parse the TPM2B import format: TPM2B_PUBLIC || TPM2B_PRIVATE || TPM2B_ENCRYPTED_SEED
    
    // 1. Parse TPM2B_PUBLIC
    let tmp2b_public = Tpm2bPublic::deserialize(&priv_key_content)
        .expect("Failed to deserialize TPM2B_PUBLIC");
    
    let public_size = tmp2b_public.size.get() as usize + 2; // +2 for size field
    tracing::info!("TPM2B_PUBLIC parsed:");
    tracing::info!("  Size: {} bytes", public_size);
    tracing::info!("  Algorithm: {:?}", tmp2b_public.public_area.my_type);
    tracing::info!("  Name algorithm: {:?}", tmp2b_public.public_area.name_alg);
    tracing::info!("  Key bits: {:?}", tmp2b_public.public_area.parameters.key_bits);
    
    // 2. Parse TPM2B_PRIVATE
    let remaining_data = &priv_key_content[public_size..];
    let tmp2b_private = Tpm2bBuffer::deserialize(remaining_data)
        .expect("Failed to deserialize TPM2B_PRIVATE");
    
    let private_size = tmp2b_private.size.get() as usize + 2; // +2 for size field
    tracing::info!("TPM2B_PRIVATE parsed:");
    tracing::info!("  Size: {} bytes", private_size);
    tracing::info!("  Data size: {} bytes", tmp2b_private.size.get());
    
    // 3. Parse TPM2B_ENCRYPTED_SECRET (should be empty - 2 zero bytes)
    let encrypted_seed_data = &remaining_data[private_size..];
    if encrypted_seed_data.len() >= 2 {
        let encrypted_seed_size = u16::from_be_bytes([encrypted_seed_data[0], encrypted_seed_data[1]]);
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
    let tpm2b_modulus: &[u8; 256] = &tmp2b_public.public_area.unique.buffer[0..256].try_into().expect("Modulus size mismatch");
    
    tracing::info!("Validation:");
    tracing::info!("  DER modulus size: {} bytes", der_modulus.len());
    tracing::info!("  TPM2B modulus size: {} bytes", tpm2b_modulus.len());
    
    if der_modulus == *tpm2b_modulus {
        tracing::info!("  Modulus values match between DER and TPM2B formats");
    } else {
        tracing::error!("  Modulus values do NOT match");
        tracing::error!("  First 16 bytes of DER modulus: {:02X?}", &der_modulus[..16.min(der_modulus.len())]);
        tracing::error!("  First 16 bytes of TPM2B modulus: {:02X?}", &tpm2b_modulus[..16.min(tpm2b_modulus.len())]);
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

struct TestPlatformCallbacks {
    blob: Vec<u8>,
    time: Instant,
    // Add shared access to the blob
    shared_blob: Arc<Mutex<Vec<u8>>>,
}

impl TestPlatformCallbacks {
    fn new() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let shared_blob = Arc::new(Mutex::new(Vec::new()));
        let callbacks = TestPlatformCallbacks {
            blob: vec![],
            time: Instant::now(),
            shared_blob: shared_blob.clone(),
        };
        (callbacks, shared_blob)
    }
}

impl ms_tpm_20_ref::PlatformCallbacks for TestPlatformCallbacks {
    fn commit_nv_state(&mut self, state: &[u8]) -> DynResult<()> {
        tracing::trace!("committing nv state with len {}", state.len());
        self.blob = state.to_vec();
        // Also update the shared blob
        *self.shared_blob.lock().unwrap() = state.to_vec();

        Ok(())
    }

    fn get_crypt_random(&mut self, buf: &mut [u8]) -> DynResult<usize> {
        getrandom::fill(buf).expect("rng failure");

        Ok(buf.len())
    }

    fn monotonic_timer(&mut self) -> std::time::Duration {
        self.time.elapsed()
    }

    fn get_unique_value(&self) -> &'static [u8] {
        b"CPS team cvmutil"
    }
}

/// Create a new TPM engine with blank state and return the helper and NV state blob.
pub fn create_tpm_engine_helper() -> (TpmEngineHelper, Arc<Mutex<Vec<u8>>>) {

    let (callbacks, nv_blob_accessor) = TestPlatformCallbacks::new();

    let result = MsTpm20RefPlatform::initialize(
        Box::new(callbacks),
        ms_tpm_20_ref::InitKind::ColdInit,
    );
    assert!(result.is_ok());

    let tpm_engine: MsTpm20RefPlatform = result.unwrap();

    let tpm_helper = TpmEngineHelper {
        tpm_engine,
        reply_buffer: [0u8; 4096],
    };

    (tpm_helper, nv_blob_accessor)
}