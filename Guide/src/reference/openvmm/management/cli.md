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
* `--uefi-firmware <FILE>`: Path to the UEFI firmware file (`MSVM.fd`). When `--uefi` is specified, this option is required only if you do not set the environment variable `OPENVMM_UEFI_FIRMWARE` (or the architecture-specific variants `X86_64_OPENVMM_UEFI_FIRMWARE`, or `AARCH64_OPENVMM_UEFI_FIRMWARE`). If omitted, the default is read from `OPENVMM_UEFI_FIRMWARE` first, then falls back to the architecture-specific variables.
* `--pcat`: Boot using the Microsoft Hyper-V PCAT BIOS
* `--disk file:<DISK>`: Exposes a single disk over VMBus. You must also pass `--hv`. The `DISK` argument can be:
  * A flat binary disk image
  * A VHD file with an extension of .vhd (Windows host only)
  * A VHDX file with an extension of .vhdx (Windows host only)
* `--private-memory`: Use private anonymous memory for guest RAM
  instead of shared file-backed sections.
* `--thp`: Enable Transparent Huge Pages for guest RAM (Linux only).
  Requires `--private-memory`.
* `--pidfile <PATH>`: Write the process ID to the specified file on startup,
  and remove it on clean exit. If the process is killed with `SIGKILL` or
  crashes, the pidfile is not removed — consumers should verify the PID is
  still alive. No file locking is performed; concurrent launches with the same
  pidfile path will overwrite each other. Not written for short-lived utility
  modes such as `--write-saved-state-proto`.
* `--nic`: Exposes a NIC using the Consomme user-mode NAT.
* `--gfx`: Enable a graphical console over VNC (see below)
* `--virtio-9p`: Expose a virtio 9p file system. Uses the format `tag,root_path`, e.g. `myfs,C:\\`.
  The file system can be mounted in a Linux guest using `mount -t 9p  -o trans=virtio tag /mnt/point`.
  You can specify this argument multiple times to create multiple file systems.
* `--virtio-fs`: Expose a virtio-fs file system. The format is the same as `--virtio-9p`. The
  file system can be mounted in a Linux guest using `mount -t virtiofs tag /mnt/point`.
  You can specify this argument multiple times to create multiple file systems.
* `--virtio-rng`: Add a virtio entropy (RNG) device, exposing `/dev/hwrng` in the Linux guest.
  The guest kernel must have `CONFIG_HW_RANDOM_VIRTIO` enabled.
* `--virtio-rng-bus <BUS>`: Select the bus for the virtio-rng device (`auto`, `mmio`, `pci`, `vpci`).
  Defaults to `auto`.
* `--vhost-user <SOCKET_PATH>,type=<TYPE>[,tag=<NAME>][,num_queues=<N>][,queue_size=<N>][,pcie_port=<PORT>]`: Attach a
  vhost-user device backed by an external process over a Unix socket (Linux
  only). The backend process must already be listening on `SOCKET_PATH`.
  Supported `type` values: `blk`, `fs`. For `type=fs`, `tag=<NAME>` is required
  and specifies the mount tag exposed to the guest (max 36 bytes).
  `num_queues` and `queue_size` control the queue layout (defaults: blk
  num_queues=1/queue_size=128, fs num_queues=1/queue_size=1024).
  Alternatively, use `device_id=<N>` instead of `type=` to specify the numeric
  virtio device ID directly, with `queue_sizes=[N,N,N]` for per-queue sizes.
  Examples:
  ```sh
  --vhost-user /tmp/vhost-blk.sock,type=blk
  --vhost-user /tmp/vhost-blk.sock,type=blk,num_queues=4,queue_size=512
  --vhost-user /tmp/vhost-blk.sock,type=blk,pcie_port=rp0
  --vhost-user /tmp/virtiofsd.sock,type=fs,tag=myfs
  --vhost-user /tmp/virtiofsd.sock,type=fs,tag=myfs,num_queues=2,queue_size=1024
  --vhost-user /tmp/vhost.sock,device_id=26,queue_sizes=[256,256]
  ```

Serial devices can be configured to appear as different devices inside the guest:

* `--com1/com2 <BACKEND>`: Configure a COM port serial device.
* `--virtio-console <BACKEND>`: Expose a virtio console device (appears as
  `/dev/hvc0` inside the guest).

The `BACKEND` argument is the same for all serial devices:

  * `none`: Serial output is dropped.
  * `console`: Serial input is read and output is written to the console.
  * `stderr`: Serial output is written to stderr.
  * `listen=PATH`: A named pipe (on Windows) or Unix socket (on Linux) is set
      up to listen on the given path. Serial input and output is relayed to this
      pipe/socket.
  * `listen=tcp:IP:PORT`: As with `listen=PATH`, but listen for TCP
      connections on the given IP address and port. Typically IP will be
      127.0.0.1, to restrict connections to the current host.

## PCIe Device Support

OpenVMM can emulate a PCI Express topology using `--pcie-root-complex` and
`--pcie-root-port`. Devices that support the `pcie_port=` option can be
attached to a root port to appear as PCIe devices in the guest.

### Setting up a PCIe topology

```sh
# Create a root complex and root port
--pcie-root-complex rc0 --pcie-root-port rc0:rp0
```

### Attaching devices to PCIe

Several device types support the `pcie_port=<name>` option to attach to a
PCIe root port. The syntax varies slightly between device types:

**Disks** (comma-separated option): `--disk`, `--nvme`, `--virtio-blk`

```sh
--virtio-blk file:/path/to/disk.raw,pcie_port=rp0
--nvme file:/path/to/disk.raw,pcie_port=rp0
--disk file:/path/to/disk.raw,pcie_port=rp0
```

**NICs** (colon-prefixed): `--net`, `--virtio-net`, `--mana`

```sh
--virtio-net pcie_port=rp0:tap:tap0  # TAP is Linux-only
--net pcie_port=rp0:consomme
--mana pcie_port=rp0:tap:tap0        # TAP is Linux-only
```

**Filesystems and other virtio devices** (colon-prefixed):
`--virtio-fs`, `--virtio-fs-shmem`, `--virtio-9p`, `--virtio-pmem`

```sh
--virtio-fs pcie_port=rp0:myfs,/path/to/share
--virtio-fs-shmem pcie_port=rp0:myfs,/path/to/share
--virtio-9p pcie_port=rp0:myfs,/path/to/share
--virtio-pmem pcie_port=rp0:/path/to/file
```

For `--virtio-rng` and `--virtio-console`, use their separate PCIe port flags:

```sh
--virtio-rng --virtio-rng-pcie-port rp0
--virtio-console console --virtio-console-pcie-port rp0
```

**vhost-user devices** (comma-separated option, Linux only): `--vhost-user`

```sh
--vhost-user /tmp/vhost-blk.sock,type=blk,pcie_port=rp0
--vhost-user /tmp/virtiofsd.sock,type=fs,tag=myfs,pcie_port=rp0
```
