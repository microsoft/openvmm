# Azure-hosted Test Images

OpenVMM utilizes pre-made VHDs in order to run tests with multiple guest
operating systems. These images are as close to a "stock" installation as
possible, created from the Azure Marketplace or downloaded directly from a
trusted upstream source.

These VHDs are stored in Azure Blob Storage, and are downloaded when running VMM
tests in CI.

## Downloading VHDs

The `cargo xtask guest-test download-image` command can be used to download vhds
to your machine.

By default it will download all available VHDs, however the `--vhd` option can
be used to only download select guests. After running it the tests can be run
just like any other. This command requires having
[AzCopy](https://learn.microsoft.com/en-us/azure/storage/common/storage-use-azcopy-v10)
installed.
