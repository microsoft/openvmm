// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt;
use std::slice;

#[macro_use]
mod macros;

pub const PROTOCOL_VERSION: &str = "9P2000.L";
const HEADER_SIZE: usize = 7;
const QID_SIZE: usize = 13;

// These messages use the format:
// id name (arg_name[size])*
// The size indicates the type:
// 1, 2, 4, 8: u8, u16, u32 or u64.
// q: A qid (1 byte type, 4 byte version, 8 byte path)
// s: A 2 byte length followed by a string of that length.
// n: A 2 byte length followed by a name of that length  (a name is a string with extra verification).
// ns: A 2 byte count followed by count names.
// qs: A 2 byte count followed by count qids.
p9_protocol_messages! {
    6 Tlerror ecode[4];
    7 Rlerror ecode[4];
    12 Tlopen fid[4] flags[4];
    13 Rlopen qid[q] iounit[4];
    14 Tlcreate fid[4] name[n] flags[4] mode[4] gid[4];
    15 Rlcreate qid[q] iounit[4];
    24 Tgetattr fid[4] request_mask[8];
    25 Rgetattr valid[8] qid[q] mode[4] uid[4] gid[4] nlink[8] rdev[8] size[8] blksize[8] blocks[8] atime_sec[8] atime_nsec[8]
        mtime_sec[8] mtime_nsec[8] ctime_sec[8] ctime_nsec[8] btime_sec[8] btime_nsec[8] r#gen[8] data_version[8];
    26 Tsetattr fid[4] valid[4] mode[4] uid[4] gid[4] size[8] atime_sec[8] atime_nsec[8] mtime_sec[8] mtime_nsec[8];
    27 Rsetattr;
    40 Treaddir fid[4] offset[8] count[4];
    41 Rreaddir count[4]; // data[count]
    72 Tmkdir dfid[4] name[n] mode[4] gid[4];
    73 Rmkdir qid[q];
    76 Tunlinkat dfid[4] name[n] flags[4];
    77 Runlinkat;
    100 Tversion msize[4] version[s];
    101 Rversion msize[4] version[s];
    104 Tattach fid[4] afid[4] uname[s] aname[s] n_uname[4];
    105 Rattach qid[q];
    110 Twalk fid[4] newfid[4] wnames[ns];
    111 Rwalk wqids[qs];
    116 Tread fid[4] offset[8] count[4];
    117 Rread count[4]; // data[count]
    118 Twrite fid[4] offset[8] count[4]; // data[count]
    119 Rwrite count[4];
    120 Tclunk fid[4];
    121 Rclunk;
}

// Plan 9 message types.
pub const MESSAGE_RLERROR: u8 = 7;

// Qid file types.
pub const QID_TYPE_FILE: u8 = 0x00;
pub const QID_TYPE_SYMLINK: u8 = 0x02;
pub const QID_TYPE_DIRECTORY: u8 = 0x80;

// Flags used by the Tlopen message.
// N.B. Omitted flags match their Linux equivalent.
pub const OPEN_FLAG_DIRECTORY: u32 = 0o200000;

// The header of a 9p protocol message.
pub struct Header {
    pub size: u32,
    pub message_type: u8,
    pub tag: u16,
}

// Identifies a file by its inode number and type.
#[derive(Default, Copy, Clone, Debug)]
pub struct Qid {
    pub path: u64,
    pub version: u32,
    pub qid_type: u8,
}

// Helper to extract fields from a buffer of bytes.
#[derive(Clone)]
pub struct SliceReader<'a> {
    slice: &'a [u8],
    offset: usize,
}

impl<'a> SliceReader<'a> {
    pub fn new(slice: &'a [u8]) -> SliceReader<'a> {
        SliceReader { slice, offset: 0 }
    }

    pub fn u8(&mut self) -> lx::Result<u8> {
        let result = self.slice.get(self.offset).ok_or(lx::Error::EINVAL)?;

        self.offset += 1;
        Ok(*result)
    }

    // Unfortunately this can't be done with generics because there is no trait for from_le_bytes.
    pub fn u16(&mut self) -> lx::Result<u16> {
        Ok(u16::from_le_bytes(
            self.read(size_of::<u16>())?.try_into().unwrap(),
        ))
    }

    pub fn u32(&mut self) -> lx::Result<u32> {
        Ok(u32::from_le_bytes(
            self.read(size_of::<u32>())?.try_into().unwrap(),
        ))
    }

    pub fn u64(&mut self) -> lx::Result<u64> {
        Ok(u64::from_le_bytes(
            self.read(size_of::<u64>())?.try_into().unwrap(),
        ))
    }

    pub fn read(&mut self, count: usize) -> lx::Result<&'a [u8]> {
        let end = self.offset + count;
        let result = &self.slice.get(self.offset..end).ok_or(lx::Error::EINVAL)?;

        self.offset = end;
        Ok(result)
    }

    pub fn header(&mut self) -> lx::Result<Header> {
        Ok(Header {
            size: self.u32()?,
            message_type: self.u8()?,
            tag: self.u16()?,
        })
    }

    // Read a string preceded with two length bytes.
    pub fn string(&mut self) -> lx::Result<&'a lx::LxStr> {
        let length = self.u16()?;
        Ok(Self::fix_string(self.read(length as usize)?))
    }

    // Read a valid path name component.
    pub fn name(&mut self) -> lx::Result<&'a lx::LxStr> {
        let name = self.string()?;
        if name.is_empty() || name == "." || name == ".." || name.as_bytes().contains(&b'/') {
            return Err(lx::Error::EINVAL);
        }

        Ok(name)
    }

    // Read zero or more valid path name components.
    // N.B. The iterator will use a separate SliceReader, so the offset of this slice reader will
    //      not be updated. Therefore, only use this if the names are the final item (which is
    //      always the case in 9p).
    pub fn names(&mut self) -> lx::Result<NameIterator<'a>> {
        let count = self.u16()?;
        Ok(NameIterator {
            reader: SliceReader::new(&self.slice[self.offset..]),
            count,
            index: 0,
        })
    }

    // Read a qid from protocol order.
    pub fn qid(&mut self) -> lx::Result<Qid> {
        Ok(Qid {
            qid_type: self.u8()?,
            version: self.u32()?,
            path: self.u64()?,
        })
    }

    // Read zero or more qids.
    // N.B. The iterator will use a separate SliceReader, so the offset of this slice reader will
    //      not be updated. Therefore, only use this if the qids are the final item (which is
    //      always the case in 9p).
    pub fn qids(&mut self) -> lx::Result<QidIterator<'a>> {
        let count = self.u16()?;
        Ok(QidIterator {
            reader: SliceReader::new(&self.slice[self.offset..]),
            count,
            index: 0,
        })
    }

    // Make sure a string doesn't contain internal NULL characters.
    fn fix_string(bytes: &[u8]) -> &lx::LxStr {
        let index = if let Some(index) = bytes.iter().position(|c| c == &b'\0') {
            index
        } else {
            bytes.len()
        };

        lx::LxStr::from_bytes(&bytes[..index])
    }
}

// Iterator to read zero or more names.
#[derive(Clone)]
pub struct NameIterator<'a> {
    reader: SliceReader<'a>,
    count: u16,
    index: u16,
}

impl NameIterator<'_> {
    pub fn name_count(&self) -> u16 {
        self.count
    }
}

impl<'a> Iterator for NameIterator<'a> {
    type Item = lx::Result<&'a lx::LxStr>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.count {
            self.index += 1;
            Some(self.reader.name())
        } else {
            None
        }
    }
}

impl fmt::Debug for NameIterator<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

#[derive(Clone)]
pub struct QidIterator<'a> {
    reader: SliceReader<'a>,
    count: u16,
    index: u16,
}

impl Iterator for QidIterator<'_> {
    type Item = lx::Result<Qid>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.count {
            self.index += 1;
            Some(self.reader.qid())
        } else {
            None
        }
    }
}

impl fmt::Debug for QidIterator<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

// Helper to write fields to a buffer of bytes.
pub struct SliceWriter<'a> {
    slice: &'a mut [u8],
    offset: usize,
}

impl<'a> SliceWriter<'a> {
    // Creat an instance to write after the header.
    pub fn new(slice: &'a mut [u8]) -> SliceWriter<'a> {
        SliceWriter {
            slice,
            offset: HEADER_SIZE,
        }
    }

    // Creat an instance that doesn't skip the header.
    pub fn new_raw(slice: &'a mut [u8]) -> SliceWriter<'a> {
        SliceWriter { slice, offset: 0 }
    }

    pub fn u8(&mut self, value: u8) -> lx::Result<()> {
        *self.slice.get_mut(self.offset).ok_or(lx::Error::EINVAL)? = value;

        self.offset += 1;
        Ok(())
    }

    // Unfortunately this can't be done with generics because there is no trait for to_le_bytes.
    pub fn u16(&mut self, value: u16) -> lx::Result<()> {
        self.write(&value.to_le_bytes())
    }

    pub fn u32(&mut self, value: u32) -> lx::Result<()> {
        self.write(&value.to_le_bytes())
    }

    pub fn u64(&mut self, value: u64) -> lx::Result<()> {
        self.write(&value.to_le_bytes())
    }

    pub fn write(&mut self, value: &[u8]) -> lx::Result<()> {
        self.next(value.len())?.copy_from_slice(value);
        Ok(())
    }

    // Write a 9p header at the start of the buffer. This uses the current offset to determine
    // the size of the message.
    pub fn header(&mut self, message_type: u8, tag: u16) -> lx::Result<()> {
        let mut writer = SliceWriter {
            slice: self.slice,
            offset: 0,
        };
        writer.u32(self.offset as u32)?;
        writer.u8(message_type)?;
        writer.u16(tag)
    }

    // Write a string preceded by a two byte length.
    pub fn string(&mut self, s: &lx::LxStr) -> lx::Result<()> {
        self.u16(s.len() as u16)?;
        self.write(s.as_bytes())
    }

    // Write a qid in protocol order.
    pub fn qid(&mut self, qid: &Qid) -> lx::Result<()> {
        self.u8(qid.qid_type)?;
        self.u32(qid.version)?;
        self.u64(qid.path)
    }

    // Write a duration as two 64 bit values.
    pub fn timespec(&mut self, timespec: &lx::Timespec) -> lx::Result<()> {
        self.u64(timespec.seconds as u64)?;
        self.u64(timespec.nanoseconds as u64)
    }

    // Write a directory entry.
    pub fn dir_entry(
        &mut self,
        name: &lx::LxStr,
        qid: &Qid,
        next_offset: u64,
        file_type: u8,
    ) -> bool {
        let size = QID_SIZE + size_of::<u64>() + size_of::<u8>() + size_of::<u16>() + name.len();
        if self.slice.len() - self.offset < size {
            return false;
        }

        self.qid(qid).unwrap();
        self.u64(next_offset).unwrap();
        self.u8(file_type).unwrap();
        self.string(name).unwrap();
        true
    }

    // Get a partial slice without updating the offset.
    pub fn peek<I>(&mut self, index: I) -> lx::Result<&mut [u8]>
    where
        I: slice::SliceIndex<[u8], Output = [u8]>,
    {
        self.slice[self.offset..]
            .get_mut(index)
            .ok_or(lx::Error::EINVAL)
    }

    // Get a partial slice and update the offset to after it.
    pub fn next(&mut self, count: usize) -> lx::Result<&mut [u8]> {
        let start = self.offset;
        let end = start + count;
        let r = self.slice.get_mut(start..end).ok_or(lx::Error::EINVAL)?;
        self.offset = end;
        Ok(r)
    }

    // Gets the currently written size.
    pub fn size(&self) -> usize {
        self.offset
    }

    // Resets the write position to after the header.
    pub fn reset(&mut self) {
        self.offset = HEADER_SIZE;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_writer_next_doesnt_advance_on_error() {
        let mut buf = vec![0u8; HEADER_SIZE + 4];
        let mut w = SliceWriter::new(&mut buf);

        // This succeeds and moves offset to HEADER_SIZE + 2.
        w.next(2).unwrap();
        assert_eq!(w.size(), HEADER_SIZE + 2);

        // This request exceeds remaining capacity by 1, so it returns Err.
        // However, next() still advances the internal offset, which is a bug.
        assert!(w.next(HEADER_SIZE).is_err());

        // The offset should not have changed on error. If it did, this assert will panic.
        assert_eq!(w.size(), HEADER_SIZE + 2);
    }
}
