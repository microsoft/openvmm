// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Crash-consistency substrate tests.
//!
//! These tests intentionally stay below the guest-visible read/write and trim
//! APIs. They exercise the already-landed cache, log, BAT, flush, and replay
//! primitives that later I/O code relies on for durability ordering.

use crate::AsyncFile;
use crate::AsyncFileExt;
use crate::bat::BlockMapping;
use crate::bat::BlockType;
use crate::cache::CacheLogState;
use crate::cache::PAGE_SIZE;
use crate::cache::PageCache;
use crate::cache::PageKey;
use crate::cache::WriteMode;
use crate::flush::FlushSequencer;
use crate::flush::Fsn;
use crate::format;
use crate::format::BatEntryState;
use crate::log::DataPage;
use crate::log::LogRegion;
use crate::log::LogWriter;
use crate::log::replay_log;
use crate::log_permits::LogPermits;
use crate::log_task::LogData;
use crate::log_task::LogRequest;
use crate::log_task::LogTask;
use crate::log_task::Lsn;
use crate::log_task::Transaction;
use crate::lsn_watermark::LsnWatermark;
use crate::open::FailureFlag;
use crate::open::VhdxFile;
use crate::tests::support::CrashFileOp;
use crate::tests::support::CrashTestFile;
use crate::tests::support::InMemoryFile;
use mesh::rpc::RpcSend;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::task::Spawn;
use std::sync::Arc;

const TEST_LOG_OFFSET: u64 = format::MB1;
const TEST_LOG_SIZE: u32 = format::MB1 as u32;
const TEST_FILE_SIZE: u64 = 8 * format::MB1;
const TARGET_OFFSET: u64 = 4 * format::MB1;

fn test_region() -> LogRegion {
    LogRegion {
        file_offset: TEST_LOG_OFFSET,
        length: TEST_LOG_SIZE,
    }
}

fn test_guid() -> guid::Guid {
    guid::guid!("12345678-1234-1234-1234-123456789abc")
}

struct Pipeline {
    tx: mesh::Sender<LogRequest<Vec<u8>>>,
    file: Arc<CrashTestFile>,
    permits: Arc<LogPermits>,
    logged_lsn: Arc<LsnWatermark>,
    applied_lsn: Arc<LsnWatermark>,
    log_task: pal_async::task::Task<()>,
    apply_task: pal_async::task::Task<()>,
}

async fn setup_pipeline(driver: &DefaultDriver) -> Pipeline {
    let file = Arc::new(CrashTestFile::from_durable(vec![
        0;
        TEST_FILE_SIZE as usize
    ]));
    let region = test_region();
    let writer = LogWriter::initialize(file.as_ref(), region, test_guid(), TEST_FILE_SIZE)
        .await
        .unwrap();

    file.clear_operations();

    let flush_sequencer = Arc::new(FlushSequencer::new());
    let permits = Arc::new(LogPermits::new(100));
    let logged_lsn = Arc::new(LsnWatermark::new());
    let applied_lsn = Arc::new(LsnWatermark::new());
    let failure_flag = Arc::new(FailureFlag::new());
    let (apply_tx, apply_rx) = mesh::channel();
    let (tx, rx) = mesh::channel();

    let apply_task = driver.spawn(
        "vhdx-crash-test-apply",
        crate::apply_task::run_apply_task(
            apply_rx,
            file.clone(),
            flush_sequencer.clone(),
            applied_lsn.clone(),
            permits.clone(),
            failure_flag.clone(),
        ),
    );
    let log_task = driver.spawn(
        "vhdx-crash-test-log",
        LogTask::new(
            file.clone(),
            writer,
            flush_sequencer,
            permits.clone(),
            logged_lsn.clone(),
            applied_lsn.clone(),
            apply_tx,
            failure_flag,
        )
        .run(rx),
    );

    Pipeline {
        tx,
        file,
        permits,
        logged_lsn,
        applied_lsn,
        log_task,
        apply_task,
    }
}

async fn finish_pipeline(pipeline: Pipeline) {
    pipeline
        .tx
        .call(LogRequest::<Vec<u8>>::Close, ())
        .await
        .unwrap()
        .unwrap();
    pipeline.log_task.await;
    pipeline.apply_task.await;
}

fn first_wal_write_index(ops: &[CrashFileOp]) -> usize {
    ops.iter()
        .position(|op| {
            matches!(
                op,
                CrashFileOp::Write { offset, .. }
                    if (TEST_LOG_OFFSET..TEST_LOG_OFFSET + TEST_LOG_SIZE as u64).contains(offset)
            )
        })
        .expect("WAL write should be recorded")
}

#[async_test]
async fn pre_log_fsn_flushes_before_wal_write(driver: DefaultDriver) {
    let pipeline = setup_pipeline(&driver).await;
    let data = Arc::new(vec![0x5a; PAGE_SIZE]);

    pipeline.permits.acquire(1).await.unwrap();
    pipeline.tx.send(LogRequest::Commit(Transaction {
        lsn: Lsn::new(1),
        data: vec![LogData::new(TARGET_OFFSET, data)],
        pre_log_fsn: Some(Fsn::new(1)),
    }));

    pipeline.logged_lsn.wait_for(Lsn::new(1)).await.unwrap();
    pipeline.applied_lsn.wait_for(Lsn::new(1)).await.unwrap();

    let ops = pipeline.file.operations();
    let wal_index = first_wal_write_index(&ops);
    let flush_index = ops
        .iter()
        .position(|op| matches!(op, CrashFileOp::Flush))
        .expect("pre-log FSN should force a flush");
    assert!(
        flush_index < wal_index,
        "flush must occur before WAL write when pre_log_fsn is set: {ops:?}"
    );

    finish_pipeline(pipeline).await;
}

#[async_test]
async fn wal_write_without_pre_log_fsn_does_not_force_flush(driver: DefaultDriver) {
    let pipeline = setup_pipeline(&driver).await;
    let data = Arc::new(vec![0xa5; PAGE_SIZE]);

    pipeline.permits.acquire(1).await.unwrap();
    pipeline.tx.send(LogRequest::Commit(Transaction {
        lsn: Lsn::new(1),
        data: vec![LogData::new(TARGET_OFFSET, data)],
        pre_log_fsn: None,
    }));

    pipeline.logged_lsn.wait_for(Lsn::new(1)).await.unwrap();

    let ops = pipeline.file.operations();
    let wal_index = first_wal_write_index(&ops);
    assert!(
        !ops[..wal_index]
            .iter()
            .any(|op| matches!(op, CrashFileOp::Flush)),
        "WAL write should not need an earlier flush without pre_log_fsn: {ops:?}"
    );

    pipeline.applied_lsn.wait_for(Lsn::new(1)).await.unwrap();
    finish_pipeline(pipeline).await;
}

#[async_test]
async fn cache_commit_uses_max_pre_log_fsn() {
    let (tx, mut rx) = mesh::channel::<LogRequest<Vec<u8>>>();
    let permits = Arc::new(LogPermits::new(100));
    let mut cache = PageCache::new(
        Arc::new(InMemoryFile::new((PAGE_SIZE * 2) as u64)),
        Some(crate::log_task::LogClient::new(tx)),
        Some(CacheLogState {
            permits,
            applied_lsn: Arc::new(LsnWatermark::new()),
        }),
        0,
    );
    cache.register_tag(0, 0);

    let mut first = cache
        .acquire_write(PageKey { tag: 0, offset: 0 }, WriteMode::Overwrite)
        .await
        .unwrap();
    first.fill(0x11);
    first.set_pre_log_fsn(Fsn::new(2));
    drop(first);

    let mut second = cache
        .acquire_write(
            PageKey {
                tag: 0,
                offset: PAGE_SIZE as u64,
            },
            WriteMode::Overwrite,
        )
        .await
        .unwrap();
    second.fill(0x22);
    second.set_pre_log_fsn(Fsn::new(5));
    drop(second);

    cache.commit().unwrap();

    match rx.recv().await.unwrap() {
        LogRequest::Commit(txn) => {
            assert_eq!(txn.data.len(), 2);
            assert_eq!(txn.pre_log_fsn, Some(Fsn::new(5)));
        }
        _ => panic!("expected Commit"),
    }
}

#[async_test]
async fn bat_mapping_stamps_pre_log_fsn(driver: DefaultDriver) {
    let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
    let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
    let key = vhdx.bat_page_key_for_block(0);
    let mapping = BlockMapping::new()
        .with_bat_state(BatEntryState::FullyPresent)
        .with_file_megabyte(4);

    vhdx.bat
        .write_block_mapping(
            &vhdx.cache,
            BlockType::Payload,
            0,
            mapping,
            Some(Fsn::new(7)),
        )
        .await
        .unwrap();

    assert_eq!(vhdx.cache.get_pre_log_fsn(key), Some(Fsn::new(7)));
    vhdx.abort().await;
}

#[async_test]
async fn replay_after_apply_is_idempotent() {
    let file = InMemoryFile::new(TEST_FILE_SIZE);
    let region = test_region();
    let guid = test_guid();
    let mut writer = LogWriter::initialize(&file, region.clone(), guid, TEST_FILE_SIZE)
        .await
        .unwrap();
    let page = [0x6d; PAGE_SIZE];

    writer
        .write_entry(
            &file,
            &[DataPage {
                file_offset: TARGET_OFFSET,
                payload: &page,
            }],
            &[],
        )
        .await
        .unwrap();
    file.write_at(TARGET_OFFSET, &page).await.unwrap();
    file.flush().await.unwrap();

    let durable = file.snapshot();
    let recovered = InMemoryFile::from_snapshot(durable.clone());
    let first = replay_log(&recovered, &region, guid).await.unwrap();
    assert!(first.replayed);

    let mut buf = vec![0; PAGE_SIZE];
    recovered.read_at(TARGET_OFFSET, &mut buf).await.unwrap();
    assert_eq!(&buf, &page);

    let recovered_again = InMemoryFile::from_snapshot(durable);
    let second = replay_log(&recovered_again, &region, guid).await.unwrap();
    assert!(second.replayed);
    let mut buf = vec![0; PAGE_SIZE];
    recovered_again
        .read_at(TARGET_OFFSET, &mut buf)
        .await
        .unwrap();
    assert_eq!(&buf, &page);
}
