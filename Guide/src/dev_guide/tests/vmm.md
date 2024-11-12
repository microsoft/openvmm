# VMM Tests

```admonish tip
Note: We recommend using [cargo-nextest](https://nexte.st/) to run unit / VMM
tests. It is a significant improvement over the built-in `cargo test` runner,
and is the test runner we use in all our CI pipelines.

You can install it locally by running: `cargo install cargo-nextest --locked`

See the [cargo-nextest](https://nexte.st/) documentation for more info.
```

The OpenVMM repo contains a set of "heavyweight" VMM tests that fully boot a
virtual machine and run validation against it. Unlike Unit tests, these are all
centralized in a single top-level `vmm_tests` directory.

The OpenVMM CI pipeline will run the full test suite; you'd typically run only
the tests relevant to the changes you're working on.

### Running VMM Tests

```admonish warning
`cargo nextest run` won't rebuild any of your changes. Make sure you `cargo build`
or `cargo xflowey igvm [RECIPE]` first!
```

VMM tests are run using standard Rust test infrastructure, and are invoked via
`cargo test` / `cargo nextest`.

```bash
cargo nextest run -p vmm_tests [TEST_FILTERS]
```

For example, to run a simple VMM test that simply boots using UEFI:

```bash
cargo nextest run -p vmm_tests x86_64::uefi_x64_frontpage
```

And, for further example, to rebuild everything and run all the tests
(see below for details on these steps):

```bash
# Install (most) of the dependencies; cargo nextest run may tell you
# about other deps.
rustup target add x86_64-unknown-uefi
rustup target add x86_64-pc-windows-msvc
sudo apt install clang-tools-14 lld-14

cargo xtask guest-test download-image
cargo xtask guest-test uefi --bootx64

# Rebuild all, and run all tests
cargo build --target x86_64-pc-windows-msvc -p pipette
cargo build --target x86_64-unknown-linux-musl -p pipette

cargo build --target x86_64-pc-windows-msvc -p openvmm

cargo xflowey build-igvm x64-test-linux-direct
cargo xflowey build-igvm x64-cvm
cargo xflowey build-igvm x64

cargo nextest run --target x86_64-pc-windows-msvc -p vmm_tests
```

#### \[Linux] Cross-compiling `pipette.exe`

These commands might use the test agent (`pipette`) that is put inside the VM,
and if the host machine OS and the guest machine OS are different, a setup
is required for cross-building. The recommended approach is to use WSL2 and
cross-compile using the freely available Microsoft Visual Studio Build Tools
or Microsoft Visual Studio Community Edition as described in
[\[WSL2\] Cross Compiling from WSL2 to Windows](../getting_started/suggested_dev_env.md#wsl2-cross-compiling-from-wsl2-to-windows)

If that is not possible, here is another option that relies on [MinGW-w64](https://www.mingw-w64.org/)
and doesn't require installing Windows:

```bash
# Do 1 once, do 2 as needed.
#
# 1. Setup the toolchain
rustup target add x86_64-pc-windows-gnu
sudo apt-get install mingw-w64-x86-64-dev
mingw-genlib -a x86_64 ./support/pal/api-ms-win-security-base-private-l1-1-1.def
sudo mv libapi-ms-win-security-base-private-l1-1-1.a /usr/x86_64-w64-mingw32/lib

# 2. Build Pipette (builds target/x86_64-pc-windows-gnu/debug/pipette.exe first)
cargo build --target x86_64-pc-windows-gnu -p pipette
```

```bash
# Run a test
cargo nextest run -p vmm_tests x86_64::uefi_x64_windows_datacenter_core_2022_x64_boot
```

#### Acquiring external dependencies

Unlike Unit Tests, VMM tests may rely on additional external artifacts in order
to run. e.g: Virtual Disk Images, pre-built OpenHCL binaries, UEFI / PCAT
firmware blobs, etc...

As such, the first step in running a VMM test is to ensure you have acquired all
external test artifacts it may depend upon.

At this time, the VMM test infrastructure does not automatically fetch / rebuild
necessary artifacts. That said - test infrastructure is designed to report clear
and actionable error messages whenever a required test artifact cannot be found,
which provide detailed instructions on how to build / acquire the missing
artifact.


#### Printing logs for VMM Tests

In order to see the OpenVMM logs while running a VMM test, do the following:
1. Add the `--no-capture` flag to your `cargo nextest` command.
2. Set `OPENVMM_LOG=trace`, replacing `trace` with the log level you want to view.

### Writing VMM Tests

To streamline the process of booting and interacting VMs during VMM tests, the
OpenVMM project uses a in-house test framework/library called `petri`.

The library does not yet have a stable API, so at this time, the best way to
learn how to write new VMM tests is by reading through the existing corpus of
tests, as well as reading through `petri`'s rustdoc-generated API docs.
