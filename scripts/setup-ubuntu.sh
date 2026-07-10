#!/bin/bash
# Setup script for running Ubuntu Server on OpenVMM via UEFI boot.
# Downloads an Ubuntu cloud image, converts it to a raw disk, and creates a
# cloud-init (NoCloud) seed disk that sets a known login password and wires up
# a serial console so you can log in over COM1.
#
# Unlike setup-alpine.sh (which uses Linux direct boot), this boots the stock
# cloud image through the mu_msvm UEFI firmware -- the same way the image boots
# on Azure or Hyper-V -- so there is no kernel/initramfs extraction step.
#
# Topology: the root disk is attached as NVMe on an emulated PCIe root complex.
# This is the most broadly compatible configuration, not a requirement of
# Ubuntu itself -- the guest boots equally well from virtio-blk or VMBus SCSI
# disks. NVMe is chosen here because:
#   * UEFI cannot yet boot from virtio-blk, so virtio-blk is not an option for
#     the boot disk.
#   * VMBus is not available on all targets (notably KVM/aarch64 has no VMBus
#     support yet), so a VMBus SCSI boot disk would not be portable.
# NVMe over emulated PCIe avoids both limitations and works everywhere.
#
# The cloud-init seed disk is attached as virtio-blk (not a second NVMe disk)
# to work around an NVMe namespace ID conflict between two raw-file NVMe disks
# -- a separate bug. virtio-blk is fine here because only the boot disk needs
# to be reachable by UEFI; cloud-init runs after the kernel is up, by which
# point the virtio-blk driver is available.
#
# Usage:
#   ./setup-ubuntu.sh [output-dir]
#
# Architecture is auto-detected from the host (uname -m) and can be
# overridden by setting the ARCH environment variable to x86_64 or aarch64,
# e.g.:
#   ARCH=aarch64 ./setup-ubuntu.sh
#
# Default output directory: ./ubuntu-uefi

set -euo pipefail

# --- Resolve target architecture ---

ARCH="${ARCH:-$(uname -m)}"
case "$ARCH" in
    x86_64|amd64)
        ARCH="x86_64"
        UBUNTU_ARCH="amd64"
        SERIAL_TTY="ttyS0"
        ;;
    aarch64|arm64)
        ARCH="aarch64"
        UBUNTU_ARCH="arm64"
        SERIAL_TTY="ttyAMA0"
        ;;
    *)
        echo "ERROR: Unsupported architecture: $ARCH (expected x86_64 or aarch64)" >&2
        exit 1
        ;;
esac

# Ubuntu 26.04 LTS "Resolute Raccoon".
UBUNTU_VERSION="26.04"
UBUNTU_CODENAME="resolute"
IMAGE_NAME="ubuntu-${UBUNTU_VERSION}-server-cloudimg-${UBUNTU_ARCH}"
IMAGE_URL="https://cloud-images.ubuntu.com/releases/${UBUNTU_CODENAME}/release/${IMAGE_NAME}.img"

# Grow the root disk to this size; cloud-init's growpart expands the rootfs to
# fill it on first boot. The downloaded image is only a few GiB.
DISK_SIZE="16G"

# Known login credentials baked in by cloud-init.
GUEST_USER="ubuntu"
GUEST_PASSWORD="ubuntu"

OUTDIR="${1:-./ubuntu-uefi}"

# --- Check required tools ---

missing=()
for tool in curl qemu-img mkfs.vfat mcopy iconv; do
    if ! command -v "$tool" &>/dev/null; then
        missing+=("$tool")
    fi
done

if [ ${#missing[@]} -gt 0 ]; then
    echo "ERROR: Missing required tools: ${missing[*]}" >&2
    echo "" >&2
    echo "Install them with your package manager, e.g.:" >&2
    echo "  sudo apt install curl qemu-utils dosfstools mtools" >&2
    echo "  sudo dnf install curl qemu-img dosfstools mtools" >&2
    echo "  sudo tdnf install curl qemu-img dosfstools mtools glibc-iconv" >&2
    exit 1
fi

mkdir -p "$OUTDIR"
cd "$OUTDIR"

# --- Step 1: Download the Ubuntu cloud image ---
# The .img file is a qcow2 image despite the extension.

if [ ! -f "${IMAGE_NAME}.img" ]; then
    echo "Downloading Ubuntu ${UBUNTU_VERSION} (${UBUNTU_ARCH}) cloud image..."
    curl --fail -Lo "${IMAGE_NAME}.img" "$IMAGE_URL"
else
    echo "Ubuntu cloud image already downloaded."
fi

# --- Step 2: Convert qcow2 to raw and grow it ---
# OpenVMM's `--disk file:` backend expects a flat/raw image, not qcow2.

echo "Converting cloud image to raw..."
qemu-img convert -f qcow2 -O raw "${IMAGE_NAME}.img" disk.raw

echo "Resizing root disk to ${DISK_SIZE}..."
qemu-img resize -f raw disk.raw "$DISK_SIZE"

# --- Step 3: Create the cloud-init (NoCloud) seed disk ---
# The cloud image ships with no password (SSH-key login only). This seed disk
# sets a known password for the default user and ensures a serial getty + serial
# console so we can log in over COM1.
#
# cloud-init's NoCloud datasource looks for a filesystem labelled `cidata`
# containing `user-data` and `meta-data`, so a tiny FAT image is all we need.

echo "Creating cloud-init seed disk..."

# user-data: set the password, enable serial password login, ensure the kernel
# console and a login getty are wired to the platform serial port.
#
# This heredoc is intentionally unquoted so $SERIAL_TTY / $GUEST_* are expanded.
cat > user-data <<USERDATA
#cloud-config
hostname: ubuntu
chpasswd:
  expire: false
  users:
    - name: ${GUEST_USER}
      password: ${GUEST_PASSWORD}
      type: text
write_files:
  - path: /etc/default/grub.d/99-openvmm-serial.cfg
    permissions: '0644'
    content: |
      GRUB_CMDLINE_LINUX_DEFAULT="console=tty1 console=${SERIAL_TTY},115200"
runcmd:
  - [ systemctl, enable, --now, "serial-getty@${SERIAL_TTY}.service" ]
  - [ update-grub ]
USERDATA

cat > meta-data <<METADATA
instance-id: openvmm-ubuntu
local-hostname: ubuntu
METADATA

# mkfs.vfat (dosfstools) needs the glibc CP850 gconv module to encode the FAT
# volume label. Minimal distros like Azure Linux omit it, which makes mkfs.fat
# abort with "Error setting code page" and leaves an unusable image. Warn early
# with the fix rather than letting the failure surface as a cryptic mcopy error.
if ! iconv -f CP850 -t UTF-8 </dev/null &>/dev/null; then
    echo "ERROR: the CP850 codepage is unavailable, so mkfs.vfat cannot create" >&2
    echo "       the cloud-init disk. Install the glibc gconv codepage modules:" >&2
    echo "         sudo tdnf install glibc-iconv         # Azure Linux" >&2
    echo "       (On Debian/Ubuntu these ship in libc6 and are always present.)" >&2
    exit 1
fi

truncate -s 1M cidata.img
mkfs.vfat -n cidata cidata.img >/dev/null
mcopy -i cidata.img user-data ::user-data
mcopy -i cidata.img meta-data ::meta-data

# --- Done ---

ABSDIR="$(pwd)"

tee README <<EOF

Ubuntu ${UBUNTU_VERSION} UEFI boot setup for OpenVMM (${ARCH})

Files in ${ABSDIR}:
  disk.raw     - Root disk image (raw, grown to ${DISK_SIZE})
  cidata.img   - Cloud-init seed disk (sets the password & serial console)

To boot with OpenVMM (from the openvmm repo root):

  cargo run -p openvmm -- \\
    --uefi \\
    --com1 console \\
    --uefi-console-mode com1 \\
    --pcie-root-complex rc0 \\
    --pcie-root-port rc0:disk \\
    --pcie-root-port rc0:cidata \\
    --pcie-root-port rc0:net \\
    --nvme-pci id=nvme-disk,pcie_port=disk \\
    --disk file:${ABSDIR}/disk.raw,on=nvme-disk \\
    --virtio-blk file:${ABSDIR}/cidata.img,ro,pcie_port=cidata \\
    --virtio-net pcie_port=net:consomme \\
    --default-boot-always-attempt \\
    -m 2G \\
    -p 2 \\
    --hv

Notes:
  * The root disk is attached as NVMe on an emulated PCIe root complex. This
    is the most broadly compatible topology -- it is NOT something Ubuntu
    requires (Ubuntu boots fine from virtio-blk or VMBus SCSI too). NVMe is
    used because UEFI cannot yet boot from virtio-blk, and VMBus is not
    available on all targets (e.g. KVM/aarch64 has no VMBus support yet), so
    NVMe over PCIe is the one option that works everywhere.
  * The cloud-init seed disk is attached as virtio-blk rather than as a second
    NVMe disk to dodge an NVMe namespace ID conflict between two raw-file NVMe
    disks (a separate bug). This is fine because only the boot disk needs to
    be reachable by UEFI -- cloud-init runs after the kernel is up and can use
    the virtio-blk driver.
  * Networking is provided by a virtio-net device on the PCIe root complex
    (no VMBus NIC), backed by the user-mode 'consomme' NAT stack.
  * EFI diagnostics are enabled (INFO level) so COM1 shows firmware device
    enumeration and default-boot decisions.
  * Running via 'cargo run' picks up the mu_msvm UEFI firmware automatically
    from .cargo/config.toml (after 'cargo xflowey restore-packages'). If you
    run the openvmm binary directly, add:
      --uefi-firmware /path/to/MSVM.fd
  * The first boot runs cloud-init, which sets the password and enables the
    serial getty. The login prompt appears once cloud-init finishes (this can
    take up to a minute on the very first boot while the rootfs is resized).
  * The guest serial console is ${SERIAL_TTY}, routed to COM1.

Login: ${GUEST_USER} / ${GUEST_PASSWORD}
Quit:  ctrl-q then q
EOF
