# Windows - OpenVMM

OpenVMM can run OpenHCL on Windows using WHP. Compared with running OpenHCL on
Hyper-V, this environment has the following differences:

1. Modeling VTLs in OpenVMM adds overhead compared with hypervisor-provided
   VTLs.
2. The WHP path implements the hypercalls used by OpenHCL rather than the full
   Hyper-V hypercall surface.
3. Some OpenHCL configuration and runtime management APIs are not exposed
   through this path.

This configuration is intended primarily for developing and validating
OpenHCL. It does not reproduce every runtime or performance characteristic of
Hyper-V. To evaluate behavior in a Hyper-V environment, see
[Running OpenHCL on Hyper-V](./hyperv.md).

## Examples

```admonish warning
These examples assume basic familiarity with the OpenVMM command line. See
[Running OpenVMM](../../openvmm/run.md) for an introduction.
```

```admonish tip
These examples all use `cargo run --`, with the assumption that you are a
developer building your own copy of OpenVMM locally!

To run these examples using a pre-compiled copy of OpenVMM, swap `cargo run
--` with `/path/to/openvmm`.
```

If you run into any issues, please refer to
[OpenVMM: Troubleshooting](../../openvmm/troubleshooting.md), and/or
[OpenHCL: Troubleshooting](../troubleshooting.md).

### _Preface:_ Using ohcldiag-dev

Add support for ohcldiag-dev by specifying the `--vmbus-vtl2-vsock-path` option at vm
launch. This will create a Unix socket that the ohcldiag-dev binary can connect to by
specifying the path to the unix socket. By default, the socket is created in the
temp directory with path ohcldiag-dev. For example, running via powershell:

```powershell
cargo run -p ohcldiag-dev -- $env:temp\ohcldiag-dev kmsg
```

### Linux direct

Linux direct will work with an interactive console available via COM ports
hosted in VTL2, relayed over VMBUS like on Hyper-V. Build a Linux direct IGVM
file and launch with the following command line to enable COM0 and COM1 for
VTL0:

```powershell
cargo run -- --hv --vtl2 --igvm openhcl-x64.bin --com3 "term,name=VTL2 OpenHCL" -m 2GB --vmbus-com1-serial "term,name=VTL0 Linux" --vmbus-com2-serial "term,name=COM2" --vmbus-vtl2-vsock-path $env:temp\ohcldiag-dev
```

This will launch OpenVMM in VTL2 mode using [Windows Terminal](https://learn.microsoft.com/en-us/windows/terminal/install) to display the
output of the serial ports. You can replace `term` with `term=<path to exe>` to use your
favorite shell and by default OpenVMM will use `cmd.exe`. A vsock window can be opened
using the OpenVMM terminal on windows using `v 9980` or whichever hvsock port is
configured to allow consoles for OpenHCL.

### Vtl2 VMBus Support

OpenHCL run under OpenVMM can act as the VMBus server to VTL0. Additionally,
OpenHCL can be configured to forward offers made by OpenVMM to VTL0.
For architectural background, see [VMBus Relay and Device Interception](../../../reference/architecture/openhcl/vmbus.md).

To run OpenVMM and OpenHCL with VMBus host relay support:

```bash
 --vmbus-redirect
```

### Assigning MANA devices to VTL2

OpenHCL can be assigned a MANA NIC to VTL2, and expose a VMBus NIC to the
guest in VTL0. Expose it by adding the following:

```bash
--net uh:consomme --vmbus-redirect
```

### Assigning SCSI devices to VTL2

You can assign a SCSI disk to VTL2 and have OpenHCL reassign it to VTL0:

```bash
--vmbus-scsi id=scsi0,vtl2 \
  --openhcl-controller id=relay0,type=scsi \
  --disk file:ubuntu.img,on=scsi0,relay=relay0 --vmbus-redirect
```

### Assigning NVME devices to VTL2

You can assign an NVME disk to VTL2 and have OpenHCL relay it to VTL0 as a
VMBus scsi device (see [Storage Translation](../../../reference/architecture/openhcl/storage_translation.md)):

```bash
--nvme-pci id=nvme0,vpci,vtl2 \
  --openhcl-controller id=relay0,type=scsi \
  --disk mem:1G,on=nvme0,relay=relay0 --vmbus-redirect
```
