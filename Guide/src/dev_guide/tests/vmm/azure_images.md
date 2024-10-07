# Azure-hosted Test Images

OpenVMM utilizes pre-made VHDs in order to run tests with multiple guest
operating systems. These images are as close to a "stock" installation as
possible, created from the Azure Marketplace or downloaded directly from a
trusted upstream source.

These VHDs are stored in Azure Blob Storage, and are downloaded when running VMM
tests in CI.

Unfortunately, due to licensing issues, these images are not available for
public download.

To run VMM tests utilizing these images outside of Microsoft, you may need to
procure and prepare similar test images yourself. At this time, we do not have
explicit documentation on how to do so.

## Downloading VHDs (Microsoft only)

> The following instructions are for Microsoft employees only.

The `cargo xtask guest-test download-image` command can be used to download vhds
to your machine.

By default it will download all available VHDs, however the `--vhd` option can
be used to only download select guests. After running it the tests can be run
just like any other. This command requires having
[AzCopy](https://learn.microsoft.com/en-us/azure/storage/common/storage-use-azcopy-v10)
installed.

Note that at the time of writing the newest version of AzCopy (10.26.0) is unable to
correctly authenticate while running under WSL. To work around this an older version can
be used. The linux build of version 10.21.2, which is known to work, can be downloaded from
[here](https://azcopyvnext.azureedge.net/releases/release-10.21.2-20231106/azcopy_linux_amd64_10.21.2.tar.gz).
