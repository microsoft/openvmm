
## Running OpenVMM

This page provides a few examples of how you might launch OpenVMM.

This is _not_ a comprehensive guide to all available config options and
settings, and should be used as a jumping off point for self-guided exploration.

### Preface: Quitting OpenVMM

By default, OpenVMM will connect the guests's COM1 serial port to the current
terminal session, forwarding all keystrokes directly to the VM.

As such, a simple `ctrl-c` does not suffice to quit OpenVMM!

Instead, you can type `crtl-q` to enter OpenVMM's
[interactive console](./diag/interactive_console.md), and enter `q` to quit.

### Example: Linux Guest

For convenience, and to offer an "instant on" example of `openvmm` running - a
sample prebuilt Linux kernel and initrd are provided via the `cargo xflowey
restore-packages` command.

To launch Linux with an interactive console into the shell within initrd, simply
run:

```shell
cargo run -- -c single
```

This launches Linux via direct boot (i.e: without going through UEFI or BIOS),
and appends `single` to the kernel command line.

The Linux console is hooked up to COM1, which is relayed to the host terminal by
default.

The kernel and initrd can be controlled via options:

* `--kernel <PATH>`: The kernel image. Must be an uncompressed kernel (vmlinux, not bzImage).
* `--initrd <PATH>`: The initial ramdisk image.
* `-c <STRING>` or `--cmdline <STRING>`: Extra kernel command line options, such as `root=/dev/sda`.

### Example: Windows, booted with UEFI

A copy of the `mu_msvm` UEFI firmware is also downloaded via `cargo xflowey restore-packages`.

To launch the UEFI firmware, pass the `--uefi` flag to OpenVMM. For this to be
useful, you will also want something to boot from; currently, only a SCSI disk
is automatically added to the boot order. So you can try:

```shell
cargo run -- --uefi --disk memdiff:windows.vhdx --gfx
```

For more info on `--gfx`, and how to actually interact with the VM using a
mouse/keyboard/video, see the [Graphical Console](#graphical-console) docs
below.

The file `windows.vhdx` can be any format of VHD(X).

Note that OpenVMM does not currently support using VHD/VHDX files on Linux
hosts. Instead, convert the OS image to raw format using the following command:

```bash
qemu-img convert -f vhdx -O raw windows.vhdx windows.img
```

Also, note the use of `memdiff`, which creates a memory-backed "differencing disk",
which ensures that any writes the VM makes to the VHD are not persisted between
runs. This is very useful when iterating on OpenVMM code, since booting the VM
becomes repeatable and you don't have to worry about shutting down properly. Use
`file` instead for normal persistent storage.

## Configuring virtual hardware

> Note: The following list is not exhaustive, and may be out of date. The most
> up to date reference is always the code itself. For a full list of command
> line arguments that can be passed to OpenVMM, run `cargo run -- --help`.

* `--processors <COUNT>`: The number of processors. Defaults to 1.
* `--memory <SIZE>`: The VM's memory size. Defaults to 1GB.
* `--hv`: Exposes Hyper-V enlightenments and VMBus support.
* `--uefi`: Boot using `mu_msvm` UEFI
* `--pcat`: Boot using the Microsoft Hyper-V PCAT BIOS
* `--disk file:<DISK>`: Exposes a single disk over VMBus. You must also pass `--hv`. The `DISK` argument can be:
  * A flat binary disk image
  * A VHD file with an extension of .vhd (Windows host only)
  * A VHDX file with an extension of .vhdx (Windows host only)
* `--nic`: Exposes a NIC using the Consomme user-mode NAT.
* `--virtio-console`: Enables a virtio serial device (via the MMIO transport) for Linux console access instead of COM1.
* `--virtio-console-pci`: Uses the PCI transport for the virtio serial console.
* `--gfx`: Enable a graphical console over VNC (see below)
* `--virtio-9p`: Expose a virtio 9p file system. Uses the format `tag:root_path`, e.g. `myfs:C:\\`.
  The file system can be mounted in a Linux guest using `mount -t 9p  -o trans=virtio tag /mnt/point`.
  You can specify this argument multiple times to create multiple file systems.
* `--virtio-fs`: Expose a virtio-fs file system. The format is the same as `--virtio-9p`. The
  file system can be mounted in a Linux guest using `mount -t virtiofs tag /mnt/point`.
  You can specify this argument multiple times to create multiple file systems.

And serial devices can each be configured to be relayed to different endpoints:

* `--com1/com2/virtio-serial <none|console|stderr|listen=PATH>`
    * `none`: Serial output is dropped.
    * `console`: Serial input is read and output is written to the console.
    * `stderr`: Serial output is written to stderr.
    * `listen=PATH`: A named pipe (on Windows) or Unix socket (on Linux) is set
      up to listen on the given path. Serial input and output is relayed to this
      pipe/socket.

## Running a full-featured Alpine Linux distribution

In order to set up Alpine for use with OpenVMM, it is easiest to first install it
normally using a Hyper-V VM. First, [download the standard Alpine x86_64 ISO
file](https://www.alpinelinux.org/downloads/). Then, create a new Generation 1
VM, using a fixed VHD (*not* VHDX) as the disk. Make sure the VM has networking
and disable automatic checkpoints.

Boot the VM from the Alpine ISO, run `setup-alpine` and go through the
installation. Most of the default options are fine. You'll want to partition the
disk using the "sys" option, which creates a /boot, a swap, and a root
partition. After installation, eject the ISO and reboot the VM.

To be able to use the installation with OpenVMM, you must enable the serial
console and configure it to allow logging in as root from the serial console.
Run the following two commands:

```bash
sed -i /^#ttyS0/s/^#// /etc/inittab
echo ttyS0 >> /etc/securetty
```

Finally, `poweroff` the VM, then run OpenVMM with the following command:

```shell
cargo run -- --hv --nic --disk file:alpine.vhd -c root=/dev/sda3
```

## Graphical console

OpenVMM supports a graphical console exposed via VNC. To enable it, pass `--gfx`
on the command line--this will start a VNC server on localhost port 5900. The
port value can be changed with the `--vnc-port <PORT>` option.

OpenVMM's VNC server also includes "pseudo" client-clipboard support, whereby the
"Ctrl-Alt-P" key sequence will be intercepted by the server to type out the
contents of the VNC clipboard.

Once OpenVMM starts, you can connect to the VNC server using any supported VNC
client. The following clients have been tested working with OpenVMM:
* [TightVNC](https://www.tightvnc.com/download.php)
* [TigerVNC](https://github.com/TigerVNC/tigervnc)
* [RealVNC](https://www.realvnc.com/en/?lai_sr=0-4&lai_sl=l)

Once you have downloaded and installed it you can connect to `localhost` with
the appropriate port to see your VM.
