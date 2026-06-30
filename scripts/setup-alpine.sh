#!/bin/bash
# Setup script for running Alpine Linux on OpenVMM via direct boot.
# Downloads and prepares an Alpine cloud image, extracts the kernel and
# initramfs, and creates a cloud-init data disk for login credentials.
#
# Usage:
#   ./setup-alpine.sh [output-dir]
#
# Architecture is auto-detected from the host (uname -m) and can be
# overridden by setting the ARCH environment variable to x86_64 or aarch64,
# e.g.:
#   ARCH=aarch64 ./setup-alpine.sh
#
# Default output directory: ./alpine-direct-boot

set -euo pipefail

# --- Resolve target architecture ---

ARCH="${ARCH:-$(uname -m)}"
case "$ARCH" in
    x86_64|amd64)
        ARCH="x86_64"
        ;;
    aarch64|arm64)
        ARCH="aarch64"
        ;;
    *)
        echo "ERROR: Unsupported architecture: $ARCH (expected x86_64 or aarch64)" >&2
        exit 1
        ;;
esac

ALPINE_VERSION="3.21"
ALPINE_RELEASE="3.21.6"
IMAGE_NAME="nocloud_alpine-${ALPINE_RELEASE}-${ARCH}-uefi-tiny-r0"
IMAGE_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/cloud/${IMAGE_NAME}.qcow2"

# Final kernel filename. On aarch64 the kernel is decompressed to a raw Linux
# `Image`, so name it accordingly; on x86_64 the compressed `vmlinuz-virt` is
# booted as-is.
if [ "$ARCH" = "aarch64" ]; then
    KERNEL="Image"
    KERNEL_DESC="Kernel (raw uncompressed arm64 Image)"
else
    KERNEL="vmlinuz-virt"
    KERNEL_DESC="Kernel (compressed bzImage)"
fi

OUTDIR="${1:-./alpine-direct-boot}"

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

# --- Step 3b: Decompress the kernel on aarch64 ---
# OpenVMM's arm64 loader requires a raw, uncompressed Linux `Image` (it checks
# the ARM64 magic in the image header). Alpine ships a compressed vmlinuz-virt,
# which may be wrapped in an EFI/PE stub and use any of several compression
# formats (gzip, zstd, lz4, xz, ...). Scan the file for the first embedded
# compressed payload and decompress it, the same way the kernel's own
# scripts/extract-vmlinux tool works. On x86_64 the bzImage carries its own
# decompressor and is loaded as-is, so this step is skipped.
if [ "$ARCH" = "aarch64" ]; then
    # Returns 0 if the file's ARM64 Image magic ("ARM\x64" at offset 56) is
    # present, i.e. it is already a raw, uncompressed kernel Image.
    is_arm64_image() {
        [ "$(dd if="$1" bs=1 skip=56 count=4 2>/dev/null)" = "ARMd" ]
    }

    # Map a kernel compression-type name to a decompressor command.
    decompressor_for() {
        case "$1" in
            gzip)        echo "gzip -dc" ;;
            lzma)        echo "xz -dc --format=lzma" ;;
            xzkern|xz)   echo "xz -dc" ;;
            lzo)         echo "lzop -dc" ;;
            lz4)         echo "lz4 -dc" ;;
            zstd22|zstd) echo "zstd -dc" ;;
            *)           return 1 ;;
        esac
    }

    # Read a little-endian u32 at the given byte offset.
    le_u32() {
        od -An -tu4 -j "$2" -N4 "$1" 2>/dev/null | tr -d ' '
    }

    # Extract the payload from an EFI zboot image (PE "MZ" stub + "zimg" magic
    # at offset 4). The header stores the payload offset (u32 @ 8), payload
    # size (u32 @ 12), and a NUL-terminated compression-type string (@ 24).
    extract_zboot() {
        local img="$1" out="$2" off size comp cmd
        [ "$(dd if="$img" bs=1 skip=4 count=4 2>/dev/null)" = "zimg" ] || return 1
        off=$(le_u32 "$img" 8)
        size=$(le_u32 "$img" 12)
        # comp_type is a NUL-terminated string; take only the text before the
        # first NUL (translate NULs to newlines and keep the first line).
        comp=$(dd if="$img" bs=1 skip=24 count=36 2>/dev/null | tr '\0' '\n' | head -n1)
        [ -n "$off" ] && [ -n "$size" ] || return 1
        cmd=$(decompressor_for "$comp") || {
            echo "ERROR: unsupported zboot compression '$comp'." >&2
            return 1
        }
        command -v "${cmd%% *}" >/dev/null 2>&1 || {
            echo "ERROR: '$comp' kernel needs '${cmd%% *}' to decompress; install it." >&2
            return 1
        }
        echo "Detected EFI zboot image (compression: $comp)."
        dd if="$img" bs=1 skip="$off" count="$size" 2>/dev/null | $cmd > "$out" 2>/dev/null
        [ -s "$out" ] && is_arm64_image "$out"
    }

    # Fallback: scan for the first known compression magic and decompress from
    # there, the same way the kernel's scripts/extract-vmlinux tool works.
    extract_kernel() {
        local img="$1" out="$2" spec magic cmd pos
        for spec in \
            '\037\213\010|gzip -dc' \
            '\3757zXZ\000|xz -dc' \
            'BZh|bzip2 -dc' \
            '\002\041\114\030|lz4 -dc' \
            '\050\265\057\375|zstd -dc' \
            '\211LZO|lzop -dc'; do
            magic="${spec%%|*}"
            cmd="${spec#*|}"
            command -v "${cmd%% *}" >/dev/null 2>&1 || continue
            pos=$(LC_ALL=C grep -a -b -o -P "$(printf '%b' "$magic")" "$img" 2>/dev/null \
                  | head -n1 | cut -d: -f1 || true)
            [ -n "${pos:-}" ] || continue
            if tail -c "+$((pos + 1))" "$img" 2>/dev/null | $cmd > "$out" 2>/dev/null \
               && [ -s "$out" ] && is_arm64_image "$out"; then
                return 0
            fi
        done
        return 1
    }

    echo "Decompressing aarch64 kernel to a raw Image..."
    if extract_zboot vmlinuz-virt Image || extract_kernel vmlinuz-virt Image; then
        rm -f vmlinuz-virt
    else
        rm -f Image
        echo "ERROR: could not decompress the aarch64 kernel to a raw Image." >&2
        echo "       Inspect it with 'file vmlinuz-virt' and decompress the" >&2
        echo "       embedded payload manually (see scripts/extract-vmlinux" >&2
        echo "       in the Linux source tree)." >&2
        exit 1
    fi
fi

# --- Step 4: Create cloud-init data disk ---
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

Alpine Linux direct boot setup for OpenVMM (${ARCH})

Files in ${ABSDIR}:
  $(printf '%-14s' "${KERNEL}") - ${KERNEL_DESC}
  initramfs-virt - Initial ramdisk
  disk.raw       - Root disk image (raw)
  cidata.img     - Cloud-init data disk (sets root password)

To boot with OpenVMM (from the openvmm repo root):

  cargo run -p openvmm -- \\
    -k ${ABSDIR}/${KERNEL} \\
    -r ${ABSDIR}/initramfs-virt \\
    --pcie-root-complex rc0 \\
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
