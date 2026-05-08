#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Helper for updating fetchzip / fetchurl SRI hashes in the .nix files in this folder.

Given one or more URLs, this script invokes ``nix-prefetch-url`` to download
and hash the artifact, then converts the resulting Nix base32 hash into the
SRI format (``sha256-<base64>=``) used by these .nix files.

By default the script passes ``--unpack``, which matches the semantics of
``fetchzip`` (the archive is unpacked and the unpacked tree is hashed). Pass
``--no-unpack`` to hash the artifact bytes directly, which matches the
semantics of ``fetchurl`` (or ``fetchzip { stripRoot = false; }`` over a
non-archive blob).

Usage:
    # Prefetch one or more URLs (fetchzip semantics, default).
    ./update_hashes.py <url> [<url> ...]

    # Prefetch one or more URLs without unpacking (fetchurl semantics).
    ./update_hashes.py --no-unpack <url> [<url> ...]

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

from __future__ import annotations

import argparse
import base64
import re
import shutil
import subprocess
import sys

# Nix base32 alphabet: omits 'e', 'o', 'u', 't' to avoid spelling words.
_NIX32_ALPHABET = "0123456789abcdfghijklmnpqrsvwxyz"

# O(1) char -> 5-bit value lookup, also used to validate input characters.
_NIX32_VALUES = {c: i for i, c in enumerate(_NIX32_ALPHABET)}

# Length in characters of a Nix base32-encoded sha256 hash.
_NIX32_SHA256_LEN = (32 * 8 - 1) // 5 + 1  # = 52

# Matches a standalone Nix base32 sha256 hash: a 52-char run of alphabet chars
# with no surrounding alphanumeric context (so we don't latch onto a substring
# of a store path or other identifier).
_NIX32_SHA256_RE = re.compile(
    rf"(?<![0-9a-z])[{_NIX32_ALPHABET}]{{{_NIX32_SHA256_LEN}}}(?![0-9a-z])"
)


def nix32_to_bytes(s: str, hashlen: int = 32) -> bytes:
    """Decode a Nix base32 string (the format printed by ``nix-prefetch-url``)
    into its raw bytes.

    Nix base32 encodes characters in reverse order vs. position: char ``n`` of
    the encoded string holds bits ``[5n .. 5n+4]`` of the hash, but the string
    itself is reversed before encoding, so the most-significant bits appear
    first when read left-to-right.
    """
    expected_len = (hashlen * 8 - 1) // 5 + 1
    if len(s) != expected_len:
        raise ValueError(
            f"unexpected nix32 length {len(s)} for {hashlen}-byte hash "
            f"(expected {expected_len})"
        )

    out = bytearray(hashlen)
    # Reverse the string so character index 0 corresponds to the lowest bits.
    for n, c in enumerate(reversed(s)):
        digit = _NIX32_VALUES.get(c)
        if digit is None:
            raise ValueError(f"invalid nix32 character: {c!r}")
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


def _extract_hash(stdout: str, url: str) -> str:
    """Find the single Nix base32 sha256 hash in ``nix-prefetch-url`` output.

    ``nix-prefetch-url`` may print the hash alone, or accompanied by a store
    path line (e.g. with ``--print-path``); the exact format also varies
    across Nix versions. To be robust we scan all of stdout for tokens
    matching the expected hash shape and require exactly one such token.
    """
    candidates = _NIX32_SHA256_RE.findall(stdout)
    # Filter out any hash-shaped substring of a store path. Store paths look
    # like ``/nix/store/<32-char-hash>-name``; the leading hash there is 32
    # chars (not 52), so it can't match our regex. But a future Nix could
    # plausibly emit something else, so guard explicitly:
    candidates = [c for c in candidates if "/nix/store/" not in c]
    # Deduplicate while preserving order — some output formats print the hash
    # twice (once on its own line, once embedded in a path).
    seen: set[str] = set()
    unique = [c for c in candidates if not (c in seen or seen.add(c))]
    if not unique:
        sys.exit(
            f"error: could not find a sha256 hash in nix-prefetch-url "
            f"output for {url}\n"
            f"--- stdout ---\n{stdout}"
        )
    if len(unique) > 1:
        sys.exit(
            f"error: found multiple candidate hashes in nix-prefetch-url "
            f"output for {url}: {unique}\n"
            f"--- stdout ---\n{stdout}"
        )
    return unique[0]


def prefetch(url: str, *, unpack: bool) -> str:
    """Download ``url`` via ``nix-prefetch-url`` and return the Nix base32 hash.

    When ``unpack`` is True, ``--unpack`` is passed (matching ``fetchzip``).
    When False, the artifact is hashed verbatim (matching ``fetchurl``).
    """
    if shutil.which("nix-prefetch-url") is None:
        sys.exit(
            "error: nix-prefetch-url not found on PATH.\n"
            "Install it with: sudo apt install nix-bin"
        )

    cmd = ["nix-prefetch-url", "--type", "sha256"]
    if unpack:
        cmd.append("--unpack")
    cmd.append(url)
    result = subprocess.run(cmd, check=True, capture_output=True, text=True)
    return _extract_hash(result.stdout, url)


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Prefetch URLs and print SRI hashes suitable for fetchzip / "
            "fetchurl in the .nix files in this folder."
        )
    )
    parser.add_argument(
        "args",
        nargs="*",
        metavar="URL_OR_HASH",
        help=(
            "URLs to prefetch with `nix-prefetch-url`, or, with --convert, "
            "Nix base32 hashes to convert to SRI."
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
    parser.add_argument(
        "--no-unpack",
        dest="unpack",
        action="store_false",
        default=True,
        help=(
            "Hash the artifact bytes directly instead of unpacking first. "
            "Use this for `fetchurl` (non-archive) sources. Ignored with "
            "--convert. Default: --unpack (matches `fetchzip`)."
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
        nix32 = prefetch(url, unpack=parsed.unpack)
        sri = nix32_to_sri(nix32)
        print(sri)

    return 0


if __name__ == "__main__":
    sys.exit(main())
