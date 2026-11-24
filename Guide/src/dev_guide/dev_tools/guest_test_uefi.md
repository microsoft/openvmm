# guest_test_uefi

`guest_test_uefi` is a minimal `no_std` + `alloc` UEFI application for
OpenVMM-specific bare-metal guest tests.

The application runs without a general-purpose guest OS, so tests can exercise
firmware variables, watchdog behavior, MMIO, and port I/O with little
intervening software.

## Runtime and action selection

Firmware launches the EFI executable from a boot disk. On startup, the
application reads the `PetriBootAction` UEFI variable from the global variable
namespace.

The current actions are:

| Value | Behavior |
| --- | --- |
| Missing, unreadable, or unknown | Run the normal UEFI test suite |
| `hibernate` | Request platform hibernation and do not return |

Petri seeds this variable through the VM's UEFI NVRAM state when a test needs a
non-default action. The default makes a manually booted image run its ordinary
test suite without additional configuration.

## Current tests

The default suite runs tests in a fixed order:

1. Allocate and print a heap-backed string, validating global allocation.
2. Read the Secure Boot `db` and `dbDefault` variables and compare their data.
3. Arm the UEFI watchdog for five seconds and wait long enough for it to reset
  the VM.

The watchdog test runs last because success destroys the current VM execution.
A write-protection test for `dbDefault` exists but is disabled until the
firmware behavior it depends on is available.

The application draws changing splash values between tests and writes progress
to the UEFI console. Capture both console and VM lifecycle events when
diagnosing a failure.

## Building and running

`guest_test_uefi` must be built for `*-unknown-uefi` targets. These are not
installed by default, so you'll need to install the correct target via `rustup`.
For example:

```bash
rustup target add x86_64-unknown-uefi
```

Since this code runs in the guest, package the built `.efi` binary into a disk
image that UEFI can read.

To streamline the process of obtaining such a disk image, `cargo xtask` includes
a helper to generate properly formatted `.img` files containing a given `.efi`
image. e.g:

```bash
# build the UEFI test application
cargo build -p guest_test_uefi --target x86_64-unknown-uefi
# create the disk image
cargo xtask guest-test uefi \
  --bootx64 target/x86_64-unknown-uefi/debug/guest_test_uefi.efi
# test in OpenVMM
cargo run -- --uefi --gfx --hv --processors 1 \
  --vmbus-scsi id=scsi0 \
  --disk memdiff:target/x86_64-unknown-uefi/debug/guest_test_uefi.img,on=scsi0
```

The output is a generic UEFI boot image and can also run under Hyper-V, QEMU,
or another VMM with compatible UEFI firmware.

```admonish note
When launching a prebuilt `openvmm` executable instead of `cargo run`, provide
the UEFI firmware path explicitly. The Cargo environment normally supplies it
from restored packages.
```

## Selecting hibernation in Petri

Petri tests should use the guest-action seeding helper rather than manually
editing the VMGS. The helper writes `PetriBootAction=hibernate` into the custom
UEFI NVRAM delta before boot and then observes the VM's hibernation behavior.

Manual launches run the default suite unless you prepare equivalent NVRAM
state.

## Converting the image

To convert the raw `.img` into other formats, `qemu-img` is very helpful:

```bash
# Adjust for the target architecture and build profile.
IMAGE_DIR=target/x86_64-unknown-uefi/debug
# VMware
qemu-img convert -f raw -O vmdk \
  ${IMAGE_DIR}/guest_test_uefi.img ${IMAGE_DIR}/guest_test_uefi.vmdk
# Hyper-V
qemu-img convert -f raw -O vhdx \
  ${IMAGE_DIR}/guest_test_uefi.img ${IMAGE_DIR}/guest_test_uefi.vhdx
```

## Troubleshooting

- If firmware does not find the application, verify the target architecture
  and the `--bootx64` or corresponding image-builder input.
- If `db` or `dbDefault` is missing, use firmware containing the expected
  Secure Boot variable defaults.
- If the watchdog test returns and panics, the firmware did not honor the
  watchdog reset request.
- If Petri performs the wrong action, inspect the custom UEFI NVRAM delta and
  confirm the variable name, namespace, and UTF-8 value.
- If no output appears, attach a graphical or serial console supported by the
  selected firmware and inspect early firmware boot logs.
