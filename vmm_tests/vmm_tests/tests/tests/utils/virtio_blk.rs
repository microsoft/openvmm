// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Disposable disks and verified guest I/O for virtio-blk tests.

use anyhow::Context;
use disk_backend_resources::FileDiskHandle;
use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use pal_async::DefaultDriver;
use pal_async::timer::PolledTimer;
use pipette_client::PipetteClient;
use pipette_client::cmd;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;

const DISK_SIZE: u64 = 64 * 1024 * 1024;
const PAYLOAD_OFFSET: u64 = 8 * 4096;

#[derive(Clone, Copy)]
pub(crate) enum DiskBacking {
    Ram,
    File,
    Vmdk,
    HostBlock { direct: bool },
}

pub(crate) struct TestDisk {
    backing: DiskBacking,
    file: tempfile::NamedTempFile,
    descriptor: Option<tempfile::NamedTempFile>,
    loop_device: Option<PathBuf>,
    loop_file: Option<std::fs::File>,
    loop_setup_pending: bool,
}

impl TestDisk {
    pub(crate) async fn new(backing: DiskBacking) -> anyhow::Result<Self> {
        let file = tempfile::Builder::new()
            .prefix("petri-virtio-blk-")
            .tempfile_in(std::env::current_dir()?)
            .context("create owned virtio-blk backing file")?;
        file.as_file().set_len(DISK_SIZE)?;
        let mut disk = Self {
            backing,
            file,
            descriptor: None,
            loop_device: None,
            loop_file: None,
            loop_setup_pending: false,
        };
        if matches!(backing, DiskBacking::Vmdk) {
            let mut descriptor = tempfile::Builder::new()
                .prefix("petri-virtio-blk-")
                .suffix(".vmdk")
                .tempfile_in(std::env::current_dir()?)
                .context("create owned VMDK descriptor")?;
            let source = disk
                .file
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .context("VMDK extent filename is not UTF-8")?;
            // Split inside the payload so verification crosses an extent boundary.
            let first_sectors = (PAYLOAD_OFFSET + 4096) / 512;
            let second_sectors = DISK_SIZE / 512 - first_sectors;
            write!(
                descriptor,
                "# Disk DescriptorFile\n\
                 version=1\n\
                 CID=fffffffe\n\
                 parentCID=ffffffff\n\
                 createType=\"twoGbMaxExtentFlat\"\n\
                 RW {first_sectors} FLAT \"{source}\" 0\n\
                 RW {second_sectors} FLAT \"{source}\" {first_sectors}\n"
            )
            .context("write owned VMDK descriptor")?;
            disk.descriptor = Some(descriptor);
        }
        if let DiskBacking::HostBlock { .. } = backing {
            anyhow::ensure!(cfg!(target_os = "linux"), "loop devices require Linux");
            let mut command = Command::new("losetup");
            command.args(["--find", "--show"]).arg(disk.file.path());
            disk.loop_setup_pending = true;
            let output = command
                .output()
                .context("attach owned virtio-blk backing file to a new loop device")?;
            anyhow::ensure!(
                output.status.success(),
                "attach owned loop device: {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            let output = String::from_utf8(output.stdout)?;
            let path = loop_device_path(output.trim())?;
            disk.loop_file = Some(std::fs::File::open(&path)?);
            disk.loop_device = Some(path);
            disk.loop_setup_pending = false;
            disk.verify_block_device()?;
            // Keep the device usable, but let the kernel clear it on last close.
            request_loop_detach(disk.path()?)?;
        }
        Ok(disk)
    }

    pub(crate) async fn resource(
        &self,
        read_only: bool,
    ) -> anyhow::Result<Resource<DiskHandleKind>> {
        Ok(match self.backing {
            DiskBacking::Ram => LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                len: Some(DISK_SIZE),
                sector_size: None,
            })
            .into_resource(),
            DiskBacking::File => FileDiskHandle(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(!read_only)
                    .open(self.file.path())
                    .context("open owned regular-file disk")?,
            )
            .into_resource(),
            DiskBacking::Vmdk => {
                let resource = openvmm_helpers::disk::open_disk_type(
                    self.path()?,
                    openvmm_helpers::disk::OpenDiskOptions {
                        read_only,
                        direct: false,
                    },
                )
                .await
                .context("open owned VMDK disk")?;
                anyhow::ensure!(
                    resource.id() == "vmdk_flat",
                    "VMDK test resolved to {}, not vmdk_flat",
                    resource.id()
                );
                resource
            }
            DiskBacking::HostBlock { direct } => {
                self.verify_block_device()?;
                let resource = openvmm_helpers::disk::open_disk_type(
                    self.path()?,
                    openvmm_helpers::disk::OpenDiskOptions { read_only, direct },
                )
                .await
                .context("open owned loop device as a host block disk")?;
                anyhow::ensure!(
                    resource.id() == "block",
                    "host block test resolved to {}, not block",
                    resource.id()
                );
                resource
            }
        })
    }

    pub(crate) fn path(&self) -> anyhow::Result<&Path> {
        match self.backing {
            DiskBacking::Ram => anyhow::bail!("RAM disks do not have a host path"),
            DiskBacking::File => Ok(self.file.path()),
            DiskBacking::Vmdk => self
                .descriptor
                .as_ref()
                .map(|file| file.path())
                .context("owned VMDK descriptor is missing"),
            DiskBacking::HostBlock { .. } => self
                .loop_device
                .as_deref()
                .context("owned loop device is not attached"),
        }
    }

    pub(crate) fn seed_backing(&self, expected: &[u8]) -> anyhow::Result<()> {
        let mut file = self
            .file
            .reopen()
            .context("open disk backing for seeding")?;
        file.seek(SeekFrom::Start(PAYLOAD_OFFSET))?;
        file.write_all(expected)
            .context("seed disk backing payload")?;
        file.sync_all().context("flush seeded disk backing")
    }

    pub(crate) fn verify_backing(&self, expected: &[u8]) -> anyhow::Result<()> {
        if matches!(self.backing, DiskBacking::Ram) {
            tracing::debug!("RAM disk has no persistent host payload to verify");
            return Ok(());
        }
        let mut file = std::fs::File::open(self.file.path())?;
        file.seek(SeekFrom::Start(PAYLOAD_OFFSET))?;
        let mut actual = vec![0; expected.len()];
        file.read_exact(&mut actual)
            .context("read flushed payload from owned backing file")?;
        anyhow::ensure!(actual == expected, "host backing payload changed");
        Ok(())
    }

    pub(crate) fn close(mut self) -> anyhow::Result<()> {
        self.detach()
    }

    fn verify_block_device(&self) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::FileTypeExt;

            let file = std::fs::File::open(self.path()?)
                .context("open descriptor for owned loop device")?;
            anyhow::ensure!(
                file.metadata()?.file_type().is_block_device(),
                "owned loop descriptor is not a block device"
            );
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        anyhow::bail!("loop devices require Linux")
    }

    fn detach(&mut self) -> anyhow::Result<()> {
        if self.loop_setup_pending && self.loop_device.is_none() {
            // Recover only devices associated with our unique file if setup
            // failed after losetup attached it but before we saved the path.
            let output = Command::new("losetup")
                .arg("--associated")
                .arg(self.file.path())
                .args(["--output", "NAME", "--noheadings"])
                .output()
                .context("find owned loop device after incomplete setup")?;
            anyhow::ensure!(
                output.status.success(),
                "find owned loop device: {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            let output = String::from_utf8(output.stdout)?;
            let path = output.trim();
            if !path.is_empty() {
                self.loop_device = Some(loop_device_path(path)?);
            }
            self.loop_setup_pending = false;
        }
        if let Some(path) = &self.loop_device {
            request_loop_detach(path)?;
            self.loop_file = None;
            self.loop_device = None;
        }
        Ok(())
    }
}

fn request_loop_detach(path: &Path) -> anyhow::Result<()> {
    let output = Command::new("losetup")
        .arg("--detach")
        .arg(path)
        .output()
        .with_context(|| format!("detach owned loop device {}", path.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "detach owned loop device {}: {}: {}",
        path.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn loop_device_path(path: &str) -> anyhow::Result<PathBuf> {
    let number = path
        .strip_prefix("/dev/loop")
        .context("losetup returned a path outside /dev/loop<digits>")?;
    anyhow::ensure!(
        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()),
        "invalid losetup device path: {path:?}"
    );
    Ok(PathBuf::from(path))
}

impl Drop for TestDisk {
    fn drop(&mut self) {
        if let Err(error) = self.detach() {
            tracing::error!(?error, device = ?self.loop_device, "failed to detach owned test loop device");
        }
    }
}

pub(crate) fn test_pattern(seed: u8) -> Vec<u8> {
    (0usize..1024 * 1024)
        .map(|index| (index.wrapping_mul(37) ^ (index / 4096)) as u8 ^ seed)
        .collect()
}

enum DiskScan {
    Complete(Option<String>),
    Pending,
}

impl DiskScan {
    fn require_disk(self, serial: &str) -> anyhow::Result<String> {
        match self {
            Self::Complete(name) => {
                name.with_context(|| format!("virtio-blk disk with serial {serial:?} not found"))
            }
            Self::Pending => anyhow::bail!("virtio-blk serial scan is pending for {serial:?}"),
        }
    }
}

async fn find_disk(agent: &PipetteClient, serial: &str) -> anyhow::Result<DiskScan> {
    let shell = agent.unix_shell();
    let script = r#"
for path in /sys/block/vd*/serial; do
    [ -e "$path" ] || continue
    name=${path%/serial}
    name=${name##*/}
    if value=$(cat "$path"); then
        printf '%s\t%s\n' "$name" "$value"
    else
        printf 'scan-pending\t%s\n' "$name"
    fi
done
echo scan-complete
"#;
    let output = cmd!(shell, "sh -c {script}")
        .read()
        .await
        .context("scan virtio-blk serials")?;
    let disks = output
        .strip_suffix("scan-complete")
        .context("incomplete virtio-blk serial scan")?;
    let mut found = None;
    let mut pending = false;
    for line in disks.lines() {
        if let Some(name) = line.strip_prefix("scan-pending\t") {
            tracing::debug!(
                disk = name,
                "virtio-blk serial read failed; scan is pending"
            );
            pending = true;
            continue;
        }
        let (name, actual_serial) = line
            .split_once('\t')
            .context("invalid virtio-blk serial scan entry")?;
        anyhow::ensure!(
            name.strip_prefix("vd").is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_lowercase())
            }),
            "unexpected virtio-blk basename: {name:?}"
        );
        if actual_serial == serial {
            anyhow::ensure!(
                found.is_none(),
                "multiple virtio-blk disks have serial {serial:?}"
            );
            found = Some(name.to_owned());
        }
    }
    Ok(if pending {
        DiskScan::Pending
    } else {
        DiskScan::Complete(found)
    })
}

pub(crate) async fn verify_io(
    agent: &PipetteClient,
    serial: &str,
    expected: &[u8],
    read_only: bool,
) -> anyhow::Result<()> {
    let shell = agent.unix_shell();
    let name = find_disk(agent, serial).await?.require_disk(serial)?;
    let sectors: u64 = cmd!(shell, "cat /sys/block/{name}/size")
        .read()
        .await?
        .trim()
        .parse()
        .context("parse virtio-blk capacity")?;
    anyhow::ensure!(
        sectors * 512 >= PAYLOAD_OFFSET + expected.len() as u64,
        "virtio-blk disk is too small"
    );
    let ro = cmd!(shell, "cat /sys/block/{name}/ro").read().await?;
    anyhow::ensure!(
        ro.trim() == if read_only { "1" } else { "0" },
        "unexpected virtio-blk read-only flag: {ro:?}"
    );
    anyhow::ensure!(
        !expected.is_empty() && expected.len().is_multiple_of(4096),
        "payload must contain complete 4096-byte blocks"
    );
    let mode = if read_only { "ro" } else { "rw" };
    let input = format!("/virtio-blk-{serial}-{mode}-input");
    let write_payload = if read_only {
        expected.iter().map(|byte| byte ^ 0xff).collect::<Vec<_>>()
    } else {
        expected.to_vec()
    };
    agent
        .write_file(&input, write_payload.as_slice())
        .await
        .context("stage virtio-blk write payload")?;
    if read_only {
        let script = format!(
            "if dd if={input} of=/dev/{name} bs=4096 seek=8 oflag=direct conv=notrunc,fsync; then echo write-succeeded; else echo write-rejected; fi"
        );
        let status = cmd!(shell, "sh -c {script}").read().await?;
        anyhow::ensure!(
            status.trim() == "write-rejected",
            "read-only disk accepted a write: {status:?}"
        );
    } else {
        cmd!(
            shell,
            "dd if={input} of=/dev/{name} bs=4096 seek=8 oflag=direct conv=notrunc,fsync"
        )
        .run()
        .await
        .context("write and flush virtio-blk payload")?;
    }
    verify_payload(agent, serial, expected).await?;
    cmd!(shell, "rm {input}").run().await?;
    Ok(())
}

pub(crate) async fn verify_payload(
    agent: &PipetteClient,
    serial: &str,
    expected: &[u8],
) -> anyhow::Result<()> {
    let shell = agent.unix_shell();
    let name = find_disk(agent, serial).await?.require_disk(serial)?;
    anyhow::ensure!(
        !expected.is_empty() && expected.len().is_multiple_of(4096),
        "payload must contain complete 4096-byte blocks"
    );
    let output = format!("/virtio-blk-{serial}-readback");
    let blocks = (expected.len() / 4096).to_string();
    cmd!(
        shell,
        "dd if=/dev/{name} of={output} bs=4096 skip=8 count={blocks} iflag=direct"
    )
    .run()
    .await
    .context("read virtio-blk payload with direct I/O")?;
    let actual = agent
        .read_file(&output)
        .await
        .context("retrieve virtio-blk read payload")?;
    anyhow::ensure!(actual == expected, "virtio-blk payload changed");
    cmd!(shell, "rm {output}").run().await?;
    Ok(())
}

pub(crate) async fn wait_for_disk(
    agent: &PipetteClient,
    serial: &str,
    present: bool,
    driver: &DefaultDriver,
) -> anyhow::Result<()> {
    let shell = agent.unix_shell();
    let mut timer = PolledTimer::new(driver);
    for _ in 0..60 {
        match find_disk(agent, serial).await? {
            DiskScan::Complete(None) if !present => return Ok(()),
            DiskScan::Complete(Some(name)) if present => {
                let script =
                    format!("if [ -b /dev/{name} ]; then echo present; else echo pending; fi");
                let state = cmd!(shell, "sh -c {script}")
                    .read()
                    .await
                    .context("query virtio-blk device node presence")?;
                match state.trim() {
                    "present" => return Ok(()),
                    "pending" => {}
                    _ => anyhow::bail!("unexpected virtio-blk presence response: {state:?}"),
                }
            }
            _ => {}
        }
        timer.sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("timed out waiting for virtio-blk serial={serial:?} present={present}")
}

pub(crate) async fn quiesce_disk(agent: &PipetteClient, serial: &str) -> anyhow::Result<String> {
    let name = find_disk(agent, serial).await?.require_disk(serial)?;
    let shell = agent.unix_shell();
    let path = format!("/sys/block/{name}/device");
    let target = cmd!(shell, "readlink -f {path}").read().await?;
    let pci_path = Path::new(target.trim())
        .parent()
        .context("virtio device has no PCI parent")?;
    let bdf = pci_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("virtio PCI parent has no UTF-8 device name")?;
    anyhow::ensure!(
        bdf.contains(':') && bdf.contains('.'),
        "unexpected virtio PCI parent: {}",
        pci_path.display()
    );
    // Reset the virtio device while its MMIO registers are still available.
    let script = "printf '%s' \"$1\" > /sys/bus/pci/drivers/virtio-pci/unbind";
    cmd!(shell, "sh -c {script} sh {bdf}")
        .run()
        .await
        .context("unbind guest virtio-pci driver before removal")?;
    Ok(pci_path.to_string_lossy().into_owned())
}

pub(crate) async fn wait_for_pci_removal(
    agent: &PipetteClient,
    path: &str,
    driver: &DefaultDriver,
) -> anyhow::Result<()> {
    let shell = agent.unix_shell();
    let mut timer = PolledTimer::new(driver);
    let script = "if [ -e \"$1\" ]; then echo present; else echo absent; fi";
    for _ in 0..60 {
        let state = cmd!(shell, "sh -c {script} sh {path}").read().await?;
        match state.trim() {
            "absent" => return Ok(()),
            "present" => {
                timer.sleep(Duration::from_millis(500)).await;
            }
            _ => anyhow::bail!("unexpected PCI removal response: {state:?}"),
        }
    }
    anyhow::bail!("timed out waiting for PCI device removal: {path}")
}
