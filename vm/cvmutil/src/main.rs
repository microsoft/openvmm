use ms_tpm_20_ref;
use ms_tpm_20_ref::MsTpm20RefPlatform;
//use tpm::{self, tpm_helper};
use tpm::tpm_helper::{self, TpmEngineHelper};
//use ms_tpm_20_ref::PlatformCallbacks;
//use crate::Tpm;
// use inspect::InspectMut;
use ms_tpm_20_ref::DynResult;
use std::fs;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::time::Instant;
use tpm::TPM_RSA_SRK_HANDLE;
use tracing::Level;

use sha2::{Digest, Sha256};

use clap::Parser;

#[derive(Parser, Debug)]
#[clap(name = "cvmutil", about = "Tool to interact with vtmp blobs.")]
struct CmdArgs {
    /// Creates a vTpm blob and stores to file. Example: ./cvmutil --createvtpmblob vTpm.blob
    #[arg(long = "createvtpmblob", value_name = "path-to-blob-file")]
    createvtpmblob: Option<String>,

    /// Writes the SRK pub in TPM_2B format. Example: ./cvmutil --writeSrk vTpm.blob srk.pub
    #[clap(long = "writeSrk", value_names = &["path-to-blob-file", "path-to-srk-out-file"], number_of_values = 2)]
    write_srk: Option<Vec<String>>,

    /// Prints the tpm key name. Example: CvmUtil.exe -printKeyName srk.pub
    #[arg(long = "printKeyName", value_name = "path-to-srkPub")]
    print_key_name: Option<String>,
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
        let file = fs::File::create(path.clone()).expect("failed to create file");
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
    } else if let Some(srkpub_path) = args.print_key_name {
        print_vtpm_srk_pub_key_name(srkpub_path);
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

            // Write the SRK pub to a file.
            //let srk_pub_file = Use the input.
            let mut srk_pub_file = fs::File::create("srk_pub.bin").expect("failed to create file");
            let srk_pub = response.out_public.public_area.serialize();
            let srk_pub = srk_pub.as_slice();
            srk_pub_file
                .write_all(&srk_pub)
                .expect("failed to write to file");

            // Do I need to do evict control here??
            // Evict the SRK handle.
            let result = tpm_engine_helper.evict_control(
                tpm::tpm20proto::TPM20_RH_OWNER,
                response.object_handle,
                TPM_RSA_SRK_HANDLE,
            );
            assert!(result.is_ok());
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
            let mut srk_pub_file = fs::File::create(srk_out_path).expect("failed to create file");
            let srk_pub = response.out_public.public_area.serialize();
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

    let public_key = tpm::tpm20proto::protocol::Tpm2bPublic::deserialize(&srkpub_content_buf)
        .expect("failed to deserialize srkpub");
    let public_area = public_key.public_area;
    // Compute SHA256 hash of the public area
    let mut hasher = Sha256::new();
    hasher.update(public_area.serialize());
    let result = hasher.finalize();
    println!("Printing key properties.\n");
    println!("SHA256 Hash:");
    print!("  ");
    for i in 0..result.len() {
        print!("{:02X} ", result[i]);
    }
    println!();

    //println!("Key name: {:?}\n", result);
    //let algorithm_id = public_area.name_alg;

    println!("Operation completed successfully.\n");
    
}

struct TestPlatformCallbacks {
    file: fs::File,
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
pub fn create_tpm_engine_helper(file: fs::File) -> TpmEngineHelper {
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
