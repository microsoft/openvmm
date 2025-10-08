// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::protocol::*;
use std::io;
use std::io::Write;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Trait used by objects that send FUSE replies to the kernel.
pub trait ReplySender {
    /// Send the specified buffers to the kernel.
    fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()>;

    /// Send an empty reply.
    fn send_empty(&mut self, unique: u64) -> io::Result<()> {
        tracing::trace!(unique, "Reply");
        self.send_error(unique, 0)
    }

    /// Send a reply with a struct as an argument.
    fn send_arg<T: std::fmt::Debug + IntoBytes + Immutable + KnownLayout>(
        &mut self,
        unique: u64,
        arg: T,
    ) -> io::Result<()> {
        tracing::trace!(unique, ?arg, "reply");
        let header = make_header(unique, size_of_val(&arg), 0);
        let header = header.as_bytes();
        let arg = arg.as_bytes();
        self.send(&[io::IoSlice::new(header), io::IoSlice::new(arg)])
    }

    /// Send an error reply.
    fn send_error(&mut self, unique: u64, error: i32) -> io::Result<()> {
        if error != 0 {
            tracing::debug!(unique, error, "reply");
        }

        let header = make_header(unique, 0, -error);
        let header = header.as_bytes();
        self.send(&[io::IoSlice::new(header)])
    }

    /// Send a reply with arbitrary data.
    fn send_data(&mut self, unique: u64, data: &[u8]) -> io::Result<()> {
        tracing::trace!(unique, len = data.len(), "reply");
        let header = make_header(unique, data.len(), 0);
        let header = header.as_bytes();
        self.send(&[io::IoSlice::new(header), io::IoSlice::new(data)])
    }

    /// Send a reply with a struct argument and arbitrary data.
    fn send_arg_data<T: std::fmt::Debug + IntoBytes + Immutable + KnownLayout>(
        &mut self,
        unique: u64,
        arg: T,
        data: &[u8],
    ) -> io::Result<()> {
        tracing::trace!(unique, ?arg, len = data.len(), "reply");

        let header = make_header(unique, size_of_val(&arg), 0);
        let header = header.as_bytes();
        let arg = arg.as_bytes();
        self.send(&[
            io::IoSlice::new(header),
            io::IoSlice::new(arg),
            io::IoSlice::new(data),
        ])
    }

    /// Send a string reply.
    fn send_string(&mut self, unique: u64, value: lx::LxString) -> io::Result<()> {
        tracing::trace!(unique, len = value.len(), "reply");
        let header = make_header(unique, value.len() + 1, 0);
        let header = header.as_bytes();
        self.send(&[
            io::IoSlice::new(header),
            io::IoSlice::new(value.as_bytes()),
            io::IoSlice::new(&[0]),
        ])
    }
}

fn make_header(unique: u64, extra_len: usize, error: i32) -> fuse_out_header {
    fuse_out_header {
        len: (size_of::<fuse_out_header>() + extra_len)
            .try_into()
            .unwrap(),
        error,
        unique,
    }
}

/// Helpers for writing a reply for `read_dir` or `read_dir_plus`.
///
/// This trait is implemented on `Cursor<&mut Vec<u8>>`. To write directory entries, create an
/// appropriately-sized vector with `Vec::with_capacity`, wrap it in a `Cursor`, and write entries
/// using the methods of this trait.
///
/// The methods of this trait ensure that the entries are correctly aligned in the buffer.
pub trait DirEntryWriter {
    /// Write a directory entry to the buffer.
    ///
    /// Returns `true` if the entry fit in the buffer; otherwise, false.
    fn dir_entry(
        &mut self,
        name: impl AsRef<lx::LxStr>,
        inode_nr: u64,
        offset: u64,
        file_type: u32,
    ) -> bool;

    /// Write a directory entry for `read_dir_plus` to the buffer.
    ///
    /// Returns `true` if the entry fit in the buffer; otherwise, false.
    fn dir_entry_plus(
        &mut self,
        name: impl AsRef<lx::LxStr>,
        offset: u64,
        entry: fuse_entry_out,
    ) -> bool;

    /// Checks whether an entry for `read_dir_plus` will fit in the buffer without writing it.
    ///
    /// Returns `true` if the entry fit in the buffer; otherwise, false.
    fn check_dir_entry_plus(&self, name: impl AsRef<lx::LxStr>) -> bool;
}

impl DirEntryWriter for Vec<u8> {
    fn dir_entry(
        &mut self,
        name: impl AsRef<lx::LxStr>,
        inode_nr: u64,
        offset: u64,
        file_type: u32,
    ) -> bool {
        // Determine if it fits in the remaining capacity of the vector.
        let name = name.as_ref();
        let size = size_of::<fuse_dirent>() + name.len();
        let aligned_size = fuse_dirent_align(size);
        if self.capacity() - self.len() < aligned_size {
            return false;
        }

        // Write the entry.
        let dentry = fuse_dirent {
            ino: inode_nr,
            off: offset,
            namelen: name.len().try_into().unwrap(),
            file_type,
        };

        self.write_all(dentry.as_bytes()).unwrap();
        self.write_all(name.as_bytes()).unwrap();

        // Write padding for the next entry.
        write_padding(self, aligned_size - size);
        true
    }

    fn dir_entry_plus(
        &mut self,
        name: impl AsRef<lx::LxStr>,
        offset: u64,
        entry: fuse_entry_out,
    ) -> bool {
        // Determine if it fits in the remaining capacity of the vector.
        let name = name.as_ref();
        let size = size_of::<fuse_direntplus>() + name.len();
        let aligned_size = fuse_dirent_align(size);
        if self.capacity() - self.len() < aligned_size {
            return false;
        }

        // Write the entry.
        let mut dentry = fuse_direntplus::new_zeroed();
        dentry.dirent.ino = entry.attr.ino;
        dentry.dirent.off = offset;
        dentry.dirent.namelen = name.len().try_into().unwrap();
        dentry.dirent.file_type = (entry.attr.mode & lx::S_IFMT) >> 12;
        dentry.entry_out = entry;
        self.write_all(dentry.as_bytes()).unwrap();
        self.write_all(name.as_bytes()).unwrap();

        // Write padding for the next entry.
        write_padding(self, aligned_size - size);
        true
    }

    fn check_dir_entry_plus(&self, name: impl AsRef<lx::LxStr>) -> bool {
        let size = size_of::<fuse_direntplus>() + name.as_ref().len();
        let aligned_size = fuse_dirent_align(size);
        self.capacity() - self.len() >= aligned_size
    }
}

/// Write up to 8 zero bytes as padding.
fn write_padding(writer: &mut impl Write, count: usize) {
    const PADDING: [u8; 8] = [0; 8];
    writer.write_all(&PADDING[..count]).unwrap();
}
