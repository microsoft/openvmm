// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module drives the MANA emuulator with the MANA driver to test the
//! end-to-end flow.

use crate::bnic_driver::BnicDriver;
use crate::bnic_driver::RxConfig;
use crate::bnic_driver::WqConfig;
use crate::gdma_driver::GdmaDriver;
use crate::mana::ResourceArena;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use gdma::VportConfig;
use gdma_defs::GdmaDevType;
use gdma_defs::GdmaQueueType;
use net_backend::null::NullEndpoint;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pci_core::msi::MsiConnection;
use std::sync::Arc;
use test_with_tracing::test;
use user_driver::DeviceBacking;
use user_driver::memory::PAGE_SIZE;
use user_driver_emulated_mock::DeviceTestMemory;
use user_driver_emulated_mock::EmulatedDevice;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

#[async_test]
async fn test_gdma(driver: DefaultDriver) {
    let mem = DeviceTestMemory::new(128, false, "test_gdma");
    let msi_conn = MsiConnection::new();
    let device = gdma::GdmaDevice::new(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        mem.guest_memory(),
        msi_conn.target(),
        vec![VportConfig {
            mac_address: [1, 2, 3, 4, 5, 6].into(),
            endpoint: Box::new(NullEndpoint::new()),
        }],
        &mut ExternallyManagedMmioIntercepts,
    );
    let dma_client = mem.dma_client();
    let device = EmulatedDevice::new(device, msi_conn, dma_client);
    let dma_client = device.dma_client();
    let buffer = dma_client.allocate_dma_buffer(6 * PAGE_SIZE).unwrap();

    let mut gdma = GdmaDriver::new(&driver, device, 1, Some(buffer))
        .await
        .unwrap();
    gdma.test_eq().await.unwrap();
    gdma.verify_vf_driver_version().await.unwrap();
    let dev_id = gdma
        .list_devices()
        .await
        .unwrap()
        .iter()
        .copied()
        .find(|dev_id| dev_id.ty == GdmaDevType::GDMA_DEVICE_MANA)
        .unwrap();

    let device_props = gdma.register_device(dev_id).await.unwrap();
    let mut bnic = BnicDriver::new(&mut gdma, dev_id);
    let _dev_config = bnic.query_dev_config().await.unwrap();
    let port_config = bnic.query_vport_config(0).await.unwrap();
    let vport = port_config.vport;
    let buffer = Arc::new(
        gdma.device()
            .dma_client()
            .allocate_dma_buffer(0x5000)
            .unwrap(),
    );
    let mut arena = ResourceArena::new();
    let eq_gdma_region = gdma
        .create_dma_region(&mut arena, dev_id, buffer.subblock(0, PAGE_SIZE))
        .await
        .unwrap();
    let rq_gdma_region = gdma
        .create_dma_region(&mut arena, dev_id, buffer.subblock(PAGE_SIZE, PAGE_SIZE))
        .await
        .unwrap();
    let rq_cq_gdma_region = gdma
        .create_dma_region(
            &mut arena,
            dev_id,
            buffer.subblock(2 * PAGE_SIZE, PAGE_SIZE),
        )
        .await
        .unwrap();
    let sq_gdma_region = gdma
        .create_dma_region(
            &mut arena,
            dev_id,
            buffer.subblock(3 * PAGE_SIZE, PAGE_SIZE),
        )
        .await
        .unwrap();
    let sq_cq_gdma_region = gdma
        .create_dma_region(
            &mut arena,
            dev_id,
            buffer.subblock(4 * PAGE_SIZE, PAGE_SIZE),
        )
        .await
        .unwrap();
    let (eq_id, _) = gdma
        .create_eq(
            &mut arena,
            dev_id,
            eq_gdma_region,
            PAGE_SIZE as u32,
            device_props.pdid,
            device_props.db_id,
            0,
        )
        .await
        .unwrap();
    let mut bnic = BnicDriver::new(&mut gdma, dev_id);
    let _rq_cfg = bnic
        .create_wq_obj(
            &mut arena,
            vport,
            GdmaQueueType::GDMA_RQ,
            &WqConfig {
                wq_gdma_region: rq_gdma_region,
                cq_gdma_region: rq_cq_gdma_region,
                wq_size: PAGE_SIZE as u32,
                cq_size: PAGE_SIZE as u32,
                cq_moderation_ctx_id: 0,
                eq_id,
            },
        )
        .await
        .unwrap();
    let _sq_cfg = bnic
        .create_wq_obj(
            &mut arena,
            vport,
            GdmaQueueType::GDMA_SQ,
            &WqConfig {
                wq_gdma_region: sq_gdma_region,
                cq_gdma_region: sq_cq_gdma_region,
                wq_size: PAGE_SIZE as u32,
                cq_size: PAGE_SIZE as u32,
                cq_moderation_ctx_id: 0,
                eq_id,
            },
        )
        .await
        .unwrap();
    bnic.config_vport_tx(vport, 0, 0).await.unwrap();
    bnic.config_vport_rx(
        vport,
        &RxConfig {
            rx_enable: Some(true),
            rss_enable: Some(false),
            hash_key: None,
            default_rxobj: None,
            indirection_table: None,
        },
    )
    .await
    .unwrap();
    arena.destroy(&mut gdma).await;
}

#[async_test]
async fn test_gdma_save_restore(driver: DefaultDriver) {
    let mem = DeviceTestMemory::new(128, false, "test_gdma");
    let msi_conn = MsiConnection::new();
    let device = gdma::GdmaDevice::new(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        mem.guest_memory(),
        msi_conn.target(),
        vec![VportConfig {
            mac_address: [1, 2, 3, 4, 5, 6].into(),
            endpoint: Box::new(NullEndpoint::new()),
        }],
        &mut ExternallyManagedMmioIntercepts,
    );
    let dma_client = mem.dma_client();

    let device = EmulatedDevice::new(device, msi_conn, dma_client);
    let cloned_device = device.clone();

    let dma_client = device.dma_client();
    let gdma_buffer = dma_client.allocate_dma_buffer(6 * PAGE_SIZE).unwrap();

    let saved_state = {
        let mut gdma = GdmaDriver::new(&driver, device, 1, Some(gdma_buffer.clone()))
            .await
            .unwrap();

        gdma.test_eq().await.unwrap();
        gdma.verify_vf_driver_version().await.unwrap();
        gdma.save().await.unwrap()
    };

    let mut new_gdma = GdmaDriver::restore(saved_state, cloned_device, gdma_buffer)
        .await
        .unwrap();

    // Validate that the new driver still works after restoration.
    new_gdma.test_eq().await.unwrap();
}

#[async_test]
async fn test_gdma_reconfig_vf(driver: DefaultDriver) {
    let mem = DeviceTestMemory::new(128, false, "test_gdma");
    let msi_conn = MsiConnection::new();
    let device = gdma::GdmaDevice::new(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        mem.guest_memory(),
        msi_conn.target(),
        vec![VportConfig {
            mac_address: [1, 2, 3, 4, 5, 6].into(),
            endpoint: Box::new(NullEndpoint::new()),
        }],
        &mut ExternallyManagedMmioIntercepts,
    );
    let dma_client = mem.dma_client();
    let device = EmulatedDevice::new(device, msi_conn, dma_client);
    let dma_client = device.dma_client();
    let buffer = dma_client.allocate_dma_buffer(6 * PAGE_SIZE).unwrap();

    let mut gdma = GdmaDriver::new(&driver, device, 1, Some(buffer))
        .await
        .unwrap();

    assert!(
        !gdma.get_vf_reconfiguration_pending(),
        "vf_reconfiguration_pending should be false"
    );

    // Get the device ID while HWC is still alive (needed for deregister later).
    let dev_id = gdma
        .list_devices()
        .await
        .unwrap()
        .iter()
        .copied()
        .find(|dev_id| dev_id.ty == GdmaDevType::GDMA_DEVICE_MANA)
        .unwrap();

    // Trigger the reconfig event (EQE 135).
    gdma.generate_reconfig_vf_event().await.unwrap();

    assert!(
        gdma.get_vf_reconfiguration_pending(),
        "vf_reconfiguration_pending should be true after reconfig event"
    );

    // Deregister should fail immediately because vf_reconfiguration_pending is set.
    let deregister_result = gdma.deregister_device(dev_id).await;
    let err = deregister_result.expect_err("deregister_device should fail after EQE 135");
    let err_msg = format!("{err:#}");
    assert!(
        err_msg.contains("VF reconfiguration pending"),
        "unexpected error: {err_msg}"
    );
    assert!(
        gdma.get_vf_reconfiguration_pending(),
        "vf_reconfiguration_pending should remain true after deregister_device"
    );
}

#[async_test]
async fn test_take_device(driver: DefaultDriver) {
    let mem = DeviceTestMemory::new(128, false, "test_take_device");
    let msi_conn = MsiConnection::new();
    let device = gdma::GdmaDevice::new(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        mem.guest_memory(),
        msi_conn.target(),
        vec![VportConfig {
            mac_address: [1, 2, 3, 4, 5, 6].into(),
            endpoint: Box::new(NullEndpoint::new()),
        }],
        &mut ExternallyManagedMmioIntercepts,
    );
    let mem_dma_client = mem.dma_client();
    let device = EmulatedDevice::new(device, msi_conn, mem_dma_client);
    let device_dma_client = device.dma_client();
    let buffer = device_dma_client
        .allocate_dma_buffer(6 * PAGE_SIZE)
        .unwrap();

    let mut gdma = GdmaDriver::new(&driver, device, 1, Some(buffer))
        .await
        .unwrap();

    assert!(
        gdma.try_device().is_some(),
        "try_device should return Some before take"
    );

    assert!(
        gdma.take_device().is_some(),
        "take_device should return Some the first time"
    );

    assert!(
        gdma.try_device().is_none(),
        "try_device should return None after take"
    );

    assert!(
        gdma.take_device().is_none(),
        "take_device should return None the second time"
    );
}

#[async_test]
#[should_panic(expected = "device has been taken")]
async fn test_device_panics_after_take(driver: DefaultDriver) {
    let mem = DeviceTestMemory::new(128, false, "test_device_panics");
    let msi_conn = MsiConnection::new();
    let device = gdma::GdmaDevice::new(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        mem.guest_memory(),
        msi_conn.target(),
        vec![VportConfig {
            mac_address: [1, 2, 3, 4, 5, 6].into(),
            endpoint: Box::new(NullEndpoint::new()),
        }],
        &mut ExternallyManagedMmioIntercepts,
    );
    let dma_client = mem.dma_client();
    let device = EmulatedDevice::new(device, msi_conn, dma_client);
    let dma_client = device.dma_client();
    let buffer = dma_client.allocate_dma_buffer(6 * PAGE_SIZE).unwrap();

    let mut gdma = GdmaDriver::new(&driver, device, 1, Some(buffer))
        .await
        .unwrap();

    let _ = gdma.take_device();
    // gdma.device() should panic because the device has been taken.
    let _ = gdma.device();
}

#[async_test]
async fn test_mana_shutdown_with_stale_vport(driver: DefaultDriver) {
    use crate::mana::ManaDevice;

    let mem = DeviceTestMemory::new(128, false, "test_mana_shutdown_stale");
    let msi_conn = MsiConnection::new();
    let device = gdma::GdmaDevice::new(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        mem.guest_memory(),
        msi_conn.target(),
        vec![VportConfig {
            mac_address: [1, 2, 3, 4, 5, 6].into(),
            endpoint: Box::new(NullEndpoint::new()),
        }],
        &mut ExternallyManagedMmioIntercepts,
    );
    let dma_client = mem.dma_client();
    let device = EmulatedDevice::new(device, msi_conn, dma_client);

    let mana = ManaDevice::new(&driver, device, 1, 1, None).await.unwrap();

    // Create a vport which holds an Arc<Inner> reference.
    let vport = mana.new_vport(0, None, mana.dev_config()).await.unwrap();

    // shutdown() should succeed, and not panic, even though a Vport still holds a reference.
    let (result, _device) = mana.shutdown().await;
    assert!(result.is_ok(), "shutdown should succeed");

    let dma_result = vport.dma_client().await;
    assert!(
        dma_result.is_err(),
        "dma_client on stale vport should return Err, not panic"
    );
}

#[async_test]
async fn test_mana_save_with_stale_vport(driver: DefaultDriver) {
    use crate::mana::ManaDevice;

    let mem = DeviceTestMemory::new(128, false, "test_mana_save_stale");
    let msi_conn = MsiConnection::new();
    let device = gdma::GdmaDevice::new(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
        mem.guest_memory(),
        msi_conn.target(),
        vec![VportConfig {
            mac_address: [1, 2, 3, 4, 5, 6].into(),
            endpoint: Box::new(NullEndpoint::new()),
        }],
        &mut ExternallyManagedMmioIntercepts,
    );
    let dma_client = mem.dma_client();
    let device = EmulatedDevice::new(device, msi_conn, dma_client);

    let mana = ManaDevice::new(&driver, device, 1, 1, None).await.unwrap();

    // Create a vport which holds an Arc<Inner> reference.
    let vport = mana.new_vport(0, None, mana.dev_config()).await.unwrap();

    // save() should succeed even though a Vport still holds a reference.
    let (result, _device) = mana.save().await;
    assert!(result.is_ok(), "save should succeed");

    let dma_result = vport.dma_client().await;
    assert!(
        dma_result.is_err(),
        "dma_client on stale vport should return Err, not panic"
    );
}
