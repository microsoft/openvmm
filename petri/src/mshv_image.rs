// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Compose an experimental mshv boot disk from prebuilt artifacts.
//!
//! The caller supplies the development UEFI host boot application and hypervisor payloads. This
//! module neither downloads artifacts nor invokes host image-building tools.

use anyhow::Context;
use guid::Guid;
use mshv_host_boot::HYPERVISOR_CMDLINE_FILE;
use mshv_host_boot::KERNEL_CMDLINE_FILE;
use petri_artifacts_common::tags::MachineArch;
use std::collections::BTreeMap;
use std::io::Seek;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

const SECTOR_SIZE: u64 = 512;
const MIB: u64 = 1024 * 1024;
const ESP_GUID: Guid = guid::guid!("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");

#[derive(Debug)]
struct ImageTimeProvider;

impl fatfs::TimeProvider for ImageTimeProvider {
    fn get_current_date(&self) -> fatfs::Date {
        fatfs::Date {
            year: 1980,
            month: 1,
            day: 1,
        }
    }

    fn get_current_date_time(&self) -> fatfs::DateTime {
        fatfs::DateTime {
            date: self.get_current_date(),
            time: fatfs::Time {
                hour: 0,
                min: 0,
                sec: 0,
                millis: 0,
            },
        }
    }
}

/// Inputs for a disposable boot disk. No installed Linux root filesystem is
/// required; the initramfs must supply `/init`.
pub struct MshvImage<'a> {
    /// Architecture of every executable supplied to the image.
    pub arch: MachineArch,
    /// `mshv_host_boot.efi`, built for the matching UEFI target.
    pub host_boot: &'a Path,
    /// Boot payload root containing `HvLoader.efi`, `lxhvloader.dll`, and
    /// `Windows/System32`. All files under `Windows` are preserved.
    pub payload: &'a Path,
    /// mshv-enabled Linux kernel, booted through its EFI stub.
    pub kernel: &'a Path,
    /// Initramfs containing the runtime.
    pub initrd: &'a Path,
    /// Literal Linux command line.
    pub kernel_cmdline: &'a str,
    /// Literal hypervisor options, passed to HvLoader after the DLL path.
    pub hypervisor_cmdline: &'a str,
}

impl MshvImage<'_> {
    /// Build a raw GPT disk with one FAT32 EFI System Partition.
    ///
    /// Publication is no-clobber and occurs only after successful construction.
    /// Inputs are streamed into a temporary file beside the destination.
    pub fn build(&self, output: &Path) -> anyhow::Result<()> {
        let (host_boot_path, kernel_path, hypervisors): (_, _, &[&str]) = match self.arch {
            MachineArch::X86_64 => (
                "EFI/BOOT/BOOTX64.EFI",
                "bzImage",
                &["windows/system32/hvix64.exe", "windows/system32/hvax64.exe"],
            ),
            MachineArch::Aarch64 => (
                "EFI/BOOT/BOOTAA64.EFI",
                "Image",
                &["windows/system32/hvaa64.exe"],
            ),
        };
        let mut files = BTreeMap::new();
        for (destination, source) in [
            (host_boot_path, self.host_boot.to_path_buf()),
            ("HvLoader.efi", self.payload.join("HvLoader.efi")),
            ("lxhvloader.dll", self.payload.join("lxhvloader.dll")),
            (kernel_path, self.kernel.to_path_buf()),
            ("initramfs.cpio", self.initrd.to_path_buf()),
        ] {
            insert_file(&mut files, destination.to_owned(), source)?;
        }
        collect_files(&mut files, self.payload, Path::new("Windows"))?;
        for required in hypervisors {
            anyhow::ensure!(
                files.keys().any(|path| path.eq_ignore_ascii_case(required)),
                "missing hypervisor payload file {required}"
            );
        }
        let kernel_cmdline =
            mshv_host_boot::command_line(self.kernel_cmdline).map_err(anyhow::Error::msg)?;
        let hypervisor_cmdline =
            mshv_host_boot::command_line(self.hypervisor_cmdline).map_err(anyhow::Error::msg)?;
        let configs = [
            (
                KERNEL_CMDLINE_FILE,
                format!("initrd=\\initramfs.cpio {kernel_cmdline}\n"),
            ),
            (
                HYPERVISOR_CMDLINE_FILE,
                format!("\\lxhvloader.dll {hypervisor_cmdline}\n"),
            ),
        ];
        for (_, content) in &configs {
            mshv_host_boot::command_line(content).map_err(anyhow::Error::msg)?;
        }
        let config_size: u64 = configs.iter().map(|(_, value)| value.len() as u64).sum();
        let payload_size = files.values().try_fold(config_size, |size, path| {
            size.checked_add(fs_err::metadata(path)?.len())
                .context("image size overflow")
        })?;
        // Leave room for FATs, directories, cluster rounding, and GPT headers.
        // Keep small payloads above FAT32's minimum cluster-count threshold.
        let size = payload_size
            .checked_add(payload_size / 8)
            .and_then(|size| size.checked_add(64 * MIB))
            .context("image size overflow")?;
        let sectors = size.div_ceil(SECTOR_SIZE);
        anyhow::ensure!(
            sectors <= u32::MAX as u64,
            "image exceeds the FAT32 sector-count limit"
        );
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut image = tempfile::NamedTempFile::new_in(parent)?;
        image.as_file().set_len(sectors * SECTOR_SIZE)?;

        let mut gpt = gptman::GPT::new_from(&mut image, SECTOR_SIZE, Guid::new_random().into())?;
        gptman::GPT::write_protective_mbr_into(&mut image, SECTOR_SIZE)?;
        gpt[1] = gptman::GPTPartitionEntry {
            partition_type_guid: ESP_GUID.into(),
            unique_partition_guid: Guid::new_random().into(),
            starting_lba: 2048,
            ending_lba: gpt.header.last_usable_lba,
            attribute_bits: 0,
            partition_name: "MSHV".into(),
        };
        gpt.write_into(&mut image)?;
        let mut partition = fscommon::StreamSlice::new(
            &mut image,
            gpt[1].starting_lba * SECTOR_SIZE,
            (gpt[1].ending_lba + 1) * SECTOR_SIZE,
        )?;
        fatfs::format_volume(
            &mut partition,
            fatfs::FormatVolumeOptions::new()
                .fat_type(fatfs::FatType::Fat32)
                .volume_label(*b"MSHV_BOOT  "),
        )?;
        partition.rewind()?;
        // Without chrono, fatfs defaults to invalid month/day zero. Use a valid
        // fixed FAT epoch for all files and directories instead of host time.
        let fs = fatfs::FileSystem::new(
            partition,
            fatfs::FsOptions::new().time_provider(&ImageTimeProvider),
        )?;
        {
            let root = fs.root_dir();
            for (destination, source) in &files {
                let mut dest = create_file(&root, destination)?;
                std::io::copy(&mut fs_err::File::open(source)?, &mut dest)
                    .with_context(|| format!("copying {} to {destination}", source.display()))?;
                dest.flush()?;
            }
            for (path, content) in &configs {
                let mut dest = create_file(&root, path)?;
                dest.write_all(content.as_bytes())?;
                dest.flush()?;
            }
        }
        fs.unmount()?;
        image.as_file().sync_all()?;
        image
            .persist_noclobber(output)
            .with_context(|| format!("publishing {}", output.display()))?;
        Ok(())
    }
}

fn insert_file(
    files: &mut BTreeMap<String, PathBuf>,
    destination: String,
    source: PathBuf,
) -> anyhow::Result<()> {
    let metadata = fs_err::symlink_metadata(&source)?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "expected a regular file, not a symlink or special file: {}",
        source.display()
    );
    anyhow::ensure!(
        metadata.len() <= u32::MAX as u64,
        "file exceeds the FAT32 file-size limit: {}",
        source.display()
    );
    anyhow::ensure!(
        !files
            .keys()
            .any(|path| path.eq_ignore_ascii_case(&destination)),
        "duplicate FAT path: {destination}"
    );
    files.insert(destination, source);
    Ok(())
}

fn collect_files(
    files: &mut BTreeMap<String, PathBuf>,
    root: &Path,
    relative: &Path,
) -> anyhow::Result<()> {
    let path = root.join(relative);
    let metadata = fs_err::symlink_metadata(&path)?;
    anyhow::ensure!(
        metadata.is_dir(),
        "expected payload directory: {}",
        path.display()
    );
    for entry in fs_err::read_dir(&path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().context("non-UTF-8 payload filename")?;
        anyhow::ensure!(
            !name.is_empty()
                && name.is_ascii()
                && !name
                    .chars()
                    .any(|c| c.is_control() || "\"*/:<>?\\|".contains(c))
                && !name.ends_with([' ', '.']),
            "unsupported FAT filename: {name:?}"
        );
        let relative = relative.join(name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(files, root, &relative)?;
        } else {
            let destination = relative
                .components()
                .map(|component| component.as_os_str().to_str().context("non-UTF-8 path"))
                .collect::<anyhow::Result<Vec<_>>>()?
                .join("/");
            insert_file(files, destination, entry.path())?;
        }
    }
    Ok(())
}

fn create_file<'a, T: fatfs::ReadWriteSeek>(
    root: &fatfs::Dir<'a, T>,
    path: &str,
) -> anyhow::Result<fatfs::File<'a, T>> {
    if let Some((parents, _)) = path.rsplit_once('/') {
        let mut directory = root.clone();
        for component in parents.split('/') {
            directory = directory
                .create_dir(component)
                .with_context(|| format!("creating directory for {path}"))?;
        }
    }
    root.create_file(path)
        .with_context(|| format!("creating {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use test_with_tracing::test;

    #[test]
    fn image_contents_and_no_clobber() -> anyhow::Result<()> {
        for arch in [MachineArch::X86_64, MachineArch::Aarch64] {
            check_image_contents(arch)?;
        }
        Ok(())
    }

    fn check_image_contents(arch: MachineArch) -> anyhow::Result<()> {
        let (boot_path, kernel_path, hypervisors): (_, _, &[&str]) = match arch {
            MachineArch::X86_64 => (
                "EFI/BOOT/BOOTX64.EFI",
                "bzImage",
                &["Windows/System32/hvix64.exe", "Windows/System32/hvax64.exe"],
            ),
            MachineArch::Aarch64 => (
                "EFI/BOOT/BOOTAA64.EFI",
                "Image",
                &["Windows/System32/hvaa64.exe"],
            ),
        };
        let dir = tempfile::tempdir()?;
        let payload = dir.path().join("payload");
        fs_err::create_dir_all(payload.join("Windows/System32"))?;
        for name in [
            "HvLoader.efi",
            "lxhvloader.dll",
            "Windows/System32/kdstub.dll",
        ]
        .into_iter()
        .chain(hypervisors.iter().copied())
        {
            fs_err::write(payload.join(name), name)?;
        }
        let artifact = dir.path().join("artifact");
        fs_err::write(&artifact, b"artifact")?;
        let inputs = MshvImage {
            arch,
            host_boot: &artifact,
            payload: &payload,
            kernel: &artifact,
            initrd: &artifact,
            kernel_cmdline: "rdinit=/init console=ttyS0 key=\"two words\" x=$root;halt",
            hypervisor_cmdline: "MSHV_ENABLE=TRUE MSHV_SCHEDULER_TYPE=CORE",
        };
        let output = dir.path().join("boot.img");
        inputs.build(&output)?;
        let mut image = fs_err::File::open(&output)?;
        let gpt = gptman::GPT::read_from(&mut image, SECTOR_SIZE)?;
        assert_eq!(gpt[1].partition_type_guid, <[u8; 16]>::from(ESP_GUID));
        assert_eq!(gpt[1].starting_lba, 2048);
        let partition = fscommon::StreamSlice::new(
            image,
            gpt[1].starting_lba * SECTOR_SIZE,
            (gpt[1].ending_lba + 1) * SECTOR_SIZE,
        )?;
        let fs = fatfs::FileSystem::new(partition, fatfs::FsOptions::new())?;
        assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
        assert_valid_timestamps(&fs.root_dir())?;
        for path in [boot_path, kernel_path, "initramfs.cpio"] {
            let mut bytes = Vec::new();
            fs.root_dir().open_file(path)?.read_to_end(&mut bytes)?;
            assert_eq!(bytes, b"artifact");
        }
        for (path, expected) in [
            (
                KERNEL_CMDLINE_FILE,
                format!("initrd=\\initramfs.cpio {}\n", inputs.kernel_cmdline),
            ),
            (
                HYPERVISOR_CMDLINE_FILE,
                format!("\\lxhvloader.dll {}\n", inputs.hypervisor_cmdline),
            ),
        ] {
            let mut content = String::new();
            fs.root_dir()
                .open_file(path)?
                .read_to_string(&mut content)?;
            assert_eq!(content, expected);
        }
        assert!(fs.root_dir().open_file("EFI/BOOT/grubx64.efi").is_err());
        assert!(fs.root_dir().open_dir("boot/grub2").is_err());
        let mut extra = String::new();
        fs.root_dir()
            .open_file("Windows/System32/kdstub.dll")?
            .read_to_string(&mut extra)?;
        assert_eq!(extra, "Windows/System32/kdstub.dll");
        fs.unmount()?;
        for (kernel_cmdline, hypervisor_cmdline) in [
            ("x\nhalt", inputs.hypervisor_cmdline),
            (inputs.kernel_cmdline, "MSHV_ENABLE=TRUE\0"),
        ] {
            let invalid = MshvImage {
                kernel_cmdline,
                hypervisor_cmdline,
                ..inputs
            };
            assert!(invalid.build(&dir.path().join("invalid.img")).is_err());
            assert!(!dir.path().join("invalid.img").exists());
        }
        fs_err::write(&output, b"do not overwrite")?;
        assert!(inputs.build(&output).is_err());
        assert_eq!(fs_err::read(&output)?, b"do not overwrite");
        fs_err::remove_file(payload.join(hypervisors[0]))?;
        assert!(inputs.build(&dir.path().join("missing.img")).is_err());
        assert!(!dir.path().join("missing.img").exists());
        Ok(())
    }

    fn assert_valid_timestamps<T: fatfs::ReadWriteSeek>(
        directory: &fatfs::Dir<'_, T>,
    ) -> anyhow::Result<()> {
        use fatfs::TimeProvider;

        for entry in directory.iter() {
            let entry = entry?;
            let expected = ImageTimeProvider.get_current_date_time();
            assert_eq!(entry.created(), expected, "{}", entry.file_name());
            assert_eq!(entry.modified(), expected, "{}", entry.file_name());
            assert_eq!(entry.accessed(), expected.date, "{}", entry.file_name());
            if entry.is_dir() && !matches!(entry.file_name().as_str(), "." | "..") {
                assert_valid_timestamps(&entry.to_dir())?;
            }
        }
        Ok(())
    }

    #[test]
    fn rejects_case_collisions_and_oversized_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source");
        let file = fs_err::File::create(&source)?;
        let mut files = BTreeMap::new();
        insert_file(&mut files, "Windows/a.dll".into(), source.clone())?;
        assert!(insert_file(&mut files, "windows/A.DLL".into(), source.clone()).is_err());
        file.set_len(u32::MAX as u64 + 1)?;
        assert!(insert_file(&mut files, "Windows/b.dll".into(), source).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_payload_symlinks() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let windows = dir.path().join("Windows");
        fs_err::create_dir(&windows)?;
        std::os::unix::fs::symlink(dir.path(), windows.join("loop"))?;
        assert!(collect_files(&mut BTreeMap::new(), dir.path(), Path::new("Windows")).is_err());
        Ok(())
    }
}
