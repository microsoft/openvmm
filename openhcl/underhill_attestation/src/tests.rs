// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::test_helpers::MockTeeCall;
use crate::test_helpers::MockTeeCallNoGetDerivedKey;
use crate::test_helpers::new_key_protector_by_id;
use disk_backend::Disk;
use disklayer_ram::ram_disk;
use guest_emulation_device::IgvmAgentAction;
use guest_emulation_device::IgvmAgentTestPlan;
use guest_emulation_transport::test_utilities::TestGet;
use key_protector::AES_WRAPPED_AES_KEY_LENGTH;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestType;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::task::Spawn;
use std::collections::VecDeque;
use test_with_tracing::test;
use vmgs_format::EncryptionAlgorithm;
use vmgs_format::FileId;
use zerocopy::IntoBytes;

const ONE_MEGA_BYTE: u64 = 1024 * 1024;

fn new_test_file() -> Disk {
    ram_disk(4 * ONE_MEGA_BYTE, false).unwrap()
}

async fn new_formatted_vmgs() -> Vmgs {
    let disk = new_test_file();

    let mut vmgs = Vmgs::format_new(disk, None).await.unwrap();

    assert!(
        key_protector_is_empty(&mut vmgs).await,
        "Newly formatted VMGS should have an empty key protector"
    );
    assert!(
        key_protector_by_id_is_empty(&mut vmgs).await,
        "Newly formatted VMGS should have an empty key protector by id"
    );

    vmgs
}

async fn key_protector_is_empty(vmgs: &mut Vmgs) -> bool {
    let key_protector = vmgs::read_key_protector(vmgs, AES_WRAPPED_AES_KEY_LENGTH)
        .await
        .unwrap();

    key_protector.as_bytes().iter().all(|&b| b == 0)
}

async fn key_protector_by_id_is_empty(vmgs: &mut Vmgs) -> bool {
    vmgs::read_key_protector_by_id(vmgs)
        .await
        .is_err_and(|err| {
            matches!(
                err,
                vmgs::ReadFromVmgsError::EntryNotFound(FileId::VM_UNIQUE_ID)
            )
        })
}

async fn hardware_key_protector_is_empty(vmgs: &mut Vmgs) -> bool {
    vmgs::read_hardware_key_protector(vmgs)
        .await
        .is_err_and(|err| {
            matches!(
                err,
                vmgs::ReadFromVmgsError::EntryNotFound(FileId::HW_KEY_PROTECTOR)
            )
        })
}

fn new_key_protector() -> KeyProtector {
    test_helpers::new_key_protector(0)
}

async fn new_test_get(
    spawn: impl Spawn,
    enable_igvm_attest: bool,
    plan: Option<IgvmAgentTestPlan>,
) -> TestGet {
    if enable_igvm_attest {
        const TEST_DEVICE_MEMORY_SIZE: u64 = 64;
        // Use `DeviceTestMemory` to set up shared memory required by the IGVM_ATTEST GET calls.
        let dev_test_mem = user_driver_emulated_mock::DeviceTestMemory::new(
            TEST_DEVICE_MEMORY_SIZE,
            true,
            "test-attest",
        );

        let mut test_get = guest_emulation_transport::test_utilities::new_transport_pair(
            spawn,
            None,
            get_protocol::ProtocolVersion::NICKEL_REV2,
            Some(dev_test_mem.guest_memory()),
            plan,
        )
        .await;

        test_get.client.set_gpa_allocator(dev_test_mem.dma_client());

        test_get
    } else {
        guest_emulation_transport::test_utilities::new_transport_pair(
            spawn,
            None,
            get_protocol::ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await
    }
}

#[async_test]
async fn do_nothing_without_derived_keys() {
    let mut vmgs = new_formatted_vmgs().await;

    let mut key_protector = new_key_protector();
    let mut key_protector_by_id = new_key_protector_by_id(None, None, false);

    let key_protector_settings = KeyProtectorActions {
        should_write_kp: false,
        use_gsp_by_id: false,
        use_hardware_unlock: false,
    };

    let bios_guid = Guid::new_random();

    unlock_vmgs_data_store(
        &mut vmgs,
        false,
        &mut key_protector,
        &mut key_protector_by_id,
        None,
        key_protector_settings,
        bios_guid,
    )
    .await
    .unwrap();

    assert!(key_protector_is_empty(&mut vmgs).await);
    assert!(key_protector_by_id_is_empty(&mut vmgs).await);

    // Create another instance as the previous `unlock_vmgs_data_store` took ownership of the last one
    let key_protector_settings = KeyProtectorActions {
        should_write_kp: false,
        use_gsp_by_id: false,
        use_hardware_unlock: false,
    };

    // Even if the VMGS is encrypted, if no derived keys are provided, nothing should happen
    unlock_vmgs_data_store(
        &mut vmgs,
        true,
        &mut key_protector,
        &mut key_protector_by_id,
        None,
        key_protector_settings,
        bios_guid,
    )
    .await
    .unwrap();

    assert!(key_protector_is_empty(&mut vmgs).await);
    assert!(key_protector_by_id_is_empty(&mut vmgs).await);
}

#[async_test]
async fn provision_vmgs_and_rotate_keys() {
    let mut vmgs = new_formatted_vmgs().await;

    let mut key_protector = new_key_protector();
    let mut key_protector_by_id = new_key_protector_by_id(None, None, false);

    let ingress = [1; AES_GCM_KEY_LENGTH];
    let egress = [2; AES_GCM_KEY_LENGTH];
    let derived_keys = Keys {
        ingress,
        decrypt_egress: None,
        encrypt_egress: egress,
    };

    let key_protector_settings = KeyProtectorActions {
        should_write_kp: true,
        use_gsp_by_id: true,
        use_hardware_unlock: false,
    };

    let bios_guid = Guid::new_random();

    // Without encryption implies the provision path
    // The VMGS will be locked using the egress key
    unlock_vmgs_data_store(
        &mut vmgs,
        false,
        &mut key_protector,
        &mut key_protector_by_id,
        Some(derived_keys),
        key_protector_settings,
        bios_guid,
    )
    .await
    .unwrap();

    // The ingress key is essentially ignored since the VMGS wasn't previously encrypted
    vmgs.unlock_with_encryption_key(&ingress).await.unwrap_err();

    // The egress key was used to lock the VMGS after provisioning
    vmgs.unlock_with_encryption_key(&egress).await.unwrap();
    // Since this is a new VMGS, the egress key is the first and only key
    assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(0));

    // Since both `should_write_kp` and `use_gsp_by_id` are true, both key protectors should be updated
    assert!(!key_protector_is_empty(&mut vmgs).await);
    assert!(!key_protector_by_id_is_empty(&mut vmgs).await);

    let found_key_protector = vmgs::read_key_protector(&mut vmgs, AES_WRAPPED_AES_KEY_LENGTH)
        .await
        .unwrap();
    assert_eq!(found_key_protector.as_bytes(), key_protector.as_bytes());

    let found_key_protector_by_id = vmgs::read_key_protector_by_id(&mut vmgs).await.unwrap();
    assert_eq!(
        found_key_protector_by_id.as_bytes(),
        key_protector_by_id.inner_for_test().as_bytes()
    );

    // Now that the VMGS has been provisioned, simulate the rotation of keys
    let new_egress = [3; AES_GCM_KEY_LENGTH];

    let mut new_key_protector = new_key_protector();
    let mut new_key_protector_by_id = new_key_protector_by_id(None, None, false);

    let key_protector_settings = KeyProtectorActions {
        should_write_kp: true,
        use_gsp_by_id: true,
        use_hardware_unlock: false,
    };

    // Ingress is now the old egress, and we provide a new new egress key
    let derived_keys = Keys {
        ingress: egress,
        decrypt_egress: None,
        encrypt_egress: new_egress,
    };

    unlock_vmgs_data_store(
        &mut vmgs,
        true,
        &mut new_key_protector,
        &mut new_key_protector_by_id,
        Some(derived_keys),
        key_protector_settings,
        bios_guid,
    )
    .await
    .unwrap();

    // We should still fail to unlock the VMGS with the original ingress key
    vmgs.unlock_with_encryption_key(&ingress).await.unwrap_err();
    // The old egress key should no longer be able to unlock the VMGS
    vmgs.unlock_with_encryption_key(&egress).await.unwrap_err();

    // The new egress key should be able to unlock the VMGS
    vmgs.unlock_with_encryption_key(&new_egress).await.unwrap();
    // The old egress key was removed, but not before the new egress key was added in the 1st slot
    assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(1));

    let found_key_protector = vmgs::read_key_protector(&mut vmgs, AES_WRAPPED_AES_KEY_LENGTH)
        .await
        .unwrap();
    assert_eq!(found_key_protector.as_bytes(), new_key_protector.as_bytes());

    let found_key_protector_by_id = vmgs::read_key_protector_by_id(&mut vmgs).await.unwrap();
    assert_eq!(
        found_key_protector_by_id.as_bytes(),
        new_key_protector_by_id.inner_for_test().as_bytes()
    );
}

#[async_test]
async fn unlock_previously_encrypted_vmgs_with_ingress_key() {
    let mut vmgs = new_formatted_vmgs().await;

    let mut key_protector = new_key_protector();
    let mut key_protector_by_id = new_key_protector_by_id(None, None, false);

    let ingress = [1; AES_GCM_KEY_LENGTH];
    let egress = [2; AES_GCM_KEY_LENGTH];

    let derived_keys = Keys {
        ingress,
        decrypt_egress: None,
        encrypt_egress: egress,
    };

    vmgs.update_encryption_key(&ingress, EncryptionAlgorithm::AES_GCM)
        .await
        .unwrap();

    // Initially, the VMGS can be unlocked using the ingress key
    vmgs.unlock_with_encryption_key(&ingress).await.unwrap();
    assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(0));

    let key_protector_settings = KeyProtectorActions {
        should_write_kp: true,
        use_gsp_by_id: true,
        use_hardware_unlock: false,
    };

    let bios_guid = Guid::new_random();

    unlock_vmgs_data_store(
        &mut vmgs,
        true,
        &mut key_protector,
        &mut key_protector_by_id,
        Some(derived_keys),
        key_protector_settings,
        bios_guid,
    )
    .await
    .unwrap();

    // After the VMGS has been unlocked, the VMGS encryption key should be rotated from ingress to egress
    vmgs.unlock_with_encryption_key(&ingress).await.unwrap_err();
    vmgs.unlock_with_encryption_key(&egress).await.unwrap();
    // The ingress key was removed, but not before the egress key was added in the 0th slot
    assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(1));

    // Since both `should_write_kp` and `use_gsp_by_id` are true, both key protectors should be updated
    let found_key_protector = vmgs::read_key_protector(&mut vmgs, AES_WRAPPED_AES_KEY_LENGTH)
        .await
        .unwrap();
    assert_eq!(found_key_protector.as_bytes(), key_protector.as_bytes());

    let found_key_protector_by_id = vmgs::read_key_protector_by_id(&mut vmgs).await.unwrap();
    assert_eq!(
        found_key_protector_by_id.as_bytes(),
        key_protector_by_id.inner_for_test().as_bytes()
    );
}

#[async_test]
async fn failed_to_persist_ingress_key_so_use_egress_key_to_unlock_vmgs() {
    let mut vmgs = new_formatted_vmgs().await;

    let mut key_protector = new_key_protector();
    let mut key_protector_by_id = new_key_protector_by_id(None, None, false);

    let ingress = [1; AES_GCM_KEY_LENGTH];
    let decrypt_egress = [2; AES_GCM_KEY_LENGTH];
    let encrypt_egress = [3; AES_GCM_KEY_LENGTH];

    let derived_keys = Keys {
        ingress,
        decrypt_egress: Some(decrypt_egress),
        encrypt_egress,
    };

    // Add only the egress key to the VMGS to simulate a failure to persist the ingress key
    vmgs.test_add_new_encryption_key(&decrypt_egress, EncryptionAlgorithm::AES_GCM)
        .await
        .unwrap();
    let egress_key_index = vmgs.test_get_active_datastore_key_index().unwrap();
    assert_eq!(egress_key_index, 0);

    vmgs.unlock_with_encryption_key(&decrypt_egress)
        .await
        .unwrap();
    let found_egress_key_index = vmgs.test_get_active_datastore_key_index().unwrap();
    assert_eq!(found_egress_key_index, egress_key_index);

    // Confirm that the ingress key cannot be used to unlock the VMGS
    vmgs.unlock_with_encryption_key(&ingress).await.unwrap_err();

    let key_protector_settings = KeyProtectorActions {
        should_write_kp: true,
        use_gsp_by_id: true,
        use_hardware_unlock: false,
    };

    let bios_guid = Guid::new_random();

    unlock_vmgs_data_store(
        &mut vmgs,
        true,
        &mut key_protector,
        &mut key_protector_by_id,
        Some(derived_keys),
        key_protector_settings,
        bios_guid,
    )
    .await
    .unwrap();

    // Confirm that the ingress key was not added
    vmgs.unlock_with_encryption_key(&ingress).await.unwrap_err();

    // Confirm that the decrypt egress key no longer works
    vmgs.unlock_with_encryption_key(&decrypt_egress)
        .await
        .unwrap_err();

    // The encrypt_egress key can unlock the VMGS and was added as a new key
    vmgs.unlock_with_encryption_key(&encrypt_egress)
        .await
        .unwrap();
    assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(1));

    // Since both `should_write_kp` and `use_gsp_by_id` are true, both key protectors should be updated
    let found_key_protector = vmgs::read_key_protector(&mut vmgs, AES_WRAPPED_AES_KEY_LENGTH)
        .await
        .unwrap();
    assert_eq!(found_key_protector.as_bytes(), key_protector.as_bytes());

    let found_key_protector_by_id = vmgs::read_key_protector_by_id(&mut vmgs).await.unwrap();
    assert_eq!(
        found_key_protector_by_id.as_bytes(),
        key_protector_by_id.inner_for_test().as_bytes()
    );
}

#[async_test]
async fn fail_to_unlock_vmgs_with_existing_ingress_key() {
    let mut vmgs = new_formatted_vmgs().await;

    let mut key_protector = new_key_protector();
    let mut key_protector_by_id = new_key_protector_by_id(None, None, false);

    let ingress = [1; AES_GCM_KEY_LENGTH];

    // Ingress and egress keys are the same
    let derived_keys = Keys {
        ingress,
        decrypt_egress: None,
        encrypt_egress: ingress,
    };

    // Add two random keys to the VMGS to simulate unlock failure when ingress and egress keys are the same
    let additional_key = [2; AES_GCM_KEY_LENGTH];
    let yet_another_key = [3; AES_GCM_KEY_LENGTH];

    vmgs.test_add_new_encryption_key(&additional_key, EncryptionAlgorithm::AES_GCM)
        .await
        .unwrap();
    assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(0));

    vmgs.test_add_new_encryption_key(&yet_another_key, EncryptionAlgorithm::AES_GCM)
        .await
        .unwrap();
    assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(1));

    let key_protector_settings = KeyProtectorActions {
        should_write_kp: true,
        use_gsp_by_id: true,
        use_hardware_unlock: false,
    };

    let bios_guid = Guid::new_random();

    let unlock_result = unlock_vmgs_data_store(
        &mut vmgs,
        true,
        &mut key_protector,
        &mut key_protector_by_id,
        Some(derived_keys),
        key_protector_settings,
        bios_guid,
    )
    .await;
    assert!(matches!(
        unlock_result,
        Err(UnlockVmgsDataStoreError::VmgsUnlockUsingExistingIngressKey(
            _
        ))
    ));
}

#[async_test]
async fn fail_to_unlock_vmgs_with_new_ingress_key() {
    let mut vmgs = new_formatted_vmgs().await;

    let mut key_protector = new_key_protector();
    let mut key_protector_by_id = new_key_protector_by_id(None, None, false);

    let derived_keys = Keys {
        ingress: [1; AES_GCM_KEY_LENGTH],
        decrypt_egress: None,
        encrypt_egress: [2; AES_GCM_KEY_LENGTH],
    };

    // Add two random keys to the VMGS to simulate unlock failure when ingress and egress keys are *not* the same
    let additional_key = [3; AES_GCM_KEY_LENGTH];
    let yet_another_key = [4; AES_GCM_KEY_LENGTH];

    vmgs.test_add_new_encryption_key(&additional_key, EncryptionAlgorithm::AES_GCM)
        .await
        .unwrap();
    assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(0));

    vmgs.test_add_new_encryption_key(&yet_another_key, EncryptionAlgorithm::AES_GCM)
        .await
        .unwrap();
    assert_eq!(vmgs.test_get_active_datastore_key_index(), Some(1));

    let key_protector_settings = KeyProtectorActions {
        should_write_kp: true,
        use_gsp_by_id: true,
        use_hardware_unlock: false,
    };

    let bios_guid = Guid::new_random();

    let unlock_result = unlock_vmgs_data_store(
        &mut vmgs,
        true,
        &mut key_protector,
        &mut key_protector_by_id,
        Some(derived_keys),
        key_protector_settings,
        bios_guid,
    )
    .await;
    assert!(matches!(
        unlock_result,
        Err(UnlockVmgsDataStoreError::VmgsUnlockUsingExistingIngressKey(
            _
        ))
    ));
}

#[async_test]
async fn pass_through_persist_all_key_protectors() {
    let mut vmgs = new_formatted_vmgs().await;
    let mut key_protector = new_key_protector();
    let mut key_protector_by_id = new_key_protector_by_id(None, None, false);
    let bios_guid = Guid::new_random();

    // Copied/cloned bits used for comparison later
    let kp_copy = key_protector.as_bytes().to_vec();
    let active_kp_copy = key_protector.active_kp;

    // When all key protector settings are true, no actions will be taken on the key protectors or VMGS
    let key_protector_settings = KeyProtectorActions {
        should_write_kp: true,
        use_gsp_by_id: true,
        use_hardware_unlock: true,
    };
    persist_all_key_protectors(
        &mut vmgs,
        &mut key_protector,
        &mut key_protector_by_id,
        bios_guid,
        key_protector_settings,
    )
    .await
    .unwrap();

    assert!(key_protector_is_empty(&mut vmgs).await);
    assert!(key_protector_by_id_is_empty(&mut vmgs).await);

    // The key protector should remain unchanged
    assert_eq!(active_kp_copy, key_protector.active_kp);
    assert_eq!(kp_copy.as_slice(), key_protector.as_bytes());
}

#[async_test]
async fn persist_all_key_protectors_write_key_protector_by_id() {
    let mut vmgs = new_formatted_vmgs().await;
    let mut key_protector = new_key_protector();
    let mut key_protector_by_id = new_key_protector_by_id(None, None, false);
    let bios_guid = Guid::new_random();

    // Copied/cloned bits used for comparison later
    let kp_copy = key_protector.as_bytes().to_vec();
    let active_kp_copy = key_protector.active_kp;

    // When `use_gsp_by_id` is true and `should_write_kp` is false, the key protector by id should be written to the VMGS
    let key_protector_settings = KeyProtectorActions {
        should_write_kp: false,
        use_gsp_by_id: true,
        use_hardware_unlock: false,
    };
    persist_all_key_protectors(
        &mut vmgs,
        &mut key_protector,
        &mut key_protector_by_id,
        bios_guid,
        key_protector_settings,
    )
    .await
    .unwrap();

    // The previously empty VMGS now holds the key protector by id but not the key protector
    assert!(key_protector_is_empty(&mut vmgs).await);
    assert!(!key_protector_by_id_is_empty(&mut vmgs).await);

    let found_key_protector_by_id = vmgs::read_key_protector_by_id(&mut vmgs).await.unwrap();
    assert_eq!(
        found_key_protector_by_id.as_bytes(),
        key_protector_by_id.inner_for_test().as_bytes()
    );

    // The key protector should remain unchanged
    assert_eq!(kp_copy.as_slice(), key_protector.as_bytes());
    assert_eq!(active_kp_copy, key_protector.active_kp);
}

#[async_test]
async fn persist_all_key_protectors_remove_ingress_kp() {
    let mut vmgs = new_formatted_vmgs().await;
    let mut key_protector = new_key_protector();
    let mut key_protector_by_id = new_key_protector_by_id(None, None, false);
    let bios_guid = Guid::new_random();

    // Copied active KP for later use
    let active_kp_copy = key_protector.active_kp;

    // When `use_gsp_by_id` is false, `should_write_kp` is true, and `use_hardware_unlock` is false, the active key protector's
    // active kp's dek should be zeroed, the active kp's gsp length should be set to 0, and the active kp should be incremented
    let key_protector_settings = KeyProtectorActions {
        should_write_kp: true,
        use_gsp_by_id: false,
        use_hardware_unlock: false,
    };
    persist_all_key_protectors(
        &mut vmgs,
        &mut key_protector,
        &mut key_protector_by_id,
        bios_guid,
        key_protector_settings,
    )
    .await
    .unwrap();

    assert!(!key_protector_is_empty(&mut vmgs).await);
    assert!(key_protector_by_id_is_empty(&mut vmgs).await);

    // The previously empty VMGS's key protector should now be overwritten
    let found_key_protector = vmgs::read_key_protector(&mut vmgs, AES_WRAPPED_AES_KEY_LENGTH)
        .await
        .unwrap();

    assert!(
        found_key_protector.dek[active_kp_copy as usize]
            .dek_buffer
            .iter()
            .all(|&b| b == 0),
    );
    assert_eq!(
        found_key_protector.gsp[active_kp_copy as usize].gsp_length,
        0
    );
    assert_eq!(found_key_protector.active_kp, active_kp_copy + 1);
}

#[async_test]
async fn persist_all_key_protectors_mark_key_protector_by_id_as_not_in_use() {
    let mut vmgs = new_formatted_vmgs().await;
    let mut key_protector = new_key_protector();
    let mut key_protector_by_id = new_key_protector_by_id(None, None, true);
    let bios_guid = Guid::new_random();

    // When `use_gsp_by_id` is false, `should_write_kp` is true, `use_hardware_unlock` is true, and
    // the key protector by id is found and not ported, the key protector by id should be marked as ported
    let key_protector_settings = KeyProtectorActions {
        should_write_kp: true,
        use_gsp_by_id: false,
        use_hardware_unlock: true,
    };

    persist_all_key_protectors(
        &mut vmgs,
        &mut key_protector,
        &mut key_protector_by_id,
        bios_guid,
        key_protector_settings,
    )
    .await
    .unwrap();

    assert!(key_protector_is_empty(&mut vmgs).await);
    assert!(!key_protector_by_id_is_empty(&mut vmgs).await);

    // The previously empty VMGS's key protector by id should now be overwritten
    let found_key_protector_by_id = vmgs::read_key_protector_by_id(&mut vmgs).await.unwrap();
    assert_eq!(found_key_protector_by_id.ported, 1);
    assert_eq!(
        found_key_protector_by_id.id_guid,
        key_protector_by_id.inner_for_test().id_guid
    );
}

// --- initialize_platform_security tests ---

#[async_test]
async fn init_sec_suppress_attestation(driver: DefaultDriver) {
    let mut vmgs = new_formatted_vmgs().await;

    // Write non-zero agent data to VMGS so we can verify it is returned.
    let agent = SecurityProfile {
        agent_data: [0xAA; AGENT_DATA_MAX_SIZE],
    };
    vmgs.write_file(FileId::ATTEST, agent.as_bytes())
        .await
        .unwrap();

    // Ensure no IGVM attest call out
    let get_pair = new_test_get(driver, false, None).await;

    let bios_guid = Guid::new_random();
    let att_cfg = Default::default();

    // Ensure VMGS is not encrypted and agent data is empty before the call
    assert!(!vmgs.encrypted());

    // Obtain a LocalDriver briefly, then run the async flow under the pool executor
    let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
    let res = initialize_platform_security(
        &get_pair.client,
        bios_guid,
        &att_cfg,
        &mut vmgs,
        None, // no TEE when suppressed
        true, // suppress_attestation
        ldriver,
        GuestStateEncryptionPolicy::None,
        true,
    )
    .await
    .unwrap();

    // VMGS remains unencrypted and KP/HWKP not written.
    assert!(!vmgs.encrypted());
    assert!(key_protector_is_empty(&mut vmgs).await);
    assert!(hardware_key_protector_is_empty(&mut vmgs).await);
    // Agent data passed through
    assert_eq!(res.agent_data.unwrap(), agent.agent_data.to_vec());
    // Secure key should be None without pre-provisioning
    assert!(res.guest_secret_key.is_none());
}

#[async_test]
async fn init_sec_secure_key_release_with_wrapped_key_request(driver: DefaultDriver) {
    let mut vmgs = new_formatted_vmgs().await;

    // IGVM attest is required
    let get_pair = new_test_get(driver, true, None).await;

    let bios_guid = Guid::new_random();
    let att_cfg = Default::default();
    let tee = MockTeeCall::new(0x1234);

    // Ensure VMGS is not encrypted and agent data is empty before the call
    assert!(!vmgs.encrypted());

    // Obtain a LocalDriver briefly, then run the async flow under the pool executor
    let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
    let res = initialize_platform_security(
        &get_pair.client,
        bios_guid,
        &att_cfg,
        &mut vmgs,
        Some(&tee),
        false,
        ldriver.clone(),
        GuestStateEncryptionPolicy::Auto,
        true,
    )
    .await
    .unwrap();

    // VMGS is now encrypted and HWKP is updated.
    assert!(vmgs.encrypted());
    assert!(!hardware_key_protector_is_empty(&mut vmgs).await);

    // Agent data should be the same as `key_reference` in the WRAPPED_KEY response.
    // See vm/devices/get/guest_emulation_device/src/test_igvm_agent.rs for the expected response.
    let key_reference = serde_json::json!({
        "key_info": {
            "host": "name"
        },
        "attestation_info": {
            "host": "attestation_name"
        }
    });
    let key_reference = serde_json::to_string(&key_reference).unwrap();
    let key_reference = key_reference.as_bytes();
    let mut expected_agent_data = [0u8; AGENT_DATA_MAX_SIZE];
    expected_agent_data[..key_reference.len()].copy_from_slice(key_reference);
    assert_eq!(res.agent_data.unwrap(), expected_agent_data.to_vec());
    // Secure key should be None without pre-provisioning
    assert!(res.guest_secret_key.is_none());

    // Second call: VMGS unlock via SKR should succeed
    initialize_platform_security(
        &get_pair.client,
        bios_guid,
        &att_cfg,
        &mut vmgs,
        Some(&tee),
        false,
        ldriver,
        GuestStateEncryptionPolicy::Auto,
        true,
    )
    .await
    .unwrap();

    // VMGS should remain encrypted
    assert!(vmgs.encrypted());
}

#[async_test]
async fn init_sec_secure_key_release_without_wrapped_key_request(driver: DefaultDriver) {
    let mut vmgs = new_formatted_vmgs().await;

    // Write non-zero agent data to workaround the WRAPPED_KEY_REQUEST requirement.
    let agent = SecurityProfile {
        agent_data: [0xAA; AGENT_DATA_MAX_SIZE],
    };
    vmgs.write_file(FileId::ATTEST, agent.as_bytes())
        .await
        .unwrap();

    // Skip WRAPPED_KEY_REQUEST for both boots
    let mut plan = IgvmAgentTestPlan::default();
    plan.insert(
        IgvmAttestRequestType::WRAPPED_KEY_REQUEST,
        VecDeque::from([IgvmAgentAction::NoResponse, IgvmAgentAction::NoResponse]),
    );

    // IGVM attest is required
    let get_pair = new_test_get(driver, true, Some(plan)).await;

    let bios_guid = Guid::new_random();
    let att_cfg = Default::default();
    let tee = MockTeeCall::new(0x1234);

    // Ensure VMGS is not encrypted and agent data is empty before the call
    assert!(!vmgs.encrypted());

    // Obtain a LocalDriver briefly, then run the async flow under the pool executor
    let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
    let res = initialize_platform_security(
        &get_pair.client,
        bios_guid,
        &att_cfg,
        &mut vmgs,
        Some(&tee),
        false,
        ldriver.clone(),
        GuestStateEncryptionPolicy::Auto,
        true,
    )
    .await
    .unwrap();

    // VMGS is now encrypted and HWKP is updated.
    assert!(vmgs.encrypted());
    assert!(!hardware_key_protector_is_empty(&mut vmgs).await);
    // Agent data passed through
    assert_eq!(res.agent_data.clone().unwrap(), agent.agent_data.to_vec());
    // Secure key should be None without pre-provisioning
    assert!(res.guest_secret_key.is_none());

    // Second call: VMGS unlock via SKR should succeed
    let res = initialize_platform_security(
        &get_pair.client,
        bios_guid,
        &att_cfg,
        &mut vmgs,
        Some(&tee),
        false,
        ldriver,
        GuestStateEncryptionPolicy::Auto,
        true,
    )
    .await
    .unwrap();

    // VMGS should remain encrypted
    assert!(vmgs.encrypted());
    // Agent data passed through
    assert_eq!(res.agent_data.clone().unwrap(), agent.agent_data.to_vec());
    // Secure key should be None without pre-provisioning
    assert!(res.guest_secret_key.is_none());
}

#[async_test]
async fn init_sec_secure_key_release_hw_sealing_backup(driver: DefaultDriver) {
    let mut vmgs = new_formatted_vmgs().await;

    // IGVM attest is required
    let mut plan = IgvmAgentTestPlan::default();
    plan.insert(
        IgvmAttestRequestType::WRAPPED_KEY_REQUEST,
        VecDeque::from([
            IgvmAgentAction::RespondSuccess,
            // initialize_platform_security will attempt SKR/unlock 10 times
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
        ]),
    );

    let get_pair = new_test_get(driver, true, Some(plan)).await;

    let bios_guid = Guid::new_random();
    let att_cfg = Default::default();

    // Ensure VMGS is not encrypted and agent data is empty before the call
    assert!(!vmgs.encrypted());

    // Obtain a LocalDriver briefly, then run the async flow under the pool executor
    let tee = MockTeeCall::new(0x1234);
    let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
    let res = initialize_platform_security(
        &get_pair.client,
        bios_guid,
        &att_cfg,
        &mut vmgs,
        Some(&tee),
        false,
        ldriver.clone(),
        GuestStateEncryptionPolicy::Auto,
        true,
    )
    .await
    .unwrap();

    // VMGS is now encrypted and HWKP is updated.
    assert!(vmgs.encrypted());
    assert!(!hardware_key_protector_is_empty(&mut vmgs).await);
    // Agent data should be the same as `key_reference` in the WRAPPED_KEY response.
    // See vm/devices/get/guest_emulation_device/src/test_igvm_agent.rs for the expected response.
    let key_reference = serde_json::json!({
        "key_info": {
            "host": "name"
        },
        "attestation_info": {
            "host": "attestation_name"
        }
    });
    let key_reference = serde_json::to_string(&key_reference).unwrap();
    let key_reference = key_reference.as_bytes();
    let mut expected_agent_data = [0u8; AGENT_DATA_MAX_SIZE];
    expected_agent_data[..key_reference.len()].copy_from_slice(key_reference);
    assert_eq!(res.agent_data.unwrap(), expected_agent_data.to_vec());
    // Secure key should be None without pre-provisioning
    assert!(res.guest_secret_key.is_none());

    // Second call: VMGS unlock via key recovered with hardware sealing
    // NOTE: The test relies on the test GED to return failing WRAPPED_KEY response
    // with retry recommendation as false to skip the retry loop in
    // secure_key_release::request_vmgs_encryption_keys. Otherwise, the test will stuck
    // on the timer.sleep() as the the driver is not progressed.
    initialize_platform_security(
        &get_pair.client,
        bios_guid,
        &att_cfg,
        &mut vmgs,
        Some(&tee),
        false,
        ldriver,
        GuestStateEncryptionPolicy::Auto,
        true,
    )
    .await
    .unwrap();

    // VMGS should remain encrypted
    assert!(vmgs.encrypted());
}

#[async_test]
async fn init_sec_secure_key_release_skip_hw_unsealing(driver: DefaultDriver) {
    let mut vmgs = new_formatted_vmgs().await;

    // IGVM attest is required
    // KEY_RELEASE succeeds on first boot, fails with skip_hw_unsealing on second boot.
    // WRAPPED_KEY is not in the plan, so it falls back to default (success) every time.
    let mut plan = IgvmAgentTestPlan::default();
    plan.insert(
        IgvmAttestRequestType::KEY_RELEASE_REQUEST,
        VecDeque::from([
            IgvmAgentAction::RespondSuccess,
            IgvmAgentAction::RespondFailureSkipHwUnsealing,
        ]),
    );

    let get_pair = new_test_get(driver, true, Some(plan)).await;

    let bios_guid = Guid::new_random();
    let att_cfg = Default::default();

    // Ensure VMGS is not encrypted and agent data is empty before the call
    assert!(!vmgs.encrypted());

    // Obtain a LocalDriver briefly, then run the async flow under the pool executor
    let tee = MockTeeCall::new(0x1234);
    let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
    let res = initialize_platform_security(
        &get_pair.client,
        bios_guid,
        &att_cfg,
        &mut vmgs,
        Some(&tee),
        false,
        ldriver.clone(),
        GuestStateEncryptionPolicy::Auto,
        true,
    )
    .await
    .unwrap();

    // VMGS is now encrypted and HWKP is updated.
    assert!(vmgs.encrypted());
    assert!(!hardware_key_protector_is_empty(&mut vmgs).await);
    // Agent data should be the same as `key_reference` in the WRAPPED_KEY response.
    let key_reference = serde_json::json!({
        "key_info": {
            "host": "name"
        },
        "attestation_info": {
            "host": "attestation_name"
        }
    });
    let key_reference = serde_json::to_string(&key_reference).unwrap();
    let key_reference = key_reference.as_bytes();
    let mut expected_agent_data = [0u8; AGENT_DATA_MAX_SIZE];
    expected_agent_data[..key_reference.len()].copy_from_slice(key_reference);
    assert_eq!(res.agent_data.unwrap(), expected_agent_data.to_vec());
    // Secure key should be None without pre-provisioning
    assert!(res.guest_secret_key.is_none());

    // Second call: KEY_RELEASE fails with skip_hw_unsealing signal.
    // The skip_hw_unsealing signal causes the hardware unsealing fallback to be
    // skipped, so VMGS unlock should fail.
    // NOTE: The test relies on the test GED to return failing KEY_RELEASE response
    // with retry recommendation as false so the retry loop terminates immediately.
    // Otherwise, the test will get stuck on timer.sleep() as the driver is not
    // progressed.
    let result = initialize_platform_security(
        &get_pair.client,
        bios_guid,
        &att_cfg,
        &mut vmgs,
        Some(&tee),
        false,
        ldriver,
        GuestStateEncryptionPolicy::Auto,
        true,
    )
    .await;

    assert!(result.is_err());
}

#[async_test]
async fn init_sec_secure_key_release_no_hw_sealing_backup(driver: DefaultDriver) {
    let mut vmgs = new_formatted_vmgs().await;

    // IGVM attest is required
    let mut plan = IgvmAgentTestPlan::default();
    plan.insert(
        IgvmAttestRequestType::WRAPPED_KEY_REQUEST,
        VecDeque::from([
            IgvmAgentAction::RespondSuccess,
            // initialize_platform_security will attempt SKR/unlock 10 times
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
            IgvmAgentAction::RespondFailure,
        ]),
    );

    let get_pair = new_test_get(driver, true, Some(plan)).await;

    let bios_guid = Guid::new_random();
    let att_cfg = Default::default();
    // Without hardware sealing support
    let tee = MockTeeCallNoGetDerivedKey {};

    // Ensure VMGS is not encrypted and agent data is empty before the call
    assert!(!vmgs.encrypted());

    // Obtain a LocalDriver briefly, then run the async flow under the pool executor
    let ldriver = pal_async::local::block_with_io(|ld| async move { ld });
    let res = initialize_platform_security(
        &get_pair.client,
        bios_guid,
        &att_cfg,
        &mut vmgs,
        Some(&tee),
        false,
        ldriver.clone(),
        GuestStateEncryptionPolicy::Auto,
        true,
    )
    .await
    .unwrap();

    // VMGS is now encrypted but HWKP remains empty.
    assert!(vmgs.encrypted());
    assert!(hardware_key_protector_is_empty(&mut vmgs).await);
    // Agent data should be the same as `key_reference` in the WRAPPED_KEY response.
    // See vm/devices/get/guest_emulation_device/src/test_igvm_agent.rs for the expected response.
    let key_reference = serde_json::json!({
        "key_info": {
            "host": "name"
        },
        "attestation_info": {
            "host": "attestation_name"
        }
    });
    let key_reference = serde_json::to_string(&key_reference).unwrap();
    let key_reference = key_reference.as_bytes();
    let mut expected_agent_data = [0u8; AGENT_DATA_MAX_SIZE];
    expected_agent_data[..key_reference.len()].copy_from_slice(key_reference);
    assert_eq!(res.agent_data.unwrap(), expected_agent_data.to_vec());
    // Secure key should be None without pre-provisioning
    assert!(res.guest_secret_key.is_none());

    // Second call: VMGS unlock should fail without hardware sealing support
    let result = initialize_platform_security(
        &get_pair.client,
        bios_guid,
        &att_cfg,
        &mut vmgs,
        Some(&tee),
        false,
        ldriver,
        GuestStateEncryptionPolicy::Auto,
        true,
    )
    .await;

    assert!(result.is_err());
}

#[test]
fn test_get_provenance_claims() {
    // Test JWT: not a valid credential or secret for anything.
    const PROVENANCE_DOC: &str = include_str!("../test_data/valid_jwt");
    let doc = PROVENANCE_DOC.trim().strip_prefix("placeholder_").unwrap();
    let claims = get_provenance_claims(doc.as_bytes()).unwrap();
    assert_eq!(
        claims.id,
        guid::guid!("03020100-0504-0706-0809-0a0b0c0d0e0f")
    );
    assert_eq!(
        claims.signer,
        "did:x509:0:sha256:ea76599d86897382aa519ff2bc0fa6b9c15d60da2ebe53e72139cd317b0797ed:subject:fican.cvmprovisioningservice.core.azure-test.net"
    );
}

#[test]
fn test_derive_vmgsid() {
    const SEED_DOC_1: &str = "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F,4C6162656C5F435053,436F6E746578745F564D4753,32";
    const SEED_DOC_2: &str = "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F,4C6162656C5F435053,436F6E746578745F564D4753";
    const SEED_DOC_3: &str = "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F,4C6162656C5F435053,436F6E746578745F564D4753,32,ABCDEF";
    const GUID: Guid = guid::guid!("b0587f2d-11e6-9f66-1af4-8b4a619147c8");

    let vmgsid1 = derive_vmgsid(SEED_DOC_1.as_bytes()).unwrap();
    assert_eq!(vmgsid1, GUID);

    let vmgsid2 = derive_vmgsid(SEED_DOC_2.as_bytes()).unwrap();
    assert_eq!(vmgsid2, GUID);

    let vmgsid3 = derive_vmgsid(SEED_DOC_3.as_bytes()).unwrap();
    assert_eq!(vmgsid3, GUID);
}
