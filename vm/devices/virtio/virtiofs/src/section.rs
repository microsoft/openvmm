// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a filesystem backed by Windows section objects (rather than
//! files). This is useful for sharing memory between the host and the guest.

// UNSAFETY: Calling Win32 Section Object APIs.
#![expect(unsafe_code)]

use crate::HandleMap;
use fuse::Mapper;
use fuse::protocol::FUSE_SETUPMAPPING_FLAG_WRITE;
use lxutil::PathExt;
use ntapi::ntmmapi::NtCreateSection;
use ntapi::ntmmapi::NtOpenSection;
use ntapi::ntmmapi::NtQuerySection;
use ntapi::ntmmapi::SECTION_BASIC_INFORMATION;
use ntapi::ntmmapi::SectionBasicInformation;
use ntapi::ntobapi::DIRECTORY_TRAVERSE;
use ntapi::winapi::shared::ntdef::LARGE_INTEGER;
use ntapi::winapi::um::winnt::PAGE_READWRITE;
use ntapi::winapi::um::winnt::SEC_COMMIT;
use ntapi::winapi::um::winnt::SECTION_MAP_READ;
use ntapi::winapi::um::winnt::SECTION_MAP_WRITE;
use ntapi::winapi::um::winnt::SECTION_QUERY;
use pal::windows::ObjectAttributes;
use pal::windows::UnicodeString;
use pal::windows::chk_status;
use pal::windows::open_object_directory;
use parking_lot::Mutex;
use parking_lot::RwLock;
use std::io;
use std::mem::zeroed;
use std::os::windows::prelude::*;
use std::path::Path;
use std::ptr::null_mut;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const PAGE_SIZE: u64 = 4096;

fn get_attr_with_cur_time() -> fuse::protocol::fuse_attr {
    let cur_time = lx::Timespec::from(&SystemTime::now().duration_since(UNIX_EPOCH).unwrap());
    fuse::protocol::fuse_attr {
        ino: 0,
        size: 0,
        blocks: 0,
        atime: cur_time.seconds as u64,
        mtime: cur_time.seconds as u64,
        ctime: cur_time.seconds as u64,
        atimensec: cur_time.nanoseconds as u32,
        mtimensec: cur_time.nanoseconds as u32,
        ctimensec: cur_time.nanoseconds as u32,
        mode: 0,
        nlink: 0,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 0,
        padding: 0,
    }
}

struct SectionFsInheritedAttrs {
    pub mode: RwLock<u32>,
    pub uid: RwLock<u32>,
    pub gid: RwLock<u32>,
}

impl SectionFsInheritedAttrs {
    pub fn new(mode: u32, uid: u32, gid: u32) -> Self {
        Self {
            mode: RwLock::new(mode),
            uid: RwLock::new(uid),
            gid: RwLock::new(gid),
        }
    }

    pub fn mode(&self) -> u32 {
        *self.mode.read()
    }

    pub fn uid(&self) -> u32 {
        *self.uid.read()
    }

    pub fn gid(&self) -> u32 {
        *self.gid.read()
    }

    pub fn update(&self, new_attrs: &fuse::protocol::fuse_setattr_in) {
        if new_attrs.valid & fuse::protocol::FATTR_MODE != 0 {
            *self.mode.write() = new_attrs.mode & 0o777;
        }
        if new_attrs.valid & fuse::protocol::FATTR_UID != 0 {
            *self.uid.write() = new_attrs.uid;
        }
        if new_attrs.valid & fuse::protocol::FATTR_GID != 0 {
            *self.gid.write() = new_attrs.gid;
        }
    }
}

/// A named section-object-backed file system.
pub struct SectionFs {
    root: OwnedHandle,
    inodes: Mutex<HandleMap<Inode>>,
    // Only the root mode can be updated, so all child inodes inherit these values.
    attr: SectionFsInheritedAttrs,
}

/// An inode in `SectionFs`.
struct Inode {
    size: u64,
    state: InodeState,
    lookup_count: u64,
}

/// The state of an `Inode`.
enum InodeState {
    /// The section exists and is open.
    Open(Section),
    /// The inode has been created, but the section has not. It will be created
    /// with the size is specified via `fallocate`.
    Pending(UnicodeString),
}

impl Inode {
    /// The attributes for an inode.
    fn attr(
        &self,
        node_id: u64,
        shared_attrs: &SectionFsInheritedAttrs,
    ) -> fuse::protocol::fuse_attr {
        fuse::protocol::fuse_attr {
            ino: node_id,
            size: self.size,
            blocks: self.size / PAGE_SIZE,
            mode: lx::S_IFREG | shared_attrs.mode(),
            nlink: 1,
            uid: shared_attrs.uid(),
            gid: shared_attrs.gid(),
            blksize: PAGE_SIZE as u32,
            ..get_attr_with_cur_time()
        }
    }

    fn section(&self) -> lx::Result<&Section> {
        match &self.state {
            InodeState::Open(section) => Ok(section),
            InodeState::Pending(_) => Err(lx::Error::EINVAL),
        }
    }
}

/// An NT section object.
struct Section(OwnedHandle);

impl Section {
    /// Creates a new pagefile-backed section object of `size` bytes.
    fn new(obj_attr: &ObjectAttributes<'_>, access: u32, size: u64) -> io::Result<Self> {
        let mut large_int_size: LARGE_INTEGER = Default::default();

        // SAFETY: calling the API according to the NT API
        unsafe {
            *large_int_size.QuadPart_mut() = size.try_into().expect("size fits in an i64");
            let mut handle = null_mut();
            chk_status(NtCreateSection(
                &mut handle,
                access,
                obj_attr.as_ptr(),
                &mut large_int_size,
                PAGE_READWRITE,
                SEC_COMMIT,
                null_mut(),
            ))?;
            Ok(Self(OwnedHandle::from_raw_handle(handle)))
        }
    }

    /// Opens a named pagefile-backed section object.
    fn open(obj_attr: &ObjectAttributes<'_>, access: u32) -> io::Result<Self> {
        // SAFETY: calling the API according to the NT API
        unsafe {
            let mut handle = null_mut();
            chk_status(NtOpenSection(&mut handle, access, obj_attr.as_ptr()))?;
            Ok(Self(OwnedHandle::from_raw_handle(handle)))
        }
    }

    /// Queries the size of the section object.
    fn query_size(&self) -> io::Result<u64> {
        // SAFETY: calling the API according to the NT API
        unsafe {
            let mut info: SECTION_BASIC_INFORMATION = zeroed();
            chk_status(NtQuerySection(
                self.0.as_raw_handle(),
                SectionBasicInformation,
                std::ptr::from_mut(&mut info).cast(),
                size_of_val(&info),
                null_mut(),
            ))?;
            Ok(*info.MaximumSize.QuadPart() as u64)
        }
    }
}

impl SectionFs {
    /// Creates a new file system, rooted in the NT directory object at `path`.
    pub fn new<P: AsRef<Path>>(path: P) -> lx::Result<Self> {
        let upath: UnicodeString = path
            .as_ref()
            .try_into()
            .map_err(|_| lx::Error::ENAMETOOLONG)?;

        let root = open_object_directory(ObjectAttributes::new().name(&upath), DIRECTORY_TRAVERSE)?;

        Ok(Self {
            root,
            inodes: Mutex::new(HandleMap::starting_at(2)), // reserve inode 1 for the root
            attr: SectionFsInheritedAttrs::new(0o600, 0, 0),
        })
    }
}

impl fuse::Fuse for SectionFs {
    fn lookup(
        &self,
        request: &fuse::Request,
        name: &lx::LxStr,
    ) -> lx::Result<fuse::protocol::fuse_entry_out> {
        if request.node_id() != 1 {
            return Err(lx::Error::ENOTDIR);
        }

        let path = Path::from_lx(name)?;
        let upath: UnicodeString = path
            .as_ref()
            .try_into()
            .map_err(|_| lx::Error::ENAMETOOLONG)?;
        let section = Section::open(
            ObjectAttributes::new()
                .root(self.root.as_handle())
                .name(&upath),
            SECTION_QUERY | SECTION_MAP_READ | SECTION_MAP_WRITE,
        )?;

        let size = section.query_size().map_err(|_| lx::Error::EINVAL)?;
        // N.B. Each lookup() creates a new node_id, so lookup_count is always 1.
        let inode = Inode {
            size,
            state: InodeState::Open(section),
            lookup_count: 1,
        };
        let attr = inode.attr(0, &self.attr);
        let node_id = self.inodes.lock().insert(inode);
        tracing::trace!(node_id, name = name.to_str().unwrap(), "node_id");
        Ok(fuse::protocol::fuse_entry_out::new(
            node_id,
            Duration::from_secs(0),
            Duration::from_secs(60),
            attr,
        ))
    }

    fn forget(&self, node_id: u64, lookup_count: u64) {
        let mut inodes = self.inodes.lock();
        if let Some(inode) = inodes.get_mut(node_id) {
            if lookup_count > inode.lookup_count {
                tracing::warn!(
                    node_id,
                    lookup_count,
                    current_lookup_count = inode.lookup_count,
                    "received forget with lookup_count greater than tracked count"
                );
                inode.lookup_count = 0;
            } else {
                inode.lookup_count -= lookup_count;
            }
            if inode.lookup_count == 0 {
                inodes.remove(node_id);
            }
        }
    }

    fn get_attr(
        &self,
        request: &fuse::Request,
        _flags: u32,
        _fh: u64,
    ) -> lx::Result<fuse::protocol::fuse_attr_out> {
        let attr = if request.node_id() == 1 {
            fuse::protocol::fuse_attr {
                ino: 1,
                mode: lx::S_IFDIR | self.attr.mode(),
                nlink: 1,
                uid: self.attr.uid(),
                gid: self.attr.gid(),
                ..get_attr_with_cur_time()
            }
        } else {
            let inodes = self.inodes.lock();
            let inode = inodes.get(request.node_id()).ok_or(lx::Error::EINVAL)?;
            inode.attr(request.node_id(), &self.attr)
        };
        Ok(fuse::protocol::fuse_attr_out::new(
            Duration::from_secs(60),
            attr,
        ))
    }

    fn set_attr(
        &self,
        request: &fuse::Request,
        arg: &fuse::protocol::fuse_setattr_in,
    ) -> lx::Result<fuse::protocol::fuse_attr_out> {
        if request.node_id() == 1 {
            self.attr.update(arg);
            self.get_attr(request, 0, 0)
        } else {
            Err(lx::Error::EINVAL)
        }
    }

    fn unlink(&self, _request: &fuse::Request, _name: &lx::LxStr) -> lx::Result<()> {
        Err(lx::Error::EINVAL)
    }

    fn rename(
        &self,
        _request: &fuse::Request,
        _name: &lx::LxStr,
        _new_dir: u64,
        _new_name: &lx::LxStr,
        _flags: u32,
    ) -> lx::Result<()> {
        Err(lx::Error::EINVAL)
    }

    fn statfs(&self, _request: &fuse::Request) -> lx::Result<fuse::protocol::fuse_kstatfs> {
        Ok(fuse::protocol::fuse_kstatfs::new(
            0, 0, 0, 0, 0, 4096, 4096, 0,
        ))
    }

    fn create(
        &self,
        request: &fuse::Request,
        name: &lx::LxStr,
        arg: &fuse::protocol::fuse_create_in,
    ) -> lx::Result<fuse::CreateOut> {
        if request.node_id() != 1 {
            return Err(lx::Error::ENOTDIR);
        }
        let path = Path::from_lx(name)?;
        let upath: UnicodeString = path
            .as_ref()
            .try_into()
            .map_err(|_| lx::Error::ENAMETOOLONG)?;
        let (size, state) = match Section::open(
            ObjectAttributes::new()
                .root(self.root.as_handle())
                .name(&upath),
            SECTION_QUERY | SECTION_MAP_READ | SECTION_MAP_WRITE,
        ) {
            Ok(section) => {
                if (arg.flags as i32) & lx::O_EXCL != 0 {
                    return Err(lx::Error::EEXIST);
                }
                let size = section.query_size()?;
                (size, InodeState::Open(section))
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => (0, InodeState::Pending(upath)),
            Err(err) => return Err(err.into()),
        };
        let inode = Inode {
            size,
            state,
            lookup_count: 1,
        };
        let mut attr = inode.attr(0, &self.attr);
        let node_id = self.inodes.lock().insert(inode);
        attr.ino = node_id;
        tracing::trace!(node_id, name = name.to_str().unwrap(), "node_id");
        Ok(fuse::CreateOut {
            entry: fuse::protocol::fuse_entry_out::new(
                node_id,
                Duration::from_secs(0),
                Duration::from_secs(60),
                attr,
            ),
            open: fuse::protocol::fuse_open_out::new(0, 0),
        })
    }

    fn destroy(&self) {
        self.inodes.lock().clear();
    }

    fn fallocate(
        &self,
        request: &fuse::Request,
        arg: &fuse::protocol::fuse_fallocate_in,
    ) -> lx::Result<()> {
        // TODO: establish a quota for guest-created sections to avoid commit exhaustion

        let mut inodes = self.inodes.lock();
        let inode = inodes.get_mut(request.node_id()).ok_or(lx::Error::EINVAL)?;
        if arg.offset != 0 || !arg.length.is_multiple_of(PAGE_SIZE) {
            return Err(lx::Error::EINVAL);
        }
        let size = arg.length;
        let section = match &inode.state {
            InodeState::Open(_) => return Err(lx::Error::EINVAL),
            InodeState::Pending(upath) => Section::new(
                ObjectAttributes::new()
                    .root(self.root.as_handle())
                    .name(upath),
                SECTION_QUERY | SECTION_MAP_READ | SECTION_MAP_WRITE,
                size,
            )?,
        };
        inode.state = InodeState::Open(section);
        inode.size = size;
        Ok(())
    }

    fn setup_mapping(
        &self,
        request: &fuse::Request,
        mapper: &dyn Mapper,
        arg: &fuse::protocol::fuse_setupmapping_in,
    ) -> lx::Result<()> {
        let mut inodes = self.inodes.lock();
        let inode = inodes.get_mut(request.node_id()).ok_or(lx::Error::EINVAL)?;
        let section = inode.section()?;
        if arg.foffset >= inode.size || arg.len == 0 {
            return Err(lx::Error::EINVAL);
        }
        let len = arg.len.min(inode.size - arg.foffset);
        let writable = arg.flags & FUSE_SETUPMAPPING_FLAG_WRITE != 0;
        tracing::trace!(
            inode = request.node_id(),
            offset = arg.foffset,
            len,
            moffset = arg.moffset,
            "Map",
        );
        mapper.map(
            arg.moffset,
            section.0.as_handle(),
            arg.foffset,
            len,
            writable,
        )
    }

    fn remove_mapping(
        &self,
        _request: &fuse::Request,
        mapper: &dyn Mapper,
        moffset: u64,
        len: u64,
    ) -> lx::Result<()> {
        tracing::trace!(moffset, len, "Unmap offset");
        mapper.unmap(moffset, len)
    }
}
