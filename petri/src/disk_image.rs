// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use fatfs::FormatVolumeOptions;
use fatfs::FsOptions;
use petri_artifacts_common::artifacts as common_artifacts;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_core::ArtifactResolver;
use petri_artifacts_core::ResolvedArtifact;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::ops::Range;
use std::path::Path;

/// The description and artifacts needed to build a pipette disk image for a VM.
pub struct AgentImage {
    os_flavor: OsFlavor,
    pipette: Option<ResolvedArtifact>,
}

impl AgentImage {
    /// Resolves the artifacts needed to build a disk image for a VM.
    pub fn new(resolver: &ArtifactResolver<'_>, arch: MachineArch, os_flavor: OsFlavor) -> Self {
        let pipette = match (os_flavor, arch) {
            (OsFlavor::Windows, MachineArch::X86_64) => Some(
                resolver
                    .require(common_artifacts::PIPETTE_WINDOWS_X64)
                    .erase(),
            ),
            (OsFlavor::Linux, MachineArch::X86_64) => Some(
                resolver
                    .require(common_artifacts::PIPETTE_LINUX_X64)
                    .erase(),
            ),
            (OsFlavor::Windows, MachineArch::Aarch64) => Some(
                resolver
                    .require(common_artifacts::PIPETTE_WINDOWS_AARCH64)
                    .erase(),
            ),
            (OsFlavor::Linux, MachineArch::Aarch64) => Some(
                resolver
                    .require(common_artifacts::PIPETTE_LINUX_AARCH64)
                    .erase(),
            ),
            (OsFlavor::FreeBsd | OsFlavor::Uefi, _) => None,
        };
        Self { os_flavor, pipette }
    }

    /// Builds a disk image containing pipette and any files needed for the guest VM
    /// to run pipette.
    pub fn build(&self) -> anyhow::Result<tempfile::NamedTempFile> {
        match self.os_flavor {
            OsFlavor::Windows => {
                // Windows doesn't use cloud-init, so we only need pipette
                // (which is configured via the IMC hive).
                build_disk_image(
                    b"pipette    ",
                    &[(
                        "pipette.exe",
                        PathOrBinary::Path(self.pipette.as_ref().unwrap().as_ref()),
                    )],
                )
            }
            OsFlavor::Linux => {
                // Linux uses cloud-init, so we need to include the cloud-init
                // configuration files as well.
                build_disk_image(
                    b"cidata     ", // cloud-init looks for a volume label of "cidata",
                    &[
                        (
                            "pipette",
                            PathOrBinary::Path(self.pipette.as_ref().unwrap().as_ref()),
                        ),
                        (
                            "meta-data",
                            PathOrBinary::Binary(include_bytes!("../guest-bootstrap/meta-data")),
                        ),
                        (
                            "user-data",
                            PathOrBinary::Binary(include_bytes!("../guest-bootstrap/user-data")),
                        ),
                        // Specify a non-present NIC to work around https://github.com/canonical/cloud-init/issues/5511
                        // TODO: support dynamically configuring the network based on vm configuration
                        (
                            "network-config",
                            PathOrBinary::Binary(include_bytes!(
                                "../guest-bootstrap/network-config"
                            )),
                        ),
                    ],
                )
            }
            OsFlavor::FreeBsd | OsFlavor::Uefi => {
                // No pipette binary yet.
                todo!()
            }
        }
    }
}

enum PathOrBinary<'a> {
    Path(&'a Path),
    Binary(&'a [u8]),
}

fn build_disk_image(
    volume_label: &[u8; 11],
    files: &[(&str, PathOrBinary<'_>)],
) -> anyhow::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new()?;
    file.as_file()
        .set_len(64 * 1024 * 1024)
        .context("failed to set file size")?;

    let partition_range =
        build_gpt(&mut file, "CIDATA").context("failed to construct partition table")?;
    build_fat32(
        &mut fscommon::StreamSlice::new(&mut file, partition_range.start, partition_range.end)?,
        volume_label,
        files,
    )
    .context("failed to format volume")?;
    Ok(file)
}

fn build_gpt(file: &mut (impl Read + Write + Seek), name: &str) -> anyhow::Result<Range<u64>> {
    const SECTOR_SIZE: u64 = 512;
    // EBD0A0A2-B9E5-4433-87C0-68B6B72699C7
    const BDP_GUID: [u8; 16] = [
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ];
    const PARTITION_GUID: [u8; 16] = [
        0x55, 0x29, 0x65, 0x69, 0x3A, 0xA7, 0x98, 0x41, 0xBA, 0xBD, 0xB5, 0x50, 0x77, 0x14, 0xA1,
        0xF3,
    ];

    let mut mbr = mbrman::MBR::new_from(file, SECTOR_SIZE as u32, [0xff; 4])?;
    let mut gpt = gptman::GPT::new_from(file, SECTOR_SIZE, [0xff; 16])?;

    // Set up the "Protective" Master Boot Record
    let first_chs = mbrman::CHS::new(0, 0, 2);
    let last_chs = mbrman::CHS::empty(); // This is wrong but doesn't really matter.
    mbr[1] = mbrman::MBRPartitionEntry {
        boot: mbrman::BOOT_INACTIVE,
        first_chs,
        sys: 0xEE, // GPT protective
        last_chs,
        starting_lba: 1,
        sectors: gpt.header.last_usable_lba.try_into().unwrap_or(0xFFFFFFFF),
    };
    mbr.write_into(file)?;

    file.rewind()?;

    // Set up the GPT Partition Table Header
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: BDP_GUID,
        unique_partition_guid: PARTITION_GUID,
        starting_lba: gpt.header.first_usable_lba,
        ending_lba: gpt.header.last_usable_lba,
        attribute_bits: 0,
        partition_name: name.into(),
    };
    gpt.write_into(file)?;

    // calculate the EFI partition's usable range
    let partition_start_byte = gpt[1].starting_lba * SECTOR_SIZE;
    let partition_num_bytes = (gpt[1].ending_lba - gpt[1].starting_lba) * SECTOR_SIZE;
    Ok(partition_start_byte..partition_start_byte + partition_num_bytes)
}

fn build_fat32(
    file: &mut (impl Read + Write + Seek),
    volume_label: &[u8; 11],
    files: &[(&str, PathOrBinary<'_>)],
) -> anyhow::Result<()> {
    fatfs::format_volume(
        &mut *file,
        FormatVolumeOptions::new()
            .volume_label(*volume_label)
            .fat_type(fatfs::FatType::Fat32),
    )
    .context("failed to format volume")?;
    let fs = fatfs::FileSystem::new(file, FsOptions::new()).context("failed to open fs")?;
    for (path, src) in files {
        let mut dest = fs
            .root_dir()
            .create_file(path)
            .context("failed to create file")?;
        match *src {
            PathOrBinary::Path(src_path) => {
                let mut src = fs_err::File::open(src_path)?;
                std::io::copy(&mut src, &mut dest).context("failed to copy file")?;
            }
            PathOrBinary::Binary(src_data) => {
                dest.write_all(src_data).context("failed to write file")?;
            }
        }
        dest.flush().context("failed to flush file")?;
    }
    fs.unmount().context("failed to unmount fs")?;
    Ok(())
}
