// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions for generating test VMGS files

use crate::Error;
use crate::FilePathArg;
use crate::read_key_path;
use crate::vmgs_file_create;
use clap::Subcommand;
use std::path::Path;
use std::path::PathBuf;
use vmgs::EncryptionAlgorithm;
use vmgs_format::VMGS_ENCRYPTION_KEY_SIZE;

#[derive(Subcommand)]
pub(crate) enum TestOperation {
    /// Create a VMGS file that has two encryption keys
    ///
    /// This is useful for testing recovery the recovery path in the
    /// `update-key` command in this scenario. Also writes an extra key for
    /// convenience when testing that command (extrakey.bin).
    TwoKeys {
        #[command(flatten)]
        file_path: FilePathArg,
        /// First encryption key file path.
        ///
        /// If not specified, use 0x01 repeated and write to firstkey.bin
        /// If specified, but does not exist, write above key to path.
        #[clap(long)]
        first_key_path: Option<PathBuf>,
        /// Second encryption key file path.
        ///
        /// If not specified, use 0x02 repeated and write to secondkey.bin
        /// If specified, but does not exist, write above key to path.
        #[clap(long)]
        second_key_path: Option<PathBuf>,
    },
}

pub(crate) async fn do_command(operation: TestOperation) -> Result<(), Error> {
    match operation {
        TestOperation::TwoKeys {
            file_path,
            first_key_path,
            second_key_path,
        } => vmgs_file_two_keys(file_path.file_path, first_key_path, second_key_path).await,
    }
}

async fn vmgs_file_two_keys(
    file_path: impl AsRef<Path>,
    first_key_path_opt: Option<impl AsRef<Path>>,
    second_key_path_opt: Option<impl AsRef<Path>>,
) -> Result<(), Error> {
    const DEFAULT_FIRST_KEY_PATH: &str = "firstkey.bin";
    const DEFAULT_SECOND_KEY_PATH: &str = "secondkey.bin";
    const EXTRA_KEY_PATH: &str = "extrakey.bin";

    let first_key_path = first_key_path_opt
        .as_ref()
        .map_or_else(|| Path::new(DEFAULT_FIRST_KEY_PATH), |p| p.as_ref());
    if !first_key_path.exists() {
        fs_err::write(first_key_path, [1; VMGS_ENCRYPTION_KEY_SIZE]).map_err(Error::KeyFile)?;
    }
    let second_key_path = second_key_path_opt
        .as_ref()
        .map_or_else(|| Path::new(DEFAULT_SECOND_KEY_PATH), |p| p.as_ref());
    if !second_key_path.exists() {
        fs_err::write(second_key_path, [2; VMGS_ENCRYPTION_KEY_SIZE]).map_err(Error::KeyFile)?;
    }
    // write a third key for convenience
    fs_err::write(EXTRA_KEY_PATH, [3; VMGS_ENCRYPTION_KEY_SIZE]).map_err(Error::KeyFile)?;

    let mut vmgs = vmgs_file_create(
        file_path,
        None,
        false,
        Some((EncryptionAlgorithm::AES_GCM, first_key_path)),
    )
    .await?;

    let second_key = read_key_path(second_key_path)?;
    eprintln!("Adding encryption key without removing old key");
    vmgs.test_add_new_encryption_key(&second_key, EncryptionAlgorithm::AES_GCM)
        .await?;

    Ok(())
}
