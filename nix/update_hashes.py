#!/usr/bin/env python3
"""Helper for updating fetchzip/fetchurl SRI hashes in the .nix files in this folder.

Given one or more URLs, this script invokes ``nix-prefetch-url --unpack`` to
download and hash the unpacked archive contents (matching what ``fetchzip``
does), then converts the resulting Nix base32 hash into the SRI format
(``sha256-<base64>=``) used by these .nix files.

Usage:
    # Prefetch one or more URLs and print SRI hashes.
    ./update_hashes.py <url> [<url> ...]

    # Convert already-computed Nix base32 hashes to SRI without re-downloading.
    ./update_hashes.py --convert <nix32-hash> [<nix32-hash> ...]

Requires ``nix-prefetch-url`` on PATH (``sudo apt install nix-bin`` on Ubuntu /
WSL). On a multi-user Nix install you may need ``sudo`` to access the daemon
socket; in that case prefix the command with ``sudo``.

Example:
    sudo ./update_hashes.py \\
        https://github.com/microsoft/mu_msvm/releases/download/v26.0.3/RELEASE-X64-VS2022-artifacts.tar.gz \\
        https://github.com/microsoft/mu_msvm/releases/download/v26.0.3/RELEASE-AARCH64-CLANGPDB-artifacts.tar.gz
"""

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

from __future__ import annotations

import argparse
import base64
import shutil
import subprocess
import sys

# Nix base32 alphabet: omits 'e', 'o', 'u', 't' to avoid spelling words.
_NIX32_ALPHABET = "0123456789abcdfghijklmnpqrsvwxyz"


def nix32_to_bytes(s: str, hashlen: int = 32) -> bytes:
    """Decode a Nix base32 string (the format printed by ``nix-prefetch-url``)
    into its raw bytes.

    Nix base32 encodes characters in reverse order vs. position: char ``n`` of
    the encoded string holds bits ``[5n .. 5n+4]`` of the hash, but the string
    itself is reversed before encoding, so the most-significant bits appear
    first when read left-to-right.
    """
    if len(s) != (hashlen * 8 - 1) // 5 + 1:
        raise ValueError(
            f"unexpected nix32 length {len(s)} for {hashlen}-byte hash"
        )

    out = bytearray(hashlen)
    # Reverse the string so character index 0 corresponds to the lowest bits.
    for n, c in enumerate(reversed(s)):
        try:
            digit = _NIX32_ALPHABET.index(c)
        except ValueError as e:
            raise ValueError(f"invalid nix32 character: {c!r}") from e
        b = 5 * n
        i, j = b // 8, b % 8
        out[i] |= (digit << j) & 0xFF
        if i + 1 < hashlen:
            out[i + 1] |= (digit >> (8 - j)) & 0xFF
    return bytes(out)


def nix32_to_sri(nix32: str) -> str:
    """Convert a Nix base32 sha256 hash to SRI (``sha256-<base64>``) format."""
    raw = nix32_to_bytes(nix32)
    return "sha256-" + base64.b64encode(raw).decode("ascii")


def prefetch(url: str) -> str:
    """Download ``url`` via ``nix-prefetch-url --unpack`` and return the hash.

    The returned value is the Nix base32 string printed on the last line of
    ``nix-prefetch-url``'s stdout.
    """
    if shutil.which("nix-prefetch-url") is None:
        sys.exit(
            "error: nix-prefetch-url not found on PATH.\n"
            "Install it with: sudo apt install nix-bin"
        )

    result = subprocess.run(
        ["nix-prefetch-url", "--unpack", "--type", "sha256", url],
        check=True,
        capture_output=True,
        text=True,
    )
    # nix-prefetch-url prints progress on stderr and the hash as the last
    # non-empty line of stdout.
    lines = [line for line in result.stdout.splitlines() if line.strip()]
    if not lines:
        sys.exit(f"error: nix-prefetch-url produced no output for {url}")
    return lines[-1].strip()


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Prefetch URLs and print SRI hashes suitable for fetchzip in the "
            ".nix files in this folder."
        )
    )
    parser.add_argument(
        "args",
        nargs="*",
        metavar="URL_OR_HASH",
        help=(
            "URLs to prefetch with `nix-prefetch-url --unpack`, or, with "
            "--convert, Nix base32 hashes to convert to SRI."
        ),
    )
    parser.add_argument(
        "--convert",
        action="store_true",
        help=(
            "Treat positional arguments as Nix base32 hashes and convert "
            "them to SRI without re-downloading."
        ),
    )
    parsed = parser.parse_args()

    if not parsed.args:
        parser.print_help()
        return 2

    if parsed.convert:
        for h in parsed.args:
            print(nix32_to_sri(h))
        return 0

    for url in parsed.args:
        print(f"# {url}", file=sys.stderr)
        nix32 = prefetch(url)
        sri = nix32_to_sri(nix32)
        print(sri)

    return 0


if __name__ == "__main__":
    sys.exit(main())
