# Test microkernels

## What TMKs are

During the lifetime of a VM, the guest operating system might request services that
require assistance from the VMM or the hypervisor. In turn, that rests on the correct
implementation of several concepts like interrupt processing, memory mappings,
maintaining caches of the (virtual) hardware, etc. that are both critical for the normal
work of the guest OS, and are hard to test. That is the case due to the guest OSes not
allowing or even breaking down should any unexpected interaction happen with these
low-level building blocks. Let alone it is not very prudent to incorporate a
test for a VMM or a hypervisor into a guest kernel and release the kernel that way!

OpenVMM includes a test harness that allows running TMKs (test microkernels) - the tests
which are built and work just like guest kernels: no standard library is available and
they have access to all (virtual) hardware. The code runs in an environment that normally
predates running a full-fledged modern kernel so the tests can work with the hardware
directly without the imposition of any specific kernel architecture.

The TMKs use a specific protocol geared towards testing with a VMM. Nonetheless, you
can use TMKs as a starting point in learning how the hardware works and (with some
effort) can run some bare-metal.

## What TMKs solve

TMKs allow testing the primitives the guest kernels rely on in a minimal, unencumbered
setting. The value of adding a TMK is to make sure that the most fundamental building blocks
on which rests the entirety of other code are implemented correctly, and the guest kernel
will run correctly.

## How to run a TMK

```admonish note
The command lines and other specifics may change as the TMKs are under heavy development.
```

1. Build it:

```sh
cargo build -p simple_tmk --config openhcl/minimal_rt/x86_64-config.toml --release
# The aarch64-unknown-none target doesn't support static PIEs at the moment.
cargo +nightly build -p simple_tmk --config openhcl/minimal_rt/aarch64-config.toml \
    --target openhcl/minimal_rt/aarch64-minimal_rt-none.json --release
```

2. See the list of tests:

```sh
cargo run -p tmk_vmm -- --tmk target/x86_64-unknown-none/release/simple_tmk --list
cargo run -p tmk_vmm -- --tmk target/aarch64-minimal_rt-none/release/simple_tmk --list
```

```console
common::boot
x86_64::apic::enable_x2apic
x86_64::apic::self_ipi
x86_64::ud2
```

3. Choose a specific test, or run all (the default):

```sh
# With KVM (on a Linux distro)
cargo run -p tmk_vmm -- --tmk target/x86_64-unknown-none/release/simple_tmk --hv kvm
# With HVF (macOS)
cargo run -p tmk_vmm -- --tmk target/aarch64-minimal_rt-none/release/simple_tmk --hv hvf
# With WHP (Windows)
cargo run -p tmk_vmm -- --tmk target/aarch64-minimal_rt-none/release/simple_tmk --hv whp
```

```console
2025-04-15T17:09:58.227718Z  INFO test: test started, name: "common::boot"
2025-04-15T17:09:58.237776Z  INFO tmk: hello world
2025-04-15T17:09:58.238365Z  INFO test: test passed, name: "common::boot"
2025-04-15T17:09:58.238413Z  INFO test: test started, name: "x86_64::apic::enable_x2apic"
2025-04-15T17:09:58.241427Z  INFO tmk: apic base: 0xfee00900 ApicBase {
  bsp: true,
  x2apic: false,
  enable: true,
  base_page: 0xfee00,
}
```

```console
2025-04-15T17:09:58.241531Z ERROR tmk: location: "tmk/simple_tmk/src/x86_64/apic.rs:27", panic: "called `Result::unwrap_err()` on an `Ok` value: ()"
2025-04-15T17:09:58.241835Z  INFO test: test failed, name: "x86_64::apic::enable_x2apic", reason: "explicit failure"
2025-04-15T17:09:58.241883Z  INFO test: test started, name: "x86_64::apic::self_ipi"
2025-04-15T17:09:58.244164Z ERROR tmk: location: "tmk/simple_tmk/src/x86_64/apic.rs:79", panic: "assertion failed: got_interrupt.load(Relaxed)"
2025-04-15T17:09:58.244461Z  INFO test: test failed, name: "x86_64::apic::self_ipi", reason: "explicit failure"
2025-04-15T17:09:58.244520Z  INFO test: test started, name: "x86_64::ud2"
2025-04-15T17:09:58.246944Z ERROR tmk: location: "tmk/simple_tmk/src/x86_64/mod.rs:28", panic: "assertion failed: recovered.load(Relaxed)"
2025-04-15T17:09:58.247228Z  INFO test: test failed, name: "x86_64::ud2", reason: "explicit failure"
```
