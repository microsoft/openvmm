# openvmm_hcl

`openvmm_hcl` is the main user-mode VMM and management process inside the
OpenHCL Linux VTL2 environment.

## Position in the boot flow

OpenHCL reaches `openvmm_hcl` after several earlier stages:

```text
host loads OpenHCL IGVM into VTL2
  `- openhcl_boot prepares memory, CPUs, and Linux boot data
       `- Linux kernel boots the initrd
            `- underhill_init prepares userspace
                 `- exec /bin/openvmm_hcl
```

The executable is not the host-side `openvmm` CLI. It runs inside
the paravisor and manages the lower-trust VTL0 guest from VTL2.

## Startup responsibilities

After Linux userspace starts, `openvmm_hcl` performs these broad tasks:

1. Read VTL2 configuration and topology from the device tree and Linux
   interfaces established by the boot loader.
2. Initialize host communication, diagnostics, tracing, and servicing control.
3. Resolve the device and backend resources compiled into the image.
4. Start the VM worker that owns the VTL0 processor and device data path.
5. Start separate workers for devices whose isolation or lifecycle requires a
   process boundary.
6. Coordinate pause, resume, save, restore, servicing, restart, and shutdown.

The process remains the policy and control-plane owner while worker processes
perform high-volume VM and device work.

## Worker model

The VM worker runs VTL0 virtual processors, handles exits, and coordinates the
guest device model. Selected devices, including supported TPM configurations,
can run in separate worker processes. OpenHCL communicates with those workers
through typed channels rather than sharing arbitrary process state.

This process model allows a worker to be restarted or serviced while the main
paravisor process retains policy and host communication state. It also limits
which process directly handles sensitive device operations.

## Trust boundaries

OpenHCL does not trust the VTL0 guest. In a confidential VM, it also cannot
blindly trust host-provided configuration. The measured IGVM and boot loader
establish which runtime inputs are acceptable before `openvmm_hcl` starts.

At runtime:

- Guest protocol and device requests are parsed as untrusted input.
- Confidential diagnostics omit or reject operations that could expose guest
  secrets.
- Host-directed actions are constrained by the image's measured policy and
  isolation mode.
- Repeated guest-triggerable failures use bounded or rate-limited diagnostics.

Debug and confidential-debug images intentionally expose more diagnostic
surface than production confidential images. Do not treat them as equivalent
security configurations.

## Build and package

The supported path builds the whole firmware image:

```bash
cargo xflowey build-igvm x64
```

The recipe determines the target, features, kernel, initrd contents, boot
loader, sidecar, and manifest. Output is placed below:

```text
flowey-out/artifacts/build-igvm/<PROFILE>/<RECIPE>/openhcl-<RECIPE>.bin
```

To experiment with a custom user-mode binary, build the target selected by the
recipe and pass it back to Flowey. For example:

```bash
cargo build --target x86_64-unknown-linux-musl \
  -p openvmm_hcl --features <FEATURES>

cargo xflowey build-igvm x64 \
  --custom-openvmm-hcl \
  target/x86_64-unknown-linux-musl/debug/openvmm_hcl
```

```admonish warning
A standalone `openvmm_hcl` file is not a bootable product. It expects the
OpenHCL kernel, initrd, device tree, memory layout, and measured configuration
provided by a matching IGVM recipe.
```

The crate only runs on Linux. Non-Linux workspace builds contain an
unsupported-platform stub.

## Diagnostics

The OpenHCL diagnostics server exposes information and development operations
owned by `openvmm_hcl` and its workers. Common sources include:

- Kernel and OpenHCL logs through `kmsg` or configured serial output.
- The inspect tree for build identity, control state, workers, devices, and
  tracing.
- Core dumps and saved-state dumps where isolation policy permits them.
- Performance, packet-capture, and memory-profile traces in supported builds.

Use `ohcldiag-dev` interactively. Its CLI and inspect paths are explicitly
unstable and confidential images restrict the available data.

## Failure and restart behavior

A fatal failure in the main process ends the paravisor control plane. A worker
failure can sometimes be diagnosed or restarted through the main process,
depending on the worker and VM state. The `ohcldiag-dev restart` command targets
the VM worker while keeping VTL0 running where the runtime supports that
transition.

For startup failures:

1. Capture the VTL2 kernel and OpenHCL log from the beginning of boot.
2. Verify that the IGVM recipe contains the expected custom binary and initrd.
3. Inspect `/etc/underhill-build-info.json` through a working diagnostic shell
   to confirm the running revision.
4. Check the device tree and command line produced by `openhcl_boot`.
5. Reproduce with an unmodified recipe before attributing the failure to a
   worker or host interface.

```admonish note title="See also"
[OpenHCL boot flow](boot.md) describes the stages before this process starts.
[Processes and components](processes.md) shows the complete process model.
[ohcldiag-dev](../../openhcl/diag/ohcldiag_dev.md) documents the development
diagnostic client.
```
