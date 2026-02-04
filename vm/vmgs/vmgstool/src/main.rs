// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

mod storage_backend;
#[cfg(feature = "test_helpers")]
mod test;
mod uefi_nvram;
mod vmgs_json;

#[cfg(feature = "test_helpers")]
use crate::test::TestOperation;
use anyhow::Result;
use clap::Args;
use clap::Parser;
use disk_backend::Disk;
use disk_vhd1::Vhd1Disk;
use fs_err::File;
use pal_async::DefaultPool;
use std::io::prelude::*;
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;
use uefi_nvram::UefiNvramOperation;
use vmgs::Error as VmgsError;
use vmgs::GspType;
use vmgs::Vmgs;
use vmgs::vmgs_helpers::get_active_header;
use vmgs::vmgs_helpers::read_headers;
use vmgs::vmgs_helpers::validate_header;
use vmgs_format::EncryptionAlgorithm;
use vmgs_format::FileId;
use vmgs_format::VMGS_BYTES_PER_BLOCK;
use vmgs_format::VMGS_DEFAULT_CAPACITY;
use vmgs_format::VMGS_ENCRYPTION_KEY_SIZE;
use vmgs_format::VmgsHeader;

const ONE_MEGA_BYTE: u64 = 1024 * 1024;
const ONE_GIGA_BYTE: u64 = ONE_MEGA_BYTE * 1024;
const VHD_DISK_FOOTER_PACKED_SIZE: u64 = 512;

#[derive(Debug, Error)]
enum Error {
    #[error("VMGS file IO")]
    VmgsFile(#[source] std::io::Error),
    #[error("VHD file error")]
    Vhd1(#[source] disk_vhd1::OpenError),
    #[error("invalid disk")]
    InvalidDisk(#[source] disk_backend::InvalidDisk),
    #[error("VMGS format")]
    Vmgs(#[source] vmgs::Error),
    #[error("VMGS file already exists")]
    FileExists,
    #[cfg(with_encryption)]
    #[error("Adding encryption key")]
    EncryptionKey(#[source] vmgs::Error),
    #[error("Data file / STDOUT IO")]
    DataFile(#[source] std::io::Error),
    #[error("The VMGS file has zero size")]
    ZeroSize,
    #[error("The VMGS file has a non zero size but the contents are empty")]
    EmptyFile,
    #[error("Invalid VMGS file size: {0} {1}")]
    InvalidVmgsFileSize(u64, String),
    #[error("Key file IO")]
    KeyFile(#[source] std::io::Error),
    #[error("Key must be {0} bytes long, is {1} bytes instead")]
    InvalidKeySize(u64, u64),
    #[error("File is not encrypted")]
    NotEncrypted,
    #[error("File must be decrypted to perform this operation but no key was provided")]
    EncryptedNoKey,
    #[error("File is VMGSv1 format")]
    V1Format,
    #[error("VmgsStorageBackend")]
    VmgsStorageBackend(#[from] storage_backend::EncryptionNotSupported),
    #[error("NVRAM storage")]
    NvramStorage(#[from] uefi_nvram_storage::NvramStorageError),
    #[error("UEFI NVRAM variable parsing")]
    NvramParsing(#[from] uefi_nvram_specvars::ParseError),
    #[error("NVRAM entry not found: {0}")]
    MissingNvramEntry(ucs2::Ucs2LeVec),
    #[error("GUID parsing")]
    Guid(#[from] guid::ParseError),
    #[error("JSON parsing")]
    SerdeJson(#[from] serde_json::Error),
    #[error("Bad JSON contents: {0}")]
    Json(String),
    #[error("File ID {0:?} already exists. Use `--allow-overwrite` to ignore.")]
    FileIdExists(FileId),
    #[error("VMGS file is encrypted using GspById")]
    GspByIdEncryption,
    #[error("VMGS file is encrypted using an unknown encryption scheme")]
    GspUnknown,
    #[error("VMGS file is using an unknown encryption algorithm")]
    EncryptionUnknown,
    #[error("Unable to parse IGVM file")]
    IgvmFile(#[source] anyhow::Error),
}

impl From<vmgs::Error> for Error {
    fn from(value: vmgs::Error) -> Self {
        match value {
            vmgs::Error::EmptyFile => Error::EmptyFile,
            vmgs::Error::V1Format => Error::V1Format,
            e => Error::Vmgs(e),
        }
    }
}

/// Automation requires certain exit codes to be guaranteed
/// main matches Error enum to ExitCode
///
/// query-encryption must return ErrorNotEncrypted if file is not encrypted
/// dump-headers must return ErrorEmpty when file is blank
/// query-size must return ErrorNotFound when file id is uninitialized
/// ExitCode::Error returned for all other errors
#[derive(Debug, Clone, Copy)]
#[repr(i32)]
enum ExitCode {
    Error = 1,
    ErrorNotEncrypted = 2,
    ErrorEmpty = 3,
    ErrorNotFound = 4,
    ErrorV1 = 5,
    ErrorGspById = 6,
    ErrorGspUnknown = 7,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[repr(u32)]
enum ResourceCode {
    #[value(name = "NONCONFIDENTIAL")]
    NonConfidential = 13510,
    #[value(name = "SNP")]
    Snp = 13515,
    #[value(name = "SNP_NO_HCL")]
    SnpNoHcl = 13516,
    #[value(name = "TDX")]
    Tdx = 13520,
    #[value(name = "TDX_NO_HCL")]
    TdxNoHcl = 13521,
}

#[derive(Args)]
struct FilePathArg {
    /// VMGS file path
    #[clap(short = 'f', long, alias = "filepath")]
    file_path: PathBuf,
}

#[derive(Args)]
struct KeyPathArg {
    /// Encryption key file path. The file must contain a key that is 32 bytes long.
    #[clap(short = 'k', long, alias = "keypath")]
    key_path: Option<PathBuf>,
}

#[derive(Args)]
struct FileIdArg {
    /// VMGS File ID
    #[clap(short = 'i', long, alias = "fileid", value_parser = parse_file_id)]
    file_id: FileId,
}

#[derive(Parser)]
#[clap(name = "vmgstool", about = "Tool to interact with VMGS files.")]
#[clap(long_about = r#"Tool to interact with VMGS files.

Unless otherwise noted, everything written to STDOUT and STDERR is unstable
and subject to change. Automated consumers of VmgsTool should generally parse
only the exit code. In some cases, the STDOUT of specific subcommands may be
made stable (ex: query-size). STDERR is for human-readable debug messages and
is never stable."#)]
enum Options {
    /// Create and initialize `filepath` as a VMGS file of size `filesize`.
    ///
    /// `keypath` and `encryptionalgorithm` must both be specified if encrypted
    /// guest state is required.
    Create {
        #[command(flatten)]
        file_path: FilePathArg,
        /// VMGS file size, default = 4194816 (~4MB)
        #[clap(short = 's', long, alias = "filesize")]
        file_size: Option<u64>,
        /// Encryption key file path. The file must contain a key that is 32 bytes long.
        ///
        /// `encryptionalgorithm` must also be specified when using this flag.
        #[clap(
            short = 'k',
            long,
            alias = "keypath",
            requires = "encryption_algorithm"
        )]
        key_path: Option<PathBuf>,
        /// Encryption algorithm. Currently AES_GCM is the only algorithm supported.
        ///
        /// `keypath` must also be specified when using this flag.
        #[clap(short = 'e', long, alias = "encryptionalgorithm", requires = "key_path", value_parser = parse_encryption_algorithm)]
        encryption_algorithm: Option<EncryptionAlgorithm>,
        /// Force creation of the VMGS file. If the VMGS filepath already exists,
        /// this flag allows an existing file to be overwritten.
        #[clap(long, alias = "forcecreate")]
        force_create: bool,
    },
    /// Write data into the specified file ID of the VMGS file.
    ///
    /// The proper key file must be specified to write encrypted data.
    Write {
        #[command(flatten)]
        file_path: FilePathArg,
        /// Data file path to read
        #[clap(short = 'd', long, alias = "datapath")]
        data_path: PathBuf,
        #[command(flatten)]
        file_id: FileIdArg,
        #[command(flatten)]
        key_path: KeyPathArg,
        /// Overwrite the VMGS data at `fileid`, even if it already exists with nonzero size
        #[clap(long, alias = "allowoverwrite")]
        allow_overwrite: bool,
    },
    /// Dump/read data from the specified file ID of the VMGS file.
    ///
    /// The proper key file must be specified to read encrypted data. If the data
    /// is encrypted and no key is specified, the data will be dumped without
    /// decrypting.
    Dump {
        #[command(flatten)]
        file_path: FilePathArg,
        /// Data file path to write
        #[clap(short = 'd', long, alias = "datapath")]
        data_path: Option<PathBuf>,
        #[command(flatten)]
        file_id: FileIdArg,
        #[command(flatten)]
        key_path: KeyPathArg,
        /// When dumping to stdout, dump data as raw bytes instead of ASCII hex
        #[clap(long, conflicts_with = "data_path")]
        raw_stdout: bool,
    },
    /// Dump headers of the VMGS file at `filepath` to the console.
    DumpHeaders {
        #[command(flatten)]
        file_path: FilePathArg,
    },
    /// Get the size of the specified `fileid` within the VMGS file
    ///
    /// The STDOUT of this subcommand is stable and contains only the file size.
    QuerySize {
        #[command(flatten)]
        file_path: FilePathArg,
        #[command(flatten)]
        file_id: FileIdArg,
    },
    /// Replace the current encryption key with a new provided key
    ///
    /// Both key files must contain a key that is 32 bytes long.
    UpdateKey {
        #[command(flatten)]
        file_path: FilePathArg,
        /// Current encryption key file path.
        #[clap(short = 'k', long, alias = "keypath")]
        key_path: PathBuf,
        /// New encryption key file path.
        #[clap(short = 'n', long, alias = "newkeypath")]
        new_key_path: PathBuf,
        /// Encryption algorithm. Currently AES_GCM is the only algorithm supported.
        #[clap(short = 'e', long, alias = "encryptionalgorithm", value_parser = parse_encryption_algorithm)]
        encryption_algorithm: EncryptionAlgorithm,
    },
    /// Encrypt an existing VMGS file
    Encrypt {
        #[command(flatten)]
        file_path: FilePathArg,
        /// Encryption key file path. The file must contain a key that is 32 bytes long.
        #[clap(short = 'k', long, alias = "keypath")]
        key_path: PathBuf,
        /// Encryption algorithm. Currently AES_GCM is the only algorithm supported.
        #[clap(short = 'e', long, alias = "encryptionalgorithm", value_parser = parse_encryption_algorithm)]
        encryption_algorithm: EncryptionAlgorithm,
    },
    /// Query whether a VMGS file is encrypted
    QueryEncryption {
        #[command(flatten)]
        file_path: FilePathArg,
    },
    /// Move data to a new file id
    Move {
        #[command(flatten)]
        file_path: FilePathArg,
        /// Source VMGS File ID
        #[clap(long, alias = "src", value_parser = parse_file_id)]
        src_file_id: FileId,
        /// Destination VMGS File ID
        #[clap(long, alias = "dst", value_parser = parse_file_id)]
        dst_file_id: FileId,
        #[command(flatten)]
        key_path: KeyPathArg,
        /// Overwrite the VMGS data at `dst_file_id`, even if it already exists
        #[clap(long, alias = "allowoverwrite")]
        allow_overwrite: bool,
    },
    /// Delete a file id
    Delete {
        #[command(flatten)]
        file_path: FilePathArg,
        #[command(flatten)]
        file_id: FileIdArg,
    },
    /// Dump information about all the File IDs allocated in the VMGS file.
    DumpFileTable {
        #[command(flatten)]
        file_path: FilePathArg,
        #[command(flatten)]
        key_path: KeyPathArg,
    },
    /// UEFI NVRAM operations
    UefiNvram {
        #[clap(subcommand)]
        operation: UefiNvramOperation,
    },
    #[cfg(feature = "test_helpers")]
    /// Create a test VMGS file
    Test {
        #[clap(subcommand)]
        operation: TestOperation,
    },
    /// Copy the IGVM file from a DLL into file ID 8 of the VMGS file.
    CopyIgvmfile {
        #[command(flatten)]
        file_path: FilePathArg,
        /// DLL file path to read
        #[clap(short = 'd', long, alias = "datapath")]
        data_path: PathBuf,
        /// Overwrite the VMGS data at `fileid 8`, even if it already exists with nonzero size
        #[clap(long, alias = "allowoverwrite")]
        allow_overwrite: bool,
        /// Resource code
        #[clap(short = 'r', long, alias = "resourcecode", value_enum)]
        resource_code: ResourceCode,
    },
}

fn parse_file_id(file_id: &str) -> Result<FileId, std::num::ParseIntError> {
    Ok(match file_id {
        "FILE_TABLE" => FileId::FILE_TABLE,
        "BIOS_NVRAM" => FileId::BIOS_NVRAM,
        "TPM_PPI" => FileId::TPM_PPI,
        "TPM_NVRAM" => FileId::TPM_NVRAM,
        "RTC_SKEW" => FileId::RTC_SKEW,
        "ATTEST" => FileId::ATTEST,
        "KEY_PROTECTOR" => FileId::KEY_PROTECTOR,
        "VM_UNIQUE_ID" => FileId::VM_UNIQUE_ID,
        "GUEST_FIRMWARE" => FileId::GUEST_FIRMWARE,
        "CUSTOM_UEFI" => FileId::CUSTOM_UEFI,
        "GUEST_WATCHDOG" => FileId::GUEST_WATCHDOG,
        "HW_KEY_PROTECTOR" => FileId::HW_KEY_PROTECTOR,
        "GUEST_SECRET_KEY" => FileId::GUEST_SECRET_KEY,
        "HIBERNATION_FIRMWARE" => FileId::HIBERNATION_FIRMWARE,
        "PLATFORM_SEED" => FileId::PLATFORM_SEED,
        "PROVENANCE_DOC" => FileId::PROVENANCE_DOC,
        "TPM_NVRAM_BACKUP" => FileId::TPM_NVRAM_BACKUP,
        "EXTENDED_FILE_TABLE" => FileId::EXTENDED_FILE_TABLE,
        v => FileId(v.parse::<u32>()?),
    })
}

fn parse_encryption_algorithm(algorithm: &str) -> Result<EncryptionAlgorithm, &'static str> {
    match algorithm {
        "AES_GCM" => Ok(EncryptionAlgorithm::AES_GCM),
        _ => Err("Encryption algorithm not supported"),
    }
}

fn extract_version(ver: u32) -> String {
    let major = (ver >> 16) & 0xFF;
    let minor = ver & 0xFF;
    format!("{major}.{minor}")
}

fn parse_legacy_args() -> Vec<String> {
    use std::env;
    let mut args: Vec<String> = env::args().collect();
    if let Some(cmd) = args.get(1) {
        let cmd_lower = cmd.to_ascii_lowercase();
        let new_cmd = match &cmd_lower[..] {
            "-c" | "-create" => Some("create"),
            "-w" | "-write" => Some("write"),
            "-r" | "-dump" => Some("dump"),
            "-rh" | "-dumpheaders" => Some("dump-headers"),
            "-qs" | "-querysize" => Some("query-size"),
            "-uk" | "-updatekey" => Some("update-key"),
            "-e" | "-encrypt" => Some("encrypt"),
            _ => None,
        };

        if let Some(new_cmd) = new_cmd {
            eprintln!("Warning: Using legacy arguments. Please migrate to the new syntax.");
            args[1] = new_cmd.to_string();

            let mut index = 2;
            while let Some(arg) = args.get(index) {
                let arg_lower = arg.to_ascii_lowercase();
                if let Some(new_arg) = match &arg_lower[..] {
                    "-f" | "-filepath" => Some("--file-path"),
                    "-s" | "-filesize" => Some("--file-size"),
                    "-i" | "-fileid" => Some("--file-id"),
                    "-d" | "-datapath" => Some("--data-path"),
                    "-ow" | "-allowoverwrite" => Some("--allow-overwrite"),
                    "-k" | "-keypath" => Some("--key-path"),
                    "-n" | "-newkeypath" => Some("--new-key-path"),
                    "-ea" | "-encryptionalgorithm" => Some("--encryption-algorithm"),
                    "-fc" | "-forcecreate" => Some("--force-create"),
                    _ => None,
                } {
                    args[index] = new_arg.to_string();
                }
                index += 1;
            }
        }
    }
    args
}

fn main() {
    DefaultPool::run_with(async |_| match do_main().await {
        Ok(_) => eprintln!("The operation completed successfully."),
        Err(e) => {
            let exit_code = match e {
                Error::NotEncrypted => ExitCode::ErrorNotEncrypted,
                Error::EmptyFile => ExitCode::ErrorEmpty,
                Error::ZeroSize => ExitCode::ErrorEmpty,
                Error::Vmgs(VmgsError::FileInfoNotAllocated) => ExitCode::ErrorNotFound,
                Error::V1Format => ExitCode::ErrorV1,
                Error::GspByIdEncryption => ExitCode::ErrorGspById,
                Error::GspUnknown => ExitCode::ErrorGspUnknown,
                _ => ExitCode::Error,
            };

            eprintln!("EXIT CODE: {} ({:?})", exit_code as i32, exit_code);
            eprintln!("ERROR: {}", e);
            let mut error_source = std::error::Error::source(&e);
            while let Some(e2) = error_source {
                eprintln!("- {}", e2);
                error_source = e2.source();
            }

            std::process::exit(exit_code as i32);
        }
    })
}

async fn do_main() -> Result<(), Error> {
    let opt = Options::parse_from(parse_legacy_args());

    match opt {
        Options::Create {
            file_path,
            file_size,
            key_path,
            encryption_algorithm,
            force_create,
        } => {
            let encryption_alg_key = encryption_algorithm.map(|x| (x, key_path.unwrap()));
            vmgs_file_create(
                file_path.file_path,
                file_size,
                force_create,
                encryption_alg_key,
            )
            .await
            .map(|_| ())
        }
        Options::Dump {
            file_path,
            data_path,
            file_id,
            key_path,
            raw_stdout,
        } => {
            vmgs_file_read(
                file_path.file_path,
                data_path,
                file_id.file_id,
                key_path.key_path,
                raw_stdout,
            )
            .await
        }
        Options::Write {
            file_path,
            data_path,
            file_id,
            key_path,
            allow_overwrite,
        } => {
            vmgs_file_write(
                file_path.file_path,
                data_path,
                file_id.file_id,
                key_path.key_path,
                allow_overwrite,
            )
            .await
        }
        Options::DumpHeaders { file_path } => vmgs_file_dump_headers(file_path.file_path).await,
        Options::QuerySize { file_path, file_id } => {
            vmgs_file_query_file_size(file_path.file_path, file_id.file_id).await
        }
        Options::UpdateKey {
            file_path,
            key_path,
            new_key_path,
            encryption_algorithm,
        } => {
            vmgs_file_update_key(
                file_path.file_path,
                encryption_algorithm,
                Some(key_path),
                new_key_path,
            )
            .await
        }
        Options::Encrypt {
            file_path,
            key_path,
            encryption_algorithm,
        } => {
            vmgs_file_update_key(
                file_path.file_path,
                encryption_algorithm,
                None as Option<PathBuf>,
                key_path,
            )
            .await
        }
        Options::QueryEncryption { file_path } => {
            vmgs_file_query_encryption(file_path.file_path).await
        }
        Options::Move {
            file_path,
            src_file_id,
            dst_file_id,
            key_path,
            allow_overwrite,
        } => {
            vmgs_file_move(
                file_path.file_path,
                src_file_id,
                dst_file_id,
                key_path.key_path,
                allow_overwrite,
            )
            .await
        }
        Options::Delete { file_path, file_id } => {
            vmgs_file_delete(file_path.file_path, file_id.file_id).await
        }
        Options::DumpFileTable {
            file_path,
            key_path,
        } => vmgs_file_dump_file_table(file_path.file_path, key_path.key_path).await,
        Options::UefiNvram { operation } => uefi_nvram::do_command(operation).await,
        #[cfg(feature = "test_helpers")]
        Options::Test { operation } => test::do_command(operation).await,
        Options::CopyIgvmfile {
            file_path,
            data_path,
            allow_overwrite,
            resource_code,
        } => {
            vmgs_file_copy_igvmfile(
                file_path.file_path,
                data_path,
                allow_overwrite,
                resource_code,
            )
            .await
        }
    }
}

async fn vmgs_file_update_key(
    file_path: impl AsRef<Path>,
    encryption_alg: EncryptionAlgorithm,
    key_path: Option<impl AsRef<Path>>,
    new_key_path: impl AsRef<Path>,
) -> Result<(), Error> {
    let new_encryption_key = read_key_path(new_key_path)?;
    let mut vmgs = vmgs_file_open(file_path, key_path, OpenMode::ReadWriteRequire).await?;

    vmgs_update_key(&mut vmgs, encryption_alg, new_encryption_key.as_ref()).await
}

#[cfg_attr(not(with_encryption), expect(unused_variables))]
async fn vmgs_update_key(
    vmgs: &mut Vmgs,
    encryption_alg: EncryptionAlgorithm,
    new_encryption_key: &[u8],
) -> Result<(), Error> {
    #[cfg(not(with_encryption))]
    unreachable!("encryption requires the encryption feature");
    #[cfg(with_encryption)]
    {
        eprintln!("Updating encryption key");
        vmgs.update_encryption_key(new_encryption_key, encryption_alg)
            .await
            .map_err(Error::EncryptionKey)?;

        Ok(())
    }
}

async fn vmgs_file_create(
    path: impl AsRef<Path>,
    file_size: Option<u64>,
    force_create: bool,
    encryption_alg_key: Option<(EncryptionAlgorithm, impl AsRef<Path>)>,
) -> Result<Vmgs, Error> {
    let disk = vhdfiledisk_create(path, file_size, force_create)?;

    let encryption_key = encryption_alg_key
        .as_ref()
        .map(|(_, key_path)| read_key_path(key_path))
        .transpose()?;
    let encryption_alg_key =
        encryption_alg_key.map(|(alg, _)| (alg, encryption_key.as_deref().unwrap()));

    let vmgs = vmgs_create(disk, encryption_alg_key).await?;

    Ok(vmgs)
}

fn vhdfiledisk_create(
    path: impl AsRef<Path>,
    req_file_size: Option<u64>,
    force_create: bool,
) -> Result<Disk, Error> {
    const MIN_VMGS_FILE_SIZE: u64 = 4 * VMGS_BYTES_PER_BLOCK as u64;
    const SECTOR_SIZE: u64 = 512;

    // validate the VHD size
    let file_size = req_file_size.unwrap_or(VMGS_DEFAULT_CAPACITY);
    if file_size < MIN_VMGS_FILE_SIZE || !file_size.is_multiple_of(SECTOR_SIZE) {
        return Err(Error::InvalidVmgsFileSize(
            file_size,
            format!(
                "Must be a multiple of {} and at least {}",
                SECTOR_SIZE, MIN_VMGS_FILE_SIZE
            ),
        ));
    }

    // check if the file already exists so we know whether to try to preserve
    // the size and footer later
    let exists = Path::new(path.as_ref()).exists();

    // open/create the file
    eprintln!("Creating file: {}", path.as_ref().display());
    let file = match fs_err::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .create_new(!force_create)
        .open(path.as_ref())
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::FileExists);
        }
        Err(err) => return Err(Error::VmgsFile(err)),
    };

    // determine if a resize is necessary
    let existing_size = exists
        .then(|| {
            Ok(file
                .metadata()?
                .len()
                .checked_sub(VHD_DISK_FOOTER_PACKED_SIZE))
        })
        .transpose()
        .map_err(Error::VmgsFile)?
        .flatten();
    let needs_resize =
        !exists || existing_size.is_none_or(|existing_size| file_size != existing_size);

    // resize the file if necessary
    let default_label = if file_size == VMGS_DEFAULT_CAPACITY {
        " (default)"
    } else {
        ""
    };
    if needs_resize {
        eprintln!(
            "Setting file size to {}{}{}",
            file_size,
            default_label,
            existing_size
                .map(|s| format!(" (previous size: {s})"))
                .unwrap_or_default(),
        );
        file.set_len(file_size).map_err(Error::VmgsFile)?;
    } else {
        eprintln!(
            "File size is already {}{}, skipping resize",
            file_size, default_label
        );
    }

    // attempt to open the VHD file if it already existed
    let disk = if needs_resize {
        None
    } else {
        Vhd1Disk::open_fixed(file.try_clone().map_err(Error::VmgsFile)?.into(), false)
            .inspect_err(|e| eprintln!("No valid VHD header found in existing file: {e:#}"))
            .ok()
    };

    // format the VHD if necessary
    let disk = match disk {
        Some(disk) => {
            eprintln!("Valid VHD footer already exists, skipping VHD format");
            disk
        }
        None => {
            eprintln!("Formatting VHD");
            Vhd1Disk::make_fixed(file.file()).map_err(Error::Vhd1)?;
            Vhd1Disk::open_fixed(file.into(), false).map_err(Error::Vhd1)?
        }
    };

    Disk::new(disk).map_err(Error::InvalidDisk)
}

#[cfg_attr(not(with_encryption), expect(unused_mut), expect(unused_variables))]
async fn vmgs_create(
    disk: Disk,
    encryption_alg_key: Option<(EncryptionAlgorithm, &[u8])>,
) -> Result<Vmgs, Error> {
    eprintln!("Formatting VMGS");
    let mut vmgs = Vmgs::format_new(disk, None).await?;

    if let Some((algorithm, encryption_key)) = encryption_alg_key {
        eprintln!("Adding encryption key");
        #[cfg(with_encryption)]
        vmgs.update_encryption_key(encryption_key, algorithm)
            .await
            .map_err(Error::EncryptionKey)?;
        #[cfg(not(with_encryption))]
        unreachable!("Encryption requires the encryption feature");
    }

    Ok(vmgs)
}

async fn vmgs_file_write(
    file_path: impl AsRef<Path>,
    data_path: impl AsRef<Path>,
    file_id: FileId,
    key_path: Option<impl AsRef<Path>>,
    allow_overwrite: bool,
) -> Result<(), Error> {
    eprintln!(
        "Opening source (raw data file): {}",
        data_path.as_ref().display()
    );

    let mut file = File::open(data_path.as_ref()).map_err(Error::DataFile)?;
    let mut buf = Vec::new();

    file.read_to_end(&mut buf).map_err(Error::DataFile)?;

    eprintln!("Read {} bytes", buf.len());

    let encrypt = key_path.is_some();
    let mut vmgs = vmgs_file_open(file_path, key_path, OpenMode::ReadWriteIgnore).await?;

    vmgs_write(&mut vmgs, file_id, &buf, encrypt, allow_overwrite).await?;

    Ok(())
}

async fn vmgs_write(
    vmgs: &mut Vmgs,
    file_id: FileId,
    data: &[u8],
    encrypt: bool,
    allow_overwrite: bool,
) -> Result<(), Error> {
    eprintln!("Writing File ID {} ({:?})", file_id.0, file_id);

    if let Ok(info) = vmgs.get_file_info(file_id) {
        if !allow_overwrite && info.valid_bytes > 0 {
            return Err(Error::FileIdExists(file_id));
        }
        if !encrypt && info.encrypted {
            eprintln!("Warning: overwriting encrypted file with plaintext data")
        }
    }

    if encrypt {
        #[cfg(with_encryption)]
        vmgs.write_file_encrypted(file_id, data).await?;
        #[cfg(not(with_encryption))]
        unreachable!("Encryption requires the encryption feature");
    } else {
        vmgs.write_file_allow_overwrite_encrypted(file_id, data)
            .await?;
    }

    Ok(())
}

/// Get data from VMGS file, and write to `data_path`.
async fn vmgs_file_read(
    file_path: impl AsRef<Path>,
    data_path: Option<impl AsRef<Path>>,
    file_id: FileId,
    key_path: Option<impl AsRef<Path>>,
    raw_stdout: bool,
) -> Result<(), Error> {
    let decrypt = key_path.is_some();
    let mut vmgs = vmgs_file_open(file_path, key_path, OpenMode::ReadOnlyWarn).await?;

    let file_info = vmgs.get_file_info(file_id)?;
    if !decrypt && file_info.encrypted {
        eprintln!("Warning: Reading encrypted file without decrypting");
    }

    let buf = vmgs_read(&mut vmgs, file_id, decrypt).await?;

    eprintln!("Read {} bytes", buf.len());
    if buf.len() != file_info.valid_bytes as usize {
        eprintln!("Warning: Bytes read from VMGS doesn't match file info");
    }

    if let Some(path) = data_path {
        eprintln!("Writing contents to {}", path.as_ref().display());
        let mut file = File::create(path.as_ref()).map_err(Error::DataFile)?;
        file.write_all(&buf).map_err(Error::DataFile)?;
    } else {
        eprintln!("Writing contents to stdout");
        if raw_stdout {
            let mut stdout = std::io::stdout();
            stdout.write_all(&buf).map_err(Error::DataFile)?;
        } else {
            for c in buf.chunks(16) {
                for b in c {
                    print!("0x{:02x},", b);
                }
                println!(
                    "{:missing$}// {}",
                    ' ',
                    c.iter()
                        .map(|c| if c.is_ascii_graphic() {
                            *c as char
                        } else {
                            '.'
                        })
                        .collect::<String>(),
                    missing = (16 - c.len()) * 5 + 1
                );
            }
        }
    }

    Ok(())
}

async fn vmgs_read(vmgs: &mut Vmgs, file_id: FileId, decrypt: bool) -> Result<Vec<u8>, Error> {
    eprintln!("Reading File ID {} ({:?})", file_id.0, file_id);
    Ok(if decrypt {
        vmgs.read_file(file_id).await?
    } else {
        vmgs.read_file_raw(file_id).await?
    })
}

async fn vmgs_file_move(
    file_path: impl AsRef<Path>,
    src: FileId,
    dst: FileId,
    key_path: Option<impl AsRef<Path>>,
    allow_overwrite: bool,
) -> Result<(), Error> {
    let mut vmgs = vmgs_file_open(file_path, key_path, OpenMode::ReadWriteRequire).await?;

    vmgs_move(&mut vmgs, src, dst, allow_overwrite).await
}

async fn vmgs_move(
    vmgs: &mut Vmgs,
    src: FileId,
    dst: FileId,
    allow_overwrite: bool,
) -> Result<(), Error> {
    eprintln!(
        "Moving File ID {} ({:?}) to File ID {} ({:?})",
        src.0, src, dst.0, dst
    );

    vmgs.move_file(src, dst, allow_overwrite).await?;

    Ok(())
}

async fn vmgs_file_delete(file_path: impl AsRef<Path>, file_id: FileId) -> Result<(), Error> {
    let mut vmgs = vmgs_file_open(
        file_path,
        None as Option<PathBuf>,
        OpenMode::ReadWriteIgnore,
    )
    .await?;

    vmgs_delete(&mut vmgs, file_id).await
}

async fn vmgs_delete(vmgs: &mut Vmgs, file_id: FileId) -> Result<(), Error> {
    eprintln!("Deleting File ID {} ({:?})", file_id.0, file_id);

    vmgs.delete_file(file_id).await?;

    Ok(())
}

async fn vmgs_file_dump_file_table(
    file_path: impl AsRef<Path>,
    key_path: Option<impl AsRef<Path>>,
) -> Result<(), Error> {
    let vmgs = vmgs_file_open(file_path, key_path, OpenMode::ReadOnlyWarn).await?;

    vmgs_dump_file_table(&vmgs)
}

fn vmgs_dump_file_table(vmgs: &Vmgs) -> Result<(), Error> {
    println!("FILE TABLE");
    println!(
        "{0:^7} {1:^25} {2:^9} {3:^9} {4:^9}",
        "File ID", "File Name", "Allocated", "Valid", "Encrypted",
    );
    println!(
        "{} {} {} {} {}",
        "-".repeat(7),
        "-".repeat(25),
        "-".repeat(9),
        "-".repeat(9),
        "-".repeat(9),
    );
    for (file_id, file_info) in vmgs.dump_file_table() {
        println!(
            "{0:>7} {1:^25?} {2:>9} {3:>9} {4:^9}",
            file_id.0,
            file_id,
            file_info.allocated_bytes,
            file_info.valid_bytes,
            file_info.encrypted,
        );
    }

    Ok(())
}

async fn vmgs_file_dump_headers(file_path: impl AsRef<Path>) -> Result<(), Error> {
    let file = File::open(file_path.as_ref()).map_err(Error::VmgsFile)?;
    let disk = vhdfiledisk_open(file, OpenMode::ReadOnlyIgnore)?;

    let (headers, res0) = match read_headers(disk).await {
        Ok(headers) => (Some(headers), Ok(())),
        Err((e, headers)) => (headers, Err(e.into())),
    };

    if let Some(headers) = headers {
        let res1 = vmgs_dump_headers(&headers.0, &headers.1);
        if res0.is_err() { res0 } else { res1 }
    } else {
        res0
    }
}

fn vmgs_dump_headers(header1: &VmgsHeader, header2: &VmgsHeader) -> Result<(), Error> {
    println!("FILE HEADERS");
    println!("{0:<23} {1:^70} {2:^70}", "Field", "Header 1", "Header 2");
    println!("{} {} {}", "-".repeat(23), "-".repeat(70), "-".repeat(70));

    let signature1 = format!("{:#018x}", header1.signature);
    let signature2 = format!("{:#018x}", header2.signature);
    println!(
        "{0:<23} {1:>70} {2:>70}",
        "Signature:", signature1, signature2
    );

    println!(
        "{0:<23} {1:>70} {2:>70}",
        "Version:",
        extract_version(header1.version),
        extract_version(header2.version)
    );
    println!(
        "{0:<23} {1:>70x} {2:>70x}",
        "Checksum:", header1.checksum, header2.checksum
    );
    println!(
        "{0:<23} {1:>70} {2:>70}",
        "Sequence:", header1.sequence, header2.sequence
    );
    println!(
        "{0:<23} {1:>70} {2:>70}",
        "HeaderSize:", header1.header_size, header2.header_size
    );

    let file_table_offset1 = format!("{:#010x}", header1.file_table_offset);
    let file_table_offset2 = format!("{:#010x}", header2.file_table_offset);
    println!(
        "{0:<23} {1:>70} {2:>70}",
        "FileTableOffset:", file_table_offset1, file_table_offset2
    );

    println!(
        "{0:<23} {1:>70} {2:>70}",
        "FileTableSize:", header1.file_table_size, header2.file_table_size
    );

    let encryption_algorithm1 = format!("{:#06x}", header1.encryption_algorithm.0);
    let encryption_algorithm2 = format!("{:#06x}", header2.encryption_algorithm.0);
    println!(
        "{0:<23} {1:>70} {2:>70}",
        "EncryptionAlgorithm:", encryption_algorithm1, encryption_algorithm2
    );

    let markers1 = format!("{:#06x}", header1.markers.into_bits());
    let markers2 = format!("{:#06x}", header2.markers.into_bits());

    println!("{0:<23} {1:>70} {2:>70}", "Markers:", markers1, markers2);

    println!("{0:<23}", "MetadataKey1:");

    let key1_nonce = format!("0x{}", hex::encode(header1.metadata_keys[0].nonce));
    let key2_nonce = format!("0x{}", hex::encode(header2.metadata_keys[0].nonce));
    println!(
        "    {0:<19} {1:>70} {2:>70}",
        "Nonce:", key1_nonce, key2_nonce
    );

    let key1_reserved = format!("{:#010x}", header1.metadata_keys[0].reserved);
    let key2_reserved = format!("{:#010x}", header2.metadata_keys[0].reserved);
    println!(
        "    {0:<19} {1:>70} {2:>70}",
        "Reserved:", key1_reserved, key2_reserved
    );

    let key1_auth_tag = format!(
        "0x{}",
        hex::encode(header1.metadata_keys[0].authentication_tag)
    );
    let key2_auth_tag = format!(
        "0x{}",
        hex::encode(header2.metadata_keys[0].authentication_tag)
    );
    println!(
        "    {0:<19} {1:>70} {2:>70}",
        "AuthenticationTag:", key1_auth_tag, key2_auth_tag
    );

    let key1_encryption_key = format!("0x{}", hex::encode(header1.metadata_keys[0].encryption_key));
    let key2_encryption_key = format!("0x{}", hex::encode(header2.metadata_keys[0].encryption_key));
    println!(
        "    {0:<19} {1:>70} {2:>70}",
        "EncryptionKey:", key1_encryption_key, key2_encryption_key
    );

    println!("{0:<23}", "MetadataKey2:");
    let key1_nonce = format!("0x{}", hex::encode(header1.metadata_keys[1].nonce));
    let key2_nonce = format!("0x{}", hex::encode(header2.metadata_keys[1].nonce));
    println!(
        "    {0:<19} {1:>70} {2:>70}",
        "Nonce:", key1_nonce, key2_nonce
    );

    let key1_reserved = format!("0x{:#010x}", header1.metadata_keys[1].reserved);
    let key2_reserved = format!("0x{:#010x}", header2.metadata_keys[1].reserved);
    println!(
        "    {0:<19} {1:>70} {2:>70}",
        "Reserved:", key1_reserved, key2_reserved
    );

    let key1_auth_tag = format!(
        "0x{}",
        hex::encode(header1.metadata_keys[1].authentication_tag)
    );
    let key2_auth_tag = format!(
        "0x{}",
        hex::encode(header2.metadata_keys[1].authentication_tag)
    );
    println!(
        "    {0:<19} {1:>70} {2:>70}",
        "AuthenticationTag:", key1_auth_tag, key2_auth_tag
    );

    let key1_encryption_key = format!("0x{}", hex::encode(header1.metadata_keys[1].encryption_key));
    let key2_encryption_key = format!("0x{}", hex::encode(header2.metadata_keys[1].encryption_key));
    println!(
        "    {0:<19} {1:>70} {2:>70}",
        "EncryptionKey:", key1_encryption_key, key2_encryption_key
    );

    let key1_reserved1 = format!("0x{:#010x}", header1.reserved_1);
    let key2_reserved1 = format!("0x{:#010x}", header2.reserved_1);
    println!(
        "{0:<23} {1:>70} {2:>70}",
        "Reserved:", key1_reserved1, key2_reserved1
    );

    println!("{} {} {}\n", "-".repeat(23), "-".repeat(70), "-".repeat(70));

    print!("Verifying header 1... ");
    let header1_result = validate_header(header1);
    match &header1_result {
        Ok(_) => println!("[VALID]"),
        Err(e) => println!("[INVALID] Error: {}", e),
    }

    print!("Verifying header 2... ");
    let header2_result = validate_header(header2);
    match &header2_result {
        Ok(_) => println!("[VALID]"),
        Err(e) => println!("[INVALID] Error: {}", e),
    }

    match get_active_header(header1_result, header2_result) {
        Ok(active_index) => match active_index {
            0 => println!("Active header is 1"),
            1 => println!("Active header is 2"),
            _ => unreachable!(),
        },
        Err(e) => {
            println!("Unable to determine active header");
            return Err(Error::Vmgs(e));
        }
    }

    Ok(())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[expect(clippy::enum_variant_names)]
enum OpenMode {
    /// Open read-only. Ignore encryption status.
    ReadOnlyIgnore,
    /// Open read-only. Warn if encrypted and no key was provided.
    ReadOnlyWarn,
    /// Open read-write. Ignore encryption status.
    ReadWriteIgnore,
    /// Open read-write. Fail if encrypted and no key was provided.
    ReadWriteRequire,
}

impl OpenMode {
    fn write(&self) -> bool {
        match self {
            OpenMode::ReadOnlyIgnore | OpenMode::ReadOnlyWarn => false,
            OpenMode::ReadWriteIgnore | OpenMode::ReadWriteRequire => true,
        }
    }
}

async fn vmgs_file_open(
    file_path: impl AsRef<Path>,
    key_path: Option<impl AsRef<Path>>,
    open_mode: OpenMode,
) -> Result<Vmgs, Error> {
    eprintln!("Opening VMGS File: {}", file_path.as_ref().display());
    let file = fs_err::OpenOptions::new()
        .read(true)
        .write(open_mode.write())
        .open(file_path.as_ref())
        .map_err(Error::VmgsFile)?;

    let disk = vhdfiledisk_open(file, open_mode)?;

    let encryption_key = key_path.map(read_key_path).transpose()?;

    let res = vmgs_open(disk, encryption_key.as_deref(), open_mode).await;

    if matches!(
        res,
        Err(Error::Vmgs(VmgsError::InvalidFormat(_)))
            | Err(Error::Vmgs(VmgsError::CorruptFormat(_)))
    ) {
        eprintln!("VMGS is corrupted or invalid. Dumping headers.");
        let _ = vmgs_file_dump_headers(file_path.as_ref()).await;
    }

    res
}

#[cfg_attr(not(with_encryption), expect(unused_mut), expect(unused_variables))]
async fn vmgs_open(
    disk: Disk,
    encryption_key: Option<&[u8]>,
    open_mode: OpenMode,
) -> Result<Vmgs, Error> {
    let mut vmgs: Vmgs = Vmgs::open(disk, None).await?;

    if let Some(encryption_key) = encryption_key {
        #[cfg(with_encryption)]
        if vmgs.is_encrypted() {
            vmgs.unlock_with_encryption_key(encryption_key).await?;
        } else {
            return Err(Error::NotEncrypted);
        }
        #[cfg(not(with_encryption))]
        unreachable!("Encryption requires the encryption feature");
    } else if vmgs.is_encrypted() {
        match open_mode {
            OpenMode::ReadWriteRequire => return Err(Error::EncryptedNoKey),
            OpenMode::ReadOnlyWarn => eprintln!(
                "Warning: Opening encrypted VMGS file without decrypting. File ID encryption status may be inaccurate."
            ),
            OpenMode::ReadOnlyIgnore | OpenMode::ReadWriteIgnore => {}
        }
    }

    Ok(vmgs)
}

fn read_key_path(path: impl AsRef<Path>) -> Result<Vec<u8>, Error> {
    eprintln!("Reading encryption key: {}", path.as_ref().display());
    let metadata = fs_err::metadata(&path).map_err(Error::KeyFile)?;
    if metadata.len() != VMGS_ENCRYPTION_KEY_SIZE as u64 {
        return Err(Error::InvalidKeySize(
            VMGS_ENCRYPTION_KEY_SIZE as u64,
            metadata.len(),
        ));
    }

    let bytes = fs_err::read(&path).map_err(Error::KeyFile)?;
    if bytes.len() != metadata.len() as usize {
        return Err(Error::InvalidKeySize(
            VMGS_ENCRYPTION_KEY_SIZE as u64,
            bytes.len() as u64,
        ));
    }

    Ok(bytes)
}

async fn vmgs_file_query_file_size(
    file_path: impl AsRef<Path>,
    file_id: FileId,
) -> Result<(), Error> {
    let vmgs = vmgs_file_open(file_path, None as Option<PathBuf>, OpenMode::ReadOnlyIgnore).await?;

    let file_size = vmgs_query_file_size(&vmgs, file_id)?;

    eprintln!(
        "File ID {} ({:?}) has a size of {}",
        file_id.0, file_id, file_size
    );

    // STABLE OUTPUT
    println!("{file_size}");

    Ok(())
}

fn vmgs_query_file_size(vmgs: &Vmgs, file_id: FileId) -> Result<u64, Error> {
    Ok(vmgs.get_file_info(file_id)?.valid_bytes)
}

async fn vmgs_file_query_encryption(file_path: impl AsRef<Path>) -> Result<(), Error> {
    print!("{} is ", file_path.as_ref().display());

    let vmgs = vmgs_file_open(file_path, None as Option<PathBuf>, OpenMode::ReadOnlyIgnore).await?;

    match (vmgs.get_encryption_algorithm(), vmgs_get_gsp_type(&vmgs)) {
        (EncryptionAlgorithm::NONE, scheme) => {
            println!("not encrypted (encryption scheme: {scheme:?})");
            Err(Error::NotEncrypted)
        }
        (EncryptionAlgorithm::AES_GCM, GspType::GspKey) => {
            println!("encrypted with AES GCM encryption algorithm using GspKey");
            Ok(())
        }
        (EncryptionAlgorithm::AES_GCM, GspType::GspById) => {
            println!("encrypted with AES GCM encryption algorithm using GspById");
            Err(Error::GspByIdEncryption)
        }
        (EncryptionAlgorithm::AES_GCM, GspType::None) => {
            println!(
                "encrypted with AES GCM encryption algorithm using an unknown encryption scheme"
            );
            Err(Error::GspUnknown)
        }
        (alg, scheme) => {
            println!(
                "using an unknown encryption algorithm: {alg:?} (encryption scheme: {scheme:?})"
            );
            Err(Error::EncryptionUnknown)
        }
    }
}

fn vmgs_get_gsp_type(vmgs: &Vmgs) -> GspType {
    if vmgs_query_file_size(vmgs, FileId::KEY_PROTECTOR).is_ok() {
        GspType::GspKey
    } else if vmgs_query_file_size(vmgs, FileId::VM_UNIQUE_ID).is_ok() {
        GspType::GspById
    } else {
        GspType::None
    }
}

fn vhdfiledisk_open(file: File, open_mode: OpenMode) -> Result<Disk, Error> {
    let file_size = file.metadata().map_err(Error::VmgsFile)?.len();
    validate_size(file_size)?;

    let disk = Disk::new(
        Vhd1Disk::open_fixed(file.into(), open_mode == OpenMode::ReadOnlyWarn)
            .map_err(Error::Vhd1)?,
    )
    .map_err(Error::InvalidDisk)?;

    Ok(disk)
}

async fn vmgs_file_copy_igvmfile(
    file_path: impl AsRef<Path>,
    data_path: impl AsRef<Path>,
    allow_overwrite: bool,
    resource_code: ResourceCode,
) -> Result<(), Error> {
    let mut vmgs = vmgs_file_open(file_path, None::<PathBuf>, OpenMode::ReadWriteIgnore).await?;

    eprintln!("Reading IGVM file from: {}", data_path.as_ref().display());

    let bytes = read_igvmfile(data_path.as_ref().to_path_buf(), resource_code).await?;

    vmgs_write(
        &mut vmgs,
        FileId::GUEST_FIRMWARE,
        &bytes,
        // IGVM file is not encrypted
        false,
        allow_overwrite,
    )
    .await?;

    Ok(())
}

async fn read_igvmfile(dll_path: PathBuf, resource_code: ResourceCode) -> Result<Vec<u8>, Error> {
    use std::io::{Read, Seek, SeekFrom};

    let file = File::open(dll_path).map_err(Error::DataFile)?;

    // Try to find the resource in the DLL
    let descriptor = resource_dll_parser::DllResourceDescriptor::new(b"VMFW", resource_code as u32);
    let (start, len) = resource_dll_parser::try_find_resource_from_dll(&file, &descriptor)
        .map_err(|e| Error::IgvmFile(anyhow::anyhow!("Failed to parse DLL: {}", e)))?
        .ok_or_else(|| {
            Error::IgvmFile(anyhow::anyhow!(
                "File is not a valid PE DLL or resource not found"
            ))
        })?;
    // Read the resource data
    let mut file = file;
    file.seek(SeekFrom::Start(start)).map_err(Error::DataFile)?;

    let mut bytes = vec![0u8; len];
    file.read_exact(&mut bytes).map_err(Error::DataFile)?;

    eprintln!("Successfully loaded IGVM file from DLL");
    eprintln!("Read {} bytes", bytes.len());

    Ok(bytes)
}

fn validate_size(file_size: u64) -> Result<(), Error> {
    const MAX_VMGS_FILE_SIZE: u64 = 4 * ONE_GIGA_BYTE;

    if file_size > MAX_VMGS_FILE_SIZE {
        return Err(Error::InvalidVmgsFileSize(
            file_size,
            format!("Must be less than {}", MAX_VMGS_FILE_SIZE),
        ));
    }

    if file_size == 0 {
        return Err(Error::ZeroSize);
    }

    if file_size < VHD_DISK_FOOTER_PACKED_SIZE {
        return Err(Error::InvalidVmgsFileSize(
            file_size,
            format!("Must be greater than {}", VHD_DISK_FOOTER_PACKED_SIZE),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::async_test;
    use tempfile::tempdir;

    async fn test_vmgs_create(
        path: impl AsRef<Path>,
        file_size: Option<u64>,
        force_create: bool,
        encryption_alg_key: Option<(EncryptionAlgorithm, &[u8])>,
    ) -> Result<(), Error> {
        let disk = vhdfiledisk_create(path, file_size, force_create)?;
        let _ = vmgs_create(disk, encryption_alg_key).await?;
        Ok(())
    }

    async fn test_vmgs_open(
        path: impl AsRef<Path>,
        open_mode: OpenMode,
        encryption_key: Option<&[u8]>,
    ) -> Result<Vmgs, Error> {
        let file = fs_err::OpenOptions::new()
            .read(true)
            .write(open_mode.write())
            .open(path.as_ref())
            .map_err(Error::VmgsFile)?;
        let disk = vhdfiledisk_open(file, open_mode)?;
        let vmgs = vmgs_open(disk, encryption_key, open_mode).await?;
        Ok(vmgs)
    }

    async fn test_vmgs_query_file_size(
        file_path: impl AsRef<Path>,
        file_id: FileId,
    ) -> Result<u64, Error> {
        let vmgs =
            vmgs_file_open(file_path, None as Option<PathBuf>, OpenMode::ReadOnlyIgnore).await?;

        vmgs_query_file_size(&vmgs, file_id)
    }

    #[cfg(with_encryption)]
    async fn test_vmgs_query_encryption(
        file_path: impl AsRef<Path>,
    ) -> Result<EncryptionAlgorithm, Error> {
        let vmgs =
            vmgs_file_open(file_path, None as Option<PathBuf>, OpenMode::ReadOnlyIgnore).await?;

        Ok(vmgs.get_encryption_algorithm())
    }

    #[cfg(with_encryption)]
    async fn test_vmgs_update_key(
        file_path: impl AsRef<Path>,
        encryption_alg: EncryptionAlgorithm,
        encryption_key: Option<&[u8]>,
        new_encryption_key: &[u8],
    ) -> Result<(), Error> {
        let mut vmgs =
            test_vmgs_open(file_path, OpenMode::ReadWriteRequire, encryption_key).await?;

        vmgs_update_key(&mut vmgs, encryption_alg, new_encryption_key).await
    }

    // Create a new test file path.
    fn new_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.vmgs");
        (dir, file_path)
    }

    #[async_test]
    async fn read_invalid_file() {
        let (_dir, path) = new_path();

        let result = test_vmgs_open(path, OpenMode::ReadOnlyWarn, None).await;

        assert!(result.is_err());
    }

    #[async_test]
    async fn read_empty_file() {
        let (_dir, path) = new_path();

        test_vmgs_create(&path, None, false, None).await.unwrap();

        let mut vmgs = test_vmgs_open(path, OpenMode::ReadOnlyWarn, None)
            .await
            .unwrap();
        let result = vmgs_read(&mut vmgs, FileId::FILE_TABLE, false).await;
        assert!(result.is_err());
    }

    #[async_test]
    async fn read_write_file() {
        let (_dir, path) = new_path();
        let buf = b"Plain text data".to_vec();

        test_vmgs_create(&path, None, false, None).await.unwrap();

        let mut vmgs = test_vmgs_open(path, OpenMode::ReadWriteRequire, None)
            .await
            .unwrap();

        vmgs_write(&mut vmgs, FileId::ATTEST, &buf, false, false)
            .await
            .unwrap();
        let read_buf = vmgs_read(&mut vmgs, FileId::ATTEST, false).await.unwrap();

        assert_eq!(buf, read_buf);
    }

    #[async_test]
    async fn multiple_write_file() {
        let (_dir, path) = new_path();
        let buf_1 = b"Random super sensitive data".to_vec();
        let buf_2 = b"Other super secret data".to_vec();
        let buf_3 = b"I'm storing so much data".to_vec();

        test_vmgs_create(&path, None, false, None).await.unwrap();

        let mut vmgs = test_vmgs_open(path, OpenMode::ReadWriteRequire, None)
            .await
            .unwrap();

        vmgs_write(&mut vmgs, FileId::BIOS_NVRAM, &buf_1, false, false)
            .await
            .unwrap();
        let read_buf_1 = vmgs_read(&mut vmgs, FileId::BIOS_NVRAM, false)
            .await
            .unwrap();

        assert_eq!(buf_1, read_buf_1);

        vmgs_write(&mut vmgs, FileId::TPM_PPI, &buf_2, false, false)
            .await
            .unwrap();
        let read_buf_2 = vmgs_read(&mut vmgs, FileId::TPM_PPI, false).await.unwrap();

        assert_eq!(buf_2, read_buf_2);

        let result = vmgs_write(&mut vmgs, FileId::BIOS_NVRAM, &buf_3, false, false).await;
        assert!(result.is_err());

        vmgs_write(&mut vmgs, FileId::BIOS_NVRAM, &buf_3, false, true)
            .await
            .unwrap();
        let read_buf_3 = vmgs_read(&mut vmgs, FileId::BIOS_NVRAM, false)
            .await
            .unwrap();

        assert_eq!(buf_2, read_buf_2);
        assert_eq!(buf_3, read_buf_3);
    }

    /// Creates a minimal PE64 DLL with a VMFW resource for testing.
    /// The resource contains `payload` at the specified `resource_id`.
    fn create_test_vmfw_dll(payload: &[u8], resource_id: u32) -> Vec<u8> {
        // PE Header constants
        const DOS_HEADER_SIZE: usize = 64;
        const PE_SIG_SIZE: usize = 4;
        const COFF_HEADER_SIZE: usize = 20;
        const OPTIONAL_HEADER_SIZE: usize = 240;
        const HEADERS_SIZE: usize = 0x200; // File-aligned
        const RSRC_SECTION_SIZE: usize = 0x200;

        let mut pe = vec![0u8; HEADERS_SIZE + RSRC_SECTION_SIZE];

        // DOS Header
        pe[0..2].copy_from_slice(b"MZ"); // e_magic
        pe[60..64].copy_from_slice(&64u32.to_le_bytes()); // e_lfanew

        let mut offset = DOS_HEADER_SIZE;

        // PE Signature
        pe[offset..offset + PE_SIG_SIZE].copy_from_slice(b"PE\0\0");
        offset += PE_SIG_SIZE;

        // COFF File Header (20 bytes)
        pe[offset..offset + 2].copy_from_slice(&0x8664u16.to_le_bytes()); // Machine: AMD64
        pe[offset + 2..offset + 4].copy_from_slice(&1u16.to_le_bytes()); // NumberOfSections
        pe[offset + 16..offset + 18].copy_from_slice(&240u16.to_le_bytes()); // SizeOfOptionalHeader
        pe[offset + 18..offset + 20].copy_from_slice(&0x2022u16.to_le_bytes()); // Characteristics
        offset += COFF_HEADER_SIZE;

        // Optional Header PE32+ (240 bytes)
        let opt_start = offset;
        pe[opt_start..opt_start + 2].copy_from_slice(&0x20bu16.to_le_bytes()); // Magic: PE32+
        pe[opt_start + 56..opt_start + 60].copy_from_slice(&0x3000u32.to_le_bytes()); // SizeOfImage
        pe[opt_start + 60..opt_start + 64].copy_from_slice(&0x200u32.to_le_bytes()); // SizeOfHeaders
        pe[opt_start + 108..opt_start + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes

        // Data directory entry 2: Resource directory (RVA=0x1000, Size=0x200)
        let rsrc_dir_offset = opt_start + 112 + 2 * 8;
        pe[rsrc_dir_offset..rsrc_dir_offset + 4].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[rsrc_dir_offset + 4..rsrc_dir_offset + 8].copy_from_slice(&0x200u32.to_le_bytes());
        offset += OPTIONAL_HEADER_SIZE;

        // Section Header for .rsrc
        pe[offset..offset + 8].copy_from_slice(b".rsrc\0\0\0");
        pe[offset + 8..offset + 12].copy_from_slice(&0x200u32.to_le_bytes()); // VirtualSize
        pe[offset + 12..offset + 16].copy_from_slice(&0x1000u32.to_le_bytes()); // VirtualAddress
        pe[offset + 16..offset + 20].copy_from_slice(&0x200u32.to_le_bytes()); // SizeOfRawData
        pe[offset + 20..offset + 24].copy_from_slice(&0x200u32.to_le_bytes()); // PointerToRawData
        pe[offset + 36..offset + 40].copy_from_slice(&0x40000040u32.to_le_bytes()); // Characteristics

        // Resource section starts at file offset 0x200 (maps to RVA 0x1000)
        let rsrc_base = HEADERS_SIZE;

        // Resource directory layout:
        // 0x00: Root directory (16 bytes) - 1 named entry for "VMFW"
        // 0x10: Root entry (8 bytes) - name RVA + subdirectory RVA
        // 0x18: Type name "VMFW" in UTF-16LE with length prefix (10 bytes)
        // 0x28: Type directory (16 bytes) - 1 ID entry
        // 0x38: Type entry (8 bytes) - ID + subdirectory RVA
        // 0x40: Language directory (16 bytes) - 1 ID entry
        // 0x50: Language entry (8 bytes) - language ID + data entry RVA
        // 0x58: Resource data entry (16 bytes)
        // 0x68: Actual payload data

        // Root directory
        pe[rsrc_base + 12..rsrc_base + 14].copy_from_slice(&1u16.to_le_bytes()); // NumberOfNamedEntries

        // Root entry: name offset with high bit set, subdirectory offset with high bit set
        pe[rsrc_base + 0x10..rsrc_base + 0x14].copy_from_slice(&0x80000018u32.to_le_bytes());
        pe[rsrc_base + 0x14..rsrc_base + 0x18].copy_from_slice(&0x80000028u32.to_le_bytes());

        // Type name "VMFW" at 0x18: length (4) + UTF-16LE
        pe[rsrc_base + 0x18..rsrc_base + 0x1a].copy_from_slice(&4u16.to_le_bytes());
        pe[rsrc_base + 0x1a..rsrc_base + 0x22]
            .copy_from_slice(&[b'V', 0, b'M', 0, b'F', 0, b'W', 0]);

        // Type directory at 0x28
        pe[rsrc_base + 0x28 + 14..rsrc_base + 0x28 + 16].copy_from_slice(&1u16.to_le_bytes()); // NumberOfIdEntries

        // Type entry at 0x38: resource ID + subdirectory offset
        pe[rsrc_base + 0x38..rsrc_base + 0x3c].copy_from_slice(&resource_id.to_le_bytes());
        pe[rsrc_base + 0x3c..rsrc_base + 0x40].copy_from_slice(&0x80000040u32.to_le_bytes());

        // Language directory at 0x40
        pe[rsrc_base + 0x40 + 14..rsrc_base + 0x40 + 16].copy_from_slice(&1u16.to_le_bytes()); // NumberOfIdEntries

        // Language entry at 0x50: language ID + data entry offset (no high bit = data)
        pe[rsrc_base + 0x50..rsrc_base + 0x54].copy_from_slice(&0x0409u32.to_le_bytes()); // English US
        pe[rsrc_base + 0x54..rsrc_base + 0x58].copy_from_slice(&0x58u32.to_le_bytes());

        // Resource data entry at 0x58
        let data_rva = 0x1000u32 + 0x68; // RVA of payload
        pe[rsrc_base + 0x58..rsrc_base + 0x5c].copy_from_slice(&data_rva.to_le_bytes());
        pe[rsrc_base + 0x5c..rsrc_base + 0x60]
            .copy_from_slice(&(payload.len() as u32).to_le_bytes());

        // Copy payload at 0x68
        let payload_offset = rsrc_base + 0x68;
        pe[payload_offset..payload_offset + payload.len()].copy_from_slice(payload);

        pe
    }

    #[async_test]
    async fn read_write_igvmfile() {
        let (_dir, path) = new_path();

        // Create a test DLL with VMFW resource
        let expected_payload = b"TEST_IGVM_FIRMWARE_PAYLOAD_DATA";
        let dll_data = create_test_vmfw_dll(expected_payload, ResourceCode::Snp as u32);

        // Write the test DLL to a temp file
        let dll_path = _dir.path().join("test_vmfw.dll");
        fs_err::write(&dll_path, &dll_data).unwrap();

        test_vmgs_create(&path, Some(ONE_MEGA_BYTE * 8), false, None)
            .await
            .unwrap();

        let mut vmgs = test_vmgs_open(&path, OpenMode::ReadWriteIgnore, None)
            .await
            .unwrap();

        let buf = read_igvmfile(dll_path, ResourceCode::Snp).await.unwrap();

        assert_eq!(buf, expected_payload);

        vmgs_write(&mut vmgs, FileId::GUEST_FIRMWARE, &buf, false, false)
            .await
            .unwrap();

        let read_buf = vmgs_read(&mut vmgs, FileId::GUEST_FIRMWARE, false)
            .await
            .unwrap();

        assert_eq!(buf, read_buf);
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn read_write_encrypted_file() {
        let (_dir, path) = new_path();
        let encryption_key = vec![5; 32];
        let buf_1 = b"123".to_vec();

        test_vmgs_create(
            &path,
            None,
            false,
            Some((EncryptionAlgorithm::AES_GCM, &encryption_key)),
        )
        .await
        .unwrap();

        let mut vmgs = test_vmgs_open(path, OpenMode::ReadWriteRequire, Some(&encryption_key))
            .await
            .unwrap();

        vmgs_write(&mut vmgs, FileId::BIOS_NVRAM, &buf_1, true, false)
            .await
            .unwrap();
        let read_buf = vmgs_read(&mut vmgs, FileId::BIOS_NVRAM, true)
            .await
            .unwrap();

        assert!(read_buf == buf_1);

        // try to normal write encrypted VMGs
        vmgs_write(&mut vmgs, FileId::TPM_PPI, &buf_1, false, false)
            .await
            .unwrap();

        // try to normal read encrypted FileId
        let _encrypted_read = vmgs_read(&mut vmgs, FileId::BIOS_NVRAM, false)
            .await
            .unwrap();
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn encrypted_read_write_plain_file() {
        // You shouldn't be able to use encryption if you create the VMGS
        // file without encryption.
        let (_dir, path) = new_path();
        let encryption_key = vec![5; VMGS_ENCRYPTION_KEY_SIZE];

        test_vmgs_create(&path, None, false, None).await.unwrap();

        let result = test_vmgs_open(path, OpenMode::ReadWriteRequire, Some(&encryption_key)).await;

        assert!(result.is_err());
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn plain_read_write_encrypted_file() {
        let (_dir, path) = new_path();
        let encryption_key = vec![5; 32];
        let buf_1 = b"123".to_vec();

        test_vmgs_create(
            &path,
            None,
            false,
            Some((EncryptionAlgorithm::AES_GCM, &encryption_key)),
        )
        .await
        .unwrap();

        let mut vmgs = test_vmgs_open(path, OpenMode::ReadWriteIgnore, None)
            .await
            .unwrap();

        vmgs_write(&mut vmgs, FileId::VM_UNIQUE_ID, &buf_1, false, false)
            .await
            .unwrap();
        let read_buf = vmgs_read(&mut vmgs, FileId::VM_UNIQUE_ID, false)
            .await
            .unwrap();

        assert!(read_buf == buf_1);
    }

    #[async_test]
    async fn query_size() {
        let (_dir, path) = new_path();
        let buf = b"Plain text data".to_vec();

        test_vmgs_create(&path, None, false, None).await.unwrap();

        {
            let mut vmgs = test_vmgs_open(&path, OpenMode::ReadWriteRequire, None)
                .await
                .unwrap();

            vmgs_write(&mut vmgs, FileId::ATTEST, &buf, false, false)
                .await
                .unwrap();
        }

        let file_size = test_vmgs_query_file_size(&path, FileId::ATTEST)
            .await
            .unwrap();
        assert_eq!(file_size, buf.len() as u64);
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn query_encrypted_file() {
        let (_dir, path) = new_path();
        let encryption_key = vec![5; 32];
        let buf_1 = b"123".to_vec();

        test_vmgs_create(
            &path,
            None,
            false,
            Some((EncryptionAlgorithm::AES_GCM, &encryption_key)),
        )
        .await
        .unwrap();

        {
            let mut vmgs = test_vmgs_open(&path, OpenMode::ReadWriteRequire, Some(&encryption_key))
                .await
                .unwrap();

            vmgs_write(&mut vmgs, FileId::BIOS_NVRAM, &buf_1, true, false)
                .await
                .unwrap();
        }

        let file_size = test_vmgs_query_file_size(&path, FileId::BIOS_NVRAM)
            .await
            .unwrap();
        assert_eq!(file_size, buf_1.len() as u64);
    }

    #[async_test]
    async fn test_validate_vmgs_file_not_empty() {
        let buf: Vec<u8> = (0..255).collect();
        let (_dir, path) = new_path();

        // create an empty (zero-length) file
        {
            fs_err::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
        }

        // verify the file is zero size
        {
            let result = test_vmgs_open(&path, OpenMode::ReadOnlyWarn, None).await;
            assert!(matches!(result, Err(Error::ZeroSize)));
        }

        // create an empty vhd of default size
        {
            vhdfiledisk_create(&path, None, true).unwrap();
        }

        // verify the file is empty (with non-zero size)
        {
            let result = test_vmgs_open(&path, OpenMode::ReadOnlyWarn, None).await;
            assert!(matches!(result, Err(Error::EmptyFile)));
        }

        // write some invalid data to the file
        {
            let mut file = fs_err::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            file.seek(std::io::SeekFrom::Start(1024)).unwrap();
            file.write_all(&buf).unwrap();
        }

        // verify the vmgs is identified as corrupted
        {
            let result = test_vmgs_open(&path, OpenMode::ReadOnlyWarn, None).await;
            matches!(result, Err(Error::Vmgs(vmgs::Error::CorruptFormat(_))));
        }

        // create a valid vmgs
        {
            test_vmgs_create(&path, None, true, None).await.unwrap();
        }

        // sanity check that the positive case works
        {
            test_vmgs_open(&path, OpenMode::ReadOnlyWarn, None)
                .await
                .unwrap();
        }
    }

    #[async_test]
    async fn test_misaligned_size() {
        let (_dir, path) = new_path();
        //File size must be % 512 to be valid, should produce error and file should not be created
        let result = test_vmgs_create(&path, Some(65537), false, None).await;
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[async_test]
    async fn test_forcecreate() {
        let (_dir, path) = new_path();
        let result = test_vmgs_create(&path, Some(4194304), false, None).await;
        assert!(result.is_ok());
        // Recreating file should fail without force create flag
        let result = test_vmgs_create(&path, Some(4194304), false, None).await;
        assert!(result.is_err());
        // Should be able to resize the file when force create is passed in
        let result = test_vmgs_create(&path, Some(8388608), true, None).await;
        assert!(result.is_ok());
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn test_update_encryption_key() {
        let (_dir, path) = new_path();
        let encryption_key = vec![5; 32];
        let new_encryption_key = vec![6; 32];
        let buf_1 = b"123".to_vec();

        test_vmgs_create(
            &path,
            None,
            false,
            Some((EncryptionAlgorithm::AES_GCM, &encryption_key)),
        )
        .await
        .unwrap();

        {
            let mut vmgs = test_vmgs_open(&path, OpenMode::ReadWriteRequire, Some(&encryption_key))
                .await
                .unwrap();

            vmgs_write(&mut vmgs, FileId::BIOS_NVRAM, &buf_1, true, false)
                .await
                .unwrap();
        }

        test_vmgs_update_key(
            &path,
            EncryptionAlgorithm::AES_GCM,
            Some(&encryption_key),
            &new_encryption_key,
        )
        .await
        .unwrap();

        {
            let mut vmgs = test_vmgs_open(&path, OpenMode::ReadOnlyWarn, Some(&new_encryption_key))
                .await
                .unwrap();

            let read_buf = vmgs_read(&mut vmgs, FileId::BIOS_NVRAM, true)
                .await
                .unwrap();
            assert!(read_buf == buf_1);
        }

        // Old key should no longer work
        let result = test_vmgs_open(&path, OpenMode::ReadOnlyWarn, Some(&encryption_key)).await;
        assert!(result.is_err());
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn test_add_encryption_key() {
        let (_dir, path) = new_path();
        let encryption_key = vec![5; 32];
        let buf_1 = b"123".to_vec();

        test_vmgs_create(&path, None, false, None).await.unwrap();

        test_vmgs_update_key(&path, EncryptionAlgorithm::AES_GCM, None, &encryption_key)
            .await
            .unwrap();

        let mut vmgs = test_vmgs_open(&path, OpenMode::ReadWriteRequire, Some(&encryption_key))
            .await
            .unwrap();

        vmgs_write(&mut vmgs, FileId::BIOS_NVRAM, &buf_1, true, false)
            .await
            .unwrap();

        let read_buf = vmgs_read(&mut vmgs, FileId::BIOS_NVRAM, true)
            .await
            .unwrap();

        assert!(read_buf == buf_1);
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn test_query_encryption_update() {
        let (_dir, path) = new_path();
        let encryption_key = vec![5; 32];

        test_vmgs_create(&path, None, false, None).await.unwrap();

        let encryption_algorithm = test_vmgs_query_encryption(&path).await.unwrap();
        assert_eq!(encryption_algorithm, EncryptionAlgorithm::NONE);

        test_vmgs_update_key(&path, EncryptionAlgorithm::AES_GCM, None, &encryption_key)
            .await
            .unwrap();

        let encryption_algorithm = test_vmgs_query_encryption(&path).await.unwrap();
        assert_eq!(encryption_algorithm, EncryptionAlgorithm::AES_GCM);
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn test_query_encryption_new() {
        let (_dir, path) = new_path();
        let encryption_key = vec![5; 32];

        test_vmgs_create(
            &path,
            None,
            false,
            Some((EncryptionAlgorithm::AES_GCM, &encryption_key)),
        )
        .await
        .unwrap();

        let encryption_algorithm = test_vmgs_query_encryption(&path).await.unwrap();
        assert_eq!(encryption_algorithm, EncryptionAlgorithm::AES_GCM);
    }

    #[async_test]
    async fn move_delete_file() {
        let (_dir, path) = new_path();
        let buf = b"Plain text data".to_vec();

        test_vmgs_create(&path, None, false, None).await.unwrap();

        let mut vmgs = test_vmgs_open(path, OpenMode::ReadWriteRequire, None)
            .await
            .unwrap();

        vmgs_write(&mut vmgs, FileId::TPM_NVRAM, &buf, false, false)
            .await
            .unwrap();
        let read_buf = vmgs_read(&mut vmgs, FileId::TPM_NVRAM, false)
            .await
            .unwrap();
        assert_eq!(buf, read_buf);

        vmgs_move(
            &mut vmgs,
            FileId::TPM_NVRAM,
            FileId::TPM_NVRAM_BACKUP,
            false,
        )
        .await
        .unwrap();
        vmgs_read(&mut vmgs, FileId::TPM_NVRAM, false)
            .await
            .unwrap_err();
        let read_buf = vmgs_read(&mut vmgs, FileId::TPM_NVRAM_BACKUP, false)
            .await
            .unwrap();
        assert_eq!(buf, read_buf);
        vmgs_delete(&mut vmgs, FileId::TPM_NVRAM_BACKUP)
            .await
            .unwrap();
        vmgs_read(&mut vmgs, FileId::TPM_NVRAM_BACKUP, false)
            .await
            .unwrap_err();
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn move_delete_file_encrypted() {
        let (_dir, path) = new_path();
        let encryption_key = vec![5; 32];
        let buf_1 = b"123".to_vec();
        let buf_2 = b"456".to_vec();

        test_vmgs_create(
            &path,
            None,
            false,
            Some((EncryptionAlgorithm::AES_GCM, &encryption_key)),
        )
        .await
        .unwrap();

        {
            let mut vmgs = test_vmgs_open(&path, OpenMode::ReadWriteRequire, Some(&encryption_key))
                .await
                .unwrap();

            vmgs_write(&mut vmgs, FileId::BIOS_NVRAM, &buf_2, true, false)
                .await
                .unwrap();
            vmgs_write(&mut vmgs, FileId::TPM_NVRAM, &buf_1, true, false)
                .await
                .unwrap();
            let read_buf = vmgs_read(&mut vmgs, FileId::TPM_NVRAM, true).await.unwrap();
            assert!(read_buf == buf_1);

            vmgs_move(
                &mut vmgs,
                FileId::TPM_NVRAM,
                FileId::TPM_NVRAM_BACKUP,
                false,
            )
            .await
            .unwrap();
            let read_buf = vmgs_read(&mut vmgs, FileId::TPM_NVRAM_BACKUP, true)
                .await
                .unwrap();
            assert!(read_buf == buf_1);
        }

        // delete the file without decrypting, as the cmdline tool would do
        {
            let mut vmgs = test_vmgs_open(&path, OpenMode::ReadWriteIgnore, None)
                .await
                .unwrap();
            vmgs_delete(&mut vmgs, FileId::TPM_NVRAM_BACKUP)
                .await
                .unwrap();
            vmgs_read(&mut vmgs, FileId::TPM_NVRAM_BACKUP, false)
                .await
                .unwrap_err();
        }

        // make sure the file is not corrupted
        {
            let mut vmgs = test_vmgs_open(&path, OpenMode::ReadWriteRequire, Some(&encryption_key))
                .await
                .unwrap();
            let read_buf = vmgs_read(&mut vmgs, FileId::BIOS_NVRAM, true)
                .await
                .unwrap();
            assert!(read_buf == buf_2);
        }
    }
}
