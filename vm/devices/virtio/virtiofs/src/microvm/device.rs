// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM virtio-fs device construction.

use super::profile::MICROVM_MOUNT_TAG;
use super::profile::MICROVM_REQUEST_QUEUES;
use super::profile::MicroVmVirtioFsProfile;
use crate::VirtioFs;
use crate::virtio::VirtioFsDevice;
use std::path::Path;
use std::sync::Arc;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::vm_task::VmTaskDriverSource;

impl VirtioFsDevice {
    /// Creates the fixed no-DAX microVM virtio-fs device.
    ///
    /// The filesystem must have been constructed with
    /// [`VirtioFs::new_microvm`], using this exact profile. This keeps the
    /// device shape and host-enforced filesystem policy inseparable.
    pub fn new_microvm(
        driver_source: &VmTaskDriverSource,
        profile: MicroVmVirtioFsProfile,
        fs: VirtioFs,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            fs.microvm_profile() == Some(&profile),
            "microVM device profile does not match the HostFs attachment"
        );
        let attachment_id = profile.attachment_id().to_owned();
        let mut device = Self::with_num_request_queues(
            driver_source,
            MICROVM_MOUNT_TAG,
            fs,
            0,
            notify_corruption,
            MICROVM_REQUEST_QUEUES,
        );
        device.microvm_attachment_id = Some(attachment_id);
        Ok(device)
    }

    /// Creates the fixed no-DAX microVM HostFs device directly from the
    /// `Microvm` resource fields and its process-local host root.
    pub fn new_microvm_hostfs(
        driver_source: &VmTaskDriverSource,
        stable_id: String,
        root_identity: Vec<u8>,
        read_only: bool,
        root_path: impl AsRef<Path>,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
    ) -> anyhow::Result<Self> {
        let profile = MicroVmVirtioFsProfile::from_attachment(stable_id, root_identity, read_only)?;
        let fs = VirtioFs::new_microvm(root_path, profile.clone())?;
        Self::new_microvm(driver_source, profile, fs, notify_corruption)
    }
}

pub(crate) fn device_features(device: &VirtioFsDevice) -> VirtioDeviceFeatures {
    VirtioDeviceFeatures::new()
        .with_ring_event_idx(true)
        .with_ring_indirect_desc(true)
        .with_ring_packed(device.microvm_attachment_id.is_none())
}

pub(crate) fn validate_shared_memory(device: &VirtioFsDevice) -> anyhow::Result<()> {
    anyhow::ensure!(
        device.microvm_attachment_id.is_none(),
        "the microVM virtio-fs profile does not expose shared memory"
    );
    Ok(())
}

pub(crate) fn validate_queue_features(
    device: &VirtioFsDevice,
    features: &VirtioDeviceFeatures,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        device.microvm_attachment_id.is_none() || !features.ring_packed(),
        "the microVM virtio-fs profile forbids packed virtqueues"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::MICROVM_ATTACHMENT_ID;
    use crate::profile::microvm_root_identity;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use virtio::VirtioDevice;
    use vmcore::vm_task::SingleDriverBackend;

    #[async_test]
    async fn microvm_profile_has_fixed_device_shape(driver: DefaultDriver) {
        let temporary_directory = tempfile::tempdir().unwrap();
        let root_identity = microvm_root_identity(temporary_directory.path()).unwrap();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let device = VirtioFsDevice::new_microvm_hostfs(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            root_identity,
            true,
            temporary_directory.path(),
            None,
        )
        .unwrap();

        assert_eq!(device.config.num_request_queues, MICROVM_REQUEST_QUEUES);
        assert_eq!(device.traits().max_queues, 2);
        assert_eq!(device.traits().shared_memory.size, 0);
        assert!(!device.traits().device_features.ring_packed());
        assert_eq!(&device.config.tag[..7], b"microvm");
    }

    #[async_test]
    async fn microvm_device_rejects_a_non_profile_attachment(driver: DefaultDriver) {
        let temporary_directory = tempfile::tempdir().unwrap();
        let profile = MicroVmVirtioFsProfile::from_attachment(
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(temporary_directory.path()).unwrap(),
            true,
        )
        .unwrap();
        let fs = VirtioFs::new(temporary_directory.path(), None).unwrap();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));

        assert!(VirtioFsDevice::new_microvm(&driver_source, profile, fs, None).is_err());
    }
}
