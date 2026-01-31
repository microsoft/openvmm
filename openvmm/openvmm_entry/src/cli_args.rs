// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CLI argument parsing.
//!
//! Code in this module must not instantiate any complex VM objects!
//!
//! In other words, this module is only responsible for marshalling raw CLI
//! strings into typed Rust structs/enums, and should consist of entirely _pure
//! functions_.
//!
//! e.g: instead of opening a `File` directly, parse the specified file path
//! into a `PathBuf`, and allow later parts of the init flow to handle opening
//! the file.

// NOTE: This module itself is not pub, but the Options struct below is
//       re-exported as pub in main to make this lint fire. It won't fire on
//       anything else on this file though.
#![warn(missing_docs)]

use anyhow::Context;
use clap::Parser;
use clap::ValueEnum;
use openvmm_defs::config::DEFAULT_PCAT_BOOT_ORDER;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::Hypervisor;
use openvmm_defs::config::PcatBootDevice;
use openvmm_defs::config::Vtl2BaseAddressType;
use openvmm_defs::config::X2ApicConfig;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use thiserror::Error;

/// OpenVMM virtual machine monitor.
///
/// This is not yet a stable interface and may change radically between
/// versions.
#[derive(Parser)]
pub struct Options {
    /// processor count
    #[clap(short = 'p', long, value_name = "COUNT", default_value = "1")]
    pub processors: u32,

    /// guest RAM size
    #[clap(
        short = 'm',
        long,
        value_name = "SIZE",
        default_value = "1GB",
        value_parser = parse_memory
    )]
    pub memory: u64,

    /// use shared memory segment
    #[clap(short = 'M', long)]
    pub shared_memory: bool,

    /// prefetch guest RAM
    #[clap(long)]
    pub prefetch: bool,

    /// start in paused state
    #[clap(short = 'P', long)]
    pub paused: bool,

    /// kernel image (when using linux direct boot)
    #[clap(short = 'k', long, value_name = "FILE", default_value = default_value_from_arch_env("OPENVMM_LINUX_DIRECT_KERNEL"))]
    pub kernel: OptionalPathBuf,

    /// initrd image (when using linux direct boot)
    #[clap(short = 'r', long, value_name = "FILE", default_value = default_value_from_arch_env("OPENVMM_LINUX_DIRECT_INITRD"))]
    pub initrd: OptionalPathBuf,

    /// extra kernel command line args
    #[clap(short = 'c', long, value_name = "STRING")]
    pub cmdline: Vec<String>,

    /// enable HV#1 capabilities
    #[clap(long)]
    pub hv: bool,

    /// enable vtl2 - only supported in WHP and simulated without hypervisor support currently
    ///
    /// Currently implies --get.
    #[clap(long, requires("hv"))]
    pub vtl2: bool,

    /// Add GET and related devices for using the OpenHCL paravisor to the
    /// highest enabled VTL.
    #[clap(long, requires("hv"))]
    pub get: bool,

    /// Disable GET and related devices for using the OpenHCL paravisor, even
    /// when --vtl2 is passed.
    #[clap(long, conflicts_with("get"))]
    pub no_get: bool,

    /// disable the VTL0 alias map presented to VTL2 by default
    #[clap(long, requires("vtl2"))]
    pub no_alias_map: bool,

    /// enable isolation emulation
    #[clap(long, requires("vtl2"))]
    pub isolation: Option<IsolationCli>,

    /// the hybrid vsock listener path
    #[clap(long, value_name = "PATH")]
    pub vsock_path: Option<String>,

    /// the VTL2 hybrid vsock listener path
    #[clap(long, value_name = "PATH", requires("vtl2"))]
    pub vtl2_vsock_path: Option<String>,

    /// the late map vtl0 ram access policy when vtl2 is enabled
    #[clap(long, requires("vtl2"), default_value = "halt")]
    pub late_map_vtl0_policy: Vtl0LateMapPolicyCli,

    /// disable in-hypervisor enlightenment implementation (where possible)
    #[clap(long)]
    pub no_enlightenments: bool,

    /// disable the in-hypervisor APIC and use the user-mode one (where possible)
    #[clap(long)]
    pub user_mode_apic: bool,

    /// attach a disk (can be passed multiple times)
    #[clap(long_help = r#"
e.g: --disk memdiff:file:/path/to/disk.vhd

syntax: <path> | kind:<arg>[,flag,opt=arg,...]

valid disk kinds:
    `mem:<len>`                    memory backed disk
        <len>: length of ramdisk, e.g.: `1G`
    `memdiff:<disk>`               memory backed diff disk
        <disk>: lower disk, e.g.: `file:base.img`
    `file:<path>`                  file-backed disk
        <path>: path to file

flags:
    `ro`                           open disk as read-only
    `dvd`                          specifies that device is cd/dvd and it is read_only
    `vtl2`                         assign this disk to VTL2
    `uh`                           relay this disk to VTL0 through SCSI-to-OpenHCL (show to VTL0 as SCSI)
    `uh-nvme`                      relay this disk to VTL0 through NVMe-to-OpenHCL (show to VTL0 as SCSI)
"#)]
    #[clap(long, value_name = "FILE")]
    pub disk: Vec<DiskCli>,

    /// attach a disk via an NVMe controller
    #[clap(long_help = r#"
e.g: --nvme memdiff:file:/path/to/disk.vhd

syntax: <path> | kind:<arg>[,flag,opt=arg,...]

valid disk kinds:
    `mem:<len>`                    memory backed disk
        <len>: length of ramdisk, e.g.: `1G`
    `memdiff:<disk>`               memory backed diff disk
        <disk>: lower disk, e.g.: `file:base.img`
    `file:<path>`                  file-backed disk
        <path>: path to file

flags:
    `ro`                           open disk as read-only
    `vtl2`                         assign this disk to VTL2
    `uh`                           relay this disk to VTL0 through SCSI-to-OpenHCL (show to VTL0 as NVMe)
    `uh-nvme`                      relay this disk to VTL0 through NVMe-to-OpenHCL (show to VTL0 as NVMe)

options:
    `pcie_port=<name>`             present the disk using pcie under the specified port, incompatible with `vtl2`, `uh`, and `uh-nvme`
"#)]
    #[clap(long)]
    pub nvme: Vec<DiskCli>,

    /// number of sub-channels for the SCSI controller
    #[clap(long, value_name = "COUNT", default_value = "0")]
    pub scsi_sub_channels: u16,

    /// expose a virtual NIC
    #[clap(long)]
    pub nic: bool,

    /// expose a virtual NIC with the given backend (consomme | dio | tap | none)
    ///
    /// Prefix with `uh:` to add this NIC via Mana emulation through OpenHCL,
    /// or `vtl2:` to assign this NIC to VTL2.
    #[clap(long)]
    pub net: Vec<NicConfigCli>,

    /// expose a virtual NIC using the Windows kernel-mode vmswitch.
    ///
    /// Specify the switch ID or "default" for the default switch.
    #[clap(long, value_name = "SWITCH_ID")]
    pub kernel_vmnic: Vec<String>,

    /// expose a graphics device
    #[clap(long)]
    pub gfx: bool,

    /// support a graphics device in vtl2
    #[clap(long, requires("vtl2"), conflicts_with("gfx"))]
    pub vtl2_gfx: bool,

    /// listen for vnc connections. implied by gfx.
    #[clap(long)]
    pub vnc: bool,

    /// VNC port number
    #[clap(long, value_name = "PORT", default_value = "5900")]
    pub vnc_port: u16,

    /// set the APIC ID offset, for testing APIC IDs that don't match VP index
    #[cfg(guest_arch = "x86_64")]
    #[clap(long, default_value_t)]
    pub apic_id_offset: u32,

    /// the maximum number of VPs per socket
    #[clap(long)]
    pub vps_per_socket: Option<u32>,

    /// enable or disable SMT (hyperthreading) (auto | force | off)
    #[clap(long, default_value = "auto")]
    pub smt: SmtConfigCli,

    /// configure x2apic (auto | supported | off | on)
    #[cfg(guest_arch = "x86_64")]
    #[clap(long, default_value = "auto", value_parser = parse_x2apic)]
    pub x2apic: X2ApicConfig,

    /// use virtio console
    #[clap(long)]
    pub virtio_console: bool,

    /// use virtio console enumerated via VPCI
    #[clap(long, conflicts_with("virtio_console"))]
    pub virtio_console_pci: bool,

    /// COM1 binding (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[clap(long, value_name = "SERIAL")]
    pub com1: Option<SerialConfigCli>,

    /// COM2 binding (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[clap(long, value_name = "SERIAL")]
    pub com2: Option<SerialConfigCli>,

    /// COM3 binding (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[clap(long, value_name = "SERIAL")]
    pub com3: Option<SerialConfigCli>,

    /// COM4 binding (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[clap(long, value_name = "SERIAL")]
    pub com4: Option<SerialConfigCli>,

    /// virtio serial binding (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[clap(long, value_name = "SERIAL")]
    pub virtio_serial: Option<SerialConfigCli>,

    /// vmbus com1 serial binding (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[structopt(long, value_name = "SERIAL")]
    pub vmbus_com1_serial: Option<SerialConfigCli>,

    /// vmbus com2 serial binding (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[structopt(long, value_name = "SERIAL")]
    pub vmbus_com2_serial: Option<SerialConfigCli>,

    /// debugcon binding (port:serial, where port is a u16, and serial is (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none))
    #[clap(long, value_name = "SERIAL")]
    pub debugcon: Option<DebugconSerialConfigCli>,

    /// boot UEFI firmware
    #[clap(long, short = 'e')]
    pub uefi: bool,

    /// UEFI firmware file
    #[clap(long, requires("uefi"), conflicts_with("igvm"), value_name = "FILE", default_value = default_value_from_arch_env("OPENVMM_UEFI_FIRMWARE"))]
    pub uefi_firmware: OptionalPathBuf,

    /// enable UEFI debugging on COM1
    #[clap(long, requires("uefi"))]
    pub uefi_debug: bool,

    /// enable memory protections in UEFI
    #[clap(long, requires("uefi"))]
    pub uefi_enable_memory_protections: bool,

    /// set PCAT boot order as comma-separated string of boot device types
    /// (e.g: floppy,hdd,optical,net).
    ///
    /// If less than 4 entries are added, entries are added according to their
    /// default boot order (optical,hdd,net,floppy)
    ///
    /// e.g: passing "floppy,optical" will result in a boot order equivalent to
    /// "floppy,optical,hdd,net".
    ///
    /// Passing duplicate types is an error.
    #[clap(long, requires("pcat"))]
    pub pcat_boot_order: Option<PcatBootOrderCli>,

    /// Boot with PCAT BIOS firmware and piix4 devices
    #[clap(long, conflicts_with("uefi"))]
    pub pcat: bool,

    /// PCAT firmware file
    #[clap(long, requires("pcat"), value_name = "FILE")]
    pub pcat_firmware: Option<PathBuf>,

    /// boot IGVM file
    #[clap(long, conflicts_with("kernel"), value_name = "FILE")]
    pub igvm: Option<PathBuf>,

    /// specify igvm vtl2 relocation type
    /// (absolute=\<addr\>, disable, auto=\<filesize,or memory size\>, vtl2=\<filesize,or memory size\>,)
    #[clap(long, requires("igvm"), default_value = "auto=filesize", value_parser = parse_vtl2_relocation)]
    pub igvm_vtl2_relocation_type: Vtl2BaseAddressType,

    /// add a virtio_9p device (e.g. myfs,C:\)
    #[clap(long, value_name = "tag,root_path")]
    pub virtio_9p: Vec<FsArgs>,

    /// output debug info from the 9p server
    #[clap(long)]
    pub virtio_9p_debug: bool,

    /// add a virtio_fs device (e.g. myfs,C:\,uid=1000,gid=2000)
    #[clap(long, value_name = "tag,root_path,[options]")]
    pub virtio_fs: Vec<FsArgsWithOptions>,

    /// add a virtio_fs device for sharing memory (e.g. myfs,\SectionDirectoryPath)
    #[clap(long, value_name = "tag,root_path")]
    pub virtio_fs_shmem: Vec<FsArgs>,

    /// add a virtio_fs device under either the PCI or MMIO bus, or whatever the hypervisor supports (pci | mmio | auto)
    #[clap(long, value_name = "BUS", default_value = "auto")]
    pub virtio_fs_bus: VirtioBusCli,

    /// virtio PMEM device
    #[clap(long, value_name = "PATH")]
    pub virtio_pmem: Option<String>,

    /// expose a virtio network with the given backend (dio | vmnic | tap |
    /// none)
    ///
    /// Prefix with `uh:` to add this NIC via Mana emulation through OpenHCL,
    /// or `vtl2:` to assign this NIC to VTL2.
    #[clap(long)]
    pub virtio_net: Vec<NicConfigCli>,

    /// send log output from the worker process to a file instead of stderr. the file will be overwritten.
    #[clap(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    /// run as a ttrpc server on the specified Unix socket
    #[clap(long, value_name = "SOCKETPATH")]
    pub ttrpc: Option<PathBuf>,

    /// run as a grpc server on the specified Unix socket
    #[clap(long, value_name = "SOCKETPATH", conflicts_with("ttrpc"))]
    pub grpc: Option<PathBuf>,

    /// do not launch child processes
    #[clap(long)]
    pub single_process: bool,

    /// device to assign (can be passed multiple times)
    #[cfg(windows)]
    #[clap(long, value_name = "PATH")]
    pub device: Vec<String>,

    /// instead of showing the frontpage the VM will shutdown instead
    #[clap(long, requires("uefi"))]
    pub disable_frontpage: bool,

    /// add a vtpm device
    #[clap(long)]
    pub tpm: bool,

    /// the mesh worker host name.
    ///
    /// Used internally for debugging and diagnostics.
    #[clap(long, default_value = "control", hide(true))]
    #[expect(clippy::option_option)]
    pub internal_worker: Option<Option<String>>,

    /// redirect the VTL 0 vmbus control plane to a proxy in VTL 2.
    #[clap(long, requires("vtl2"))]
    pub vmbus_redirect: bool,

    /// limit the maximum protocol version allowed by vmbus; used for testing purposes
    #[clap(long, value_parser = vmbus_core::parse_vmbus_version)]
    pub vmbus_max_version: Option<u32>,

    /// The disk to use for the VMGS.
    ///
    /// If this is not provided, guest state will be stored in memory.
    #[clap(long_help = r#"
e.g: --vmgs memdiff:file:/path/to/file.vmgs

syntax: <path> | kind:<arg>[,flag]

valid disk kinds:
    `mem:<len>`                     memory backed disk
        <len>: length of ramdisk, e.g.: `1G` or `VMGS_DEFAULT`
    `memdiff:<disk>[;create=<len>]` memory backed diff disk
        <disk>: lower disk, e.g.: `file:base.img`
    `file:<path>`                   file-backed disk
        <path>: path to file

flags:
    `fmt`                           reprovision the VMGS before boot
    `fmt-on-fail`                   reprovision the VMGS before boot if it is corrupted
"#)]
    #[clap(long)]
    pub vmgs: Option<VmgsCli>,

    /// Use GspById guest state encryption policy with a test seed
    #[clap(long, requires("vmgs"))]
    pub test_gsp_by_id: bool,

    /// VGA firmware file
    #[clap(long, requires("pcat"), value_name = "FILE")]
    pub vga_firmware: Option<PathBuf>,

    /// enable secure boot
    #[clap(long)]
    pub secure_boot: bool,

    /// use secure boot template
    #[clap(long)]
    pub secure_boot_template: Option<SecureBootTemplateCli>,

    /// custom uefi nvram json file
    #[clap(long, value_name = "PATH")]
    pub custom_uefi_json: Option<PathBuf>,

    /// the path to a named pipe (Windows) or Unix socket (Linux) to relay to the connected
    /// tty.
    ///
    /// This is a hidden argument used internally.
    #[clap(long, hide(true))]
    pub relay_console_path: Option<PathBuf>,

    /// the title of the console window spawned from the relay console.
    ///
    /// This is a hidden argument used internally.
    #[clap(long, hide(true))]
    pub relay_console_title: Option<String>,

    /// enable in-hypervisor gdb debugger
    #[clap(long, value_name = "PORT")]
    pub gdb: Option<u16>,

    /// enable emulated MANA devices with the given network backend (see --net)
    #[clap(long)]
    pub mana: Vec<NicConfigCli>,

    /// use a specific hypervisor interface
    #[clap(long, value_parser = parse_hypervisor)]
    pub hypervisor: Option<Hypervisor>,

    /// (dev utility) boot linux using a custom (raw) DSDT table.
    ///
    /// This is a _very_ niche utility, and it's unlikely you'll need to use it.
    ///
    /// e.g: this flag helped bring up certain Hyper-V Generation 1 legacy
    /// devices without needing to port the associated ACPI code into OpenVMM's
    /// DSDT builder.
    #[clap(long, value_name = "FILE", conflicts_with_all(&["uefi", "pcat", "igvm"]))]
    pub custom_dsdt: Option<PathBuf>,

    /// attach an ide drive (can be passed multiple times)
    ///
    /// Each ide controller has two channels. Each channel can have up to two
    /// attachments.
    ///
    /// If the `s` flag is not passed then the drive will we be attached to the
    /// primary ide channel if space is available. If two attachments have already
    /// been added to the primary channel then the drive will be attached to the
    /// secondary channel.
    #[clap(long_help = r#"
e.g: --ide memdiff:file:/path/to/disk.vhd

syntax: <path> | kind:<arg>[,flag,opt=arg,...]

valid disk kinds:
    `mem:<len>`                    memory backed disk
        <len>: length of ramdisk, e.g.: `1G`
    `memdiff:<disk>`               memory backed diff disk
        <disk>: lower disk, e.g.: `file:base.img`
    `file:<path>`                  file-backed disk
        <path>: path to file

flags:
    `ro`                           open disk as read-only
    `s`                            attach drive to secondary ide channel
    `dvd`                          specifies that device is cd/dvd and it is read_only
"#)]
    #[clap(long, value_name = "FILE", requires("pcat"))]
    pub ide: Vec<IdeDiskCli>,

    /// attach a floppy drive (should be able to be passed multiple times). VM must be generation 1 (no UEFI)
    ///
    #[clap(long_help = r#"
e.g: --floppy memdiff:/path/to/disk.vfd,ro

syntax: <path> | kind:<arg>[,flag,opt=arg,...]

valid disk kinds:
    `mem:<len>`                    memory backed disk
        <len>: length of ramdisk, e.g.: `1G`
    `memdiff:<disk>`               memory backed diff disk
        <disk>: lower disk, e.g.: `file:base.img`
    `file:<path>`                  file-backed disk
        <path>: path to file

flags:
    `ro`                           open disk as read-only
"#)]
    #[clap(long, value_name = "FILE", requires("pcat"))]
    pub floppy: Vec<FloppyDiskCli>,

    /// enable guest watchdog device
    #[clap(long)]
    pub guest_watchdog: bool,

    /// enable OpenHCL's guest crash dump device, targeting the specified path
    #[clap(long)]
    pub openhcl_dump_path: Option<PathBuf>,

    /// halt the VM when the guest requests a reset, instead of resetting it
    #[clap(long)]
    pub halt_on_reset: bool,

    /// write saved state .proto files to the specified path
    #[clap(long)]
    pub write_saved_state_proto: Option<PathBuf>,

    /// specify the IMC hive file for booting Windows
    #[clap(long)]
    pub imc: Option<PathBuf>,

    /// Expose MCR device
    #[clap(long)]
    pub mcr: bool, // TODO MCR: support closed source CLI flags

    /// expose a battery device
    #[clap(long)]
    pub battery: bool,

    /// set the uefi console mode
    #[clap(long)]
    pub uefi_console_mode: Option<UefiConsoleModeCli>,

    /// set the EFI diagnostics log level
    #[clap(long_help = r#"
Set the EFI diagnostics log level.

options:
    default                        default (ERROR and WARN only)
    info                           info (ERROR, WARN, and INFO)
    full                           full (all log levels)
"#)]
    #[clap(long, requires("uefi"))]
    pub efi_diagnostics_log_level: Option<EfiDiagnosticsLogLevelCli>,

    /// Perform a default boot even if boot entries exist and fail
    #[clap(long)]
    pub default_boot_always_attempt: bool,

    /// Attach a PCI Express root complex to the VM
    #[clap(long_help = r#"
Attach root complexes to the VM.

Examples:
    # Attach root complex rc0 on segment 0 with bus and MMIO ranges
    --pcie-root-complex rc0,segment=0,start_bus=0,end_bus=255,low_mmio=4M,high_mmio=1G

Syntax: <name>[,opt=arg,...]

Options:
    `segment=<value>`              configures the PCI Express segment, default 0
    `start_bus=<value>`            lowest valid bus number, default 0
    `end_bus=<value>`              highest valid bus number, default 255
    `low_mmio=<size>`              low MMIO window size, default 4M
    `high_mmio=<size>`             high MMIO window size, default 1G
"#)]
    #[clap(long, conflicts_with("pcat"))]
    pub pcie_root_complex: Vec<PcieRootComplexCli>,

    /// Attach a PCI Express root port to the VM
    #[clap(long_help = r#"
Attach root ports to root complexes.

Examples:
    # Attach root port rc0rp0 to root complex rc0
    --pcie-root-port rc0:rc0rp0

    # Attach root port rc0rp1 to root complex rc0 with hotplug support
    --pcie-root-port rc0:rc0rp1,hotplug

Syntax: <root_complex_name>:<name>[,hotplug]

Options:
    `hotplug`                      enable hotplug support for this root port
"#)]
    #[clap(long, conflicts_with("pcat"))]
    pub pcie_root_port: Vec<PcieRootPortCli>,

    /// Attach a PCI Express switch to the VM
    #[clap(long_help = r#"
Attach switches to root ports or downstream switch ports to create PCIe hierarchies.

Examples:
    # Connect switch0 (with 4 downstream switch ports) directly to root port rp0
    --pcie-switch rp0:switch0,num_downstream_ports=4

    # Connect switch1 (with 2 downstream switch ports) to downstream port 0 of switch0
    --pcie-switch switch0-downstream-0:switch1,num_downstream_ports=2

    # Create a 3-level hierarchy: rp0 -> switch0 -> switch1 -> switch2
    --pcie-switch rp0:switch0
    --pcie-switch switch0-downstream-0:switch1
    --pcie-switch switch1-downstream-1:switch2

    # Enable hotplug on all downstream switch ports of switch0
    --pcie-switch rp0:switch0,hotplug

Syntax: <port_name>:<name>[,opt,opt=arg,...]

    port_name can be:
        - Root port name (e.g., "rp0") to connect directly to a root port
        - Downstream port name (e.g., "switch0-downstream-1") to connect to another switch

Options:
    `hotplug`                       enable hotplug support for all downstream switch ports
    `num_downstream_ports=<value>`  number of downstream ports, default 4
"#)]
    #[clap(long, conflicts_with("pcat"))]
    pub pcie_switch: Vec<GenericPcieSwitchCli>,

    /// Attach a PCIe remote device to a downstream port
    #[clap(long_help = r#"
Attach PCIe devices to root ports or downstream switch ports
which are implemented in a simulator running in a remote process.

Examples:
    # Attach to root port rc0rp0 with default socket
    --pcie-remote rc0rp0

    # Attach with custom socket path
    --pcie-remote rc0rp0,socket=/tmp/custom.sock

    # Specify HU and controller identifiers
    --pcie-remote rc0rp0,hu=1,controller=0

    # Multiple devices on different ports
    --pcie-remote rc0rp0,socket=/tmp/dev0.sock
    --pcie-remote rc0rp1,socket=/tmp/dev1.sock

Syntax: <port_name>[,opt=arg,...]

Options:
    `socket=<path>`                 Unix socket path (default: /tmp/qemu-pci-remote-0-ep.sock)
    `hu=<value>`                    Hardware unit identifier (default: 0)
    `controller=<value>`            Controller identifier (default: 0)
"#)]
    #[clap(long, conflicts_with("pcat"))]
    pub pcie_remote: Vec<PcieRemoteCli>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FsArgs {
    pub tag: String,
    pub path: String,
}

impl FromStr for FsArgs {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut s = s.split(',');
        let (Some(tag), Some(path), None) = (s.next(), s.next(), s.next()) else {
            anyhow::bail!("expected <tag>,<path>");
        };
        Ok(Self {
            tag: tag.to_owned(),
            path: path.to_owned(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FsArgsWithOptions {
    /// The file system tag.
    pub tag: String,
    /// The root path.
    pub path: String,
    /// The extra options, joined with ';'.
    pub options: String,
}

impl FromStr for FsArgsWithOptions {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut s = s.split(',');
        let (Some(tag), Some(path)) = (s.next(), s.next()) else {
            anyhow::bail!("expected <tag>,<path>[,<options>]");
        };
        let options = s.collect::<Vec<_>>().join(";");
        Ok(Self {
            tag: tag.to_owned(),
            path: path.to_owned(),
            options,
        })
    }
}

#[derive(Copy, Clone, clap::ValueEnum)]
pub enum VirtioBusCli {
    Auto,
    Mmio,
    Pci,
    Vpci,
}

#[derive(clap::ValueEnum, Clone, Copy)]
pub enum SecureBootTemplateCli {
    Windows,
    UefiCa,
}

fn parse_memory(s: &str) -> anyhow::Result<u64> {
    if s == "VMGS_DEFAULT" {
        Ok(vmgs_format::VMGS_DEFAULT_CAPACITY)
    } else {
        || -> Option<u64> {
            let mut b = s.as_bytes();
            if s.ends_with('B') {
                b = &b[..b.len() - 1]
            }
            if b.is_empty() {
                return None;
            }
            let multi = match b[b.len() - 1] as char {
                'T' => Some(1024 * 1024 * 1024 * 1024),
                'G' => Some(1024 * 1024 * 1024),
                'M' => Some(1024 * 1024),
                'K' => Some(1024),
                _ => None,
            };
            if multi.is_some() {
                b = &b[..b.len() - 1]
            }
            let n: u64 = std::str::from_utf8(b).ok()?.parse().ok()?;
            Some(n * multi.unwrap_or(1))
        }()
        .with_context(|| format!("invalid memory size '{0}'", s))
    }
}

/// Parse a number from a string that could be prefixed with 0x to indicate hex.
fn parse_number(s: &str) -> Result<u64, std::num::ParseIntError> {
    match s.strip_prefix("0x") {
        Some(rest) => u64::from_str_radix(rest, 16),
        None => s.parse::<u64>(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DiskCliKind {
    // mem:<len>
    Memory(u64),
    // memdiff:<kind>
    MemoryDiff(Box<DiskCliKind>),
    // sql:<path>[;create=<len>]
    Sqlite {
        path: PathBuf,
        create_with_len: Option<u64>,
    },
    // sqldiff:<path>[;create]:<kind>
    SqliteDiff {
        path: PathBuf,
        create: bool,
        disk: Box<DiskCliKind>,
    },
    // autocache:[key]:<kind>
    AutoCacheSqlite {
        cache_path: String,
        key: Option<String>,
        disk: Box<DiskCliKind>,
    },
    // prwrap:<kind>
    PersistentReservationsWrapper(Box<DiskCliKind>),
    // file:<path>[;create=<len>]
    File {
        path: PathBuf,
        create_with_len: Option<u64>,
    },
    // blob:<type>:<url>
    Blob {
        kind: BlobKind,
        url: String,
    },
    // crypt:<cipher>:<key_file>:<kind>
    Crypt {
        cipher: DiskCipher,
        key_file: PathBuf,
        disk: Box<DiskCliKind>,
    },
    // delay:<delay_ms>:<kind>
    DelayDiskWrapper {
        delay_ms: u64,
        disk: Box<DiskCliKind>,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq)]
pub enum DiskCipher {
    #[clap(name = "xts-aes-256")]
    XtsAes256,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum BlobKind {
    Flat,
    Vhd1,
}

fn parse_path_and_len(arg: &str) -> anyhow::Result<(PathBuf, Option<u64>)> {
    Ok(match arg.split_once(';') {
        Some((path, len)) => {
            let Some(len) = len.strip_prefix("create=") else {
                anyhow::bail!("invalid syntax after ';', expected 'create=<len>'")
            };

            let len = parse_memory(len)?;

            (path.into(), Some(len))
        }
        None => (arg.into(), None),
    })
}

impl FromStr for DiskCliKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let disk = match s.split_once(':') {
            // convenience support for passing bare paths as file disks
            None => {
                let (path, create_with_len) = parse_path_and_len(s)?;
                DiskCliKind::File {
                    path,
                    create_with_len,
                }
            }
            Some((kind, arg)) => match kind {
                "mem" => DiskCliKind::Memory(parse_memory(arg)?),
                "memdiff" => DiskCliKind::MemoryDiff(Box::new(arg.parse()?)),
                "sql" => {
                    let (path, create_with_len) = parse_path_and_len(arg)?;
                    DiskCliKind::Sqlite {
                        path,
                        create_with_len,
                    }
                }
                "sqldiff" => {
                    let (path_and_opts, kind) =
                        arg.split_once(':').context("expected path[;opts]:kind")?;
                    let disk = Box::new(kind.parse()?);
                    match path_and_opts.split_once(';') {
                        Some((path, create)) => {
                            if create != "create" {
                                anyhow::bail!("invalid syntax after ';', expected 'create'")
                            }
                            DiskCliKind::SqliteDiff {
                                path: path.into(),
                                create: true,
                                disk,
                            }
                        }
                        None => DiskCliKind::SqliteDiff {
                            path: path_and_opts.into(),
                            create: false,
                            disk,
                        },
                    }
                }
                "autocache" => {
                    let (key, kind) = arg.split_once(':').context("expected [key]:kind")?;
                    let cache_path = std::env::var("OPENVMM_AUTO_CACHE_PATH")
                        .context("must set cache path via OPENVMM_AUTO_CACHE_PATH")?;
                    DiskCliKind::AutoCacheSqlite {
                        cache_path,
                        key: (!key.is_empty()).then(|| key.to_string()),
                        disk: Box::new(kind.parse()?),
                    }
                }
                "prwrap" => DiskCliKind::PersistentReservationsWrapper(Box::new(arg.parse()?)),
                "file" => {
                    let (path, create_with_len) = parse_path_and_len(arg)?;
                    DiskCliKind::File {
                        path,
                        create_with_len,
                    }
                }
                "blob" => {
                    let (blob_kind, url) = arg.split_once(':').context("expected kind:url")?;
                    let blob_kind = match blob_kind {
                        "flat" => BlobKind::Flat,
                        "vhd1" => BlobKind::Vhd1,
                        _ => anyhow::bail!("unknown blob kind {blob_kind}"),
                    };
                    DiskCliKind::Blob {
                        kind: blob_kind,
                        url: url.to_string(),
                    }
                }
                "crypt" => {
                    let (cipher, (key, kind)) = arg
                        .split_once(':')
                        .and_then(|(cipher, arg)| Some((cipher, arg.split_once(':')?)))
                        .context("expected cipher:key_file:kind")?;
                    DiskCliKind::Crypt {
                        cipher: ValueEnum::from_str(cipher, false)
                            .map_err(|err| anyhow::anyhow!("invalid cipher: {err}"))?,
                        key_file: PathBuf::from(key),
                        disk: Box::new(kind.parse()?),
                    }
                }
                kind => {
                    // here's a fun edge case: what if the user passes `--disk d:\path\to\disk.img`?
                    //
                    // in this case, we actually want to treat that leading `d:` as part of the
                    // path, rather than as a disk with `kind == 'd'`
                    let (path, create_with_len) = parse_path_and_len(s)?;
                    if path.has_root() {
                        DiskCliKind::File {
                            path,
                            create_with_len,
                        }
                    } else {
                        anyhow::bail!("invalid disk kind {kind}");
                    }
                }
            },
        };
        Ok(disk)
    }
}

#[derive(Clone)]
pub struct VmgsCli {
    pub kind: DiskCliKind,
    pub provision: ProvisionVmgs,
}

#[derive(Copy, Clone)]
pub enum ProvisionVmgs {
    OnEmpty,
    OnFailure,
    True,
}

impl FromStr for VmgsCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let (kind, opt) = s
            .split_once(',')
            .map(|(k, o)| (k, Some(o)))
            .unwrap_or((s, None));
        let kind = kind.parse()?;

        let provision = match opt {
            None => ProvisionVmgs::OnEmpty,
            Some("fmt-on-fail") => ProvisionVmgs::OnFailure,
            Some("fmt") => ProvisionVmgs::True,
            Some(opt) => anyhow::bail!("unknown option: '{opt}'"),
        };

        Ok(VmgsCli { kind, provision })
    }
}

// <kind>[,ro]
#[derive(Clone)]
pub struct DiskCli {
    pub vtl: DeviceVtl,
    pub kind: DiskCliKind,
    pub read_only: bool,
    pub is_dvd: bool,
    pub underhill: Option<UnderhillDiskSource>,
    pub pcie_port: Option<String>,
}

#[derive(Copy, Clone)]
pub enum UnderhillDiskSource {
    Scsi,
    Nvme,
}

impl FromStr for DiskCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let mut opts = s.split(',');
        let kind = opts.next().unwrap().parse()?;

        let mut read_only = false;
        let mut is_dvd = false;
        let mut underhill = None;
        let mut vtl = DeviceVtl::Vtl0;
        let mut pcie_port = None;
        for opt in opts {
            let mut s = opt.split('=');
            let opt = s.next().unwrap();
            match opt {
                "ro" => read_only = true,
                "dvd" => {
                    is_dvd = true;
                    read_only = true;
                }
                "vtl2" => {
                    vtl = DeviceVtl::Vtl2;
                }
                "uh" => underhill = Some(UnderhillDiskSource::Scsi),
                "uh-nvme" => underhill = Some(UnderhillDiskSource::Nvme),
                "pcie_port" => {
                    let port = s.next();
                    if port.is_none_or(|p| p.is_empty()) {
                        anyhow::bail!("`pcie_port` requires a port name");
                    }
                    pcie_port = Some(String::from(port.unwrap()));
                }
                opt => anyhow::bail!("unknown option: '{opt}'"),
            }
        }

        if underhill.is_some() && vtl != DeviceVtl::Vtl0 {
            anyhow::bail!("`uh` or `uh-nvme` is incompatible with `vtl2`");
        }

        if pcie_port.is_some() && (underhill.is_some() || vtl != DeviceVtl::Vtl0 || is_dvd) {
            anyhow::bail!("`pcie_port` is incompatible with `uh`, `uh-nvme`, `vtl2`, and `dvd`");
        }

        Ok(DiskCli {
            vtl,
            kind,
            read_only,
            is_dvd,
            underhill,
            pcie_port,
        })
    }
}

// <kind>[,ro,s]
#[derive(Clone)]
pub struct IdeDiskCli {
    pub kind: DiskCliKind,
    pub read_only: bool,
    pub channel: Option<u8>,
    pub device: Option<u8>,
    pub is_dvd: bool,
}

impl FromStr for IdeDiskCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let mut opts = s.split(',');
        let kind = opts.next().unwrap().parse()?;

        let mut read_only = false;
        let mut channel = None;
        let mut device = None;
        let mut is_dvd = false;
        for opt in opts {
            let mut s = opt.split('=');
            let opt = s.next().unwrap();
            match opt {
                "ro" => read_only = true,
                "p" => channel = Some(0),
                "s" => channel = Some(1),
                "0" => device = Some(0),
                "1" => device = Some(1),
                "dvd" => {
                    is_dvd = true;
                    read_only = true;
                }
                _ => anyhow::bail!("unknown option: '{opt}'"),
            }
        }

        Ok(IdeDiskCli {
            kind,
            read_only,
            channel,
            device,
            is_dvd,
        })
    }
}

// <kind>[,ro]
#[derive(Clone, Debug, PartialEq)]
pub struct FloppyDiskCli {
    pub kind: DiskCliKind,
    pub read_only: bool,
}

impl FromStr for FloppyDiskCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        if s.is_empty() {
            anyhow::bail!("empty disk spec");
        }
        let mut opts = s.split(',');
        let kind = opts.next().unwrap().parse()?;

        let mut read_only = false;
        for opt in opts {
            let mut s = opt.split('=');
            let opt = s.next().unwrap();
            match opt {
                "ro" => read_only = true,
                _ => anyhow::bail!("unknown option: '{opt}'"),
            }
        }

        Ok(FloppyDiskCli { kind, read_only })
    }
}

#[derive(Clone)]
pub struct DebugconSerialConfigCli {
    pub port: u16,
    pub serial: SerialConfigCli,
}

impl FromStr for DebugconSerialConfigCli {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((port, serial)) = s.split_once(',') else {
            return Err("invalid format (missing comma between port and serial)".into());
        };

        let port: u16 = parse_number(port)
            .map_err(|_| "could not parse port".to_owned())?
            .try_into()
            .map_err(|_| "port must be 16-bit")?;
        let serial: SerialConfigCli = serial.parse()?;

        Ok(Self { port, serial })
    }
}

/// (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
#[derive(Clone, Debug, PartialEq)]
pub enum SerialConfigCli {
    None,
    Console,
    NewConsole(Option<PathBuf>, Option<String>),
    Stderr,
    Pipe(PathBuf),
    Tcp(SocketAddr),
    File(PathBuf),
}

impl FromStr for SerialConfigCli {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let keyvalues = SerialConfigCli::parse_keyvalues(s)?;

        let first_key = match keyvalues.first() {
            Some(first_pair) => first_pair.0.as_str(),
            None => Err("invalid serial configuration: no values supplied")?,
        };
        let first_value = keyvalues.first().unwrap().1.as_ref();

        let ret = match first_key {
            "none" => SerialConfigCli::None,
            "console" => SerialConfigCli::Console,
            "stderr" => SerialConfigCli::Stderr,
            "file" => match first_value {
                Some(path) => SerialConfigCli::File(path.into()),
                None => Err("invalid serial configuration: file requires a value")?,
            },
            "term" => {
                // If user supplies a name key, use it to title the window
                let window_name = keyvalues.iter().find(|(key, _)| key == "name");
                let window_name = match window_name {
                    Some((_, Some(name))) => Some(name.clone()),
                    _ => None,
                };

                SerialConfigCli::NewConsole(first_value.map(|p| p.into()), window_name)
            }
            "listen" => match first_value {
                Some(path) => {
                    if let Some(tcp) = path.strip_prefix("tcp:") {
                        let addr = tcp
                            .parse()
                            .map_err(|err| format!("invalid tcp address: {err}"))?;
                        SerialConfigCli::Tcp(addr)
                    } else {
                        SerialConfigCli::Pipe(path.into())
                    }
                }
                None => Err(
                    "invalid serial configuration: listen requires a value of tcp:addr or pipe",
                )?,
            },
            _ => {
                return Err(format!(
                    "invalid serial configuration: '{}' is not a known option",
                    first_key
                ));
            }
        };

        Ok(ret)
    }
}

impl SerialConfigCli {
    /// Parse a comma separated list of key=value options into a vector of
    /// key/value pairs.
    fn parse_keyvalues(s: &str) -> Result<Vec<(String, Option<String>)>, String> {
        let mut ret = Vec::new();

        // For each comma separated item in the supplied list
        for item in s.split(',') {
            // Split on the = for key and value
            // If no = is found, treat key as key and value as None
            let mut eqsplit = item.split('=');
            let key = eqsplit.next();
            let value = eqsplit.next();

            if let Some(key) = key {
                ret.push((key.to_owned(), value.map(|x| x.to_owned())));
            } else {
                // An empty key is invalid
                return Err("invalid key=value pair in serial config".into());
            }
        }
        Ok(ret)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum EndpointConfigCli {
    None,
    Consomme { cidr: Option<String> },
    Dio { id: Option<String> },
    Tap { name: String },
}

impl FromStr for EndpointConfigCli {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let ret = match s.split(':').collect::<Vec<_>>().as_slice() {
            ["none"] => EndpointConfigCli::None,
            ["consomme", s @ ..] => EndpointConfigCli::Consomme {
                cidr: s.first().map(|&s| s.to_owned()),
            },
            ["dio", s @ ..] => EndpointConfigCli::Dio {
                id: s.first().map(|s| (*s).to_owned()),
            },
            ["tap", name] => EndpointConfigCli::Tap {
                name: (*name).to_owned(),
            },
            _ => return Err("invalid network backend".into()),
        };

        Ok(ret)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NicConfigCli {
    pub vtl: DeviceVtl,
    pub endpoint: EndpointConfigCli,
    pub max_queues: Option<u16>,
    pub underhill: bool,
}

impl FromStr for NicConfigCli {
    type Err = String;

    fn from_str(mut s: &str) -> Result<Self, Self::Err> {
        let mut vtl = DeviceVtl::Vtl0;
        let mut max_queues = None;
        let mut underhill = false;
        while let Some((opt, rest)) = s.split_once(':') {
            if let Some((opt, val)) = opt.split_once('=') {
                match opt {
                    "queues" => {
                        max_queues = Some(val.parse().map_err(|_| "failed to parse queue count")?);
                    }
                    _ => break,
                }
            } else {
                match opt {
                    "vtl2" => {
                        vtl = DeviceVtl::Vtl2;
                    }
                    "uh" => underhill = true,
                    _ => break,
                }
            }
            s = rest;
        }

        if underhill && vtl != DeviceVtl::Vtl0 {
            return Err("`uh` is incompatible with `vtl2`".into());
        }

        let endpoint = s.parse()?;
        Ok(NicConfigCli {
            vtl,
            endpoint,
            max_queues,
            underhill,
        })
    }
}

#[derive(Debug, Error)]
#[error("unknown hypervisor: {0}")]
pub struct UnknownHypervisor(String);

fn parse_hypervisor(s: &str) -> Result<Hypervisor, UnknownHypervisor> {
    match s {
        "kvm" => Ok(Hypervisor::Kvm),
        "mshv" => Ok(Hypervisor::MsHv),
        "whp" => Ok(Hypervisor::Whp),
        _ => Err(UnknownHypervisor(s.to_owned())),
    }
}

#[derive(Debug, Error)]
#[error("unknown VTL2 relocation type: {0}")]
pub struct UnknownVtl2RelocationType(String);

fn parse_vtl2_relocation(s: &str) -> Result<Vtl2BaseAddressType, UnknownVtl2RelocationType> {
    match s {
        "disable" => Ok(Vtl2BaseAddressType::File),
        s if s.starts_with("auto=") => {
            let s = s.strip_prefix("auto=").unwrap_or_default();
            let size = if s == "filesize" {
                None
            } else {
                let size = parse_memory(s).map_err(|e| {
                    UnknownVtl2RelocationType(format!(
                        "unable to parse memory size from {} for 'auto=' type, {e}",
                        e
                    ))
                })?;
                Some(size)
            };
            Ok(Vtl2BaseAddressType::MemoryLayout { size })
        }
        s if s.starts_with("absolute=") => {
            let s = s.strip_prefix("absolute=");
            let addr = parse_number(s.unwrap_or_default()).map_err(|e| {
                UnknownVtl2RelocationType(format!(
                    "unable to parse number from {} for 'absolute=' type",
                    e
                ))
            })?;
            Ok(Vtl2BaseAddressType::Absolute(addr))
        }
        s if s.starts_with("vtl2=") => {
            let s = s.strip_prefix("vtl2=").unwrap_or_default();
            let size = if s == "filesize" {
                None
            } else {
                let size = parse_memory(s).map_err(|e| {
                    UnknownVtl2RelocationType(format!(
                        "unable to parse memory size from {} for 'vtl2=' type, {e}",
                        e
                    ))
                })?;
                Some(size)
            };
            Ok(Vtl2BaseAddressType::Vtl2Allocate { size })
        }
        _ => Err(UnknownVtl2RelocationType(s.to_owned())),
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum SmtConfigCli {
    Auto,
    Force,
    Off,
}

#[derive(Debug, Error)]
#[error("expected auto, force, or off")]
pub struct BadSmtConfig;

impl FromStr for SmtConfigCli {
    type Err = BadSmtConfig;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let r = match s {
            "auto" => Self::Auto,
            "force" => Self::Force,
            "off" => Self::Off,
            _ => return Err(BadSmtConfig),
        };
        Ok(r)
    }
}

#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
fn parse_x2apic(s: &str) -> Result<X2ApicConfig, &'static str> {
    let r = match s {
        "auto" => X2ApicConfig::Auto,
        "supported" => X2ApicConfig::Supported,
        "off" => X2ApicConfig::Unsupported,
        "on" => X2ApicConfig::Enabled,
        _ => return Err("expected auto, supported, off, or on"),
    };
    Ok(r)
}

#[derive(Debug, Copy, Clone, ValueEnum)]
pub enum Vtl0LateMapPolicyCli {
    Off,
    Log,
    Halt,
    Exception,
}

#[derive(Debug, Copy, Clone, ValueEnum)]
pub enum IsolationCli {
    Vbs,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct PcatBootOrderCli(pub [PcatBootDevice; 4]);

impl FromStr for PcatBootOrderCli {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut default_order = DEFAULT_PCAT_BOOT_ORDER.map(Some);
        let mut order = Vec::new();

        for item in s.split(',') {
            let device = match item {
                "optical" => PcatBootDevice::Optical,
                "hdd" => PcatBootDevice::HardDrive,
                "net" => PcatBootDevice::Network,
                "floppy" => PcatBootDevice::Floppy,
                _ => return Err("unknown boot device type"),
            };

            let default_pos = default_order
                .iter()
                .position(|x| x == &Some(device))
                .ok_or("cannot pass duplicate boot devices")?;

            order.push(default_order[default_pos].take().unwrap());
        }

        order.extend(default_order.into_iter().flatten());
        assert_eq!(order.len(), 4);

        Ok(Self(order.try_into().unwrap()))
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum UefiConsoleModeCli {
    Default,
    Com1,
    Com2,
    None,
}

#[derive(Copy, Clone, Debug, Default, ValueEnum)]
pub enum EfiDiagnosticsLogLevelCli {
    #[default]
    Default,
    Info,
    Full,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PcieRootComplexCli {
    pub name: String,
    pub segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
    pub low_mmio: u32,
    pub high_mmio: u64,
}

impl FromStr for PcieRootComplexCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const DEFAULT_PCIE_CRS_LOW_SIZE: u32 = 4 * 1024 * 1024; // 4M
        const DEFAULT_PCIE_CRS_HIGH_SIZE: u64 = 1024 * 1024 * 1024; // 1G

        let mut opts = s.split(',');
        let name = opts.next().context("expected root complex name")?;
        if name.is_empty() {
            anyhow::bail!("must provide a root complex name");
        }

        let mut segment = 0;
        let mut start_bus = 0;
        let mut end_bus = 255;
        let mut low_mmio = DEFAULT_PCIE_CRS_LOW_SIZE;
        let mut high_mmio = DEFAULT_PCIE_CRS_HIGH_SIZE;
        for opt in opts {
            let mut s = opt.split('=');
            let opt = s.next().context("expected option")?;
            match opt {
                "segment" => {
                    let seg_str = s.next().context("expected segment number")?;
                    segment = u16::from_str(seg_str).context("failed to parse segment number")?;
                }
                "start_bus" => {
                    let bus_str = s.next().context("expected start bus number")?;
                    start_bus =
                        u8::from_str(bus_str).context("failed to parse start bus number")?;
                }
                "end_bus" => {
                    let bus_str = s.next().context("expected end bus number")?;
                    end_bus = u8::from_str(bus_str).context("failed to parse end bus number")?;
                }
                "low_mmio" => {
                    let low_mmio_str = s.next().context("expected low MMIO size")?;
                    low_mmio = parse_memory(low_mmio_str)
                        .context("failed to parse low MMIO size")?
                        .try_into()?;
                }
                "high_mmio" => {
                    let high_mmio_str = s.next().context("expected high MMIO size")?;
                    high_mmio =
                        parse_memory(high_mmio_str).context("failed to parse high MMIO size")?;
                }
                opt => anyhow::bail!("unknown option: '{opt}'"),
            }
        }

        if start_bus >= end_bus {
            anyhow::bail!("start_bus must be less than or equal to end_bus");
        }

        Ok(PcieRootComplexCli {
            name: name.to_string(),
            segment,
            start_bus,
            end_bus,
            low_mmio,
            high_mmio,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PcieRootPortCli {
    pub root_complex_name: String,
    pub name: String,
    pub hotplug: bool,
}

impl FromStr for PcieRootPortCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut opts = s.split(',');
        let names = opts.next().context("expected root port identifiers")?;
        if names.is_empty() {
            anyhow::bail!("must provide root port identifiers");
        }

        let mut s = names.split(':');
        let rc_name = s.next().context("expected name of parent root complex")?;
        let rp_name = s.next().context("expected root port name")?;

        if let Some(extra) = s.next() {
            anyhow::bail!("unexpected token: '{extra}'")
        }

        let mut hotplug = false;

        // Parse optional flags
        for opt in opts {
            match opt {
                "hotplug" => hotplug = true,
                _ => anyhow::bail!("unexpected option: '{opt}'"),
            }
        }

        Ok(PcieRootPortCli {
            root_complex_name: rc_name.to_string(),
            name: rp_name.to_string(),
            hotplug,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenericPcieSwitchCli {
    pub port_name: String,
    pub name: String,
    pub num_downstream_ports: u8,
    pub hotplug: bool,
}

impl FromStr for GenericPcieSwitchCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut opts = s.split(',');
        let names = opts.next().context("expected switch identifiers")?;
        if names.is_empty() {
            anyhow::bail!("must provide switch identifiers");
        }

        let mut s = names.split(':');
        let port_name = s.next().context("expected name of parent port")?;
        let switch_name = s.next().context("expected switch name")?;

        if let Some(extra) = s.next() {
            anyhow::bail!("unexpected token: '{extra}'")
        }

        let mut num_downstream_ports = 4u8; // Default value
        let mut hotplug = false;

        for opt in opts {
            let mut kv = opt.split('=');
            let key = kv.next().context("expected option name")?;

            match key {
                "num_downstream_ports" => {
                    let value = kv.next().context("expected option value")?;
                    if let Some(extra) = kv.next() {
                        anyhow::bail!("unexpected token: '{extra}'")
                    }
                    num_downstream_ports = value.parse().context("invalid num_downstream_ports")?;
                }
                "hotplug" => {
                    if kv.next().is_some() {
                        anyhow::bail!("hotplug option does not take a value")
                    }
                    hotplug = true;
                }
                _ => anyhow::bail!("unknown option: '{key}'"),
            }
        }

        Ok(GenericPcieSwitchCli {
            port_name: port_name.to_string(),
            name: switch_name.to_string(),
            num_downstream_ports,
            hotplug,
        })
    }
}

/// CLI configuration for a PCIe remote device.
#[derive(Clone, Debug, PartialEq)]
pub struct PcieRemoteCli {
    /// Name of the PCIe downstream port to attach to.
    pub port_name: String,
    /// Unix socket path for the remote simulator.
    pub socket_path: Option<String>,
    /// Hardware unit identifier for plug request.
    pub hu: u16,
    /// Controller identifier for plug request.
    pub controller: u16,
}

impl FromStr for PcieRemoteCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut opts = s.split(',');
        let port_name = opts.next().context("expected port name")?;
        if port_name.is_empty() {
            anyhow::bail!("must provide a port name");
        }

        let mut socket_path = None;
        let mut hu = 0u16;
        let mut controller = 0u16;

        for opt in opts {
            let mut kv = opt.split('=');
            let key = kv.next().context("expected option name")?;
            let value = kv.next();

            match key {
                "socket" => {
                    let path = value.context("socket requires a path")?;
                    if let Some(extra) = kv.next() {
                        anyhow::bail!("unexpected token: '{extra}'")
                    }
                    if path.is_empty() {
                        anyhow::bail!("socket path cannot be empty");
                    }
                    socket_path = Some(path.to_string());
                }
                "hu" => {
                    let val = value.context("hu requires a value")?;
                    if let Some(extra) = kv.next() {
                        anyhow::bail!("unexpected token: '{extra}'")
                    }
                    hu = val.parse().context("failed to parse hu")?;
                }
                "controller" => {
                    let val = value.context("controller requires a value")?;
                    if let Some(extra) = kv.next() {
                        anyhow::bail!("unexpected token: '{extra}'")
                    }
                    controller = val.parse().context("failed to parse controller")?;
                }
                _ => anyhow::bail!("unknown option: '{key}'"),
            }
        }

        Ok(PcieRemoteCli {
            port_name: port_name.to_string(),
            socket_path,
            hu,
            controller,
        })
    }
}

/// Read a environment variable that may / may-not have a target-specific
/// prefix. e.g: `default_value_from_arch_env("FOO")` would first try and read
/// from `FOO`, and if that's not found, it will try `X86_64_FOO`.
///
/// Must return an `OsString`, in order to be compatible with `clap`'s
/// default_value code. As such - to encode the absence of the env-var, an empty
/// OsString is returned.
fn default_value_from_arch_env(name: &str) -> OsString {
    let prefix = if cfg!(guest_arch = "x86_64") {
        "X86_64"
    } else if cfg!(guest_arch = "aarch64") {
        "AARCH64"
    } else {
        return Default::default();
    };
    let prefixed = format!("{}_{}", prefix, name);
    std::env::var_os(name)
        .or_else(|| std::env::var_os(prefixed))
        .unwrap_or_default()
}

/// Workaround to use `Option<PathBuf>` alongside [`default_value_from_arch_env`]
#[derive(Clone)]
pub struct OptionalPathBuf(pub Option<PathBuf>);

impl From<&std::ffi::OsStr> for OptionalPathBuf {
    fn from(s: &std::ffi::OsStr) -> Self {
        OptionalPathBuf(if s.is_empty() { None } else { Some(s.into()) })
    }
}

#[cfg(test)]
// UNSAFETY: Needed to set and remove environment variables in tests
#[expect(unsafe_code)]
mod tests {
    use super::*;

    fn with_env_var<F, R>(name: &str, value: &str, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        // SAFETY:
        // Safe in a testing context because it won't be changed concurrently
        unsafe {
            std::env::set_var(name, value);
        }
        let result = f();
        // SAFETY:
        // Safe in a testing context because it won't be changed concurrently
        unsafe {
            std::env::remove_var(name);
        }
        result
    }

    #[test]
    fn test_parse_file_disk_with_create() {
        let s = "file:test.vhd;create=1G";
        let disk = DiskCliKind::from_str(s).unwrap();

        match disk {
            DiskCliKind::File {
                path,
                create_with_len,
            } => {
                assert_eq!(path, PathBuf::from("test.vhd"));
                assert_eq!(create_with_len, Some(1024 * 1024 * 1024)); // 1G
            }
            _ => panic!("Expected File variant"),
        }
    }

    #[test]
    fn test_parse_direct_file_with_create() {
        let s = "test.vhd;create=1G";
        let disk = DiskCliKind::from_str(s).unwrap();

        match disk {
            DiskCliKind::File {
                path,
                create_with_len,
            } => {
                assert_eq!(path, PathBuf::from("test.vhd"));
                assert_eq!(create_with_len, Some(1024 * 1024 * 1024)); // 1G
            }
            _ => panic!("Expected File variant"),
        }
    }

    #[test]
    fn test_parse_memory_disk() {
        let s = "mem:1G";
        let disk = DiskCliKind::from_str(s).unwrap();
        match disk {
            DiskCliKind::Memory(size) => {
                assert_eq!(size, 1024 * 1024 * 1024); // 1G
            }
            _ => panic!("Expected Memory variant"),
        }
    }

    #[test]
    fn test_parse_pcie_disk() {
        assert_eq!(
            DiskCli::from_str("mem:1G,pcie_port=p0").unwrap().pcie_port,
            Some("p0".to_string())
        );
        assert_eq!(
            DiskCli::from_str("file:path.vhdx,pcie_port=p0")
                .unwrap()
                .pcie_port,
            Some("p0".to_string())
        );
        assert_eq!(
            DiskCli::from_str("memdiff:file:path.vhdx,pcie_port=p0")
                .unwrap()
                .pcie_port,
            Some("p0".to_string())
        );

        // Missing port name
        assert!(DiskCli::from_str("file:disk.vhd,pcie_port=").is_err());

        // Incompatible with various other disk fields
        assert!(DiskCli::from_str("file:disk.vhd,pcie_port=p0,vtl2").is_err());
        assert!(DiskCli::from_str("file:disk.vhd,pcie_port=p0,uh").is_err());
        assert!(DiskCli::from_str("file:disk.vhd,pcie_port=p0,uh-nvme").is_err());
    }

    #[test]
    fn test_parse_memory_diff_disk() {
        let s = "memdiff:file:base.img";
        let disk = DiskCliKind::from_str(s).unwrap();
        match disk {
            DiskCliKind::MemoryDiff(inner) => match *inner {
                DiskCliKind::File {
                    path,
                    create_with_len,
                } => {
                    assert_eq!(path, PathBuf::from("base.img"));
                    assert_eq!(create_with_len, None);
                }
                _ => panic!("Expected File variant inside MemoryDiff"),
            },
            _ => panic!("Expected MemoryDiff variant"),
        }
    }

    #[test]
    fn test_parse_sqlite_disk() {
        let s = "sql:db.sqlite;create=2G";
        let disk = DiskCliKind::from_str(s).unwrap();
        match disk {
            DiskCliKind::Sqlite {
                path,
                create_with_len,
            } => {
                assert_eq!(path, PathBuf::from("db.sqlite"));
                assert_eq!(create_with_len, Some(2 * 1024 * 1024 * 1024));
            }
            _ => panic!("Expected Sqlite variant"),
        }

        // Test without create option
        let s = "sql:db.sqlite";
        let disk = DiskCliKind::from_str(s).unwrap();
        match disk {
            DiskCliKind::Sqlite {
                path,
                create_with_len,
            } => {
                assert_eq!(path, PathBuf::from("db.sqlite"));
                assert_eq!(create_with_len, None);
            }
            _ => panic!("Expected Sqlite variant"),
        }
    }

    #[test]
    fn test_parse_sqlite_diff_disk() {
        // Test with create option
        let s = "sqldiff:diff.sqlite;create:file:base.img";
        let disk = DiskCliKind::from_str(s).unwrap();
        match disk {
            DiskCliKind::SqliteDiff { path, create, disk } => {
                assert_eq!(path, PathBuf::from("diff.sqlite"));
                assert!(create);
                match *disk {
                    DiskCliKind::File {
                        path,
                        create_with_len,
                    } => {
                        assert_eq!(path, PathBuf::from("base.img"));
                        assert_eq!(create_with_len, None);
                    }
                    _ => panic!("Expected File variant inside SqliteDiff"),
                }
            }
            _ => panic!("Expected SqliteDiff variant"),
        }

        // Test without create option
        let s = "sqldiff:diff.sqlite:file:base.img";
        let disk = DiskCliKind::from_str(s).unwrap();
        match disk {
            DiskCliKind::SqliteDiff { path, create, disk } => {
                assert_eq!(path, PathBuf::from("diff.sqlite"));
                assert!(!create);
                match *disk {
                    DiskCliKind::File {
                        path,
                        create_with_len,
                    } => {
                        assert_eq!(path, PathBuf::from("base.img"));
                        assert_eq!(create_with_len, None);
                    }
                    _ => panic!("Expected File variant inside SqliteDiff"),
                }
            }
            _ => panic!("Expected SqliteDiff variant"),
        }
    }

    #[test]
    fn test_parse_autocache_sqlite_disk() {
        // Test with environment variable set
        let disk = with_env_var("OPENVMM_AUTO_CACHE_PATH", "/tmp/cache", || {
            DiskCliKind::from_str("autocache::file:disk.vhd").unwrap()
        });
        assert!(matches!(
            disk,
            DiskCliKind::AutoCacheSqlite {
                cache_path,
                key,
                disk: _disk,
            } if cache_path == "/tmp/cache" && key.is_none()
        ));

        // Test without environment variable
        assert!(DiskCliKind::from_str("autocache::file:disk.vhd").is_err());
    }

    #[test]
    fn test_parse_disk_errors() {
        assert!(DiskCliKind::from_str("invalid:").is_err());
        assert!(DiskCliKind::from_str("memory:extra").is_err());

        // Test sqlite: without environment variable
        assert!(DiskCliKind::from_str("sqlite:").is_err());
    }

    #[test]
    fn test_parse_errors() {
        // Invalid memory size
        assert!(DiskCliKind::from_str("mem:invalid").is_err());

        // Invalid syntax for SQLiteDiff
        assert!(DiskCliKind::from_str("sqldiff:path").is_err());

        // Missing OPENVMM_AUTO_CACHE_PATH for AutoCacheSqlite
        // SAFETY:
        // Safe in a testing context because it won't be changed concurrently
        unsafe {
            std::env::remove_var("OPENVMM_AUTO_CACHE_PATH");
        }
        assert!(DiskCliKind::from_str("autocache:key:file:disk.vhd").is_err());

        // Invalid blob kind
        assert!(DiskCliKind::from_str("blob:invalid:url").is_err());

        // Invalid cipher
        assert!(DiskCliKind::from_str("crypt:invalid:key.bin:file:disk.vhd").is_err());

        // Invalid format for crypt (missing parts)
        assert!(DiskCliKind::from_str("crypt:xts-aes-256:key.bin").is_err());

        // Invalid disk kind
        assert!(DiskCliKind::from_str("invalid:path").is_err());

        // Missing create size
        assert!(DiskCliKind::from_str("file:disk.vhd;create=").is_err());
    }

    #[test]
    fn test_fs_args_from_str() {
        let args = FsArgs::from_str("tag1,/path/to/fs").unwrap();
        assert_eq!(args.tag, "tag1");
        assert_eq!(args.path, "/path/to/fs");

        // Test error cases
        assert!(FsArgs::from_str("tag1").is_err());
        assert!(FsArgs::from_str("tag1,/path,extra").is_err());
    }

    #[test]
    fn test_fs_args_with_options_from_str() {
        let args = FsArgsWithOptions::from_str("tag1,/path/to/fs,opt1,opt2").unwrap();
        assert_eq!(args.tag, "tag1");
        assert_eq!(args.path, "/path/to/fs");
        assert_eq!(args.options, "opt1;opt2");

        // Test without options
        let args = FsArgsWithOptions::from_str("tag1,/path/to/fs").unwrap();
        assert_eq!(args.tag, "tag1");
        assert_eq!(args.path, "/path/to/fs");
        assert_eq!(args.options, "");

        // Test error case
        assert!(FsArgsWithOptions::from_str("tag1").is_err());
    }

    #[test]
    fn test_serial_config_from_str() {
        assert_eq!(
            SerialConfigCli::from_str("none").unwrap(),
            SerialConfigCli::None
        );
        assert_eq!(
            SerialConfigCli::from_str("console").unwrap(),
            SerialConfigCli::Console
        );
        assert_eq!(
            SerialConfigCli::from_str("stderr").unwrap(),
            SerialConfigCli::Stderr
        );

        // Test file config
        let file_config = SerialConfigCli::from_str("file=/path/to/file").unwrap();
        if let SerialConfigCli::File(path) = file_config {
            assert_eq!(path.to_str().unwrap(), "/path/to/file");
        } else {
            panic!("Expected File variant");
        }

        // Test term config with name, but no specific path
        match SerialConfigCli::from_str("term,name=MyTerm").unwrap() {
            SerialConfigCli::NewConsole(None, Some(name)) => {
                assert_eq!(name, "MyTerm");
            }
            _ => panic!("Expected NewConsole variant with name"),
        }

        // Test term config without name, but no specific path
        match SerialConfigCli::from_str("term").unwrap() {
            SerialConfigCli::NewConsole(None, None) => (),
            _ => panic!("Expected NewConsole variant without name"),
        }

        // Test term config with name
        match SerialConfigCli::from_str("term=/dev/pts/0,name=MyTerm").unwrap() {
            SerialConfigCli::NewConsole(Some(path), Some(name)) => {
                assert_eq!(path.to_str().unwrap(), "/dev/pts/0");
                assert_eq!(name, "MyTerm");
            }
            _ => panic!("Expected NewConsole variant with name"),
        }

        // Test term config without name
        match SerialConfigCli::from_str("term=/dev/pts/0").unwrap() {
            SerialConfigCli::NewConsole(Some(path), None) => {
                assert_eq!(path.to_str().unwrap(), "/dev/pts/0");
            }
            _ => panic!("Expected NewConsole variant without name"),
        }

        // Test TCP config
        match SerialConfigCli::from_str("listen=tcp:127.0.0.1:1234").unwrap() {
            SerialConfigCli::Tcp(addr) => {
                assert_eq!(addr.to_string(), "127.0.0.1:1234");
            }
            _ => panic!("Expected Tcp variant"),
        }

        // Test pipe config
        match SerialConfigCli::from_str("listen=/path/to/pipe").unwrap() {
            SerialConfigCli::Pipe(path) => {
                assert_eq!(path.to_str().unwrap(), "/path/to/pipe");
            }
            _ => panic!("Expected Pipe variant"),
        }

        // Test error cases
        assert!(SerialConfigCli::from_str("").is_err());
        assert!(SerialConfigCli::from_str("unknown").is_err());
        assert!(SerialConfigCli::from_str("file").is_err());
        assert!(SerialConfigCli::from_str("listen").is_err());
    }

    #[test]
    fn test_endpoint_config_from_str() {
        // Test none
        assert!(matches!(
            EndpointConfigCli::from_str("none").unwrap(),
            EndpointConfigCli::None
        ));

        // Test consomme without cidr
        match EndpointConfigCli::from_str("consomme").unwrap() {
            EndpointConfigCli::Consomme { cidr: None } => (),
            _ => panic!("Expected Consomme variant without cidr"),
        }

        // Test consomme with cidr
        match EndpointConfigCli::from_str("consomme:192.168.0.0/24").unwrap() {
            EndpointConfigCli::Consomme { cidr: Some(cidr) } => {
                assert_eq!(cidr, "192.168.0.0/24");
            }
            _ => panic!("Expected Consomme variant with cidr"),
        }

        // Test dio without id
        match EndpointConfigCli::from_str("dio").unwrap() {
            EndpointConfigCli::Dio { id: None } => (),
            _ => panic!("Expected Dio variant without id"),
        }

        // Test dio with id
        match EndpointConfigCli::from_str("dio:test_id").unwrap() {
            EndpointConfigCli::Dio { id: Some(id) } => {
                assert_eq!(id, "test_id");
            }
            _ => panic!("Expected Dio variant with id"),
        }

        // Test tap
        match EndpointConfigCli::from_str("tap:tap0").unwrap() {
            EndpointConfigCli::Tap { name } => {
                assert_eq!(name, "tap0");
            }
            _ => panic!("Expected Tap variant"),
        }

        // Test error case
        assert!(EndpointConfigCli::from_str("invalid").is_err());
    }

    #[test]
    fn test_nic_config_from_str() {
        use openvmm_defs::config::DeviceVtl;

        // Test basic endpoint
        let config = NicConfigCli::from_str("none").unwrap();
        assert_eq!(config.vtl, DeviceVtl::Vtl0);
        assert!(config.max_queues.is_none());
        assert!(!config.underhill);
        assert!(matches!(config.endpoint, EndpointConfigCli::None));

        // Test with vtl2
        let config = NicConfigCli::from_str("vtl2:none").unwrap();
        assert_eq!(config.vtl, DeviceVtl::Vtl2);
        assert!(matches!(config.endpoint, EndpointConfigCli::None));

        // Test with queues
        let config = NicConfigCli::from_str("queues=4:none").unwrap();
        assert_eq!(config.max_queues, Some(4));
        assert!(matches!(config.endpoint, EndpointConfigCli::None));

        // Test with underhill
        let config = NicConfigCli::from_str("uh:none").unwrap();
        assert!(config.underhill);
        assert!(matches!(config.endpoint, EndpointConfigCli::None));

        // Test error cases
        assert!(NicConfigCli::from_str("queues=invalid:none").is_err());
        assert!(NicConfigCli::from_str("uh:vtl2:none").is_err()); // uh incompatible with vtl2
    }

    #[test]
    fn test_smt_config_from_str() {
        assert_eq!(SmtConfigCli::from_str("auto").unwrap(), SmtConfigCli::Auto);
        assert_eq!(
            SmtConfigCli::from_str("force").unwrap(),
            SmtConfigCli::Force
        );
        assert_eq!(SmtConfigCli::from_str("off").unwrap(), SmtConfigCli::Off);

        // Test error cases
        assert!(SmtConfigCli::from_str("invalid").is_err());
        assert!(SmtConfigCli::from_str("").is_err());
    }

    #[test]
    fn test_pcat_boot_order_from_str() {
        // Test single device
        let order = PcatBootOrderCli::from_str("optical").unwrap();
        assert_eq!(order.0[0], PcatBootDevice::Optical);

        // Test multiple devices
        let order = PcatBootOrderCli::from_str("hdd,net").unwrap();
        assert_eq!(order.0[0], PcatBootDevice::HardDrive);
        assert_eq!(order.0[1], PcatBootDevice::Network);

        // Test error cases
        assert!(PcatBootOrderCli::from_str("invalid").is_err());
        assert!(PcatBootOrderCli::from_str("optical,optical").is_err()); // duplicate device
    }

    #[test]
    fn test_floppy_disk_from_str() {
        // Test basic disk
        let disk = FloppyDiskCli::from_str("file:/path/to/floppy.img").unwrap();
        assert!(!disk.read_only);
        match disk.kind {
            DiskCliKind::File {
                path,
                create_with_len,
            } => {
                assert_eq!(path.to_str().unwrap(), "/path/to/floppy.img");
                assert_eq!(create_with_len, None);
            }
            _ => panic!("Expected File variant"),
        }

        // Test with read-only flag
        let disk = FloppyDiskCli::from_str("file:/path/to/floppy.img,ro").unwrap();
        assert!(disk.read_only);

        // Test error cases
        assert!(FloppyDiskCli::from_str("").is_err());
        assert!(FloppyDiskCli::from_str("file:/path/to/floppy.img,invalid").is_err());
    }

    #[test]
    fn test_pcie_root_complex_from_str() {
        const ONE_MB: u64 = 1024 * 1024;
        const ONE_GB: u64 = 1024 * ONE_MB;

        const DEFAULT_LOW_MMIO: u32 = (4 * ONE_MB) as u32;
        const DEFAULT_HIGH_MMIO: u64 = ONE_GB;

        assert_eq!(
            PcieRootComplexCli::from_str("rc0").unwrap(),
            PcieRootComplexCli {
                name: "rc0".to_string(),
                segment: 0,
                start_bus: 0,
                end_bus: 255,
                low_mmio: DEFAULT_LOW_MMIO,
                high_mmio: DEFAULT_HIGH_MMIO,
            }
        );

        assert_eq!(
            PcieRootComplexCli::from_str("rc1,segment=1").unwrap(),
            PcieRootComplexCli {
                name: "rc1".to_string(),
                segment: 1,
                start_bus: 0,
                end_bus: 255,
                low_mmio: DEFAULT_LOW_MMIO,
                high_mmio: DEFAULT_HIGH_MMIO,
            }
        );

        assert_eq!(
            PcieRootComplexCli::from_str("rc2,start_bus=32").unwrap(),
            PcieRootComplexCli {
                name: "rc2".to_string(),
                segment: 0,
                start_bus: 32,
                end_bus: 255,
                low_mmio: DEFAULT_LOW_MMIO,
                high_mmio: DEFAULT_HIGH_MMIO,
            }
        );

        assert_eq!(
            PcieRootComplexCli::from_str("rc3,end_bus=31").unwrap(),
            PcieRootComplexCli {
                name: "rc3".to_string(),
                segment: 0,
                start_bus: 0,
                end_bus: 31,
                low_mmio: DEFAULT_LOW_MMIO,
                high_mmio: DEFAULT_HIGH_MMIO,
            }
        );

        assert_eq!(
            PcieRootComplexCli::from_str("rc4,start_bus=32,end_bus=127,high_mmio=2G").unwrap(),
            PcieRootComplexCli {
                name: "rc4".to_string(),
                segment: 0,
                start_bus: 32,
                end_bus: 127,
                low_mmio: DEFAULT_LOW_MMIO,
                high_mmio: 2 * ONE_GB,
            }
        );

        assert_eq!(
            PcieRootComplexCli::from_str("rc5,segment=2,start_bus=32,end_bus=127").unwrap(),
            PcieRootComplexCli {
                name: "rc5".to_string(),
                segment: 2,
                start_bus: 32,
                end_bus: 127,
                low_mmio: DEFAULT_LOW_MMIO,
                high_mmio: DEFAULT_HIGH_MMIO,
            }
        );

        assert_eq!(
            PcieRootComplexCli::from_str("rc6,low_mmio=1M,high_mmio=64G").unwrap(),
            PcieRootComplexCli {
                name: "rc6".to_string(),
                segment: 0,
                start_bus: 0,
                end_bus: 255,
                low_mmio: ONE_MB as u32,
                high_mmio: 64 * ONE_GB,
            }
        );

        // Error cases
        assert!(PcieRootComplexCli::from_str("").is_err());
        assert!(PcieRootComplexCli::from_str("poorly,").is_err());
        assert!(PcieRootComplexCli::from_str("configured,complex").is_err());
        assert!(PcieRootComplexCli::from_str("fails,start_bus=foo").is_err());
        assert!(PcieRootComplexCli::from_str("fails,start_bus=32,end_bus=31").is_err());
        assert!(PcieRootComplexCli::from_str("rc,start_bus=256").is_err());
        assert!(PcieRootComplexCli::from_str("rc,end_bus=256").is_err());
        assert!(PcieRootComplexCli::from_str("rc,low_mmio=5G").is_err());
        assert!(PcieRootComplexCli::from_str("rc,low_mmio=aG").is_err());
        assert!(PcieRootComplexCli::from_str("rc,high_mmio=bad").is_err());
        assert!(PcieRootComplexCli::from_str("rc,high_mmio").is_err());
    }

    #[test]
    fn test_pcie_root_port_from_str() {
        assert_eq!(
            PcieRootPortCli::from_str("rc0:rc0rp0").unwrap(),
            PcieRootPortCli {
                root_complex_name: "rc0".to_string(),
                name: "rc0rp0".to_string(),
                hotplug: false,
            }
        );

        assert_eq!(
            PcieRootPortCli::from_str("my_rc:port2").unwrap(),
            PcieRootPortCli {
                root_complex_name: "my_rc".to_string(),
                name: "port2".to_string(),
                hotplug: false,
            }
        );

        // Test with hotplug flag
        assert_eq!(
            PcieRootPortCli::from_str("my_rc:port2,hotplug").unwrap(),
            PcieRootPortCli {
                root_complex_name: "my_rc".to_string(),
                name: "port2".to_string(),
                hotplug: true,
            }
        );

        // Error cases
        assert!(PcieRootPortCli::from_str("").is_err());
        assert!(PcieRootPortCli::from_str("rp0").is_err());
        assert!(PcieRootPortCli::from_str("rp0,opt").is_err());
        assert!(PcieRootPortCli::from_str("rc0:rp0:rp3").is_err());
        assert!(PcieRootPortCli::from_str("rc0:rp0,invalid_option").is_err());
    }

    #[test]
    fn test_pcie_switch_from_str() {
        assert_eq!(
            GenericPcieSwitchCli::from_str("rp0:switch0").unwrap(),
            GenericPcieSwitchCli {
                port_name: "rp0".to_string(),
                name: "switch0".to_string(),
                num_downstream_ports: 4,
                hotplug: false,
            }
        );

        assert_eq!(
            GenericPcieSwitchCli::from_str("port1:my_switch,num_downstream_ports=4").unwrap(),
            GenericPcieSwitchCli {
                port_name: "port1".to_string(),
                name: "my_switch".to_string(),
                num_downstream_ports: 4,
                hotplug: false,
            }
        );

        assert_eq!(
            GenericPcieSwitchCli::from_str("rp2:sw,num_downstream_ports=8").unwrap(),
            GenericPcieSwitchCli {
                port_name: "rp2".to_string(),
                name: "sw".to_string(),
                num_downstream_ports: 8,
                hotplug: false,
            }
        );

        // Test hierarchical connections
        assert_eq!(
            GenericPcieSwitchCli::from_str("switch0-downstream-1:child_switch").unwrap(),
            GenericPcieSwitchCli {
                port_name: "switch0-downstream-1".to_string(),
                name: "child_switch".to_string(),
                num_downstream_ports: 4,
                hotplug: false,
            }
        );

        // Test hotplug flag
        assert_eq!(
            GenericPcieSwitchCli::from_str("rp0:switch0,hotplug").unwrap(),
            GenericPcieSwitchCli {
                port_name: "rp0".to_string(),
                name: "switch0".to_string(),
                num_downstream_ports: 4,
                hotplug: true,
            }
        );

        // Test hotplug with num_downstream_ports
        assert_eq!(
            GenericPcieSwitchCli::from_str("rp0:switch0,num_downstream_ports=8,hotplug").unwrap(),
            GenericPcieSwitchCli {
                port_name: "rp0".to_string(),
                name: "switch0".to_string(),
                num_downstream_ports: 8,
                hotplug: true,
            }
        );

        // Error cases
        assert!(GenericPcieSwitchCli::from_str("").is_err());
        assert!(GenericPcieSwitchCli::from_str("switch0").is_err());
        assert!(GenericPcieSwitchCli::from_str("rp0:switch0:extra").is_err());
        assert!(GenericPcieSwitchCli::from_str("rp0:switch0,invalid_opt=value").is_err());
        assert!(GenericPcieSwitchCli::from_str("rp0:switch0,num_downstream_ports=bad").is_err());
        assert!(GenericPcieSwitchCli::from_str("rp0:switch0,num_downstream_ports=").is_err());
        assert!(GenericPcieSwitchCli::from_str("rp0:switch0,invalid_flag").is_err());
    }

    #[test]
    fn test_pcie_remote_from_str() {
        // Basic port name only
        assert_eq!(
            PcieRemoteCli::from_str("rc0rp0").unwrap(),
            PcieRemoteCli {
                port_name: "rc0rp0".to_string(),
                socket_path: None,
                hu: 0,
                controller: 0,
            }
        );

        // With socket path
        assert_eq!(
            PcieRemoteCli::from_str("rc0rp0,socket=/tmp/custom.sock").unwrap(),
            PcieRemoteCli {
                port_name: "rc0rp0".to_string(),
                socket_path: Some("/tmp/custom.sock".to_string()),
                hu: 0,
                controller: 0,
            }
        );

        // With all options
        assert_eq!(
            PcieRemoteCli::from_str("myport,socket=/tmp/dev.sock,hu=1,controller=2").unwrap(),
            PcieRemoteCli {
                port_name: "myport".to_string(),
                socket_path: Some("/tmp/dev.sock".to_string()),
                hu: 1,
                controller: 2,
            }
        );

        // Only hu and controller
        assert_eq!(
            PcieRemoteCli::from_str("port0,hu=5,controller=3").unwrap(),
            PcieRemoteCli {
                port_name: "port0".to_string(),
                socket_path: None,
                hu: 5,
                controller: 3,
            }
        );

        // Error cases
        assert!(PcieRemoteCli::from_str("").is_err());
        assert!(PcieRemoteCli::from_str("port,socket=").is_err());
        assert!(PcieRemoteCli::from_str("port,hu=").is_err());
        assert!(PcieRemoteCli::from_str("port,hu=bad").is_err());
        assert!(PcieRemoteCli::from_str("port,controller=").is_err());
        assert!(PcieRemoteCli::from_str("port,controller=bad").is_err());
        assert!(PcieRemoteCli::from_str("port,unknown=value").is_err());
    }
}
