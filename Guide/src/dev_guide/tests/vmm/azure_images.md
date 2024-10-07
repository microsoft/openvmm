# Azure-hosted Test Images


OpenVMM utilizes pre-made VHDs in order to run tests with multiple guest
operating systems. These images are as close to a "stock" installation as
possible, created from the Azure Marketplace or downloaded directly from a
trusted upstream source.

> NOTE: Due to licensing issues, these images are not available for public
> download.
>
> The following instructions are for Microsoft employees only.

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

Note that at the time of writing the newest version of AzCopy (10.26.0) is unable to
correctly authenticate while running under WSL. To work around this an older version can
be used. The linux build of version 10.21.2, which is known to work, can be downloaded from
[here](https://azcopyvnext.azureedge.net/releases/release-10.21.2-20231106/azcopy_linux_amd64_10.21.2.tar.gz).

## Uploading new VHDs

Images can be uploaded directly into blob storage after downloading them locally.
Images uploaded from an external source in this way __must__ add a `SOURCE_URL`
field in their metadata containing the original URL the file was downloaded from.

## Creating new VHDs from the Azure Marketplace

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
