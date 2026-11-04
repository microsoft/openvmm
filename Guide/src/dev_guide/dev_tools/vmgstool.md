# VmgsTool

`vmgstool` creates, inspects, and modifies version 3 VM Guest State (VMGS)
files for provisioning and debugging.

OpenHCL uses VMGS v3 on both Hyper-V and OpenVMM. OpenVMM also uses VMGS v3 for
VMs without OpenHCL. Hyper-V VMs without an HCL use a different state format.

VMGS persists firmware variables and security state on behalf of a VM. The
store is packaged as a VHD and contains numbered logical files. These are data
slots rather than host filesystem files; for example, BIOS NVRAM and vTPM
state occupy separate well-known file IDs.

Confidential VMs can encrypt selected VMGS contents before deployment so the
host handles encrypted state rather than plaintext guest secrets.

## Data model

A VMGS store contains redundant headers, a file table, and allocated data
blocks. Commands identify logical data through either a numeric file ID or a
known symbolic name. See [`vmgs_format::FileId`][] for the authoritative list
of IDs and names.

[`vmgs_format::FileId`]: https://github.com/microsoft/openvmm/blob/main/vm/vmgs/vmgs_format/src/lib.rs#L41-L65

Use `dump-file-table` to discover which IDs are allocated before modifying a
store:

```powershell
vmgstool.exe dump-file-table --file-path path\to\guest.vmgs
```

```admonish note
`dump-file-table` can inspect an encrypted store without its key. However, its
per-file encryption information can be inaccurate when the store was last
modified by a pre-1.8 version of OpenHCL, VmgsTool, or OpenVMM.
```

## Alternatively: Pre-Built Binaries

If you would prefer to use VmgsTool without building it from scratch, you can
download pre-built copies of the binary from
[OpenVMM CI](https://github.com/microsoft/openvmm/actions/workflows/openvmm-ci.yaml).

Simply select a successful pipeline run (should have a Green checkbox), and
scroll down to select an appropriate `*-vmgstool` artifact for your particular
architecture and operating system.

## Running

```admonish tip
Note: The examples in this section use the Windows executable `vmgstool.exe`,
which can be replaced with the Linux executable `vmgstool`.

Developers who have already setup their development environment may also use
the appropriate `cargo run` command. For more details on building,

see the [build](#building) section below.
```

VmgsTool commands continue to evolve, so use `vmgstool.exe --help` for the
current interface. Every command and nested operation also has help:

```powershell
vmgstool.exe uefi-nvram dump --help
```

Except where explicitly documented, stdout and stderr are for humans and are
not stable automation formats.

### Read and Write Raw Data

To read raw data from a VMGS file, use the `dump` command. For example, to
export the decrypted binary contents of the BIOS_NVRAM (`--fileid 1`) to a file:

```powershell
vmgstool.exe dump --filepath path\to\guest.vmgs `
    --keypath path\to\key.bin --datapath path\to\nvram.bin --fileid 1
```

To write raw data to a VMGS file, use the `write` command. For example, to write
those NVRAM variables to a different, unencrypted VMGS file:

```powershell
vmgstool.exe write --filepath path\to\guest.vmgs `
    --datapath path\to\nvram.bin --fileid 1
```

By default, `write` refuses to replace a nonempty slot. Add
`--allow-overwrite` only after confirming the destination ID.

If `dump` has no `--data-path`, it writes an ASCII hexadecimal representation
to stdout. `--raw-stdout` selects raw bytes and cannot be combined with an
output path.

### Read and Parse UEFI NVRAM Variables

Furthermore, VmgsTool contains parsers to help debug UEFI NVRAM variables in
VMGS file ID 1 (`BIOS_NVRAM`). To dump the variables from an encrypted VMGS and
truncate binary data without a parser:

```powershell
vmgstool.exe uefi-nvram dump --filepath path\to\guest.vmgs `
    --keypath path\to\key.bin --truncate
```

### Read DLL File to Write IGVMfile to VMGS

VmgsTool can extract an IGVM from a resource DLL and write it to VMGS file ID 8
(`GUEST_FIRMWARE`). Select one of `NONCONFIDENTIAL`, `SNP`, `TDX`,
`SNP_NO_HCL`, `TDX_NO_HCL`, or `OPENHCL`:

```powershell
vmgstool.exe copy-igvmfile --filepath path\to\guest.vmgs `
    --datapath path\to\vmfirmwareigvm.dll --resource-code SNP
```

### Delete Boot Variables to Recover a VM that Fails to Boot

A VM may fail to boot if the disk configuration changes and
UEFI's `DefaultBootAlwaysAttempt` setting is disabled.
Deleting the existing (invalid) boot entries using VmgsTool
will trigger a default boot (which attempts to boot all available partitions and devices).

To print the boot entries in an encrypted VMGS file:

```powershell
vmgstool.exe uefi-nvram remove-boot-entries `
    --filepath path\to\guest.vmgs --keypath path\to\key.bin --dry-run
```

To actually remove the boot entries from the VMGS file, remove `--dry-run`.
This will remove all `Boot####` variables and the `BootOrder` variable.

To remove a specific boot entry or another UEFI NVRAM variable, use
`remove-entry`. For example, to remove `Boot0000`:

```powershell
vmgstool.exe uefi-nvram remove-entry --filepath path\to\guest.vmgs `
    --keypath path\to\key.bin --name Boot0000 `
    --vendor 8be4df61-93ca-11d2-aa0d-00e098032b8c
```

Always run `remove-boot-entries --dry-run` first. Removing all boot variables
causes firmware to fall back to default boot enumeration and can change which
disk starts.

## Troubleshooting

### Expected at least N more bytes, but only found M

If you get an error similar to the one below, it is likely that you are trying
to read an encrypted VMGS file and haven't provided the correct decryption key.

```text
ERROR: remove_boot_entries error
Caused by:
    0: error loading data from Nvram storage
    1: unexpected EOF. expected at least 2330702412 more bytes, but only found 30067
```

## Building

Prior to building VmgsTool, please ensure you have built either OpenVMM or
OpenHCL at least once, to ensure you have all necessary build dependencies
installed.

VmgsTool can be built with `cargo build -p vmgstool` for Windows and Linux.
To interact with encrypted VMGS files, you will need to compile with the
encryption feature: `cargo build --features encryption -p vmgstool`
