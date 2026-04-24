# Alpine Linux, via Direct Boot

This guide boots Alpine Linux from a cloud disk image using Linux direct boot
(no UEFI or BIOS firmware required). It demonstrates how to set up PCIe, attach
virtio-blk disks, and use virtio-net for networking.

## Quick start

Run the setup script from the openvmm repo:

```bash
./scripts/setup-alpine.sh [output-dir]
```

The default output directory is `./alpine-direct-boot`. The script prints the
boot command when it finishes. It also writes a `README` file to the output
directory with the same command for future reference.

Login: `root` / `alpine`. Quit OpenVMM with `ctrl-q` then `q`.

```admonish warning
The default password is intended for local development only. Change it
or configure SSH keys before using a networking backend that exposes
the VM to other hosts.
```

## What the script does

1. **Downloads** the Alpine v3.21 "nocloud" UEFI tiny cloud image (qcow2).
2. **Converts** it to a raw disk image (`disk.raw`) with `qemu-img`.
3. **Extracts the kernel and initramfs** (`vmlinuz-virt`, `initramfs-virt`)
   from the ext4 root partition inside the image. This step requires `sudo`
   for the loopback mount.
4. **Extracts an ELF kernel** (`vmlinux-virt`) from the bzImage. OpenVMM's
   direct boot loader requires an uncompressed ELF, not a compressed bzImage.
   The script uses Python to find and decompress the gzip-embedded ELF.
5. **Creates a cloud-init data disk** (`cidata.img`), a small FAT image
   labeled `cidata` containing `user-data` and `meta-data` files. Alpine's
   `tiny-cloud` service reads these on first boot. The `user-data` uses
   `runcmd` to set the root password and add a getty on `hvc0` (Alpine does
   not spawn one by default).

### Required host tools

`curl`, `qemu-img`, `python3`, `mkfs.vfat`, `mcopy` (from mtools), and
`sudo` (for the loopback mount).

## Key points about the boot command

- **`--hv`**: Enables Hyper-V enlightenments for better performance.
- **`--pcie-root-complex` and `--pcie-root-port`**: Required for PCI device
  visibility. The default direct boot DSDT does not include a PCI bus, so
  without these flags, virtio devices will not be detected by the kernel.
- **`--virtio-blk ...,pcie_port=disk`**: Attaches the raw disk image as a
  virtio-blk device on a PCIe root port.
- **`--virtio-blk ...,ro,pcie_port=cidata`**: Attaches the cloud-init data
  image read-only on a second PCIe root port.
- **`--virtio-net pcie_port=net:consomme`**: Adds a virtio-net NIC using
  the consomme user-mode NAT backend, on a third PCIe root port.
- **`--com1 none`**: Disables the default COM1 serial port (which would
  otherwise claim the console).
- **`--virtio-console console --virtio-console-pcie-port console`**: Adds a
  virtio-console device (`/dev/hvc0` in the guest) on a fourth PCIe root
  port. This replaces COM1 as the interactive console.
- **`root=/dev/vda2`**: The root filesystem is on partition 2 of the virtio
  disk.
- **`modules=virtio_pci,virtio_blk,ext4`**: Tells the Alpine initramfs to
  load these modules early, before attempting to mount the root filesystem.
- **`console=hvc0`**: Directs the kernel to use the virtio-console
  (`/dev/hvc0`) for console output instead of the legacy serial port.

```admonish tip
Use `ctrl-q` then `q` to quit OpenVMM (not `ctrl-c`).
```
