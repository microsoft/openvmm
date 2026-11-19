# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Exercise Linux filesystem syscalls against the virtio-9p share."""

import errno
import os
from pathlib import Path
import shutil
import subprocess


def expect_errno(expected, operation):
    try:
        operation()
    except OSError as error:
        assert error.errno == expected, error
    else:
        raise AssertionError(f"operation should fail with errno {expected}")


root = Path("/mnt/plan9")
with (root / "file").open("xb", buffering=0) as file:
    file.write(b"from guest")
    os.fdatasync(file.fileno())
    os.fsync(file.fileno())

directory = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
try:
    os.fsync(directory)
finally:
    os.close(directory)

stat = os.statvfs(root)
assert stat.f_bsize > 0
assert stat.f_blocks > 0
assert 0 <= stat.f_bavail <= stat.f_bfree <= stat.f_blocks
assert stat.f_namemax >= len("host-file")

assert os.readlink(root / "host-link") == "host-file"
assert (root / "host-link").read_bytes() == b"from host"
os.symlink("file", root / "symlink")
assert os.readlink(root / "symlink") == "file"
assert (root / "symlink").read_bytes() == b"from guest"
expect_errno(errno.EEXIST, lambda: os.symlink("file", root / "symlink"))
expect_errno(errno.EINVAL, lambda: os.readlink(root / "file"))

os.link(root / "file", root / "hardlink")
assert os.stat(root / "file").st_ino == os.stat(root / "hardlink").st_ino
assert os.stat(root / "file").st_nlink == 2
assert (root / "hardlink").read_bytes() == b"from guest"
expect_errno(errno.EEXIST, lambda: os.link(root / "file", root / "hardlink"))

expect_errno(
    errno.EOPNOTSUPP, lambda: os.getxattr(root / "file", "security.capability")
)
expect_errno(
    errno.EOPNOTSUPP, lambda: os.setxattr(root / "file", "user.test", b"value")
)

# Executing a shared binary must tolerate unsupported file capabilities.
executable = root / "true"
fd = os.open(executable, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o755)
with os.fdopen(fd, "wb") as destination, open("/bin/true", "rb") as source:
    shutil.copyfileobj(source, destination)
subprocess.run([executable], check=True)
