// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Inputs for an mshv root partition booted through UEFI.

use crate::Disk;
use crate::PetriInitrd;
use crate::ResolvedArtifact;
use crate::mshv_image::MshvImage;
use std::sync::Arc;

/// Explicit boot media for an x86-64 mshv root partition.
///
/// Pass this recipe to [`crate::UefiGuest::Mshv`]. Petri composes and owns the
/// boot disk when preparing the VM. The caller supplies UEFI firmware and
/// configures nested virtualization, storage, networking, and file shares.
#[derive(Debug)]
pub struct MshvBoot {
    /// Development-only x64 UEFI host boot application executable.
    pub host_boot: ResolvedArtifact,
    /// Linux kernel with mshv EFI-launch support.
    pub kernel: ResolvedArtifact,
    /// Prepared initrd and its entry point.
    pub initrd: PetriInitrd,
    /// Extracted artifact directory containing `boot/`.
    pub boot_artifacts: ResolvedArtifact,
    /// Linux options; Petri appends the initrd entry point and vsock driver selection.
    pub kernel_cmdline: String,
    /// Hypervisor options for HvLoader, without the loader DLL path.
    pub hypervisor_cmdline: String,
}

impl MshvBoot {
    /// Compose a temporary raw disk for the guest's boot drive.
    ///
    /// Copies the prepared initrd unchanged. This neither prepares the guest
    /// userspace nor selects its agent, network, or trust store.
    pub(crate) fn build(&self, use_virtio_vsock: bool) -> anyhow::Result<Disk> {
        let image = tempfile::NamedTempFile::new()?.into_temp_path();
        fs_err::remove_file(&image)?;
        let boot = self.boot_artifacts.get().join("boot");
        MshvImage {
            host_boot: self.host_boot.get(),
            payload: &boot,
            kernel: self.kernel.get(),
            initrd: self.initrd.path.as_ref(),
            kernel_cmdline: &format!(
                "{} {} rdinit={}",
                self.kernel_cmdline,
                crate::vm::linux_vsock_blacklist(use_virtio_vsock),
                self.initrd.rdinit_param
            ),
            hypervisor_cmdline: &self.hypervisor_cmdline,
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
        for name in [
            "HvLoader.efi",
            "lxhvloader.dll",
            "Windows/System32/hvix64.exe",
            "Windows/System32/hvax64.exe",
            "mshv_host_boot.efi",
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
            host_boot: ResolvedArtifact::from_local_path(boot.join("mshv_host_boot.efi")),
            kernel: ResolvedArtifact::from_local_path(kernel),
            initrd,
            boot_artifacts: ResolvedArtifact::from_local_path(temp.path().to_path_buf()),
            kernel_cmdline: "test_option=caller".into(),
            hypervisor_cmdline: "MSHV_SCHEDULER_TYPE=ROOT".into(),
        }
        .build(true)?;
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
            .open_file(mshv_host_boot::KERNEL_CMDLINE_FILE)?
            .read_to_string(&mut config)?;
        assert_eq!(
            config,
            "initrd=\\initramfs.cpio test_option=caller initcall_blacklist=hv_sock_init rdinit=/caller-init\n"
        );
        config.clear();
        fs.root_dir()
            .open_file(mshv_host_boot::HYPERVISOR_CMDLINE_FILE)?
            .read_to_string(&mut config)?;
        assert_eq!(config, "\\lxhvloader.dll MSHV_SCHEDULER_TYPE=ROOT\n");
        fs.unmount()?;
        assert_eq!(fs_err::read(&initrd_path)?, prepared);
        drop(disk);
        assert!(!output.exists());
        Ok(())
    }
}
