# Fuzzing in OpenVMM

Fuzzing infrastructure in OpenVMM is based on the excellent
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) project, which makes it super easy to get
up-and-running with fuzzing in Rust projects.

For the curious: Under-the-hood, `cargo-fuzz` hooks into LLVM's
[libFuzzer](https://www.llvm.org/docs/LibFuzzer.html) to do the actual fuzzing.
