// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions for generating test VMGS files

use crate::Error;
use crate::FilePathArg;
use crate::vhdfiledisk_create;
use crate::vmgs_create;
use clap::Subcommand;
use disk_backend::Disk;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use vmgs::EncryptionAlgorithm;
use vmgs::Vmgs;
use vmgs_format::VMGS_ENCRYPTION_KEY_SIZE;

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
