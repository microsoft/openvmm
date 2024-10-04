// Copyright (C) Microsoft Corporation. All rights reserved.

//! CLI argument parsing for the underhill core process.

#![warn(missing_docs)]

use anyhow::bail;
use anyhow::Context;
use std::path::PathBuf;

// We've made our own parser here instead of using something like clap in order
// to save on compiled file size. We don't need all the features a crate can provide.
/// underhill core command-line and environment variable options.
pub struct Options {
    /// (UNDERHILL_WAIT_FOR_START=1 | --wait-for-start)
    ///  wait for a diagnostics start request before initializing and starting the VM
    pub wait_for_start: bool,

    /// (UNDERHILL_REFORMAT_VMGS=1 | --reformat-vmgs)
    /// reformat the VMGS file on boot. useful for running potentially destructive VMGS tests.
    pub reformat_vmgs: bool,

    /// (UNDERHILL_PID_FILE_PATH=/path/to/file | --pid /path/to/file)
    /// write the PID to the specified path
    pub pid: Option<PathBuf>,

    /// (UNDERHILL_VMBUS_MAX_VERSION=\<number\>)
    /// limit the maximum protocol version allowed by vmbus; used for testing purposes
    pub vmbus_max_version: Option<u32>,

    /// (UNDERHILL_VMBUS_ENABLE_MNF=1)
    /// Enable handling of MNF in the Underhill vmbus server, instead of the host.
    pub vmbus_enable_mnf: Option<bool>,

    /// (UNDERHILL_CMDLINE_APPEND=\<string\>)
    /// Command line to append to VTL0, only used with direct boot.
    pub cmdline_append: Option<String>,

    /// (UNDERHILL_VNC_PORT=\<number\> | --vnc-port \<number\>) (default: 3)
    /// VNC (vsock) port number
    pub vnc_port: u32,

    /// (UNDERHILL_GDBSTUB=1)
    /// Enables the GDB stub for debugging the guest.
    pub gdbstub: bool,

    /// (UNDERHILL_GDBSTUB_PORT=\<number\>) (default: 4)
    /// GDB stub (vsock) port number.
    pub gdbstub_port: u32,

    /// (UNDERHILL_VTL0_STARTS_PAUSED=1)
    /// Start with VTL0 paused
    pub vtl0_starts_paused: bool,

    /// (UNDERHILL_FRAMEBUFFER_GPA_BASE=\<number\>)
    /// Base GPA of the fixed framebuffer mapping for underhill to read.
    /// If a value is provided, a graphics device is exposed.
    // TODO: send this value as an IGVM device tree parameter instead
    pub framebuffer_gpa_base: Option<u64>,

    /// (UNDERHILL_SERIAL_WAIT_FOR_RTS=\<bool\>)
    /// Whether the emulated 16550 waits for guest DTR+RTS before pulling data
    /// from the host.
    pub serial_wait_for_rts: bool,

    /// (UNDERHILL_FORCE_LOAD_VTL0_IMAGE=\<string\>)
    /// Force load the specified image in VTL0. The image must support the
    /// option specified.
    ///
    /// Valid options are "pcat, uefi, linux".
    pub force_load_vtl0_image: Option<String>,

    /// (UNDERHILL_NVME_VFIO=1)
    /// Use the user-mode VFIO NVMe driver instead of the Linux driver.
    pub nvme_vfio: bool,

    /// (UNDERHILL_MCR_DEVICE=1)
    /// MCR Device Enable
    pub mcr: bool, // TODO MCR: support closed-source ENV vars

    /// (UNDERHILL_EMULATE_APIC=1)
    /// Enable an APIC emulator.
    pub emulate_apic: bool,

    /// (UNDERHILL_ENABLE_SHARED_VISIBILITY_POOL=1)
    /// Enable the shared visibility pool. This is enabled by default on
    /// hardware isolated platforms, but can be enabled for testing. Hardware
    /// devices will use the shared pool for DMA if enabled.
    pub enable_shared_visibility_pool: bool,

    /// (UNDERHILL_CVM_GUEST_VSM=1)
    /// Enable support for guest vsm in CVMs. This is disabled by default.
    pub cvm_guest_vsm: bool,

    /// (UNDERHILL_HALT_ON_GUEST_HALT=1) When receiving a halt request from a
    /// lower VTL, halt underhill instead of forwarding the halt request to the
    /// host. This allows for debugging state without the partition state
    /// changing from the host.
    pub halt_on_guest_halt: bool,

    /// (UNDERHILL_NO_SIDECAR_HOTPLUG=1) Leave sidecar VPs remote even if they
    /// hit exits.
    pub no_sidecar_hotplug: bool,
}

impl Options {
    pub(crate) fn parse(extra_args: Vec<String>) -> anyhow::Result<Self> {
        let parse_env_bool = |name| {
            std::env::var_os(name)
                .map(|v| v.to_ascii_lowercase() == "true" || v == "1")
                .unwrap_or_default()
        };

        let parse_env_number = |name| {
            std::env::var_os(name)
                .map(|v| {
                    v.to_string_lossy().parse().context(format!(
                        "Error parsing numeric environment variable {} {:?}",
                        name, v
                    ))
                })
                .transpose()
        };

        let mut wait_for_start = parse_env_bool("UNDERHILL_WAIT_FOR_START");
        let mut reformat_vmgs = parse_env_bool("UNDERHILL_REFORMAT_VMGS");
        let mut pid = std::env::var_os("UNDERHILL_PID_FILE_PATH")
            .map(|x| x.to_string_lossy().into_owned().into());
        let vmbus_max_version = std::env::var_os("UNDERHILL_VMBUS_MAX_VERSION")
            .map(|x| {
                vmbus_core::parse_vmbus_version(&(x.to_string_lossy()))
                    .map_err(|x| anyhow::anyhow!("Error parsing vmbus max version: {}", x))
            })
            .transpose()?;
        let vmbus_enable_mnf = if std::env::var_os("UNDERHILL_VMBUS_ENABLE_MNF").is_some() {
            Some(parse_env_bool("UNDERHILL_VMBUS_ENABLE_MNF"))
        } else {
            None
        };
        let cmdline_append =
            std::env::var_os("UNDERHILL_CMDLINE_APPEND").map(|x| x.to_string_lossy().into_owned());
        let force_load_vtl0_image = std::env::var_os("UNDERHILL_FORCE_LOAD_VTL0_IMAGE")
            .map(|x| x.to_string_lossy().into_owned());
        let mut vnc_port = parse_env_number("UNDERHILL_VNC_PORT")?.map(|x| x as u32);
        let framebuffer_gpa_base = parse_env_number("UNDERHILL_FRAMEBUFFER_GPA_BASE")?;
        let vtl0_starts_paused = parse_env_bool("UNDERHILL_VTL0_STARTS_PAUSED");
        let serial_wait_for_rts = parse_env_bool("UNDERHILL_SERIAL_WAIT_FOR_RTS");
        let nvme_vfio = parse_env_bool("UNDERHILL_NVME_VFIO");
        let emulate_apic = parse_env_bool("UNDERHILL_EMULATE_APIC");
        let mcr = parse_env_bool("UNDERHILL_MCR_DEVICE");
        let enable_shared_visibility_pool =
            parse_env_bool("UNDERHILL_ENABLE_SHARED_VISIBILITY_POOL");
        let cvm_guest_vsm = parse_env_bool("UNDERHILL_CVM_GUEST_VSM");
        let halt_on_guest_halt = parse_env_bool("UNDERHILL_HALT_ON_GUEST_HALT");
        let no_sidecar_hotplug = parse_env_bool("UNDERHILL_NO_SIDECAR_HOTPLUG");
        let gdbstub = parse_env_bool("UNDERHILL_GDBSTUB");
        let gdbstub_port = parse_env_number("UNDERHILL_GDBSTUB_PORT")?.map(|x| x as u32);

        let mut args = std::env::args().chain(extra_args);
        // Skip our own filename.
        args.next();

        while let Some(next) = args.next() {
            let arg = next;

            match &*arg {
                "--wait-for-start" => wait_for_start = true,
                "--reformat-vmgs" => reformat_vmgs = true,

                x if x.starts_with("--") && x.len() > 2 => {
                    if let Some(eq) = arg.find('=') {
                        let (name, value) = arg.split_at(eq);
                        // Don't forget to exclude the '=' itself.
                        let value = &value[1..];
                        Self::parse_value_arg(name, value, &mut pid, &mut vnc_port)?;
                    } else {
                        if let Some(value) = args.next() {
                            Self::parse_value_arg(&arg, &value, &mut pid, &mut vnc_port)?;
                        } else {
                            bail!("Expected a value after argument {}", arg);
                        }
                    }
                }
                x => bail!("Unrecognized argument {}", x),
            }
        }

        Ok(Self {
            wait_for_start,
            reformat_vmgs,
            pid,
            vmbus_max_version,
            vmbus_enable_mnf,
            cmdline_append,
            vnc_port: vnc_port.unwrap_or(3),
            framebuffer_gpa_base,
            gdbstub,
            gdbstub_port: gdbstub_port.unwrap_or(4),
            vtl0_starts_paused,
            serial_wait_for_rts,
            force_load_vtl0_image,
            nvme_vfio,
            mcr,
            emulate_apic,
            enable_shared_visibility_pool,
            cvm_guest_vsm,
            halt_on_guest_halt,
            no_sidecar_hotplug,
        })
    }

    fn parse_value_arg(
        name: &str,
        value: &str,
        pid: &mut Option<PathBuf>,
        vnc_port: &mut Option<u32>,
    ) -> anyhow::Result<()> {
        match name {
            "--pid" => *pid = Some(value.into()),
            "--vnc-port" => {
                *vnc_port = Some(
                    value
                        .parse()
                        .context(format!("Error parsing VNC port {}", value))?,
                )
            }
            x => bail!("Unrecognized argument {}", x),
        }

        Ok(())
    }
}
