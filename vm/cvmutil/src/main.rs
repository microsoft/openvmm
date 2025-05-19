use ms_tpm_20_ref;
use ms_tpm_20_ref::MsTpm20RefPlatform;
//use tpm::{self, tpm_helper};
use tpm::tpm20proto::protocol::{
    Tpm2bBuffer, Tpm2bPublic, TpmsRsaParams, TpmtPublic, TpmtRsaScheme, TpmtSymDefObject,
};
use tpm::tpm20proto::{AlgId, AlgIdEnum, TpmaObjectBits};
use tpm::tpm_helper::{self, TpmEngineHelper};
use tpm::TPM_RSA_SRK_HANDLE;
mod marshal;
use marshal::TpmtSensitive;
use zerocopy::FromZeroes;
//use ms_tpm_20_ref::PlatformCallbacks;
//use crate::Tpm;
// use inspect::InspectMut;
use ms_tpm_20_ref::DynResult;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::time::Instant;
use std::{fs, fs::File, vec};
use tracing::Level;

use base64::Engine;
//use openssl::bn::BigNum;
use openssl::ec::EcGroup;
use openssl::ec::EcKey;
//use openssl::ec::PointConversionForm;
use openssl::nid::Nid;
use openssl::pkey::PKey;
use openssl::rsa::Rsa;
use sha2::{Digest, Sha256};

use clap::Parser;

#[derive(Parser, Debug)]
#[clap(name = "cvmutil", about = "Tool to interact with vtmp blobs.")]
struct CmdArgs {
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

    #[arg(short = 's', long = "createRandomKeyInTpm2ImportBlobFormat", value_names = &["algorithm", "publicKey", "output-file"], long_help="Create random RSA/ECC key in Tpm2 import blob format:TPM2B_PUBLIC || TP2B_PRIVATE || TP2B_ENCRYPTED_SEED \n./cvmutil --createRandomKeyInTpm2ImportBlobFormat rsa rsa_pub.der marshalled_import_blob.tpm2b")]
    create_random_key_in_tpm2_import_blob_format: Option<Vec<String>>,

    #[arg(short = 'd', long = "printDER", value_name = "path-to-pubKey-der", long_help="Print info about DER key \n./cvmutil --printDER rsa_pub.der")]
    print_pub_key_der: Option<String>,

    #[arg(short = 't', long = "printTPM2B", value_name = "path-to-privKey-tpm2b", long_help="Print info about TPM2B import file: TPM2B_PUBLIC || TP2B_PRIVATE || TP2B_ENCRYPTED_SEED. \n./cvmutil --printTPM2B marshalled_import_blob.tpm2b")]
    print_priv_key_tpm2b: Option<String>,
}

fn main() {
    // Initialize the logger.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .log_internal_errors(true)
        .with_max_level(Level::ERROR)
        .init();

    // Parse the command line arguments.
    let args = CmdArgs::parse();

    if let Some(path) = args.createvtpmblob {
        println!("CreateVtpmBlob: {}", path);
        let file = File::create(path.clone()).expect("failed to create file");
        println!("Creating vTPM blob in file: {:?}", path);
        let tpm_engine_helper = create_tpm_engine_helper(file);
        create_vtpm_blob(tpm_engine_helper);
    } else if let Some(paths) = args.write_srk {
        if paths.len() == 2 {
            let vtpm_blob_path = &paths[0];
            // Restore the TPM engine from the vTPM blob.
            let vtpm_engine_helper = restore_tpm_engine_helper(vtpm_blob_path.to_string());
            let srk_out_path = &paths[1];
            println!(
                "WriteSrk: blob file: {}, Srk out file: {}",
                vtpm_blob_path, srk_out_path
            );
            export_vtpm_srk_pub(vtpm_engine_helper, srk_out_path);
        } else {
            println!("Invalid number of arguments for --writeSrk. Expected 2 values.");
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
            println!("Invalid number of arguments for --createRandomKeyInTpm2ImportBlobFormat. Expected 3 values.");
        }
    } else if let Some(srkpub_path) = args.print_key_name {
        print_vtpm_srk_pub_key_name(srkpub_path);
    } else if let Some(pub_der_path) = args.print_pub_key_der {
        print_pub_key_der(pub_der_path);
    } else if let Some(priv_tpm2b_path) = args.print_priv_key_tpm2b {
        print_tpm2bimport_content(priv_tpm2b_path);
    } else {
        println!("No command specified. Please re-run with --help for usage information.");
        println!();
    }
}

fn create_vtpm_blob(mut tpm_engine_helper: TpmEngineHelper) {
    // Create a vTPM instance.
    let result = tpm_engine_helper.initialize_tpm_engine();
    assert!(result.is_ok());

    //let mut tpm_engine_helper = create_tpm_engine_helper();
    //restart_tpm_engine(&mut tpm_engine_helper, false, true);

    // Create a primary key: SRK
    let auth_handle = tpm::tpm20proto::TPM20_RH_OWNER;
    // DON'T have a template for SRK. Use EK template instead for now.
    let result = tpm_helper::srk_pub_template();
    assert!(result.is_ok());
    let in_public = result.unwrap();
    let result = tpm_engine_helper.create_primary(auth_handle, in_public);
    match result {
        Ok(response) => {
            println!("SRK handle: {:?}", response.object_handle);
            assert_ne!(response.out_public.size.get(), 0);
            println!("SRK public area: {:?}", response.out_public.public_area);

            // Do I need to do evict control here??
            // Evict the SRK handle.
            let result = tpm_engine_helper.evict_control(
                tpm::tpm20proto::TPM20_RH_OWNER,
                response.object_handle,
                TPM_RSA_SRK_HANDLE,
            );
            assert!(result.is_ok());

            // TODO: Do we need to change_seed() or the creation assigns ones?

        }
        Err(e) => {
            println!("Error in create_primary: {:?}", e);
        }
    }
    //assert!(result.is_ok());

    // Save the state of the TPM engine.
    tpm_engine_helper.tpm_engine.save_state();
}

fn export_vtpm_srk_pub(mut tpm_engine_helper: TpmEngineHelper, srk_out_path: &str) {
    // Create a vTPM instance.
    println!("Initializing TPM engine.");
    let result = tpm_engine_helper.initialize_tpm_engine();
    assert!(result.is_ok());
    print!("TPM engine initialized.");

    // Extract SRK primary key public area.
    let result = tpm_engine_helper.read_public(TPM_RSA_SRK_HANDLE);
    match result {
        Ok(response) => {
            println!("SRK public area: {:?}", response.out_public.public_area);

            // Write the SRK pub to a file.
            //let srk_pub_file = Use the input.
            let mut srk_pub_file = File::create(srk_out_path).expect("failed to create file");
            let srk_pub = response.out_public.serialize();
            let srk_pub = srk_pub.as_slice();
            srk_pub_file
                .write_all(&srk_pub)
                .expect("failed to write to file");
        }
        Err(e) => {
            println!("Error in read_public: {:?}", e);
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
    println!("Printing key properties.\n");
    println!("Public key type: {:?}", public_area.r#type);
    println!("Public hash alg: {:?}", public_area.name_alg);
    println!("Public key size in bits: {:?}", public_area.parameters.key_bits);
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
    println!("Key name:\n");
    println!(" {}", base64_key);

    // Print RSA bytes in hex to be able to compare with tpm2_readpublic -c 0x81000001
    println!("\nRSA key bytes:");
    print!("  ");
    for i in 0..tpm_helper::RSA_2K_MODULUS_SIZE {
        // if i % 16 == 0 {
        //     println!();
        //     print!("  ");
        // }
        print!("{:02x}", rsa_key.buffer[i]);
    }
    println!();
    println!("\nOperation completed successfully.\n");
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
            let public_key_der = rsa.public_key_to_der_pkcs1().unwrap();
            let pkey = PKey::from_rsa(rsa).unwrap();
            println!("RSA 2048-bit key generated.");

            // Export the public key to a file in pem format
            let mut pub_file = File::create(public_key_file).unwrap();
            pub_file.write_all(&public_key_der).unwrap();
            println!("RSA public key saved to {public_key_file}.");
            print_sha256_hash(public_key_der.as_slice());

            // Convert the private key to TPM2B format
            let tpm2_import_blob = get_key_in_tpm2_import_format_rsa(&pkey);

            // Save the TPM2B private key to a file
            let mut priv_file = File::create(private_key_tpm2b_file).unwrap();
            priv_file.write_all(&tpm2_import_blob).unwrap();
            println!("RSA private key in TPM2B import format saved to {private_key_tpm2b_file}.");
            let private_key_der = pkey.private_key_to_der().unwrap();
            print_sha256_hash(&private_key_der.as_slice());
        }
        "ecc" => {
            // Create a random ECC P-256 key using openssl-sys crate.
            let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
            let ec_key = EcKey::generate(&group).unwrap();
            let pkey = PKey::from_ec_key(ec_key).unwrap();
            println!("ECC P-256 key generated.");

            // Export the public key to a file
            let public_key_pem = pkey.public_key_to_pem().unwrap();
            let mut pub_file = File::create(public_key_file).unwrap();
            pub_file.write_all(&public_key_pem).unwrap();
            println!("ECC public key saved to {public_key_file}.");

            // Convert the private key to TPM2B format
            // TODO: define the ECC version for TPM2B format
            let tpm2_import_blob = get_key_in_tpm2_import_format_rsa(&pkey);

            // Save the TPM2B private key to a file
            let mut priv_file = File::create(private_key_tpm2b_file).unwrap();
            priv_file.write_all(&tpm2_import_blob).unwrap();

            println!("ECC private key in TPM2B import format saved to {private_key_tpm2b_file}.");
        }
        _ => {
            println!("Invalid algorithm. Supported algorithms are rsa and ecc.");
            return;
        }
    }
}

// Convert the private key to TPM2B format
fn get_key_in_tpm2_import_format_rsa(priv_key: &PKey<openssl::pkey::Private>) -> Vec<u8> {
    let rsa = priv_key.rsa().unwrap();

    let key_bits: u16 = rsa.size() as u16 * 8; // 2048;
    println!("Key bits: {:?}", key_bits);
    let exponent = 0;
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
        &Tpm2bBuffer::new(rsa.n().to_vec().as_slice())
        .unwrap()
        .buffer,
    )
    .unwrap();
    let tpm2b_public = Tpm2bPublic::new(tpmt_public_area);

    // Create a TPM2B_PRIVATE structure
    // let cb_public_exp = rsa.e().to_vec().len();
    // let cb_modulus = rsa.n().to_vec().len();
    let priv_prime1 = Tpm2bBuffer::new(rsa.p().unwrap().to_vec().as_slice()).unwrap();
    let rsa_sensitive = TpmtSensitive {
        sensitive_type: tpmt_public_area.r#type,
        auth_value: Tpm2bBuffer::new_zeroed(),
        seed_value: Tpm2bBuffer::new_zeroed(),
        sensitive: priv_prime1,
    };
    //let source_slice = &soft_rsa_key_blob[offset..offset + cb_prime1];
    //rsa_sensitive.sensitive.buffer[..cb_prime1].copy_from_slice(source_slice);
    let marshaled_tpmt_sensitive = marshal::tpmt_sensitive_marshal(&rsa_sensitive).unwrap();
    let tpm2b_private = Tpm2bBuffer::new(marshaled_tpmt_sensitive.as_slice()).unwrap();

    // Marshal the TPM2B_PUBLIC and TPM2B_PRIVATE structures into TP2B_IMPORT format
    // TPM2B_IMPORT = TPM2B_PUBLIC || TPM2B_PRIVATE || TPM2B_ENCRYPTED_SEED
    let tpm2b_import = marshal::marshal_tpm2b_import(&tpm2b_public, &tpm2b_private).unwrap();

    tpm2b_import.to_vec()
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
    println!("Key type: {:?}", pkey.id());
    println!("Key size: {:?}", pkey.bits());
    print_sha256_hash(pkey.public_key_to_der().unwrap().as_slice());

    println!("\nOperation completed successfully.\n");
}

/// Print SHA256 hash of the data.
fn print_sha256_hash(data: &[u8]) {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let hash = hasher.finalize();
    println!("SHA256 hash:");
    print!("  ");
    for i in 0..hash.len() {
        if i % 16 == 0 {
            println!();
            print!("  ");
        }
        print!("{:02X} ", hash[i]);
    }
    println!();
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
    println!("TPM2B import file size: {:?}", tpm2b_import_content.len());

    // Reverse the operations in get_key_in_tpm2_import_format_rsa
    // Deserialize the tpm2b import to a Tpm2bPublic and Tpm2bBuffer.
    let tpm2b_public = Tpm2bPublic::deserialize(&tpm2b_import_content)
        .expect("failed to deserialize tpm2b public");
    println!("TPM2B public size: {:?}", tpm2b_public.size);
    println!("TPM2B public type: {:?}", tpm2b_public.public_area.r#type);
    let tpm2b_public_size = u16::from_be(tpm2b_public.size.into()) as usize;
    println!("TPM2B public size: {:?}", tpm2b_public_size);
    let tpm2b_private = Tpm2bBuffer::deserialize(&tpm2b_import_content[tpm2b_public_size..])
        .expect("failed to deserialize tpm2b private");
    println!("TPM2B private size: {:?}", tpm2b_private.size);
    
    // // Deserialize the priv key der to a rsa private key from the TPM2B format.
    // // Ignore first 2 bytes of the TPM2B format.
    // // Read length of the key from the next 2 bytes in BE byte order.
    // let len =
    //     u16::from_be_bytes([priv_key_tpm2b_content_buf[2], priv_key_tpm2b_content_buf[3]]) as usize;
    // println!("Length of the tpm2b : {:?}", len);
    // // Read the key from the rest of the buffer.
    // let rsa = Rsa::private_key_from_der(&priv_key_tpm2b_content_buf[4..4 + len])
    //     .expect("failed to deserialize priv key tpm2b");
    //let pkey = PKey::from_rsa(rsa).unwrap();

    println!("\nOperation completed successfully.\n");
}

struct TestPlatformCallbacks {
    file: File,
    time: Instant,
}

impl ms_tpm_20_ref::PlatformCallbacks for TestPlatformCallbacks {
    fn commit_nv_state(&mut self, state: &[u8]) -> DynResult<()> {
        tracing::info!("commit_nv_state: {:?}", state);
        tracing::info!("committing nv state with len {}", state.len());
        self.file.rewind()?;
        self.file.write_all(state)?;
        Ok(())
    }

    fn get_crypt_random(&mut self, buf: &mut [u8]) -> DynResult<usize> {
        getrandom::getrandom(buf).expect("rng failure");

        Ok(buf.len())
    }

    fn monotonic_timer(&mut self) -> std::time::Duration {
        self.time.elapsed()
    }

    fn get_unique_value(&self) -> &'static [u8] {
        b"vtpm test"
    }
}

/// Create a new TPM engine with blank state.
pub fn create_tpm_engine_helper(file: File) -> TpmEngineHelper {
    let result = MsTpm20RefPlatform::initialize(
        Box::new(TestPlatformCallbacks {
            file,
            time: Instant::now(),
        }),
        ms_tpm_20_ref::InitKind::ColdInit,
    );
    assert!(result.is_ok());

    let tpm_engine: MsTpm20RefPlatform = result.unwrap();

    TpmEngineHelper {
        tpm_engine,
        reply_buffer: [0u8; 4096],
    }
}

/// Restore a TPM engine helper from vtpm blob.
pub fn restore_tpm_engine_helper(vtpm_blob_path: String) -> TpmEngineHelper {
    let mut vtpm_blob_file = fs::OpenOptions::new()
        .write(true)
        .read(true)
        .open(vtpm_blob_path)
        .expect("failed to open file");

    let mut vtpm_content_buf = Vec::new();
    vtpm_blob_file
        .read_to_end(&mut vtpm_content_buf)
        .expect("failed to read file");

    let result = MsTpm20RefPlatform::initialize(
        Box::new(TestPlatformCallbacks {
            file: vtpm_blob_file,
            time: Instant::now(),
        }),
        ms_tpm_20_ref::InitKind::ColdInitWithPersistentState {
            nvmem_blob: vtpm_content_buf.into(),
        },
    );
    assert!(result.is_ok());

    let tpm_engine: MsTpm20RefPlatform = result.unwrap();

    TpmEngineHelper {
        tpm_engine,
        reply_buffer: [0u8; 4096],
    }
}
