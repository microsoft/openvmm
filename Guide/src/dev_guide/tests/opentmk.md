# OpenTMK

`opentmk` is a configurable UEFI Test Microkernel payload for low-level Hyper-V
and OpenHCL test scenarios.

## Distinction from `simple_tmk`

The repository contains two separate microkernel systems:

| System | Image | Selection model | Executor |
| --- | --- | --- | --- |
| `tmk/simple_tmk` | Bare-metal ELF | Static `tmk_tests` descriptors | Host `tmk_vmm` process |
| `opentmk` | UEFI executable in a VHD | Embedded JSON configuration | Payload dispatches its own test |

Use OpenTMK when the test needs its UEFI-based runtime, Hyper-V test contexts,
multiple VTLs, or a configuration patched into the boot image. Use `simple_tmk` 
for small tests that should run through the multi-backend `tmk_vmm` executor.

## Build and package

The supported local entry point builds the UEFI executable and packages it in
a bootable VHD:

```bash
cargo xflowey build-opentmk
```

## Embedded configuration

OpenTMK contains a fixed configuration region that host tooling can locate and
patch without relinking the UEFI executable. Supply a JSON selection file to
the build pipeline:

```bash
cargo xflowey build-opentmk x86-64 \
  --config path/to/test-config.json
```

The configuration identifies the backend and test to dispatch and carries
test-specific parameters. The exact schema follows `opentmk_protocol`; use
configurations from the test being developed as the starting point.

If `--config` is omitted, the VHD contains the unconfigured OpenTMK executable.
That artifact is useful as an intermediate build output but cannot select a
test until a harness patches it.

## Guest lifecycle

When firmware launches the image, OpenTMK:

1. Initializes UEFI services and the OpenTMK runtime.
2. Reads and validates the embedded configuration region.
3. Selects the registered backend.
4. Dispatches the configured test with its parameters.
5. Reports progress through its logging and assertion infrastructure.

## Running the VHD

The Flowey output is a guest disk. Attach it to a UEFI VM using the environment
required by the selected test. OpenTMK itself has no host command line.

A generic OpenVMM launch has this shape:

```bash
cargo run -p openvmm -- --uefi \
  --vmbus-scsi id=scsi0 \
  --disk memdiff:path/to/opentmk.vhd,on=scsi0
```

Most OpenTMK tests require additional Hyper-V or OpenHCL configuration. Copy
the VM setup from the owning test rather than assuming the generic launch is
sufficient.

## Failures and diagnostics

An unknown backend or test indicates a mismatch between the JSON configuration
and the payload's registered dispatch table. A missing or malformed config
usually fails before test code starts.

Capture the guest serial output from the beginning of boot. It distinguishes
UEFI/runtime initialization failures from assertions inside the selected test.
Because the payload runs without a general-purpose OS, there is no Pipette or
shell to inspect after a fatal failure.
