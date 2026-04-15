# VMM Tests

The OpenVMM repo contains a set of "heavyweight" VMM tests that fully boot a
virtual machine and run validation against it. Unlike Unit tests, these are all
centralized in a single top-level `vmm_tests` directory.

The OpenVMM PR and CI pipelines will run the full test suite on all supported
platforms; you'd typically run only the tests relevant to the changes you're
working on.

## Writing VMM Tests

To streamline the process of booting and interacting with VMs during VMM tests, the
OpenVMM project uses an in-house test framework/library called `petri`.

The library does not yet have a stable API, so at this time, the best way to
learn how to write new VMM tests is by reading through the existing corpus of
tests (start with vmm_tests/vmm_tests/tests/tests/multiarch.rs),
as well as reading through `petri`'s rustdoc-generated API docs.

The tests are currently generated using a macro (`#[vmm_test]`) that allows
the same test body to be run in a variety of scenarios, with different guest
operating systems, firmwares, and VMMs (including Hyper-V, which is useful
for testing certain OpenHCL features that aren't supported when using
OpenVMM as the host VMM).

### "heavy" tests

The global [nextest.toml](https://github.com/microsoft/openvmm/blob/main/.config/nextest.toml)
configures how tests run in our test environments. The `default` and `ci` profiles
control things like timeouts, and how many resources we allocate to a given test. The number
of required threads is a fuzzy requirement relative to the number of VPs consumed by the VM under
test, the amount of memory your test needs, the host test framework (petri itself), and so on.

We have some pre-defined overrides that perform filterset matching on test name. These overrides
are curated to balance individual trial (test case) performance against overall concurrency on
engineers' local machines and in CI. Put these special words in your test to opt in to that override:

- `heavy` - if your test is heavier than the typical vmm_test. E.g., your test explicitly requests 16 virtual processors.
- `very_heavy` if your test is heavier than a `heavy` test. E.g., your test explicitly requests 32 virtual processors.

### "unstable" tests

If a test is not yet reliable enough to gate PRs, add `unstable` to the macro.

For individual variants:

```rust,ignore
#[vmm_test(
    // unstable variant:
    unstable_hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    // other reliable variants:
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
    // ...
)]
async fn my_test<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    // ...
}
```

For all variants of the test:

```rust,ignore
#[vmm_test_with(unstable(
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
    // ...
))]
async fn my_test<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    // ...
}
```

Unstable tests run in the same CI job as stable tests. When an unstable test fails
the CI run will pass with a warning

To promote an unstable test to stable, remove `unstable` from the macro. This is
a single-place change — no CI or configuration updates are required.

To ignore these `unstable` tags and report failures for all tests when running
locally, set the following environment variable: `PETRI_REPORT_UNSTABLE_FAIL=1`

## Running VMM Tests (Flowey)

The easiest way to run VMM tests locally is `cargo xflowey vmm-tests-run`. It
automatically discovers required artifacts, builds dependencies, and runs your
tests in a single command.

To run a **specific test** (or set of tests), use `--filter` with a
[nextest filter](https://nexte.st/docs/filtersets/) expression:

```bash
cargo xflowey vmm-tests-run --filter "test(my_test_name)" --dir /tmp/vmm-tests-run
```

### Targeting a Platform

By default, `vmm-tests-run` builds for the current host. Use `--target` to
build for a different platform. The supported targets are:

| Target | Description |
|--------|-------------|
| `windows-x64` | Windows x86_64 (Hyper-V / WHP) |
| `windows-aarch64` | Windows ARM64 (Hyper-V / WHP) |
| `linux-x64` | Linux x86_64 |

**Cross-compiling for Windows from WSL2** is fully supported, you can build
and run Windows VMM tests directly from your WSL2 shell. This requires the
cross-compilation environment to be set up first.

Then target Windows as usual. The output directory **must** be on the Windows
filesystem (e.g., `/mnt/d/...`):

```bash
cargo xflowey vmm-tests-run --target windows-x64 --dir /mnt/d/vmm_tests
```

For full cross-compilation setup instructions, see
[Cross Compiling for Windows](../getting_started/cross_compile.md).

When running Hyper-V tests, your user account must be a member of the
Hyper-V Administrators group.

To see all available options: `cargo xflowey vmm-tests-run --help`.

## Running VMM Tests (Manual)

```admonish tip
Note: We recommend using [cargo-nextest](https://nexte.st/) to run unit / VMM
tests. It is a significant improvement over the built-in `cargo test` runner,
and is the test runner we use in all our CI pipelines.

You can install it locally by running: `cargo install cargo-nextest --locked`

See the [cargo-nextest](https://nexte.st/) documentation for more info.
```

You can directly invoke `cargo test` or `cargo nextest` to run the vmm
tests manually.

Unlike Unit Tests, VMM tests may rely on additional external artifacts in order
to run. e.g: Virtual Disk Images, pre-built OpenHCL binaries, UEFI / PCAT
firmware blobs, etc.

As such, the first step in running a VMM test is to ensure you have acquired all
external test artifacts it may depend upon.

The VMM test infrastructure does not automatically fetch / rebuild
necessary artifacts unless you are using [flowey](#running-vmm-tests-flowey).
However, the test infrastructure is designed to report clear
and actionable error messages whenever a required test artifact cannot be found,
which provide detailed instructions on how to build / acquire the missing
artifact. Some dependencies can only be built on Linux (OpenHCL and Linux
pipette, for example). If you are building on Linux and want to run Windows
guest tests, pipette will need to be
[cross compiled for Windows](#linux-cross-compiling-pipetteexe).

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
cargo nextest run -p vmm_tests multiarch::openvmm_uefi_x64_frontpage
```

And, for further example, to rebuild everything* and run all* the tests
(see below for details on these steps):

*This will not work for Hyper-V tests. TMK tests need additional build steps.

```bash
# Install (most) of the dependencies; cargo nextest run may tell you
# about other deps.
rustup target add x86_64-unknown-none
rustup target add x86_64-unknown-uefi
rustup target add x86_64-pc-windows-msvc
sudo apt install clang-tools-14 lld-14

cargo install cargo-nextest --locked

cargo xtask guest-test download-image
cargo xtask guest-test uefi --bootx64

# Rebuild all, and run all tests
cargo build --target x86_64-pc-windows-msvc -p pipette
cargo build --target x86_64-unknown-linux-musl -p pipette

cargo build --target x86_64-pc-windows-msvc -p openvmm

cargo xflowey build-igvm x64-test-linux-direct
cargo xflowey build-igvm x64-cvm
cargo xflowey build-igvm x64

cargo nextest run --target x86_64-pc-windows-msvc -p vmm_tests --filter-expr 'all() & !test(hyperv) & !test(tmk)'
```

### \[Linux] Cross-compiling `pipette.exe`

These commands might use the test agent (`pipette`) that is put inside the VM,
and if the host machine OS and the guest machine OS are different, a setup
is required for cross-building. The recommended approach is to use WSL2 and
cross-compile using the freely available Microsoft Visual Studio Build Tools
or Microsoft Visual Studio Community Edition as described in
[\[WSL2\] Cross Compiling from WSL2 to Windows](../getting_started/cross_compile.md)

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
cargo nextest run -p vmm_tests multiarch::openvmm_uefi_x64_windows_datacenter_core_2022_x64_boot
```

### Printing logs for VMM Tests

In order to see the OpenVMM logs while running a VMM test, do the following:
1. Add the `--no-capture` flag to your `cargo nextest` command.
2. Set `OPENVMM_LOG=trace`, replacing `trace` with the log level you want to view.
