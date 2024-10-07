# guest_test_uefi

`guest_test_uefi` is a minimal, no-fuss, `no_std` + `alloc` environment from
which to write unit tests that are able to exercise OpenVMM devices, without
having to deal with annoying things like "virtual memory" or "OS security
features".

Want to write to an arbitrary MMIO/PIO address? Go for it! It's just you, the
VM, and the (very unobtrusive) UEFI runtime!

## Building + Running

`guest_test_uefi` must be built for `XXX-unknown-uefi` targets. These are not
installed by default, so you'll need to install the correct target via `rustup`.
For example:

```bash
rustup target add x86_64-unknown-uefi
```

Since this code runs in the guest, the built `.efi` binary needs to get packaged
into a disk image that UEFI can read.

To streamline the process of obtaining such a disk image, `cargo xtask` includes
a helper to generate properly formatted `.img` files containing a given `.efi`
image. e.g:

```bash
# build the UEFI test application
cargo build -p guest_test_uefi --target x86_64-unknown-uefi
# create the disk image
cargo xtask guest-test uefi --bootx64 ./target/x86_64-unknown-uefi/debug/guest_test_uefi.efi
# test in OpenVMM
cargo run -- --uefi --gfx --hv --processors 1 --disk memdiff:./target/x86_64-unknown-uefi/debug/guest_test_uefi.img
```

Protip: this is a generic UEFI binary, and can be run outside of the HvLite repo
as well (e.g: in QEMU, Hyper-V, etc...)!

To convert the raw `.img` into other formats, `qemu-img` is very helpful:

```bash
# Adjust for the target architecture and type of the build
OVMM_UEFI_TEST_IMG_DIR=./target/x86_64-unknown-uefi/debug
# VmWare
qemu-img convert -f raw -O vmdk ${OVMM_UEFI_TEST_IMG_DIR}/guest_test_uefi.img ${OVMM_UEFI_TEST_IMG_DIR}/guest_test_uefi.vmdk
# Hyper-V
qemu-img convert -f raw -O vhdx  ${OVMM_UEFI_TEST_IMG_DIR}/guest_test_uefi.img ${OVMM_UEFI_TEST_IMG_DIR}/guest_test_uefi.vhdx
# The files:
ls -la $OVMM_UEFI_TEST_IMG_DIR/{*.img,*.vhdx,*.vmdk}
```
