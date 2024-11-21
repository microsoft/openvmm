use crate::vmgs_file_open;
use crate::Error;
use crate::FilePathArg;
use crate::KeyPathArg;
use crate::OpenMode;
use clap::Subcommand;
use clap::ValueEnum;
use guid::Guid;
use prost::Message;
use std::path::PathBuf;
use vmgs::FileId;
use vmgs::Vmgs;

#[derive(Subcommand)]
pub(crate) enum DiskTableOperation {
    /// List disks
    List,
    /// Add a disk entry
    Add {
        /// The disk identifier
        #[clap(long, short)]
        disk_id: Guid,
        /// The path to a file containing the disk key
        #[clap(long, short)]
        key_path: Option<PathBuf>,
        /// The cipher to use for the disk encryption
        #[clap(long, short)]
        cipher: DiskCipher,
        /// Update the key if it already exists
        #[clap(long, short)]
        allow_overwrite: bool,
    },
    /// Remove a disk key
    Remove {
        /// The disk identifier
        #[clap(long)]
        disk: String,
    },
}

#[derive(ValueEnum, Copy, Clone)]
pub(crate) enum DiskCipher {
    None,
    #[clap(name = "xts-aes-256")]
    XtsAes256,
}

async fn open_file(
    file_path: &FilePathArg,
    key_path: &KeyPathArg,
    open_mode: OpenMode,
) -> Result<Vmgs, Error> {
    vmgs_file_open(
        &file_path.file_path,
        key_path.key_path.as_deref(),
        open_mode,
        false,
    )
    .await
}

pub(crate) async fn do_command(
    file_path: FilePathArg,
    key_path: KeyPathArg,
    operation: DiskTableOperation,
) -> Result<(), Error> {
    match operation {
        DiskTableOperation::List => {
            let mut vmgs = open_file(&file_path, &key_path, OpenMode::ReadOnly).await?;
            let table = read_disk_table(&mut vmgs).await?;
            for entry in &table.disks {
                let cipher = match entry.cipher() {
                    vmgs_format::DiskCipher::Unspecified => {
                        format!("unknown ({})", entry.cipher)
                    }
                    vmgs_format::DiskCipher::None => format!("none"),
                    vmgs_format::DiskCipher::XtsAes256 => format!("xts-aes-256"),
                };
                println!("{disk_id} {cipher}", disk_id = entry.disk_id);
            }
            Ok(())
        }
        DiskTableOperation::Add {
            disk_id,
            key_path: disk_key_path,
            cipher,
            allow_overwrite,
        } => {
            let mut vmgs = open_file(&file_path, &key_path, OpenMode::ReadWrite).await?;
            let mut table = read_disk_table(&mut vmgs).await?;
            let key = if matches!(cipher, DiskCipher::None) {
                if disk_key_path.is_some() {
                    return Err(Error::UnexpectedDiskKeyFile);
                }
                Vec::new()
            } else {
                fs_err::read(disk_key_path.ok_or(Error::MissingDiskKeyFile)?)
                    .map_err(Error::DiskKeyFile)?
            };
            let new_entry = vmgs_format::Disk {
                disk_id: disk_id.to_string(),
                cipher: (match cipher {
                    DiskCipher::None => vmgs_format::DiskCipher::None,
                    DiskCipher::XtsAes256 => vmgs_format::DiskCipher::XtsAes256,
                })
                .into(),
                key,
            };
            if let Some(entry) = table
                .disks
                .iter_mut()
                .find(|k| k.disk_id == new_entry.disk_id)
            {
                if !allow_overwrite {
                    return Err(Error::DiskEntryExists);
                }
                *entry = new_entry;
            } else {
                table.disks.push(new_entry);
            }
            vmgs.write_file_encrypted(FileId::DISK_TABLE, &table.encode_to_vec())
                .await?;
            Ok(())
        }
        DiskTableOperation::Remove { disk } => {
            let mut vmgs = open_file(&file_path, &key_path, OpenMode::ReadWrite).await?;
            let mut table = read_disk_table(&mut vmgs).await?;
            let mut i = 0;
            table.disks.retain_mut(|k| {
                let keep = k.disk_id != disk;
                if !keep {
                    i += 1;
                };
                keep
            });
            if i == 0 {
                return Err(Error::DiskEntryNotFound);
            }
            vmgs.write_file_encrypted(FileId::DISK_TABLE, &table.encode_to_vec())
                .await?;
            Ok(())
        }
    }
}

async fn read_disk_table(vmgs: &mut Vmgs) -> Result<vmgs_format::DiskTable, Error> {
    let data = match vmgs.read_file(FileId::DISK_TABLE).await {
        Ok(data) => data,
        Err(vmgs::Error::FileInfoAllocated) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    vmgs_format::DiskTable::decode(data.as_slice()).map_err(Error::DiskTableCorrupt)
}
