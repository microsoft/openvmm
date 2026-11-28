// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use closeable_mutex::CloseableMutex;
use std::sync::Arc;
use std::sync::Weak;

/// Owns the MMIO forwarding path to a dynamically attached device.
pub struct MmioRevoker(Arc<CloseableMutex<MmioForwarder>>);

impl MmioRevoker {
    pub(super) fn new(device: Weak<CloseableMutex<dyn ChipsetDevice>>) -> Self {
        Self(Arc::new(CloseableMutex::new(MmioForwarder(device))))
    }

    pub(super) fn target(&self) -> Weak<CloseableMutex<dyn ChipsetDevice>> {
        Arc::<CloseableMutex<MmioForwarder>>::downgrade(&self.0)
    }

    /// Revoke new MMIO accesses, including accesses from stale range lookups.
    pub fn revoke(self) {
        drop(self.0.close_permanently());
    }
}

struct MmioForwarder(Weak<CloseableMutex<dyn ChipsetDevice>>);

impl ChipsetDevice for MmioForwarder {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }
}

impl MmioIntercept for MmioForwarder {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        (|| {
            Some(
                self.0
                    .upgrade()?
                    .lock_if_open()?
                    .supports_mmio()?
                    .mmio_read(addr, data),
            )
        })()
        .unwrap_or(IoResult::Err(IoError::NoResponse))
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        (|| {
            Some(
                self.0
                    .upgrade()?
                    .lock_if_open()?
                    .supports_mmio()?
                    .mmio_write(addr, data),
            )
        })()
        .unwrap_or(IoResult::Err(IoError::NoResponse))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    struct TestDevice;

    impl ChipsetDevice for TestDevice {
        fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
            Some(self)
        }
    }

    impl MmioIntercept for TestDevice {
        fn mmio_read(&mut self, _: u64, data: &mut [u8]) -> IoResult {
            data.fill(0x5a);
            IoResult::Ok
        }

        fn mmio_write(&mut self, _: u64, _: &[u8]) -> IoResult {
            IoResult::Ok
        }
    }

    #[test]
    fn revocation_blocks_stale_mmio_lookup_before_device_teardown() {
        let device: Arc<CloseableMutex<dyn ChipsetDevice>> =
            Arc::new(CloseableMutex::new(TestDevice));
        let revoker = MmioRevoker::new(Arc::downgrade(&device));
        let lookup = revoker.target().upgrade().unwrap();
        let mut data = [0; 4];
        lookup
            .lock()
            .supports_mmio()
            .unwrap()
            .mmio_read(0, &mut data)
            .unwrap();
        assert_eq!(data, [0x5a; 4]);

        revoker.revoke();
        assert!(lookup.lock_if_open().is_none());
        assert!(device.lock_if_open().is_some());
    }
}
