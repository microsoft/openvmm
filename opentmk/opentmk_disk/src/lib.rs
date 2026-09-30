// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build a bootable OpenTMK UEFI disk image for the flowey `build-opentmk` path.
//! [`build_opentmk_vhd`] builds as-is; [`build_opentmk_vhd_with_config`] patches [`opentmk_protocol::TestConfig`].

use anyhow::Context;
use guid::Guid;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::ops::Range;

const SECTOR_SIZE: u64 = 512;
const IMAGE_SIZE: u64 = 64 * 1024 * 1024;

/// EFI System Partition type GUID.
const EFI_SYSTEM_PARTITION_GUID: Guid = guid::guid!("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");

/// Target architecture for the boot image, selecting the removable-media path.
#[derive(Debug, Clone, Copy)]
pub enum Arch {
    /// x86_64 (`EFI/BOOT/BOOTX64.EFI`).
    X86_64,
    /// aarch64 (`EFI/BOOT/BOOTAA64.EFI`).
    Aarch64,
}

/// Build a fixed VHD that boots the given OpenTMK `.efi` as-is.
pub fn build_opentmk_vhd(efi: &[u8], arch: Arch) -> anyhow::Result<tempfile::NamedTempFile> {
    build_image(efi, arch)
}

/// Build a fixed VHD that boots a copy of `efi` with `config_json` patched into
/// its embedded config region, selecting which test to run.
pub fn build_opentmk_vhd_with_config(
    efi: &[u8],
    arch: Arch,
    config_json: &[u8],
) -> anyhow::Result<tempfile::NamedTempFile> {
    let mut efi = efi.to_vec();
    opentmk_protocol::patch_opentmk_config(&mut efi, config_json)
        .context("failed to patch opentmk test config")?;
    build_image(&efi, arch)
}

fn build_image(efi: &[u8], arch: Arch) -> anyhow::Result<tempfile::NamedTempFile> {
    let boot_path = match arch {
        Arch::X86_64 => "EFI/BOOT/BOOTX64.EFI",
        Arch::Aarch64 => "EFI/BOOT/BOOTAA64.EFI",
    };
    let mut image_file = tempfile::Builder::new().suffix(".vhd").tempfile()?;
    image_file
        .as_file()
        .set_len(IMAGE_SIZE)
        .context("failed to set file size")?;

    build_fat32_disk_image(&mut image_file, "ESP", b"ESP        ", &[(boot_path, efi)])?;

    disk_vhd1::Vhd1Disk::make_fixed(image_file.as_file())
        .context("failed to make vhd for uefi boot image")?;
    Ok(image_file)
}

fn build_fat32_disk_image(
    file: &mut (impl Read + Write + Seek),
    gpt_name: &str,
    volume_label: &[u8; 11],
    files: &[(&str, &[u8])],
) -> anyhow::Result<()> {
    let range = build_gpt(file, gpt_name).context("failed to construct partition table")?;
    build_fat32(
        &mut fscommon::StreamSlice::new(file, range.start, range.end)?,
        volume_label,
        files,
    )
    .context("failed to format volume")
}

fn build_gpt(file: &mut (impl Read + Write + Seek), name: &str) -> anyhow::Result<Range<u64>> {
    let mut gpt = gptman::GPT::new_from(file, SECTOR_SIZE, Guid::new_random().into())?;
    gptman::GPT::write_protective_mbr_into(file, SECTOR_SIZE)?;
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: EFI_SYSTEM_PARTITION_GUID.into(),
        unique_partition_guid: Guid::new_random().into(),
        starting_lba: gpt.header.first_usable_lba,
        ending_lba: gpt.header.last_usable_lba,
        attribute_bits: 0,
        partition_name: name.into(),
    };
    gpt.write_into(file)?;
    let start = gpt[1].starting_lba * SECTOR_SIZE;
    // GPT LBAs are inclusive, so the sector count spans `end - start + 1`.
    let bytes = (gpt[1].ending_lba - gpt[1].starting_lba + 1) * SECTOR_SIZE;
    Ok(start..start + bytes)
}

fn build_fat32(
    file: &mut (impl Read + Write + Seek),
    volume_label: &[u8; 11],
    files: &[(&str, &[u8])],
) -> anyhow::Result<()> {
    fatfs::format_volume(
        &mut *file,
        fatfs::FormatVolumeOptions::new()
            .volume_label(*volume_label)
            .fat_type(fatfs::FatType::Fat32),
    )
    .context("failed to format volume")?;
    let fs = fatfs::FileSystem::new(file, fatfs::FsOptions::new()).context("failed to open fs")?;
    for (path, data) in files {
        // Create parent directories (e.g. `EFI/BOOT`), since fatfs does not.
        let mut dir = fs.root_dir();
        let mut components = path.split('/').peekable();
        let mut file_name = *path;
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                file_name = component;
                break;
            }
            if component.is_empty() {
                continue;
            }
            dir = match dir.open_dir(component) {
                Ok(existing) => existing,
                Err(_) => dir
                    .create_dir(component)
                    .context("failed to create directory")?,
            };
        }
        let mut dest = dir
            .create_file(file_name)
            .context("failed to create file")?;
        dest.write_all(data).context("failed to write file")?;
        dest.flush().context("failed to flush file")?;
    }
    fs.unmount().context("failed to unmount fs")
}
