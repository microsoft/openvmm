// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::NvmeDriver;
use crate::RequestError;
use crate::queue_pair::AdminAerHandler;
use crate::queue_pair::AerHandler;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use disk_backend::Disk;
use disk_prwrap::DiskWithReservations;
use futures::StreamExt;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::CancelContext;
use mesh::CellUpdater;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use nvme::NvmeControllerCaps;
use nvme_resources::fault::AdminQueueFaultBehavior;
use nvme_resources::fault::AdminQueueFaultConfig;
use nvme_resources::fault::FaultConfiguration;
use nvme_resources::fault::IoQueueFaultBehavior;
use nvme_resources::fault::IoQueueFaultConfig;
use nvme_spec::AdminOpcode;
use nvme_spec::AsynchronousEventRequestDw0;
use nvme_spec::Cap;
use nvme_spec::Command;
use nvme_spec::nvm;
use nvme_spec::nvm::DsmRange;
use nvme_test::command_match::CommandMatchBuilder;
use pal_async::DefaultDriver;
use pal_async::async_test;
use parking_lot::Mutex;
use pci_core::msi::MsiInterruptSet;
use scsi_buffers::OwnedRequestBuffers;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use test_with_tracing::test;
use user_driver::DeviceBacking;
use user_driver::DeviceRegisterIo;
use user_driver::DmaClient;
use user_driver::interrupt::DeviceInterrupt;
use user_driver_emulated_mock::DeviceTestDmaClientCallbacks;
use user_driver_emulated_mock::DeviceTestMemory;
use user_driver_emulated_mock::EmulatedDevice;
use user_driver_emulated_mock::Mapping;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// When given a failed AER completion this test ensures that the AER handler
/// responds to the RPC with the appropriate error and stops issuing further AERs.
#[async_test]
async fn test_admin_aer_handler_failed_completion(_driver: DefaultDriver) {
    // ARRANGE
    enum TestReq {
        Aen(Rpc<(), Result<AsynchronousEventRequestDw0, RequestError>>),
    }

    let cid = 0;
    let failure_status = nvme_spec::Status::INVALID_COMMAND_OPCODE.0;
    let failed_completion = nvme_spec::Completion {
        dw0: 0,
        dw1: 0,
        sqhd: 0,
        sqid: 0,
        cid,
        status: nvme_spec::CompletionStatus::new().with_status(failure_status),
    };

    // Create both sides of the RPC channel and other admin AER handler setup.
    let (send, mut recv) = mesh::channel::<TestReq>();
    let pending_aen = send.call(TestReq::Aen, ());
    let send_aen = recv.next().await.expect("aen request received");
    let TestReq::Aen(rpc) = send_aen;
    let mut handler = AdminAerHandler::new();

    // Handler was just created, so this should never be false.
    assert!(handler.poll_send_aer());
    handler.handle_aen_request(rpc);
    handler.update_awaiting_cid(cid);

    // ACT: Try to handle a failed completion.
    handler.handle_completion(&failed_completion);

    // ASSERT: The AEN response should be sent and should indicate failure.
    let response = CancelContext::new()
        .with_timeout(Duration::from_secs(2))
        .until_cancelled(pending_aen) // Avoid hanging test
        .await
        .expect("got response before timeout")
        .expect("aen rpc completed");
    match response {
        Err(RequestError::Nvme(err)) => {
            assert_eq!(err.status(), nvme_spec::Status(failure_status));
        }
        other => panic!("unexpected aen response: {other:?}"),
    }
    assert!(
        !handler.poll_send_aer(),
        "handler should stop issuing AERs after a failed completion"
    );
}

#[async_test]
#[should_panic(expected = "assertion `left == right` failed: cid sequence number mismatch:")]
async fn test_nvme_admin_fault_bad_cid(driver: DefaultDriver) {
    let mut output_cmd = Command::new_zeroed();
    output_cmd.cdw0.set_cid(1); // AER will have cid 0.

    test_nvme_fault_injection(
        driver,
        FaultConfiguration::new(CellUpdater::new(true).cell()).with_admin_queue_fault(
            AdminQueueFaultConfig::new().with_submission_queue_fault(
                CommandMatchBuilder::new()
                    .match_cdw0_opcode(AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0)
                    .build(),
                AdminQueueFaultBehavior::Update(output_cmd),
            ),
        ),
    )
    .await;
}

#[async_test]
async fn test_nvme_io_fault_long_reservation_report(driver: DefaultDriver) {
    let report_header = nvm::ReservationReportExtended {
        report: nvm::ReservationReport {
            generation: 0,
            rtype: nvm::ReservationType(0),
            regctl: (128_u16).into(), // Indicates at-least 2 pages worth of data
            ptpls: 0,
            ..FromZeros::new_zeroed()
        },
        ..FromZeros::new_zeroed()
    };

    test_nvme_fault_injection(
        driver,
        FaultConfiguration::new(CellUpdater::new(true).cell()).with_io_queue_fault(
            IoQueueFaultConfig::new(CellUpdater::new(true).cell()).with_completion_queue_fault(
                CommandMatchBuilder::new()
                    .match_cdw0_opcode(nvm::NvmOpcode::RESERVATION_REPORT.0)
                    .build(),
                IoQueueFaultBehavior::CustomPayload(report_header.as_bytes().to_vec()),
            ),
        ),
    )
    .await;
}

#[async_test]
async fn test_nvme_driver_direct_dma(driver: DefaultDriver) {
    test_nvme_driver(
        driver,
        NvmeTestConfig {
            allow_dma: true,
            fail_at_driver_create: false,
            fail_at_io_issuer: false,
        },
    )
    .await;
}

#[async_test]
async fn test_nvme_driver_bounce_buffer(driver: DefaultDriver) {
    test_nvme_driver(
        driver,
        NvmeTestConfig {
            allow_dma: false,
            fail_at_driver_create: false,
            fail_at_io_issuer: false,
        },
    )
    .await;
}

#[async_test]
async fn test_nvme_driver_fails_to_create_dma_alloc_failure(driver: DefaultDriver) {
    test_nvme_driver(
        driver,
        NvmeTestConfig {
            allow_dma: true,
            fail_at_driver_create: true,
            fail_at_io_issuer: false,
        },
    )
    .await;
}

#[async_test]
async fn test_nvme_driver_fallback_due_to_dma_alloc_failures(driver: DefaultDriver) {
    test_nvme_driver(
        driver,
        NvmeTestConfig {
            allow_dma: true,
            fail_at_driver_create: false,
            fail_at_io_issuer: true,
        },
    )
    .await;
}

#[async_test]
async fn test_nvme_ioqueue_max_mqes(driver: DefaultDriver) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 64;

    // Memory setup
    let pages = 1000;
    let device_test_memory = DeviceTestMemory::new(pages, false, "test_nvme_ioqueue_max_mqes");
    let guest_mem = device_test_memory.guest_memory();
    let dma_client = device_test_memory.dma_client();

    // Controller Driver Setup
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut msi_set = MsiInterruptSet::new();
    let nvme = nvme::NvmeController::new(
        &driver_source,
        guest_mem,
        &mut msi_set,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
    );

    let mut device = NvmeTestEmulatedDevice::new(nvme, msi_set, dma_client.clone());

    // Mock response at offset 0 since that is where Cap will be accessed
    let max_u16: u16 = 65535;
    let cap: Cap = Cap::new().with_mqes_z(max_u16);
    device.set_mock_response_u64(Some((0, cap.into())));

    let driver = NvmeDriver::new(&driver_source, CPU_COUNT, device, false).await;
    assert!(driver.is_ok());
}

#[async_test]
async fn test_nvme_ioqueue_invalid_mqes(driver: DefaultDriver) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 64;

    // Memory setup
    let pages = 1000;
    let device_test_memory = DeviceTestMemory::new(pages, false, "test_nvme_ioqueue_invalid_mqes");
    let guest_mem = device_test_memory.guest_memory();
    let dma_client = device_test_memory.dma_client();

    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut msi_set = MsiInterruptSet::new();
    let nvme = nvme::NvmeController::new(
        &driver_source,
        guest_mem,
        &mut msi_set,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
    );

    let mut device = NvmeTestEmulatedDevice::new(nvme, msi_set, dma_client.clone());

    // Setup mock response at offset 0
    let cap: Cap = Cap::new().with_mqes_z(0);
    device.set_mock_response_u64(Some((0, cap.into())));
    let driver = NvmeDriver::new(&driver_source, CPU_COUNT, device, false).await;

    assert!(driver.is_err());
}

struct NvmeTestConfig {
    allow_dma: bool,
    fail_at_driver_create: bool,
    fail_at_io_issuer: bool,
}

async fn test_nvme_driver(driver: DefaultDriver, config: NvmeTestConfig) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 64;

    let NvmeTestConfig {
        allow_dma,
        fail_at_driver_create,
        fail_at_io_issuer,
    } = config;

    // Arrange: Create 8MB of space. First 4MB for the device and second 4MB for the payload.
    let pages = 1024; // 4MB
    let device_test_memory = DeviceTestMemory::new(pages * 2, allow_dma, "test_nvme_driver");
    let guest_mem = device_test_memory.guest_memory(); // Access to 0-8MB

    let fail_alloc = Arc::new(AtomicBool::new(false));
    let dma_client = Arc::new(user_driver_emulated_mock::DeviceTestDmaClient::new(
        device_test_memory.dma_client(),
        NvmeTestDmaClientCallbacks {
            fail_alloc: fail_alloc.clone(),
        },
    )); // Access 0-4MB
    let payload_mem = device_test_memory.payload_mem(); // Access 4-8MB. This will allow dma if the `allow_dma` flag is set.

    // Arrange: Create the NVMe controller and driver.
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut msi_set = MsiInterruptSet::new();
    let nvme = nvme::NvmeController::new(
        &driver_source,
        guest_mem.clone(),
        &mut msi_set,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
    );

    nvme.client() // 2MB namespace
        .add_namespace(1, disklayer_ram::ram_disk(2 << 20, false).unwrap())
        .await
        .unwrap();
    let device = NvmeTestEmulatedDevice::new(nvme, msi_set, dma_client.clone());

    if fail_at_driver_create {
        fail_alloc.store(true, Ordering::SeqCst);
        let driver_result = NvmeDriver::new(&driver_source, CPU_COUNT, device, false).await;
        assert!(driver_result.is_err());
        return;
    }

    let mut driver = NvmeDriver::new(&driver_source, CPU_COUNT, device, false)
        .await
        .unwrap();
    let namespace = driver.namespace(1).await.unwrap();

    // Act: Write 1024 bytes of data to disk starting at LBA 1.
    let buf_range = OwnedRequestBuffers::linear(0, 16384, true); // 32 blocks
    payload_mem.write_at(0, &[0xcc; 4096]).unwrap();
    namespace
        .write(
            0,
            1,
            2,
            false,
            &payload_mem,
            buf_range.buffer(&payload_mem).range(),
        )
        .await
        .unwrap();

    let fallback_count = driver.fallback_cpu_count();
    if fail_at_io_issuer {
        fail_alloc.store(true, Ordering::SeqCst);
    }

    // Act: Read 16384 bytes of data from disk starting at LBA 0.
    namespace
        .read(
            1,
            0,
            32,
            &payload_mem,
            buf_range.buffer(&payload_mem).range(),
        )
        .await
        .unwrap();
    let mut v = [0; 4096];
    payload_mem.read_at(0, &mut v).unwrap();

    // Assert: First block should be 0x00 since we never wrote to it. Followed by 1024 bytes of 0xcc.
    assert_eq!(&v[..512], &[0; 512]);
    assert_eq!(&v[512..1536], &[0xcc; 1024]);
    assert!(v[1536..].iter().all(|&x| x == 0));

    // Assert: there should be another fallback CPU only if we forced allocation failure.
    if fail_at_io_issuer {
        assert_eq!(driver.fallback_cpu_count(), fallback_count + 1); // New CPU
    } else {
        assert_eq!(driver.fallback_cpu_count(), fallback_count);
    }

    namespace
        .deallocate(
            0,
            &[
                DsmRange {
                    context_attributes: 0,
                    starting_lba: 1000,
                    lba_count: 2000,
                },
                DsmRange {
                    context_attributes: 0,
                    starting_lba: 2,
                    lba_count: 2,
                },
            ],
        )
        .await
        .unwrap();

    // Test the fallback queue functionality (because of MSI-X limitations). Only do this
    // if the test didn't already force a fallback due to DMA client allocation failures.
    if !fail_at_io_issuer {
        assert_eq!(driver.fallback_cpu_count(), 0);

        namespace
            .read(
                63,
                0,
                32,
                &payload_mem,
                buf_range.buffer(&guest_mem).range(),
            )
            .await
            .unwrap();

        assert_eq!(driver.fallback_cpu_count(), 1);

        let mut v = [0; 4096];
        payload_mem.read_at(0, &mut v).unwrap();
        assert_eq!(&v[..512], &[0; 512]);
        assert_eq!(&v[512..1024], &[0xcc; 512]);
        assert!(v[1024..].iter().all(|&x| x == 0));
    }

    driver.shutdown().await;
}

// This helper function creates a NVMe fault controller with a namespace backed
// by PRDisk(RamDisk). It then creates and initializes the NVMe driver attached
// to the fault controller. The fault configuration passed in is applied to the controller.
// Admin queue is exercised during driver setup and IO queue is exercised by
// requesting a reservation report for cpu 0 and then performing a write to the
// namespace.
async fn test_nvme_fault_injection(driver: DefaultDriver, fault_configuration: FaultConfiguration) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 64;

    // Arrange: Create 8MB of space. First 4MB for the device and second 4MB for the payload.
    let pages = 1024; // 4MB
    let device_test_memory = DeviceTestMemory::new(pages * 2, false, "test_nvme_driver");
    let guest_mem = device_test_memory.guest_memory(); // Access to 0-8MB
    let dma_client = device_test_memory.dma_client(); // Access 0-4MB
    let payload_mem = device_test_memory.payload_mem(); // allow_dma is false, so this will follow the 'normal' test path (i.e. with bounce buffering behind the scenes)

    // Arrange: Create the NVMe controller and driver.
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut msi_set = MsiInterruptSet::new();
    let nvme = nvme_test::NvmeFaultController::new(
        &driver_source,
        guest_mem.clone(),
        &mut msi_set,
        &mut ExternallyManagedMmioIntercepts,
        nvme_test::NvmeFaultControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
        fault_configuration,
    );

    nvme.client() // 2MB namespace
        .add_namespace(
            1,
            Disk::new(DiskWithReservations::new(
                disklayer_ram::ram_disk(2 << 20, false).unwrap(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    let device = NvmeTestEmulatedDevice::new(nvme, msi_set, dma_client.clone());
    let mut driver = NvmeDriver::new(&driver_source, CPU_COUNT, device, false)
        .await
        .unwrap();
    let namespace = driver.namespace(1).await.unwrap();
    let _ = namespace.reservation_report_extended(0).await;

    // Act: Write 1024 bytes of data to disk starting at LBA 1.
    let buf_range = OwnedRequestBuffers::linear(0, 16384, true); // 32 blocks
    payload_mem.write_at(0, &[0xcc; 4096]).unwrap();
    namespace
        .write(
            0,
            1,
            2,
            false,
            &payload_mem,
            buf_range.buffer(&payload_mem).range(),
        )
        .await
        .unwrap();

    driver.shutdown().await;
}

#[derive(Inspect)]
pub struct NvmeTestEmulatedDevice<T: InspectMut, U: DmaClient> {
    device: EmulatedDevice<T, U>,
    #[inspect(debug)]
    mocked_response_u32: Arc<Mutex<Option<(usize, u32)>>>,
    #[inspect(debug)]
    mocked_response_u64: Arc<Mutex<Option<(usize, u64)>>>,
}

#[derive(Inspect)]
pub struct NvmeTestMapping<T> {
    mapping: Mapping<T>,
    #[inspect(debug)]
    mocked_response_u32: Arc<Mutex<Option<(usize, u32)>>>,
    #[inspect(debug)]
    mocked_response_u64: Arc<Mutex<Option<(usize, u64)>>>,
}

impl<T: PciConfigSpace + MmioIntercept + InspectMut, U: DmaClient> NvmeTestEmulatedDevice<T, U> {
    /// Creates a new emulated device, wrapping `device`, using the provided MSI controller.
    pub fn new(device: T, msi_set: MsiInterruptSet, dma_client: Arc<U>) -> Self {
        Self {
            device: EmulatedDevice::new(device, msi_set, dma_client.clone()),
            mocked_response_u32: Arc::new(Mutex::new(None)),
            mocked_response_u64: Arc::new(Mutex::new(None)),
        }
    }

    // TODO: set_mock_response_u32 is intentionally not implemented to avoid dead code.
    pub fn set_mock_response_u64(&mut self, mapping: Option<(usize, u64)>) {
        let mut mock_response = self.mocked_response_u64.lock();
        *mock_response = mapping;
    }
}

/// Implementation of DeviceBacking trait for NvmeTestEmulatedDevice
impl<T: 'static + Send + InspectMut + MmioIntercept, U: 'static + DmaClient> DeviceBacking
    for NvmeTestEmulatedDevice<T, U>
{
    type Registers = NvmeTestMapping<T>;

    fn id(&self) -> &str {
        self.device.id()
    }

    fn map_bar(&mut self, n: u8) -> anyhow::Result<Self::Registers> {
        Ok(NvmeTestMapping {
            mapping: self.device.map_bar(n).unwrap(),
            mocked_response_u32: Arc::clone(&self.mocked_response_u32),
            mocked_response_u64: Arc::clone(&self.mocked_response_u64),
        })
    }

    fn dma_client(&self) -> Arc<dyn DmaClient> {
        self.device.dma_client()
    }

    fn max_interrupt_count(&self) -> u32 {
        self.device.max_interrupt_count()
    }

    fn map_interrupt(&mut self, msix: u32, _cpu: u32) -> anyhow::Result<DeviceInterrupt> {
        self.device.map_interrupt(msix, _cpu)
    }
}

impl<T: MmioIntercept + Send> DeviceRegisterIo for NvmeTestMapping<T> {
    fn len(&self) -> usize {
        self.mapping.len()
    }

    fn read_u32(&self, offset: usize) -> u32 {
        let mock_response = self.mocked_response_u32.lock();

        // Intercept reads to the mocked offset address
        if let Some((mock_offset, mock_data)) = *mock_response {
            if mock_offset == offset {
                return mock_data;
            }
        }

        self.mapping.read_u32(offset)
    }

    fn read_u64(&self, offset: usize) -> u64 {
        let mock_response = self.mocked_response_u64.lock();

        // Intercept reads to the mocked offset address
        if let Some((mock_offset, mock_data)) = *mock_response {
            if mock_offset == offset {
                return mock_data;
            }
        }

        self.mapping.read_u64(offset)
    }

    fn write_u32(&self, offset: usize, data: u32) {
        self.mapping.write_u32(offset, data);
    }

    fn write_u64(&self, offset: usize, data: u64) {
        self.mapping.write_u64(offset, data);
    }
}

struct NvmeTestDmaClientCallbacks {
    fail_alloc: Arc<AtomicBool>,
}

impl DeviceTestDmaClientCallbacks for NvmeTestDmaClientCallbacks {
    fn allocate_dma_buffer(
        &self,
        inner: &page_pool_alloc::PagePoolAllocator,
        size: usize,
    ) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        match self.fail_alloc.load(Ordering::SeqCst) {
            true => anyhow::bail!("alloc failed"),
            false => inner.allocate_dma_buffer(size),
        }
    }

    fn attach_pending_buffers(
        &self,
        inner: &page_pool_alloc::PagePoolAllocator,
    ) -> anyhow::Result<Vec<user_driver::memory::MemoryBlock>> {
        match self.fail_alloc.load(Ordering::SeqCst) {
            true => anyhow::bail!("alloc failed"),
            false => inner.attach_pending_buffers(),
        }
    }
}
