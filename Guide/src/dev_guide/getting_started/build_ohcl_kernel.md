# Building a custom OpenHCL Linux Kernel

```admonish note
This step is NOT required in order to build OpenHCL!

Unless you have a specific reason, it is _strongly_ recommended to stick to
the pre-built Kernel image which is automatically downloaded as part of the
OpenHCL build process.
```

## Cloning the kernel repository

If you need to rebuild the kernel, the sources are available in the [OHCL-Linux-Kernel](https://github.com/microsoft/OHCL-Linux-Kernel) repo :

* the main branch is [product/hcl-main/6.6](https://github.com/microsoft/OHCL-Linux-Kernel/tree/product/hcl-main/6.6),
* the dev branch is [project/hcl-dev/6.6](https://github.com/microsoft/OHCL-Linux-Kernel/tree/project/hcl-dev/6.6).

Unless you need the entire repo history, cloning just one branch and `depth = 1`, saves
significant time and disk space:

```sh
git clone https://github.com/microsoft/OHCL-Linux-Kernel.git -b product/hcl-main/6.6 --depth=1
```

Cloning under Windows is likely to fail due to some files using names that Windows reserves, e.g. `aux.c`, and
NTFS being non case-sensitive by default as there are few files in the Linux kernel repo whose names differ
in their case only. To clone successfully under Windows, need a fix in `ntdll` (merged in `Ni`?),
and a case-sensitive NTFS partition. Best to start with the default WSL2 if there is no existing working setup.

## Building the kernel locally

The following instructions for building the kernel locally target the Ubuntu distributions.
In order to build on rpm-based systems the only changes are likely to be the way the package
manager is invoked and installing the `kernel-devel` package instead of what is needed for
Ubuntu.

The paths below are relative to the cloned kernel repository root.

Do once for every machine that hasn't run this step successfully:

```sh
./Microsoft/install-deps.sh
```

If you plan to use your custom kernel to work with confidential VMs, you need to enable a few more options
in the kernel. Do once after cloning the kernel repository or every time you remove local changes from your
kernel configuration file:

```sh
./Microsoft/merge-cvm-config.sh
```

Every time the kernel needs to be rebuilt:

```sh
./Microsoft/build-hcl-kernel.sh
```

The output directory is `./out`, it contains the kernel binary, the kernel modules,
and the debug symbols, and `cargo xflowey build-igvm` can be pointed to it to use
as a source of the kernel binary and the modules.

In the case you are iterating on a change, install [`ccache`](https://ccache.dev/)
for decreasing the kernel build time significantly, might be close to an order of
magnitude. To use the compiler cache, prepend `CC="ccache gcc"` to the build command.
To see if the cache integrates itself into the toolchain:

```sh
host:~$ which gcc

/usr/lib64/ccache/gcc
```
