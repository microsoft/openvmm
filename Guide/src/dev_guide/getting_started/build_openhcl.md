# Building OpenHCL

**Prerequisites:**

- [Getting started on Linux / WSL2](./linux.md).

Reminder: OpenHCL cannot currently be built on Windows hosts!

* * *

An OpenHCL IGVM firmware image is composed of several distinct binaries and
artifacts. For example: the `openvmm_hcl` usermode binary, the OpenHCL boot
shim, the OpenHCL Linux kernel and initrd, etc....

Some of these components are built directly out of the OpenVMM repo, whereas
others must be downloaded as pre-built artifacts from other associated repos.
Various tools and scripts will then transform, package, and re-package these
artifacts into a final OpenHCL IGVM firmware binary.

Fortunately, we don't expect you do to all those steps manually!

All the complexity of installing the correct system dependencies, building the
right binaries, downloading the right artifacts, etc... is neatly encapsulated
behind a single `cargo xflowey build-igvm` command, which orchestrates the
entire end-to-end OpenHCL build process.

Using the `build-igvm` flow is as simple as running:

```bash
cargo xflowey build-igvm [RECIPE]
```

The first build will take some time as all the dependencies are
installed/downloaded/built.

**Note: At this time, OpenHCL can only be built on Linux (or WSL2)!**

A "recipe" corresponds to one of the pre-defined IGVM SKUs that are actively
supported and tested in OpenVMM's build infrastructure.

A single recipe encodes _all_ the details of what goes into an individual IGVM
file, such as what build flags `openvmm_hcl` should be built with, what goes
into a VTL2 initrd, what `igvmfilegen` manifest is being used, etc...

- e.g: `x64`, for a "standard" x64 IGVM
- e.g: `aarch64`, for a "standard" aarch64 IGVM
- e.g: `x64-cvm`, for a x64 CVM IGVM
- e.g: `x64-test-linux-direct`, for x64 IGVM booting a test linux direct image
- _for a full list of available recipes, please run `cargo xflowey build-igvm --help`_

New recipes can be added by modifying the `build-igvm` source code.

Build output is then binplaced to: `flowey-out/artifacts/build-igvm/{release-mode}/{recipe}/openhcl-{recipe}.bin`

So, for example:

```bash
cargo xflowey build-igvm x64-cvm
# output: flowey-out/artifacts/build-igvm/debug/x64-cvm/openhcl-x64-cvm.bin

cargo xflowey build-igvm x64 --release
# output: flowey-out/artifacts/build-igvm/release/x64/openhcl-x64.bin
```

```admonish warning
`cargo xflowey build-igvm` is designed to be used as part of the
developer inner-loop, and does _NOT_ have a stable CLI suitable for CI or any
other form of production automation!

In-tree pipelines and automation should interface with the underlying `flowey`
infrastructure that powers `cargo xflowey build-igvm`, _without_ relying on
the details of its CLI.
```

## Building ohcldiag-dev

`ohcldiag-dev` is typically built as a Windows binary.

This can be done directly from Windows, or using
[cross-compilation from WSL2](../getting_started/cross_compile.md).

The command to build `ohcldiag-dev` is simply:

```sh
# you may need to run `rustup target add x86_64-pc-windows-msvc` first
cargo build -p ohcldiag-dev --target x86_64-pc-windows-msvc
```

**Note:** Thanks to x86 emulation built into Windows, `ohcldiag-dev.exe` that is
built for x64 Windows will work on Aarch64 Windows as well.

## Troubleshooting

This section documents some common errors you may encounter while building
OpenHCL.

If you are still running into issues, consider filing an issue on the OpenVMM
GitHub Issue tracker.

### Help! The build failed due to a missing dependency

If you don't mind having `xflowey` install some dependencies globally on your
machine (i.e: via `apt install`, or `rustup toolchain add`), you can pass
`--auto-install-deps` to your invocation of `build-igvm`.

Alternatively - `build-igvm` _should_ emit useful human-readable error messages
when it encounters a dependency that isn't installed, with a suggestion on how
to install it.

If it doesn't - please file an Issue!

### Help! Everything is rebuilding even though I only made a small change

Cargo's target triple handling can be a bit buggy. Try running with:

```bash
CARGO_BUILD_TARGET=x86_64-unknown-linux-gnu cargo build-igvm [RECIPE]
```

or adding the below to your .bashrc:

```bash
export CARGO_BUILD_TARGET=x86_64-unknown-linux-gnu
```

## Build Customization

Aside from building IGVM files corresponding the the built-in IGVM recipes,
`build-igvm` also offers a plethora of customization options for developers who
wish to build specialized custom IGVM files for local testing.

Some examples of potentially useful customization include:

- `--override-manifest`: Override the recipe's `igvmfilegen` manifest file
    via, in order to tweak different kernel command line options, different VTL0
    boot configuration, or different VTL2 memory sizes.

- `--custom-openvmm-hcl`: Specify a pre-built `openvmm_hcl` binary. This is
    useful in case you have already built it with some custom settings, e.g.:

    ```bash
    cargo build --target x86_64-unknown-linux-musl -p openvmm_hcl --features myfeature
    cargo xflowey build-igvm x64 --custom-openvmm-hcl target/x86_64-unknown-linux-musl/debug/openvmm_hcl
    ```

- Specify a custom VTL2 kernel `vmlinux` / `Image`, instead of using a
    pre-packed stable/dev kernel.

    ```bash
    cargo xflowey build-igvm x64 --custom-kernel path/to/my/prebuilt/vmlinux
    ```

For a full list of available customizations, refer to `build-igvm --help`.

### Advanced

Depending on what you're doing, you may need to build the individual components
that go into an OpenHCL IGVM build.

Our `flowey`-based pipelines handle the complexities of properly invoking and
orchestrating the various individual build tools / scripts used to construct
IGVM files, but a sufficiently motivated user can go through these steps
manually.

Please consult the source code for `cargo xflowey build-igvm` for a breakdown of
all build steps and available customization options.

Note that the canonical "source of truth" for how to build end-to-end OpenHCL
IGVM files are these build scripts themselves, and the specific flow is subject
to change over time!
