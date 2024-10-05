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

## Download it

Github actions in the OpenVMM repo build VmgsTool binaries for Linux and Windows.
You can easily pull the latest binaries: click on the latest run in
[OpenVMM CI](https://github.com/microsoft/openvmm/actions/workflows/openvmm-ci.yaml)
, and under "Artifacts" , you'll see them listed
(there should be artifacts titled `*-vmgstool` with different flavors the binaries were built with).

## Use it

>Note: The examples in this section use the Windows executable `vmgstool.exe`,
>which can be replaced with the Linux executable `vmgstool`.
>Developers who have already setup their development environment may also use
>the appropriate `cargo run` command. For more details on building,
>see the [build](#build-it) section below.

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

## Build it

Before you build the VmgsTool, you need to setup your development
environment. Please please refer to the getting started guide for
[Windows](./getting_started.md) or [WSL2](./getting_started_wsl.md).

If you want to build/test/experiment with this tool,
VmgsTool can be built with `cargo build -p vmgstool` for Windows and Linux.
To interact with encrypted VMGS files, you will need to compile
with the appropriate encryption feature.

Windows: `cargo build --features "encryption_win" -p vmgstool`

Linux/WSL2: `cargo build --features "encryption_ossl" -p vmgstool`
