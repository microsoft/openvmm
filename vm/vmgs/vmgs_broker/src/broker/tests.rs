// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::VmgsClientError;
use crate::spawn_vmgs_broker;
use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use mesh::rpc::RpcSend;
use pal_async::DefaultDriver;
use pal_async::async_test;
use parking_lot::Mutex;
use scsi_buffers::RequestBuffers;
use std::sync::Arc;
use test_with_tracing::test;
use vmgs_format::EncryptionAlgorithm;

#[derive(Debug, PartialEq, Eq)]
enum Operation {
    Write(u64),
    Flush,
}

#[derive(Default)]
struct IoState {
    operations: Vec<Operation>,
    flush_count: usize,
    fail_flush_at: Option<usize>,
}

/// A RAM disk decorator that records ordering without recording data or keys.
#[derive(inspect::Inspect)]
struct TestDisk {
    disk: Disk,
    #[inspect(skip)]
    io: Arc<Mutex<IoState>>,
}

impl DiskIo for TestDisk {
    fn disk_type(&self) -> &str {
        "vmgs-broker-test"
    }

    fn sector_count(&self) -> u64 {
        self.disk.sector_count()
    }

    fn sector_size(&self) -> u32 {
        self.disk.sector_size()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.disk.disk_id()
    }

    fn physical_sector_size(&self) -> u32 {
        self.disk.physical_sector_size()
    }

    fn is_fua_respected(&self) -> bool {
        self.disk.is_fua_respected()
    }

    fn is_read_only(&self) -> bool {
        self.disk.is_read_only()
    }

    async fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
    ) -> Result<(), DiskError> {
        self.disk.unmap(sector, count, block_level_only).await
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        self.disk.unmap_behavior()
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        self.disk.read_vectored(buffers, sector).await
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.io.lock().operations.push(Operation::Write(sector));
        self.disk.write_vectored(buffers, sector, fua).await
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        {
            let mut io = self.io.lock();
            io.operations.push(Operation::Flush);
            io.flush_count += 1;
            if io.fail_flush_at == Some(io.flush_count) {
                return Err(DiskError::Io(std::io::Error::other(
                    "injected flush failure",
                )));
            }
        }
        self.disk.sync_cache().await
    }
}

fn new_test_disk() -> (Disk, Arc<Mutex<IoState>>) {
    let io = Arc::new(Mutex::new(IoState::default()));
    let disk = Disk::new(TestDisk {
        disk: disklayer_ram::ram_disk(4 * 1024 * 1024, false).unwrap(),
        io: io.clone(),
    })
    .unwrap();
    (disk, io)
}

async fn encrypted_vmgs(disk: Disk, key: &[u8; 32]) -> Vmgs {
    let mut vmgs = Vmgs::format_new(disk, None).await.unwrap();
    vmgs.update_encryption_key(key, EncryptionAlgorithm::AES_GCM)
        .await
        .unwrap();
    vmgs
}

#[async_test]
async fn guarded_write_matches_and_flushes_header(driver: DefaultDriver) {
    let (disk, io) = new_test_disk();
    let key = [1; 32];
    let vmgs = encrypted_vmgs(disk.clone(), &key).await;
    let (client, task) = spawn_vmgs_broker(driver, vmgs);
    assert!(client.active_encryption_key().await.unwrap() == key);
    *io.lock() = IoState::default();

    assert!(
        client
            .write_file_if_encryption_key_matches(FileId::ATTEST, b"sealed candidate".to_vec(), key)
            .await
            .unwrap()
    );
    let saved = client.save().await.unwrap();
    // Each header occupies one sector (not one VMGS block).
    let header_sector = saved.active_header_index as u64;
    {
        let io = io.lock();
        assert_eq!(io.flush_count, 2);
        assert!(io.operations.ends_with(&[
            Operation::Flush,
            Operation::Write(header_sector),
            Operation::Flush,
        ]));
    }
    assert!(
        !client
            .get_file_info(FileId::ATTEST)
            .await
            .unwrap()
            .encrypted
    );
    assert!(client.active_encryption_key().await.unwrap() == key);
    assert_eq!(
        client.read_file(FileId::ATTEST).await.unwrap(),
        b"sealed candidate"
    );
    drop(client);
    task.await;

    let mut reopened = Vmgs::open(disk, None).await.unwrap();
    // The guarded API writes plaintext even though the store is encrypted.
    assert_eq!(
        reopened.read_file(FileId::ATTEST).await.unwrap(),
        b"sealed candidate"
    );
    reopened.unlock_with_encryption_key(&key).await.unwrap();
    assert!(reopened.active_encryption_key().unwrap() == &key);
}

#[async_test]
async fn guarded_write_mismatch_has_no_side_effects(driver: DefaultDriver) {
    let (disk, io) = new_test_disk();
    let key = [1; 32];
    let mut vmgs = encrypted_vmgs(disk, &key).await;
    vmgs.write_file(FileId::ATTEST, b"current candidate")
        .await
        .unwrap();
    let (client, task) = spawn_vmgs_broker(driver, vmgs);
    let before = client.save().await.unwrap();
    *io.lock() = IoState::default();

    // Exercise differences at both ends of the key and an empty key. None is
    // a matching prefix or a request to disable the guard.
    for index in [0, 31] {
        let mut stale_key = key;
        stale_key[index] ^= 1;
        assert!(
            !client
                .write_file_if_encryption_key_matches(FileId::ATTEST, b"stale".to_vec(), stale_key)
                .await
                .unwrap()
        );
    }
    assert!(
        !client
            .write_file_if_encryption_key_matches(FileId::TPM_PPI, b"stale".to_vec(), [0; 32])
            .await
            .unwrap()
    );
    assert!(io.lock().operations.is_empty());
    assert_eq!(
        client.read_file(FileId::ATTEST).await.unwrap(),
        b"current candidate"
    );
    assert!(matches!(
        client.get_file_info(FileId::TPM_PPI).await,
        Err(VmgsClientError::Vmgs(VmgsBrokerError::FileInfoNotAllocated))
    ));
    let after = client.save().await.unwrap();
    assert_eq!(
        before.active_header_sequence_number,
        after.active_header_sequence_number
    );
    assert_eq!(before.active_header_index, after.active_header_index);
    assert!(client.active_encryption_key().await.unwrap() == key);
    drop(client);
    task.await;
}

#[async_test]
async fn guarded_write_rechecks_key_after_rotation() {
    let (disk, io) = new_test_disk();
    let old_key = [1; 32];
    let new_key = [2; 32];
    let vmgs = encrypted_vmgs(disk.clone(), &old_key).await;
    let mut broker = VmgsBrokerTask::new(vmgs);
    let (send, mut recv) = mesh::mpsc_channel();

    let result = send.call_failable(VmgsBrokerRpc::ActiveEncryptionKey, ());
    broker.process_message(recv.recv().await.unwrap()).await;
    let candidate_key = result.await.unwrap();
    assert!(candidate_key == old_key);

    // Rotate after reading the key, before publishing its candidate. Keep this
    // test-only access private rather than adding a rotation RPC to the API.
    broker
        .vmgs
        .update_encryption_key(&new_key, EncryptionAlgorithm::AES_GCM)
        .await
        .unwrap();
    broker
        .vmgs
        .write_file(FileId::ATTEST, b"new key candidate")
        .await
        .unwrap();
    *io.lock() = IoState::default();
    let result = send.call_failable(
        VmgsBrokerRpc::WriteFileIfEncryptionKeyMatches,
        (
            FileId::ATTEST.into(),
            b"old key candidate".to_vec(),
            candidate_key,
        ),
    );
    broker.process_message(recv.recv().await.unwrap()).await;
    assert!(!result.await.unwrap());
    assert!(io.lock().operations.is_empty());
    assert!(broker.vmgs.active_encryption_key().unwrap() == &new_key);
    assert_eq!(
        broker.vmgs.read_file(FileId::ATTEST).await.unwrap(),
        b"new key candidate"
    );

    let result = send.call_failable(
        VmgsBrokerRpc::WriteFileIfEncryptionKeyMatches,
        (
            FileId::ATTEST.into(),
            b"replacement candidate".to_vec(),
            new_key,
        ),
    );
    broker.process_message(recv.recv().await.unwrap()).await;
    assert!(result.await.unwrap());
    drop(broker);
    let mut reopened = Vmgs::open(disk, None).await.unwrap();
    reopened.unlock_with_encryption_key(&new_key).await.unwrap();
    assert!(reopened.active_encryption_key().unwrap() == &new_key);
    assert_eq!(
        reopened.read_file(FileId::ATTEST).await.unwrap(),
        b"replacement candidate"
    );
}

#[async_test]
async fn guarded_write_rejects_plaintext_and_locked_stores(driver: DefaultDriver) {
    for encrypted in [false, true] {
        let (disk, io) = new_test_disk();
        let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
        if encrypted {
            vmgs.update_encryption_key(&[1; 32], EncryptionAlgorithm::AES_GCM)
                .await
                .unwrap();
        }
        drop(vmgs);
        let vmgs = Vmgs::open(disk, None).await.unwrap();
        let (client, task) = spawn_vmgs_broker(driver.clone(), vmgs);
        *io.lock() = IoState::default();
        assert!(matches!(
            client.active_encryption_key().await,
            Err(VmgsClientError::Vmgs(_))
        ));
        assert!(matches!(
            client
                .write_file_if_encryption_key_matches(FileId::ATTEST, vec![1], [1; 32])
                .await,
            Err(VmgsClientError::Vmgs(_))
        ));
        assert!(io.lock().operations.is_empty());
        drop(client);
        task.await;
    }
}

#[async_test]
async fn guarded_write_preserves_encrypted_file(driver: DefaultDriver) {
    let (disk, io) = new_test_disk();
    let key = [1; 32];
    let mut vmgs = encrypted_vmgs(disk, &key).await;
    vmgs.write_file_encrypted(FileId::BIOS_NVRAM, b"protected")
        .await
        .unwrap();
    let (client, task) = spawn_vmgs_broker(driver, vmgs);
    *io.lock() = IoState::default();
    assert!(matches!(
        client
            .write_file_if_encryption_key_matches(FileId::BIOS_NVRAM, b"plaintext".to_vec(), key)
            .await,
        Err(VmgsClientError::Vmgs(_))
    ));
    assert!(io.lock().operations.is_empty());
    assert!(client.active_encryption_key().await.unwrap() == key);
    assert!(
        client
            .get_file_info(FileId::BIOS_NVRAM)
            .await
            .unwrap()
            .encrypted
    );
    assert_eq!(
        client.read_file(FileId::BIOS_NVRAM).await.unwrap(),
        b"protected"
    );
    drop(client);
    task.await;
}

#[async_test]
async fn guarded_write_propagates_final_flush_error(driver: DefaultDriver) {
    let (disk, io) = new_test_disk();
    let key = [1; 32];
    let vmgs = encrypted_vmgs(disk, &key).await;
    let (client, task) = spawn_vmgs_broker(driver, vmgs);
    *io.lock() = IoState {
        // The first flush precedes the header; fail the extra final flush.
        fail_flush_at: Some(2),
        ..Default::default()
    };
    let result = client
        .write_file_if_encryption_key_matches(FileId::ATTEST, b"candidate".to_vec(), key)
        .await;
    let Err(VmgsClientError::Vmgs(VmgsBrokerError::Other(error))) = result else {
        panic!("final flush failure must be returned as a VMGS error");
    };
    assert!(error.to_string().contains("Error flushing the disk"));
    assert_eq!(io.lock().flush_count, 2);
    assert!(client.active_encryption_key().await.unwrap() == key);
    // A flush failure is not a rollback: the write already completed in memory.
    assert_eq!(
        client.read_file(FileId::ATTEST).await.unwrap(),
        b"candidate"
    );
    drop(client);
    task.await;
}
