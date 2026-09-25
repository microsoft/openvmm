// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM virtio-fs device construction and dormant-slot save/restore.

use super::profile::MICROVM_ATTACHMENT_ID;
use super::profile::MICROVM_MOUNT_TAG;
use super::profile::MICROVM_REQUEST_QUEUES;
use super::profile::MicroVmVirtioFsProfile;
use super::saved_state::SavedState;
use super::state::save_dormant_microvm_state;
use super::state::validate_dormant_microvm_state;
use super::state::validate_microvm_state;
use crate::VirtioFs;
use crate::virtio::VirtioFsDevice;
use std::path::Path;
use std::sync::Arc;
use task_control::TaskControl;
use virtio::device::saved_state::DeviceQueueState;
use virtio::device::saved_state::DeviceStateValidator;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::vm_task::VmTaskDriverSource;

struct DormantMicrovmFs;

impl fuse::Fuse for DormantMicrovmFs {}

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
            fs.clone(),
            0,
            notify_corruption,
            MICROVM_REQUEST_QUEUES,
        );
        device.microvm_attachment_id = Some(attachment_id);
        device.microvm_profile = Some(profile);
        device.stateful_fs = Some(fs);
        Ok(device)
    }

    /// Creates the fixed no-DAX microVM virtio-fs device without a host attachment.
    pub fn new_microvm_dormant(
        driver_source: &VmTaskDriverSource,
        stable_id: String,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            stable_id == MICROVM_ATTACHMENT_ID,
            "microVM virtio-fs attachment ID must be '{MICROVM_ATTACHMENT_ID}'"
        );
        let mut device = Self::with_num_request_queues(
            driver_source,
            MICROVM_MOUNT_TAG,
            DormantMicrovmFs,
            0,
            notify_corruption,
            MICROVM_REQUEST_QUEUES,
        );
        device.microvm_attachment_id = Some(stable_id);
        Ok(device)
    }

    /// Creates the fixed no-DAX microVM HostFs device directly from the
    /// `Microvm` resource fields and its process-local host root.
    pub fn new_microvm_hostfs(
        driver_source: &VmTaskDriverSource,
        stable_id: String,
        root_identity: Vec<u8>,
        read_only: bool,
        denied_paths: Vec<String>,
        root_path: impl AsRef<Path>,
        notify_corruption: Option<Arc<dyn Fn() + Sync + Send>>,
    ) -> anyhow::Result<Self> {
        let profile = MicroVmVirtioFsProfile::from_attachment(
            stable_id,
            root_identity,
            read_only,
            denied_paths,
        )?;
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

pub(crate) fn supports_accelerated_doorbells(device: &VirtioFsDevice) -> bool {
    device.microvm_attachment_id.is_none() || device.microvm_profile.is_some()
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

pub(crate) fn reset(device: &mut VirtioFsDevice) {
    device.admission.resume();
    device.save_error = None;
}

pub(crate) fn quiesce_input(device: &mut VirtioFsDevice) {
    if device.microvm_attachment_id.is_some() {
        device.admission.quiesce();
    }
}

pub(crate) fn resume_input(device: &mut VirtioFsDevice) {
    if device.microvm_attachment_id.is_some() {
        device.admission.resume();
        device.save_error = None;
    }
}

pub(crate) fn supports_save_restore(device: &VirtioFsDevice) -> bool {
    device.microvm_attachment_id.is_some()
}

pub(crate) fn save_device(
    device: &mut VirtioFsDevice,
) -> Result<Option<SavedStateBlob>, SaveError> {
    let Some(attachment_id) = device.microvm_attachment_id.as_deref() else {
        return Err(SaveError::NotSupported);
    };
    if let Some(error) = device.save_error.take() {
        return Err(SaveError::Other(error));
    }
    if device.workers.iter().any(TaskControl::has_state) {
        return Err(SaveError::Other(anyhow::anyhow!(
            "virtio-fs queues are still running"
        )));
    }
    device
        .admission
        .verify_drained()
        .map_err(SaveError::Other)?;
    let state = match (&device.microvm_profile, &device.stateful_fs) {
        (Some(profile), Some(fs)) => fs
            .save_microvm_state(profile, device.fs.save_state())
            .map_err(SaveError::Other)?,
        (None, None) => save_dormant_microvm_state(attachment_id, device.fs.save_state())
            .map_err(SaveError::Other)?,
        _ => {
            return Err(SaveError::Other(anyhow::anyhow!(
                "microVM virtio-fs profile and attachment state disagree"
            )));
        }
    };
    Ok(Some(SavedStateBlob::new(state)))
}

pub(crate) fn restore_device(
    device: &mut VirtioFsDevice,
    state: Option<SavedStateBlob>,
) -> Result<(), RestoreError> {
    let attachment_id = device
        .microvm_attachment_id
        .as_deref()
        .ok_or(RestoreError::SavedStateNotSupported)?;
    let state = state.ok_or_else(|| {
        RestoreError::InvalidSavedState(anyhow::anyhow!(
            "microVM virtio-fs is missing device-private saved state"
        ))
    })?;
    let state: SavedState = state.parse().map_err(RestoreError::ProtobufDecode)?;
    if state.dormant {
        let session_state = validate_dormant_microvm_state(&state, attachment_id)
            .map_err(RestoreError::InvalidSavedState)?;
        return device
            .fs
            .restore_state(session_state)
            .map_err(|error| RestoreError::InvalidSavedState(error.into()));
    }
    let profile = device.microvm_profile.as_ref().ok_or_else(|| {
        RestoreError::InvalidSavedState(anyhow::anyhow!(
            "active microVM virtio-fs state requires a host attachment"
        ))
    })?;
    validate_microvm_state(&state, profile).map_err(RestoreError::InvalidSavedState)?;
    let fs = device.stateful_fs.as_ref().ok_or_else(|| {
        RestoreError::InvalidSavedState(anyhow::anyhow!(
            "microVM virtio-fs filesystem attachment is unavailable"
        ))
    })?;
    fs.restore_microvm_state(profile, state, &device.fs)
        .map_err(RestoreError::InvalidSavedState)
}

pub(crate) fn device_state_validator(device: &VirtioFsDevice) -> DeviceStateValidator {
    let attachment_id = device.microvm_attachment_id.clone();
    let profile = device.microvm_profile.clone();
    Box::new(
        move |state, features, queues: &[DeviceQueueState], _guest_memory| {
            let attachment_id = attachment_id
                .as_deref()
                .ok_or(RestoreError::SavedStateNotSupported)?;
            if features.ring_packed() {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "microVM virtio-fs restored packed virtqueue"
                )));
            }
            if queues.len() != 2 {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "microVM virtio-fs requires one hiprio queue and one request queue"
                )));
            }
            let state = state.ok_or_else(|| {
                RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "microVM virtio-fs is missing device-private saved state"
                ))
            })?;
            let state: SavedState = state.parse().map_err(RestoreError::ProtobufDecode)?;
            if state.dormant {
                validate_dormant_microvm_state(&state, attachment_id)
                    .map(|_| ())
                    .map_err(RestoreError::InvalidSavedState)
            } else {
                let profile = profile.as_ref().ok_or_else(|| {
                    RestoreError::InvalidSavedState(anyhow::anyhow!(
                        "active microVM virtio-fs state requires a host attachment"
                    ))
                })?;
                validate_microvm_state(&state, profile).map_err(RestoreError::InvalidSavedState)
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::microvm_root_identity;
    use chipset_device::io::IoResult;
    use chipset_device::mmio::MmioIntercept;
    use guestmem::GuestMemory;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use virtio::VirtioDevice;
    use virtio::transport::VirtioMmioDevice;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::line_interrupt::LineInterrupt;
    use vmcore::save_restore::SaveRestore;
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
            Vec::new(),
            temporary_directory.path(),
            None,
        )
        .unwrap();

        assert_eq!(device.config.num_request_queues, MICROVM_REQUEST_QUEUES);
        assert_eq!(device.traits().max_queues, 2);
        assert_eq!(device.traits().shared_memory.size, 0);
        assert!(!device.traits().device_features.ring_packed());
        assert!(device.supports_save_restore());
        assert!(device.supports_accelerated_doorbells());
        assert_eq!(&device.config.tag[..7], b"microvm");
    }

    #[async_test]
    async fn dormant_microvm_profile_uses_emulated_doorbells(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let device = VirtioFsDevice::new_microvm_dormant(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            None,
        )
        .unwrap();

        assert_eq!(device.traits().max_queues, 2);
        assert!(device.supports_save_restore());
        assert!(!device.supports_accelerated_doorbells());
    }

    #[async_test]
    async fn microvm_dormant_state_restores_into_active_attachment(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let mut source = VirtioFsDevice::new_microvm_dormant(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            None,
        )
        .unwrap();
        source.quiesce_input().await.unwrap();
        let state = source.save_device().unwrap().unwrap();

        let root = tempfile::tempdir().unwrap();
        let mut destination = VirtioFsDevice::new_microvm_hostfs(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(root.path()).unwrap(),
            false,
            Vec::new(),
            root.path(),
            None,
        )
        .unwrap();
        destination.restore_device(Some(state)).unwrap();
    }

    #[async_test]
    async fn microvm_active_state_rejects_dormant_destination(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let root = tempfile::tempdir().unwrap();
        let mut source = VirtioFsDevice::new_microvm_hostfs(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(root.path()).unwrap(),
            false,
            Vec::new(),
            root.path(),
            None,
        )
        .unwrap();
        source.quiesce_input().await.unwrap();
        let state = source.save_device().unwrap().unwrap();

        let mut destination = VirtioFsDevice::new_microvm_dormant(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            None,
        )
        .unwrap();
        assert!(destination.restore_device(Some(state)).is_err());
    }

    #[async_test]
    async fn microvm_device_rejects_a_non_profile_attachment(driver: DefaultDriver) {
        let temporary_directory = tempfile::tempdir().unwrap();
        let profile = MicroVmVirtioFsProfile::from_attachment(
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(temporary_directory.path()).unwrap(),
            true,
            Vec::new(),
        )
        .unwrap();
        let fs = VirtioFs::new(temporary_directory.path(), None).unwrap();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));

        assert!(VirtioFsDevice::new_microvm(&driver_source, profile, fs, None).is_err());
    }

    #[async_test]
    async fn inactive_transport_restore_defers_microvm_hostfs_state(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let source_device = VirtioFsDevice::new_microvm_dormant(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            None,
        )
        .unwrap();
        let mut source = VirtioMmioDevice::new(
            Box::new(source_device),
            &driver,
            GuestMemory::empty(),
            LineInterrupt::detached(),
            None,
            0,
            0x1000,
        )
        .unwrap();
        source.quiesce_input().await.unwrap();
        source.stop().await;
        let saved = source.save().unwrap();
        assert!(
            saved
                .device_state
                .as_ref()
                .unwrap()
                .parse::<SavedState>()
                .unwrap()
                .dormant
        );

        let root = tempfile::tempdir().unwrap();
        let destination_device = VirtioFsDevice::new_microvm_hostfs(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(root.path()).unwrap(),
            false,
            Vec::new(),
            root.path(),
            None,
        )
        .unwrap();
        let mut destination = VirtioMmioDevice::new(
            Box::new(destination_device),
            &driver,
            GuestMemory::empty(),
            LineInterrupt::detached(),
            None,
            0,
            0x1000,
        )
        .unwrap();
        destination.restore(saved).unwrap();
        destination.start_fallible().await.unwrap();
        destination.quiesce_input().await.unwrap();
        destination.stop().await;
        let mut staged = destination.save().unwrap();
        assert!(
            staged
                .device_state
                .as_ref()
                .unwrap()
                .parse::<SavedState>()
                .unwrap()
                .dormant
        );

        destination.start_fallible().await.unwrap();
        let mut config = [0; 4];
        match destination.mmio_read(0x100, &mut config) {
            IoResult::Defer(token) => token.read_future(&mut config).await.unwrap(),
            other => panic!("expected deferred config read, got {other:?}"),
        }
        destination.stop().await;
        let activated = destination.save().unwrap();
        assert!(
            !activated
                .device_state
                .unwrap()
                .parse::<SavedState>()
                .unwrap()
                .dormant
        );

        let mut invalid_private = staged
            .device_state
            .as_ref()
            .unwrap()
            .parse::<SavedState>()
            .unwrap();
        invalid_private.attachment_id = "wrong-attachment".to_owned();
        staged.device_state = Some(SavedStateBlob::new(invalid_private));
        let invalid_device = VirtioFsDevice::new_microvm_dormant(
            &driver_source,
            MICROVM_ATTACHMENT_ID.to_owned(),
            None,
        )
        .unwrap();
        let mut invalid_destination = VirtioMmioDevice::new(
            Box::new(invalid_device),
            &driver,
            GuestMemory::empty(),
            LineInterrupt::detached(),
            None,
            0,
            0x1000,
        )
        .unwrap();
        assert!(invalid_destination.restore(staged).is_err());
    }
}
