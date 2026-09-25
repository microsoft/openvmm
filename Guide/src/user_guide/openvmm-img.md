# openvmm-img

`openvmm-img` creates, inspects, validates, and converts disk images using
OpenVMM's native storage implementations. It is part of the OpenVMM software
suite, but can be used independently of the virtual machine monitor.

The tool currently supports VHDX image management and conversion between raw
and VHDX images. Run it from the repository with Cargo:

```bash
cargo run -p openvmm-img -- --help
```

## Creating Images

Create a dynamic VHDX image by naming it with a `.vhdx` extension or by
specifying the format explicitly:

```bash
cargo run -p openvmm-img -- create path/to/disk.vhdx --size 64G
cargo run -p openvmm-img -- create path/to/disk --format vhdx --size 64G
```

Format-specific options are passed as comma-separated key-value pairs. Use
`allocation=fixed` to provision all VHDX payload blocks during creation:

```bash
cargo run -p openvmm-img -- create path/to/disk.vhdx --size 64G \
    --format-options allocation=fixed
```

Use `--parent` to create a differencing image. The parent implies dynamic
allocation, so it cannot be combined with `allocation=fixed`:

```bash
cargo run -p openvmm-img -- create path/to/child.vhdx \
    --size 64G --parent path/to/parent.vhdx
```

A differencing child inherits its parent's logical sector size unless one is
specified explicitly. An explicit logical sector size must match the parent.
The child and parent may have different virtual disk sizes.

Run `format-options` without a format to list the supported formats, or pass a
format to list its accepted options:

```bash
cargo run -p openvmm-img -- format-options
cargo run -p openvmm-img -- format-options vhdx
```

## Inspecting and Validating Images

Use `info` for image metadata, `map` for allocated virtual ranges, and `check`
to validate VHDX metadata and its differencing parent chain:

```bash
cargo run -p openvmm-img -- info path/to/disk.vhdx
cargo run -p openvmm-img -- map path/to/disk.vhdx
cargo run -p openvmm-img -- check path/to/disk.vhdx
```

Pass `--json` to `info` or `map` for machine-readable output. `check` exits
with status 2 for an inconsistent image and status 1 for other failures.

Use `replay` to repair an image with a dirty VHDX write-ahead log. Pass
`--dry-run` to report whether replay is required without changing the image.

```bash
cargo run -p openvmm-img -- replay path/to/disk.vhdx --dry-run
cargo run -p openvmm-img -- replay path/to/disk.vhdx
```

## Converting Images

Convert between raw and VHDX images by specifying the output format:

```bash
cargo run -p openvmm-img -- convert path/to/disk.raw \
    --output path/to/disk.vhdx --output-format vhdx
cargo run -p openvmm-img -- convert path/to/disk.vhdx \
    --output path/to/disk.raw --output-format raw
```

VHDX output options use the same key-value syntax. For example, this creates a
fixed VHDX with 4 MiB payload blocks:

```bash
cargo run -p openvmm-img -- convert path/to/disk.raw \
    --output path/to/disk.vhdx --output-format vhdx \
    --format-options allocation=fixed,block_size=4M
```

The input format is inferred from `.raw`, `.img`, or `.vhdx`. Use
`--input-format` when the input has another extension. Differencing VHDX inputs
are not supported by `convert`.

All-zero copy chunks are skipped so dynamic VHDX and raw outputs remain sparse
when the host filesystem supports sparse files.
