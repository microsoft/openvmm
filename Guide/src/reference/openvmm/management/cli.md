# CLI

```admonish danger title="Disclaimer"
The following list is not exhaustive, and may be out of date.

The most up to date reference is always the [code itself](https://openvmm.dev/rustdoc/linux/openvmm_entry/struct.Options.html),
as well as the generated CLI help (via `cargo run -- --help`).
```

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
* `--gfx`: Enable a graphical console over VNC (see below)
* `--virtio-9p`: Expose a virtio 9p file system. Uses the format `tag,root_path`, e.g. `myfs,C:\\`.
  The file system can be mounted in a Linux guest using `mount -t 9p  -o trans=virtio tag /mnt/point`.
  You can specify this argument multiple times to create multiple file systems.
* `--virtio-fs`: Expose a virtio-fs file system. The format is the same as `--virtio-9p`. The
  file system can be mounted in a Linux guest using `mount -t virtiofs tag /mnt/point`.
  You can specify this argument multiple times to create multiple file systems.

And serial devices can each be configured to be relayed to different endpoints:

* `--com1/com2 <none|console|stderr|listen=PATH|listen=tcp:IP:PORT>`
  * `none`: Serial output is dropped.
  * `console`: Serial input is read and output is written to the console.
  * `stderr`: Serial output is written to stderr.
  * `listen=PATH`: A named pipe (on Windows) or Unix socket (on Linux) is set
      up to listen on the given path. Serial input and output is relayed to this
      pipe/socket.
  * `listen=tcp:IP:PORT`: As with `listen=PATH`, but listen for TCP
      connections on the given IP address and port. Typically IP will be
      127.0.0.1, to restrict connections to the current host.
