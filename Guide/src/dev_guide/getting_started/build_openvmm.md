# Building OpenVMM

**Prerequisites:**

- One of:
  - [Getting started on Windows](./windows.md)
  - [Getting started on Linux / WSL2](./linux.md).

```admonish tip
It is strongly suggested that you use WSL2, and [cross compile](./cross_compile.md)
for Windows when necessary.
```

## Build Dependencies

OpenVMM currently requires a handful of external dependencies to be present in
order to properly build / run. e.g: a copy of `protoc` to compile Protobuf
files, a copy of the `mu_msvm` UEFI firmware, some test linux kernels, etc...

Running the following command will fetch and unpack these various artifacts into
the correct locations within the repo:

```sh
cargo xflowey restore-packages
```

If you intend to cross-compile, refer to the command's `--help` for additional
options related to downloading packages for other architectures.

## Building

OpenVMM uses the standard Rust build system, `cargo`.

To build OpenVMM, simply run:

```sh
cargo build
```

Note that certain features may require compiling with additional `--feature`
flags.

## Troubleshooting

This section documents some common errors you may encounter while building
OpenVMM.

If you are still running into issues, consider filing an issue on the OpenVMM
GitHub Issue tracker.

### failed to invoke protoc

**Error:**

```text
error: failed to run custom build command for `inspect_proto v0.0.0 (/home/daprilik/src/openvmm/support/inspect_proto)`

Caused by:
  process didn't exit successfully: `/home/daprilik/src/openvmm/target/debug/build/inspect_proto-e959f9d63c672ccc/build-script-build` (exit status: 101)
  --- stderr
  thread 'main' panicked at support/inspect_proto/build.rs:23:10:
  called `Result::unwrap()` on an `Err` value: Custom { kind: NotFound, error: "failed to invoke protoc (hint: https://docs.rs/prost-build/#sourcing-protoc): (path: \"/home/daprilik/src/openvmm/.packages/Google.Protobuf.Tools/tools/protoc\"): No such file or directory (os error 2)" }
  note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
warning: build failed, waiting for other jobs to finish...
```

Note: the specific package that throws this error may vary, and may not always be `inspect_proto`

**Solution:**

You attempted to build OpenVMM without first restoring necessary packages.

Please run `cargo xflowey restore-packages`, and try again.

### use of unstable library feature

**Error:**

```text
error[E0658]: use of unstable library feature 'absolute_path'
  --> flowey/flowey/src/lib.rs:37:17
   |
37 |                 std::path::absolute(self)
   |                 ^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #92750 <https://github.com/rust-lang/rust/issues/92750> for more information

For more information about this error, try `rustc --explain E0658`.
error: could not compile `flowey` (lib) due to previous error
```

**Solution:**

Install Rust using the official instructions for [Linux](https://openvmm.dev/guide/dev_guide/getting_started/linux.html#installing-rust) or [Windows](https://openvmm.dev/guide/dev_guide/getting_started/windows.html#installing-rust).
