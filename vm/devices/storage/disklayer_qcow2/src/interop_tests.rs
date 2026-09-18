// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interop tests that exercise the qcow2 disk layer against the real QEMU
//! tooling (`qemu-img` and `qemu-io`) so the two independent implementations
//! validate each other's on-disk format.
//!
//! QEMU writes an image that this layer must read back byte-for-byte, and this
//! layer writes an image that `qemu-img check` must validate and `qemu-img
//! convert` must read back byte-for-byte.
//!
//! The tests require the QEMU tools to be present on the host. CI installs
//! them on Linux via the `qemu-utils` package; when they cannot be found the
//! tests skip themselves with a note.

use crate::Qcow2Layer;
use disk_backend::Disk;
use disk_layered::DiskLayer;
use disk_layered::LayerConfiguration;
use disk_layered::LayeredDisk;
use guestmem::GuestMemory;
use pal_async::async_test;
use scsi_buffers::OwnedRequestBuffers;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

/// Locate a QEMU host tool (e.g. `qemu-img`).
///
/// The path is taken from the `OPENVMM_TEST_<NAME>` environment variable when
/// set -- the escape hatch for flowey-owned test infrastructure to hand the
/// tests a specific binary -- and otherwise looked up on `$PATH`. Returns
/// `None` when the tool cannot be found.
fn qemu_tool(name: &str) -> Option<PathBuf> {
    let env_key = format!(
        "OPENVMM_TEST_{}",
        name.replace('-', "_").to_ascii_uppercase()
    );
    if let Some(path) = std::env::var_os(&env_key) {
        return Some(PathBuf::from(path));
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn get_qemu_img_from_test_artifacts() -> Option<PathBuf> {
    qemu_tool("qemu-img")
}

fn get_qemu_io_from_test_artifacts() -> Option<PathBuf> {
    qemu_tool("qemu-io")
}

/// Run `cmd` and assert that it exits successfully, returning its output.
fn run_qemu(cmd: &mut Command) -> Output {
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{cmd:?} exited with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

/// Create a 1 MiB qcow2 image with 4 KiB clusters using `qemu-img create`.
fn qemu_create_image(qemu_img: &Path, path: &Path) {
    run_qemu(
        Command::new(qemu_img)
            .args(["create", "-f", "qcow2", "-o", "cluster_size=4096"])
            .arg(path)
            .arg("1M"),
    );
}

/// Open `path` as a `Disk` backed by this crate's qcow2 disk layer.
async fn open_disk(path: &Path, read_only: bool) -> Disk {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(!read_only)
        .open(path)
        .unwrap();
    let header = crate::header::Qcow2Header::from_file(&mut file).unwrap();
    let layer = Qcow2Layer::new(file, header, read_only).unwrap();
    Disk::new(
        LayeredDisk::new(
            true,
            vec![LayerConfiguration {
                layer: DiskLayer::new(layer),
                write_through: false,
                read_cache: false,
            }],
        )
        .await
        .unwrap(),
    )
    .unwrap()
}

/// QEMU writes an image (via `qemu-io`), and this layer reads it back
/// byte-for-byte.
#[async_test]
async fn qemu_written_image_reads_back() {
    let Some((qemu_img, qemu_io)) =
        get_qemu_img_from_test_artifacts().zip(get_qemu_io_from_test_artifacts())
    else {
        eprintln!("skipping qemu_written_image_reads_back: qemu-img/qemu-io not available");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("qemu.qcow2");
    qemu_create_image(&qemu_img, &path);

    // Write 0xAB across the whole 1 MiB virtual disk.
    run_qemu(
        Command::new(&qemu_io)
            .args(["-f", "qcow2", "-c", "write -P 0xAB 0 1048576"])
            .arg(&path),
    );

    let disk = open_disk(&path, true).await;
    assert_eq!(disk.sector_count(), 2048);

    let mem = GuestMemory::allocate(512);
    let owned = OwnedRequestBuffers::linear(0, 512, true);
    let mut buf = vec![0u8; 512];
    for sector in [0, 1000, 2047] {
        disk.read_vectored(&owned.buffer(&mem), sector)
            .await
            .unwrap();
        mem.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, vec![0xAB; 512], "sector {sector}");
    }
}

/// This layer writes an image that `qemu-img check` validates and
/// `qemu-img convert` reads back byte-for-byte.
#[async_test]
async fn qcow2_written_image_checks_with_qemu() {
    let Some(qemu_img) = get_qemu_img_from_test_artifacts() else {
        eprintln!("skipping qcow2_written_image_checks_with_qemu: qemu-img not available");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("qcow2.qcow2");
    qemu_create_image(&qemu_img, &path);

    // Write 0xAB into sector 0 (cluster 0) and 0xCD into sector 8 (cluster 1),
    // forcing both an in-place allocation and a fresh-cluster allocation.
    let disk = open_disk(&path, false).await;
    let mem = GuestMemory::allocate(512);
    let owned = OwnedRequestBuffers::linear(0, 512, true);
    mem.write_at(0, &[0xAB; 512]).unwrap();
    disk.write_vectored(&owned.buffer(&mem), 0, false)
        .await
        .unwrap();
    mem.write_at(0, &[0xCD; 512]).unwrap();
    disk.write_vectored(&owned.buffer(&mem), 8, false)
        .await
        .unwrap();
    drop(disk);

    let out = run_qemu(
        Command::new(&qemu_img)
            .args(["check", "-f", "qcow2"])
            .arg(&path),
    );
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.contains("No errors were found"),
        "unexpected qemu-img check output: {out}"
    );

    let raw_path = dir.path().join("qcow2.raw");
    run_qemu(
        Command::new(&qemu_img)
            .args(["convert", "-f", "qcow2", "-O", "raw"])
            .arg(&path)
            .arg(&raw_path),
    );

    let raw = std::fs::read(&raw_path).unwrap();
    assert_eq!(raw.len(), 1024 * 1024);
    assert_eq!(&raw[0..512], &[0xAB; 512]);
    assert!(raw[512..4096].iter().all(|&b| b == 0));
    assert_eq!(&raw[4096..4608], &[0xCD; 512]);
    assert!(raw[4608..].iter().all(|&b| b == 0));
}
