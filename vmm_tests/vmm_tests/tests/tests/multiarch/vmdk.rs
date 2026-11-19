// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end tests for flat VMDK disks.

use anyhow::Context as _;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_helpers::disk::OpenDiskOptions;
use openvmm_helpers::disk::open_disk_type;
use pal_async::DefaultDriver;
use petri::PetriTestParams;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::PipetteClient;
use petri::pipette::cmd;
use std::path::Path;
use std::path::PathBuf;
use virtio_resources::VirtioPciDeviceHandle;
use virtio_resources::blk::VirtioBlkHandle;
use vm_resource::IntoResource as _;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;
use vmm_test_macros::openvmm_test;

// Sector size used by VMDK extents and guest read commands.
const VMDK_SECTOR_SIZE: usize = 512;
// Number of distinct source files used while staying below the mesh handle limit.
const VMDK_SOURCE_COUNT: usize = 60;
// Relative directory containing the generated source files.
const VMDK_SOURCE_DIRECTORY_NAME: &str = "source files";
// Byte pattern written to sector zero of every generated source.
const SOURCE_FILL_BYTE: u8 = 0xa5;
// Serial assigned to the test disk for guest-side identification.
const VMDK_SERIAL: &str = "petri-vmdk";

// Maximum guest polling attempts while waiting for the virtio-blk device.
const DEVICE_DISCOVERY_ATTEMPTS: usize = 20;
// Expected Linux name of the first virtio-blk disk.
const GUEST_BLOCK_DEVICE_NAME: &str = "vda";
// Guest device path corresponding to `GUEST_BLOCK_DEVICE_NAME`.
const GUEST_BLOCK_DEVICE_PATH: &str = "/dev/vda";
// Guest sysfs directory inspected while waiting for the block device.
const GUEST_SYS_BLOCK_DIRECTORY: &str = "/sys/block";
// Linux special file used to test rejection of non-regular sources.
const LINUX_NULL_DEVICE_PATH: &str = "/dev/null";
// Guest temporary file containing bytes expected from concurrent reads.
const GUEST_EXPECTED_DATA_PATH: &str = "/tmp/petri-vmdk-expected.bin";

// Number of guest workers issuing reads concurrently.
const CONCURRENT_READ_WORKER_COUNT: usize = 32;
// Number of reads issued sequentially by each guest worker.
const CONCURRENT_READS_PER_WORKER: usize = 16;

/// Host files, resource handle, and expected guest data for the VM test.
struct VmdkFixture {
    /// Keeps the descriptor and source files alive for the test duration.
    directory: tempfile::TempDir,
    /// Disk resource attached to the VM.
    disk: Resource<DiskHandleKind>,
    /// Bytes expected from a full guest disk read.
    expected: Vec<u8>,
    /// Number of virtual sectors exposed to the guest.
    sector_count: usize,
}

/// Builds a descriptor with alternating `RW` and `RDONLY` extents.
///
/// Every source file contains the same `0xa5` pattern in sector 0 and a
/// source-specific marker in sector 1. The descriptor maps sector 1 from every
/// source. It also maps sector 0 from the first source, verifying that two
/// extents can read different offsets from the same file:
///
/// ```text
/// RW 1 FLAT "source files/source 00.bin" 0
/// RW 1 FLAT "source files/source 00.bin" 1
/// RDONLY 1 FLAT "source files/source 01.bin" 1
/// ```
fn create_flat_vmdk() -> anyhow::Result<VmdkFixture> {
    // Create an isolated descriptor directory with a nested source directory.
    let directory = tempfile::tempdir().context("failed to create VMDK directory")?;
    let first_source_file_name = "source 00.bin";
    let descriptor_file_name = "disk.vmdk";

    let sources = directory.path().join(VMDK_SOURCE_DIRECTORY_NAME);
    std::fs::create_dir(&sources).context("failed to create VMDK source directory")?;

    // Reference the first source twice at different offsets. The resource
    // opener should reuse one host file handle for both extents.
    let mut extents =
        format!("RW 1 FLAT \"{VMDK_SOURCE_DIRECTORY_NAME}/{first_source_file_name}\" 0\n");
    let mut expected = vec![SOURCE_FILL_BYTE; VMDK_SECTOR_SIZE];

    // Generate every source file, its expected guest-visible bytes, and its
    // corresponding descriptor extent.
    for index in 0..VMDK_SOURCE_COUNT {
        let marker = u8::try_from(index + 1).unwrap();
        let name = format!("source {index:02}.bin");
        let relative_source_path = format!("{VMDK_SOURCE_DIRECTORY_NAME}/{name}");
        let host_source_path = sources.join(&name);

        // Sector zero is deliberately different so the guest read also checks
        // that the descriptor's source offset is honored.
        let mut contents = vec![SOURCE_FILL_BYTE; VMDK_SECTOR_SIZE];

        // Leave the final referenced sector one byte long; all earlier source
        // sectors contain a complete marker pattern.
        let source_sector_byte_count = if index + 1 == VMDK_SOURCE_COUNT {
            1
        } else {
            VMDK_SECTOR_SIZE
        };
        contents.extend(std::iter::repeat_n(marker, source_sector_byte_count));

        // Model the backend's zero padding for bytes beyond the source file.
        let mut expected_sector = [0; VMDK_SECTOR_SIZE];
        expected_sector[..source_sector_byte_count].fill(marker);
        expected.extend(expected_sector);

        // Write the file on the host.
        std::fs::write(&host_source_path, contents)
            .with_context(|| format!("failed to write VMDK source {index}"))?;

        // Alternate both supported descriptor access modes across extents.
        let access = if index.is_multiple_of(2) {
            "RW"
        } else {
            "RDONLY"
        };
        extents.push_str(&format!("{access} 1 FLAT \"{relative_source_path}\" 1\n"));
    }

    // Serialize the complete split-flat descriptor around the generated extent
    // list.
    let descriptor = directory.path().join(descriptor_file_name);
    std::fs::write(
        &descriptor,
        format!(
            "# Disk DescriptorFile\n\
             version=1\n\
             CID=fffffffe\n\
             parentCID=ffffffff\n\
             createType=\"twoGbMaxExtentFlat\"\n\
             \n\
             # Extent description\n\
             {extents}\n\
             # The Disk Data Base\n\
             #DDB\n"
        ),
    )
    .context("failed to write VMDK descriptor")?;

    // Open the fixture through the production VMDK resource path.
    let disk = disk_vmdk::open_vmdk(&descriptor, true).context("failed to open VMDK")?;

    // Return the resource together with its expected guest-visible layout.
    Ok(VmdkFixture {
        directory,
        disk,
        expected,
        sector_count: VMDK_SOURCE_COUNT + 1,
    })
}

/// Writes a minimal descriptor used by host-side rejection tests.
fn write_flat_descriptor(
    directory: &tempfile::TempDir,
    extent_path: &str,
    sector_count: u64,
) -> anyhow::Result<PathBuf> {
    // Build a single-extent descriptor around the caller-provided source path.
    let descriptor_file_name = "test.vmdk";
    let descriptor = directory.path().join(descriptor_file_name);
    std::fs::write(
        &descriptor,
        format!(
            "# Disk DescriptorFile\n\
             version=1\n\
             parentCID=ffffffff\n\
             createType=\"twoGbMaxExtentFlat\"\n\
             # Extent description\n\
             RW {sector_count} FLAT \"{extent_path}\" 0\n"
        ),
    )?;
    Ok(descriptor)
}

/// Adds the supplied VMDK disk to the VM configuration as a read-only virtio-blk
/// device attached to a PCIe root port.
fn with_read_only_vmdk_disk(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    disk: Resource<DiskHandleKind>,
) -> PetriVmBuilder<OpenVmmPetriBackend> {
    config.modify_backend(move |backend| {
        backend
            .with_pcie_root_topology(1, 1, 1)
            .with_custom_config(move |config| {
                config.pcie_devices.push(PcieDeviceConfig {
                    port_name: "s0rc0rp0".into(),
                    resource: VirtioPciDeviceHandle(
                        VirtioBlkHandle {
                            disk,
                            read_only: true,
                            serial: Some(VMDK_SERIAL.into()),
                        }
                        .into_resource(),
                    )
                    .into_resource(),
                });
            })
    })
}

/// Waits for the guest's first virtio-blk disk to report the expected capacity.
///
/// Call after VM startup, before performing guest disk I/O. Polls
/// `/sys/block/vda/size` until it equals `sector_count` (in 512-byte sectors),
/// waiting one second between attempts. Returns `/dev/vda` for subsequent I/O.
///
/// # Errors
///
/// Returns an error if the device is missing or has the wrong capacity after
/// [`DEVICE_DISCOVERY_ATTEMPTS`] attempts, or if a retry delay command fails.
async fn wait_for_virtio_blk(agent: &PipetteClient, sector_count: usize) -> anyhow::Result<String> {
    let shell = agent.unix_shell();
    let size_path = format!("{GUEST_SYS_BLOCK_DIRECTORY}/{GUEST_BLOCK_DEVICE_NAME}/size");

    // Poll sysfs until the expected virtio-blk device and capacity appear.
    for attempt in 1..=DEVICE_DISCOVERY_ATTEMPTS {
        let size = cmd!(shell, "cat {size_path}").read().await;
        match size {
            Ok(size) if size.trim() == sector_count.to_string() => {
                return Ok(GUEST_BLOCK_DEVICE_PATH.into());
            }
            Err(error) if attempt < DEVICE_DISCOVERY_ATTEMPTS => {
                tracing::warn!(
                    attempt,
                    error = error.as_ref() as &dyn std::error::Error,
                    "VMDK devices are not ready"
                );
                cmd!(shell, "sleep 1").run().await?;
            }
            Ok(size) if attempt < DEVICE_DISCOVERY_ATTEMPTS => {
                tracing::warn!(
                    attempt,
                    actual_size = size.trim(),
                    sector_count,
                    "virtio-blk VMDK has the wrong size"
                );
                cmd!(shell, "sleep 1").run().await?;
            }
            Ok(size) => {
                anyhow::bail!(
                    "virtio-blk VMDK has {} sectors, expected {sector_count}",
                    size.trim()
                )
            }
            Err(error) => return Err(error).context("virtio-blk VMDK did not appear"),
        }
    }
    unreachable!()
}

/// Verifies that buffered and direct reads of the VMDK at `path` match `expected`.
///
/// Call after [`wait_for_virtio_blk`]. Both reads cover the whole disk without
/// assuming a particular extent count or layout.
async fn verify_extent_mapping(
    agent: &PipetteClient,
    path: &str,
    expected: &[u8],
) -> anyhow::Result<()> {
    // Read the complete virtual disk and compare its ordered extent contents.
    let contents = agent
        .read_file(path)
        .await
        .with_context(|| format!("failed to read {path}"))?;
    anyhow::ensure!(
        contents == expected,
        "VMDK data read from {path} did not match its ordered extents"
    );

    // Verify the full disk with direct I/O, bypassing the guest page cache.
    let shell = agent.unix_shell();
    let direct_read = cmd!(
        shell,
        "dd if={path} bs=512 iflag=direct,fullblock status=none"
    )
    .output()
    .await
    .context("failed to read VMDK with direct I/O")?;
    anyhow::ensure!(
        direct_read.stdout == expected,
        "direct VMDK read from {path} did not match expected data"
    );
    Ok(())
}

/// Verify that virtio-blk exposes the VMDK as write-protected.
async fn verify_read_only(agent: &PipetteClient, path: &str) -> anyhow::Result<()> {
    // Confirm that Linux reports the virtio-blk device as read-only.
    let device = path.rsplit('/').next().context("invalid guest disk path")?;
    let shell = agent.unix_shell();
    let read_only = cmd!(shell, "cat /sys/block/{device}/ro").read().await?;
    anyhow::ensure!(
        read_only.trim() == "1",
        "guest did not report {path} as read-only"
    );

    // Verify that a guest write is rejected by the device.
    let block = [0x5a; VMDK_SECTOR_SIZE];
    anyhow::ensure!(
        agent.write_file(path, &block[..]).await.is_err(),
        "guest unexpectedly wrote to read-only VMDK {path}"
    );
    Ok(())
}

/// Verify that truncating an opened source file is reported as a guest I/O error.
async fn verify_runtime_truncation(
    agent: &PipetteClient,
    path: &str,
    truncate_source: &Path,
    truncate_sector: usize,
) -> anyhow::Result<()> {
    // Remove the sector backing the selected virtual-disk range after opening.
    std::fs::OpenOptions::new()
        .write(true)
        .open(truncate_source)?
        .set_len(VMDK_SECTOR_SIZE as u64)?;

    // Bypass the guest cache and verify that reading the removed source data
    // fails.
    let shell = agent.unix_shell();
    let truncate_sector = truncate_sector.to_string();
    let direct_read = cmd!(
        shell,
        "dd if={path} of={LINUX_NULL_DEVICE_PATH} bs=512 skip={truncate_sector} count=1 iflag=direct,fullblock status=none"
    )
    .run()
    .await;
    anyhow::ensure!(
        direct_read.is_err(),
        "guest read unexpectedly succeeded after truncating a VMDK source"
    );
    Ok(())
}

/// Verify that concurrent direct reads return the complete expected disk data.
async fn verify_concurrent_reads(
    agent: &PipetteClient,
    path: &str,
    expected: &[u8],
    sector_count: usize,
) -> anyhow::Result<()> {
    // Stage the complete expected disk image inside the guest.
    agent
        .write_file(GUEST_EXPECTED_DATA_PATH, expected)
        .await
        .context("failed to write expected VMDK data into the guest")?;

    // Each worker repeatedly bypasses the guest page cache and compares the
    // complete disk against the expected data.
    let script = format!(
        r#"
pids=""
worker=0
while [ "$worker" -lt {CONCURRENT_READ_WORKER_COUNT} ]; do
    (
        iteration=0
        while [ "$iteration" -lt {CONCURRENT_READS_PER_WORKER} ]; do
            dd if="{path}" bs={VMDK_SECTOR_SIZE} count={sector_count} iflag=direct,fullblock status=none |
                cmp -s "{GUEST_EXPECTED_DATA_PATH}" - || exit 1
            iteration=$((iteration + 1))
        done
    ) &
    pids="$pids $!"
    worker=$((worker + 1))
done

status=0
for pid in $pids; do
    wait "$pid" || status=1
done
exit "$status"
"#
    );

    let shell = agent.unix_shell();
    cmd!(shell, "sh -c {script}")
        .run()
        .await
        .context("concurrent VMDK reads failed")
}

// Petri host tests reject invalid VMDKs before VM construction.
#[cfg(unix)]
petri::test!(flat_vmdk_rejects_non_regular_sources, |_| Some(()));

/// Verifies that non-regular files cannot be used as VMDK extent sources.
#[cfg(unix)]
async fn flat_vmdk_rejects_non_regular_sources(
    _params: PetriTestParams<'_>,
    _driver: DefaultDriver,
    (): (),
) -> anyhow::Result<()> {
    // Reference a character device instead of a regular extent source file.
    let directory = tempfile::tempdir().context("failed to create VMDK directory")?;
    let descriptor = write_flat_descriptor(&directory, LINUX_NULL_DEVICE_PATH, 1)?;

    // Open through the same format-routing path used by OpenVMM.
    let error = open_disk_type(
        &descriptor,
        OpenDiskOptions {
            read_only: true,
            direct: false,
        },
    )
    .await
    .expect_err("invalid VMDK descriptor was accepted");
    // Require rejection specifically because the source is not a regular file.
    anyhow::ensure!(
        format!("{error:#}").contains("is not a regular file"),
        "unexpected error for non-regular VMDK source: {error:#}"
    );
    Ok(())
}

// Petri host test for invalid extent sources rejected before VM construction.
petri::test!(flat_vmdk_rejects_invalid_sources, |_| Some(()));

/// Verifies rejection of missing VMDK sources and extents larger than their backing files.
async fn flat_vmdk_rejects_invalid_sources(
    _params: PetriTestParams<'_>,
    _driver: DefaultDriver,
    (): (),
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir().context("failed to create VMDK directory")?;

    // A descriptor referencing a missing source must fail when opening the source.
    let missing_source_file_name = "missing.bin";
    let descriptor = write_flat_descriptor(&directory, missing_source_file_name, 1)?;
    let error = open_disk_type(
        &descriptor,
        OpenDiskOptions {
            read_only: true,
            direct: false,
        },
    )
    .await
    .expect_err("invalid VMDK descriptor was accepted");
    // Check that the missing source is reported as an extent open failure.
    anyhow::ensure!(
        format!("{error:#}").contains("failed to open VMDK extent"),
        "unexpected error for missing VMDK source: {error:#}"
    );

    // An extent larger than its source must fail layout validation.
    let source_file_name = "five-sectors.bin";
    std::fs::write(
        directory.path().join(source_file_name),
        vec![0; 5 * VMDK_SECTOR_SIZE],
    )?;
    let descriptor = write_flat_descriptor(&directory, source_file_name, 10)?;
    let error = open_disk_type(
        &descriptor,
        OpenDiskOptions {
            read_only: true,
            direct: false,
        },
    )
    .await
    .expect_err("invalid VMDK descriptor was accepted");
    // Check that the error identifies both the extent end and the available source size.
    anyhow::ensure!(
        format!("{error:#}")
            .contains("extent 0 ends at source sector 10, but source file 0 has only 5 sectors"),
        "unexpected error for oversized VMDK extent: {error:#}"
    );

    Ok(())
}

/// Boots a Linux guest with a flat VMDK attached as a read-only virtio-blk disk.
///
/// Verifies extent mapping, write protection, and guest read errors after a
/// backing file is truncated.
#[openvmm_test(linux_direct_x64)]
async fn flat_vmdk_success(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    // Build the host fixture and transport its open source handles into the VM.
    let VmdkFixture {
        directory,
        disk,
        expected,
        sector_count,
    } = create_flat_vmdk()?;

    // Pipette uses hvsocket, so serial output is unnecessary here. Disabling
    // its host handles leaves room for all 60 source files in the startup
    // mesh message.
    let config = with_read_only_vmdk_disk(config.without_serial_output(), disk);
    let (vm, agent) = config.run().await?;

    // Wait for Linux to expose the expected virtio-blk capacity.
    let path = wait_for_virtio_blk(&agent, sector_count).await?;

    // Check extent ordering, source offsets, and zero padding through buffered
    // and direct guest reads.
    verify_extent_mapping(&agent, &path, &expected).await?;

    // Confirm that the guest reports the disk as read-only and rejects writes.
    verify_read_only(&agent, &path).await?;

    // Truncate the middle source. Source N backs guest sector N + 1 because
    // the fixture prepends an extra extent from the first source.
    let source_index = VMDK_SOURCE_COUNT / 2;
    let source_file_name = format!("source {source_index:02}.bin");
    let source_path = directory
        .path()
        .join(VMDK_SOURCE_DIRECTORY_NAME)
        .join(source_file_name);
    let guest_sector = source_index + 1;

    // Truncate the opened source and require the guest read to fail.
    verify_runtime_truncation(&agent, &path, &source_path, guest_sector).await?;

    // Shut down through the guest agent and require clean VM teardown.
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await
}

/// Stress the VMDK read path with concurrent direct reads across all extents.
#[openvmm_test(linux_direct_x64)]
async fn flat_vmdk_concurrent_reads(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    // Build the same multi-source fixture used by the functional VM test.
    let VmdkFixture {
        directory: _directory,
        disk,
        expected,
        sector_count,
    } = create_flat_vmdk()?;

    // Leave enough startup message handles for all 60 source files.
    let config = with_read_only_vmdk_disk(config.without_serial_output(), disk);
    let (vm, agent) = config.run().await?;

    // Wait for the guest disk to report the fixture's expected capacity.
    let path = wait_for_virtio_blk(&agent, sector_count).await?;

    // Repeatedly read and compare the complete disk from many guest workers.
    verify_concurrent_reads(&agent, &path, &expected, sector_count).await?;

    // Shut down through the guest agent and require clean VM teardown.
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await
}
