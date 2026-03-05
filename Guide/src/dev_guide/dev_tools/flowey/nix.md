# Nix

In order to enable reproducible builds, experimental flowey features are being built to utilize the [Nix package manager](https://nixos.org/) for dependency management and environment isolation.
The root of the nix configuration lives in `shell.nix`. If you're unsure where the Nix definition is for a dependency, you should be able to track it down from there.

## Updating Nix Packages

Nix dependencies require a hash of their contents to ensure integrity and reproducibility. When updating a dependency, you'll need to update the release that's being pulled and its corresponding hash.

For instance, let's say we have a new release of the OpenHCL Kernel and we want to update it in our Nix configuration:

1. Go to the corresponding `.nix` file (in this case, `openhcl_kernel.nix`)
2. Clear the hash to an empty string
3. Update the version
4. Run `nix-shell --pure` and use the printed error to get the new hash

> **Warning:** Because Nix caches dependencies based on the hash, if you don't clear the hash to an empty string before updating the version, `nix-shell --pure` will run without error even though the dependency hasn't actually been updated.

Here's an example of what the error will look like when done correctly:

```bash
error: hash mismatch in fixed-output derivation '/nix/store/cc7hhyslx1dnw01nmjx11zqim2l50awp-source.drv':
         specified: sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
            got:    sha256-wUDWFazJM80oztKqpuRwj8Wvto2Uo/OuVGvhpszIw+A=
error: Cannot build '/nix/store/spg5vbm6mzmsxpg5v2ibg97qrz8khc70-openhcl-kernel-x64-6.12.52.4.drv'.
       Reason: 1 dependency failed.
       Output paths:
         /nix/store/kv8s7pld70yvzxzd77swz9hb3pygkrhl-openhcl-kernel-x64-6.12.52.4
```

Given this error, you would update the corresponding hash to `sha256-An1N76i1MPb+rrQ1nBpoiuxnNeD0E+VuwqXdkPzaZn0=` in the `openhcl_kernel.nix` file.

## Updating the Rust Version

The Nix shell derives its Rust toolchain version from `rust-version` in the root `Cargo.toml` and resolves it against a pinned [rust-overlay](https://github.com/oxalica/rust-overlay) commit in `shell.nix`. When `rust-version` is bumped in `Cargo.toml`, the pinned rust-overlay may not yet include the new version, causing an error like:

```bash
error: No rust version matching 1.XX.* found in rust-overlay
```

To fix this, update the rust-overlay pin in `shell.nix` to a commit that includes the new Rust version:

1. Go to the [rust-overlay stable branch commits](https://github.com/oxalica/rust-overlay/commits/stable) and copy the latest commit SHA
2. In `shell.nix`, find the `rust_overlay` `fetchTarball` block and replace the commit SHA in the `url` with the new one (the long hex string in the URL path, e.g., `https://github.com/oxalica/rust-overlay/archive/<commit-sha>.tar.gz`)
3. Clear the `sha256` field to an empty string — this is a _content_ hash of the tarball (not the commit SHA), and Nix will compute the correct value for you
4. Run `nix-shell --pure` and use the printed error to get the new `sha256`
5. Update the `sha256` with the correct hash from the error
