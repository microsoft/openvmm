// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "linux")]
#![allow(
    clippy::field_reassign_with_default, // protocol code benefits from imperative field assignment
)]

use fuse::protocol::*;
use fuse::Connection;
use fuse::*;
use std::fs;
use std::os::linux::fs::MetadataExt;
use std::path::Path;
use zerocopy::FromZeros;

// Implements a file system similar to libfuse's hello_ll sample.
// This test is excluded from CI because it requires root.
#[cfg_attr(not(feature = "ci"), test_with_tracing::test)]
#[cfg_attr(feature = "ci", allow(dead_code))]
fn fuse_hello() {
    let mount_point = tempfile::tempdir().unwrap();

    let mut conn = Connection::mount(&mount_point).expect("This test requires root");
    let handle = {
        let _unmounter = Unmounter {
            mount_point: mount_point.as_ref(),
        };

        let handle = std::thread::spawn(move || {
            conn.run(TestFs {}).unwrap();
        });

        std::thread::sleep(std::time::Duration::from_secs(1));
        let metadata = fs::metadata(&mount_point).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.st_ino(), 1);
        assert_eq!(metadata.st_nlink(), 2);
        assert_eq!(metadata.st_mode(), lx::S_IFDIR | 0o755);
        let metadata = fs::metadata(mount_point.path().join("hello")).unwrap();
        assert_eq!(metadata.st_ino(), 2);
        assert_eq!(metadata.st_nlink(), 1);
        assert_eq!(metadata.st_mode(), lx::S_IFREG | 0o644);
        assert_eq!(metadata.len(), 13);

        let contents = fs::read_to_string(mount_point.path().join("hello")).unwrap();
        assert_eq!(contents, "Hello, world!");
        {
            let dir: Vec<_> = fs::read_dir(&mount_point).unwrap().collect();
            // Our fs returned . and .., but fs::read_dir hides them.
            assert_eq!(dir.len(), 1);
            let entry = dir[0].as_ref().unwrap();
            assert_eq!(entry.file_name(), "hello");
        }

        handle
    };

    handle.join().unwrap();
}

struct TestFs {}

impl Fuse for TestFs {
    fn get_attr(&self, request: &Request, _flags: u32, _fh: u64) -> lx::Result<fuse_attr_out> {
        let mut attr = fuse_attr_out::new_zeroed();
        attr.attr_valid = 1;
        attr.attr = Self::stat(request.node_id())?;
        Ok(attr)
    }

    fn lookup(&self, request: &Request, name: &lx::LxStr) -> lx::Result<fuse_entry_out> {
        if request.node_id() != FUSE_ROOT_ID || name != "hello" {
            return Err(lx::Error::ENOENT);
        }

        Ok(fuse_entry_out {
            nodeid: 2,
            generation: 0,
            entry_valid: 1,
            entry_valid_nsec: 0,
            attr_valid: 1,
            attr_valid_nsec: 0,
            attr: Self::stat(2)?,
        })
    }

    fn open(&self, request: &Request, flags: u32) -> lx::Result<fuse_open_out> {
        if request.node_id() != 2 {
            return Err(lx::Error::EISDIR);
        }

        let flags = flags as i32;
        if flags & lx::O_ACCESS_MASK != lx::O_RDONLY {
            return Err(lx::Error::EACCES);
        }

        Ok(fuse_open_out {
            fh: 1,
            open_flags: 0,
            padding: 0,
        })
    }

    fn read(&self, request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        assert!(request.node_id() == 2);
        let len = Self::HELLO_CONTENTS.len() as u64;
        if arg.offset >= len {
            return Ok(Vec::new());
        }

        let end = std::cmp::min(arg.offset + arg.size as u64, len) as usize;
        Ok(Vec::from(&Self::HELLO_CONTENTS[arg.offset as usize..end]))
    }

    fn read_dir(&self, request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        assert!(request.node_id() == FUSE_ROOT_ID);
        let mut writer = Vec::with_capacity(arg.size as usize);
        let mut offset = arg.offset;
        let mut has_entry = false;
        loop {
            match offset {
                0 => {
                    has_entry = true;
                    if !writer.dir_entry(".", FUSE_ROOT_ID, offset + 1, lx::DT_DIR as u32) {
                        break;
                    }
                }
                1 => {
                    has_entry = true;
                    if !writer.dir_entry("..", FUSE_ROOT_ID, offset + 1, lx::DT_DIR as u32) {
                        break;
                    }
                }
                2 => {
                    has_entry = true;
                    if !writer.dir_entry("hello", 2, offset + 1, lx::DT_REG as u32) {
                        break;
                    }
                }
                _ => {
                    break;
                }
            }

            offset += 1;
        }

        if writer.is_empty() && has_entry {
            return Err(lx::Error::EINVAL);
        }

        Ok(writer)
    }
}

impl TestFs {
    const HELLO_CONTENTS: &'static [u8] = b"Hello, world!";

    fn stat(ino: u64) -> lx::Result<fuse_attr> {
        let mut attr = fuse_attr::new_zeroed();
        attr.ino = ino;
        match ino {
            FUSE_ROOT_ID => {
                attr.mode = lx::S_IFDIR | 0o755;
                attr.nlink = 2;
            }
            2 => {
                attr.mode = lx::S_IFREG | 0o644;
                attr.nlink = 1;
                attr.size = Self::HELLO_CONTENTS.len() as u64;
            }
            _ => {
                return Err(lx::Error::ENOENT);
            }
        }

        Ok(attr)
    }
}

struct Unmounter<'a> {
    mount_point: &'a Path,
}

impl Drop for Unmounter<'_> {
    fn drop(&mut self) {
        if let Err(e) = Connection::unmount(self.mount_point, libc::MNT_DETACH) {
            tracing::error!("Unmount failed: {}", e);
        }

        if let Err(e) = fs::remove_dir_all(self.mount_point) {
            tracing::error!("Rmdir failed: {}", e);
        }
    }
}
