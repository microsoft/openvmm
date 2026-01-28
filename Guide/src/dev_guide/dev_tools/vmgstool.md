# VmgsTool

OpenHCL VMs store their firmware state and attributes (UEFI variables) in a special VM
Guest State (VMGS) file. The OpenHCL interacts with and persists data in the VMGS file
on behalf of the VM. The VMGS file is packaged as a VHD which the host OS interacts with.
For Confidential OpenHCL VMs, this VHD can be encrypted before VM deployment, so that the
host only interacts with an encrypted VHD and hence the file's contents are kept confidential from the host.

The VMGS file contains several elements called "files" (these are not strictly files, simply
"chunks of data”, logical groupings of data). Each "file" has a unique, well known index;
for example, vTPM state is stored in file id "3".

The VmgsTool is a tool that allows for offline manipulation of a VMGS (version 3) file
for provisioning and debugging purposes. Basically, it's a tool to interact with the VMGS
file, and it can help you perform operations such as reading, creating, modifying and
removing "files" from the VMGS file and even creating an encrypted datastore to
allow certain "files" to be encrypted as the scenario requires it.

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

The VmgsTool commands are always evolving, so use `vmgstool.exe --help` to see the
most up to date information about the available commands. Options for each command
and subcommand are also available. For example: `vmgstool.exe uefi-nvram dump --help`

### Read and Write Raw Data

To read raw data from a VMGS file, use the `dump` command. For example, to
export the decrypted binary contents of the BIOS_NVRAM (`--fileid 1`) to a file:

`vmgstool.exe dump --filepath <vmgs file path> --keypath <key file path> --datapath <data file path> --fileid 1`

To write raw data to a VMGS file, use the `write` command. For example, to write
those NVRAM variables to a different, unencrypted VMGS file:

`vmgstool.exe write --filepath <vmgs file path> --datapath <data file path> --fileid 1`

### Read and Parse UEFI NVRAM Variables

Furthermore, the VmgsTool contains parsers to help debug issues with the UEFI NVRAM
variables stored in the VMGS FileId 1 (BIOS_NVRAM). For example, to dump the NVRAM
variables for an encrypted VMGS file, truncating the binary data contents of
variables without parsers:

`vmgstool.exe uefi-nvram dump --filepath <vmgs file path> --keypath <key file path> --truncate`

### Read DLL File to Write IGVMfile to VMGS

Additionally, the VmgsTool contains a tool to read the IGVMfile from a DLL (passed in as a data file)
and write it to VMGS FileId 8 (GUEST_FIRMWARE). To do this pass one of five resource codes
(nonconfidential, snp, tdx, snp_no_hcl, tdx_no_hcl) into the cmdline tool:

`vmgstool.exe copy-igvmfile --filepath <vmgs file path> --keypath <key file path> --datapath <dll path> --resource-code <code>`

### Delete Boot Variables to Recover a VM that Fails to Boot

A VM may fail to boot if the disk configuration changes and
UEFI's `DefaultBootAlwaysAttempt` setting is disabled.
Deleting the existing (invalid) boot entries using VmgsTool
will trigger a default boot (which attempts to boot all available partitions and devices).

To print the boot entries in an encrypted VMGS file:
`vmgstool.exe uefi-nvram remove-boot-entries --filepath <vmgs file path> --keypath <key file path> --dry-run`

To actually remove the boot entries from the VMGS file, remove `--dry-run`.
This will remove all `Boot####` variables and the `BootOrder` variable.

If you would like to remove a specific boot entry or any other UEFI NVRAM variable,
use `remove-entry`. For example, to remove `Boot0000`:

`vmgstool.exe uefi-nvram remove-entry --filepath <vmgs file path>--keypath <key file path> --name Boot0000 --vendor 8be4df61-93ca-11d2-aa0d-00e098032b8c`

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
appropriate encryption feature.

Windows: `cargo build --features "encryption_win" -p vmgstool`

Linux/WSL2: `cargo build --features "encryption_ossl" -p vmgstool`
