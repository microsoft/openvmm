# Tests

Writing (and running!) good tests is critical to maintaining quality.

There are three distinct types of tests to keep in mind as a developer:

* Unit tests
* Doc tests
* VMM tests

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

## Unit tests

Unit tests test individual functions or components without pulling in lots of
ambient infrastructure. In Rust, these are usually written in the same file as
the product code--this ensures that the test has access to any internal methods
or state it requires, and it makes it easier to ensure that tests and code are
updated at the same time.

A typical module with unit tests might look something like this:

```rust
fn add_5(n: u32) -> u32 {
    n + 5
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_add_5() {
        assert_eq!(add_5(3), 8);
    }
}
```

In the OpenVMM repo, all the unit tests are run on every pull request, on an
arbitrary build machine. As a result of this approach, it's important that unit
tests run quickly, do not affect the state of the machine that runs them, and do
not take a dependency on machine configuration (so no root/administrator access
or virtualization requirement).

We may loosen these guidelines over time if it becomes necessary. You can also
mark tests with `#[ignore]` if they do not meet these guidelines but are useful
for manual testing.

See the [unit testing section](https://doc.rust-lang.org/rust-by-example/testing/unit_testing.html)
in "Rust by example" for more details.

## Doc tests

Rust has another type of unit tests known as doc tests. These are unit tests
that are written in the API documentation comments of public functions. They
will be run automatically along with the unit tests, so the same guidelines
apply.

When do you choose a doc test over a unit test? Doc tests can only access public
functionality, and they are intended to document the usage of a function or
method, not to exhaustively check every case. So write doc tests primarily as
examples for other developers, and rely on unit tests for your main coverage.

An example might look like this:

```rust
/// Adds 5 to `n`.
///
/// ```
/// assert_eq!(mycrate::add_5(3), 8);
/// ```
pub fn add_5(n: u32) -> u32 {
    n + 5
}
```

See the [documentation testing
section](https://doc.rust-lang.org/rust-by-example/testing/doc_testing.html) in
Rust by example for more info.

## VMM Tests

The OpenVMM repo contains a set of "heavyweight" VMM tests that fully boot a
virtual machine and run validation against it. Unlike Unit tests, these are all
centralized in a single top-level `vmm_tests` directory.

###  Running VMM Tests

VMM tests in the OpenVMM repo are built on-top of standard Rust test
infrastructure, and are invoked via `cargo test` / `cargo nextest`.

```bash
cargo nextest run vmm_tests [TEST_FILTERS]
```

### Acquiring external dependencies

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

### Printing logs for VMM Tests

In order to see the OpenVMM logs while running a VMM test, do the following:
1. Add the `--no-capture` flag to your `cargo nextest` command.
2. Set `HVLITE_LOG=trace`, replacing `trace` with the log level you want to view.

### Guest OS Test images

VMM tests run a variety of different guest OSes, which are downloaded / built
from different sources.

#### Azure-stored VHD tests

OpenVMM utilizes pre-made VHDs in order to run tests with multiple guest
operating systems. They contain unmodified guest images either created from the
Azure Marketplace or downloaded from a trusted source.

These VHDs are stored in Azure Blob Storage, and are downloaded when running VMM
tests in CI.

> NOTE: Due to licensing issues, these images are not available for public
> download.
>
> The following instructions are for Microsoft employees only.

##### Downloading VHDs

The `cargo xtask guest-test download-image` command can be used to download vhds to your
machine. By default it will download all available VHDs, however the --vhd
option can be used to only download select guests. After running it the tests
can be run just like any other. This command requires having
[AzCopy](https://learn.microsoft.com/en-us/azure/storage/common/storage-use-azcopy-v10)
installed.

Note that at the time of writing the newest version of AzCopy (10.26.0) is unable to
correctly authenticate while running under WSL. To work around this an older version can
be used. The linux build of version 10.21.2, which is known to work, can be downloaded from
[here](https://azcopyvnext.azureedge.net/releases/release-10.21.2-20231106/azcopy_linux_amd64_10.21.2.tar.gz).

##### Uploading new VHDs

Images can be uploaded directly into blob storage after downloading them locally.
Images uploaded from an external source in this way __must__ add a `SOURCE_URL`
field in their metadata containing the original URL the file was downloaded from.

##### Creating new VHDs from the Azure Marketplace

Creating a new VHD for test usage is done with the
[Azure CLI](https://learn.microsoft.com/en-us/cli/azure/install-azure-cli).
Once this is installed, open a powershell window, log in to your internal
account, and select the HvLite subscription:

```powershell
az login
az account set -n HvLite
```

Next find the OS you wish to create a disk for using `az vm image list`. This
command has many filtering options, read its help for more information. Running
a completely unfiltered search and manually scrolling through the results is
not recommended. As an example:

```powershell
az vm image list --output table --all --offer WindowsServer --sku smalldisk-g2
```

Once you've found the OS you want copy down its `Sku`, `Version`, and `URN` values.
These are used to create a disk containing this OS. By convention we set the name
of this disk to `<OsName>-<Sku>-<Version>`. Using one of the items from the previous
example, this would look like:

```powershell
$name = "WindowsServer-2022-datacenter-core-smalldisk-g2-20348.1906.230803"
az disk create --resource-group HvLite-Test-VHDs --location westus2 --output table --name $name --image-reference MicrosoftWindowsServer:WindowsServer:2022-datacenter-core-smalldisk-g2:20348.1906.230803
```

Next, copy the newly created disk into our blob store. Again, by convention, we
use `<OsName>-<Sku>-<Version>` as the destination blob name. The 'tsv' output format is
specified on the first command and triple quotes are used on the second to
ensure proper formatting of the produced URL:

```powershell
$sasUrl = $(az disk grant-access --resource-group HvLite-Test-VHDs --output tsv --name $name --query [accessSas] --duration-in-seconds 3600)
az storage blob copy start --account-name hvlitetestvhds --destination-container vhds --source-uri """$sasUrl""" --destination-blob "$name.vhd"
```

The copy operation will take some time to complete. You can check its status by
running:

```powershell
az storage blob show --account-name hvlitetestvhds --container-name vhds --output table --query properties.copy --name "$name.vhd"
```

Once the copy operation has successfully completed you should delete the disk,
as we no longer have a use for it:

```powershell
az disk revoke-access --resource-group HvLite-Test-VHDs --name $name
az disk delete --resource-group HvLite-Test-VHDs --name $name
```

Finally, go add your new vhd blob to `petri/src/vhds/src/files.rs` so that
it gets downloaded during CI and local runs.

#### `guest_test_uefi`

`guest_test_uefi` is a minimal, no-fuss, `no_std` + `alloc` environment from
which to write unit tests that are able to exercise OpenVMM devices, without
having to deal with annoying things like "virtual memory" or "OS security
features".

Want to write to an arbitrary MMIO/PIO address? Go for it! It's just you, the
VM, and the (very unobtrusive) UEFI runtime!

##### Building + Running

`guest_test_uefi` must be built for `XXX-unknown-uefi` targets. These are not
installed by default, so you'll need to install the correct target via `rustup`.
For example:

```bash
rustup target add x86_64-unknown-uefi
```

Since this code runs in the guest, the built `.efi` binary needs to get packaged
into a disk image that UEFI can read.

To streamline the process of obtaining such a disk image, `cargo xtask` includes
a helper to generate properly formatted `.img` files containing a given `.efi`
image. e.g:

```bash
# build the UEFI test application
cargo build -p guest_test_uefi --target x86_64-unknown-uefi
# create the disk image
cargo xtask guest-test uefi --bootx64 ./target/x86_64-unknown-uefi/debug/guest_test_uefi.efi
# test in OpenVMM
cargo run -- --uefi --gfx --hv --processors 1 --disk memdiff:./target/x86_64-unknown-uefi/debug/guest_test_uefi.img
```

Protip: this is a generic UEFI binary, and can be run outside of the HvLite repo
as well (e.g: in QEMU, Hyper-V, etc...)!

To convert the raw `.img` into other formats, `qemu-img` is very helpful:

```bash
# Adjust for the target architecture and type of the build
OVMM_UEFI_TEST_IMG_DIR=./target/x86_64-unknown-uefi/debug
# VmWare
qemu-img convert -f raw -O vmdk ${OVMM_UEFI_TEST_IMG_DIR}/guest_test_uefi.img ${OVMM_UEFI_TEST_IMG_DIR}/guest_test_uefi.vmdk
# Hyper-V
qemu-img convert -f raw -O vhdx  ${OVMM_UEFI_TEST_IMG_DIR}/guest_test_uefi.img ${OVMM_UEFI_TEST_IMG_DIR}/guest_test_uefi.vhdx
# The files:
ls -la $OVMM_UEFI_TEST_IMG_DIR/{*.img,*.vhdx,*.vmdk}
```
