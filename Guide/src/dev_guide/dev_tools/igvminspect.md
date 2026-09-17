# igvminspect

`igvminspect` is a command line tool for inspecting IGVM files. It
provides `dump` and `extract` subcommands.

## `igvminspect dump`

The `dump` subcommand prints the contents of an IGVM file in a
human-readable format, including the fixed header and all directives.
It accepts raw IGVM files and the embedded IGVM in `vmfirmwareigvm.dll`
or `vmfirmwarecvm.dll`, including CoRIM headers. This replaces the former
`igvmfilegen dump` command; `igvmfilegen dump-corim` remains available.

```bash
cargo run -p igvminspect -- dump --filepath path/to/openhcl.bin
```

## `igvminspect extract`

The `extract` subcommand decomposes an IGVM file into its logical
parts and writes them into a directory tree. If a `.bin.map` file is
provided, page data regions are named after their corresponding
components; otherwise all regions are labeled `unmapped`.

### Usage

```bash
cargo run -p igvminspect -- extract \
  --file path/to/openhcl.bin \
  --output path/to/extracted \
  --map path/to/openhcl.bin.map
```

- `--file`: The IGVM file or firmware resource DLL to extract.
- `--output`: A new directory to write extracted parts into. Existing
  directories and symlinks are rejected to avoid overwriting files or mixing
  artifacts from different extractions.
- `--map` (optional): The `.bin.map` file produced alongside the IGVM
  file. Used to split page data into named components
  (`underhill-kernel`, `underhill-initrd`, etc.).

Omit `--map` to extract without component names. Map isolation sections are
matched to the IGVM platform headers, so overlapping ranges from different
platforms stay separate. Malformed or ambiguous map layouts return an error.

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
path/to/extracted/
  headers/
    platforms.txt
    initializations.txt
  regions/
    0000_underhill-kernel.bin
    0001_underhill-initrd.cpio.gz
    0002_underhill-boot-shim.bin
    0003_sidecar-kernel.bin
    ...
  regions.txt
  vp_context/
    snp_vp0.bin
    x64_vbs_Vtl2_vp0.txt
    ...
  parameter_areas/
    area_0000_0004.bin
  metadata.txt
```

Page data is extracted separately for each compatibility-mask bit. Pages
at the same GPA can differ between platforms, so they are never discarded
based on their address alone. Shared directives appear in each applicable
platform's region stream. Contiguous pages coalesce only when their platform,
component, flags, and data type match. `regions.txt` records these attributes
and the original component name; `metadata.txt` retains page directive order
and original masks.

Region filenames have a unique numeric prefix. Characters in component names
other than ASCII letters, digits, hyphens, and underscores are percent-encoded,
so a map label cannot introduce a path outside the output directory.

Components are assigned file extensions based on their content format:
the initrd gets `.cpio.gz`, command-line strings get `.txt`, device
trees get `.dtb`, and everything else gets `.bin`.
