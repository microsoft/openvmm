# vhdxtool

`vhdxtool` is a cross-platform command-line utility for creating, inspecting,
validating, replaying, mapping, and converting VHDX images.

## Usage

Run the tool through Cargo while developing:

```sh
cargo run -p vhdxtool -- --help
```

Create dynamic, fixed, and differencing images:

```sh
cargo run -p vhdxtool -- create disk.vhdx --size 64G
cargo run -p vhdxtool -- create fixed.vhdx --size 64G --type fixed
cargo run -p vhdxtool -- create child.vhdx --size 64G --type differencing --parent disk.vhdx
```

A differencing image must have the same virtual size as its parent. The parent
locator always records the parent's data-write GUID and records a relative path
when the tool can compute one. On Windows it also records an extended-length
absolute Win32 path.

Inspect and validate an image:

```sh
cargo run -p vhdxtool -- info disk.vhdx
cargo run -p vhdxtool -- info disk.vhdx --json
cargo run -p vhdxtool -- map disk.vhdx --json
cargo run -p vhdxtool -- check disk.vhdx
cargo run -p vhdxtool -- replay disk.vhdx --dry-run
cargo run -p vhdxtool -- replay disk.vhdx
```

`check` validates VHDX metadata and follows differencing parent chains. It exits
with status 2 for an inconsistent image and status 1 for other errors. A dirty
log is reported as a warning with status 0 so that it can be repaired with
`replay`.

Convert raw files to VHDX, VHDX files to sparse raw files, or between dynamic
and fixed VHDX layouts:

```sh
cargo run -p vhdxtool -- convert disk.raw --output disk.vhdx --output-format vhdx
cargo run -p vhdxtool -- convert disk.vhdx --output disk.raw --output-format raw
cargo run -p vhdxtool -- convert dynamic.vhdx --output fixed.vhdx --output-format vhdx --type fixed
```

All-zero copy chunks are skipped so dynamic VHDX and raw outputs remain sparse
where the host filesystem supports sparse files. Use `-v` for trace-level
progress and VHDX diagnostics.
