// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions for generating test VMGS files

use crate::Error;
use crate::FilePathArg;
use crate::OpenMode;
use crate::vhdfiledisk_create;
use crate::vmgs_create;
use crate::vmgs_file_open;
use crate::vmgs_write;
use clap::Subcommand;
use disk_backend::Disk;
use fs_err::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use vmgs::EncryptionAlgorithm;
use vmgs::Vmgs;
use vmgs_format::FileId;
use vmgs_format::VMGS_ENCRYPTION_KEY_SIZE;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[repr(u32)]
pub(crate) enum ResourceCode {
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

#[derive(Subcommand)]
pub(crate) enum TestOperation {
    /// Generate a key to test VMGS encryption
    MakeKey {
        /// Key file path. If not specified, use key.bin.
        #[clap(long)]
        key_path: Option<PathBuf>,
        /// Use a repeating character instead of randomly generating the key.
        #[clap(long)]
        repeated: Option<char>,
        /// Force creation of the key file. If the file already exists,
        /// this flag allows an existing file to be overwritten.
        #[clap(long)]
        force_create: bool,
    },
    /// Create a VMGS file that has two encryption keys
    ///
    /// This is useful for testing the recovery path in the
    /// `update-key` command in this scenario.
    TwoKeys {
        #[command(flatten)]
        file_path: FilePathArg,
        /// First encryption key file path.
        ///
        /// If not specified, generate a random key and write to firstkey.bin
        /// If specified, but does not exist, write random key to path.
        #[clap(long)]
        first_key_path: Option<PathBuf>,
        /// Second encryption key file path.
        ///
        /// If not specified, generate a random key and write to secondkey.bin
        /// If specified, but does not exist, write random key to path.
        #[clap(long)]
        second_key_path: Option<PathBuf>,
        /// Force creation of the key file. If the file already exists,
        /// this flag allows an existing file to be overwritten.
        #[clap(long)]
        force_create: bool,
    },
    /// Copy the IGVM file from a DLL into file ID 8 of the VMGS file.
    CopyIgvmfile {
        #[command(flatten)]
        file_path: FilePathArg,
        /// DLL file path to read
        #[clap(short = 'd', long, alias = "datapath")]
        data_path: PathBuf,
        /// Overwrite the VMGS data at file ID 8 (FileId::GUEST_FIRMWARE), even if it already exists with nonzero size
        #[clap(long, alias = "allowoverwrite")]
        allow_overwrite: bool,
        /// Resource code
        #[clap(short = 'r', long, alias = "resourcecode", value_enum)]
        resource_code: ResourceCode,
    },
}

pub(crate) async fn do_command(operation: TestOperation) -> Result<(), Error> {
    match operation {
        TestOperation::MakeKey {
            key_path,
            repeated,
            force_create,
        } => make_key(key_path, repeated, force_create).map(|_| ()),
        TestOperation::TwoKeys {
            file_path,
            first_key_path,
            second_key_path,
            force_create,
        } => vmgs_file_two_keys(
            file_path.file_path,
            first_key_path,
            second_key_path,
            force_create,
        )
        .await
        .map(|_| ()),
        TestOperation::CopyIgvmfile {
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

fn make_key(
    key_path: Option<impl AsRef<Path>>,
    repeated: Option<char>,
    force_create: bool,
) -> Result<[u8; VMGS_ENCRYPTION_KEY_SIZE], Error> {
    const DEFAULT_KEY_PATH: &str = "key.bin";
    let key_path = key_path
        .as_ref()
        .map_or_else(|| Path::new(DEFAULT_KEY_PATH), |p| p.as_ref());

    let mut key_file = fs_err::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .create_new(!force_create)
        .truncate(true)
        .open(key_path)
        .map_err(Error::KeyFile)?;

    let key = if let Some(val) = repeated {
        [val as u8; VMGS_ENCRYPTION_KEY_SIZE]
    } else {
        let mut key = [0u8; VMGS_ENCRYPTION_KEY_SIZE];
        getrandom::fill(&mut key).expect("rng failure");
        key
    };

    key_file.write_all(&key).map_err(Error::KeyFile)?;

    Ok(key)
}

async fn vmgs_file_two_keys(
    file_path: impl AsRef<Path>,
    first_key_path_opt: Option<impl AsRef<Path>>,
    second_key_path_opt: Option<impl AsRef<Path>>,
    force_create: bool,
) -> Result<Vmgs, Error> {
    const DEFAULT_FIRST_KEY_PATH: &str = "firstkey.bin";
    const DEFAULT_SECOND_KEY_PATH: &str = "secondkey.bin";

    let first_key_path = first_key_path_opt
        .as_ref()
        .map_or_else(|| Path::new(DEFAULT_FIRST_KEY_PATH), |p| p.as_ref());
    let first_key = make_key(Some(first_key_path), None, false)?;
    let second_key_path = second_key_path_opt
        .as_ref()
        .map_or_else(|| Path::new(DEFAULT_SECOND_KEY_PATH), |p| p.as_ref());
    let second_key = make_key(Some(second_key_path), None, false)?;

    let disk = vhdfiledisk_create(file_path, None, force_create)?;

    vmgs_two_keys(disk, &first_key, &second_key).await
}

#[cfg_attr(
    not(with_encryption),
    expect(unused_mut),
    expect(unreachable_code),
    expect(unused_variables)
)]
async fn vmgs_two_keys(disk: Disk, first_key: &[u8], second_key: &[u8]) -> Result<Vmgs, Error> {
    let mut vmgs = vmgs_create(disk, Some((EncryptionAlgorithm::AES_GCM, first_key))).await?;

    #[cfg(with_encryption)]
    {
        eprintln!("Adding encryption key without removing old key");
        vmgs.test_add_new_encryption_key(second_key, EncryptionAlgorithm::AES_GCM)
            .await?;
    }
    #[cfg(not(with_encryption))]
    unreachable!("Encryption requires the encryption feature");

    Ok(vmgs)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_vmgs_create;
    use crate::tests::test_vmgs_open;
    use crate::vmgs_read;
    use pal_async::async_test;
    use tempfile::tempdir;

    const ONE_MEGA_BYTE: u64 = 1024 * 1024;

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
        let required_len = payload_offset + payload.len();
        if required_len > pe.len() {
            pe.resize(required_len, 0);
        }
        pe[payload_offset..required_len].copy_from_slice(payload);

        pe
    }

    #[async_test]
    async fn read_write_igvmfile() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.vmgs");

        // Create a test DLL with VMFW resource
        let expected_payload = b"TEST_IGVM_FIRMWARE_PAYLOAD_DATA";
        let dll_data = create_test_vmfw_dll(expected_payload, ResourceCode::Snp as u32);

        // Write the test DLL to a temp file
        let dll_path = dir.path().join("test_vmfw.dll");
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
}
