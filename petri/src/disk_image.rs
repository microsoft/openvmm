// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tools for building a disk image for a VM.

use anyhow::Context;
use fatfs::FormatVolumeOptions;
use fatfs::FsOptions;
use guid::Guid;
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
#[derive(Debug)]
pub struct AgentImage {
    os_flavor: OsFlavor,
    pipette: Option<ResolvedArtifact>,
    extras: Vec<(String, ResolvedArtifact)>,
}

/// Disk image type
pub enum ImageType {
    /// Raw image
    Raw,
    /// Fixed VHD1
    Vhd,
}

impl AgentImage {
    /// Resolves the artifacts needed to build a disk image for a VM.
    pub fn new(os_flavor: OsFlavor) -> Self {
        Self {
            os_flavor,
            pipette: None,
            extras: Vec::new(),
        }
    }

    /// Adds the appropriate pipette binary to the image
    pub fn with_pipette(mut self, resolver: &ArtifactResolver<'_>, arch: MachineArch) -> Self {
        self.pipette = match (self.os_flavor, arch) {
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
            (OsFlavor::FreeBsd | OsFlavor::Uefi, _) => {
                todo!("No pipette binary yet for os");
            }
        };
        self
    }

    /// Check if the image contains pipette
    pub fn contains_pipette(&self) -> bool {
        self.pipette.is_some()
    }

    /// Adds an extra file to the disk image.
    pub fn add_file(&mut self, name: &str, artifact: ResolvedArtifact) {
        self.extras.push((name.to_string(), artifact));
    }

    /// Builds a disk image containing pipette and any files needed for the guest VM
    /// to run pipette.
    pub fn build(&self, image_type: ImageType) -> anyhow::Result<Option<tempfile::NamedTempFile>> {
        let mut files = self
            .extras
            .iter()
            .map(|(name, artifact)| (name.as_str(), PathOrBinary::Path(artifact.as_ref())))
            .collect::<Vec<_>>();
        let volume_label = match self.os_flavor {
            OsFlavor::Windows => {
                // Windows doesn't use cloud-init, so we only need pipette
                // (which is configured via the IMC hive).
                if let Some(pipette) = self.pipette.as_ref() {
                    files.push(("pipette.exe", PathOrBinary::Path(pipette.as_ref())));
                }
                b"pipette    "
            }
            OsFlavor::Linux => {
                if let Some(pipette) = self.pipette.as_ref() {
                    files.push(("pipette", PathOrBinary::Path(pipette.as_ref())));
                }
                // Linux uses cloud-init, so we need to include the cloud-init
                // configuration files as well.
                files.extend([
                    (
                        "meta-data",
                        PathOrBinary::Binary(include_bytes!("../guest-bootstrap/meta-data")),
                    ),
                    (
                        "user-data",
                        if self.pipette.is_some() {
                            PathOrBinary::Binary(include_bytes!("../guest-bootstrap/user-data"))
                        } else {
                            PathOrBinary::Binary(include_bytes!(
                                "../guest-bootstrap/user-data-no-agent"
                            ))
                        },
                    ),
                    // Specify a non-present NIC to work around https://github.com/canonical/cloud-init/issues/5511
                    // TODO: support dynamically configuring the network based on vm configuration
                    (
                        "network-config",
                        PathOrBinary::Binary(include_bytes!("../guest-bootstrap/network-config")),
                    ),
                ]);
                b"cidata     " // cloud-init looks for a volume label of "cidata",
            }
            // Nothing OS-specific yet for other flavors
            _ => b"cidata     ",
        };

        if files.is_empty() {
            Ok(None)
        } else {
            let mut image_file = match image_type {
                ImageType::Raw => tempfile::NamedTempFile::new()?,
                ImageType::Vhd => tempfile::Builder::new().suffix(".vhd").tempfile()?,
            };

            image_file
                .as_file()
                .set_len(64 * 1024 * 1024)
                .context("failed to set file size")?;

            build_fat32_disk_image(&mut image_file, "CIDATA", volume_label, &files)?;

            if matches!(image_type, ImageType::Vhd) {
                disk_vhd1::Vhd1Disk::make_fixed(image_file.as_file())
                    .context("failed to make vhd for agent image")?;
            }

            Ok(Some(image_file))
        }
    }
}

pub(crate) const SECTOR_SIZE: u64 = 512;

pub(crate) enum PathOrBinary<'a> {
    Path(&'a Path),
    Binary(&'a [u8]),
}

pub(crate) fn build_fat32_disk_image(
    file: &mut (impl Read + Write + Seek),
    gpt_name: &str,
    volume_label: &[u8; 11],
    files: &[(&str, PathOrBinary<'_>)],
) -> anyhow::Result<()> {
    let partition_range =
        build_gpt(file, gpt_name).context("failed to construct partition table")?;
    build_fat32(
        &mut fscommon::StreamSlice::new(file, partition_range.start, partition_range.end)?,
        volume_label,
        files,
    )
    .context("failed to format volume")?;
    Ok(())
}

fn build_gpt(file: &mut (impl Read + Write + Seek), name: &str) -> anyhow::Result<Range<u64>> {
    let mut gpt = gptman::GPT::new_from(file, SECTOR_SIZE, Guid::new_random().into())?;

    // Set up the "Protective" Master Boot Record
    gptman::GPT::write_protective_mbr_into(file, SECTOR_SIZE)?;

    // Set up the GPT Partition Table Header
    gpt[1] = gptman::GPTPartitionEntry {
        // Basic data partition guid
        partition_type_guid: guid::guid!("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7").into(),
        unique_partition_guid: Guid::new_random().into(),
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
