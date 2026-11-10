// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The internal protocol for communications between the proxy and the device wrapper.

use chipset_device::io::IoError;
use chipset_device::pci::PciConfigByteEnable;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::rpc::Rpc;
use vmcore::save_restore::SavedStateBlob;

/// Capabilities of the remote chipset device requested at initialization.
#[derive(MeshPayload)]
pub(crate) struct DeviceInit {
    /// MMIO configuration, if MMIO is supported.
    pub mmio: Option<MmioInit>,
    /// PIO configuration, if PIO is supported.
    pub pio: Option<PioInit>,
    /// PCI configuration, if PCI is supported.
    pub pci: Option<PciInit>,
}

/// MMIO capabilities of the remote chipset device requested at initialization.
#[derive(MeshPayload)]
pub(crate) struct MmioInit {
    /// The static MMIO regions requested by the device.
    /// Each entry is a tuple of (name, base port, end port).
    pub static_regions: Vec<(String, u64, u64)>,
    /// Whether the device supports memory-like MMIO atomic operations.
    pub compare_exchange: bool,
}

/// PIO capabilities of the remote chipset device requested at initialization.
#[derive(MeshPayload)]
pub(crate) struct PioInit {
    /// The static PIO ports requested by the device.
    /// Each entry is a tuple of (name, base port, end port).
    pub static_regions: Vec<(String, u16, u16)>,
}

/// PCI capabilities of the remote chipset device requested at initialization.
#[derive(MeshPayload)]
pub(crate) struct PciInit {
    /// The suggested BDF (Bus, Device, Function) for the device.
    pub suggested_bdf: Option<(u8, u8, u8)>,
}

/// Requests sent to the remote device.
#[derive(MeshPayload)]
pub(crate) enum DeviceRequest {
    /// Perform a MMIO read operation.
    MmioRead(ReadRequest<u64>),
    /// Perform a MMIO write operation.
    MmioWrite(WriteRequest<u64, Vec<u8>>),
    /// Perform a PIO read operation.
    PioRead(ReadRequest<u16>),
    /// Perform a PIO write operation.
    PioWrite(WriteRequest<u16, Vec<u8>>),
    /// Perform a PCI config space read.
    PciConfigRead(ReadRequest<u16>, PciConfigByteEnable),
    /// Perform a PCI config space write.
    PciConfigWrite(WriteRequest<u16, u32>, PciConfigByteEnable),
    /// Start the device
    Start,
    /// Stop the device
    Stop(Rpc<(), ()>),
    /// Reset the device
    Reset(Rpc<(), ()>),
    /// Release resources of a stopped device and exit the worker.
    Teardown(Rpc<(), ()>),
    /// Save the device state
    Save(Rpc<(), Result<SavedStateBlob, RemoteError>>),
    /// Restore the device state
    Restore(Rpc<SavedStateBlob, Result<(), RemoteError>>),
    /// Attempt a memory-like MMIO compare-and-exchange.
    MmioCompareExchange(Rpc<(u64, Vec<u8>, Vec<u8>), Result<Option<bool>, IoError>>),
}

/// Responses sent from the remote device.
#[derive(MeshPayload)]
pub(crate) enum DeviceResponse {
    /// Response to a read operation.
    Read {
        /// ID number for the request
        id: usize,
        /// Data read from the device or an error.
        result: Result<Vec<u8>, IoError>,
    },
    /// Response to a write operation.
    Write {
        /// ID number for the request
        id: usize,
        /// Result of the write operation.
        result: Result<(), IoError>,
    },
}

/// Requests sent to the remote device for a read.
#[derive(MeshPayload)]
pub(crate) struct ReadRequest<T> {
    /// ID number for the request
    pub id: usize,
    /// Address to read from.
    pub address: T,
    /// Size of the read (1, 2, 4, or 8 bytes).
    pub size: usize,
}

/// Requests sent to the remote device for a write.
#[derive(MeshPayload)]
pub(crate) struct WriteRequest<T, V> {
    /// ID number for the request
    pub id: usize,
    /// Address to write to.
    pub address: T,
    /// Data to write.
    pub data: V,
}

#[cfg(test)]
mod tests {
    use super::DeviceRequest;
    use super::MmioInit;
    use mesh::MeshPayload;
    use mesh::rpc::RpcSend;
    use pal_async::async_test;
    use test_with_tracing::test;

    #[derive(MeshPayload)]
    struct LegacyMmioInit {
        static_regions: Vec<(String, u64, u64)>,
    }

    #[test]
    fn legacy_mmio_capabilities_do_not_enable_compare_exchange() {
        let old = LegacyMmioInit {
            static_regions: vec![("test".into(), 0x1000, 0x1fff)],
        };
        let decoded: MmioInit =
            mesh::payload::SerializedMessage::<mesh::resource::Resource>::from_message(old)
                .into_message()
                .unwrap();
        assert!(!decoded.compare_exchange);
        assert_eq!(
            decoded.static_regions,
            vec![("test".into(), 0x1000, 0x1fff)]
        );
    }

    #[test]
    fn new_mmio_capabilities_remain_readable_by_legacy_receivers() {
        let current = MmioInit {
            static_regions: vec![("test".into(), 0x1000, 0x1fff)],
            compare_exchange: true,
        };
        let decoded: LegacyMmioInit =
            mesh::payload::SerializedMessage::<mesh::resource::Resource>::from_message(current)
                .into_message()
                .unwrap();
        assert_eq!(
            decoded.static_regions,
            vec![("test".into(), 0x1000, 0x1fff)]
        );
    }

    #[async_test]
    async fn compare_exchange_rpc_survives_serialization() {
        let (send, mut recv) = mesh::channel();
        let pending = send.call(
            DeviceRequest::MmioCompareExchange,
            (0x1234, vec![1, 2], vec![3, 4]),
        );
        let message = recv.recv().await.unwrap();
        let decoded: DeviceRequest =
            mesh::payload::SerializedMessage::<mesh::resource::Resource>::from_message(message)
                .into_message()
                .unwrap();
        let DeviceRequest::MmioCompareExchange(rpc) = decoded else {
            panic!("expected compare-and-exchange request");
        };
        let ((address, current, new), rpc) = rpc.split();
        assert_eq!((address, current, new), (0x1234, vec![1, 2], vec![3, 4]));
        rpc.complete(Ok(Some(false)));
        assert_eq!(pending.await.unwrap().unwrap(), Some(false));
    }
}
