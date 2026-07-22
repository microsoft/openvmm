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
use cxl_spec::spec::CfmwsWindowRestrictions;
use guid::Guid;
use openvmm_defs::config::DEFAULT_PCAT_BOOT_ORDER;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::PcatBootDevice;
use openvmm_defs::config::Vtl2BaseAddressType;
use openvmm_defs::config::X2ApicConfig;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use thiserror::Error;

/// Parse CLI options, using a thread with a larger stack on Windows to avoid
/// stack overflow in debug builds due to clap's deep stack usage.
/// See <https://github.com/clap-rs/clap/issues/5134>.
pub(crate) fn parse_options() -> Options {
    // In non-optimized builds, clap uses an embarrassing amount of stack space
    // to construct the `Command` instance for `Options`, more than the Windows
    // default of 1MB. This has been known since 2023:
    // <https://github.com/clap-rs/clap/issues/5134>, but no one has stepped up
    // to fix it.
    //
    // Work around this by running the code on a thread with lots of stack
    // space. This is easier and more reliable than configuring the PE binary to
    // have a larger stack.
    fn on_big_stack<R: Send>(f: impl Send + FnOnce() -> R) -> R {
        if cfg!(windows) {
            std::thread::scope(|s| {
                std::thread::Builder::new()
                    .stack_size(0x400000)
                    .spawn_scoped(s, f)
                    .unwrap()
                    .join()
                    .unwrap()
            })
        } else {
            f()
        }
    }

    on_big_stack(Options::parse)
}

const DEFAULT_MEMORY_SIZE: u64 = 1024 * 1024 * 1024;

/// Guest memory configuration parsed from `--memory` (and, flattened, from
/// `--numa`).
///
/// Fields are the raw parsed options; callers apply the defaults (`size`
/// defaults to [`DEFAULT_MEMORY_SIZE`]; `transparent_hugepages` defaults to
/// `!hugepages`). Cross-field validation lives in [`MemoryCli::validate`].
#[derive(Debug, Clone, Default, PartialEq, Eq, vmm_cli::KeyValueArgs)]
pub struct MemoryCli {
    /// Guest RAM size. Defaults to [`DEFAULT_MEMORY_SIZE`] when unset.
    pub size: Option<vmm_cli::MemorySize>,
    /// Whether shared file-backed memory was explicitly requested (tri-state:
    /// unset / `on` / `off`).
    #[kv(flag)]
    pub shared: Option<bool>,
    /// Whether to prefetch guest RAM.
    #[kv(flag)]
    pub prefetch: bool,
    /// Whether to use transparent huge pages. When unset, defaults to enabled
    /// unless `hugepages` is set.
    #[kv(flag, key = "thp")]
    pub transparent_hugepages: Option<bool>,
    /// Whether to use explicit hugetlb memfd backing for guest RAM.
    #[kv(flag)]
    pub hugepages: bool,
    /// Explicit hugetlb page size.
    pub hugepage_size: Option<vmm_cli::MemorySize>,
    /// File used to back guest RAM.
    pub file: Option<PathBuf>,
}

impl MemoryCli {
    /// Validate cross-field constraints shared by `--memory` and `--numa`.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.hugepage_size.is_some() && !self.hugepages {
            anyhow::bail!("hugepage_size requires hugepages=on");
        }
        if self.hugepages {
            if self.shared == Some(false) {
                anyhow::bail!("hugepages=on conflicts with shared=off");
            }
            if self.file.is_some() {
                anyhow::bail!("hugepages=on conflicts with file=...");
            }
        }
        Ok(())
    }
}

/// NUMA node configuration parsed from `--numa`.
#[derive(Debug, Clone, PartialEq, Eq, vmm_cli::KeyValueArgs)]
pub struct NumaNodeCli {
    /// Memory configuration (size, shared, prefetch, hugepages, etc.).
    #[kv(flatten)]
    pub memory: MemoryCli,
    /// Host NUMA node to bind memory allocation to.
    pub host_numa_node: Option<u32>,
    /// Explicit VP indices for this node.
    pub vps: Option<vmm_cli::BracketRangeList>,
}

/// NUMA distance parsed from `--numa-distance`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumaDistanceCli {
    /// Source node index.
    pub src: u32,
    /// Destination node index.
    pub dst: u32,
    /// Distance value (10-255, 255 = unreachable).
    pub distance: u8,
}

/// OpenVMM virtual machine monitor.
///
/// This is not yet a stable interface and may change radically between
/// versions.
#[derive(Parser)]
pub struct Options {
    /// processor count
    #[clap(short = 'p', long, value_name = "COUNT", default_value = "1")]
    pub processors: u32,

    /// guest RAM configuration (`SIZE` or `key=value[,key=value...]`)
    #[clap(
        short = 'm',
        long,
        value_name = "PARAMS",
        default_value = "1GB",
        value_parser = parse_memory_config,
        conflicts_with = "numa",
        long_help = r#"Configure guest RAM.

Syntax: SIZE | key=value[,key=value...]

Size suffixes accept K, M, G, and T, optionally followed by B.

Options:
    size=<SIZE>              guest RAM size, default 1GB
    shared[=on|off]          use shared file-backed RAM, default on
    prefetch[=on|off]        pre-populate guest RAM mappings
    thp[=on|off]             mark guest RAM as THP-eligible (Linux), default on
    hugepages[=on|off]       allocate RAM from hugetlb/large pages (Linux, Windows)
    hugepage_size=<SIZE>     hugepage size, default 2MB; requires hugepages=on
    file=<PATH>              use an existing file as guest RAM backing

Examples:
    --memory 4G
    --memory size=64GB,hugepages=on,hugepage_size=2MB
    --memory size=4G,file=path/to/memory.bin
    --memory size=4G,thp=off"#
    )]
    pub memory: MemoryCli,

    /// NUMA node configuration (repeatable, one per node).
    ///
    /// Each --numa specifies one guest NUMA node. Mutually exclusive with
    /// --memory.
    #[clap(
        long,
        value_name = "PARAMS",
        value_parser = parse_numa_node,
        conflicts_with = "memory",
        long_help = r#"Configure a guest NUMA node (repeatable, one per node).

Syntax: key=value[,key=value...]

Options:
    size=<SIZE>              RAM for this node (required)
    shared[=on|off]          use shared file-backed RAM, default on
    prefetch[=on|off]        pre-populate guest RAM mappings
    thp[=on|off]             mark node RAM as THP-eligible (Linux), default on
    hugepages[=on|off]       allocate RAM from hugetlb/large pages (Linux, Windows)
    hugepage_size=<SIZE>     hugepage size, default 2MB; requires hugepages=on
    host_numa_node=<N>       bind allocation to host NUMA node N
    vps=<LIST>               explicit VP indices (e.g. "[0,1,2,3]")

  VP lists use bracket syntax with comma-separated indices and dash
  ranges: vps=[0,1] or vps=[0-3] or vps=[0,1,4-5]. An empty list, vps=[],
  declares a CPU-less node (e.g. a generic-initiator target); unlike a
  non-empty list, it may be combined with nodes that omit vps.

Examples:
    --numa size=2G --numa size=2G
    --numa size=2G,host_numa_node=0 --numa size=2G,host_numa_node=1
    --numa size=2G,hugepages=on,vps=[0,1] --numa size=2G,vps=[2,3]
    --numa size=2G,vps=[0-3] --numa size=2G,vps=[4-7]
    --numa size=2G --numa size=0,vps=[]"#
    )]
    pub numa: Option<Vec<NumaNodeCli>>,

    /// NUMA distance (repeatable). Format: SRC:DST:DISTANCE.
    ///
    /// SRC and DST are 0-based node indices. DISTANCE is 10-255 (10 = local, 255 = unreachable).
    /// Specify each direction explicitly (not auto-symmetric).
    #[clap(long, value_name = "SRC:DST:DIST", value_parser = parse_numa_distance, conflicts_with = "memory", requires = "numa")]
    pub numa_distance: Option<Vec<NumaDistanceCli>>,

    /// use shared memory segment
    #[clap(short = 'M', long, hide = true)]
    pub shared_memory: bool,

    /// prefetch guest RAM
    #[clap(long = "prefetch", hide = true, conflicts_with = "numa")]
    pub deprecated_prefetch: bool,

    /// back guest RAM with a file instead of anonymous memory.
    /// The file is created/opened and sized to the guest RAM size.
    /// Enables snapshot save (fsync) and restore (open + mmap).
    #[clap(
        long = "memory-backing-file",
        value_name = "FILE",
        hide = true,
        conflicts_with_all = ["deprecated_private_memory", "numa"]
    )]
    pub deprecated_memory_backing_file: Option<PathBuf>,

    /// Restore VM from a snapshot directory (implies file-backed memory from
    /// the snapshot's memory.bin). Cannot be used with --memory-backing-file.
    #[clap(
        long,
        value_name = "DIR",
        conflicts_with_all = ["deprecated_memory_backing_file", "numa"]
    )]
    pub restore_snapshot: Option<PathBuf>,

    /// use private anonymous memory for guest RAM
    #[clap(long = "private-memory", hide = true, conflicts_with_all = ["deprecated_memory_backing_file", "restore_snapshot", "numa"])]
    pub deprecated_private_memory: bool,

    /// enable transparent huge pages for guest RAM (Linux only; deprecated, THP is on by default)
    #[clap(long = "thp", hide = true, conflicts_with = "numa")]
    pub deprecated_thp: bool,

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

    /// Use a full device tree instead of ACPI tables for ARM64 Linux direct
    /// boot. By default, ARM64 uses ACPI mode (stub DT + EFI + ACPI tables).
    /// This flag selects the legacy DT-only path. Rejected on x86.
    #[clap(long, conflicts_with_all = ["uefi", "pcat", "igvm"])]
    pub device_tree: bool,

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

    /// Run without VMBus, even if --hv or --uefi are specified.
    #[clap(
        long,
        conflicts_with_all = [
            "vmbus_vsock_path",
            "vmbus_vtl2_vsock_path",
            "vmbus_redirect",
            "vmbus_max_version",
            "vmbus_com1_serial",
            "vmbus_com2_serial",
            "vtl2",
            "get",
            "pcat",
        ],
    )]
    pub no_vmbus: bool,

    /// disable the VTL0 alias map presented to VTL2 by default
    #[clap(long, requires("vtl2"))]
    pub no_alias_map: bool,

    /// enable isolation emulation
    #[clap(long, requires("vtl2"))]
    pub isolation: Option<IsolationCli>,

    /// the hybrid vsock listener path
    #[clap(long, value_name = "PATH", alias = "vsock-path")]
    pub vmbus_vsock_path: Option<String>,

    /// the VTL2 hybrid vsock listener path
    #[clap(long, value_name = "PATH", requires("vtl2"), alias = "vtl2-vsock-path")]
    pub vmbus_vtl2_vsock_path: Option<String>,

    /// the late map vtl0 ram access policy when vtl2 is enabled
    #[clap(long, requires("vtl2"), default_value = "halt")]
    pub late_map_vtl0_policy: Vtl0LateMapPolicyCli,

    /// attach a disk (can be passed multiple times)
    #[clap(long_help = r#"
e.g: --disk memdiff:file:/path/to/disk.vhd

syntax: <path> | kind:<arg>[,flag,opt=arg,...]

valid disk kinds:
    `mem:<len>`                    memory backed disk
        <len>: length of ramdisk, e.g.: `1G`
    `memdiff:<disk>`               memory backed diff disk
        <disk>: lower disk, e.g.: `file:base.img`
    `file:<path>[;direct][;create=<len>]`   file-backed disk
        <path>: path to file
        `;direct`: bypass the OS page cache
    `sql:<path>[;create=<len>]`    SQLite-backed disk (dev/test)
    `sqldiff:<path>[;create]:<disk>` SQLite diff layer on a backing disk
    `autocache:<key>:<disk>`       auto-cached SQLite layer (use `autocache::<disk>` to omit key; needs OPENVMM_AUTO_CACHE_PATH)
    `blob:<type>:<url>`            HTTP blob (read-only)
        <type>: `flat` or `vhd1`
    `crypt:<cipher>:<key_file>:<disk>` encrypted disk wrapper
        <cipher>: `xts-aes-256`
    `prwrap:<disk>`                persistent reservations wrapper

flags:
    `ro`                           open disk as read-only
    `dvd`                          specifies that device is cd/dvd and it is read_only
    `vtl2`                         assign this disk to VTL2
    `uh`                           relay this disk to VTL0 through SCSI-to-OpenHCL (show to VTL0 as SCSI)
    `uh-nvme`                      relay this disk to VTL0 through NVMe-to-OpenHCL (show to VTL0 as SCSI)

options:
    `pcie_port=<name>`             present the disk using pcie under the specified port, incompatible with `dvd`, `vtl2`, `uh`, and `uh-nvme`
    `on=<name>`                    attach to a named controller (NVMe or SCSI), incompatible with `pcie_port` and `vtl2`
    `nsid=<N>`                     NVMe namespace ID (1-based), requires `on`; auto-assigned if omitted
    `lun=<N>`                      SCSI LUN (0-based), requires `on`; auto-assigned if omitted
    `relay=<ctrl>[:<loc>]`         relay through OpenHCL to the named OpenHCL controller, with optional location (LUN or NSID)
"#)]
    #[clap(long, value_name = "FILE")]
    pub disk: Vec<DiskCli>,

    /// \[deprecated\] attach a disk via an NVMe controller
    ///
    /// Use --nvme-pci and --disk on=\<name\> instead.
    #[clap(long_help = r#"
e.g: --nvme memdiff:file:/path/to/disk.vhd

syntax: <path> | kind:<arg>[,flag,opt=arg,...]

valid disk kinds:
    `mem:<len>`                    memory backed disk
        <len>: length of ramdisk, e.g.: `1G`
    `memdiff:<disk>`               memory backed diff disk
        <disk>: lower disk, e.g.: `file:base.img`
    `file:<path>[;direct][;create=<len>]`   file-backed disk
        <path>: path to file
        `;direct`: bypass the OS page cache
    `sql:<path>[;create=<len>]`    SQLite-backed disk (dev/test)
    `sqldiff:<path>[;create]:<disk>` SQLite diff layer on a backing disk
    `autocache:<key>:<disk>`       auto-cached SQLite layer (use `autocache::<disk>` to omit key; needs OPENVMM_AUTO_CACHE_PATH)
    `blob:<type>:<url>`            HTTP blob (read-only)
        <type>: `flat` or `vhd1`
    `crypt:<cipher>:<key_file>:<disk>` encrypted disk wrapper
        <cipher>: `xts-aes-256`
    `prwrap:<disk>`                persistent reservations wrapper

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

    /// create a named NVMe controller
    #[clap(long_help = r#"
Create a named NVMe controller with an explicit transport.

syntax: id=<name>,pcie_port=<port> | id=<name>,vpci[=<guid>]

The controller name can be referenced by `--disk` with the `on=<name>`
option to attach namespaces to this controller.

options:
    `id=<name>`                    controller name (required)
    `pcie_port=<port>`             present on PCIe under the specified port
    `vpci[=<guid>]`                present via VPCI; optional instance GUID
    `vtl2`                         assign to VTL2 (default VTL0)

Exactly one of `pcie_port` or `vpci` must be specified.

Examples:
    --nvme-pci id=nvme0,pcie_port=p0
    --nvme-pci id=nvme1,vpci
    --nvme-pci id=nvme2,vpci=008091f6-9688-497d-9091-af347dc9173c
"#)]
    #[clap(long = "nvme-pci")]
    pub nvme_pci: Vec<NvmeControllerCli>,

    /// create a named VMBus SCSI controller
    #[clap(long_help = r#"
Create a named VMBus SCSI controller.

syntax: id=<name>[,sub_channels=<N>][,vtl2]

The controller name can be referenced by `--disk` with the `on=<name>`
option to attach disks to this controller.

options:
    `id=<name>`                    controller name (required)
    `sub_channels=<N>`             number of sub-channels (default 0)
    `vtl2`                         assign to VTL2 (default VTL0)

Examples:
    --vmbus-scsi id=scsi0
    --vmbus-scsi id=scsi1,sub_channels=4
"#)]
    #[clap(long = "vmbus-scsi")]
    pub vmbus_scsi: Vec<ScsiControllerCli>,

    /// register an OpenHCL-managed storage controller (relay target)
    #[clap(long_help = r#"
Register an OpenHCL-managed storage controller that can be used as a
relay target with `--disk ... relay=<name>`.

syntax: id=<name>,type=scsi|nvme[,guid=<guid>]

options:
    `id=<name>`                    controller name (required)
    `type=scsi|nvme`               controller protocol (required)
    `guid=<guid>`                  instance GUID (auto-derived from name if omitted)

Examples:
    --openhcl-controller id=vtl0-scsi,type=scsi
    --openhcl-controller id=vtl0-nvme,type=nvme,guid=09a59b81-...
"#)]
    #[clap(long = "openhcl-controller")]
    pub openhcl_controller: Vec<OpenhclControllerCli>,

    /// attach a CXL Type-3 test endpoint on a PCIe root port
    #[clap(long = "cxl-test", value_name = "mem:<len>,pcie_port=<name>")]
    pub cxl_test: Vec<CxlTestDeviceCli>,

    /// attach a disk via a virtio-blk controller
    #[clap(long_help = r#"
e.g: --virtio-blk memdiff:file:/path/to/disk.vhd

syntax: <path> | kind:<arg>[,flag,opt=arg,...]

valid disk kinds:
    `mem:<len>`                    memory backed disk
        <len>: length of ramdisk, e.g.: `1G`
    `memdiff:<disk>`               memory backed diff disk
        <disk>: lower disk, e.g.: `file:base.img`
    `file:<path>[;direct]`                  file-backed disk
        <path>: path to file
        `;direct`: bypass the OS page cache

flags:
    `ro`                           open disk as read-only

options:
    `pcie_port=<name>`             present the disk using pcie under the specified port
"#)]
    #[clap(long = "virtio-blk")]
    pub virtio_blk: Vec<DiskCli>,

    /// Attach a vhost-user device via a Unix socket.
    ///
    /// The first positional argument is the socket path. Options:
    ///
    /// ```text
    ///   type=blk|fs                        — device type (shorthand)
    ///   device_id=N                        — numeric virtio device ID
    ///   tag=NAME                           — mount tag (required for type=fs)
    ///   num_queues=N                       — queue count (type=blk/fs only)
    ///   queue_size=N                       — per-queue size (type=blk/fs only)
    ///   queue_sizes=[N,N,N]                — per-queue sizes (device_id= only)
    ///   pcie_port=NAME                     — present on PCIe under the specified port
    /// ```
    ///
    /// Examples:
    ///
    /// ```text
    ///   --vhost-user /tmp/vhost.sock,type=blk
    ///   --vhost-user /tmp/vhost.sock,type=blk,num_queues=4,queue_size=512
    ///   --vhost-user /tmp/vhost.sock,device_id=2,queue_sizes=[128,128]
    ///   --vhost-user /tmp/vhost.sock,type=blk,pcie_port=port0
    ///   --vhost-user /tmp/virtiofsd.sock,type=fs,tag=myfs
    ///   --vhost-user /tmp/virtiofsd.sock,type=fs,tag=myfs,num_queues=2,queue_size=1024
    /// ```
    #[cfg(target_os = "linux")]
    #[clap(long = "vhost-user")]
    pub vhost_user: Vec<VhostUserCli>,

    /// number of sub-channels for the SCSI controller
    #[clap(long, value_name = "COUNT", default_value = "0")]
    pub scsi_sub_channels: u16,

    /// expose a virtual NIC
    #[clap(long)]
    pub nic: bool,

    /// expose a virtual NIC with the given backend (consomme | dio | tap | none)
    ///
    /// Prefix with `uh:` to add this NIC via Mana emulation through OpenHCL,
    /// `vtl2:` to assign this NIC to VTL2, or `pcie_port=<port_name>:` to
    /// expose the NIC over emulated PCIe at the specified port.
    ///
    /// For consomme, forward host ports into the guest with `hostfwd=`:
    ///   --net consomme:hostfwd=tcp::3389-:3389
    ///   --net consomme:hostfwd=tcp:127.0.0.1:8080-:80
    ///   --net consomme:hostfwd=tcp:\[::1\]:8080-:80
    ///   --net consomme:10.0.0.0/24,hostfwd=tcp::22-:22,hostfwd=udp::5000-:5000
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

    /// VNC server configuration (listen address, port, client limit, etc.).
    #[clap(flatten)]
    pub vnc: VncCli,

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

    /// configure PCIe MSI controller for aarch64 (auto | its | v2m)
    #[cfg(guest_arch = "aarch64")]
    #[clap(long, default_value = "auto")]
    pub gic_msi: GicMsiCli,

    /// configure SMMUv3 IOMMU for an aarch64 PCIe root complex (repeatable).
    ///
    /// Syntax: `rc=<name>[,accel][,oas=auto|N]`.
    #[cfg(guest_arch = "aarch64")]
    #[clap(long, value_name = "SMMU_CONFIG")]
    pub smmu: Vec<SmmuCli>,

    /// COM1 binding, optionally prefixed with `debugger-mode:` (see below)
    /// (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    ///
    /// Prefix the binding with `debugger-mode:` to run this COM port in
    /// debugger mode for WinDbg kernel debugging over serial (KD), e.g.
    /// `--com1 debugger-mode:listen=<path>` or
    /// `--com1 debugger-mode:listen=tcp:<ip>:<port>`. In debugger mode OpenVMM
    /// keeps this port's backend drained and may drop bytes instead of applying
    /// backpressure, so the KD transport does not deadlock across guest
    /// resets/reboots (KD recovers dropped bytes via its own retransmission).
    /// Debugger mode is independent per COM port.
    #[clap(long, value_name = "SERIAL")]
    pub com1: Option<ComSerialConfigCli>,

    /// COM2 binding, optionally prefixed with `debugger-mode:` (see --com1)
    /// (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[clap(long, value_name = "SERIAL")]
    pub com2: Option<ComSerialConfigCli>,

    /// COM3 binding, optionally prefixed with `debugger-mode:` (see --com1)
    /// (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[clap(long, value_name = "SERIAL")]
    pub com3: Option<ComSerialConfigCli>,

    /// COM4 binding, optionally prefixed with `debugger-mode:` (see --com1)
    /// (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[clap(long, value_name = "SERIAL")]
    pub com4: Option<ComSerialConfigCli>,

    /// vmbus com1 serial binding (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[structopt(long, value_name = "SERIAL")]
    pub vmbus_com1_serial: Option<SerialConfigCli>,

    /// vmbus com2 serial binding (console | stderr | listen=\<path\> | file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> | term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[structopt(long, value_name = "SERIAL")]
    pub vmbus_com2_serial: Option<SerialConfigCli>,

    /// Only allow guest to host serial traffic
    #[clap(long)]
    pub serial_tx_only: bool,

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

    /// force UEFI to bounce-buffer all DMA traffic
    #[clap(long, requires("uefi"))]
    pub uefi_force_dma_bounce: bool,

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
    ///
    /// Prefix with `pcie_port=<port_name>:` to expose the device over
    /// emulated PCIe at the specified port.
    #[clap(long, value_name = "[pcie_port=PORT:]tag,root_path")]
    pub virtio_9p: Vec<FsArgs>,

    /// output debug info from the 9p server
    #[clap(long)]
    pub virtio_9p_debug: bool,

    /// add a virtio_fs device (e.g. myfs,C:\,uid=1000,gid=2000)
    ///
    /// Prefix with `pcie_port=<port_name>:` to expose the device over
    /// emulated PCIe at the specified port.
    #[clap(long, value_name = "[pcie_port=PORT:]tag,root_path,[options]")]
    pub virtio_fs: Vec<FsArgsWithOptions>,

    /// add a virtio_fs device for sharing memory (e.g. myfs,\SectionDirectoryPath)
    ///
    /// Prefix with `pcie_port=<port_name>:` to expose the device over
    /// emulated PCIe at the specified port.
    #[clap(long, value_name = "[pcie_port=PORT:]tag,root_path")]
    pub virtio_fs_shmem: Vec<FsArgs>,

    /// add a virtio_fs device under either the PCI or MMIO bus, or whatever the hypervisor supports (pci | mmio | auto)
    #[clap(long, value_name = "BUS", default_value = "auto")]
    pub virtio_fs_bus: VirtioBusCli,

    /// virtio PMEM device
    ///
    /// Prefix with `pcie_port=<port_name>:` to expose the device over
    /// emulated PCIe at the specified port.
    #[clap(long, value_name = "[pcie_port=PORT:]PATH")]
    pub virtio_pmem: Option<VirtioPmemArgs>,

    /// add a virtio entropy (RNG) device
    #[clap(long)]
    pub virtio_rng: bool,

    /// add a virtio-rng device under either the PCI or MMIO bus, or whatever the hypervisor supports (pci | mmio | vpci | auto)
    #[clap(long, value_name = "BUS", default_value = "auto")]
    pub virtio_rng_bus: VirtioBusCli,

    /// attach the virtio-rng device to the specified PCIe port (overrides --virtio-rng-bus)
    #[clap(long, value_name = "PORT", requires("virtio_rng"))]
    pub virtio_rng_pcie_port: Option<String>,

    /// virtio console device backed by a serial backend (/dev/hvc0 in guest)
    ///
    /// Accepts serial config (console | stderr | listen=\<path\> |
    /// file=\<path\> (overwrites) | listen=tcp:\<ip\>:\<port\> |
    /// term[=\<program\>]\[,name=\<windowtitle\>\] | none)
    #[clap(long)]
    pub virtio_console: Option<SerialConfigCli>,

    /// attach the virtio-console device to the specified PCIe port
    #[clap(long, value_name = "PORT", requires("virtio_console"))]
    pub virtio_console_pcie_port: Option<String>,

    /// add a virtio vsock device with the given Unix socket base path
    #[clap(long, value_name = "PATH")]
    pub virtio_vsock_path: Option<String>,

    /// expose a virtio network with the given backend (dio | vmnic | tap |
    /// none)
    ///
    /// Prefix with `uh:` to add this NIC via Mana emulation through OpenHCL,
    /// `vtl2:` to assign this NIC to VTL2, or `pcie_port=<port_name>:` to
    /// expose the NIC over emulated PCIe at the specified port.
    #[clap(long)]
    pub virtio_net: Vec<NicConfigCli>,

    /// send log output from the worker process to a file instead of stderr. the file will be overwritten.
    #[clap(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    /// write the process ID to the specified file on startup, and remove it on
    /// exit. the file is not removed if the process is killed with SIGKILL or
    /// crashes. no file locking is performed.
    #[clap(long, value_name = "PATH")]
    pub pidfile: Option<PathBuf>,

    /// \[deprecated\] run as a ttrpc server on the specified Unix socket
    ///
    /// Use `--rpc path=<PATH>,transport=ttrpc` instead.
    #[clap(long, value_name = "SOCKETPATH")]
    pub ttrpc: Option<PathBuf>,

    /// \[deprecated\] run as a grpc server on the specified Unix socket
    ///
    /// Use `--rpc path=<PATH>,transport=grpc` instead.
    #[clap(long, value_name = "SOCKETPATH", conflicts_with("ttrpc"))]
    pub grpc: Option<PathBuf>,

    /// run as an RPC server on the specified Unix socket
    #[clap(long_help = r#"
Run as an RPC server on the specified Unix socket.

syntax: path=<PATH>[,transport=<TRANSPORT>]

options:
    `path=<PATH>`                  Unix socket path to listen on (required)
    `transport=<TRANSPORT>`        wire transport to accept (default: auto)

valid transports:
    `auto`                         auto-detect ttrpc vs. gRPC per connection
    `ttrpc`                        accept ttrpc clients only
    `grpc`                         accept gRPC clients only

Examples:
    --rpc path=/tmp/openvmm.sock
    --rpc path=/tmp/openvmm.sock,transport=ttrpc
"#)]
    #[clap(
        long,
        value_name = "path=PATH[,transport=TRANSPORT]",
        conflicts_with("ttrpc"),
        conflicts_with("grpc")
    )]
    pub rpc: Option<RpcCli>,

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
    ///
    /// Prefix with `pcie_port=<port_name>:` to expose the nic over emulated PCIe
    /// at the specified port.
    #[clap(long)]
    pub mana: Vec<NicConfigCli>,

    /// use a specific hypervisor interface, with optional backend-specific
    /// parameters.
    ///
    /// Format: `name` or `name:key=val,key,...`
    ///
    /// WHP parameters (x86_64 guests only):
    ///   user_mode_apic       - use user-mode APIC emulator
    ///   no_enlightenments    - disable in-hypervisor enlightenments
    ///
    /// Examples:
    ///   --hypervisor whp
    ///   --hypervisor whp:user_mode_apic
    ///   --hypervisor whp:user_mode_apic,no_enlightenments
    ///   --hypervisor kvm
    #[clap(long)]
    pub hypervisor: Option<String>,

    /// expose hardware virtualization (VMX/SVM) to the guest so it can run its
    /// own hypervisor.
    ///
    /// Only supported on x86_64, and only by backends that support nested
    /// virtualization (currently WHP and KVM). Requires host support.
    #[clap(long)]
    pub nested_virt: bool,

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
    `file:<path>[;create=<len>]`   file-backed disk
        <path>: path to file
    `sql:<path>[;create=<len>]`    SQLite-backed disk (dev/test)
    `sqldiff:<path>[;create]:<disk>` SQLite diff layer on a backing disk
    `blob:<type>:<url>`            HTTP blob (read-only)
        <type>: `flat` or `vhd1`
    `crypt:<cipher>:<key_file>:<disk>` encrypted disk wrapper
        <cipher>: `xts-aes-256`

additional wrapper kinds (e.g., `autocache`, `prwrap`) are also supported;
this list is not exhaustive.

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
e.g: --floppy memdiff:file:/path/to/disk.vfd,ro

syntax: <path> | kind:<arg>[,flag,opt=arg,...]

valid disk kinds:
    `mem:<len>`                    memory backed disk
        <len>: length of ramdisk, e.g.: `1G`
    `memdiff:<disk>`               memory backed diff disk
        <disk>: lower disk, e.g.: `file:base.img`
    `file:<path>[;create=<len>]`   file-backed disk
        <path>: path to file
    `sql:<path>[;create=<len>]`    SQLite-backed disk (dev/test)
    `sqldiff:<path>[;create]:<disk>` SQLite diff layer on a backing disk
    `blob:<type>:<url>`            HTTP blob (read-only)
        <type>: `flat` or `vhd1`
    `crypt:<cipher>:<key_file>:<disk>` encrypted disk wrapper
        <cipher>: `xts-aes-256`

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

    /// what to do when the guest requests a reset: reset it (default), halt the
    /// VM for inspection, or exit the VMM process (use `exit:<code>` to set the
    /// exit status)
    #[clap(long, value_name = "ACTION", default_value = "reset", value_parser = parse_guest_power_action)]
    pub guest_reset_action: GuestPowerAction,

    /// what to do when the guest powers off or hibernates: halt the VM for
    /// inspection (default), reset it, or exit the VMM process (use
    /// `exit:<code>` to set the exit status)
    #[clap(long, value_name = "ACTION", default_value = "halt", value_parser = parse_guest_power_action)]
    pub guest_shutdown_action: GuestPowerAction,

    /// what to do when the guest triple-faults: halt the VM for inspection
    /// (default), reset it, or exit the VMM process (use `exit:<code>` to set
    /// the exit status)
    #[clap(long, value_name = "ACTION", default_value = "halt", value_parser = parse_guest_power_action)]
    pub guest_crash_action: GuestPowerAction,

    /// what to do when the guest watchdog fires (the guest stopped petting it):
    /// reset the VM (default), halt it for inspection, or exit the VMM process
    /// (use `exit:<code>` to set the exit status). Requires `--guest-watchdog`.
    #[clap(long, value_name = "ACTION", default_value = "reset", value_parser = parse_guest_power_action, requires = "guest_watchdog")]
    pub guest_watchdog_action: GuestPowerAction,

    /// write saved state .proto files to the specified path
    #[clap(long)]
    pub write_saved_state_proto: Option<PathBuf>,

    /// specify the IMC hive file for booting Windows
    #[clap(long)]
    pub imc: Option<PathBuf>,

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

    /// Enable AMD IOMMU (AMD-Vi) emulation on specified root complexes.
    /// Repeat for each root complex that should have an IOMMU, e.g.:
    ///   --amd-iommu rc0 --amd-iommu rc1
    /// The IOMMU appears at device 0 function 0 on each specified root
    /// complex. Requires --pcie-root-complex.
    #[cfg(guest_arch = "x86_64")]
    #[clap(long)]
    pub amd_iommu: Vec<String>,

    /// Enable Intel VT-d IOMMU emulation on specified root complexes.
    /// Repeat for each root complex that should have an IOMMU, e.g.:
    ///   --intel-vtd rc0 --intel-vtd rc1
    /// Mutually exclusive with --amd-iommu within the same VM.
    /// Requires --pcie-root-complex.
    #[cfg(guest_arch = "x86_64")]
    #[clap(long)]
    pub intel_vtd: Vec<String>,

    /// Attach a PCI Express root complex to the VM
    #[clap(long_help = r#"
Attach root complexes to the VM.

Examples:
    # Attach root complex rc0 on segment 0 with bus and MMIO ranges
    --pcie-root-complex rc0,segment=0,start_bus=0,end_bus=255,low_mmio=4M,high_mmio=1G

    # Configure HDM window size and restrictions (bitmask)
    --pcie-root-complex rc1,hdm=2G,hdm_window_restrictions=0x21

Syntax: <name>[,opt=arg,...]

Options:
    `segment=<value>`              configures the PCI Express segment, default 0
    `start_bus=<value>`            lowest valid bus number, default 0
    `end_bus=<value>`              highest valid bus number, default 255
    `low_mmio=<size>`              low MMIO window size, default 64M
    `high_mmio=<size>`             high MMIO window size, default 1G
    `low_mmio_base=<addr>`         pin low MMIO window base address (0x-prefixed hex)
    `high_mmio_base=<addr>`        pin high MMIO window base address (0x-prefixed hex)
    `hdm=<size>`                   HDM decoder MMIO window size (CFMWS window), default 1G
    `hdm_window_restrictions=<m>`  CFMWS window restriction bitmask (u16, decimal or 0x-prefixed hex),
                                   default DEVICE_COHERENT (bit 0, value 0x1)
    `preserve_bars`                keep pinned BARs at their assigned addresses
    `node=<value>`                 NUMA node the root complex is associated with
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

    # Attach root port rc0rp2 at device 5, function 0
    --pcie-root-port rc0:rc0rp2,addr=5

    # Attach root port rc0rp3 at device 5, function 1
    --pcie-root-port rc0:rc0rp3,addr=5.1

Syntax: <root_complex_name>:<name>[,opt,opt=arg,...]

Options:
    `addr=<dev>[.<fn>]`            device/function to place this port at (default:
                                   lowest available); dev 0-31, fn 0-7
    `hotplug`                      enable hotplug support for this root port
    `acs=<mask>`                   ACS capability bitmask (u16, decimal or 0x-prefixed hex)
    `cxl`                          configure this root port as CXL-capable
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
    `acs=<mask>`                    ACS capability bitmask for downstream switch ports
"#)]
    #[clap(long, conflicts_with("pcat"))]
    pub pcie_switch: Vec<GenericPcieSwitchCli>,

    /// Declare the device behind a PCIe port as an SRAT generic initiator
    #[clap(long_help = r#"
Declare that the device directly behind a PCIe port is a generic initiator
(GI) for a NUMA node, generating an SRAT Generic Initiator Affinity structure.

The port may be a root port or a switch downstream port, so this works for
devices that sit behind a switch (e.g. a GPU placed under a switch shared
with a NIC for peer-to-peer DMA). The port is resolved by name against the
live topology after switch downstream ports are enumerated.

Examples:
    # The device behind switch downstream port sw1-downstream-0 is a generic
    # initiator for NUMA node 1
    --pcie-generic-initiator port=sw1-downstream-0,node=1

    # Also works for a root port name
    --pcie-generic-initiator port=rp0,node=2

Syntax: port=<port_name>,node=<node>
"#)]
    #[clap(
        long = "pcie-generic-initiator",
        value_name = "port=<name>,node=<node>",
        conflicts_with("pcat")
    )]
    pub pcie_generic_initiator: Vec<PcieGenericInitiatorCli>,

    /// Attach a PCIe remote device to a downstream port
    #[clap(long_help = r#"
Attach PCIe devices to root ports or downstream switch ports
which are implemented in a simulator running in a remote process.

Examples:
    # Attach to root port rc0rp0 with default socket
    --pcie-remote rc0rp0

    # Attach with custom socket address
    --pcie-remote rc0rp0,socket=0.0.0.0:48914

    # Specify HU and controller identifiers
    --pcie-remote rc0rp0,hu=1,controller=0

    # Multiple devices on different ports
    --pcie-remote rc0rp0,socket=0.0.0.0:48914
    --pcie-remote rc0rp1,socket=0.0.0.0:48915

Syntax: <port_name>[,opt=arg,...]

Options:
    `socket=<address>`              TCP socket (default: localhost:48914)
    `hu=<value>`                    Hardware unit identifier (default: 0)
    `controller=<value>`            Controller identifier (default: 0)
"#)]
    #[clap(long, conflicts_with("pcat"))]
    pub pcie_remote: Vec<PcieRemoteCli>,

    /// Assign a host PCI device to the guest via VFIO (Linux only)
    #[clap(long_help = r#"
Assign a host PCI device to the guest via Linux VFIO.

The device must be bound to vfio-pci on the host before starting the VM.

Examples:
    --vfio host=0000:01:00.0,port=rp0
    --vfio host=0000:01:00.0,port=rp0,iommu=iommu0

Keys:
    host=<pci_bdf>    (required) PCI address on the host
    port=<name>       (required) Root port or downstream switch port name
    iommu=<id>        (optional) Reference to an --iommu object. When present,
                      uses VFIO cdev + iommufd instead of the legacy group path.
"#)]
    #[cfg(target_os = "linux")]
    #[clap(long, conflicts_with("pcat"))]
    pub vfio: Vec<VfioDeviceCli>,

    /// Create an iommufd context for VFIO cdev device assignment
    #[clap(long_help = r#"
Declare an iommufd context. Opens /dev/iommu so it can be referenced by
--vfio devices via the iommu=<id> key. The associated IOAS is allocated
the first time a --vfio device referring to this id is opened.

Requires Linux kernel >= 6.6 with iommufd support.

Examples:
    --iommu id=iommu0 --vfio host=0000:01:00.0,port=rp0,iommu=iommu0

Syntax: id=<name>
"#)]
    #[cfg(target_os = "linux")]
    #[clap(long, conflicts_with("pcat"))]
    pub iommu: Vec<IommuCli>,
}

impl Options {
    /// Returns the effective guest RAM size.
    pub fn memory_size(&self) -> u64 {
        self.memory.size.map(|m| m.0).unwrap_or(DEFAULT_MEMORY_SIZE)
    }

    /// Returns whether guest RAM should be prefetched.
    pub fn prefetch_memory(&self) -> bool {
        self.memory.prefetch || self.deprecated_prefetch
    }

    /// Returns whether guest RAM should use private anonymous backing.
    pub fn private_memory(&self) -> bool {
        self.memory.shared == Some(false) || self.deprecated_private_memory
    }

    /// Returns whether guest RAM should be marked THP-eligible.
    pub fn transparent_hugepages(&self) -> bool {
        self.memory
            .transparent_hugepages
            .unwrap_or(!self.memory.hugepages)
            || self.deprecated_thp
    }

    /// Returns the effective file backing path for guest RAM.
    pub fn memory_backing_file(&self) -> Option<&PathBuf> {
        self.memory
            .file
            .as_ref()
            .or(self.deprecated_memory_backing_file.as_ref())
    }

    /// Validates combinations that span the new `--memory` parser and legacy aliases.
    ///
    /// Only checks that cannot be expressed elsewhere live here. Conflicts
    /// within a single `--memory` string are enforced by the parser, and
    /// semantic constraints (platform support, private-vs-shared, huge pages
    /// vs. legacy RAM, etc.) are enforced by the membacking builder at VM
    /// build time; those are not duplicated here.
    pub fn validate_memory_options(&self) -> anyhow::Result<()> {
        if self.memory.file.is_some() && self.deprecated_memory_backing_file.is_some() {
            anyhow::bail!("--memory file=... conflicts with --memory-backing-file");
        }
        if self.memory.file.is_some() && self.restore_snapshot.is_some() {
            anyhow::bail!("--memory file=... conflicts with --restore-snapshot");
        }
        if self.memory.shared == Some(true) && self.deprecated_private_memory {
            anyhow::bail!("--memory shared=on conflicts with --private-memory");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FsArgs {
    pub tag: String,
    pub path: String,
    pub pcie_port: Option<String>,
}

impl FromStr for FsArgs {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (pcie_port, s) = parse_pcie_port_prefix(s);
        let mut s = s.split(',');
        let (Some(tag), Some(path), None) = (s.next(), s.next(), s.next()) else {
            anyhow::bail!("expected [pcie_port=<port>:]<tag>,<path>");
        };
        Ok(Self {
            tag: tag.to_owned(),
            path: path.to_owned(),
            pcie_port,
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
    /// Optional PCIe port name.
    pub pcie_port: Option<String>,
}

impl FromStr for FsArgsWithOptions {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (pcie_port, s) = parse_pcie_port_prefix(s);
        let mut s = s.split(',');
        let (Some(tag), Some(path)) = (s.next(), s.next()) else {
            anyhow::bail!("expected [pcie_port=<port>:]<tag>,<path>[,<options>]");
        };
        let options = s.collect::<Vec<_>>().join(";");
        Ok(Self {
            tag: tag.to_owned(),
            path: path.to_owned(),
            options,
            pcie_port,
        })
    }
}

/// What the VMM does on a guest power event (reset, power-off/hibernate,
/// triple-fault, or watchdog timeout). Parsed from `reset`, `halt`, `exit`, or
/// `exit:<code>`; a bare `exit` uses status 0.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GuestPowerAction {
    /// Restart the guest.
    Reset,
    /// Stop the VM but keep the VMM process, so it can be inspected or
    /// restarted from the REPL.
    Halt,
    /// Exit the VMM process with this status code.
    Exit(u8),
}

/// Parse a [`GuestPowerAction`] from `reset`, `halt`, `exit`, or `exit:<code>`.
/// A bare `exit` exits with status 0; `exit:<code>` exits with `<code>` (0-255).
fn parse_guest_power_action(s: &str) -> Result<GuestPowerAction, String> {
    match s {
        "reset" => Ok(GuestPowerAction::Reset),
        "halt" => Ok(GuestPowerAction::Halt),
        "exit" => Ok(GuestPowerAction::Exit(0)),
        _ => match s.strip_prefix("exit:") {
            Some(code) => code
                .parse::<u8>()
                .map(GuestPowerAction::Exit)
                .map_err(|err| format!("invalid exit code '{code}' (expected 0-255): {err}")),
            None => Err(format!(
                "expected reset, halt, exit, or exit:<code>, got '{s}'"
            )),
        },
    }
}

#[derive(Copy, Clone, clap::ValueEnum)]
pub enum VirtioBusCli {
    Auto,
    Mmio,
    Pci,
    Vpci,
}

/// Parse an optional `pcie_port=<name>:` prefix from a CLI argument string.
///
/// Returns `(Some(port_name), rest)` if the prefix is present, or
/// `(None, original)` if not.
fn parse_pcie_port_prefix(s: &str) -> (Option<String>, &str) {
    if let Some(rest) = s.strip_prefix("pcie_port=") {
        if let Some((port, rest)) = rest.split_once(':') {
            if !port.is_empty() {
                return (Some(port.to_string()), rest);
            }
        }
    }
    (None, s)
}

#[derive(Clone, Debug, PartialEq)]
pub struct VirtioPmemArgs {
    pub path: String,
    pub pcie_port: Option<String>,
}

impl FromStr for VirtioPmemArgs {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (pcie_port, s) = parse_pcie_port_prefix(s);
        if s.is_empty() {
            anyhow::bail!("expected [pcie_port=<port>:]<path>");
        }
        Ok(Self {
            path: s.to_owned(),
            pcie_port,
        })
    }
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
            n.checked_mul(multi.unwrap_or(1))
        }()
        .with_context(|| format!("invalid memory size '{0}'", s))
    }
}

/// Parses an address, which must be a `0x`-prefixed hexadecimal value.
fn parse_address(s: &str) -> anyhow::Result<u64> {
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .with_context(|| format!("invalid address '{s}', expected a 0x-prefixed hex value"))?;
    u64::from_str_radix(hex, 16).with_context(|| format!("invalid address '{s}'"))
}

fn parse_acs_capability_mask(value: &str) -> anyhow::Result<u16> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u16::from_str_radix(hex, 16).context("invalid ACS capability mask")
    } else {
        value.parse::<u16>().context("invalid ACS capability mask")
    }
}

fn parse_memory_config(s: &str) -> anyhow::Result<MemoryCli> {
    // Bare shortcut: `--memory 64G` sets only the size.
    let memory = if !s.contains('=') && !s.contains(',') {
        MemoryCli {
            size: Some(s.parse::<vmm_cli::MemorySize>()?),
            ..Default::default()
        }
    } else {
        s.parse::<MemoryCli>()?
    };
    memory.validate()?;
    Ok(memory)
}

fn parse_numa_node(s: &str) -> anyhow::Result<NumaNodeCli> {
    let node: NumaNodeCli = s.parse()?;
    anyhow::ensure!(
        node.memory.size.is_some(),
        "numa node requires 'size' option"
    );
    anyhow::ensure!(
        node.memory.file.is_none(),
        "'file' is not supported in --numa"
    );
    node.memory.validate()?;
    Ok(node)
}

fn parse_numa_distance(s: &str) -> anyhow::Result<NumaDistanceCli> {
    let parts: Vec<&str> = s.split(':').collect();
    anyhow::ensure!(
        parts.len() == 3,
        "expected SRC:DST:DISTANCE format, got '{s}'"
    );
    let src = parts[0].parse::<u32>().context("invalid source node")?;
    let dst = parts[1]
        .parse::<u32>()
        .context("invalid destination node")?;
    let distance = parts[2].parse::<u8>().context("invalid distance")?;
    anyhow::ensure!(
        distance >= 10,
        "distance must be >= 10 (10 = local), got {distance}"
    );
    Ok(NumaDistanceCli { src, dst, distance })
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
    // file:<path>[;direct][;create=<len>]
    File {
        path: PathBuf,
        create_with_len: Option<u64>,
        direct: bool,
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

struct FileOpts {
    path: PathBuf,
    create_with_len: Option<u64>,
    direct: bool,
}

fn parse_file_opts(arg: &str) -> anyhow::Result<FileOpts> {
    let mut path = arg;
    let mut create_with_len = None;
    let mut direct = false;

    // Parse semicolon-delimited options after the path.
    if let Some((p, rest)) = arg.split_once(';') {
        path = p;
        for opt in rest.split(';') {
            if let Some(len) = opt.strip_prefix("create=") {
                create_with_len = Some(parse_memory(len)?);
            } else if opt == "direct" {
                direct = true;
            } else {
                anyhow::bail!("invalid file option '{opt}', expected 'create=<len>' or 'direct'");
            }
        }
    }

    Ok(FileOpts {
        path: path.into(),
        create_with_len,
        direct,
    })
}

impl DiskCliKind {
    /// Parse an `autocache:[key]:<kind>` disk spec, given the cache path
    /// (normally read from `OPENVMM_AUTO_CACHE_PATH`).
    fn parse_autocache(
        arg: &str,
        cache_path: Result<String, std::env::VarError>,
    ) -> anyhow::Result<Self> {
        let (key, kind) = arg.split_once(':').context("expected [key]:kind")?;
        let cache_path = cache_path.context("must set cache path via OPENVMM_AUTO_CACHE_PATH")?;
        Ok(DiskCliKind::AutoCacheSqlite {
            cache_path,
            key: (!key.is_empty()).then(|| key.to_string()),
            disk: Box::new(kind.parse()?),
        })
    }
}

impl FromStr for DiskCliKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let disk = match s.split_once(':') {
            // convenience support for passing bare paths as file disks
            None => {
                let FileOpts {
                    path,
                    create_with_len,
                    direct,
                } = parse_file_opts(s)?;
                DiskCliKind::File {
                    path,
                    create_with_len,
                    direct,
                }
            }
            Some((kind, arg)) => match kind {
                "mem" => DiskCliKind::Memory(parse_memory(arg)?),
                "memdiff" => DiskCliKind::MemoryDiff(Box::new(arg.parse()?)),
                "sql" => {
                    let FileOpts {
                        path,
                        create_with_len,
                        direct,
                    } = parse_file_opts(arg)?;
                    if direct {
                        anyhow::bail!("'direct' is not supported for 'sql' disks");
                    }
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
                    Self::parse_autocache(arg, std::env::var("OPENVMM_AUTO_CACHE_PATH"))?
                }
                "prwrap" => DiskCliKind::PersistentReservationsWrapper(Box::new(arg.parse()?)),
                "file" => {
                    let FileOpts {
                        path,
                        create_with_len,
                        direct,
                    } = parse_file_opts(arg)?;
                    DiskCliKind::File {
                        path,
                        create_with_len,
                        direct,
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
                    let FileOpts {
                        path,
                        create_with_len,
                        direct,
                    } = parse_file_opts(s)?;
                    if path.has_root() {
                        DiskCliKind::File {
                            path,
                            create_with_len,
                            direct,
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

/// Wire transport selection for `--rpc`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum RpcTransportCli {
    /// Auto-detect ttrpc vs. gRPC per connection, based on the first byte of
    /// the stream.
    #[default]
    Auto,
    /// Accept ttrpc clients only.
    Ttrpc,
    /// Accept gRPC clients only.
    Grpc,
}

/// RPC server configuration parsed from `--rpc`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcCli {
    /// Unix socket path to listen on.
    pub path: PathBuf,
    /// Wire transport to accept.
    pub transport: RpcTransportCli,
}

impl FromStr for RpcCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let mut path = None;
        let mut transport = None;
        for part in s.split(',') {
            let (key, value) = part
                .split_once('=')
                .with_context(|| format!("invalid rpc option '{part}', expected key=value"))?;
            match key {
                "path" => {
                    anyhow::ensure!(path.is_none(), "duplicate option 'path'");
                    anyhow::ensure!(!value.is_empty(), "'path' requires a value");
                    path = Some(PathBuf::from(value));
                }
                "transport" => {
                    anyhow::ensure!(transport.is_none(), "duplicate option 'transport'");
                    transport = Some(match value {
                        "auto" => RpcTransportCli::Auto,
                        "ttrpc" => RpcTransportCli::Ttrpc,
                        "grpc" => RpcTransportCli::Grpc,
                        _ => anyhow::bail!(
                            "invalid transport '{value}', expected auto, ttrpc, or grpc"
                        ),
                    });
                }
                _ => anyhow::bail!("unknown rpc option '{key}'"),
            }
        }

        Ok(RpcCli {
            path: path.context("'path' is required")?,
            transport: transport.unwrap_or_default(),
        })
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

/// VNC server configuration options.
#[derive(clap::Args)]
pub struct VncCli {
    /// Listen for VNC connections. Implied by --gfx.
    #[clap(long)]
    pub vnc: bool,

    /// VNC port number
    #[clap(long, value_name = "PORT", default_value = "5900")]
    pub vnc_port: u16,

    /// VNC listen address (use 0.0.0.0 for all IPv4, :: for dual-stack IPv4+IPv6).
    /// Accepts a bare IP address (combined with --vnc-port) or a full socket
    /// address like [::1]:5900 (overrides --vnc-port).
    #[clap(long, value_name = "ADDRESS", default_value = "127.0.0.1")]
    pub vnc_listen: String,

    /// Maximum concurrent VNC clients (~8MB memory per client for framebuffer buffers)
    #[clap(long, value_name = "COUNT", default_value = "16")]
    pub vnc_max_clients: usize,

    /// When the client limit is reached, disconnect the oldest client
    /// instead of rejecting the new connection
    #[clap(long)]
    pub vnc_evict_oldest: bool,
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
    pub controller: Option<String>,
    pub nsid: Option<u32>,
    pub lun: Option<u8>,
    pub relay: Option<(String, Option<u32>)>,
}

#[derive(Copy, Clone)]
pub enum UnderhillDiskSource {
    Scsi,
    Nvme,
}

/// A `relay=<name>[:<location>]` target for an OpenHCL-managed controller.
struct RelayTarget {
    name: String,
    location: Option<u32>,
}

impl FromStr for RelayTarget {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(if let Some((name, loc)) = s.split_once(':') {
            RelayTarget {
                name: name.to_string(),
                location: Some(loc.parse::<u32>().context("invalid relay location")?),
            }
        } else {
            RelayTarget {
                name: s.to_string(),
                location: None,
            }
        })
    }
}

/// Raw `--disk`/`--nvme`/`--virtio-blk` options, resolved and validated into a
/// [`DiskCli`] by its `FromStr`.
#[derive(vmm_cli::KeyValueArgs)]
struct DiskArgs {
    #[kv(positional)]
    kind: DiskCliKind,
    #[kv(flag)]
    ro: bool,
    #[kv(flag)]
    dvd: bool,
    #[kv(flag, key = "vtl2", present = DeviceVtl::Vtl2, absent = DeviceVtl::Vtl0)]
    vtl: DeviceVtl,
    #[kv(flag)]
    uh: bool,
    #[kv(flag, key = "uh-nvme")]
    uh_nvme: bool,
    pcie_port: Option<String>,
    #[kv(key = "on")]
    controller: Option<String>,
    nsid: Option<u32>,
    lun: Option<u8>,
    relay: Option<RelayTarget>,
}

impl FromStr for DiskCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let args: DiskArgs = s.parse()?;

        let underhill = match (args.uh, args.uh_nvme) {
            (false, false) => None,
            (true, false) => Some(UnderhillDiskSource::Scsi),
            (false, true) => Some(UnderhillDiskSource::Nvme),
            (true, true) => anyhow::bail!("`uh` and `uh-nvme` are mutually exclusive"),
        };
        let read_only = args.ro || args.dvd;
        let is_dvd = args.dvd;
        let vtl = args.vtl;
        let pcie_port = args.pcie_port;
        let controller = args.controller;
        let nsid = args.nsid;
        let lun = args.lun;
        let relay = args.relay.map(|r| (r.name, r.location));

        if underhill.is_some() && vtl != DeviceVtl::Vtl0 {
            anyhow::bail!("`uh` or `uh-nvme` is incompatible with `vtl2`");
        }

        if pcie_port.is_some() && (underhill.is_some() || vtl != DeviceVtl::Vtl0 || is_dvd) {
            anyhow::bail!("`pcie_port` is incompatible with `uh`, `uh-nvme`, `vtl2`, and `dvd`");
        }

        if controller.is_some() && pcie_port.is_some() {
            anyhow::bail!("`on` is incompatible with `pcie_port`");
        }

        if controller.is_some() && vtl != DeviceVtl::Vtl0 {
            anyhow::bail!(
                "`vtl2` is incompatible with `on`; the controller's VTL determines placement"
            );
        }

        if controller.is_some() && underhill.is_some() {
            anyhow::bail!("`on` is incompatible with `uh` and `uh-nvme`; use `relay` instead");
        }

        if nsid.is_some() && controller.is_none() {
            anyhow::bail!("`nsid` requires `on`");
        }

        if lun.is_some() && controller.is_none() {
            anyhow::bail!("`lun` requires `on`");
        }

        if nsid.is_some() && lun.is_some() {
            anyhow::bail!("`nsid` and `lun` are mutually exclusive");
        }

        if relay.is_some() && controller.is_none() {
            anyhow::bail!("`relay` requires `on`");
        }

        if relay.is_some() && underhill.is_some() {
            anyhow::bail!("`relay` is incompatible with `uh` and `uh-nvme`");
        }

        Ok(DiskCli {
            vtl,
            kind: args.kind,
            read_only,
            is_dvd,
            underhill,
            pcie_port,
            controller,
            nsid,
            lun,
            relay,
        })
    }
}

/// The transport for a named NVMe controller.
#[derive(Clone, Debug, PartialEq, vmm_cli::KeyValueGroup)]
pub enum NvmeControllerTransport {
    /// Present via PCIe on the specified root port.
    #[kv(key = "pcie_port")]
    Pcie(String),
    /// Present via VPCI with an optional instance GUID.
    #[kv(key = "vpci")]
    Vpci(Option<Guid>),
}

/// CLI arguments for a named NVMe controller.
#[derive(Clone, Debug, vmm_cli::KeyValueArgs)]
pub struct NvmeControllerCli {
    /// Controller name, referenced by `--disk on=<name>`.
    pub id: String,
    /// Transport configuration.
    #[kv(flatten)]
    pub transport: NvmeControllerTransport,
    /// VTL assignment (default VTL0).
    #[kv(flag, key = "vtl2", present = DeviceVtl::Vtl2, absent = DeviceVtl::Vtl0)]
    pub vtl: DeviceVtl,
}

/// CLI arguments for a named VMBus SCSI controller.
#[derive(Clone, Debug, vmm_cli::KeyValueArgs)]
pub struct ScsiControllerCli {
    /// Controller name, referenced by `--disk on=<name>`.
    pub id: String,
    /// Number of sub-channels.
    #[kv(default)]
    pub sub_channels: u16,
    /// VTL assignment (default VTL0).
    #[kv(flag, key = "vtl2", present = DeviceVtl::Vtl2, absent = DeviceVtl::Vtl0)]
    pub vtl: DeviceVtl,
}

/// Protocol type for an OpenHCL-managed controller.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum OpenhclControllerType {
    Scsi,
    Nvme,
}

/// CLI arguments for an OpenHCL-managed storage controller (relay target).
#[derive(Clone, Debug, vmm_cli::KeyValueArgs)]
pub struct OpenhclControllerCli {
    /// Controller name, referenced by `--disk ... relay=<name>`.
    pub id: String,
    /// Controller protocol.
    #[kv(key = "type")]
    pub controller_type: OpenhclControllerType,
    /// Instance GUID (auto-derived from name if omitted).
    pub guid: Option<Guid>,
}

impl FromStr for OpenhclControllerType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "scsi" => OpenhclControllerType::Scsi,
            "nvme" => OpenhclControllerType::Nvme,
            other => anyhow::bail!("unknown controller type: '{other}'"),
        })
    }
}

/// CLI arguments for a CXL Type-3 test endpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct CxlTestDeviceCli {
    /// Size of HDM memory the test device should expose and back.
    pub hdm_size: u64,
    /// PCIe root port name where the device is attached.
    pub pcie_port: String,
}

impl FromStr for CxlTestDeviceCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let mut opts = s.split(',');
        let first = opts.next().context("expected CXL test device config")?;
        let (kind, arg) = first
            .split_once(':')
            .context("expected CXL test syntax: mem:<len>")?;

        if kind != "mem" {
            anyhow::bail!("unsupported CXL test backing kind '{kind}', expected 'mem'");
        }

        let hdm_size = parse_memory(arg).context("failed to parse CXL test HDM size")?;
        let mut pcie_port = None;

        for opt in opts {
            let mut kv = opt.split('=');
            let key = kv.next().unwrap_or_default();
            match key {
                "pcie_port" => {
                    let val = kv.next();
                    if val.is_none_or(|v| v.is_empty()) {
                        anyhow::bail!("`pcie_port` requires a port name");
                    }
                    pcie_port = Some(val.unwrap().to_string());
                }
                _ => anyhow::bail!("unknown option: '{opt}'"),
            }
        }

        let Some(pcie_port) = pcie_port else {
            anyhow::bail!("`pcie_port=<name>` is required for `--cxl-test`");
        };

        Ok(Self {
            hdm_size,
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
#[derive(Clone, Debug, PartialEq, vmm_cli::KeyValueArgs)]
pub struct FloppyDiskCli {
    #[kv(positional)]
    pub kind: DiskCliKind,
    #[kv(flag, key = "ro")]
    pub read_only: bool,
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

/// A COM port binding, optionally prefixed with `debugger-mode:` to run the
/// port in debugger mode for WinDbg / KD-over-serial.
#[derive(Clone, Debug, PartialEq)]
pub struct ComSerialConfigCli {
    /// Whether this COM port runs in debugger mode (for WinDbg / KD-over-serial).
    pub debugger_mode: bool,
    /// The serial backend for this COM port.
    pub backend: SerialConfigCli,
}

impl FromStr for ComSerialConfigCli {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.strip_prefix("debugger-mode:") {
            Some(rest) => Ok(Self {
                debugger_mode: true,
                backend: rest.parse()?,
            }),
            None => Ok(Self {
                debugger_mode: false,
                backend: s.parse()?,
            }),
        }
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
    Consomme {
        cidr: Option<String>,
        host_fwd: Vec<HostPortConfigCli>,
    },
    Dio {
        id: Option<String>,
    },
    Tap {
        name: String,
    },
}

/// Parsed host port forwarding configuration from the CLI.
#[derive(Clone, Debug, PartialEq)]
pub struct HostPortConfigCli {
    pub protocol: HostPortProtocolCli,
    pub host_address: Option<std::net::IpAddr>,
    pub host_port: u16,
    pub guest_port: u16,
}

/// Protocol for host port forwarding.
#[derive(Clone, Debug, PartialEq)]
pub enum HostPortProtocolCli {
    Tcp,
    Udp,
}

fn parse_hostfwd(s: &str) -> Result<HostPortConfigCli, String> {
    // Format: protocol:[hostaddr]:hostport-[guestaddr]:guestport
    // Examples: "tcp::3389-:3389", "tcp:127.0.0.1:8080-:80", "tcp:[::1]:8080-:80"
    let (host_part, guest_part) = s.split_once('-').ok_or_else(|| {
        format!(
            "invalid hostfwd format '{s}', \
             expected 'proto:[hostaddr]:hostport-[guestaddr]:guestport'"
        )
    })?;

    // Extract protocol from host part (first colon-delimited field)
    let (proto, host_addr_port) = host_part.split_once(':').ok_or_else(|| {
        format!("invalid hostfwd host part '{host_part}', expected 'proto:[hostaddr]:hostport'")
    })?;
    let protocol = match proto {
        "tcp" => HostPortProtocolCli::Tcp,
        "udp" => HostPortProtocolCli::Udp,
        other => {
            return Err(format!(
                "unknown hostfwd protocol '{other}', expected 'tcp' or 'udp'"
            ));
        }
    };

    let (host_address, host_port) = parse_addr_port(host_addr_port)
        .map_err(|e| format!("invalid hostfwd host address/port: {e}"))?;
    let (_, guest_port) = parse_addr_port(guest_part)
        .map_err(|e| format!("invalid hostfwd guest address/port: {e}"))?;

    Ok(HostPortConfigCli {
        protocol,
        host_address,
        host_port,
        guest_port,
    })
}

/// Parse an address-port pair in one of these forms:
/// - `[ipv6addr]:port`
/// - `addr:port`
/// - `:port`  (empty address)
/// - `port`   (no address)
fn parse_addr_port(s: &str) -> Result<(Option<std::net::IpAddr>, u16), String> {
    if let Some(rest) = s.strip_prefix('[') {
        // Bracketed IPv6 address: [addr]:port
        let (addr, port) = rest
            .split_once("]:")
            .ok_or_else(|| format!("expected '[addr]:port', got '[{rest}'"))?;
        let port: u16 = port.parse().map_err(|_| format!("invalid port '{port}'"))?;
        let addr: std::net::IpAddr = addr
            .parse()
            .map_err(|e| format!("invalid address '{addr}': {e}"))?;
        Ok((Some(addr), port))
    } else {
        match s.rsplit_once(':') {
            Some((addr, port)) => {
                let port: u16 = port.parse().map_err(|_| format!("invalid port '{port}'"))?;
                let addr = if addr.is_empty() {
                    None
                } else {
                    let parsed: std::net::IpAddr = addr
                        .parse()
                        .map_err(|e| format!("invalid address '{addr}': {e}"))?;
                    Some(parsed)
                };
                Ok((addr, port))
            }
            None => {
                let port: u16 = s.parse().map_err(|_| format!("invalid port '{s}'"))?;
                Ok((None, port))
            }
        }
    }
}

impl FromStr for EndpointConfigCli {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let ret = match s.split(':').collect::<Vec<_>>().as_slice() {
            ["none"] => EndpointConfigCli::None,
            ["consomme", rest @ ..] => {
                let remaining = rest.join(":");
                let mut cidr = None;
                let mut host_fwd = Vec::new();
                for opt in remaining.split(',').filter(|s| !s.is_empty()) {
                    if let Some(fwd) = opt.strip_prefix("hostfwd=") {
                        host_fwd.push(parse_hostfwd(fwd)?);
                    } else if cidr.is_none() {
                        cidr = Some(opt.to_owned());
                    } else {
                        return Err(format!("unexpected consomme option '{opt}'"));
                    }
                }
                EndpointConfigCli::Consomme { cidr, host_fwd }
            }
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
    pub pcie_port: Option<String>,
}

impl FromStr for NicConfigCli {
    type Err = String;

    fn from_str(mut s: &str) -> Result<Self, Self::Err> {
        let mut vtl = DeviceVtl::Vtl0;
        let mut max_queues = None;
        let mut underhill = false;
        let mut pcie_port = None;
        while let Some((opt, rest)) = s.split_once(':') {
            if let Some((opt, val)) = opt.split_once('=') {
                match opt {
                    "queues" => {
                        max_queues = Some(val.parse().map_err(|_| "failed to parse queue count")?);
                    }
                    "pcie_port" => {
                        if val.is_empty() {
                            return Err("`pcie_port=` requires port name argument".into());
                        }
                        pcie_port = Some(val.to_string());
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

        if pcie_port.is_some() && (underhill || vtl != DeviceVtl::Vtl0) {
            return Err("`pcie_port` is incompatible with `uh` and `vtl2`".into());
        }

        let endpoint = s.parse()?;
        Ok(NicConfigCli {
            vtl,
            endpoint,
            max_queues,
            underhill,
            pcie_port,
        })
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

/// PCIe MSI controller selection for aarch64.
#[derive(Debug, Copy, Clone, Default, ValueEnum)]
pub enum GicMsiCli {
    /// Use ITS when available, fall back to GICv2m.
    #[default]
    Auto,
    /// Force GICv3 ITS (LPI-based MSIs).
    Its,
    /// Force GICv2m (SPI-based MSIs).
    V2m,
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
    pub low_mmio_base: Option<u64>,
    pub high_mmio_base: Option<u64>,
    pub preserve_bars: bool,
    pub hdm: u64,
    pub hdm_window_restrictions: CfmwsWindowRestrictions,
    pub vnode: Option<u32>,
}

/// A `0x`-prefixed hexadecimal address, used for MMIO base overrides.
struct HexAddress(u64);

impl FromStr for HexAddress {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(HexAddress(parse_address(s)?))
    }
}

/// A CFMWS window-restrictions bitmask (`0x21` or `33`).
struct CfmwsWindowRestrictionsCli(CfmwsWindowRestrictions);

impl FromStr for CfmwsWindowRestrictionsCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(CfmwsWindowRestrictionsCli(
            parse_cxl_cfmws_window_restriction_u16_bitmask(s)?,
        ))
    }
}

/// Raw `--pcie-root-complex` options, parsed declaratively. Validated and
/// mapped into the public [`PcieRootComplexCli`] by its `FromStr`.
#[derive(vmm_cli::KeyValueArgs)]
struct PcieRootComplexArgs {
    #[kv(positional)]
    name: String,
    #[kv(default)]
    segment: u16,
    #[kv(default)]
    start_bus: u8,
    #[kv(default = 255)]
    end_bus: u8,
    #[kv(default = vmm_cli::MemorySize(64 * 1024 * 1024))]
    low_mmio: vmm_cli::MemorySize,
    #[kv(default = vmm_cli::MemorySize(1024 * 1024 * 1024))]
    high_mmio: vmm_cli::MemorySize,
    low_mmio_base: Option<HexAddress>,
    high_mmio_base: Option<HexAddress>,
    #[kv(flag)]
    preserve_bars: bool,
    #[kv(default = vmm_cli::MemorySize(1024 * 1024 * 1024))]
    hdm: vmm_cli::MemorySize,
    #[kv(default = CfmwsWindowRestrictionsCli(CfmwsWindowRestrictions::DEVICE_COHERENT))]
    hdm_window_restrictions: CfmwsWindowRestrictionsCli,
    #[kv(key = "node")]
    vnode: Option<u32>,
}

impl FromStr for PcieRootComplexCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let args: PcieRootComplexArgs = s.parse()?;

        if args.start_bus > args.end_bus {
            anyhow::bail!("start_bus must be <= end_bus");
        }

        let low_mmio = u32::try_from(args.low_mmio.0).context("low MMIO size exceeds 32 bits")?;

        Ok(PcieRootComplexCli {
            name: args.name,
            segment: args.segment,
            start_bus: args.start_bus,
            end_bus: args.end_bus,
            low_mmio,
            high_mmio: args.high_mmio.0,
            low_mmio_base: args.low_mmio_base.map(|a| a.0),
            high_mmio_base: args.high_mmio_base.map(|a| a.0),
            preserve_bars: args.preserve_bars,
            hdm: args.hdm.0,
            hdm_window_restrictions: args.hdm_window_restrictions.0,
            vnode: args.vnode,
        })
    }
}

fn parse_cxl_cfmws_window_restriction_u16_bitmask(
    s: &str,
) -> anyhow::Result<CfmwsWindowRestrictions> {
    let bits = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).context("invalid hex bitmask")?
    } else {
        u16::from_str(s).context("invalid decimal bitmask")?
    };

    CfmwsWindowRestrictions::try_from_bits(bits)
        .context("bitmask includes reserved CFMWS window restriction bits")
}

#[derive(Clone, Debug, PartialEq)]
pub struct PcieRootPortCli {
    pub root_complex_name: String,
    pub name: String,
    pub devfn: Option<u8>,
    pub hotplug: bool,
    pub acs_capabilities_supported: Option<u16>,
    pub cxl: bool,
}

/// A colon-joined `parent:child` name pair used as the positional head of
/// `--pcie-root-port` and `--pcie-switch`.
struct PortNamePair {
    parent: String,
    child: String,
}

impl FromStr for PortNamePair {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let mut it = s.split(':');
        let parent = it
            .next()
            .filter(|x| !x.is_empty())
            .context("expected parent name")?;
        let child = it
            .next()
            .filter(|x| !x.is_empty())
            .context("expected child name")?;
        anyhow::ensure!(it.next().is_none(), "unexpected token in '{s}'");
        Ok(PortNamePair {
            parent: parent.to_string(),
            child: child.to_string(),
        })
    }
}

/// A PCIe device/function address (`XX[.Y]`) parsed into a devfn.
struct PcieAddr(u8);

impl FromStr for PcieAddr {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(PcieAddr(parse_pcie_addr(s)?))
    }
}

/// An ACS capability bitmask (hex `0x..` or decimal).
struct AcsMask(u16);

impl FromStr for AcsMask {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(AcsMask(parse_acs_capability_mask(s)?))
    }
}

/// Raw `--pcie-root-port` options, mapped into [`PcieRootPortCli`].
#[derive(vmm_cli::KeyValueArgs)]
struct RootPortArgs {
    #[kv(positional)]
    names: PortNamePair,
    addr: Option<PcieAddr>,
    #[kv(flag)]
    hotplug: bool,
    acs: Option<AcsMask>,
    #[kv(flag)]
    cxl: bool,
}

impl FromStr for PcieRootPortCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let args: RootPortArgs = s.parse()?;
        Ok(PcieRootPortCli {
            root_complex_name: args.names.parent,
            name: args.names.child,
            devfn: args.addr.map(|a| a.0),
            hotplug: args.hotplug,
            acs_capabilities_supported: args.acs.map(|a| a.0),
            cxl: args.cxl,
        })
    }
}

/// Parses a PCIe address of the form `XX[.Y]`, where `XX` is the device number
/// (0-31) and the optional `Y` is the function number (0-7), into a devfn
/// (`device << 3 | function`).
fn parse_pcie_addr(s: &str) -> anyhow::Result<u8> {
    let parse_int = |v: &str| -> anyhow::Result<u8> {
        if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
            u8::from_str_radix(hex, 16).context("invalid hex number")
        } else {
            v.parse().context("invalid number")
        }
    };

    let mut parts = s.split('.');
    let device = parse_int(parts.next().context("expected device number")?)?;
    let function = match parts.next() {
        Some(f) => parse_int(f)?,
        None => 0,
    };
    if parts.next().is_some() {
        anyhow::bail!("unexpected token in addr '{s}'");
    }
    if device > 31 {
        anyhow::bail!("device number {device} out of range (0-31)");
    }
    if function > 7 {
        anyhow::bail!("function number {function} out of range (0-7)");
    }
    Ok((device << 3) | function)
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenericPcieSwitchCli {
    pub port_name: String,
    pub name: String,
    pub num_downstream_ports: u8,
    pub hotplug: bool,
    pub acs_capabilities_supported: Option<u16>,
}

/// Raw `--pcie-switch` options, mapped into [`GenericPcieSwitchCli`].
#[derive(vmm_cli::KeyValueArgs)]
struct SwitchArgs {
    #[kv(positional)]
    names: PortNamePair,
    #[kv(default = 4)]
    num_downstream_ports: u8,
    #[kv(flag)]
    hotplug: bool,
    acs: Option<AcsMask>,
}

impl FromStr for GenericPcieSwitchCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let args: SwitchArgs = s.parse()?;
        Ok(GenericPcieSwitchCli {
            port_name: args.names.parent,
            name: args.names.child,
            num_downstream_ports: args.num_downstream_ports,
            hotplug: args.hotplug,
            acs_capabilities_supported: args.acs.map(|a| a.0),
        })
    }
}

/// CLI configuration mapping a PCIe port name to a generic-initiator NUMA node.
#[derive(Clone, Debug, PartialEq, vmm_cli::KeyValueArgs)]
pub struct PcieGenericInitiatorCli {
    /// Name of the PCIe port (root port or switch downstream port) behind
    /// which the generic-initiator device resides.
    #[kv(key = "port")]
    pub port_name: String,
    /// NUMA node the device is a generic initiator for.
    pub node: u32,
}

/// CLI configuration for a PCIe remote device.
#[derive(Clone, Debug, PartialEq, vmm_cli::KeyValueArgs)]
pub struct PcieRemoteCli {
    /// Name of the PCIe downstream port to attach to.
    #[kv(positional)]
    pub port_name: String,
    /// TCP socket address for the remote simulator.
    #[kv(key = "socket")]
    pub socket_addr: Option<String>,
    /// Hardware unit identifier for plug request.
    #[kv(default)]
    pub hu: u16,
    /// Controller identifier for plug request.
    #[kv(default)]
    pub controller: u16,
}

/// CLI configuration for a VFIO-assigned PCI device.
///
/// Syntax: `host=<bdf>,port=<name>[,iommu=<id>][,bar0=pt..bar5=pt]`
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub struct VfioDeviceCli {
    /// Name of the PCIe downstream port to attach to.
    pub port_name: String,
    /// PCI BDF address of the device on the host (e.g., "0000:01:00.0").
    pub pci_id: String,
    /// Optional iommufd context ID. When set, uses VFIO cdev + iommufd
    /// instead of the legacy group/container path.
    pub iommu: Option<String>,
    /// Per-BAR passthrough flags. When `bar_pt[i]` is true, the virtual
    /// BAR is pre-programmed with the physical BAR address (GPA = HPA).
    pub bar_pt: [bool; 6],
}

/// Marker for a BAR passthrough value; only `pt` is accepted.
#[cfg(target_os = "linux")]
struct Pt;

#[cfg(target_os = "linux")]
impl FromStr for Pt {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(s == "pt", "expected 'pt'");
        Ok(Pt)
    }
}

/// Per-BAR passthrough flags (`bar0=pt` .. `bar5=pt`), flattened into
/// [`VfioArgs`].
#[cfg(target_os = "linux")]
#[derive(vmm_cli::KeyValueArgs)]
struct BarFlags {
    bar0: Option<Pt>,
    bar1: Option<Pt>,
    bar2: Option<Pt>,
    bar3: Option<Pt>,
    bar4: Option<Pt>,
    bar5: Option<Pt>,
}

/// Raw `--vfio` options, resolved and validated into a [`VfioDeviceCli`].
#[cfg(target_os = "linux")]
#[derive(vmm_cli::KeyValueArgs)]
struct VfioArgs {
    host: String,
    port: String,
    iommu: Option<String>,
    #[kv(flatten)]
    bars: BarFlags,
}

#[cfg(target_os = "linux")]
impl FromStr for VfioDeviceCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let args: VfioArgs = s.parse()?;

        // Reject path separators to prevent sysfs path traversal via Path::join.
        if args.host.contains('/') || args.host.contains("..") {
            anyhow::bail!("PCI address must not contain path separators");
        }

        let bars = args.bars;
        let bar_pt = [
            bars.bar0.is_some(),
            bars.bar1.is_some(),
            bars.bar2.is_some(),
            bars.bar3.is_some(),
            bars.bar4.is_some(),
            bars.bar5.is_some(),
        ];

        Ok(VfioDeviceCli {
            port_name: args.port,
            pci_id: args.host,
            iommu: args.iommu,
            bar_pt,
        })
    }
}

/// CLI configuration for an SMMUv3 instance.
///
/// Syntax: `rc=<name>[,accel][,oas=auto|N]`. `oas` defaults to `auto`.
#[cfg(guest_arch = "aarch64")]
#[derive(Clone, Debug, vmm_cli::KeyValueArgs)]
pub struct SmmuCli {
    /// Name of the PCIe root complex this SMMU covers.
    #[kv(key = "rc")]
    pub rc_name: String,
    /// Enable HW-accelerated nested translation (iommufd).
    #[kv(flag)]
    pub accel: bool,
    /// Output address size policy.
    #[kv(default)]
    pub oas: SmmuOasCli,
}

/// Output address size (OAS) policy parsed from `--smmu`.
#[cfg(guest_arch = "aarch64")]
#[derive(Clone, Copy, Debug, Default)]
pub enum SmmuOasCli {
    /// Advertise a fixed default OAS (see the `--smmu` docs for the sizing
    /// policy and when a larger fixed OAS is required).
    #[default]
    Auto,
    /// Fixed OAS in bits (one of 32, 36, 40, 42, 44, 48, 52).
    Fixed(u8),
}

#[cfg(guest_arch = "aarch64")]
impl FromStr for SmmuOasCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(if s == "auto" {
            SmmuOasCli::Auto
        } else {
            SmmuOasCli::Fixed(s.parse().context("oas must be 'auto' or a number")?)
        })
    }
}

/// CLI configuration for an iommufd context.
///
/// Syntax: `id=<name>`
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, vmm_cli::KeyValueArgs)]
pub struct IommuCli {
    /// Unique identifier for this iommufd context.
    pub id: String,
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

#[cfg(target_os = "linux")]
#[derive(Clone)]
pub enum VhostUserDeviceTypeCli {
    /// Block device — config from backend via GET_CONFIG, with num_queues
    /// patched by the frontend.
    Blk {
        num_queues: Option<u16>,
        queue_size: Option<u16>,
    },
    /// Filesystem device — frontend-owned config with mount tag.
    Fs {
        tag: String,
        num_queues: Option<u16>,
        queue_size: Option<u16>,
    },
    /// Generic device identified by numeric virtio device ID.
    Other {
        device_id: u16,
        queue_sizes: Vec<u16>,
    },
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
pub struct VhostUserCli {
    pub socket_path: String,
    pub device_type: VhostUserDeviceTypeCli,
    pub pcie_port: Option<String>,
}

/// Raw `--vhost-user` options, resolved into a [`VhostUserCli`] by its
/// `FromStr`.
#[cfg(target_os = "linux")]
#[derive(vmm_cli::KeyValueArgs)]
struct VhostUserArgs {
    #[kv(positional)]
    socket_path: String,
    #[kv(key = "type")]
    type_name: Option<String>,
    device_id: Option<u16>,
    tag: Option<String>,
    pcie_port: Option<String>,
    num_queues: Option<u16>,
    queue_size: Option<u16>,
    queue_sizes: Option<vmm_cli::BracketList<u16>>,
}

#[cfg(target_os = "linux")]
impl FromStr for VhostUserCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let mut args: VhostUserArgs = s.parse()?;
        let type_name = args.type_name.take();

        if type_name.is_some() == args.device_id.is_some() {
            anyhow::bail!("must specify type=<name> or device_id=<N>");
        }

        // Each variant consumes the options it accepts; whatever is left over
        // afterward was used with the wrong device type.
        let device_type = match type_name.as_deref() {
            Some("fs") => VhostUserDeviceTypeCli::Fs {
                tag: args.tag.take().context("type=fs requires tag=<name>")?,
                num_queues: args.num_queues.take(),
                queue_size: args.queue_size.take(),
            },
            Some("blk") => VhostUserDeviceTypeCli::Blk {
                num_queues: args.num_queues.take(),
                queue_size: args.queue_size.take(),
            },
            Some(ty) => anyhow::bail!("unknown vhost-user device type: '{ty}'"),
            None => {
                let queue_sizes = args
                    .queue_sizes
                    .take()
                    .context("device_id= requires queue_sizes=[N,N,...]")?
                    .0;
                anyhow::ensure!(!queue_sizes.is_empty(), "queue_sizes must be non-empty");
                VhostUserDeviceTypeCli::Other {
                    device_id: args.device_id.unwrap(),
                    queue_sizes,
                }
            }
        };

        if args.tag.is_some() {
            anyhow::bail!("tag= is only valid for type=fs");
        }
        if args.queue_sizes.is_some() {
            anyhow::bail!("queue_sizes= is only valid for device_id=");
        }
        if args.num_queues.is_some() || args.queue_size.is_some() {
            anyhow::bail!(
                "num_queues= and queue_size= are not valid for device_id=; use queue_sizes="
            );
        }

        Ok(VhostUserCli {
            socket_path: args.socket_path,
            device_type,
            pcie_port: args.pcie_port,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use test_with_tracing::test;

    #[test]
    fn test_parse_rpc() {
        // explicit path, default transport
        let rpc = RpcCli::from_str("path=/tmp/openvmm.sock").unwrap();
        assert_eq!(rpc.path, Path::new("/tmp/openvmm.sock"));
        assert_eq!(rpc.transport, RpcTransportCli::Auto);

        // explicit transport
        for (s, transport) in [
            ("auto", RpcTransportCli::Auto),
            ("ttrpc", RpcTransportCli::Ttrpc),
            ("grpc", RpcTransportCli::Grpc),
        ] {
            let rpc = RpcCli::from_str(&format!("path=/tmp/s.sock,transport={s}")).unwrap();
            assert_eq!(rpc.path, Path::new("/tmp/s.sock"));
            assert_eq!(rpc.transport, transport);
        }

        // errors
        assert!(RpcCli::from_str("").is_err());
        assert!(RpcCli::from_str("transport=ttrpc").is_err());
        assert!(RpcCli::from_str("path=").is_err());
        assert!(RpcCli::from_str("path=/tmp/s.sock,transport=bogus").is_err());
        assert!(RpcCli::from_str("path=/tmp/s.sock,bogus=1").is_err());
        assert!(RpcCli::from_str("path=/a,path=/b").is_err());
    }

    #[test]
    fn test_parse_file_opts() {
        // file: prefix with create
        let disk = DiskCliKind::from_str("file:test.vhd;create=1G").unwrap();
        assert!(matches!(
            &disk,
            DiskCliKind::File { path, create_with_len: Some(len), direct: false }
                if path == Path::new("test.vhd") && *len == 1024 * 1024 * 1024
        ));

        // bare path with create (no file: prefix)
        let disk = DiskCliKind::from_str("test.vhd;create=1G").unwrap();
        assert!(matches!(
            &disk,
            DiskCliKind::File { path, create_with_len: Some(len), direct: false }
                if path == Path::new("test.vhd") && *len == 1024 * 1024 * 1024
        ));

        // direct flag
        let disk = DiskCliKind::from_str("file:/dev/sdb;direct").unwrap();
        assert!(matches!(
            &disk,
            DiskCliKind::File { path, create_with_len: None, direct: true }
                if path == Path::new("/dev/sdb")
        ));

        // direct + create in either order
        let disk = DiskCliKind::from_str("file:disk.img;direct;create=1G").unwrap();
        assert!(matches!(
            &disk,
            DiskCliKind::File { path, create_with_len: Some(len), direct: true }
                if path == Path::new("disk.img") && *len == 1024 * 1024 * 1024
        ));

        let disk = DiskCliKind::from_str("file:disk.img;create=1G;direct").unwrap();
        assert!(matches!(
            &disk,
            DiskCliKind::File { path, create_with_len: Some(len), direct: true }
                if path == Path::new("disk.img") && *len == 1024 * 1024 * 1024
        ));

        // plain path, no options
        let disk = DiskCliKind::from_str("file:disk.img").unwrap();
        assert!(matches!(
            &disk,
            DiskCliKind::File { path, create_with_len: None, direct: false }
                if path == Path::new("disk.img")
        ));

        // invalid option rejected
        assert!(DiskCliKind::from_str("file:disk.img;bogus").is_err());

        // direct rejected for sql disks
        assert!(DiskCliKind::from_str("sql:db.sqlite;direct").is_err());
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
                    ..
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
                        ..
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
                        ..
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
        // Test with cache path provided
        let disk =
            DiskCliKind::parse_autocache(":file:disk.vhd", Ok("/tmp/cache".to_string())).unwrap();
        assert!(matches!(
            disk,
            DiskCliKind::AutoCacheSqlite {
                cache_path,
                key,
                disk: _disk,
            } if cache_path == "/tmp/cache" && key.is_none()
        ));

        // Test with key
        let disk =
            DiskCliKind::parse_autocache("mykey:file:disk.vhd", Ok("/tmp/cache".to_string()))
                .unwrap();
        assert!(matches!(
            disk,
            DiskCliKind::AutoCacheSqlite {
                cache_path,
                key: Some(key),
                disk: _disk,
            } if cache_path == "/tmp/cache" && key == "mykey"
        ));

        // Test without cache path
        assert!(
            DiskCliKind::parse_autocache(":file:disk.vhd", Err(std::env::VarError::NotPresent),)
                .is_err()
        );
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
        assert!(
            DiskCliKind::parse_autocache("key:file:disk.vhd", Err(std::env::VarError::NotPresent),)
                .is_err()
        );

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
            EndpointConfigCli::Consomme {
                cidr: None,
                host_fwd,
            } => assert!(host_fwd.is_empty()),
            _ => panic!("Expected Consomme variant without cidr"),
        }

        // Test consomme with cidr
        match EndpointConfigCli::from_str("consomme:192.168.0.0/24").unwrap() {
            EndpointConfigCli::Consomme {
                cidr: Some(cidr),
                host_fwd,
            } => {
                assert_eq!(cidr, "192.168.0.0/24");
                assert!(host_fwd.is_empty());
            }
            _ => panic!("Expected Consomme variant with cidr"),
        }

        // Test consomme with hostfwd
        match EndpointConfigCli::from_str("consomme:hostfwd=udp:127.0.0.1:5000-:5000").unwrap() {
            EndpointConfigCli::Consomme { cidr, host_fwd } => {
                assert!(cidr.is_none());
                assert_eq!(host_fwd.len(), 1);
                assert_eq!(host_fwd[0].protocol, HostPortProtocolCli::Udp);
                assert_eq!(
                    host_fwd[0].host_address,
                    Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                );
                assert_eq!(host_fwd[0].host_port, 5000);
                assert_eq!(host_fwd[0].guest_port, 5000);
            }
            _ => panic!("Expected Consomme variant with hostfwd"),
        }

        // Test consomme with cidr and hostfwd
        match EndpointConfigCli::from_str("consomme:10.0.0.0/24,hostfwd=tcp::2222-:22").unwrap() {
            EndpointConfigCli::Consomme { cidr, host_fwd } => {
                assert_eq!(cidr.as_deref(), Some("10.0.0.0/24"));
                assert_eq!(host_fwd.len(), 1);
                assert_eq!(host_fwd[0].protocol, HostPortProtocolCli::Tcp);
                assert_eq!(host_fwd[0].host_port, 2222);
                assert_eq!(host_fwd[0].guest_port, 22);
            }
            _ => panic!("Expected Consomme variant with cidr and hostfwd"),
        }

        // Test consomme with multiple hostfwd
        match EndpointConfigCli::from_str("consomme:hostfwd=tcp::2222-:22,hostfwd=tcp::3389-:3389")
            .unwrap()
        {
            EndpointConfigCli::Consomme { cidr, host_fwd } => {
                assert!(cidr.is_none());
                assert_eq!(host_fwd.len(), 2);
                assert_eq!(host_fwd[0].host_port, 2222);
                assert_eq!(host_fwd[0].guest_port, 22);
                assert_eq!(host_fwd[1].host_port, 3389);
                assert_eq!(host_fwd[1].guest_port, 3389);
            }
            _ => panic!("Expected Consomme variant with multiple hostfwd"),
        }

        // Test consomme with different host and guest ports
        match EndpointConfigCli::from_str("consomme:hostfwd=tcp:127.0.0.1:8080-:80").unwrap() {
            EndpointConfigCli::Consomme { cidr, host_fwd } => {
                assert!(cidr.is_none());
                assert_eq!(host_fwd.len(), 1);
                assert_eq!(host_fwd[0].protocol, HostPortProtocolCli::Tcp);
                assert_eq!(
                    host_fwd[0].host_address,
                    Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                );
                assert_eq!(host_fwd[0].host_port, 8080);
                assert_eq!(host_fwd[0].guest_port, 80);
            }
            _ => panic!("Expected Consomme variant with host/guest port mapping"),
        }

        // Test consomme with guest address (accepted but ignored by backend)
        match EndpointConfigCli::from_str("consomme:hostfwd=tcp::8080-10.0.0.2:80").unwrap() {
            EndpointConfigCli::Consomme { cidr, host_fwd } => {
                assert!(cidr.is_none());
                assert_eq!(host_fwd[0].host_port, 8080);
                assert_eq!(host_fwd[0].guest_port, 80);
            }
            _ => panic!("Expected Consomme variant with guest address"),
        }

        // Test consomme with IPv6 host address (bracketed)
        match EndpointConfigCli::from_str("consomme:hostfwd=tcp:[::1]:8080-:80").unwrap() {
            EndpointConfigCli::Consomme { cidr, host_fwd } => {
                assert!(cidr.is_none());
                assert_eq!(host_fwd.len(), 1);
                assert_eq!(host_fwd[0].protocol, HostPortProtocolCli::Tcp);
                assert_eq!(
                    host_fwd[0].host_address,
                    Some(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST))
                );
                assert_eq!(host_fwd[0].host_port, 8080);
                assert_eq!(host_fwd[0].guest_port, 80);
            }
            _ => panic!("Expected Consomme variant with IPv6 hostfwd"),
        }

        // Test consomme with IPv6 guest address (bracketed)
        match EndpointConfigCli::from_str("consomme:hostfwd=tcp::8080-[::1]:80").unwrap() {
            EndpointConfigCli::Consomme { cidr, host_fwd } => {
                assert!(cidr.is_none());
                assert_eq!(host_fwd[0].host_port, 8080);
                assert_eq!(host_fwd[0].guest_port, 80);
            }
            _ => panic!("Expected Consomme variant with IPv6 guest address"),
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
        assert!(config.pcie_port.is_none());
        assert!(matches!(config.endpoint, EndpointConfigCli::None));

        // Test with vtl2
        let config = NicConfigCli::from_str("vtl2:none").unwrap();
        assert_eq!(config.vtl, DeviceVtl::Vtl2);
        assert!(config.pcie_port.is_none());
        assert!(matches!(config.endpoint, EndpointConfigCli::None));

        // Test with queues
        let config = NicConfigCli::from_str("queues=4:none").unwrap();
        assert_eq!(config.max_queues, Some(4));
        assert!(config.pcie_port.is_none());
        assert!(matches!(config.endpoint, EndpointConfigCli::None));

        // Test with underhill
        let config = NicConfigCli::from_str("uh:none").unwrap();
        assert!(config.underhill);
        assert!(config.pcie_port.is_none());
        assert!(matches!(config.endpoint, EndpointConfigCli::None));

        // Test with pcie_port
        let config = NicConfigCli::from_str("pcie_port=rp0:none").unwrap();
        assert_eq!(config.pcie_port.unwrap(), "rp0".to_string());
        assert!(matches!(config.endpoint, EndpointConfigCli::None));

        // Test error cases
        assert!(NicConfigCli::from_str("queues=invalid:none").is_err());
        assert!(NicConfigCli::from_str("uh:vtl2:none").is_err()); // uh incompatible with vtl2
        assert!(NicConfigCli::from_str("pcie_port=rp0:vtl2:none").is_err());
        assert!(NicConfigCli::from_str("uh:pcie_port=rp0:none").is_err());
        assert!(NicConfigCli::from_str("pcie_port=:none").is_err());
        assert!(NicConfigCli::from_str("pcie_port:none").is_err());
    }

    #[test]
    fn test_parse_pcie_port_prefix() {
        // Successful prefix parsing
        let (port, rest) = parse_pcie_port_prefix("pcie_port=rp0:tag,path");
        assert_eq!(port.unwrap(), "rp0");
        assert_eq!(rest, "tag,path");

        // No prefix
        let (port, rest) = parse_pcie_port_prefix("tag,path");
        assert!(port.is_none());
        assert_eq!(rest, "tag,path");

        // Empty port name — not parsed as a prefix
        let (port, rest) = parse_pcie_port_prefix("pcie_port=:tag,path");
        assert!(port.is_none());
        assert_eq!(rest, "pcie_port=:tag,path");

        // Missing colon — not parsed as a prefix
        let (port, rest) = parse_pcie_port_prefix("pcie_port=rp0");
        assert!(port.is_none());
        assert_eq!(rest, "pcie_port=rp0");
    }

    #[test]
    fn test_cxl_test_device_cli_parse_valid() {
        let cfg = CxlTestDeviceCli::from_str("mem:1G,pcie_port=rp0").unwrap();
        assert_eq!(cfg.hdm_size, 1024 * 1024 * 1024);
        assert_eq!(cfg.pcie_port, "rp0");
    }

    #[test]
    fn test_cxl_test_device_cli_parse_invalid() {
        assert!(CxlTestDeviceCli::from_str("file:disk.img,pcie_port=rp0").is_err());
        assert!(CxlTestDeviceCli::from_str("mem:1G").is_err());
        assert!(CxlTestDeviceCli::from_str("mem:1G,pcie_port=").is_err());
    }

    #[test]
    fn test_fs_args_pcie_port() {
        // Without pcie_port
        let args = FsArgs::from_str("myfs,/path").unwrap();
        assert_eq!(args.tag, "myfs");
        assert_eq!(args.path, "/path");
        assert!(args.pcie_port.is_none());

        // With pcie_port
        let args = FsArgs::from_str("pcie_port=rp0:myfs,/path").unwrap();
        assert_eq!(args.pcie_port.unwrap(), "rp0");
        assert_eq!(args.tag, "myfs");
        assert_eq!(args.path, "/path");

        // Error: wrong number of fields
        assert!(FsArgs::from_str("myfs").is_err());
        assert!(FsArgs::from_str("pcie_port=rp0:myfs").is_err());
    }

    #[test]
    fn test_fs_args_with_options_pcie_port() {
        // Without pcie_port
        let args = FsArgsWithOptions::from_str("myfs,/path,uid=1000").unwrap();
        assert_eq!(args.tag, "myfs");
        assert_eq!(args.path, "/path");
        assert_eq!(args.options, "uid=1000");
        assert!(args.pcie_port.is_none());

        // With pcie_port
        let args = FsArgsWithOptions::from_str("pcie_port=rp0:myfs,/path,uid=1000").unwrap();
        assert_eq!(args.pcie_port.unwrap(), "rp0");
        assert_eq!(args.tag, "myfs");
        assert_eq!(args.path, "/path");
        assert_eq!(args.options, "uid=1000");

        // Error: missing path
        assert!(FsArgsWithOptions::from_str("myfs").is_err());
    }

    #[test]
    fn test_virtio_pmem_args_pcie_port() {
        // Without pcie_port
        let args = VirtioPmemArgs::from_str("/path/to/file").unwrap();
        assert_eq!(args.path, "/path/to/file");
        assert!(args.pcie_port.is_none());

        // With pcie_port
        let args = VirtioPmemArgs::from_str("pcie_port=rp0:/path/to/file").unwrap();
        assert_eq!(args.pcie_port.unwrap(), "rp0");
        assert_eq!(args.path, "/path/to/file");

        // Error: empty path
        assert!(VirtioPmemArgs::from_str("").is_err());
        assert!(VirtioPmemArgs::from_str("pcie_port=rp0:").is_err());
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
                ..
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

        const DEFAULT_LOW_MMIO: u32 = (64 * ONE_MB) as u32;
        const DEFAULT_HIGH_MMIO: u64 = ONE_GB;
        const DEFAULT_HDM: u64 = ONE_GB;
        const DEFAULT_HDM_WINDOW_RESTRICTIONS: CfmwsWindowRestrictions =
            CfmwsWindowRestrictions::DEVICE_COHERENT;

        assert_eq!(
            PcieRootComplexCli::from_str("rc0").unwrap(),
            PcieRootComplexCli {
                name: "rc0".to_string(),
                segment: 0,
                start_bus: 0,
                end_bus: 255,
                low_mmio: DEFAULT_LOW_MMIO,
                high_mmio: DEFAULT_HIGH_MMIO,
                hdm: DEFAULT_HDM,
                hdm_window_restrictions: DEFAULT_HDM_WINDOW_RESTRICTIONS,
                vnode: None,
                low_mmio_base: None,
                high_mmio_base: None,
                preserve_bars: false,
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
                hdm: DEFAULT_HDM,
                hdm_window_restrictions: DEFAULT_HDM_WINDOW_RESTRICTIONS,
                vnode: None,
                low_mmio_base: None,
                high_mmio_base: None,
                preserve_bars: false,
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
                hdm: DEFAULT_HDM,
                hdm_window_restrictions: DEFAULT_HDM_WINDOW_RESTRICTIONS,
                vnode: None,
                low_mmio_base: None,
                high_mmio_base: None,
                preserve_bars: false,
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
                hdm: DEFAULT_HDM,
                hdm_window_restrictions: DEFAULT_HDM_WINDOW_RESTRICTIONS,
                vnode: None,
                low_mmio_base: None,
                high_mmio_base: None,
                preserve_bars: false,
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
                hdm: DEFAULT_HDM,
                hdm_window_restrictions: DEFAULT_HDM_WINDOW_RESTRICTIONS,
                vnode: None,
                low_mmio_base: None,
                high_mmio_base: None,
                preserve_bars: false,
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
                hdm: DEFAULT_HDM,
                hdm_window_restrictions: DEFAULT_HDM_WINDOW_RESTRICTIONS,
                vnode: None,
                low_mmio_base: None,
                high_mmio_base: None,
                preserve_bars: false,
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
                hdm: DEFAULT_HDM,
                hdm_window_restrictions: DEFAULT_HDM_WINDOW_RESTRICTIONS,
                vnode: None,
                low_mmio_base: None,
                high_mmio_base: None,
                preserve_bars: false,
            }
        );

        assert_eq!(
            PcieRootComplexCli::from_str("rc7,hdm=2G").unwrap(),
            PcieRootComplexCli {
                name: "rc7".to_string(),
                segment: 0,
                start_bus: 0,
                end_bus: 255,
                low_mmio: DEFAULT_LOW_MMIO,
                high_mmio: DEFAULT_HIGH_MMIO,
                hdm: 2 * ONE_GB,
                hdm_window_restrictions: DEFAULT_HDM_WINDOW_RESTRICTIONS,
                vnode: None,
                low_mmio_base: None,
                high_mmio_base: None,
                preserve_bars: false,
            }
        );

        assert_eq!(
            PcieRootComplexCli::from_str("rc8,hdm_window_restrictions=0x21").unwrap(),
            PcieRootComplexCli {
                name: "rc8".to_string(),
                segment: 0,
                start_bus: 0,
                end_bus: 255,
                low_mmio: DEFAULT_LOW_MMIO,
                high_mmio: DEFAULT_HIGH_MMIO,
                hdm: DEFAULT_HDM,
                hdm_window_restrictions: CfmwsWindowRestrictions::try_from_bits(0x21).unwrap(),
                vnode: None,
                low_mmio_base: None,
                high_mmio_base: None,
                preserve_bars: false,
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
        assert!(PcieRootComplexCli::from_str("rc,hdm=bad").is_err());
        assert!(PcieRootComplexCli::from_str("rc,hdm").is_err());
        assert!(PcieRootComplexCli::from_str("rc,hdm_window_restrictions=bad").is_err());
        assert!(PcieRootComplexCli::from_str("rc,hdm_window_restrictions").is_err());
        assert!(PcieRootComplexCli::from_str("rc,cxl").is_err());

        // node option
        assert_eq!(
            PcieRootComplexCli::from_str("rc9,node=1").unwrap(),
            PcieRootComplexCli {
                name: "rc9".to_string(),
                segment: 0,
                start_bus: 0,
                end_bus: 255,
                low_mmio: DEFAULT_LOW_MMIO,
                high_mmio: DEFAULT_HIGH_MMIO,
                hdm: DEFAULT_HDM,
                hdm_window_restrictions: DEFAULT_HDM_WINDOW_RESTRICTIONS,
                vnode: Some(1),
                low_mmio_base: None,
                high_mmio_base: None,
                preserve_bars: false,
            }
        );
    }

    #[test]
    fn test_pcie_root_port_from_str() {
        assert_eq!(
            PcieRootPortCli::from_str("rc0:rc0rp0").unwrap(),
            PcieRootPortCli {
                root_complex_name: "rc0".to_string(),
                name: "rc0rp0".to_string(),
                devfn: None,
                hotplug: false,
                acs_capabilities_supported: None,
                cxl: false,
            }
        );

        assert_eq!(
            PcieRootPortCli::from_str("my_rc:port2").unwrap(),
            PcieRootPortCli {
                root_complex_name: "my_rc".to_string(),
                name: "port2".to_string(),
                devfn: None,
                hotplug: false,
                acs_capabilities_supported: None,
                cxl: false,
            }
        );

        // Test with hotplug flag
        assert_eq!(
            PcieRootPortCli::from_str("my_rc:port2,hotplug").unwrap(),
            PcieRootPortCli {
                root_complex_name: "my_rc".to_string(),
                name: "port2".to_string(),
                devfn: None,
                hotplug: true,
                acs_capabilities_supported: None,
                cxl: false,
            }
        );

        assert_eq!(
            PcieRootPortCli::from_str("my_rc:port3,acs=0").unwrap(),
            PcieRootPortCli {
                root_complex_name: "my_rc".to_string(),
                name: "port3".to_string(),
                devfn: None,
                hotplug: false,
                acs_capabilities_supported: Some(0),
                cxl: false,
            }
        );

        assert_eq!(
            PcieRootPortCli::from_str("my_rc:port3,acs=0x5f").unwrap(),
            PcieRootPortCli {
                root_complex_name: "my_rc".to_string(),
                name: "port3".to_string(),
                devfn: None,
                hotplug: false,
                acs_capabilities_supported: Some(0x005f),
                cxl: false,
            }
        );

        assert_eq!(
            PcieRootPortCli::from_str("my_rc:port4,cxl").unwrap(),
            PcieRootPortCli {
                root_complex_name: "my_rc".to_string(),
                name: "port4".to_string(),
                devfn: None,
                hotplug: false,
                acs_capabilities_supported: None,
                cxl: true,
            }
        );

        // Test addr= (device only, and device.function)
        assert_eq!(
            PcieRootPortCli::from_str("my_rc:port5,addr=5").unwrap(),
            PcieRootPortCli {
                root_complex_name: "my_rc".to_string(),
                name: "port5".to_string(),
                devfn: Some(5 << 3),
                hotplug: false,
                acs_capabilities_supported: None,
                cxl: false,
            }
        );
        assert_eq!(
            PcieRootPortCli::from_str("my_rc:port6,addr=5.1").unwrap(),
            PcieRootPortCli {
                root_complex_name: "my_rc".to_string(),
                name: "port6".to_string(),
                devfn: Some((5 << 3) | 1),
                hotplug: false,
                acs_capabilities_supported: None,
                cxl: false,
            }
        );
        assert_eq!(
            PcieRootPortCli::from_str("my_rc:port7,addr=0x1f.7").unwrap(),
            PcieRootPortCli {
                root_complex_name: "my_rc".to_string(),
                name: "port7".to_string(),
                devfn: Some(0xff),
                hotplug: false,
                acs_capabilities_supported: None,
                cxl: false,
            }
        );

        // Error cases
        assert!(PcieRootPortCli::from_str("").is_err());
        assert!(PcieRootPortCli::from_str("rp0").is_err());
        assert!(PcieRootPortCli::from_str("rp0,opt").is_err());
        assert!(PcieRootPortCli::from_str("rc0:rp0:rp3").is_err());
        assert!(PcieRootPortCli::from_str("rc0:rp0,invalid_option").is_err());
        assert!(PcieRootPortCli::from_str("rc0:rp0,cxl=true").is_err());
        assert!(PcieRootPortCli::from_str("rc0:rp0,addr=32").is_err());
        assert!(PcieRootPortCli::from_str("rc0:rp0,addr=0.8").is_err());
        assert!(PcieRootPortCli::from_str("rc0:rp0,addr=1.2.3").is_err());
        assert!(PcieRootPortCli::from_str("rc0:rp0,addr").is_err());
    }

    #[test]
    fn test_pcie_generic_initiator_from_str() {
        assert_eq!(
            PcieGenericInitiatorCli::from_str("port=rp0,node=1").unwrap(),
            PcieGenericInitiatorCli {
                port_name: "rp0".to_string(),
                node: 1,
            }
        );

        // Order should not matter.
        assert_eq!(
            PcieGenericInitiatorCli::from_str("node=2,port=sw0-downstream-1").unwrap(),
            PcieGenericInitiatorCli {
                port_name: "sw0-downstream-1".to_string(),
                node: 2,
            }
        );

        // Error cases
        assert!(PcieGenericInitiatorCli::from_str("").is_err());
        assert!(PcieGenericInitiatorCli::from_str("port=rp0").is_err());
        assert!(PcieGenericInitiatorCli::from_str("node=1").is_err());
        assert!(PcieGenericInitiatorCli::from_str("rp0=1").is_err());
        assert!(PcieGenericInitiatorCli::from_str("port=,node=1").is_err());
        assert!(PcieGenericInitiatorCli::from_str("port=rp0,node=x").is_err());
        assert!(PcieGenericInitiatorCli::from_str("port=rp0,node=1,extra").is_err());
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
                acs_capabilities_supported: None,
            }
        );

        assert_eq!(
            GenericPcieSwitchCli::from_str("port1:my_switch,num_downstream_ports=4").unwrap(),
            GenericPcieSwitchCli {
                port_name: "port1".to_string(),
                name: "my_switch".to_string(),
                num_downstream_ports: 4,
                hotplug: false,
                acs_capabilities_supported: None,
            }
        );

        assert_eq!(
            GenericPcieSwitchCli::from_str("rp2:sw,num_downstream_ports=8").unwrap(),
            GenericPcieSwitchCli {
                port_name: "rp2".to_string(),
                name: "sw".to_string(),
                num_downstream_ports: 8,
                hotplug: false,
                acs_capabilities_supported: None,
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
                acs_capabilities_supported: None,
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
                acs_capabilities_supported: None,
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
                acs_capabilities_supported: None,
            }
        );

        assert_eq!(
            GenericPcieSwitchCli::from_str("rp0:switch0,acs=0").unwrap(),
            GenericPcieSwitchCli {
                port_name: "rp0".to_string(),
                name: "switch0".to_string(),
                num_downstream_ports: 4,
                hotplug: false,
                acs_capabilities_supported: Some(0),
            }
        );

        assert_eq!(
            GenericPcieSwitchCli::from_str("rp0:switch0,acs=95").unwrap(),
            GenericPcieSwitchCli {
                port_name: "rp0".to_string(),
                name: "switch0".to_string(),
                num_downstream_ports: 4,
                hotplug: false,
                acs_capabilities_supported: Some(95),
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
                socket_addr: None,
                hu: 0,
                controller: 0,
            }
        );

        // With socket address
        assert_eq!(
            PcieRemoteCli::from_str("rc0rp0,socket=localhost:22567").unwrap(),
            PcieRemoteCli {
                port_name: "rc0rp0".to_string(),
                socket_addr: Some("localhost:22567".to_string()),
                hu: 0,
                controller: 0,
            }
        );

        // With all options
        assert_eq!(
            PcieRemoteCli::from_str("myport,socket=localhost:22568,hu=1,controller=2").unwrap(),
            PcieRemoteCli {
                port_name: "myport".to_string(),
                socket_addr: Some("localhost:22568".to_string()),
                hu: 1,
                controller: 2,
            }
        );

        // Only hu and controller
        assert_eq!(
            PcieRemoteCli::from_str("port0,hu=5,controller=3").unwrap(),
            PcieRemoteCli {
                port_name: "port0".to_string(),
                socket_addr: None,
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

    #[test]
    fn test_parse_memory_units() {
        assert_eq!(parse_memory("64G").unwrap(), 64 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory("64GB").unwrap(), 64 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory("3MB").unwrap(), 3 * 1024 * 1024);
        assert_eq!(parse_memory("512KB").unwrap(), 512 * 1024);
        assert!(parse_memory("3MiB").is_err());
    }

    #[test]
    fn test_memory_config_size_only() {
        assert_eq!(
            parse_memory_config("64G").unwrap(),
            MemoryCli {
                size: Some(vmm_cli::MemorySize(64 * 1024 * 1024 * 1024)),
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_memory_config_key_value() {
        assert_eq!(
            parse_memory_config("size=2G,shared=off,prefetch=on,thp=on").unwrap(),
            MemoryCli {
                size: Some(vmm_cli::MemorySize(2 * 1024 * 1024 * 1024)),
                shared: Some(false),
                prefetch: true,
                transparent_hugepages: Some(true),
                ..Default::default()
            }
        );

        assert_eq!(
            parse_memory_config("size=4GB,hugepages=on,hugepage_size=2MB").unwrap(),
            MemoryCli {
                size: Some(vmm_cli::MemorySize(4 * 1024 * 1024 * 1024)),
                hugepages: true,
                hugepage_size: Some(vmm_cli::MemorySize(2 * 1024 * 1024)),
                ..Default::default()
            }
        );

        assert_eq!(
            parse_memory_config("file=/tmp/memory.bin").unwrap(),
            MemoryCli {
                file: Some(PathBuf::from("/tmp/memory.bin")),
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_memory_config_rejects_invalid_combinations() {
        assert!(parse_memory_config("size=1G,size=2G").is_err());
        assert!(parse_memory_config("hugepage_size=2M").is_err());
        assert!(parse_memory_config("hugepages=on,shared=off").is_err());
        assert!(parse_memory_config("hugepages=on,file=/tmp/memory.bin").is_err());

        // Semantic validation of the hugepage size happens in the memory
        // builder, not in CLI parsing.
        assert_eq!(
            parse_memory_config("hugepages=on,hugepage_size=3MB")
                .unwrap()
                .hugepage_size,
            Some(vmm_cli::MemorySize(3 * 1024 * 1024))
        );
    }

    #[test]
    fn test_memory_options_merge_legacy_aliases() {
        let opt = Options::try_parse_from([
            "openvmm",
            "--memory",
            "2G",
            "--prefetch",
            "--private-memory",
            "--thp",
        ])
        .unwrap();
        opt.validate_memory_options().unwrap();
        assert_eq!(opt.memory_size(), 2 * 1024 * 1024 * 1024);
        assert!(opt.prefetch_memory());
        assert!(opt.private_memory());
        assert!(opt.transparent_hugepages());
    }

    #[test]
    fn test_serial_debugger_mode_option_parsed() {
        // No COM port configured: no debugger mode.
        let opt = Options::try_parse_from(["openvmm"]).unwrap();
        assert!(opt.com1.is_none());

        // A plain backend is not in debugger mode.
        let opt = Options::try_parse_from(["openvmm", "--com1", "none"]).unwrap();
        let com1 = opt.com1.unwrap();
        assert!(!com1.debugger_mode);
        assert_eq!(com1.backend, SerialConfigCli::None);

        // The `debugger-mode:` prefix enables debugger mode for just that port,
        // and the remainder still parses as the backend.
        let opt = Options::try_parse_from([
            "openvmm",
            "--com1",
            "debugger-mode:listen=/tmp/kd",
            "--com2",
            "none",
        ])
        .unwrap();
        let com1 = opt.com1.unwrap();
        assert!(com1.debugger_mode);
        assert_eq!(com1.backend, SerialConfigCli::Pipe("/tmp/kd".into()));
        // Other ports remain independent (not in debugger mode).
        assert!(!opt.com2.unwrap().debugger_mode);

        // The prefix must not eat colons in the backend (e.g. a tcp address).
        let opt = Options::try_parse_from([
            "openvmm",
            "--com1",
            "debugger-mode:listen=tcp:127.0.0.1:5555",
        ])
        .unwrap();
        let com1 = opt.com1.unwrap();
        assert!(com1.debugger_mode);
        assert_eq!(
            com1.backend,
            SerialConfigCli::Tcp("127.0.0.1:5555".parse().unwrap())
        );
    }

    #[test]
    fn test_memory_options_allow_legacy_thp_with_new_private_memory() {
        let opt = Options::try_parse_from(["openvmm", "--memory", "shared=off", "--thp"]).unwrap();
        opt.validate_memory_options().unwrap();
        assert!(opt.private_memory());
        assert!(opt.transparent_hugepages());
    }

    #[test]
    fn test_memory_options_reject_conflicting_legacy_aliases() {
        let opt = Options::try_parse_from(["openvmm", "--memory", "shared=on", "--private-memory"])
            .unwrap();
        assert!(opt.validate_memory_options().is_err());
    }

    #[test]
    fn test_pidfile_option_parsed() {
        let opt = Options::try_parse_from(["openvmm", "--pidfile", "/tmp/test.pid"]).unwrap();
        assert_eq!(opt.pidfile, Some(PathBuf::from("/tmp/test.pid")));
    }

    #[test]
    fn test_guest_power_action_flags() {
        // Defaults preserve the historical behavior: reset and watchdog reboot,
        // power-off and crash keep the stopped VM.
        let opt = Options::try_parse_from(["openvmm"]).unwrap();
        assert_eq!(opt.guest_reset_action, GuestPowerAction::Reset);
        assert_eq!(opt.guest_shutdown_action, GuestPowerAction::Halt);
        assert_eq!(opt.guest_crash_action, GuestPowerAction::Halt);
        assert_eq!(opt.guest_watchdog_action, GuestPowerAction::Reset);
        // The CLI defaults must match the shared GuestPowerActions::default() the
        // ttrpc server uses, so the two launch paths never drift.
        assert_eq!(
            crate::vm_controller::GuestPowerActions {
                shutdown: opt.guest_shutdown_action,
                reset: opt.guest_reset_action,
                crash: opt.guest_crash_action,
                watchdog: opt.guest_watchdog_action,
            },
            crate::vm_controller::GuestPowerActions::default(),
        );

        let opt = Options::try_parse_from([
            "openvmm",
            "--guest-watchdog",
            "--guest-reset-action",
            "exit",
            "--guest-shutdown-action",
            "exit:5",
            "--guest-crash-action",
            "reset",
            "--guest-watchdog-action",
            "halt",
        ])
        .unwrap();
        // A bare `exit` is status 0; `exit:5` carries the code through.
        assert_eq!(opt.guest_reset_action, GuestPowerAction::Exit(0));
        assert_eq!(opt.guest_shutdown_action, GuestPowerAction::Exit(5));
        assert_eq!(opt.guest_crash_action, GuestPowerAction::Reset);
        assert_eq!(opt.guest_watchdog_action, GuestPowerAction::Halt);

        // Malformed and out-of-range exit codes are rejected (status is 0-255).
        assert!(Options::try_parse_from(["openvmm", "--guest-reset-action", "exit:nope"]).is_err());
        assert!(Options::try_parse_from(["openvmm", "--guest-reset-action", "exit:300"]).is_err());
        assert!(Options::try_parse_from(["openvmm", "--guest-reset-action", "exit:-1"]).is_err());

        // --guest-watchdog-action requires the watchdog device (--guest-watchdog).
        assert!(Options::try_parse_from(["openvmm", "--guest-watchdog-action", "halt"]).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_vfio_device_cli_parse() {
        // Required keys only.
        let v = VfioDeviceCli::from_str("host=0000:01:00.0,port=rp0").unwrap();
        assert_eq!(v.pci_id, "0000:01:00.0");
        assert_eq!(v.port_name, "rp0");
        assert_eq!(v.iommu, None);

        // With optional iommu= key. Keys may appear in any order.
        let v = VfioDeviceCli::from_str("port=rp1,iommu=iommu0,host=0000:02:00.0").unwrap();
        assert_eq!(v.pci_id, "0000:02:00.0");
        assert_eq!(v.port_name, "rp1");
        assert_eq!(v.iommu.as_deref(), Some("iommu0"));

        // BAR passthrough flags set the corresponding indices.
        let v = VfioDeviceCli::from_str("host=0000:01:00.0,port=rp0,bar0=pt,bar2=pt").unwrap();
        assert_eq!(v.bar_pt, [true, false, true, false, false, false]);

        // A BAR flag only accepts `pt`, and requires a value.
        assert!(VfioDeviceCli::from_str("host=0000:01:00.0,port=rp0,bar0=x").is_err());
        assert!(VfioDeviceCli::from_str("host=0000:01:00.0,port=rp0,bar0").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_vfio_device_cli_errors() {
        // Missing required keys.
        assert!(VfioDeviceCli::from_str("port=rp0").is_err());
        assert!(VfioDeviceCli::from_str("host=0000:01:00.0").is_err());

        // Unknown key.
        assert!(VfioDeviceCli::from_str("host=0000:01:00.0,port=rp0,foo=bar").is_err());

        // Duplicate keys are rejected.
        assert!(VfioDeviceCli::from_str("host=0000:01:00.0,host=0000:02:00.0,port=rp0").is_err());
        assert!(VfioDeviceCli::from_str("host=0000:01:00.0,port=rp0,port=rp1").is_err());
        assert!(VfioDeviceCli::from_str("host=0000:01:00.0,port=rp0,iommu=a,iommu=b").is_err());

        // Empty values are rejected.
        assert!(VfioDeviceCli::from_str("host=,port=rp0").is_err());
        assert!(VfioDeviceCli::from_str("host=0000:01:00.0,port=").is_err());
        assert!(VfioDeviceCli::from_str("host=0000:01:00.0,port=rp0,iommu=").is_err());

        // Missing '=' separator.
        assert!(VfioDeviceCli::from_str("host").is_err());
        assert!(VfioDeviceCli::from_str("host=0000:01:00.0,port=rp0,iommu").is_err());

        // Path-traversal characters in the host BDF are rejected.
        assert!(VfioDeviceCli::from_str("host=../../etc/passwd,port=rp0").is_err());
        assert!(VfioDeviceCli::from_str("host=foo/bar,port=rp0").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_iommu_cli_parse() {
        let c = IommuCli::from_str("id=iommu0").unwrap();
        assert_eq!(c.id, "iommu0");

        // Wrong key.
        assert!(IommuCli::from_str("name=iommu0").is_err());

        // Missing '=' separator.
        assert!(IommuCli::from_str("iommu0").is_err());

        // Empty id.
        assert!(IommuCli::from_str("id=").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_vhost_user_cli() {
        // type=blk: socket is positional; num_queues/queue_size optional.
        let v = VhostUserCli::from_str("/run/blk.sock,type=blk,num_queues=2").unwrap();
        assert_eq!(v.socket_path, "/run/blk.sock");
        assert!(matches!(
            v.device_type,
            VhostUserDeviceTypeCli::Blk {
                num_queues: Some(2),
                queue_size: None
            }
        ));
        assert_eq!(v.pcie_port, None);

        // type=fs requires a tag.
        let v = VhostUserCli::from_str("/run/fs.sock,type=fs,tag=myfs,pcie_port=p0").unwrap();
        assert!(matches!(
            &v.device_type,
            VhostUserDeviceTypeCli::Fs { tag, .. } if tag == "myfs"
        ));
        assert_eq!(v.pcie_port.as_deref(), Some("p0"));

        // device_id with a bracketed queue_sizes list.
        let v = VhostUserCli::from_str("/run/x.sock,device_id=9,queue_sizes=[16,32]").unwrap();
        assert!(matches!(
            &v.device_type,
            VhostUserDeviceTypeCli::Other { device_id: 9, queue_sizes } if *queue_sizes == vec![16, 32]
        ));

        // Errors.
        assert!(VhostUserCli::from_str("/run/x.sock").is_err()); // neither type nor device_id
        assert!(VhostUserCli::from_str("/run/x.sock,type=blk,device_id=1").is_err()); // both
        assert!(VhostUserCli::from_str("/run/x.sock,type=fs").is_err()); // fs without tag
        assert!(VhostUserCli::from_str("/run/x.sock,type=zzz").is_err()); // unknown type
        assert!(VhostUserCli::from_str("/run/x.sock,type=blk,tag=t").is_err()); // tag on non-fs
        assert!(VhostUserCli::from_str("/run/x.sock,type=blk,queue_sizes=[1]").is_err()); // queue_sizes on non-device_id
        assert!(VhostUserCli::from_str("/run/x.sock,device_id=1,num_queues=2").is_err()); // num_queues on device_id
        assert!(VhostUserCli::from_str("/run/x.sock,device_id=1,queue_sizes=[]").is_err()); // empty list
        assert!(VhostUserCli::from_str("/run/x.sock,device_id=1").is_err()); // device_id without queue_sizes
    }

    #[test]
    fn test_nvme_controller_cli_pcie() {
        let c = NvmeControllerCli::from_str("id=nvme0,pcie_port=p0").unwrap();
        assert_eq!(c.id, "nvme0");
        assert_eq!(c.transport, NvmeControllerTransport::Pcie("p0".into()));
    }

    #[test]
    fn test_nvme_controller_cli_vpci_no_guid() {
        let c = NvmeControllerCli::from_str("id=nvme1,vpci").unwrap();
        assert_eq!(c.id, "nvme1");
        assert!(matches!(c.transport, NvmeControllerTransport::Vpci(None)));
        assert!(NvmeControllerCli::from_str("id=nvme1,vpci=").is_err());
    }

    #[test]
    fn test_nvme_controller_cli_vpci_with_guid() {
        let c = NvmeControllerCli::from_str("id=nvme2,vpci=008091f6-9688-497d-9091-af347dc9173c")
            .unwrap();
        assert_eq!(c.id, "nvme2");
        assert!(matches!(
            c.transport,
            NvmeControllerTransport::Vpci(Some(_))
        ));
    }

    #[test]
    fn test_nvme_controller_cli_errors() {
        // Missing id.
        assert!(NvmeControllerCli::from_str("pcie_port=p0").is_err());
        // Missing transport.
        assert!(NvmeControllerCli::from_str("id=nvme0").is_err());
        // Both transports.
        assert!(NvmeControllerCli::from_str("id=nvme0,pcie_port=p0,vpci").is_err());
        // Unknown option.
        assert!(NvmeControllerCli::from_str("id=nvme0,pcie_port=p0,foo=bar").is_err());
        // Empty id.
        assert!(NvmeControllerCli::from_str("id=,pcie_port=p0").is_err());
        // Empty pcie_port.
        assert!(NvmeControllerCli::from_str("id=nvme0,pcie_port=").is_err());
        // Invalid GUID.
        let err = NvmeControllerCli::from_str("id=nvme0,vpci=not-a-guid").unwrap_err();
        assert!(err.to_string().contains("invalid value for option 'vpci'"));
    }

    #[test]
    fn test_disk_cli_controller() {
        let d = DiskCli::from_str("file:disk.vhd,on=nvme0").unwrap();
        assert_eq!(d.controller.as_deref(), Some("nvme0"));
        assert_eq!(d.nsid, None);
    }

    #[test]
    fn test_disk_cli_controller_with_nsid() {
        let d = DiskCli::from_str("file:disk.vhd,on=nvme0,nsid=3").unwrap();
        assert_eq!(d.controller.as_deref(), Some("nvme0"));
        assert_eq!(d.nsid, Some(3));
    }

    #[test]
    fn test_disk_cli_controller_errors() {
        // nsid without on.
        assert!(DiskCli::from_str("file:disk.vhd,nsid=1").is_err());
        // lun without on.
        assert!(DiskCli::from_str("file:disk.vhd,lun=0").is_err());
        // on with pcie_port.
        assert!(DiskCli::from_str("file:disk.vhd,on=nvme0,pcie_port=p0").is_err());
        // Empty controller name.
        assert!(DiskCli::from_str("file:disk.vhd,on=").is_err());
        // Invalid nsid.
        assert!(DiskCli::from_str("file:disk.vhd,on=nvme0,nsid=abc").is_err());
        // nsid and lun together.
        assert!(DiskCli::from_str("file:disk.vhd,on=c,nsid=1,lun=0").is_err());
    }

    #[test]
    fn test_disk_cli_controller_with_lun() {
        let d = DiskCli::from_str("file:disk.vhd,on=scsi0,lun=3").unwrap();
        assert_eq!(d.controller.as_deref(), Some("scsi0"));
        assert_eq!(d.lun, Some(3));
        assert_eq!(d.nsid, None);
    }

    #[test]
    fn test_scsi_controller_cli() {
        let c = ScsiControllerCli::from_str("id=scsi0").unwrap();
        assert_eq!(c.id, "scsi0");
        assert_eq!(c.sub_channels, 0);
    }

    #[test]
    fn test_scsi_controller_cli_with_sub_channels() {
        let c = ScsiControllerCli::from_str("id=scsi1,sub_channels=4").unwrap();
        assert_eq!(c.id, "scsi1");
        assert_eq!(c.sub_channels, 4);
    }

    #[test]
    fn test_scsi_controller_cli_errors() {
        // Missing id.
        assert!(ScsiControllerCli::from_str("sub_channels=4").is_err());
        // Empty id.
        assert!(ScsiControllerCli::from_str("id=").is_err());
        // Unknown option.
        assert!(ScsiControllerCli::from_str("id=scsi0,foo=bar").is_err());
        // Invalid sub_channels.
        assert!(ScsiControllerCli::from_str("id=scsi0,sub_channels=abc").is_err());
    }

    #[test]
    fn test_disk_cli_relay() {
        let d = DiskCli::from_str("file:disk.vhd,on=src,relay=tgt").unwrap();
        assert_eq!(d.relay.as_ref().unwrap().0, "tgt");
        assert_eq!(d.relay.as_ref().unwrap().1, None);
    }

    #[test]
    fn test_disk_cli_relay_with_location() {
        let d = DiskCli::from_str("file:disk.vhd,on=src,relay=tgt:3").unwrap();
        assert_eq!(d.relay.as_ref().unwrap().0, "tgt");
        assert_eq!(d.relay.as_ref().unwrap().1, Some(3));
    }

    #[test]
    fn test_disk_cli_relay_errors() {
        // relay without on.
        assert!(DiskCli::from_str("file:disk.vhd,relay=tgt").is_err());
        // relay with uh.
        assert!(DiskCli::from_str("file:disk.vhd,on=src,relay=tgt,uh").is_err());
        // relay with invalid location.
        assert!(DiskCli::from_str("file:disk.vhd,on=src,relay=tgt:abc").is_err());
        // empty relay.
        assert!(DiskCli::from_str("file:disk.vhd,on=src,relay=").is_err());
    }

    #[test]
    fn test_nvme_controller_cli_vtl2() {
        let c = NvmeControllerCli::from_str("id=nvme0,vpci,vtl2").unwrap();
        assert_eq!(c.vtl, DeviceVtl::Vtl2);
    }

    #[test]
    fn test_scsi_controller_cli_vtl2() {
        let c = ScsiControllerCli::from_str("id=scsi0,vtl2").unwrap();
        assert_eq!(c.vtl, DeviceVtl::Vtl2);
    }

    #[test]
    fn test_openhcl_controller_cli() {
        let c = OpenhclControllerCli::from_str("id=vtl0-scsi,type=scsi").unwrap();
        assert_eq!(c.id, "vtl0-scsi");
        assert_eq!(c.controller_type, OpenhclControllerType::Scsi);
        assert_eq!(c.guid, None);
    }

    #[test]
    fn test_openhcl_controller_cli_nvme_with_guid() {
        let c = OpenhclControllerCli::from_str(
            "id=vtl0-nvme,type=nvme,guid=09a59b81-2bf6-4164-81d7-3a0dc977ba65",
        )
        .unwrap();
        assert_eq!(c.controller_type, OpenhclControllerType::Nvme);
        assert!(c.guid.is_some());
    }

    #[test]
    fn test_openhcl_controller_cli_errors() {
        // Missing id.
        assert!(OpenhclControllerCli::from_str("type=scsi").is_err());
        // Missing type.
        assert!(OpenhclControllerCli::from_str("id=foo").is_err());
        // Invalid type.
        assert!(OpenhclControllerCli::from_str("id=foo,type=ide").is_err());
        // Invalid guid.
        assert!(OpenhclControllerCli::from_str("id=foo,type=scsi,guid=bad").is_err());
    }

    #[test]
    fn test_parse_vp_list() {
        use vmm_cli::BracketRangeList;

        let parse = |s: &str| {
            s.parse::<BracketRangeList>()
                .and_then(|v| v.expand_below(1024))
        };

        // Individual indices.
        assert_eq!(parse("[0,1,2,3]").unwrap(), vec![0, 1, 2, 3]);

        // Single index.
        assert_eq!(parse("[5]").unwrap(), vec![5]);

        // Dash range.
        assert_eq!(parse("[0-3]").unwrap(), vec![0, 1, 2, 3]);

        // Mixed indices and ranges.
        assert_eq!(parse("[0,1,4-6,10]").unwrap(), vec![0, 1, 4, 5, 6, 10]);

        // Whitespace tolerance.
        assert_eq!(parse("[0, 1, 2-4]").unwrap(), vec![0, 1, 2, 3, 4]);

        // Missing brackets.
        assert!(parse("0,1,2").is_err());
        assert!(parse("0-3").is_err());

        // Inverted range.
        assert!(parse("[3-0]").is_err());

        // Non-numeric.
        assert!(parse("[a,b]").is_err());
    }

    #[test]
    fn test_parse_numa_node() {
        use super::parse_numa_node;

        // Basic node with size only.
        let n = parse_numa_node("size=2G").unwrap();
        assert_eq!(
            n.memory.size,
            Some(vmm_cli::MemorySize(2 * 1024 * 1024 * 1024))
        );
        assert!(n.vps.is_none());
        assert!(n.host_numa_node.is_none());

        // Node with bracket VP list.
        let n = parse_numa_node("size=1G,vps=[0,1,2,3]").unwrap();
        assert_eq!(n.vps.unwrap().expand_below(1024).unwrap(), [0, 1, 2, 3]);

        // Node with VP range in brackets.
        let n = parse_numa_node("size=1G,vps=[0-3]").unwrap();
        assert_eq!(n.vps.unwrap().expand_below(1024).unwrap(), [0, 1, 2, 3]);

        // Node with host_numa_node.
        let n = parse_numa_node("size=1G,host_numa_node=1").unwrap();
        assert_eq!(n.host_numa_node, Some(1));

        // All options together.
        let n = parse_numa_node("size=1G,vps=[0,1],host_numa_node=0,hugepages=on").unwrap();
        assert_eq!(n.vps.unwrap().expand_below(1024).unwrap(), [0, 1]);
        assert_eq!(n.host_numa_node, Some(0));
        assert!(n.memory.hugepages);

        // Missing size.
        assert!(parse_numa_node("vps=[0,1]").is_err());

        // Bare vps without brackets.
        assert!(parse_numa_node("size=1G,vps=0,1").is_err());

        // Duplicate vps.
        assert!(parse_numa_node("size=1G,vps=[0],vps=[1]").is_err());

        // `file` is rejected for NUMA nodes.
        assert!(parse_numa_node("size=1G,file=/tmp/x").is_err());

        // Empty vps=[] for memory-only node.
        let n = parse_numa_node("size=1G,vps=[]").unwrap();
        assert!(n.vps.unwrap().0.is_empty());
    }

    #[test]
    fn test_parse_numa_distance() {
        use super::parse_numa_distance;

        let d = parse_numa_distance("0:1:20").unwrap();
        assert_eq!(d.src, 0);
        assert_eq!(d.dst, 1);
        assert_eq!(d.distance, 20);

        // Self-distance.
        let d = parse_numa_distance("0:0:10").unwrap();
        assert_eq!(d.distance, 10);

        // Distance below minimum.
        assert!(parse_numa_distance("0:1:5").is_err());

        // Wrong format.
        assert!(parse_numa_distance("0:1").is_err());
        assert!(parse_numa_distance("0:1:20:extra").is_err());
    }

    #[cfg(guest_arch = "aarch64")]
    #[test]
    fn test_smmu_cli_from_str() {
        // Minimal: only rc=, oas defaults to auto, accel off.
        let s = SmmuCli::from_str("rc=pcie0").unwrap();
        assert_eq!(s.rc_name, "pcie0");
        assert!(!s.accel);
        assert!(matches!(s.oas, SmmuOasCli::Auto));

        // accel flag.
        let s = SmmuCli::from_str("rc=pcie0,accel").unwrap();
        assert_eq!(s.rc_name, "pcie0");
        assert!(s.accel);
        assert!(matches!(s.oas, SmmuOasCli::Auto));

        // Explicit oas=auto.
        let s = SmmuCli::from_str("rc=pcie0,oas=auto").unwrap();
        assert!(matches!(s.oas, SmmuOasCli::Auto));

        // Fixed oas.
        let s = SmmuCli::from_str("rc=pcie0,oas=52").unwrap();
        assert!(matches!(s.oas, SmmuOasCli::Fixed(52)));

        // All keys/flags together, order independent.
        let s = SmmuCli::from_str("oas=48,accel,rc=pcie1").unwrap();
        assert_eq!(s.rc_name, "pcie1");
        assert!(s.accel);
        assert!(matches!(s.oas, SmmuOasCli::Fixed(48)));

        // Missing required rc=.
        assert!(SmmuCli::from_str("accel").is_err());
        assert!(SmmuCli::from_str("oas=52").is_err());

        // Empty rc= value.
        assert!(SmmuCli::from_str("rc=").is_err());

        // Duplicate rc= key.
        assert!(SmmuCli::from_str("rc=pcie0,rc=pcie1").is_err());

        // Non-numeric oas value.
        assert!(SmmuCli::from_str("rc=pcie0,oas=big").is_err());

        // Unknown key.
        assert!(SmmuCli::from_str("rc=pcie0,foo=bar").is_err());

        // Unknown flag.
        assert!(SmmuCli::from_str("rc=pcie0,turbo").is_err());
    }
}
