// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Prepare an mshv boot disk independently of the VM that will launch it.

use crate::Disk;
use crate::PetriInitrd;
use crate::mshv_image::MshvImage;
use std::path::Path;
use std::sync::Arc;

/// Explicit boot media for an x86-64 mshv root partition.
///
/// This prepares guest media only. The caller supplies UEFI firmware and
/// configures nested virtualization, storage, networking, and file shares.
pub struct MshvBoot<'a> {
    /// Linux kernel with mshv EFI-launch support.
    pub kernel: &'a Path,
    /// Prepared initrd and its entry point.
    pub initrd: &'a PetriInitrd,
    /// Extracted artifact directory containing `boot/`.
    pub boot_artifacts: &'a Path,
}

impl MshvBoot<'_> {
    /// Compose a temporary raw disk for [`crate::UefiGuest::Custom`].
    ///
    /// Copies the prepared initrd unchanged. This neither prepares the guest
    /// userspace nor selects its agent, network, or trust store.
    pub fn build(&self) -> anyhow::Result<Disk> {
        let image = tempfile::NamedTempFile::new()?.into_temp_path();
        fs_err::remove_file(&image)?;
        let boot = self.boot_artifacts.join("boot");
        MshvImage {
            shim: &boot.join("EFI/BOOT/bootx64.efi"),
            grub: &boot.join("EFI/BOOT/grubx64.efi"),
            payload: &boot,
            kernel: self.kernel,
            initrd: self.initrd.path.as_ref(),
            kernel_cmdline: &format!("console=ttyS0 rdinit={} panic=-1", self.initrd.rdinit_param),
        }
        .build(&image)?;
        Ok(Disk::Temporary(Arc::new(image)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use test_with_tracing::test;

    #[test]
    fn explicit_inputs_build_guest_media_without_a_vm() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let boot = temp.path().join("boot");
        fs_err::create_dir_all(boot.join("Windows/System32"))?;
        fs_err::create_dir_all(boot.join("EFI/BOOT"))?;
        for name in [
            "HvLoader.efi",
            "lxhvloader.dll",
            "Windows/System32/hvix64.exe",
            "Windows/System32/hvax64.exe",
            "EFI/BOOT/bootx64.efi",
            "EFI/BOOT/grubx64.efi",
        ] {
            fs_err::write(boot.join(name), name)?;
        }
        let kernel = temp.path().join("root-kernel");
        let initrd_path = temp.path().join("prepared-initrd");
        fs_err::write(&kernel, b"explicit root kernel")?;
        let prepared = b"prepared initrd contents";
        fs_err::write(&initrd_path, prepared)?;
        let initrd = PetriInitrd {
            path: crate::TempOrPersistentPath::Persistent(initrd_path.clone()),
            rdinit_param: "/caller-init".into(),
        };
        let disk = MshvBoot {
            kernel: &kernel,
            initrd: &initrd,
            boot_artifacts: temp.path(),
        }
        .build()?;
        let Disk::Temporary(path) = &disk else {
            panic!("expected owned temporary disk")
        };
        let output = path.to_path_buf();
        let mut image = fs_err::File::open(&output)?;
        let gpt = gptman::GPT::read_from(&mut image, 512)?;
        let partition = fscommon::StreamSlice::new(
            image,
            gpt[1].starting_lba * 512,
            (gpt[1].ending_lba + 1) * 512,
        )?;
        let fs = fatfs::FileSystem::new(partition, fatfs::FsOptions::new())?;
        let mut bytes = Vec::new();
        fs.root_dir()
            .open_file("bzImage")?
            .read_to_end(&mut bytes)?;
        assert_eq!(bytes, b"explicit root kernel");
        bytes.clear();
        fs.root_dir()
            .open_file("initramfs.cpio")?
            .read_to_end(&mut bytes)?;
        assert_eq!(bytes, prepared);
        let mut config = String::new();
        fs.root_dir()
            .open_file("boot/grub2/grub.cfg")?
            .read_to_string(&mut config)?;
        assert!(config.contains("'rdinit=/caller-init'"));
        fs.unmount()?;
        assert_eq!(fs_err::read(&initrd_path)?, prepared);
        drop(disk);
        assert!(!output.exists());
        Ok(())
    }
}
