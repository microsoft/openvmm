# Building OpenVMM

**Prerequisites:**

- One of:
  - [Getting started on Windows](../getting_started.md)
  - [Getting started on WSL2](../getting_started_wsl.md).

* * *

It is strongly suggested that you use [WSL2](../getting_started_wsl.md)
for OpenVMM development, and [cross compile](../openhcl/cross_compile.md)
for Windows when necessary.

## Pre-build Dependencies

OpenVMM currently requires a handful of external dependencies to be present in
order to properly build / run. e.g: a copy of `protoc` to compile Protobuf
files, a copy of the `mu_msvm` UEFI firmware, some test linux kernels, etc...

Running the following command will fetch and unpack these various artifacts into
the correct locations within the repo:

```sh
# Where `ARCH` is either `x86-64` or `aarch64`
cargo xflowey restore-packages [ARCH]
```

### [Linux] Additional Dependencies

On Linux, there are various other dependencies you will need depending on what
you're working on. On Debian-based distros such as Ubuntu, running the following
command within WSL will install these dependencies.

In the future, it is likely that this step will be folded into the
`cargo xflowey restore-packages` command.

```bash
$ sudo apt install \
  binutils              \
  build-essential       \
  gcc-aarch64-linux-gnu \
  libssl-dev
```

## Building

OpenVMM uses the standard Rust build system, `cargo`.

To build OpenVMM, simply run:

```sh
cargo build
```

Note that certain features may require compiling with additional `--feature`
flags.
