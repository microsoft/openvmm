# Incubator

Incubator runs cross-compiled test executables inside a QEMU-emulated Linux
environment when the host cannot provide the required architecture or hardware
model.

## When to use Incubator

Use Incubator for tests that need hardware behavior outside the normal OpenVMM
host test environment.

Ordinary VMM tests should continue to use `cargo xflowey vmm-tests-run`
without `--incubator`. QEMU TCG is significantly slower than native execution,
so Incubator is reserved for tests that need its emulated platform.

## Execution model

Incubator introduces an outer VM around the normal Petri test process:

```text
x64 Linux host
  `- Incubator process
       `- QEMU TCG AArch64 Linux VM (L1)
            |- Pipette agent
            `- VMM test executable
                 `- OpenVMM using KVM
                      `- test VM (L2)
```

The VMM test executable is cross-compiled for
`*-unknown-linux-musl`. It runs inside the QEMU VM and starts OpenVMM
there. OpenVMM then uses the emulated KVM interface to create the test VM.

Incubator is therefore not an OpenVMM backend. It is a Cargo target runner that
places the existing test executable in a machine capable of running it.

## Running the current Incubator tests

Run the AArch64 TCG test set from a Linux host:

```bash
cargo xflowey vmm-tests-run \
  --incubator \
  --target linux-aarch64-musl \
  --filter "test(aarch64_tcg)"
```

`--target` is required with `--incubator`.

To select a profile explicitly, pass either its short name or a path:

```bash
cargo xflowey vmm-tests-run \
  --incubator aarch64-tcg-pcie \
  --target linux-aarch64-musl \
  --filter "test(aarch64_tcg)"
```

## Direct invocation

The binary also has a direct CLI for debugging the runner itself:

```bash
cargo run -p incubator -- \
  --profile petri/incubator/profiles/aarch64-tcg-pcie.toml \
  --share path/to/shared-root \
  --map-command-path \
  path/to/shared-root/test-binary
```

Direct use requires a suitable kernel, initrd, Pipette binary, QEMU, and shared
test artifacts. Running through `cargo xflowey vmm-tests-run` is preferred
because Flowey resolves and connects those inputs automatically.

## Output and failures

Incubator writes a per-process serial log named
`incubator-serial.<PID>.log` under the test output directory. Guest command
stdout and stderr flow through Pipette to nextest.

When a run fails, check the serial log first. It distinguishes a guest boot or
Pipette startup failure from a failure in the nested VMM test itself. QEMU
stderr is also captured and reported when the process exits.
