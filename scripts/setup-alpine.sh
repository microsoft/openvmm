#!/bin/bash
# Setup script for running Alpine Linux on OpenVMM via direct boot.
# Downloads and prepares an Alpine cloud image, extracts the kernel and
# initramfs, and creates a cloud-init data disk for login credentials.
#
# Usage:
#   ./setup-alpine.sh [output-dir]
#
# Default output directory: ./alpine-direct-boot

set -euo pipefail

ALPINE_VERSION="3.21"
ALPINE_RELEASE="3.21.6"
IMAGE_NAME="nocloud_alpine-${ALPINE_RELEASE}-x86_64-uefi-tiny-r0"
IMAGE_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/cloud/${IMAGE_NAME}.qcow2"

OUTDIR="${1:-./alpine-direct-boot}"

# --- Check required tools ---

missing=()
for tool in curl qemu-img python3 mkfs.vfat mcopy; do
    if ! command -v "$tool" &>/dev/null; then
        missing+=("$tool")
    fi
done

if [ ${#missing[@]} -gt 0 ]; then
    echo "ERROR: Missing required tools: ${missing[*]}" >&2
    echo "" >&2
    echo "Install them with your package manager, e.g.:" >&2
    echo "  sudo apt install curl qemu-utils python3 dosfstools mtools" >&2
    echo "  sudo dnf install curl qemu-img python3 dosfstools mtools" >&2
    exit 1
fi

mkdir -p "$OUTDIR"
cd "$OUTDIR"

# --- Step 1: Download the Alpine cloud image ---

if [ ! -f "${IMAGE_NAME}.qcow2" ]; then
    echo "Downloading Alpine ${ALPINE_RELEASE} cloud image..."
    curl --fail -Lo "${IMAGE_NAME}.qcow2" "$IMAGE_URL"
else
    echo "Alpine cloud image already downloaded."
fi

# --- Step 2: Convert qcow2 to raw ---

echo "Converting qcow2 to raw..."
qemu-img convert -f qcow2 -O raw "${IMAGE_NAME}.qcow2" disk.raw

# --- Step 3: Extract kernel and initramfs from partition 2 ---
# The raw image has a GPT partition table:
#   Partition 1: 512K EFI system partition (offset 512)
#   Partition 2: Linux ext4 root filesystem (offset 1048576 = sector 2048 * 512)

echo "Extracting kernel and initramfs from disk image..."
MNT=$(mktemp -d)
cleanup() {
    if mountpoint -q "$MNT" 2>/dev/null; then
        sudo umount "$MNT"
    fi
    rmdir "$MNT" 2>/dev/null || true
}
trap cleanup EXIT
sudo mount -o loop,offset=1048576,ro disk.raw "$MNT"
sudo cp "$MNT/boot/vmlinuz-virt" vmlinuz-virt
sudo cp "$MNT/boot/initramfs-virt" initramfs-virt
sudo umount "$MNT"
rmdir "$MNT"
trap - EXIT
sudo chown "$(id -u):$(id -g)" vmlinuz-virt initramfs-virt
chmod 644 vmlinuz-virt initramfs-virt

# --- Step 4: Extract ELF kernel from bzImage ---
# OpenVMM's direct boot loader requires an uncompressed ELF kernel (vmlinux),
# not a compressed bzImage.

echo "Extracting ELF kernel from bzImage..."
python3 -c "
import zlib
data = open('vmlinuz-virt', 'rb').read()
i = 0
while i < len(data) - 1:
    if data[i:i+2] == b'\x1f\x8b':
        try:
            d = zlib.decompressobj(16 + zlib.MAX_WBITS)
            elf = d.decompress(data[i:])
            if elf[:4] == b'\x7fELF':
                open('vmlinux-virt', 'wb').write(elf)
                print(f'  Extracted ELF kernel ({len(elf)} bytes) from offset {hex(i)}')
                break
        except Exception:
            pass
    i += 1
else:
    raise SystemExit('ERROR: no embedded ELF kernel found in vmlinuz-virt')
"

# --- Step 5: Create cloud-init data disk ---
# The Alpine cloud image ships with all accounts locked. This creates a small
# FAT disk with cloud-init config that sets the root password on first boot.

echo "Creating cloud-init data disk..."
cat > user-data <<'USERDATA'
#cloud-config
runcmd:
  - echo 'root:alpine' | chpasswd
  - grep -q hvc0 /etc/inittab || echo 'hvc0::respawn:/sbin/getty 115200 hvc0' >> /etc/inittab
  - kill -HUP 1
USERDATA

cat > meta-data <<'METADATA'
instance-id: openvmm-alpine
local-hostname: alpine
METADATA

truncate -s 1M cidata.img
mkfs.vfat -n cidata cidata.img >/dev/null
mcopy -i cidata.img user-data ::user-data
mcopy -i cidata.img meta-data ::meta-data

# --- Done ---

ABSDIR="$(pwd)"

tee README <<EOF

Alpine Linux direct boot setup for OpenVMM

Files in ${ABSDIR}:
  vmlinux-virt   - Uncompressed ELF kernel
  initramfs-virt - Initial ramdisk
  disk.raw       - Root disk image (raw)
  cidata.img     - Cloud-init data disk (sets root password)

To boot with OpenVMM (from the openvmm repo root):

  cargo run -p openvmm -- \\
    -k ${ABSDIR}/vmlinux-virt \\
    -r ${ABSDIR}/initramfs-virt \\
    --pcie-root-complex rc0,segment=0,start_bus=0,end_bus=255,low_mmio=4M,high_mmio=1G \\
    --pcie-root-port rc0:disk \\
    --pcie-root-port rc0:cidata \\
    --pcie-root-port rc0:net \\
    --pcie-root-port rc0:console \\
    --virtio-blk file:${ABSDIR}/disk.raw,pcie_port=disk \\
    --virtio-blk file:${ABSDIR}/cidata.img,ro,pcie_port=cidata \\
    --virtio-net pcie_port=net:consomme \\
    --com1 none \\
    --virtio-console console --virtio-console-pcie-port console \\
    -c "root=/dev/vda2 rootfstype=ext4 modules=virtio_pci,virtio_blk,ext4" \\
    -m 512M \\
    -p 2 \\
    --hv

Login: root / alpine
Quit:  ctrl-q then q
EOF
