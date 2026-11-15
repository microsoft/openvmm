# TMK VMM

`tmk_vmm` is a small host-side VMM that discovers and executes tests from a
Test Microkernel ELF image.

## Why use a TMK

A full OpenVMM VM includes firmware, an operating system, a broad device model,
and guest-control infrastructure. That is useful for integration coverage but
makes a poor fixture for a focused processor or hypervisor-interface test.

`tmk_vmm` creates only the VM state needed by the Test Microkernel:

- A single virtual processor.
- 4 MiB of guest RAM.
- Architecture-specific initial register state.
- A fixed MMIO command address for guest-to-host reporting.

This keeps failures close to the hypervisor behavior under test.
`tmk_vmm` does not support general-purpose VMs.

## Host and guest split

```text
tmk_vmm host process
  |- parse the TMK ELF
  |- enumerate test descriptors
  |- create a minimal partition and VP
  |- handle guest MMIO commands
  `- report test results

simple_tmk guest image
  |- initialize the minimal runtime
  |- invoke one selected test
  `- send log, panic, or completion commands
```

The host reloads the TMK and creates fresh execution state for each selected
test.

## Building the executor

Build a native executor with Cargo:

```bash
cargo build -p tmk_vmm
```

The available hypervisor backend is determined by the build platform and guest
architecture. Flowey builds additional native and static Linux variants for
VMM-test artifacts.

## Running a TMK

List tests without creating a VM:

```bash
target/debug/tmk_vmm --tmk path/to/simple_tmk --list
```

Run every test using the automatically selected hypervisor:

```bash
target/debug/tmk_vmm --tmk path/to/simple_tmk
```

Pass test names as positional arguments to run a subset:

```bash
target/debug/tmk_vmm --tmk path/to/simple_tmk boot apic_timer
```

## ELF test discovery

The input is a 64-bit ELF containing a `tmk_tests` section. Each descriptor
stores:

- A pointer and length for the UTF-8 test name.
- The guest entry point.
- An `expected_failure` flag.
- A `linux_only` flag.

`tmk_vmm --list` parses and relocates this metadata without entering the guest.
When running a test, the loader maps the ELF, prepares page tables and initial
registers, and passes a `StartInput` structure to the TMK.

## Result protocol

The host reserves MMIO address `0xffff0000`. The guest writes a pointer to a
`tmk_protocol::Command` at that address. Commands can:

- Log a UTF-8 string held in guest memory.
- Report a panic with message, file, and line.
- Complete explicitly with a success value.

If the VP halts or faults before completion, `tmk_vmm` reports the halt reason
and available register state. A normal failure makes the process exit nonzero.
For a descriptor marked `expected_failure`, a failure or fault is success and a
normal pass is an error.

## Troubleshooting

- `no hypervisor available` means no compiled native backend passed its
  availability check. Verify host permissions or pass `--hv` explicitly.
- An empty `--list` result usually means the image was not built with the TMK
  linker configuration or contains no registered tests.
- A protocol error indicates that guest pointers or command contents were not
  valid for the mapped memory.
- A `Faulted` result includes processor state; use it before adding logging to
  code that may fail before the command channel works.

```admonish note title="See also"
[simple_tmk](simple_tmk.md) describes the guest payload and test authoring
model.
```
