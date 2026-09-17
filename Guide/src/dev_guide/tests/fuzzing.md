# Fuzzing in OpenVMM

Fuzzing infrastructure in OpenVMM is based on the excellent
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) project, which makes it super easy to get
up-and-running with fuzzing in Rust projects.

Under-the-hood, `cargo-fuzz` hooks into LLVM's
[libFuzzer](https://www.llvm.org/docs/LibFuzzer.html) to do the actual fuzzing.

```admonish important
Fuzzing only works on **Linux**. libfuzzer-sys doesn't support Windows.
On **aarch64**, set `RUSTFLAGS="-Ctarget-feature=+lse,+neon"` before any
cargo-fuzz command, or builds will fail with atomics errors.
```

OpenVMM fuzzers target several categories of code:

- **Chipset devices** (battery, CMOS/RTC, IDE) — PIO, MMIO, and PCI config
  interfaces exposed to guests
- **VMBus devices** (storvsp) — VMBus channel protocol and SCSI command
  processing
- **Driver stacks** (NVMe) — driver-side fuzzing against fuzzed device
  responses
- **Unsafe abstractions** (scsi_buffers, guestmem, sparse_mmap) — safe API
  surface over unsafe internals
- **Protocol parsers** (UEFI NVRAM, mesh ttrpc, UCS-2) — parsing and
  validation of structured data
