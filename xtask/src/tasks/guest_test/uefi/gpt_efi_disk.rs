// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use anyhow::Result;
use fatfs::FileSystem;
use fatfs::FormatVolumeOptions;
use fatfs::FsOptions;
use guid::Guid;
use std::io::Cursor;
use std::io::Seek;
use std::path::Path;

const SECTOR_SIZE: usize = 512;
const EFI_GUID: Guid = guid::guid!("{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}");

pub fn create_gpt_efi_disk(out_img: &Path, with_files: &[(&Path, &Path)]) -> Result<()> {
    if out_img.extension().unwrap_or_default() != "img" {
        return Err(anyhow::anyhow!(
            "only .img disk images are supported at this time"
        ));
    }

    let disk_size = 1024 * 1024 * 32; // 32MB disk should be enough for our tests
    let num_sectors = disk_size / SECTOR_SIZE;

    let mut disk = vec![0; num_sectors * SECTOR_SIZE];

    let efi_partition_range = {
        let mut cur = Cursor::new(&mut disk);
        let mut gpt =
            gptman::GPT::new_from(&mut cur, SECTOR_SIZE as u64, Guid::new_random().into())?;

        // Set up the "Protective" Master Boot Record
        gptman::GPT::write_protective_mbr_into(&mut cur, SECTOR_SIZE as u64)?;

        // Set up the GPT Partition Table Header
        gpt[1] = gptman::GPTPartitionEntry {
            partition_type_guid: EFI_GUID.into(),
            unique_partition_guid: Guid::new_random().into(),
            starting_lba: gpt.header.first_usable_lba,
            ending_lba: gpt.header.last_usable_lba,
            attribute_bits: 0,
            partition_name: "EFI".into(),
        };
        gpt.write_into(&mut cur)?;

        // calculate the EFI partition's usable range
        let partition_start_byte = gpt[1].starting_lba as usize * SECTOR_SIZE;
        let partition_num_bytes = (gpt[1].ending_lba - gpt[1].starting_lba) as usize * SECTOR_SIZE;
        partition_start_byte..partition_start_byte + partition_num_bytes
    };

    init_fat(&mut disk[efi_partition_range], with_files).context("initializing FAT partition")?;

    fs_err::write(out_img, &disk)?;
    log::info!("Wrote test image to: {}", out_img.display());

    Ok(())
}

fn init_fat(partition: &mut [u8], with_files: &[(&Path, &Path)]) -> Result<()> {
    let efi_fs = {
        let mut cursor = Cursor::new(partition);
        fatfs::format_volume(
            &mut cursor,
            FormatVolumeOptions::new()
                .fat_type(fatfs::FatType::Fat32)
                .volume_label(*b"hvlite_test"),
        )?;

        cursor.rewind()?;
        FileSystem::new(cursor, FsOptions::new().update_accessed_date(false))?
    };

    let root_dir = efi_fs.root_dir();
    for (dst_file, src_file) in with_files {
        let ancestors = dst_file.ancestors().collect::<Vec<_>>();
        let num_ancestors = ancestors.len();

        for (i, chunk) in ancestors.into_iter().rev().enumerate() {
            // skip the root '/'
            if i == 0 {
                continue;
            }

            if i != num_ancestors - 1 {
                log::info!("creating dir {}", chunk.display());
                root_dir
                    .create_dir(chunk.to_str().unwrap())
                    .context("creating dir")?;
            } else {
                log::info!("creating file {}", chunk.display());
                let mut file = root_dir
                    .create_file(chunk.to_str().unwrap())
                    .context(format!("creating file {}", chunk.display()))?;
                std::io::copy(&mut fs_err::File::open(src_file)?, &mut file)?;
            }
        }
    }

    log::info!("{:?}", efi_fs.stats()?);

    Ok(())
}
