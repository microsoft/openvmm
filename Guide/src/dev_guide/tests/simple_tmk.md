# simple_tmk

`simple_tmk` is the bare-metal guest payload containing the repository's basic
Test Microkernel processor and interrupt tests.

## Runtime model

The payload is a `no_std`, `minimal_rt` executable. It has no bootloader,
firmware interface, process model, or command line. `tmk_vmm` loads its ELF
directly, initializes the processor, and selects one test by index.

The shared `tmk_core` runtime provides:

- A test entry point and panic handler.
- Guest-to-host logging and completion commands.
- Scoped interrupt-handler registration.
- Architecture-specific helpers such as x86 MSR access.

The payload is useful for code that must touch processor state directly. Use a
normal VMM test when the behavior requires firmware, devices, or a guest OS.

## Building

Build the x86_64 payload with the same command emitted by Petri's artifact
resolver:

```bash
RUSTC_BOOTSTRAP=1 cargo build -p simple_tmk \
  --config openhcl/minimal_rt/x86_64-config.toml
```

Build AArch64 with:

```bash
RUSTC_BOOTSTRAP=1 cargo build -p simple_tmk \
  --config openhcl/minimal_rt/aarch64-config.toml
```

The output is an ELF consumed by `tmk_vmm`. Running it as a host process is not
supported; non-`minimal_rt` builds contain only a rejecting stub.

Flowey builds the correct payload automatically when selected Petri tests
require a `SIMPLE_TMK_*` artifact.

## Defining a test

A test is a function annotated with `#[tmk_test]`:

```rust,ignore
#[tmk_test]
fn example(_: TestContext<'_>) {
    log!("example started");
    assert_eq!(2 + 2, 4);
}
```

The macro emits a static descriptor into the ELF `tmk_tests` section. The
descriptor gives `tmk_vmm` the test name, entry point, and flags needed for
discovery and execution.

Tests that intentionally fault can be marked as expected failures:

```rust,ignore
#[tmk_test(expected_failure, linux_only)]
fn expected_abort(_: TestContext<'_>) {
    // Trigger the architecture condition under test.
}
```

## Scoped interrupt handlers

On x86_64, `TestContext` exposes a `Scope`. A test can create a nested scope,
install an ISR for a vector, enable interrupts, and let the scope restore its
handler state before returning. This keeps one test's interrupt setup from
leaking into another test implementation.

## Running and listing

After building both binaries:

```bash
target/debug/tmk_vmm --tmk \
  target/x86_64-unknown-none/debug/simple_tmk --list
```

Run one test by passing its discovered name:

```bash
target/debug/tmk_vmm --tmk \
  target/x86_64-unknown-none/debug/simple_tmk ud2
```

Output appears in the `tmk_vmm` tracing stream. A panic becomes a structured
protocol message when the runtime remains operational; an earlier processor
fault is reported from the host VP loop.

## Adding coverage safely

When adding a test:

1. Keep the initial machine assumptions explicit.
2. Install handlers before enabling or triggering interrupts.
3. Bound polling loops so a missing hardware event becomes a failure.
4. Restore mutable processor state when later operations in the same test need
   a known configuration.
5. Run through every backend whose behavior the test claims to cover.

Tests share the image but receive fresh VM state from `tmk_vmm`, so global
state does not need to survive between test cases.

```admonish note title="See also"
[TMK VMM](tmk_vmm.md) documents the host executor, hypervisor backends, and
result protocol.
```
