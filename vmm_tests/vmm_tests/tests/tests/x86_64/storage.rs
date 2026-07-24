// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests that focus on OpenHCL storage scenarios.
//! These tests require OpenHCL and a Linux guest.
//! They also require VTL2 support in OpenHCL, which is currently only available
//! on x86-64.

use crate::utils::ExpectedGuestDevice;
use crate::utils::get_device_paths;
use anyhow::Context;
use disk_backend_resources::FileDiskHandle;
use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::DiskLayerHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use guid::Guid;
use mesh::rpc::RpcSend;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerHandle;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::VpciDeviceConfig;
#[cfg(windows)]
use petri::PetriGuestStateLifetime;
use petri::PetriVmBuilder;
#[cfg(windows)]
use petri::PetriVmmBackend;
#[cfg(windows)]
use petri::ResolvedArtifact;
#[cfg(windows)]
use petri::hyperv::HyperVPetriBackend;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::PipetteClient;
use petri::pipette::cmd;
use petri::vtl2_settings::ControllerType;
use petri::vtl2_settings::Vtl2LunBuilder;
use petri::vtl2_settings::Vtl2StorageBackingDeviceBuilder;
use petri::vtl2_settings::Vtl2StorageControllerBuilder;
use petri::vtl2_settings::build_vtl2_storage_backing_physical_devices;
#[cfg(windows)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_X64;
#[cfg(windows)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::RELEASE_25_05_STANDARD_X64;
use scsidisk_resources::SimpleScsiDiskHandle;
use scsidisk_resources::SimpleScsiDvdHandle;
use scsidisk_resources::SimpleScsiDvdRequest;
use std::fs::File;
use std::io::Write;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use vm_resource::IntoResource;
use vmm_test_macros::openvmm_test;
#[cfg(windows)]
use vmm_test_macros::vmm_test;

/// Create a VPCI device config for an NVMe controller assigned to VTL2, with a single namespace.
/// The namespace will be backed by either a file or a ramdisk, depending on whether
/// `backing_file` is `Some` or `None`.
pub(crate) fn new_test_vtl2_nvme_device(
    nsid: u32,
    size: u64,
    instance_id: Guid,
    backing_file: Option<File>,
) -> VpciDeviceConfig {
    let layer = if let Some(file) = backing_file {
        LayeredDiskHandle::single_layer(DiskLayerHandle(FileDiskHandle(file).into_resource()))
    } else {
        LayeredDiskHandle::single_layer(RamDiskLayerHandle {
            len: Some(size),
            sector_size: None,
        })
    };

    VpciDeviceConfig {
        vtl: DeviceVtl::Vtl2,
        instance_id,
        resource: NvmeControllerHandle {
            subsystem_id: instance_id,
            max_io_queues: 64,
            msix_count: 64,
            namespaces: vec![NamespaceDefinition {
                nsid,
                disk: layer.into_resource(),
                read_only: false,
            }],
            requests: None,
        }
        .into_resource(),
        vnode: None,
    }
}

/// Runs a series of validation steps inside the Linux guest to verify that the
/// storage devices (especially as presented by OpenHCL's vSCSI implementation
/// storvsp) are present and working correctly.
///
/// May `panic!`, `assert!`, or return an `Err` if any checks fail. Which
/// mechanism is used depends on the nature of the failure and the most
/// convenient way to check for it in this routine.
async fn test_storage_linux(
    agent: &PipetteClient,
    controller_guid: Guid,
    expected_devices: Vec<ExpectedGuestDevice>,
) -> anyhow::Result<()> {
    const DEVICE_DISCOVER_RETRIES: u32 = 20;
    const DEVICE_DISCOVER_SLEEP_SECS: u64 = 3;

    let sh = agent.unix_shell();

    // Discover device paths, with retries
    let device_paths = {
        let mut attempt = 0;
        loop {
            match get_device_paths(agent, controller_guid, expected_devices.clone()).await {
                Ok(paths) => {
                    tracing::info!(?paths, "Discovered device paths");
                    break paths;
                }
                Err(e) if attempt + 1 < DEVICE_DISCOVER_RETRIES => {
                    tracing::warn!(
                        "Attempt {}/{}: Failed to get device paths: {:#}. Retrying in {} seconds...",
                        attempt + 1,
                        DEVICE_DISCOVER_RETRIES,
                        e,
                        DEVICE_DISCOVER_SLEEP_SECS
                    );
                    let seconds = format!("{DEVICE_DISCOVER_SLEEP_SECS}");
                    cmd!(sh, "sleep {seconds}").run().await?;
                    attempt += 1;
                }
                Err(e) => {
                    anyhow::bail!(
                        "Failed to get device paths after {} attempts: {:#}",
                        DEVICE_DISCOVER_RETRIES,
                        e
                    );
                }
            }
        }
    };

    // Do IO to all devices. Generate a file with random contents so that we
    // can verify that the writes (and reads) work correctly.
    //
    // - `{o,i}flag=direct` is needed to ensure that the IO is not served
    //   from the guest's cache.
    // - `conv=fsync` is needed to ensure that the write is flushed to the
    //    device before `dd` exits.
    // - `iflag=fullblock` is needed to ensure that `dd` reads the full
    //   amount of data requested, otherwise it may read less and exit
    //   early.
    for device in &device_paths {
        tracing::info!(?device, "Performing IO tests");
        cmd!(sh, "dd if=/dev/urandom of=/tmp/random_data bs=1M count=100")
            .run()
            .await?;

        cmd!(
            sh,
            "dd if=/tmp/random_data of={device} bs=1M count=100 oflag=direct conv=fsync"
        )
        .run()
        .await?;

        cmd!(
            sh,
            "dd if={device} of=/tmp/verify_data bs=1M count=100 iflag=direct,fullblock"
        )
        .run()
        .await?;

        cmd!(sh, "cmp -s /tmp/random_data /tmp/verify_data")
            .read()
            .await
            .with_context(|| format!("Read and written data differs for device {device}"))?;

        cmd!(sh, "rm -f /tmp/random_data /tmp/verify_data")
            .run()
            .await?;
    }

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a SCSI disk assigned to VTL2, an NVMe disk assigned to VTL2, and
/// vmbus relay. This should expose two disks to VTL0 via vmbus.
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn storvsp(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    const NVME_INSTANCE: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");
    let vtl2_lun = 5;
    let vtl0_scsi_lun = 0;
    let vtl0_nvme_lun = 1;
    let vtl2_nsid = 37;
    let scsi_instance = Guid::new_random();
    const SCSI_DISK_SECTORS: u64 = 0x4_0000;
    const NVME_DISK_SECTORS: u64 = 0x5_0000;
    const SECTOR_SIZE: u64 = 512;
    const EXPECTED_SCSI_DISK_SIZE_BYTES: u64 = SCSI_DISK_SECTORS * SECTOR_SIZE;
    const EXPECTED_NVME_DISK_SIZE_BYTES: u64 = NVME_DISK_SECTORS * SECTOR_SIZE;

    // Assumptions made by test infra & routines:
    //
    // 1. Some test-infra added disks are 64MiB in size. Since we find disks by size,
    // ensure that our test disks are a different size.
    // 2. Disks under test need to be at least 100MiB for the IO tests (see [`test_storage_linux`]),
    // with some arbitrary buffer (5MiB in this case).
    static_assertions::const_assert_ne!(EXPECTED_SCSI_DISK_SIZE_BYTES, 64 * 1024 * 1024);
    static_assertions::const_assert!(EXPECTED_SCSI_DISK_SIZE_BYTES > 105 * 1024 * 1024);
    static_assertions::const_assert_ne!(EXPECTED_NVME_DISK_SIZE_BYTES, 64 * 1024 * 1024);
    static_assertions::const_assert!(EXPECTED_NVME_DISK_SIZE_BYTES > 105 * 1024 * 1024);

    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.vmbus_devices.push((
                    DeviceVtl::Vtl2,
                    ScsiControllerHandle {
                        instance_id: scsi_instance,
                        max_sub_channel_count: 1,
                        devices: vec![ScsiDeviceAndPath {
                            path: ScsiPath {
                                path: 0,
                                target: 0,
                                lun: vtl2_lun as u8,
                            },
                            device: SimpleScsiDiskHandle {
                                disk: LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                                    len: Some(SCSI_DISK_SECTORS * SECTOR_SIZE),
                                    sector_size: None,
                                })
                                .into_resource(),
                                read_only: false,
                                parameters: Default::default(),
                            }
                            .into_resource(),
                        }],
                        io_queue_depth: None,
                        requests: None,
                        poll_mode_queue_depth: None,
                    }
                    .into_resource(),
                ));
                c.vpci_devices.push(new_test_vtl2_nvme_device(
                    vtl2_nsid,
                    NVME_DISK_SECTORS * SECTOR_SIZE,
                    NVME_INSTANCE,
                    None,
                ));
            })
        })
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .with_instance_id(scsi_instance)
                .add_lun(
                    Vtl2LunBuilder::disk()
                        .with_location(vtl0_scsi_lun)
                        .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                            ControllerType::Scsi,
                            scsi_instance,
                            vtl2_lun,
                        )),
                )
                .add_lun(
                    Vtl2LunBuilder::disk()
                        .with_location(vtl0_nvme_lun)
                        .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                            ControllerType::Nvme,
                            NVME_INSTANCE,
                            vtl2_nsid,
                        )),
                )
                .build(),
        )
        .run()
        .await?;

    test_storage_linux(
        &agent,
        scsi_instance,
        vec![
            ExpectedGuestDevice {
                lun: vtl0_scsi_lun,
                disk_size_sectors: SCSI_DISK_SECTORS as usize,
                friendly_name: "scsi".to_string(),
            },
            ExpectedGuestDevice {
                lun: vtl0_nvme_lun,
                disk_size_sectors: NVME_DISK_SECTORS as usize,
                friendly_name: "nvme".to_string(),
            },
        ],
    )
    .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

#[cfg(windows)]
fn write_both_endian_u16(bytes: &mut [u8], value: u16) {
    bytes[..2].copy_from_slice(&value.to_le_bytes());
    bytes[2..].copy_from_slice(&value.to_be_bytes());
}

#[cfg(windows)]
fn write_both_endian_u32(bytes: &mut [u8], value: u32) {
    bytes[..4].copy_from_slice(&value.to_le_bytes());
    bytes[4..].copy_from_slice(&value.to_be_bytes());
}

#[cfg(windows)]
fn write_iso_directory_record(record: &mut [u8], extent: u32, data_length: u32, identifier: u8) {
    record[0] = 34;
    write_both_endian_u32(&mut record[2..10], extent);
    write_both_endian_u32(&mut record[10..18], data_length);
    record[18..25].copy_from_slice(&[126, 1, 1, 0, 0, 0, 0]);
    record[25] = 2;
    write_both_endian_u16(&mut record[28..32], 1);
    record[32] = 1;
    record[33] = identifier;
}

#[cfg(windows)]
fn write_test_iso(file: &mut File, size: u64) -> anyhow::Result<()> {
    const BLOCK_SIZE: usize = 2048;
    const PRIMARY_VOLUME_DESCRIPTOR_BLOCK: usize = 16;
    const TERMINATOR_BLOCK: usize = 17;
    const LITTLE_ENDIAN_PATH_TABLE_BLOCK: usize = 18;
    const BIG_ENDIAN_PATH_TABLE_BLOCK: usize = 19;
    const ROOT_DIRECTORY_BLOCK: usize = 20;

    anyhow::ensure!(
        size >= (ROOT_DIRECTORY_BLOCK + 1) as u64 * BLOCK_SIZE as u64,
        "test ISO is too small"
    );
    anyhow::ensure!(
        size.is_multiple_of(BLOCK_SIZE as u64),
        "test ISO size is not block aligned"
    );

    let block_count: u32 = (size / BLOCK_SIZE as u64).try_into()?;
    let mut image = vec![0_u8; size.try_into()?];

    let primary = &mut image[PRIMARY_VOLUME_DESCRIPTOR_BLOCK * BLOCK_SIZE
        ..(PRIMARY_VOLUME_DESCRIPTOR_BLOCK + 1) * BLOCK_SIZE];
    primary[0] = 1;
    primary[1..6].copy_from_slice(b"CD001");
    primary[6] = 1;
    primary[8..40].fill(b' ');
    primary[40..72].fill(b' ');
    primary[40..48].copy_from_slice(b"ADE_TEST");
    write_both_endian_u32(&mut primary[80..88], block_count);
    write_both_endian_u16(&mut primary[120..124], 1);
    write_both_endian_u16(&mut primary[124..128], 1);
    write_both_endian_u16(&mut primary[128..132], BLOCK_SIZE as u16);
    write_both_endian_u32(&mut primary[132..140], 10);
    primary[140..144].copy_from_slice(&(LITTLE_ENDIAN_PATH_TABLE_BLOCK as u32).to_le_bytes());
    primary[148..152].copy_from_slice(&(BIG_ENDIAN_PATH_TABLE_BLOCK as u32).to_be_bytes());
    write_iso_directory_record(
        &mut primary[156..190],
        ROOT_DIRECTORY_BLOCK as u32,
        BLOCK_SIZE as u32,
        0,
    );
    primary[190..813].fill(b' ');
    primary[813..830].copy_from_slice(b"2026072400000000\0");
    primary[830..847].copy_from_slice(b"2026072400000000\0");
    primary[847..864].copy_from_slice(b"0000000000000000\0");
    primary[864..881].copy_from_slice(b"0000000000000000\0");
    primary[881] = 1;

    let terminator = &mut image[TERMINATOR_BLOCK * BLOCK_SIZE..(TERMINATOR_BLOCK + 1) * BLOCK_SIZE];
    terminator[0] = 255;
    terminator[1..6].copy_from_slice(b"CD001");
    terminator[6] = 1;

    let little_endian_path_table = &mut image[LITTLE_ENDIAN_PATH_TABLE_BLOCK * BLOCK_SIZE..];
    little_endian_path_table[0] = 1;
    little_endian_path_table[2..6].copy_from_slice(&(ROOT_DIRECTORY_BLOCK as u32).to_le_bytes());
    little_endian_path_table[6..8].copy_from_slice(&1_u16.to_le_bytes());

    let big_endian_path_table = &mut image[BIG_ENDIAN_PATH_TABLE_BLOCK * BLOCK_SIZE..];
    big_endian_path_table[0] = 1;
    big_endian_path_table[2..6].copy_from_slice(&(ROOT_DIRECTORY_BLOCK as u32).to_be_bytes());
    big_endian_path_table[6..8].copy_from_slice(&1_u16.to_be_bytes());

    let root_directory = &mut image[ROOT_DIRECTORY_BLOCK * BLOCK_SIZE..];
    write_iso_directory_record(
        &mut root_directory[..34],
        ROOT_DIRECTORY_BLOCK as u32,
        BLOCK_SIZE as u32,
        0,
    );
    write_iso_directory_record(
        &mut root_directory[34..68],
        ROOT_DIRECTORY_BLOCK as u32,
        BLOCK_SIZE as u32,
        1,
    );

    file.write_all(&image).context("write test ISO")
}

/// Test the Azure Disk Encryption (ADE) scenario:
/// Boot a Generation 1 Windows VM, the VTL storage should look like this:
///  - IDE:
///    0:0 - OS disk
///    0:1 - <empty>
///    1:0 - CD ROM ISO (with a disc initially)
///    1:1 - BEK Volume (empty initially)
///
/// Sequence of events:
///  1. Boot the VM, verify that the IDE devices are present and have the expected sizes.
///  2. Eject the CD ROM from the guest (mirrors azure agent behavior)
///  2. Power off the VM, and add a BEK volume
///  3. Power on the VM
///  4. Set up bitlocker in the guest, and verify that the BEK volume is populated with the expected data.
///  5. Soft reboot the VM, ensure that it boots again successfully
///
/// Ideally the OS disk is backed by NVMe from the host to VTL2. The BEK volume should be backed by vSCSI on the host to VTL2, as would the CD-ROM. These can be on the same vSCSI controller.
#[cfg(windows)]
#[vmm_test(hyperv_openhcl_pcat_x64(vhd(windows_datacenter_core_2022_x64)))]
async fn storvsp_hyperv_ade(
    config: PetriVmBuilder<HyperVPetriBackend>,
) -> Result<(), anyhow::Error> {
    storvsp_hyperv_ade_core(config).await
}

/// Reproduce the ADE sequence using the latest release OpenHCL from the first
/// boot through CD-ROM ejection, the power cycle, BitLocker setup, and the final
/// guest reboot.
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_pcat_x64(vhd(windows_datacenter_core_2022_x64))[LATEST_RELEASE_STANDARD_X64]
)]
async fn storvsp_hyperv_ade_latest_release(
    config: PetriVmBuilder<HyperVPetriBackend>,
    (release_openhcl,): (ResolvedArtifact<LATEST_RELEASE_STANDARD_X64>,),
) -> Result<(), anyhow::Error> {
    storvsp_hyperv_ade_core(
        config
            .with_custom_openhcl(release_openhcl)
            .with_guest_state_lifetime(PetriGuestStateLifetime::Disk),
    )
    .await
}

/// Reproduce the ADE sequence using OpenHCL release 2505 from the first boot
/// through CD-ROM ejection, the power cycle, BitLocker setup, and the final
/// guest reboot.
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_pcat_x64(vhd(windows_datacenter_core_2022_x64))[RELEASE_25_05_STANDARD_X64]
)]
async fn storvsp_hyperv_ade_release_2505(
    config: PetriVmBuilder<HyperVPetriBackend>,
    (release_openhcl,): (ResolvedArtifact<RELEASE_25_05_STANDARD_X64>,),
) -> Result<(), anyhow::Error> {
    storvsp_hyperv_ade_core(
        config
            .with_custom_openhcl(release_openhcl)
            .with_guest_state_lifetime(PetriGuestStateLifetime::Disk),
    )
    .await
}

#[cfg(windows)]
async fn storvsp_hyperv_ade_core(
    config: PetriVmBuilder<HyperVPetriBackend>,
) -> Result<(), anyhow::Error> {
    use petri::Disk;
    use petri::Drive;
    use petri::VmbusStorageType;
    use petri::Vtl;

    const VTL2_CD_LUN: u32 = 0;
    const VTL2_BEK_LUN: u32 = 1;
    const IDE_SECONDARY_CHANNEL: u32 = 1;
    const IDE_CD_LOCATION: u32 = 0;
    const IDE_BEK_LOCATION: u32 = 1;
    const OS_DISK_SIZE_BYTES: u64 = 32_210_196_480;
    const CD_MEDIA_SIZE_BYTES: u64 = 4 * 1024 * 1024;
    const BEK_DISK_SIZE_BYTES: u64 = 128 * 1024 * 1024;

    let vtl2_storage_instance = Guid::new_random();

    let mut cd_media = tempfile::NamedTempFile::with_suffix("provisioning.iso")
        .context("create temporary fake provisioning CD media")?;
    write_test_iso(cd_media.as_file_mut(), CD_MEDIA_SIZE_BYTES)?;
    let cd_media_path = cd_media.into_temp_path();

    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .add_vmbus_storage_controller(&vtl2_storage_instance, Vtl::Vtl2, VmbusStorageType::Scsi)
        .add_vmbus_drive(
            Drive::new(Some(Disk::Persistent(cd_media_path.to_path_buf())), true),
            &vtl2_storage_instance,
            Some(VTL2_CD_LUN),
        )
        .add_vtl2_ide_lun(
            Vtl2LunBuilder::dvd()
                .with_channel(IDE_SECONDARY_CHANNEL)
                .with_location(IDE_CD_LOCATION)
                .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                    ControllerType::Scsi,
                    vtl2_storage_instance,
                    VTL2_CD_LUN,
                )),
        )
        .run()
        .await?;

    let shell = agent.windows_shell();
    let initial_storage_check = format!(
        r#"
$ErrorActionPreference = 'Stop'
$ideDisks = @(Get-CimInstance Win32_DiskDrive | Where-Object {{ $_.InterfaceType -eq 'IDE' }})
if ($ideDisks.Count -ne 1) {{
    throw "Expected one IDE disk before adding the BEK volume, found $($ideDisks.Count)"
}}
if ([uint64]$ideDisks[0].Size -ne {OS_DISK_SIZE_BYTES}) {{
    throw "Expected a {OS_DISK_SIZE_BYTES}-byte OS disk, found $($ideDisks[0].Size) bytes"
}}

$cdroms = @(Get-CimInstance Win32_CDROMDrive)
if ($cdroms.Count -ne 1) {{
    throw "Expected one IDE CD-ROM, found $($cdroms.Count)"
}}
if (-not $cdroms[0].MediaLoaded) {{
    throw 'Expected the IDE CD-ROM to contain media'
}}
if ([uint64]$cdroms[0].Size -ne {CD_MEDIA_SIZE_BYTES}) {{
    throw "Expected {CD_MEDIA_SIZE_BYTES} bytes of CD media, found $($cdroms[0].Size) bytes"
}}
"#
    );
    cmd!(shell, "powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &initial_storage_check,
        ])
        .run()
        .await
        .context("verify initial ADE storage layout")?;

    let eject_cdrom = r#"
$ErrorActionPreference = 'Stop'
$source = @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

public static class CdromEjector
{
    private const uint FsctlLockVolume = 0x00090018;
    private const uint FsctlUnlockVolume = 0x0009001c;
    private const uint IoctlStorageEjectMedia = 0x002d4808;

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern SafeFileHandle CreateFile(
        string fileName,
        uint desiredAccess,
        uint shareMode,
        IntPtr securityAttributes,
        uint creationDisposition,
        uint flagsAndAttributes,
        IntPtr templateFile);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool DeviceIoControl(
        SafeFileHandle device,
        uint controlCode,
        IntPtr inputBuffer,
        uint inputBufferSize,
        IntPtr outputBuffer,
        uint outputBufferSize,
        out uint bytesReturned,
        IntPtr overlapped);

    public static void Eject(string path)
    {
        using (SafeFileHandle device = CreateFile(
            path,
            0x80000000,
            3,
            IntPtr.Zero,
            3,
            0,
            IntPtr.Zero))
        {
            if (device.IsInvalid)
            {
                throw new Win32Exception(Marshal.GetLastWin32Error());
            }

            uint bytesReturned;
            if (!DeviceIoControl(device, FsctlLockVolume, IntPtr.Zero, 0, IntPtr.Zero, 0, out bytesReturned, IntPtr.Zero))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error());
            }

            try
            {
                if (!DeviceIoControl(device, IoctlStorageEjectMedia, IntPtr.Zero, 0, IntPtr.Zero, 0, out bytesReturned, IntPtr.Zero))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
            }
            finally
            {
                DeviceIoControl(device, FsctlUnlockVolume, IntPtr.Zero, 0, IntPtr.Zero, 0, out bytesReturned, IntPtr.Zero);
            }
        }
    }
}
'@

Add-Type -TypeDefinition $source
[CdromEjector]::Eject('\\.\CdRom0')

$deadline = (Get-Date).AddSeconds(30)
do {
    $cdrom = Get-CimInstance Win32_CDROMDrive
    if (-not $cdrom.MediaLoaded) {
        exit 0
    }
    Start-Sleep -Seconds 1
} while ((Get-Date) -lt $deadline)

throw 'The IDE CD-ROM still reports loaded media after eject'
"#;
    cmd!(shell, "powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", eject_cdrom])
        .run()
        .await
        .context("eject ADE CD media")?;

    agent.power_off().await?;
    vm.wait_for_clean_shutdown().await?;

    let mut bek_vhd =
        tempfile::NamedTempFile::with_suffix("ade-bek.vhd").context("create temporary BEK VHD")?;
    bek_vhd
        .as_file()
        .set_len(BEK_DISK_SIZE_BYTES)
        .context("set BEK VHD length")?;
    disk_vhd1::Vhd1Disk::make_fixed(bek_vhd.as_file_mut()).context("make fixed BEK VHD")?;
    let bek_vhd_path = bek_vhd.into_temp_path();

    vm.set_vmbus_drive(
        Drive::new(Some(Disk::Persistent(bek_vhd_path.to_path_buf())), false),
        &vtl2_storage_instance,
        Some(VTL2_BEK_LUN),
    )
    .await?;
    vm.add_vtl2_ide_lun(
        Vtl2LunBuilder::disk()
            .with_channel(IDE_SECONDARY_CHANNEL)
            .with_location(IDE_BEK_LOCATION)
            .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                ControllerType::Scsi,
                vtl2_storage_instance,
                VTL2_BEK_LUN,
            )),
    )
    .await?;

    let agent = vm.start().await?;
    let shell = agent.windows_shell();
    let prepare_bek_volume = format!(
        r#"
$ErrorActionPreference = 'Stop'
$ideDisks = @(Get-CimInstance Win32_DiskDrive | Where-Object {{ $_.InterfaceType -eq 'IDE' }})
if ($ideDisks.Count -ne 2) {{
    throw "Expected two IDE disks after adding the BEK volume, found $($ideDisks.Count)"
}}

$osDisk = @($ideDisks | Where-Object {{ [uint64]$_.Size -eq {OS_DISK_SIZE_BYTES} }})
if ($osDisk.Count -ne 1) {{
    throw "Expected one {OS_DISK_SIZE_BYTES}-byte OS disk, found $($osDisk.Count)"
}}

$bekDisk = @(Get-Disk | Where-Object {{ [uint64]$_.Size -eq {BEK_DISK_SIZE_BYTES} }})
if ($bekDisk.Count -ne 1) {{
    throw "Expected one {BEK_DISK_SIZE_BYTES}-byte BEK disk, found $($bekDisk.Count)"
}}
if ($bekDisk[0].PartitionStyle -ne 'RAW') {{
    throw "Expected a raw BEK disk, found partition style $($bekDisk[0].PartitionStyle)"
}}
if (Get-Volume -DriveLetter B -ErrorAction SilentlyContinue) {{
    throw 'Drive letter B is already in use'
}}

Initialize-Disk -Number $bekDisk[0].Number -PartitionStyle MBR
$partition = New-Partition -DiskNumber $bekDisk[0].Number -UseMaximumSize -DriveLetter B
$partition | Format-Volume -FileSystem FAT32 -NewFileSystemLabel BEK -Confirm:$false
"#
    );
    cmd!(shell, "powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &prepare_bek_volume,
        ])
        .run()
        .await
        .context("prepare BEK volume")?;

    let enable_bitlocker = r#"
$ErrorActionPreference = 'Stop'
$policyPath = 'HKLM:\SOFTWARE\Policies\Microsoft\FVE'
New-Item -Path $policyPath -Force | Out-Null
New-ItemProperty -Path $policyPath -Name EnableBDEWithNoTPM -PropertyType DWord -Value 1 -Force | Out-Null
New-ItemProperty -Path $policyPath -Name UseAdvancedStartup -PropertyType DWord -Value 1 -Force | Out-Null

& manage-bde.exe -on C: -usedspaceonly -startupkey B:\ -skiphardwaretest
if ($LASTEXITCODE -ne 0) {
    throw "manage-bde -on failed with exit code $LASTEXITCODE"
}

$deadline = (Get-Date).AddMinutes(20)
do {
    $status = (& manage-bde.exe -status C: | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "manage-bde -status failed with exit code $LASTEXITCODE"
    }
    if ($status -match 'Percentage Encrypted:\s+100(?:\.0+)?%') {
        break
    }
    if ((Get-Date) -ge $deadline) {
        throw "BitLocker encryption did not complete within 20 minutes:`n$status"
    }
    Start-Sleep -Seconds 5
} while ($true)

$protectors = (& manage-bde.exe -protectors -get C: -type ExternalKey | Out-String)
if ($LASTEXITCODE -ne 0 -or $protectors -notmatch 'External Key') {
    throw "Expected an external key protector:`n$protectors"
}

$bekFiles = @(Get-ChildItem -Path B:\ -Filter '*.BEK' -File -Force)
if ($bekFiles.Count -ne 1) {
    throw "Expected one BEK file, found $($bekFiles.Count)"
}
if ($bekFiles[0].Length -eq 0) {
    throw 'The BEK file is empty'
}
"#;
    cmd!(shell, "powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            enable_bitlocker,
        ])
        .run()
        .await
        .context("enable BitLocker with a BEK startup key")?;

    agent.reboot().await?;
    let agent = vm.wait_for_reset().await?;
    let shell = agent.windows_shell();
    let bitlocker_status = cmd!(shell, "manage-bde.exe")
        .args(["-status", "C:"])
        .read()
        .await
        .context("read BitLocker status after reboot")?;
    anyhow::ensure!(
        bitlocker_status.contains("Lock Status:") && bitlocker_status.contains("Unlocked"),
        "OS volume did not unlock after reboot: {bitlocker_status}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test a Linux VM with a SCSI disk assigned to VTL2 and
/// vmbus relay. This should expose one disk to VTL0 via vmbus.
#[cfg(windows)]
#[vmm_test(hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)))]
async fn storvsp_hyperv<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> Result<(), anyhow::Error> {
    let vtl2_lun = 5;
    let vtl0_scsi_lun = 0;
    let scsi_instance = Guid::new_random();
    let vtl2_vsid = Guid::new_random();
    const SCSI_DISK_SECTORS: u64 = 0x4_0000;
    const SECTOR_SIZE: u64 = 512;
    const EXPECTED_SCSI_DISK_SIZE_BYTES: u64 = SCSI_DISK_SECTORS * SECTOR_SIZE;

    // Assumptions made by test infra & routines:
    //
    // 1. Some test-infra added disks are 64MiB in size. Since we find disks by size,
    // ensure that our test disks are a different size.
    // 2. Disks under test need to be at least 100MiB for the IO tests (see [`test_storage_linux`]),
    // with some arbitrary buffer (5MiB in this case).
    static_assertions::const_assert_ne!(EXPECTED_SCSI_DISK_SIZE_BYTES, 64 * 1024 * 1024);
    static_assertions::const_assert!(EXPECTED_SCSI_DISK_SIZE_BYTES > 105 * 1024 * 1024);

    let mut vhd =
        tempfile::NamedTempFile::with_suffix("vtl2.vhd").context("create temp vtl2 vhd")?;
    vhd.as_file()
        .set_len(EXPECTED_SCSI_DISK_SIZE_BYTES)
        .context("set file length")?;

    disk_vhd1::Vhd1Disk::make_fixed(vhd.as_file_mut()).context("make fixed")?;

    // Close a handle to the file without deleting it, so that Hyper-V can open it.
    let vhd_path = vhd.into_temp_path();

    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .with_instance_id(scsi_instance)
                .build(),
        )
        .add_vmbus_storage_controller(&vtl2_vsid, petri::Vtl::Vtl2, petri::VmbusStorageType::Scsi)
        .add_vmbus_drive(
            petri::Drive::new(Some(petri::Disk::Persistent(vhd_path.to_path_buf())), false),
            &vtl2_vsid,
            Some(vtl2_lun),
        )
        .run()
        .await?;

    vm.modify_vtl2_settings(|s| {
        let storage_controllers = &mut s.dynamic.as_mut().unwrap().storage_controllers;
        assert_eq!(storage_controllers.len(), 1);
        assert_eq!(
            storage_controllers[0].instance_id,
            scsi_instance.to_string()
        );

        let controller = storage_controllers.get_mut(0).unwrap();
        controller.luns.push(
            Vtl2LunBuilder::disk()
                .with_location(vtl0_scsi_lun)
                .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                    ControllerType::Scsi,
                    vtl2_vsid,
                    vtl2_lun,
                ))
                .build(),
        );
    })
    .await?;

    test_storage_linux(
        &agent,
        scsi_instance,
        vec![ExpectedGuestDevice {
            lun: vtl0_scsi_lun,
            disk_size_sectors: SCSI_DISK_SECTORS as usize,
            friendly_name: "scsi".to_string(),
        }],
    )
    .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test a Hyper-V OpenHCL Linux VM with an NVMe emulator device assigned to
/// VTL2, relayed to VTL0 via SCSI. Validates that the guest can discover and
/// perform IO on the disk.
#[cfg(windows)]
#[vmm_test(hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)))]
async fn storvsp_nvme_hyperv<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
) -> Result<(), anyhow::Error> {
    let vtl0_nvme_lun = 0;
    let nvme_nsid = 1;
    let nvme_vsid = Guid::new_random();
    let scsi_instance = Guid::new_random();
    const NVME_DISK_SECTORS: u64 = 0x5_0000;
    const SECTOR_SIZE: u64 = 512;
    const EXPECTED_NVME_DISK_SIZE_BYTES: u64 = NVME_DISK_SECTORS * SECTOR_SIZE;

    let mut vhd =
        tempfile::NamedTempFile::with_suffix("nvme.vhd").context("create temp nvme vhd")?;
    vhd.as_file()
        .set_len(EXPECTED_NVME_DISK_SIZE_BYTES)
        .context("set file length")?;

    disk_vhd1::Vhd1Disk::make_fixed(vhd.as_file_mut()).context("make fixed")?;

    // Close the handle without deleting the file, so Hyper-V can open it.
    let vhd_path = vhd.into_temp_path();

    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .add_vmbus_storage_controller(&nvme_vsid, petri::Vtl::Vtl2, petri::VmbusStorageType::Nvme)
        .add_vmbus_drive(
            petri::Drive::new(Some(petri::Disk::Persistent(vhd_path.to_path_buf())), false),
            &nvme_vsid,
            Some(nvme_nsid),
        )
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .with_instance_id(scsi_instance)
                .add_lun(
                    Vtl2LunBuilder::disk()
                        .with_location(vtl0_nvme_lun)
                        .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                            ControllerType::Nvme,
                            nvme_vsid,
                            nvme_nsid,
                        )),
                )
                .build(),
        )
        .run()
        .await?;

    test_storage_linux(
        &agent,
        scsi_instance,
        vec![ExpectedGuestDevice {
            lun: vtl0_nvme_lun,
            disk_size_sectors: NVME_DISK_SECTORS as usize,
            friendly_name: "nvme".to_string(),
        }],
    )
    .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn openhcl_linux_stripe_storvsp(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    const NVME_INSTANCE_1: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");
    const NVME_INSTANCE_2: Guid = guid::guid!("06a97a09-d5ad-4689-b638-9419d7346a68");
    let vtl0_nvme_lun = 0;
    let vtl2_nsid = 1;
    const NVME_DISK_SECTORS: u64 = 0x2_0000;
    const SECTOR_SIZE: u64 = 512;
    const NUMBER_OF_STRIPE_DEVICES: u64 = 2;
    const EXPECTED_STRIPED_DISK_SIZE_SECTORS: u64 = NVME_DISK_SECTORS * NUMBER_OF_STRIPE_DEVICES;
    const EXPECTED_STRIPED_DISK_SIZE_BYTES: u64 = EXPECTED_STRIPED_DISK_SIZE_SECTORS * SECTOR_SIZE;
    let scsi_instance = Guid::new_random();

    // Assumptions made by test infra & routines:
    //
    // 1. Some test-infra added disks are 64MiB in size. Since we find disks by size,
    // ensure that our test disks are a different size.
    // 2. Disks under test need to be at least 100MiB for the IO tests (see [`test_storage_linux`]),
    // with some arbitrary buffer (5MiB in this case).
    static_assertions::const_assert_ne!(EXPECTED_STRIPED_DISK_SIZE_BYTES, 64 * 1024 * 1024);
    static_assertions::const_assert!(EXPECTED_STRIPED_DISK_SIZE_BYTES > 105 * 1024 * 1024);

    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.vpci_devices.extend([
                    new_test_vtl2_nvme_device(
                        vtl2_nsid,
                        NVME_DISK_SECTORS * SECTOR_SIZE,
                        NVME_INSTANCE_1,
                        None,
                    ),
                    new_test_vtl2_nvme_device(
                        vtl2_nsid,
                        NVME_DISK_SECTORS * SECTOR_SIZE,
                        NVME_INSTANCE_2,
                        None,
                    ),
                ]);
            })
        })
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .with_instance_id(scsi_instance)
                .add_lun(
                    Vtl2LunBuilder::disk()
                        .with_location(vtl0_nvme_lun)
                        .with_chunk_size_in_kb(128)
                        .with_physical_devices(vec![
                            Vtl2StorageBackingDeviceBuilder::new(
                                ControllerType::Nvme,
                                NVME_INSTANCE_1,
                                vtl2_nsid,
                            ),
                            Vtl2StorageBackingDeviceBuilder::new(
                                ControllerType::Nvme,
                                NVME_INSTANCE_2,
                                vtl2_nsid,
                            ),
                        ]),
                )
                .build(),
        )
        .run()
        .await?;

    test_storage_linux(
        &agent,
        scsi_instance,
        vec![ExpectedGuestDevice {
            lun: vtl0_nvme_lun,
            disk_size_sectors: (NVME_DISK_SECTORS * NUMBER_OF_STRIPE_DEVICES) as usize,
            friendly_name: "striped-nvme".to_string(),
        }],
    )
    .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a SCSI DVD assigned to VTL2, and vmbus
/// relay. This should expose a DVD to VTL0 via vmbus. Start with an empty
/// drive, then add and remove media.
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn openhcl_linux_storvsp_dvd(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    let vtl2_lun = 5;
    let vtl0_scsi_lun = 0;
    let scsi_instance = Guid::new_random();

    let (hot_plug_send, hot_plug_recv) = mesh::channel();

    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.vmbus_devices.push((
                    DeviceVtl::Vtl2,
                    ScsiControllerHandle {
                        instance_id: scsi_instance,
                        max_sub_channel_count: 1,
                        devices: vec![ScsiDeviceAndPath {
                            path: ScsiPath {
                                path: 0,
                                target: 0,
                                lun: vtl2_lun as u8,
                            },
                            device: SimpleScsiDvdHandle {
                                media: None,
                                requests: Some(hot_plug_recv),
                            }
                            .into_resource(),
                        }],
                        io_queue_depth: None,
                        requests: None,
                        poll_mode_queue_depth: None,
                    }
                    .into_resource(),
                ));
            })
        })
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .with_instance_id(scsi_instance)
                .add_lun(Vtl2LunBuilder::dvd().with_location(vtl0_scsi_lun))
                // No physical devices initially, so the drive is empty
                .build(),
        )
        .run()
        .await?;

    let read_drive = || agent.read_file("/dev/sr0");

    let ensure_no_medium = |r: anyhow::Result<_>| {
        match r {
            Ok(_) => anyhow::bail!("expected error reading from dvd drive"),
            Err(e) => {
                let e = format!("{:#}", e);
                if !e.contains("No medium found") {
                    anyhow::bail!("unexpected error reading from dvd drive: {e}");
                }
            }
        }
        Ok(())
    };

    // Initially no media.
    ensure_no_medium(read_drive().await)?;

    let len = 0x42000;

    hot_plug_send
        .call_failable(
            SimpleScsiDvdRequest::ChangeMedia,
            Some(
                LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                    len: Some(len),
                    sector_size: None,
                })
                .into_resource(),
            ),
        )
        .await
        .context("failed to change media")?;

    vm.modify_vtl2_settings(|v| {
        v.dynamic.as_mut().unwrap().storage_controllers[0].luns[0].physical_devices =
            build_vtl2_storage_backing_physical_devices(vec![Vtl2StorageBackingDeviceBuilder::new(
                ControllerType::Scsi,
                scsi_instance,
                vtl2_lun,
            )])
    })
    .await
    .context("failed to modify vtl2 settings")?;

    let b = read_drive().await.context("failed to read dvd drive")?;
    assert_eq!(
        b.len() as u64,
        len,
        "expected {} bytes, got {}",
        len,
        b.len()
    );

    // Remove media.
    vm.modify_vtl2_settings(|v| {
        v.dynamic.as_mut().unwrap().storage_controllers[0].luns[0].physical_devices =
            build_vtl2_storage_backing_physical_devices(vec![])
    })
    .await
    .context("failed to modify vtl2 settings")?;

    ensure_no_medium(read_drive().await)?;

    hot_plug_send
        .call_failable(SimpleScsiDvdRequest::ChangeMedia, None)
        .await
        .context("failed to change media")?;

    agent.power_off().await?;
    drop(hot_plug_send);
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a SCSI DVD assigned to VTL2, using NVMe
/// backing, and vmbus relay. This should expose a DVD to VTL0 via vmbus.
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn openhcl_linux_storvsp_dvd_nvme(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    const NVME_INSTANCE: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");
    let vtl2_nsid = 1;
    let nvme_disk_sectors: u64 = 0x4000;
    let sector_size = 4096;

    let vtl2_lun = 5;
    let scsi_instance = Guid::new_random();

    let disk_len = nvme_disk_sectors * sector_size;
    let mut backing_file = tempfile::tempfile()?;
    let data_chunk: Vec<u8> = (0..64).collect();
    let data_chunk = data_chunk.as_slice();
    let mut bytes = vec![0_u8; disk_len as usize];
    bytes.chunks_exact_mut(64).for_each(|v| {
        v.copy_from_slice(data_chunk);
    });
    backing_file.write_all(&bytes)?;

    let (vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.vpci_devices.extend([new_test_vtl2_nvme_device(
                    vtl2_nsid,
                    disk_len,
                    NVME_INSTANCE,
                    Some(backing_file),
                )]);
            })
        })
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .with_instance_id(scsi_instance)
                .add_lun(
                    Vtl2LunBuilder::dvd()
                        .with_location(vtl2_lun)
                        .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                            ControllerType::Nvme,
                            NVME_INSTANCE,
                            vtl2_nsid,
                        )),
                )
                .build(),
        )
        .run()
        .await?;

    tracing::info!("VM is running, issuing read to dvd drive");

    let b = agent
        .read_file("/dev/sr0")
        .await
        .context("failed to read dvd drive")?;
    assert_eq!(
        b.len() as u64,
        disk_len,
        "expected {} bytes, got {}",
        disk_len,
        b.len()
    );
    assert_eq!(b[..], bytes[..], "content mismatch");

    tracing::info!("read complete and verified, powering off VM");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test an OpenHCL Linux direct VM with several NVMe namespaces assigned to VTL2, and
/// vmbus relay. This should expose the disks to VTL0 as SCSI via vmbus.
/// The disks are added and removed in a loop, dynamically after VM boot rather than being there at boot time.
#[openvmm_test(
    openhcl_linux_direct_x64,
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn storvsp_dynamic_add_disk(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> Result<(), anyhow::Error> {
    const NVME_INSTANCE: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");
    const NS_COUNT: u32 = 8;
    const FIRST_NS: u32 = 30;
    const FIRST_LUN: u32 = 0;
    const SECTOR_SIZE: u64 = 512;
    const NUM_ITERATIONS: u32 = 3;

    // 128MB for the first NS and 1MB extra for each subsequent NS
    const fn disk_sectors(index: u32) -> u64 {
        (128 + (index as u64)) * 1024 * 1024 / SECTOR_SIZE
    }

    let scsi_instance = Guid::new_random();

    // Assumptions made by test infra & routines:
    //
    // 1. Some test-infra added disks are 64MiB in size. Since we find disks by size,
    // ensure that our test disks are a different size.
    // 2. Disks under test need to be at least 100MiB for the IO tests (see [`test_storage_linux`]),
    // with some arbitrary buffer (5MiB in this case).
    static_assertions::const_assert!(disk_sectors(0) * SECTOR_SIZE > 105 * 1024 * 1024);

    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                // Create NVMe controller with all namespaces
                c.vpci_devices.push(VpciDeviceConfig {
                    vtl: DeviceVtl::Vtl2,
                    instance_id: NVME_INSTANCE,
                    resource: NvmeControllerHandle {
                        subsystem_id: NVME_INSTANCE,
                        max_io_queues: 64,
                        msix_count: 64,
                        namespaces: (0..NS_COUNT)
                            .map(|i| NamespaceDefinition {
                                nsid: FIRST_NS + i,
                                disk: LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                                    len: Some(disk_sectors(i) * SECTOR_SIZE),
                                    sector_size: None,
                                })
                                .into_resource(),
                                read_only: false,
                            })
                            .collect(),
                        requests: None,
                    }
                    .into_resource(),
                    vnode: None,
                });
            })
        })
        .with_custom_vtl2_settings(move |v| {
            v.dynamic.as_mut().unwrap().storage_controllers.push(
                Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                    .with_instance_id(scsi_instance)
                    // No disks are attached initially
                    .build(),
            )
        })
        .run()
        .await?;

    tracing::info!("Testing that no disks are present in the guest");
    test_storage_linux(&agent, scsi_instance, vec![]).await?;

    for iteration in 1..=NUM_ITERATIONS {
        // Now dynamically add disks
        tracing::info!("Dynamically adding disks to VTL2 settings {iteration}/{NUM_ITERATIONS}");
        vm.modify_vtl2_settings(|s| {
            s.dynamic.as_mut().unwrap().storage_controllers[0]
                .luns
                .extend((0..NS_COUNT).map(|i| {
                    Vtl2LunBuilder::disk()
                        .with_location(FIRST_LUN + i)
                        .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                            ControllerType::Nvme,
                            NVME_INSTANCE,
                            FIRST_NS + i,
                        ))
                        .build()
                }))
        })
        .await?;

        tracing::info!(
            "Testing presence and IO on all disks in guest {iteration}/{NUM_ITERATIONS}"
        );
        test_storage_linux(
            &agent,
            scsi_instance,
            (0..NS_COUNT)
                .map(|i| ExpectedGuestDevice {
                    lun: FIRST_LUN + i,
                    disk_size_sectors: disk_sectors(i) as usize,
                    friendly_name: format!("nvme{}", i),
                })
                .collect(),
        )
        .await?;

        tracing::info!(
            "Dynamically removing all disks from VTL2 settings {iteration}/{NUM_ITERATIONS}"
        );
        vm.modify_vtl2_settings(|s| {
            s.dynamic.as_mut().unwrap().storage_controllers[0]
                .luns
                .clear();
        })
        .await?;

        tracing::info!("Testing absence of disks in guest {iteration}/{NUM_ITERATIONS}");
        test_storage_linux(&agent, scsi_instance, vec![]).await?;
    }

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
