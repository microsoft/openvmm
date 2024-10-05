# Developer Features

The following are developer features that are enabled through special OpenVMM cfg options.

## Linux Direct Boot for VTL0

OpenVMM has the ability to directly boot a Linux kernel in VTL0, like normal
OpenVMM. This can be used to test Linux specific scenarios that do not require
UEFI.

By default, the kernel and initrd will be taken from the `openvmm-deps`
package.

Note that the graphical console currently does not work due to a framebuffer
issue, so a serial console or ssh must be used to access VTL0.

## Serial Access

To get access to the VTL0 VM via Serial console, first you must add a serial
port to the VM for the guest to connect to:

```powershell
Set-VmComPort HclVm00 1 "\\.\pipe\HclVm00-com1"
```

Com port 1 will correlate with TTY0 in VTL0.

To connect to COM 1 use hvc:

```
hvc serial -rcp 1 HclVm00
```
