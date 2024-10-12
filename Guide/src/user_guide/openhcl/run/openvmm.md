# Windows - OpenVMM

OpenVMM currently has basic support for running with OpenHCL when run on Windows
with WHP, with some caveats:

1. Performance is not great due to the extra overhead of OpenVMM modeling VTLs,
   not the hypervisor.
2. Not all hypercalls are implemented, only the set used by OpenHCL. If any
   are missing, you can implement the required support in OpenVMM's WHP
   implementation.

> Due to the developer-oriented nature of this support at the moment, the
> following examples assume you are already comfortable with the OpenVMM command
> line, and are willing to deal with certain "rough edges" in the experience.
>
> At this time, we strongly suggest running and testing OpenHCL on Hyper-V.

# Examples

## Preface: Using ohcldiag-dev

Add support for ohcldiag-dev by specifying the `--vtl2-vsock-path` option at vm
launch. This will create a Unix socket that the ohcldiag-dev binary can connect to by
specifying the path to the unix socket. By default, the socket is created in the
temp directory with path ohcldiag-dev. For example, running via powershell:

```powershell
cargo run -p ohcldiag-dev -- $env:temp\ohcldiag-dev kmsg
```

## Linux direct

Linux direct will work with an interactive console available via COM ports
hosted in VTL2, relayed over VMBUS like on Hyper-V. Build a Linux direct IGVM
file and launch with the following command line to enable COM0 and COM1 for
VTL0:

```powershell
cargo run -- --hv --vtl2 --igvm openhcl-x64.bin --com3 term -m 2GB --vmbus-com1-serial term --vmbus-com2-serial term --vtl2-vsock-path $env:temp\ohcldiag-dev
```

This will launch OpenVMM in VTL2 mode using Windows Terminal to display the
output of the serial ports. You can use `term=<path to exe>` to use your
favorite shell and by default OpenVMM will use `cmd.exe`. A vsock window can be
opened using the OpenVMM terminal on windows using `v 9980` or whichever hvsock
port is configured to allow consoles for OpenHCL.



## Vtl2 VMBus Support

OpenHCL run under OpenVMM can act as the VMBus server to VTL0. Additionally,
OpenHCL can be configured to forward offers made by OpenVMM to VTL0.

To run OpenVMM and OpenHCL with VMBus host relay support:

```bash
 --vmbus-redirect
```

## Assigning MANA devices to VTL2

OpenHCL can be assigned a MANA NIC to VTL2, and expose a VMBus NIC to the
guest in VTL0. Expose it by adding the following:

```bash
--net uh:consomme --vmbus-redirect
```

## Assigning SCSI devices to VTL2

You can assign a SCSI disk to VTL2 and have OpenHCL reassign it to VTL0:

```bash
--disk file:ubuntu.img,uh --vmbus-redirect
```

## Assigning NVME devices to VTL2

You can assign an NVME disk to VTL2 and have OpenHCL relay it to VTL0 as a
VMBus scsi device:

```bash
--disk mem:1G,uh-nvme --vmbus-redirect
```
