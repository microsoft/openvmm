# VMM Tests

> Note: We recommend using [cargo-nextest](https://nexte.st/) to run unit / VMM
> tests. It is a significant improvement over the built-in `cargo test` runner,
> and is the test runner we use in all our CI pipelines.
>
> You can install it locally by running:
>
> ```bash
> cargo install cargo-nextest --locked
> ```
>
> See the [cargo-nextest](https://nexte.st/) documentation for more info.


The OpenVMM repo contains a set of "heavyweight" VMM tests that fully boot a
virtual machine and run validation against it. Unlike Unit tests, these are all
centralized in a single top-level `vmm_tests` directory.

### Running VMM Tests

VMM tests are run using standard Rust test infrastructure, and are invoked via
`cargo test` / `cargo nextest`.

```bash
cargo nextest run vmm_tests [TEST_FILTERS]
```

For example, to run a simple VMM test that simply boots using UEFI:

```bash
cargo nextest run -p vmm_tests x86_64::uefi_x64_frontpage
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
2. Set `HVLITE_LOG=trace`, replacing `trace` with the log level you want to view.

### Writing VMM Tests

To streamline the process of booting and interacting VMs during VMM tests, the
OpenVMM project uses a in-house test framework/library called `petri`.

The library does not yet have a stable API, so at this time, the best way to
learn how to write new VMM tests is by reading through the existing corpus of
tests, as well as reading through `petri`'s rustdoc-generated API docs.
