# igvminspect

`igvminspect` is a command line tool for inspecting IGVM files. It
provides `dump` and `extract` subcommands.

## `igvminspect dump`

The `dump` subcommand prints the contents of an IGVM file in a
human-readable format, including the fixed header and all directives.

```bash
cargo run -p igvminspect -- dump --filepath <file.bin>
```

## `igvminspect extract`

The `extract` subcommand decomposes an IGVM file into its logical
parts and writes them into a directory tree. If a `.bin.map` file is
provided, page data regions are named after their corresponding
components; otherwise all regions are labeled `unmapped`.

### Usage

```bash
cargo run -p igvminspect -- extract \
  --file <file.bin> \
  --output <output-dir> \
  [--map <file.bin.map>]
```

- `--file`: The IGVM file to extract.
- `--output`: Directory to write extracted parts into.
- `--map` (optional): The `.bin.map` file produced alongside the IGVM
  file. Used to split page data into named components
  (`underhill-kernel`, `underhill-initrd`, etc.).

### Example

```bash
cargo xflowey build-igvm x64

cargo run -p igvminspect -- extract \
  --file flowey-out/artifacts/build-igvm/debug/x64/openhcl-x64.bin \
  --map \
    flowey-out/artifacts/build-igvm/debug/x64/openhcl-x64.bin.map \
  --output /tmp/igvm-extracted
```

### Extracted directory structure

The IGVM file is extracted into:

```text
<output-dir>/
  headers/
    platforms.txt
    initializations.txt
  regions/
    underhill-kernel_0.bin
    underhill-initrd.cpio.gz
    underhill-boot-shim_0.bin
    sidecar-kernel_0.bin
    ...
  regions.txt
  vp_context/
    snp_vp0.bin
    x64_vbs_Vtl2_vp0.txt
    ...
  parameter_areas/
    area_0000.bin
  metadata.txt
```

Pages at the same GPA with different compatibility masks (SNP/TDX/VBS)
are deduplicated since the data is identical.

Components are assigned file extensions based on their content format:
the initrd gets `.cpio.gz`, command-line strings get `.txt`, device
trees get `.dtb`, and everything else gets `.bin`.
