# Azure-hosted Test Images

OpenVMM utilizes pre-made VHDs in order to run tests with multiple guest
operating systems. These images are as close to a "stock" installation as
possible, created from the Azure Marketplace or downloaded directly from a
trusted upstream source.

These VHDs are stored in Azure Blob Storage, and are downloaded when running VMM
tests in CI.

## Lazy Fetching (Remote Artifacts)

When running tests with the OpenVMM backend, disk artifacts that are hosted on
Azure Blob Storage can be fetched lazily over HTTP instead of being
pre-downloaded. The VMM reads disk sectors on demand via HTTP Range requests,
with a local SQLite cache so that sectors are only fetched once.

This is controlled by two environment variables:

- **`PETRI_REMOTE_ARTIFACTS`** — Set to `0` or `false` to force all artifacts to
  be resolved locally, disabling lazy fetching. By default, remote access is
  allowed for artifacts that opt in.
- **`PETRI_CACHE_DIR`** — Override the directory used for the SQLite read cache.
  Defaults to a platform-appropriate cache directory (e.g.
  `~/.cache/petri` on Linux, `~/Library/Caches/petri` on macOS,
  `%LOCALAPPDATA%\petri\cache` on Windows).

```admonish note
VHDX artifacts (e.g. `WINDOWS_11_ENTERPRISE_AARCH64`) do **not** support lazy
fetching and must be downloaded locally. The Hyper-V backend also requires all
artifacts to be local.
```

## Downloading VHDs

The `cargo xtask guest-test download-image` command can be used to download vhds
to your machine.

By default it will download all available VHDs, however the `--vhd` option can
be used to only download select guests. After running it the tests can be run
just like any other. This command requires having
[AzCopy](https://learn.microsoft.com/en-us/azure/storage/common/storage-use-azcopy-v10)
installed.
