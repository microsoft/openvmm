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

Use `--type fixed` to provision all payload blocks during creation. Use
`--type differencing` with `--parent` to create a child image:

```bash
cargo run -p openvmm-img -- create path/to/child.vhdx \
    --size 64G --type differencing --parent path/to/parent.vhdx
```

A differencing child inherits its parent's logical sector size unless one is
specified explicitly. An explicit logical sector size must match the parent.
The child and parent may have different virtual disk sizes.

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

The input format is inferred from `.raw`, `.img`, or `.vhdx`. Use
`--input-format` when the input has another extension. Differencing VHDX inputs
are not supported by `convert`.

All-zero copy chunks are skipped so dynamic VHDX and raw outputs remain sparse
when the host filesystem supports sparse files.
