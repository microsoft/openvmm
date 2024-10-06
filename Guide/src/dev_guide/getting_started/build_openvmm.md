# Building OpenVMM

**Prerequisites:**

- One of:
  - [Getting started on Windows](./windows.md)
  - [Getting started on Linux / WSL2](./linux.md).

* * *

It is strongly suggested that you use WSL2, and [cross compile](./suggested_dev_env.md#wsl2-cross-compiling-from-wsl2-to-windows)
for Windows when necessary.

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
