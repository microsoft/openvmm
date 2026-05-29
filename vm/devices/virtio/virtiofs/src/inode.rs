// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::file::VirtioFsFile;
use crate::util;
use fuse::protocol::*;
use lx::LxStr;
use lx::LxString;
use lxutil::LxCreateOptions;
use lxutil::LxVolume;
use lxutil::LxVolumeOptions;
use lxutil::PathBufExt;
use parking_lot::RwLock;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Process-wide counter used to assign virtio-fs volume IDs. The id is
/// paired with `lx::ino_t` in `InodeMap` to dedup inodes from distinct
/// volumes that happen to share a numeric inode number.
static NEXT_VOLUME_ID: AtomicU64 = AtomicU64::new(1);

/// An `Arc<LxVolume>` paired with a virtio-fs-local numeric identifier.
/// Cloned through child inode creation so all inodes rooted at the same
/// physical volume share the same `id`.
pub struct Volume {
    id: u64,
    /// Per-share key folded into reported inode numbers (see
    /// [`Volume::namespaced_ino`]). Zero for single-root mounts (identity
    /// transform); non-zero for children of an aggregate root.
    ino_ns: u64,
    volume: Arc<LxVolume>,
}

impl Volume {
    fn with_namespace(volume: Arc<LxVolume>, aggregate_child: bool) -> Arc<Self> {
        let id = NEXT_VOLUME_ID.fetch_add(1, Ordering::Relaxed);
        // Key the inode namespace off the (process-unique) volume id so each
        // aggregate child gets a distinct, stable key.
        let ino_ns = if aggregate_child { id } else { 0 };
        Arc::new(Self { id, ino_ns, volume })
    }

    /// Open an `LxVolume` rooted at `root_path` (honoring `options` when
    /// supplied) and wrap it in a [`Volume`]. Single home for the
    /// volume-construction pattern used by [`VirtioFs::new`](crate::VirtioFs)
    /// and (via [`Volume::open_aggregate_child`]) the aggregate paths.
    pub fn open(
        root_path: impl AsRef<Path>,
        options: Option<&LxVolumeOptions>,
    ) -> lx::Result<Arc<Self>> {
        Self::open_inner(root_path, options, false)
    }

    /// Like [`Volume::open`], but marks the resulting volume as a child of an
    /// aggregate (multi-path) root, so its reported inode numbers are
    /// namespaced (see [`Volume::namespaced_ino`]) to avoid cross-share
    /// `(st_dev, st_ino)` collisions under the single shared superblock.
    pub fn open_aggregate_child(
        root_path: impl AsRef<Path>,
        options: Option<&LxVolumeOptions>,
    ) -> lx::Result<Arc<Self>> {
        Self::open_inner(root_path, options, true)
    }

    fn open_inner(
        root_path: impl AsRef<Path>,
        options: Option<&LxVolumeOptions>,
        aggregate_child: bool,
    ) -> lx::Result<Arc<Self>> {
        let volume = if let Some(options) = options {
            options.new_volume(root_path)
        } else {
            LxVolume::new(root_path)
        }?;
        Ok(Self::with_namespace(Arc::new(volume), aggregate_child))
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn lx_volume(&self) -> &Arc<LxVolume> {
        &self.volume
    }

    /// Fold this volume's per-share key into a raw host inode number so the
    /// same underlying inode number reported from two different aggregate
    /// shares no longer collides under the single shared superblock.
    ///
    /// Identity (returns `raw` unchanged) for single-root mounts (`ino_ns ==
    /// 0`) and for volumes whose inode numbers aren't stable (FAT/exFAT
    /// recycle them — the same gate used by [`VirtioFsInode::dedup_key`]).
    ///
    /// XOR-by-constant is a bijection, so distinct files *within* a share
    /// keep distinct inode numbers (preserving hard-link identity); the
    /// distinct per-share key removes the systematic cross-share collision
    /// where two volumes report the same small inode number.
    pub fn namespaced_ino(&self, raw: lx::ino_t) -> lx::ino_t {
        if self.ino_ns == 0 || !self.volume.supports_stable_file_id() {
            return raw;
        }
        raw ^ self.ino_ns.wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }
}

/// One child of an aggregate (multi-path) virtio-fs root.
pub struct SyntheticChild {
    pub name: LxString,
    pub volume: Arc<Volume>,
}

/// Validates a name is acceptable as a child of the synthetic root: non-
/// empty, no `/` or NUL, and not `.` or `..`.
pub(crate) fn validate_child_name_bytes(name: &[u8]) -> lx::Result<()> {
    if name.is_empty() || name == b"." || name == b".." {
        return Err(lx::Error::EINVAL);
    }
    if name.iter().any(|b| *b == b'/' || *b == 0) {
        return Err(lx::Error::EINVAL);
    }
    Ok(())
}

/// Shared state for an aggregate virtio-fs device. Held by the
/// `SyntheticRoot` inode (for lookup/enumeration), by the optional
/// [`crate::VirtiofsAggregateHandle`] (for live child appends), and by
/// the owning [`crate::VirtioFs`] (to flip `tearing_down` on drop).
pub(crate) struct AggregateState {
    /// Synthetic children. Append-only — existing entries are never
    /// removed or reordered, which keeps offset-based READDIR cookies
    /// stable across concurrent appends.
    children: RwLock<Vec<SyntheticChild>>,
    readonly: bool,
    /// Set to `true` by `VirtioFs::Drop` (via `mark_tearing_down`) to
    /// reject subsequent `add_child` calls with `EAGAIN`.
    tearing_down: AtomicBool,
}

impl AggregateState {
    pub fn new(children: Vec<SyntheticChild>, readonly: bool) -> Arc<Self> {
        Arc::new(Self {
            children: RwLock::new(children),
            readonly,
            tearing_down: AtomicBool::new(false),
        })
    }

    pub fn is_active(&self) -> bool {
        !self.tearing_down.load(Ordering::Acquire)
    }

    /// The aggregate's readonly setting. Fixed at construction; every child
    /// must match it.
    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn mark_tearing_down(&self) {
        self.tearing_down.store(true, Ordering::Release);
    }

    /// Current child count; used to synthesize the root directory's `nlink`.
    pub fn child_count(&self) -> usize {
        self.children.read().len()
    }

    /// Looks up a child by name. Returns a cloned `Arc<Volume>` so the
    /// caller can release the lock before doing slow I/O.
    pub fn find_child(&self, name: &LxStr) -> Option<Arc<Volume>> {
        self.children
            .read()
            .iter()
            .find(|c| *c.name == *name)
            .map(|c| Arc::clone(&c.volume))
    }

    /// Snapshots child names for READDIR; the lock is held only for the
    /// clone. Children appended after the snapshot are invisible to this
    /// call but appear on the next READDIR (append-only ordering keeps
    /// cookies stable).
    pub fn snapshot_names(&self) -> Vec<LxString> {
        self.children
            .read()
            .iter()
            .map(|c| c.name.clone())
            .collect()
    }

    /// Appends a new child. Returns `EAGAIN` if tearing down, `EINVAL` on
    /// readonly mismatch, or `EEXIST` if the name is already in use.
    pub fn add_child(
        &self,
        name: LxString,
        volume: Arc<Volume>,
        child_readonly: bool,
    ) -> lx::Result<()> {
        if !self.is_active() {
            return Err(lx::Error::EAGAIN);
        }
        if self.readonly != child_readonly {
            return Err(lx::Error::EINVAL);
        }
        let mut guard = self.children.write();
        // Re-check lifecycle under the lock; teardown could race the early check.
        if !self.is_active() {
            return Err(lx::Error::EAGAIN);
        }
        if guard.iter().any(|c| c.name == name) {
            return Err(lx::Error::EEXIST);
        }
        guard.push(SyntheticChild { name, volume });
        Ok(())
    }
}

/// Key used to deduplicate inodes in the inode map so that repeated lookups
/// of the same underlying file return one stable FUSE node id (which the
/// guest needs for features such as inotify).
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum DedupKey {
    /// `(volume_id, host_inode_number)`, for volumes that report stable
    /// inode numbers (e.g. NTFS). The inode number is a reliable identity
    /// and is immutable for an inode's lifetime.
    Ino(u64, lx::ino_t),
    /// `(volume_id, host_path)`, for volumes that recycle inode numbers
    /// (FAT/exFAT) where the inode number is not a safe identity. The path
    /// is the stable identity instead.
    Path(u64, PathBuf),
}

/// An inode backed by a real host path on a single [`Volume`]. This is the
/// ordinary virtio-fs inode; all of its operations delegate straight to the
/// backing `LxVolume`.
struct RealInode {
    volume: Arc<Volume>,
    path: RwLock<PathBuf>,
    inode_nr: lx::ino_t,
    /// `true` if this inode is the root of an auto-mounted child of an
    /// aggregate virtio-fs root. Such inodes return `FUSE_ATTR_SUBMOUNT`
    /// in their attrs so the guest kernel mounts a new superblock for
    /// them, isolating their inode-number namespace from siblings.
    is_submount: bool,
}

/// The synthetic read-only root directory of an aggregate virtio-fs.
///
/// `SyntheticRoot` lives at FUSE_ROOT_ID. Its only meaningful operation is
/// `lookup_child(name)` which routes to one of the children; readdir
/// enumerates them. All mutating ops return EROFS. The shared
/// `AggregateState` is also held by the device's
/// [`crate::VirtiofsAggregateHandle`] (if any) so that children can be
/// appended after construction.
struct SyntheticRoot {
    state: Arc<AggregateState>,
}

/// The kind of file system inode being represented.
enum InodeKind {
    /// An inode backed by a real host path on a single [`Volume`].
    Real(RealInode),
    /// The synthetic read-only root of an aggregate virtio-fs.
    SyntheticRoot(SyntheticRoot),
}

/// Implements inode callbacks for virtio-fs.
pub struct VirtioFsInode {
    kind: InodeKind,
    lookup_count: AtomicU64,
}

impl VirtioFsInode {
    /// Create a new `Real` inode for the specified path, fetching its
    /// attributes via `LxVolume::lstat`.
    pub fn new(volume: Arc<Volume>, path: PathBuf) -> lx::Result<(Self, lx::Stat)> {
        let stat = volume.lx_volume().lstat(&path)?;
        let inode = Self::with_attr(volume, path, &stat);
        Ok((inode, stat))
    }

    /// Create a new `Real` inode for the specified path, using previously
    /// retrieved attributes.
    pub fn with_attr(volume: Arc<Volume>, path: PathBuf, stat: &lx::Stat) -> Self {
        Self::new_real(volume, path, stat.inode_nr, false)
    }

    /// Create a new `Real` inode flagged as the root of an auto-mount
    /// submount (used when the guest looks up a child of the synthetic
    /// aggregate root).
    pub fn new_submount_root(volume: Arc<Volume>, inode_nr: lx::ino_t) -> Self {
        Self::new_real(volume, PathBuf::new(), inode_nr, true)
    }

    fn new_real(
        volume: Arc<Volume>,
        path: PathBuf,
        inode_nr: lx::ino_t,
        is_submount: bool,
    ) -> Self {
        Self {
            kind: InodeKind::Real(RealInode {
                volume,
                path: RwLock::new(path),
                inode_nr,
                is_submount,
            }),
            lookup_count: AtomicU64::new(1),
        }
    }

    /// Create the synthetic root inode for an aggregate virtio-fs.
    pub fn new_synthetic_root(state: Arc<AggregateState>) -> Self {
        Self {
            kind: InodeKind::SyntheticRoot(SyntheticRoot { state }),
            lookup_count: AtomicU64::new(1),
        }
    }

    /// Returns the backing [`RealInode`], or `None` for the synthetic root.
    fn real(&self) -> Option<&RealInode> {
        match &self.kind {
            InodeKind::Real(real) => Some(real),
            InodeKind::SyntheticRoot(_) => None,
        }
    }

    /// Returns the backing [`RealInode`], or `EROFS` for the synthetic root.
    /// Used by mutating operations to consolidate the "not on synthetic
    /// root" check.
    fn real_for_mutate(&self) -> lx::Result<&RealInode> {
        self.real().ok_or(lx::Error::EROFS)
    }

    /// Returns true if this inode is the synthetic root of an aggregate
    /// virtio-fs.
    pub fn is_synthetic_root(&self) -> bool {
        matches!(self.kind, InodeKind::SyntheticRoot(_))
    }

    /// Returns the inode number as reported by the underlying host file
    /// system, or `0` for the synthetic root (which has no host inode).
    ///
    /// N.B. This may be different from the inode's FUSE node ID.
    pub fn inode_nr(&self) -> lx::ino_t {
        match &self.kind {
            InodeKind::Real(real) => real.inode_nr,
            InodeKind::SyntheticRoot(_) => 0,
        }
    }

    /// Applies this inode's volume namespace to a raw host inode number (see
    /// [`Volume::namespaced_ino`]). Identity for the synthetic root, which
    /// has no backing volume.
    pub fn namespaced_ino(&self, raw: lx::ino_t) -> lx::ino_t {
        match &self.kind {
            InodeKind::Real(real) => real.volume.namespaced_ino(raw),
            InodeKind::SyntheticRoot(_) => raw,
        }
    }

    /// Returns the dedup key for this inode, or `None` if the inode should
    /// not participate in deduplication.
    ///
    /// `SyntheticRoot` never dedupes. `Real` inodes on a volume that reports
    /// stable inode numbers dedupe by [`DedupKey::Ino`]. On volumes that
    /// recycle inode numbers (FAT/exFAT), the inode number is not a safe
    /// identity, so non-empty paths dedupe by [`DedupKey::Path`] instead —
    /// keeping a single FUSE node id stable across repeated lookups of the
    /// same path. A path-keyed inode with an empty path (a submount or
    /// volume root) returns `None`.
    pub fn dedup_key(&self) -> Option<DedupKey> {
        match &self.kind {
            InodeKind::Real(real) => real.dedup_key(),
            InodeKind::SyntheticRoot(_) => None,
        }
    }

    /// Computes the [`DedupKey::Path`] that a child named `name` of this
    /// inode would use, for path-keyed (non-stable-id) volumes only.
    ///
    /// Returns `None` for stable-id volumes — those dedupe by inode number,
    /// which a delete/recreate naturally changes, so there is no stale path
    /// entry to evict — and for the synthetic root. Used to evict the dedup
    /// entry when a path is removed (unlink/rmdir) or vacated (rename), so a
    /// later create at the same path cannot alias a stale node id.
    pub fn child_path_dedup_key(&self, name: &LxStr) -> Option<DedupKey> {
        let real = self.real()?;
        if real.volume.lx_volume().supports_stable_file_id() {
            return None;
        }
        let path = child_path(&real.path.read(), name).ok()?;
        Some(DedupKey::Path(real.volume.id(), path))
    }

    /// Increments the lookup count and replaces the recorded path.
    pub fn lookup(&self, new_path: PathBuf) {
        self.lookup_count.fetch_add(1, Ordering::AcqRel);
        if let InodeKind::Real(real) = &self.kind {
            *real.path.write() = new_path;
        }
    }

    /// Increments the lookup count without updating the path.
    ///
    /// Used when returning an existing inode in a FUSE reply (e.g. for hard
    /// links) where the kernel will track the reference and later send a
    /// forget.
    pub fn inc_lookup(&self) {
        self.lookup_count.fetch_add(1, Ordering::AcqRel);
    }

    /// Decrements the lookup count, and returns the new count.
    pub fn forget(&self, node_id: u64, lookup_count: u64) -> u64 {
        let mut old_count = self.lookup_count.load(Ordering::Acquire);
        loop {
            let new_count = if lookup_count > old_count {
                tracing::warn!(node_id, "Too many forgets for inode");
                0
            } else {
                old_count - lookup_count
            };

            match self.lookup_count.compare_exchange_weak(
                old_count,
                new_count,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break new_count,
                Err(value) => old_count = value,
            }
        }
    }

    /// Performs a lookup for a child of this inode.
    pub fn lookup_child(&self, name: &LxStr) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        match &self.kind {
            InodeKind::Real(real) => real.lookup_child(name),
            InodeKind::SyntheticRoot(root) => root.lookup_child(name),
        }
    }

    /// Retrieves the attributes of this inode.
    pub fn get_attr(&self) -> lx::Result<fuse_attr> {
        match &self.kind {
            InodeKind::Real(real) => real.get_attr(),
            InodeKind::SyntheticRoot(root) => Ok(root.get_attr()),
        }
    }

    /// Retrieves the extended attributes of this inode.
    pub fn get_statx(&self) -> lx::Result<fuse_statx> {
        match &self.kind {
            InodeKind::Real(real) => real.get_statx(),
            InodeKind::SyntheticRoot(root) => Ok(root.get_statx()),
        }
    }

    /// Sets the attributes of this inode.
    pub fn set_attr(&self, arg: &fuse_setattr_in, request_uid: lx::uid_t) -> lx::Result<fuse_attr> {
        self.real_for_mutate()?.set_attr(arg, request_uid)
    }

    /// Opens the inode, creating a file object.
    pub fn open(self: Arc<VirtioFsInode>, flags: u32) -> lx::Result<VirtioFsFile> {
        match &self.kind {
            InodeKind::Real(real) => {
                let flags = (flags as i32) | lx::O_NOFOLLOW;
                let file = real
                    .volume
                    .lx_volume()
                    .open(&*real.path.read(), flags, None)?;
                Ok(VirtioFsFile::new_real(file, self))
            }
            InodeKind::SyntheticRoot(_) => {
                // The synthetic root is a directory; the kernel uses
                // OPENDIR, which the FUSE layer routes through `open`.
                // Hand back a synthetic directory file object that knows
                // how to enumerate the aggregate children.
                Ok(VirtioFsFile::new_synthetic_root_dir(self))
            }
        }
    }

    /// Creates a new file as a child of this inode, and opens it.
    pub fn create(
        &self,
        name: &LxStr,
        flags: u32,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> lx::Result<(VirtioFsInode, fuse_attr, lxutil::LxFile)> {
        self.real_for_mutate()?.create(name, flags, mode, uid, gid)
    }

    /// Creates a new directory as a child of this inode.
    pub fn mkdir(
        &self,
        name: &LxStr,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        self.real_for_mutate()?.mkdir(name, mode, uid, gid)
    }

    /// Creates a new regular, device, socket, or fifo file as a child of this inode.
    pub fn mknod(
        &self,
        name: &LxStr,
        mode: u32,
        uid: u32,
        gid: u32,
        device_id: u32,
    ) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        self.real_for_mutate()?
            .mknod(name, mode, uid, gid, device_id)
    }

    /// Creates a new symlink as a child of this inode.
    pub fn symlink(
        &self,
        name: &LxStr,
        target: &LxStr,
        uid: u32,
        gid: u32,
    ) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        self.real_for_mutate()?.symlink(name, target, uid, gid)
    }

    /// Creates a new hard link as a child of this inode.
    pub fn link(&self, name: &LxStr, target: &VirtioFsInode) -> lx::Result<fuse_attr> {
        let target = target.real_for_mutate()?;
        self.real_for_mutate()?.link(name, target)
    }

    /// Reads the target of the symbolic link, if this inode is a symbolic link.
    pub fn read_link(&self) -> lx::Result<LxString> {
        match &self.kind {
            InodeKind::Real(real) => real.read_link(),
            // Synthetic root is a directory, not a symlink.
            InodeKind::SyntheticRoot(_) => Err(lx::Error::EINVAL),
        }
    }

    /// Removes a file or directory child of this inode.
    pub fn unlink(&self, name: &LxStr, flags: i32) -> lx::Result<()> {
        self.real_for_mutate()?.unlink(name, flags)
    }

    /// Renames a child of this inode.
    pub fn rename(
        &self,
        name: &LxStr,
        new_dir: &VirtioFsInode,
        new_name: &LxStr,
        flags: u32,
    ) -> lx::Result<()> {
        let new_dir = new_dir.real_for_mutate()?;
        self.real_for_mutate()?
            .rename(name, new_dir, new_name, flags)
    }

    /// Gets the attributes of the file system that the inode resides on.
    pub fn stat_fs(&self) -> lx::Result<fuse_kstatfs> {
        match &self.kind {
            InodeKind::Real(real) => real.stat_fs(),
            InodeKind::SyntheticRoot(_) => Ok(synthetic_root_statfs()),
        }
    }

    /// Gets the value or the size of an extended attribute on this inode.
    pub fn get_xattr(&self, name: &LxStr, value: Option<&mut [u8]>) -> lx::Result<usize> {
        match &self.kind {
            InodeKind::Real(real) => real.get_xattr(name, value),
            // Synthetic root has no xattrs.
            InodeKind::SyntheticRoot(_) => Err(lx::Error::ENODATA),
        }
    }

    /// Sets an extended attribute on this inode.
    pub fn set_xattr(&self, name: &LxStr, value: &[u8], flags: u32) -> lx::Result<()> {
        self.real_for_mutate()?.set_xattr(name, value, flags)
    }

    /// Lists the extended attributes on this inode.
    pub fn list_xattr(&self, list: Option<&mut [u8]>) -> lx::Result<usize> {
        match &self.kind {
            InodeKind::Real(real) => real.list_xattr(list),
            // Synthetic root has no xattrs: report zero bytes.
            InodeKind::SyntheticRoot(_) => Ok(0),
        }
    }

    /// Removes an extended attribute from this inode.
    pub fn remove_xattr(&self, name: &LxStr) -> lx::Result<()> {
        self.real_for_mutate()?.remove_xattr(name)
    }

    /// Gets a clone of the stored path. Returns an empty `PathBuf` for the
    /// synthetic root (which has no host path).
    pub fn clone_path(&self) -> PathBuf {
        match &self.kind {
            InodeKind::Real(real) => real.path.read().clone(),
            InodeKind::SyntheticRoot(_) => PathBuf::new(),
        }
    }

    /// Provides read-only access to the aggregate state of a synthetic
    /// root, for use by readdir on the synthetic root directory.
    pub fn aggregate_state(&self) -> Option<&Arc<AggregateState>> {
        match &self.kind {
            InodeKind::SyntheticRoot(root) => Some(&root.state),
            InodeKind::Real(_) => None,
        }
    }
}

impl RealInode {
    /// See [`VirtioFsInode::dedup_key`].
    fn dedup_key(&self) -> Option<DedupKey> {
        if self.volume.lx_volume().supports_stable_file_id() {
            return Some(DedupKey::Ino(self.volume.id(), self.inode_nr));
        }
        let path = self.path.read();
        if path.as_os_str().is_empty() {
            None
        } else {
            Some(DedupKey::Path(self.volume.id(), path.clone()))
        }
    }

    /// Builds a `fuse_attr` from a host stat, applying this volume's inode
    /// namespace and the supplied `FUSE_ATTR_*` flags.
    fn attr(&self, stat: &lx::Stat, attr_flags: u32) -> fuse_attr {
        let mut attr = util::stat_to_fuse_attr_with_flags(stat, attr_flags);
        attr.ino = self.volume.namespaced_ino(attr.ino);
        attr
    }

    /// Builds a `fuse_statx` from a host statx, applying this volume's inode
    /// namespace.
    fn statx_attr(&self, statx: &lx::StatEx) -> fuse_statx {
        let mut sx = util::statx_to_fuse_statx(statx);
        sx.ino = self.volume.namespaced_ino(sx.ino);
        sx
    }

    fn lookup_child(&self, name: &LxStr) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        let path = child_path(&self.path.read(), name)?;
        let stat = self.volume.lx_volume().lstat(&path)?;
        // Descendants of a submount root are *not* themselves submounts —
        // the kernel has already mounted a fresh superblock at the submount
        // root, so children are just ordinary inodes within it.
        let inode = VirtioFsInode::new_real(Arc::clone(&self.volume), path, stat.inode_nr, false);
        let attr = self.attr(&stat, 0);
        Ok((inode, attr))
    }

    fn get_attr(&self) -> lx::Result<fuse_attr> {
        let stat = self.volume.lx_volume().lstat(&*self.path.read())?;
        Ok(self.attr(&stat, submount_flag(self.is_submount)))
    }

    fn get_statx(&self) -> lx::Result<fuse_statx> {
        // `fuse_statx` has no flags field for `FUSE_ATTR_SUBMOUNT`, so the
        // submount marker is intentionally omitted from statx replies. The
        // kernel only consumes `FUSE_ATTR_SUBMOUNT` from `fuse_attr`
        // (LOOKUP, GETATTR, READDIRPLUS) anyway.
        let statx = self.volume.lx_volume().statx(&*self.path.read())?;
        Ok(self.statx_attr(&statx))
    }

    fn set_attr(&self, arg: &fuse_setattr_in, request_uid: lx::uid_t) -> lx::Result<fuse_attr> {
        let attr = util::fuse_set_attr_to_lxutil(arg, request_uid);
        // Because FUSE_HANDLE_KILLPRIV is set, set-user-ID and set-group-ID must be cleared
        // depending on the attributes being set. Lxutil takes care of that on Windows (and Linux
        // does it naturally).
        let stat = self
            .volume
            .lx_volume()
            .set_attr_stat(&*self.path.read(), attr)?;
        Ok(self.attr(&stat, submount_flag(self.is_submount)))
    }

    fn create(
        &self,
        name: &LxStr,
        flags: u32,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> lx::Result<(VirtioFsInode, fuse_attr, lxutil::LxFile)> {
        let path = child_path(&self.path.read(), name)?;
        let options = LxCreateOptions::new(mode, uid, gid);
        let flags = (flags as i32) | lx::O_CREAT | lx::O_NOFOLLOW;
        let file = self.volume.lx_volume().open(&path, flags, Some(options))?;
        let stat = file.fstat()?.into();
        let inode = VirtioFsInode::with_attr(Arc::clone(&self.volume), path, &stat);
        let attr = self.attr(&stat, 0);
        Ok((inode, attr, file))
    }

    fn mkdir(
        &self,
        name: &LxStr,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        let path = child_path(&self.path.read(), name)?;
        let stat = self
            .volume
            .lx_volume()
            .mkdir_stat(&path, LxCreateOptions::new(mode, uid, gid))?;
        let inode = VirtioFsInode::with_attr(Arc::clone(&self.volume), path, &stat);
        let attr = self.attr(&stat, 0);
        Ok((inode, attr))
    }

    fn mknod(
        &self,
        name: &LxStr,
        mode: u32,
        uid: u32,
        gid: u32,
        device_id: u32,
    ) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        let path = child_path(&self.path.read(), name)?;
        let stat = self.volume.lx_volume().mknod_stat(
            &path,
            LxCreateOptions::new(mode, uid, gid),
            device_id as usize,
        )?;
        let inode = VirtioFsInode::with_attr(Arc::clone(&self.volume), path, &stat);
        let attr = self.attr(&stat, 0);
        Ok((inode, attr))
    }

    fn symlink(
        &self,
        name: &LxStr,
        target: &LxStr,
        uid: u32,
        gid: u32,
    ) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        let path = child_path(&self.path.read(), name)?;
        let stat = self.volume.lx_volume().symlink_stat(
            &path,
            target,
            LxCreateOptions::new(lx::S_IFLNK | 0o777, uid, gid),
        )?;
        let inode = VirtioFsInode::with_attr(Arc::clone(&self.volume), path, &stat);
        let attr = self.attr(&stat, 0);
        Ok((inode, attr))
    }

    fn link(&self, name: &LxStr, target: &RealInode) -> lx::Result<fuse_attr> {
        // Hard links cannot cross filesystems.
        if self.volume.id() != target.volume.id() {
            return Err(lx::Error::EXDEV);
        }
        let path = child_path(&self.path.read(), name)?;
        let stat = self
            .volume
            .lx_volume()
            .link_stat(&*target.path.read(), path)?;
        Ok(self.attr(&stat, 0))
    }

    fn read_link(&self) -> lx::Result<LxString> {
        self.volume.lx_volume().read_link(&*self.path.read())
    }

    fn unlink(&self, name: &LxStr, flags: i32) -> lx::Result<()> {
        let path = child_path(&self.path.read(), name)?;
        self.volume.lx_volume().unlink(path, flags)
    }

    fn rename(
        &self,
        name: &LxStr,
        new_dir: &RealInode,
        new_name: &LxStr,
        flags: u32,
    ) -> lx::Result<()> {
        // Renames cannot cross filesystems.
        if self.volume.id() != new_dir.volume.id() {
            return Err(lx::Error::EXDEV);
        }
        let path = child_path(&self.path.read(), name)?;
        let new_path = child_path(&new_dir.path.read(), new_name)?;
        self.volume.lx_volume().rename(path, new_path, flags)
    }

    fn stat_fs(&self) -> lx::Result<fuse_kstatfs> {
        let stat_fs = self.volume.lx_volume().stat_fs(&*self.path.read())?;
        Ok(fuse_kstatfs::new(
            stat_fs.block_count,
            stat_fs.free_block_count,
            stat_fs.available_block_count,
            stat_fs.file_count,
            stat_fs.available_file_count,
            stat_fs.block_size as u32,
            stat_fs.maximum_file_name_length as u32,
            stat_fs.file_record_size as u32,
        ))
    }

    fn get_xattr(&self, name: &LxStr, value: Option<&mut [u8]>) -> lx::Result<usize> {
        self.volume
            .lx_volume()
            .get_xattr(&*self.path.read(), name, value)
    }

    fn set_xattr(&self, name: &LxStr, value: &[u8], flags: u32) -> lx::Result<()> {
        self.volume
            .lx_volume()
            .set_xattr(&*self.path.read(), name, value, flags as i32)
    }

    fn list_xattr(&self, list: Option<&mut [u8]>) -> lx::Result<usize> {
        self.volume.lx_volume().list_xattr(&*self.path.read(), list)
    }

    fn remove_xattr(&self, name: &LxStr) -> lx::Result<()> {
        self.volume
            .lx_volume()
            .remove_xattr(&*self.path.read(), name)
    }
}

impl SyntheticRoot {
    /// Looks up a child of the synthetic root, returning a submount-root
    /// inode flagged with `FUSE_ATTR_SUBMOUNT`.
    fn lookup_child(&self, name: &LxStr) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        // Snapshot the matching child's volume, then release the lock
        // before the slow stat.
        let volume = self.state.find_child(name).ok_or(lx::Error::ENOENT)?;
        let stat = volume.lx_volume().lstat(PathBuf::new())?;
        let mut attr = util::stat_to_fuse_attr_with_flags(&stat, FUSE_ATTR_SUBMOUNT);
        attr.ino = volume.namespaced_ino(attr.ino);
        let inode = VirtioFsInode::new_submount_root(volume, stat.inode_nr);
        Ok((inode, attr))
    }

    fn get_attr(&self) -> fuse_attr {
        synthetic_root_attr(self.state.child_count())
    }

    fn get_statx(&self) -> fuse_statx {
        synthetic_root_statx(self.state.child_count())
    }
}

fn submount_flag(is_submount: bool) -> u32 {
    if is_submount { FUSE_ATTR_SUBMOUNT } else { 0 }
}

/// Build a child path by appending `name` to `parent`. Defense in depth:
/// the FUSE request parser already rejects bad names, but validate here too
/// so a bypass becomes `EINVAL` rather than a path-traversal hazard.
fn child_path(parent: &Path, name: &LxStr) -> lx::Result<PathBuf> {
    validate_child_name_bytes(name.as_bytes())?;
    let mut path = parent.to_path_buf();
    path.push_lx(name)?;
    Ok(path)
}

/// Synthesizes a `fuse_attr` for the synthetic aggregate root directory.
fn synthetic_root_attr(child_count: usize) -> fuse_attr {
    // The mode MUST include the directory file-type bits or Linux will
    // reject the attr as malformed (S_IFMT is zero -> "unknown type").
    let mode = lx::S_IFDIR | 0o555;
    // For a directory, nlink conventionally counts ".", "..", and each
    // subdirectory's "..". With N children that's `2 + N`.
    let nlink: u32 = 2u32.saturating_add(child_count as u32);
    fuse_attr {
        ino: FUSE_ROOT_ID,
        size: 0,
        blocks: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        atimensec: 0,
        mtimensec: 0,
        ctimensec: 0,
        mode,
        nlink,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// Synthesizes a `fuse_statx` for the synthetic aggregate root directory.
fn synthetic_root_statx(child_count: usize) -> fuse_statx {
    let mode = (lx::S_IFDIR | 0o555) as u16;
    let nlink: u32 = 2u32.saturating_add(child_count as u32);
    let zero_ts = || fuse_sx_time {
        sec: 0,
        nsec: 0,
        _rsvd: 0,
    };
    let mask = lx::StatExMask::new()
        .with_file_type(true)
        .with_mode(true)
        .with_nlink(true)
        .with_uid(true)
        .with_gid(true)
        .with_ino(true)
        .with_size(true)
        .with_blocks(true);
    fuse_statx {
        mask: mask.into_bits(),
        blksize: 4096,
        attributes: 0,
        nlink,
        uid: 0,
        gid: 0,
        mode,
        ino: FUSE_ROOT_ID,
        size: 0,
        blocks: 0,
        attributes_mask: 0,
        atime: zero_ts(),
        btime: zero_ts(),
        mtime: zero_ts(),
        ctime: zero_ts(),
        rdev_major: 0,
        rdev_minor: 0,
        dev_major: 0,
        dev_minor: 0,
        _rsvd1: 0,
        _rsvd2: [0; 14],
    }
}

/// Synthesizes a `fuse_kstatfs` for the synthetic aggregate root. The
/// reported sizes are sane defaults; capacity numbers are zero (this is a
/// "filesystem" with no real backing storage).
fn synthetic_root_statfs() -> fuse_kstatfs {
    // Note: `fuse_kstatfs::new` ordering -- (blocks, bfree, bavail, files,
    // ffree, bsize, namelen, frsize).
    fuse_kstatfs::new(0, 0, 0, 0, 0, 4096, 255, 4096)
}
