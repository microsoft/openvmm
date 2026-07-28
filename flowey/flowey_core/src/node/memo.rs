// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A shared, key-addressed memoization store for expensive flowey steps.
//!
//! # Motivation
//!
//! flowey's DAG only prunes work based on _reachability_ - it has no notion of
//! _freshness_. Every step's closure runs unconditionally, every run. That's
//! fine for cheap steps, but flowey also performs plenty of expensive
//! post-processing (splitting debug info out of a multi-gigabyte binary,
//! extracting large archives, downloading release assets, ...) which produces
//! byte-identical output run after run.
//!
//! This module provides the machinery to avoid that redundant work: a step
//! declares a [`MemoKey`] describing everything its output depends on, and the
//! store either hands back the outputs from a previous run, or runs the step
//! and records its outputs for next time.
//!
//! # Using it
//!
//! Most node authors should reach for [`NodeCtx::emit_memoized_rust_step`],
//! which wraps a regular rust step with store lookup / insertion.
//!
//! [`NodeCtx::emit_memoized_rust_step`]: super::NodeCtx::emit_memoized_rust_step
//!
//! Code that needs to memoize something from _within_ an existing step (e.g:
//! shared helper functions that aren't in a position to emit their own step)
//! can use [`MemoStore::get_or_insert_with`] directly.
//!
//! # Keys
//!
//! A [`MemoKey`] is an ordered list of named key material. Anything that can
//! change the step's output must be in the key, including:
//!
//! - a `recipe` name and `version` identifying the transform itself. **Bump the
//!   version whenever the step's logic changes**, otherwise stale entries will
//!   be silently reused.
//! - the identity of any external tool being invoked, via
//!   [`MemoKey::with_tool`]
//! - every input file, via [`MemoKey::with_path`]
//! - every input value / flag, via [`MemoKey::with_value`]
//!
//! Input files are keyed by a cheap `(len, mtime)` _stamp_ rather than a
//! content hash, the same trick cargo and ninja use. This means an input that
//! gets rebuilt to a byte-identical result will still miss the cache - that's
//! an acceptable trade for not having to read gigabytes of input on every run.
//!
//! ## Keys and flowey's two phases
//!
//! Key material that comes from the filesystem must be sampled at _runtime_,
//! not at flow-build time. Sampling early is wrong twice over: the key would be
//! computed before earlier steps have had a chance to produce the inputs or
//! install the tools, and on CI backends the pipeline is generated on a
//! different machine than the one that runs it.
//!
//! [`MemoKey::with_path`] and [`MemoKey::with_tool`] therefore take a
//! [`RustRuntimeServices`] which they don't otherwise use, purely so that
//! calling them outside a step body doesn't compile. Node authors assembling a
//! key declaratively should build a [`MemoKeySpec`] instead, which records what
//! to sample and defers the sampling until the step runs.
//!
//! # Immutability
//!
//! Outputs are handed out as paths _inside_ the store. Consumers must treat
//! them as read-only.
//!
//! Every entry records a stamp of its _whole directory_ - not merely the paths
//! the step declared - and is evicted on lookup if that stamp no longer
//! matches. Mutating an entry is therefore self-healing rather than permanently
//! corrupting, whether the entry came from
//! [`NodeCtx::emit_memoized_rust_step`] or from [`MemoStore::get_or_insert_with`].
//!
//! # Availability
//!
//! The store lives inside flowey's persistent dir, which is typically
//! unavailable on CI (agents get wiped between jobs). When there's no store,
//! memoized steps degrade into plain steps that always do their work.
//!
//! Memoization can be disabled entirely by setting `FLOWEY_NO_MEMO=1`.

use super::ClaimVar;
use super::ClaimedReadVar;
use super::ReadVar;
use super::StepCtx;
use super::steps::rust::RustRuntimeServices;
use anyhow::Context as _;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Name of the memo store dir, relative to flowey's persistent dir root.
pub const MEMO_STORE_DIR_NAME: &str = ".memo";

const ENTRIES_DIR: &str = "entries";
const TMP_DIR: &str = "tmp";
const META_FILE: &str = "memo.json";
const USED_FILE: &str = "used";
const OUT_DIR: &str = "out";

/// Default cap on the total size of the memo store, in bytes.
pub const DEFAULT_MAX_STORE_SIZE: u64 = 20 * 1024 * 1024 * 1024;

/// Tmp dirs older than this are assumed to be leftovers from a crashed (or
/// memoization-disabled) run, and are cleaned up by [`MemoStore::gc`].
const TMP_MAX_AGE_SECS: u64 = 24 * 60 * 60;

/// Entries touched more recently than this are never evicted, since a
/// concurrently-running flowey may be holding paths into them.
const EVICTION_GRACE_SECS: u64 = 60 * 60;

/// Returns whether memoization has been globally disabled via
/// `FLOWEY_NO_MEMO`.
pub fn memoization_disabled() -> bool {
    matches!(
        std::env::var("FLOWEY_NO_MEMO").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Returns the configured max store size, honoring `FLOWEY_MEMO_MAX_SIZE`.
pub fn configured_max_store_size() -> anyhow::Result<u64> {
    match std::env::var("FLOWEY_MEMO_MAX_SIZE") {
        Ok(s) => parse_size_bytes(&s).context("while parsing FLOWEY_MEMO_MAX_SIZE"),
        Err(_) => Ok(DEFAULT_MAX_STORE_SIZE),
    }
}

/// Parse a human-readable byte size, e.g: `512`, `20GiB`, `1.5gb`.
pub fn parse_size_bytes(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);

    let num: f64 = num
        .parse()
        .with_context(|| format!("invalid size {s:?}: expected a leading number"))?;

    let mult: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024 * 1024 * 1024 * 1024,
        other => anyhow::bail!("invalid size {s:?}: unknown unit {other:?}"),
    };

    Ok((num * mult as f64) as u64)
}

/// Render a byte count in a human-readable form.
pub fn format_size_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// A cheap `(len, mtime)` fingerprint of a single file.
///
/// This intentionally does _not_ hash file contents. See the module docs for
/// why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    /// Length of the file, in bytes.
    pub len: u64,
    /// Whole seconds of the file's mtime, relative to the unix epoch.
    pub mtime_secs: u64,
    /// Sub-second nanoseconds of the file's mtime.
    pub mtime_nanos: u32,
}

impl FileStamp {
    /// Stamp a single file (or symlink - the link itself is stamped, not its
    /// target).
    pub fn new(path: &Path) -> anyhow::Result<Self> {
        Self::from_meta(fs_err::symlink_metadata(path)?)
    }

    /// Stamp a single file, following symlinks.
    pub fn new_following(path: &Path) -> anyhow::Result<Self> {
        Self::from_meta(fs_err::metadata(path)?)
    }

    fn from_meta(meta: std::fs::Metadata) -> anyhow::Result<Self> {
        let (mtime_secs, mtime_nanos) = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap_or((0, 0));
        Ok(Self {
            len: meta.len(),
            mtime_secs,
            mtime_nanos,
        })
    }
}

impl std::fmt::Display for FileStamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            len,
            mtime_secs,
            mtime_nanos,
        } = self;
        write!(f, "len={len},mtime={mtime_secs}.{mtime_nanos:09}")
    }
}

/// A resolved memoization key: an ordered list of named key material.
///
/// Build one with [`MemoKey::new`] plus the various `with_*` methods. Note
/// that key material is order-sensitive, so the same logical key must always
/// be built in the same order (which happens naturally, since the order is
/// determined by the code that builds it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoKey {
    parts: Vec<(String, String)>,
}

impl MemoKey {
    /// Start a new key for the given `recipe` (a stable, human-readable name
    /// for the transform - conventionally the node's modpath) at the given
    /// `version`.
    ///
    /// **Bump `version` whenever the transform's logic changes.** Failing to do
    /// so means previously-stored outputs, produced by the _old_ logic, will be
    /// silently reused.
    pub fn new(recipe: impl AsRef<str>, version: u32) -> Self {
        Self {
            parts: vec![("recipe".into(), format!("{}@v{version}", recipe.as_ref()))],
        }
    }

    /// Add a pre-rendered string as key material.
    #[must_use]
    pub fn with_str(mut self, name: impl Into<String>, material: impl Into<String>) -> Self {
        self.parts.push((name.into(), material.into()));
        self
    }

    /// Add a serializable value as key material.
    ///
    /// # Panics
    ///
    /// Panics if `val` cannot be serialized to JSON. Key material is expected
    /// to be plain data.
    #[must_use]
    pub fn with_value<T: Serialize>(self, name: impl Into<String>, val: &T) -> Self {
        let material =
            serde_json::to_string(val).expect("memo key material must be serializable to json");
        self.with_str(name, material)
    }

    /// Add a file or directory as key material, stamping it (and, for
    /// directories, everything under it) with [`FileStamp`].
    ///
    /// `rt` is unused, and is present purely to statically prove this is being
    /// called at runtime. See [`Self::with_tool`].
    pub fn with_path(
        self,
        rt: &RustRuntimeServices<'_>,
        name: impl Into<String>,
        path: &Path,
    ) -> anyhow::Result<Self> {
        let _ = rt;
        let material = stamp_path(path)
            .with_context(|| format!("while stamping memo key input {}", path.display()))?;
        Ok(self.with_str(name, material))
    }

    /// Add an external tool as key material, by resolving `name` on `PATH` and
    /// stamping the resulting binary.
    ///
    /// This is preferable to keying on something like `tool --version`: it
    /// avoids spawning a process, and it catches changes a version string
    /// misses (a distro rebuilding the same version, or `PATH` being pointed at
    /// a different install).
    ///
    /// Resolution is best-effort. If the tool can't be found on `PATH` - which
    /// happens legitimately when commands are run through a wrapper, e.g. a WSL
    /// shim - the key records that fact and moves on, rather than failing a
    /// step that would otherwise have worked.
    ///
    /// # Phase
    ///
    /// `rt` is unused, and is present purely to statically prove this is being
    /// called at runtime. Inspecting `PATH` (or the filesystem generally) at
    /// flow-build time would be wrong in two ways: the key would be computed
    /// before earlier steps have had a chance to _install_ the tool, and on CI
    /// backends it would sample the machine that generated the pipeline rather
    /// than the agent that runs it.
    ///
    /// Node authors building a key declaratively should use
    /// [`MemoKeySpec::tool`], which defers resolution to runtime for them.
    #[must_use]
    pub fn with_tool(self, rt: &RustRuntimeServices<'_>, name: &str) -> Self {
        let _ = rt;
        self.with_str(format!("tool:{name}"), stamp_tool(name))
    }

    /// A stable hex digest of this key, used as the store's entry name.
    pub fn hash(&self) -> String {
        let mut hasher = sha2::Sha256::new();
        for (name, material) in &self.parts {
            hasher.update(name.as_bytes());
            hasher.update([0]);
            hasher.update(material.as_bytes());
            hasher.update([0]);
        }
        let digest = hasher.finalize();
        // 128 bits of a sha256 is plenty to make collisions a non-concern,
        // while keeping store paths readable.
        digest[..16].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A human-readable rendering of the key, recorded alongside each entry so
    /// that a suspicious cache hit can actually be investigated.
    pub fn description(&self) -> String {
        self.parts
            .iter()
            .map(|(name, material)| format!("{name} = {material}\n"))
            .collect()
    }
}

/// Resolve `name` on `PATH` and stamp the binary it resolves to.
///
/// Best-effort: see [`MemoKey::with_tool`].
fn stamp_tool(name: &str) -> String {
    let path = match which::which(name) {
        Ok(path) => path,
        Err(err) => {
            log::debug!("memo key: could not resolve tool {name:?} on PATH: {err}");
            return "unresolved".into();
        }
    };

    // stamp the symlink's _target_, so that an alternatives-style
    // `objcopy -> objcopy.gold` flip, or an in-place package upgrade, is
    // visible in the key.
    match FileStamp::new_following(&path) {
        Ok(stamp) => format!("{}:{stamp}", path.display()),
        Err(err) => {
            log::debug!(
                "memo key: could not stamp tool at {}: {err}",
                path.display()
            );
            format!("{}:unstampable", path.display())
        }
    }
}

/// Recursively stamp `path`, producing a deterministic string.
fn stamp_path(path: &Path) -> anyhow::Result<String> {
    let meta = fs_err::symlink_metadata(path)?;

    if meta.is_symlink() {
        let target = fs_err::read_link(path)?;
        return Ok(format!("symlink->{}", target.display()));
    }

    if meta.is_file() {
        return Ok(FileStamp::new(path)?.to_string());
    }

    let mut out = String::from("dir{");
    let mut entries = fs_err::read_dir(path)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    for name in entries {
        let child = path.join(&name);
        out.push_str(&name.to_string_lossy());
        out.push(':');
        out.push_str(&stamp_path(&child)?);
        out.push(';');
    }
    out.push('}');
    Ok(out)
}

/// Metadata recorded alongside each store entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoEntryMeta {
    /// Hex digest of the key that produced this entry.
    pub key_hash: String,
    /// Human-readable rendering of the key, for debugging.
    pub key_description: String,
    /// When the entry was created, in seconds since the unix epoch.
    pub created_unix_secs: u64,
    /// Total size of the entry's output dir, in bytes.
    pub size_bytes: u64,
    /// Stamp of the entry's whole output dir, used to detect external mutation.
    pub dir_stamp: String,
}

/// A store entry, either freshly created or restored from a previous run.
///
/// An entry _is_ its directory - there is no separate notion of which files in
/// it "count". Consumers navigate [`Self::dir`] using whatever layout the
/// producing step established.
#[derive(Clone, Debug)]
pub struct MemoEntry {
    /// Directory containing the entry's contents.
    pub dir: PathBuf,
    /// The entry's metadata.
    pub meta: MemoEntryMeta,
    /// Whether this entry was restored from a previous run (as opposed to
    /// having just been created).
    pub hit: bool,
}

/// A partially-built store entry.
///
/// Put the entry's contents in [`Self::dir`], then hand this to
/// [`MemoStore::commit`]. Dropping it without committing discards the work.
pub struct MemoStaging {
    /// `None` once committed, so the drop guard doesn't delete an installed
    /// entry.
    root: Option<PathBuf>,
    /// The `out` subdirectory of `root`, so the entry's metadata can sit
    /// alongside the contents without colliding with them.
    out: PathBuf,
}

impl MemoStaging {
    /// The directory to build the entry's contents in.
    pub fn dir(&self) -> &Path {
        &self.out
    }

    fn take(&mut self) -> PathBuf {
        self.root.take().expect("commit consumes self")
    }
}

impl Drop for MemoStaging {
    fn drop(&mut self) {
        if let Some(root) = self.root.take() {
            let _ = fs_err::remove_dir_all(root);
        }
    }
}

/// Summary of the store's contents.
#[derive(Clone, Debug)]
pub struct MemoStats {
    /// Root of the store.
    pub root: PathBuf,
    /// Number of entries.
    pub entries: usize,
    /// Total size of all entries, in bytes.
    pub size_bytes: u64,
}

/// A single entry, as reported by [`MemoStore::entries`].
#[derive(Clone, Debug)]
pub struct MemoEntryInfo {
    /// Hex digest naming the entry.
    pub hash: String,
    /// Size of the entry, in bytes.
    pub size_bytes: u64,
    /// When the entry was last used, in seconds since the unix epoch.
    pub last_used_unix_secs: u64,
    /// The entry's metadata, if it could be read.
    pub meta: Option<MemoEntryMeta>,
}

/// Summary of what a [`MemoStore::gc`] pass did.
#[derive(Clone, Debug)]
pub struct MemoGcSummary {
    /// Number of entries evicted.
    pub evicted_entries: usize,
    /// Number of bytes reclaimed.
    pub evicted_bytes: u64,
    /// Total size of the store after the pass, in bytes.
    pub remaining_bytes: u64,
}

/// A key-addressed store of memoized step outputs.
///
/// This is a lightweight handle - it's cheap to clone, and it round-trips
/// through flowey's Var database, so nodes can pass a `ReadVar<MemoStore>`
/// around rather than a bare path that every consumer has to know how to
/// interpret. Obtain one via [`NodeCtx::memo_store`].
///
/// See the [module docs](self) for the overall design.
///
/// [`NodeCtx::memo_store`]: super::NodeCtx::memo_store
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoStore {
    root: PathBuf,
}

impl MemoStore {
    /// Open (creating if necessary) the store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let store = Self { root: root.into() };
        store.ensure_dirs()?;
        Ok(store)
    }

    /// Open the store without creating it, returning `None` if it doesn't
    /// exist yet.
    pub fn open_existing(root: impl Into<PathBuf>) -> anyhow::Result<Option<Self>> {
        let root: PathBuf = root.into();
        if !root.join(ENTRIES_DIR).exists() {
            return Ok(None);
        }
        Ok(Some(Self { root }))
    }

    /// Create the store's directory skeleton.
    ///
    /// Cheap, and re-run on each insertion, since a handle may have been
    /// deserialized long after the store it names was blown away (e.g: by a
    /// concurrent `cargo xflowey cache clear`).
    fn ensure_dirs(&self) -> anyhow::Result<()> {
        fs_err::create_dir_all(self.root.join(ENTRIES_DIR))?;
        fs_err::create_dir_all(self.root.join(TMP_DIR))?;
        Ok(())
    }

    /// The store's root dir.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entry_dir(&self, hash: &str) -> PathBuf {
        self.root.join(ENTRIES_DIR).join(hash)
    }

    /// Look up a previously-stored entry.
    ///
    /// Entries whose declared outputs no longer match their recorded stamps
    /// (i.e: something modified them in-place) are evicted and reported as a
    /// miss.
    pub fn lookup(&self, key: &MemoKey) -> anyhow::Result<Option<MemoEntry>> {
        let hash = key.hash();
        let entry_dir = self.entry_dir(&hash);
        let out_dir = entry_dir.join(OUT_DIR);

        let Ok(meta) = fs_err::read(entry_dir.join(META_FILE)) else {
            return Ok(None);
        };
        let meta: MemoEntryMeta = match serde_json::from_slice(&meta) {
            Ok(meta) => meta,
            Err(err) => {
                log::warn!("evicting memo entry {hash} with unreadable metadata: {err}");
                self.evict(&hash)?;
                return Ok(None);
            }
        };

        // The whole directory is the entry, so the whole directory is what has
        // to be intact - checking only the paths the step reported would miss
        // everything else it left behind.
        match stamp_path(&out_dir) {
            Ok(actual) if actual == meta.dir_stamp => {}
            _ => {
                log::warn!("evicting memo entry {hash}: its contents were modified or removed");
                self.evict(&hash)?;
                return Ok(None);
            }
        }

        self.touch(&entry_dir);

        Ok(Some(MemoEntry {
            dir: out_dir,
            meta,
            hit: true,
        }))
    }

    /// Return the entry for `key`, running `f` to populate it if it isn't
    /// already in the store.
    ///
    /// `f` is handed an empty directory to fill; the whole directory becomes
    /// the entry, and is reachable afterwards as [`MemoEntry::dir`].
    ///
    /// The directory is atomically installed into the store once `f` succeeds,
    /// so a crashed or failed run never leaves a partial entry behind.
    ///
    /// When memoization is disabled via `FLOWEY_NO_MEMO`, `f` always runs and
    /// its output is handed back from a scratch dir rather than an installed
    /// entry. That scratch dir necessarily outlives this call - the returned
    /// path points into it - and is reclaimed by the next [`Self::gc`].
    ///
    /// Entries made this way have no declared outputs - the caller navigates
    /// [`MemoEntry::dir`] itself - but are still covered by the
    /// external-mutation check in [`Self::lookup`], which stamps the whole
    /// directory.
    ///
    /// Callers that can't produce their output directly into a single
    /// directory - e.g. one bulk download that has to be split across several
    /// entries - should drive [`Self::lookup`], [`Self::stage`] and
    /// [`Self::commit`] themselves.
    pub fn get_or_insert_with(
        &self,
        key: &MemoKey,
        f: impl FnOnce(&Path) -> anyhow::Result<()>,
    ) -> anyhow::Result<MemoEntry> {
        if let Some(entry) = self.lookup(key)? {
            log::info!("memo hit: {}", key.hash());
            return Ok(entry);
        }

        log::info!("memo miss: {}", key.hash());

        let staging = self.stage()?;
        f(staging.dir())?;
        self.commit(key, staging)
    }

    /// Begin building an entry.
    ///
    /// The returned handle owns a private directory; put the entry's contents
    /// in [`MemoStaging::dir`] and then hand it to [`Self::commit`]. Dropping it
    /// without committing throws the work away.
    ///
    /// Together with [`Self::lookup`] this is the un-sugared form of
    /// [`Self::get_or_insert_with`], for callers that can't produce their
    /// output directly into one directory - e.g. a bulk download that has to be
    /// split across several entries.
    pub fn stage(&self) -> anyhow::Result<MemoStaging> {
        self.ensure_dirs()?;
        let root = self.new_tmp_dir()?;
        let out = root.join(OUT_DIR);
        fs_err::create_dir_all(&out)?;
        Ok(MemoStaging {
            root: Some(root),
            out,
        })
    }

    /// Atomically install a [`Self::stage`] directory as the entry for `key`.
    ///
    /// When memoization is disabled via `FLOWEY_NO_MEMO` the contents are left
    /// where they are rather than entering the store, so that a disabled run
    /// behaves as though it had never memoized. The staging dir is reclaimed by
    /// the next [`Self::gc`].
    pub fn commit(&self, key: &MemoKey, mut staging: MemoStaging) -> anyhow::Result<MemoEntry> {
        let root = staging.take();

        if memoization_disabled() {
            log::info!("memoization disabled (FLOWEY_NO_MEMO) - key {}", key.hash());
            let out_dir = root.join(OUT_DIR);
            return Ok(MemoEntry {
                meta: self.build_meta(key, &key.hash(), &out_dir)?,
                dir: out_dir,
                hit: false,
            });
        }

        self.install(key, &key.hash(), &root)
    }

    /// Write out metadata for a populated scratch dir and atomically install it
    /// as the entry for `key`.
    ///
    /// Consumes `scratch` - it is either renamed into the store, or removed.
    fn install(&self, key: &MemoKey, hash: &str, scratch: &Path) -> anyhow::Result<MemoEntry> {
        let out_dir = scratch.join(OUT_DIR);

        let res = (|| {
            let meta = self.build_meta(key, hash, &out_dir)?;
            fs_err::write(scratch.join(META_FILE), serde_json::to_vec_pretty(&meta)?)?;
            anyhow::Ok(meta)
        })();

        let meta = match res {
            Ok(meta) => meta,
            Err(err) => {
                let _ = fs_err::remove_dir_all(scratch);
                return Err(err);
            }
        };

        // atomically install the entry. a concurrent flowey run may have
        // gotten there first, in which case we simply use theirs.
        let entry_dir = self.entry_dir(hash);
        match fs_err::rename(scratch, &entry_dir) {
            Ok(()) => {}
            Err(err) => {
                let _ = fs_err::remove_dir_all(scratch);
                if let Some(entry) = self.lookup(key)? {
                    log::info!("memo entry {hash} was installed concurrently - using it");
                    return Ok(entry);
                }
                return Err(err).context("while installing memo entry");
            }
        }

        self.touch(&entry_dir);

        Ok(MemoEntry {
            dir: entry_dir.join(OUT_DIR),
            meta,
            hit: false,
        })
    }

    fn build_meta(
        &self,
        key: &MemoKey,
        hash: &str,
        out_dir: &Path,
    ) -> anyhow::Result<MemoEntryMeta> {
        Ok(MemoEntryMeta {
            key_hash: hash.to_owned(),
            key_description: key.description(),
            created_unix_secs: now_unix_secs(),
            size_bytes: dir_size(out_dir)?,
            dir_stamp: stamp_path(out_dir)?,
        })
    }

    fn new_tmp_dir(&self) -> anyhow::Result<PathBuf> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = self.root.join(TMP_DIR).join(format!(
            "{}-{}-{}",
            std::process::id(),
            now_unix_secs(),
            n
        ));
        // a leftover dir from a previous run with the same pid/timestamp would
        // be catastrophic to reuse, so start from scratch.
        if dir.exists() {
            fs_err::remove_dir_all(&dir)?;
        }
        fs_err::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Record that an entry was used, for the benefit of [`Self::gc`].
    fn touch(&self, entry_dir: &Path) {
        let _ = fs_err::write(entry_dir.join(USED_FILE), now_unix_secs().to_string());
    }

    fn last_used(&self, entry_dir: &Path) -> u64 {
        fs_err::read_to_string(entry_dir.join(USED_FILE))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Remove a single entry.
    fn evict(&self, hash: &str) -> anyhow::Result<()> {
        let entry_dir = self.entry_dir(hash);
        if !entry_dir.exists() {
            return Ok(());
        }
        // move it out of the way first, so that a concurrent lookup never sees
        // a half-deleted entry.
        let doomed_parent = self.new_tmp_dir()?;
        match fs_err::rename(&entry_dir, doomed_parent.join("doomed")) {
            Ok(()) => fs_err::remove_dir_all(&doomed_parent)?,
            // someone else is already evicting it
            Err(_) => {
                let _ = fs_err::remove_dir_all(&doomed_parent);
            }
        }
        Ok(())
    }

    /// List every entry currently in the store.
    pub fn entries(&self) -> anyhow::Result<Vec<MemoEntryInfo>> {
        let mut out = Vec::new();
        let entries_dir = self.root.join(ENTRIES_DIR);
        if !entries_dir.exists() {
            return Ok(out);
        }
        for e in fs_err::read_dir(&entries_dir)? {
            let e = e?;
            if !e.file_type()?.is_dir() {
                continue;
            }
            let dir = e.path();
            let meta = fs_err::read(dir.join(META_FILE))
                .ok()
                .and_then(|raw| serde_json::from_slice::<MemoEntryMeta>(&raw).ok());
            out.push(MemoEntryInfo {
                hash: e.file_name().to_string_lossy().into_owned(),
                size_bytes: meta.as_ref().map(|m| m.size_bytes).unwrap_or(0),
                last_used_unix_secs: self.last_used(&dir),
                meta,
            });
        }
        Ok(out)
    }

    /// Summarize the store's contents.
    pub fn stats(&self) -> anyhow::Result<MemoStats> {
        let entries = self.entries()?;
        Ok(MemoStats {
            root: self.root.clone(),
            entries: entries.len(),
            size_bytes: entries.iter().map(|e| e.size_bytes).sum(),
        })
    }

    /// Evict least-recently-used entries until the store is at or below
    /// `max_bytes`, and clean up any stale scratch dirs.
    pub fn gc(&self, max_bytes: u64) -> anyhow::Result<MemoGcSummary> {
        self.gc_tmp()?;

        let mut entries = self.entries()?;
        let mut total: u64 = entries.iter().map(|e| e.size_bytes).sum();

        // oldest first
        entries.sort_by_key(|e| e.last_used_unix_secs);

        // Entries touched very recently may belong to a _concurrently running_
        // flowey, which is holding paths into them. Evicting one out from under
        // it would turn into a confusing mid-run I/O error, so leave them alone
        // and let a later pass reclaim them.
        //
        // Note that this is belt-and-braces: `lookup` touches on every hit, so
        // an in-use entry is also the _most_ recently used and would be the
        // last thing LRU picks anyway.
        let now = now_unix_secs();
        let in_use_cutoff = now.saturating_sub(EVICTION_GRACE_SECS);

        let mut evicted_entries = 0;
        let mut evicted_bytes = 0;
        let mut skipped_in_use = 0;
        for entry in entries {
            if total <= max_bytes {
                break;
            }
            if entry.last_used_unix_secs > in_use_cutoff {
                skipped_in_use += 1;
                continue;
            }
            self.evict(&entry.hash)?;
            total = total.saturating_sub(entry.size_bytes);
            evicted_bytes += entry.size_bytes;
            evicted_entries += 1;
        }

        if skipped_in_use > 0 && total > max_bytes {
            log::info!(
                "memo store is over its cap, but {skipped_in_use} entries were skipped as \
                 possibly in use by a concurrent run"
            );
        }

        Ok(MemoGcSummary {
            evicted_entries,
            evicted_bytes,
            remaining_bytes: total,
        })
    }

    /// Remove scratch dirs left behind by crashed runs, plus any belonging to
    /// this process.
    ///
    /// GC runs after the pipeline has finished, so anything this process staged
    /// is definitively done with - which is what keeps `FLOWEY_NO_MEMO` from
    /// piling up scratch dirs, since in that mode outputs are handed back from
    /// a scratch dir rather than an installed entry.
    fn gc_tmp(&self) -> anyhow::Result<()> {
        let tmp = self.root.join(TMP_DIR);
        if !tmp.exists() {
            return Ok(());
        }
        let our_prefix = format!("{}-", std::process::id());
        let now = now_unix_secs();
        for e in fs_err::read_dir(&tmp)? {
            let e = e?;

            if e.file_name().to_string_lossy().starts_with(&our_prefix) {
                let _ = fs_err::remove_dir_all(e.path());
                continue;
            }

            let age = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| now.saturating_sub(d.as_secs()))
                .unwrap_or(0);
            if age > TMP_MAX_AGE_SECS {
                let _ = fs_err::remove_dir_all(e.path());
            }
        }
        Ok(())
    }

    /// Remove every entry in the store.
    pub fn clear(&self) -> anyhow::Result<()> {
        for dir in [ENTRIES_DIR, TMP_DIR] {
            let dir = self.root.join(dir);
            if dir.exists() {
                fs_err::remove_dir_all(&dir)?;
            }
            fs_err::create_dir_all(&dir)?;
        }
        Ok(())
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn dir_size(dir: &Path) -> anyhow::Result<u64> {
    let mut total = 0;
    for e in fs_err::read_dir(dir)? {
        let e = e?;
        let meta = e.metadata()?;
        if meta.is_dir() {
            total += dir_size(&e.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

/// A declarative description of a memoized step's key, resolved into a
/// [`MemoKey`] at runtime.
///
/// Unlike [`MemoKey`], this can reference flowey Vars, whose values (and, for
/// path Vars, whose on-disk stamps) aren't known until the step actually runs.
pub struct MemoKeySpec {
    recipe: String,
    version: u32,
    static_parts: Vec<(String, String)>,
    tool_parts: Vec<String>,
    path_parts: Vec<(String, ReadVar<PathBuf>)>,
    value_parts: Vec<(String, Box<dyn KeyVar>)>,
}

/// A [`MemoKeySpec`] which has been claimed by a step.
pub struct ClaimedMemoKeySpec {
    recipe: String,
    version: u32,
    static_parts: Vec<(String, String)>,
    tool_parts: Vec<String>,
    path_parts: Vec<(String, ClaimedReadVar<PathBuf>)>,
    value_parts: Vec<(String, Box<dyn ClaimedKeyVar>)>,
}

/// Type-erased [`ReadVar`] whose value contributes to a memo key.
trait KeyVar {
    fn claim_boxed(self: Box<Self>, ctx: &mut StepCtx<'_>) -> Box<dyn ClaimedKeyVar>;
}

/// The claimed counterpart of [`KeyVar`].
trait ClaimedKeyVar {
    fn read_material(self: Box<Self>, rt: &mut RustRuntimeServices<'_>) -> String;
}

impl<T: Serialize + DeserializeOwned + 'static> KeyVar for ReadVar<T> {
    fn claim_boxed(self: Box<Self>, ctx: &mut StepCtx<'_>) -> Box<dyn ClaimedKeyVar> {
        Box::new((*self).claim(ctx))
    }
}

impl<T: Serialize + DeserializeOwned + 'static> ClaimedKeyVar for ClaimedReadVar<T> {
    fn read_material(self: Box<Self>, rt: &mut RustRuntimeServices<'_>) -> String {
        let val = rt.read(*self);
        serde_json::to_string(&val).expect("memo key material must be serializable to json")
    }
}

impl MemoKeySpec {
    /// Start a new key spec. See [`MemoKey::new`] for the meaning of `recipe`
    /// and `version`.
    pub fn new(recipe: impl Into<String>, version: u32) -> Self {
        Self {
            recipe: recipe.into(),
            version,
            static_parts: Vec::new(),
            tool_parts: Vec::new(),
            path_parts: Vec::new(),
            value_parts: Vec::new(),
        }
    }

    /// Add a value that's known at flow-build time as key material.
    ///
    /// # Panics
    ///
    /// Panics if `val` cannot be serialized to JSON.
    #[must_use]
    pub fn value<T: Serialize>(mut self, name: impl Into<String>, val: &T) -> Self {
        let material =
            serde_json::to_string(val).expect("memo key material must be serializable to json");
        self.static_parts.push((name.into(), material));
        self
    }

    /// Add a file or directory as key material. The path is stamped at runtime.
    #[must_use]
    pub fn path(mut self, name: impl Into<String>, path: &ReadVar<PathBuf>) -> Self {
        self.path_parts.push((name.into(), path.clone()));
        self
    }

    /// Add an external tool as key material. See [`MemoKey::with_tool`].
    ///
    /// The tool is resolved on `PATH` and stamped at runtime, so this correctly
    /// picks up tools that were installed by an earlier step in the same flow.
    #[must_use]
    pub fn tool(mut self, name: impl Into<String>) -> Self {
        self.tool_parts.push(name.into());
        self
    }

    /// Add a runtime Var's _value_ (not the file it may point at) as key
    /// material.
    #[must_use]
    pub fn var<T>(mut self, name: impl Into<String>, var: &ReadVar<T>) -> Self
    where
        T: Serialize + DeserializeOwned + 'static,
    {
        self.value_parts.push((name.into(), Box::new(var.clone())));
        self
    }
}

impl ClaimedMemoKeySpec {
    /// Resolve the spec into a concrete [`MemoKey`].
    pub fn resolve(self, rt: &mut RustRuntimeServices<'_>) -> anyhow::Result<MemoKey> {
        let Self {
            recipe,
            version,
            static_parts,
            tool_parts,
            path_parts,
            value_parts,
        } = self;

        let mut key = MemoKey::new(recipe, version);
        for (name, material) in static_parts {
            key = key.with_str(name, material);
        }
        for name in tool_parts {
            key = key.with_tool(rt, &name);
        }
        for (name, var) in value_parts {
            let material = var.read_material(rt);
            key = key.with_str(name, material);
        }
        for (name, var) in path_parts {
            let path = rt.read(var);
            key = key.with_path(rt, name, &path)?;
        }
        Ok(key)
    }
}

impl ClaimVar for MemoKeySpec {
    type Claimed = ClaimedMemoKeySpec;

    fn claim(self, ctx: &mut StepCtx<'_>) -> Self::Claimed {
        let Self {
            recipe,
            version,
            static_parts,
            tool_parts,
            path_parts,
            value_parts,
        } = self;
        ClaimedMemoKeySpec {
            recipe,
            version,
            static_parts,
            tool_parts,
            path_parts: path_parts
                .into_iter()
                .map(|(name, var)| (name, var.claim(ctx)))
                .collect(),
            value_parts: value_parts
                .into_iter()
                .map(|(name, var)| (name, var.claim_boxed(ctx)))
                .collect(),
        }
    }
}

/// The runtime half of a memoized step.
///
/// Returns the directory holding the step's results: a store entry when
/// memoization is active, or the step's own working dir when it isn't. Callers
/// derive whatever paths they need from it.
///
/// `body` runs only when the entry has to be produced, and is an ordinary
/// flowey step body - it does its work in the working directory, which is
/// pointed at a staging dir for the duration.
pub(crate) fn run_memoized(
    rt: &mut RustRuntimeServices<'_>,
    store: Option<ClaimedReadVar<MemoStore>>,
    key: ClaimedMemoKeySpec,
    body: impl FnOnce(&mut RustRuntimeServices<'_>) -> anyhow::Result<()>,
) -> anyhow::Result<PathBuf> {
    let store = store.map(|store| rt.read(store));

    // `FLOWEY_NO_MEMO` is handled here rather than deferring to the store, so
    // that it is genuinely equivalent to never having memoized: the work lands
    // in the step's own working dir, which flowey wipes after every run.
    let store = store.filter(|_| !memoization_disabled());

    let Some(store) = store else {
        // no persistent storage available (e.g: CI), or memoization disabled -
        // just do the work, right where the step already is.
        body(rt)?;
        return Ok(rt.sh.current_dir());
    };

    let key = key.resolve(rt)?;
    let hash = key.hash();

    if let Some(entry) = store.lookup(&key)? {
        log::info!("memo hit: {hash}");
        return Ok(entry.dir);
    }

    log::info!("memo miss: {hash}");

    let staging = store.stage()?;

    // Flowey's contract is that a step's working directory is where it does its
    // work, so point it at the staging dir and let the body be an ordinary
    // step. Restored afterwards so the rest of the step behaves normally.
    let orig = rt.sh.current_dir();
    let res = with_working_dir(rt, staging.dir(), body);
    with_working_dir(rt, &orig, |_| Ok(()))?;
    res?;

    Ok(store.commit(&key, staging)?.dir)
}

/// Run `f` with the shell's working directory temporarily set to `dir`.
fn with_working_dir<R>(
    rt: &mut RustRuntimeServices<'_>,
    dir: &Path,
    f: impl FnOnce(&mut RustRuntimeServices<'_>) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    // flowey steps observe the working dir through both the shell and the
    // process, so both have to move.
    std::env::set_current_dir(dir)?;
    rt.sh.change_dir(dir);
    f(rt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(v: u32) -> MemoKey {
        MemoKey::new("test", v).with_value("flag", &true)
    }

    #[test]
    fn key_hash_is_stable_and_sensitive() {
        assert_eq!(key(1).hash(), key(1).hash());
        assert_ne!(key(1).hash(), key(2).hash());
        assert_ne!(
            MemoKey::new("test", 1).with_value("a", &1).hash(),
            MemoKey::new("test", 1).with_value("b", &1).hash()
        );
    }

    #[test]
    fn insert_then_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoStore::open(tmp.path()).unwrap();

        let mut ran = 0;
        for _ in 0..2 {
            let entry = store
                .get_or_insert_with(&key(1), |out_dir| {
                    ran += 1;
                    fs_err::write(out_dir.join("thing"), b"hello")?;
                    Ok(())
                })
                .unwrap();
            assert_eq!(fs_err::read(entry.dir.join("thing")).unwrap(), b"hello");
        }
        assert_eq!(ran, 1);
    }

    #[test]
    fn failed_body_leaves_no_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoStore::open(tmp.path()).unwrap();

        let res = store.get_or_insert_with(&key(1), |_| anyhow::bail!("nope"));
        assert!(res.is_err());
        assert!(store.lookup(&key(1)).unwrap().is_none());
        assert_eq!(store.stats().unwrap().entries, 0);
    }

    #[test]
    fn mutated_output_is_evicted() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoStore::open(tmp.path()).unwrap();

        let entry = store
            .get_or_insert_with(&key(1), |out_dir| {
                fs_err::write(out_dir.join("thing"), b"hello")?;
                Ok(())
            })
            .unwrap();

        fs_err::write(entry.dir.join("thing"), b"tampered!").unwrap();

        assert!(store.lookup(&key(1)).unwrap().is_none());
    }

    #[test]
    fn added_file_is_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoStore::open(tmp.path()).unwrap();

        let entry = store
            .get_or_insert_with(&key(1), |out_dir| {
                fs_err::write(out_dir.join("thing"), b"hello")?;
                Ok(())
            })
            .unwrap();

        // stamping the whole directory catches files appearing or vanishing,
        // not just changes to ones the step declared
        fs_err::write(entry.dir.join("extra"), b"surprise").unwrap();

        assert!(store.lookup(&key(1)).unwrap().is_none());
    }

    #[test]
    fn gc_evicts_least_recently_used() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoStore::open(tmp.path()).unwrap();

        for i in 0..4u32 {
            store
                .get_or_insert_with(&key(i), |out_dir| {
                    fs_err::write(out_dir.join("thing"), vec![0; 1024])?;
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(store.stats().unwrap().entries, 4);

        // backdate everything past the in-use grace period, so LRU is free to
        // evict
        backdate_all(&store);

        let summary = store.gc(2048).unwrap();
        assert!(summary.remaining_bytes <= 2048);
        assert_eq!(store.stats().unwrap().entries, 2);
    }

    /// Rewrite every entry's `used` marker to be well outside the eviction
    /// grace period.
    fn backdate_all(store: &MemoStore) {
        let old = now_unix_secs() - EVICTION_GRACE_SECS * 2;
        for entry in store.entries().unwrap() {
            fs_err::write(
                store.entry_dir(&entry.hash).join(USED_FILE),
                old.to_string(),
            )
            .unwrap();
        }
    }

    #[test]
    fn gc_spares_recently_used_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoStore::open(tmp.path()).unwrap();

        for i in 0..4u32 {
            store
                .get_or_insert_with(&key(i), |out_dir| {
                    fs_err::write(out_dir.join("thing"), vec![0; 1024])?;
                    Ok(())
                })
                .unwrap();
        }

        // all four were just touched, so a concurrent run could be using any of
        // them - GC must leave them be even though it's way over cap
        let summary = store.gc(0).unwrap();
        assert_eq!(summary.evicted_entries, 0);
        assert_eq!(store.stats().unwrap().entries, 4);
    }

    #[test]
    fn gc_reclaims_this_process_scratch_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoStore::open(tmp.path()).unwrap();

        // a scratch dir this process left behind, e.g. by FLOWEY_NO_MEMO
        let scratch = store.new_tmp_dir().unwrap();
        assert!(scratch.exists());

        // and one from some other, still-possibly-live process
        let other = tmp.path().join(TMP_DIR).join("999999999-0-0");
        fs_err::create_dir_all(&other).unwrap();

        store.gc(u64::MAX).unwrap();

        assert!(!scratch.exists());
        assert!(other.exists());
    }

    #[test]
    fn parse_sizes() {
        assert_eq!(parse_size_bytes("512").unwrap(), 512);
        assert_eq!(parse_size_bytes("1KiB").unwrap(), 1024);
        assert_eq!(parse_size_bytes("2 gb").unwrap(), 2 * 1024 * 1024 * 1024);
        assert!(parse_size_bytes("12 parsecs").is_err());
    }

    fn item_key(id: &str) -> MemoKey {
        MemoKey::new("batch", 1).with_str("id", id)
    }

    /// The shape `download_gh_release` uses: fetch everything that missed into
    /// one scratch dir, then split it into per-item entries.
    fn fetch_missing(
        store: &MemoStore,
        ids: &[&str],
    ) -> anyhow::Result<(Vec<String>, Vec<PathBuf>)> {
        let mut misses = Vec::new();
        let mut paths = Vec::new();

        for id in ids {
            match store.lookup(&item_key(id))? {
                Some(entry) => paths.push(entry.dir.join(id)),
                None => misses.push((*id).to_owned()),
            }
        }

        let fetched = misses.clone();
        if !misses.is_empty() {
            let bulk = store.stage()?;
            for id in &misses {
                fs_err::write(bulk.dir().join(id), id.as_bytes())?;
            }
            for id in &misses {
                let staging = store.stage()?;
                fs_err::rename(bulk.dir().join(id), staging.dir().join(id))?;
                paths.push(store.commit(&item_key(id), staging)?.dir.join(id));
            }
        }

        Ok((fetched, paths))
    }

    #[test]
    fn staged_entries_are_independently_reusable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoStore::open(tmp.path()).unwrap();

        // seed two of the three
        fetch_missing(&store, &["a", "b"]).unwrap();

        // only the third should need fetching
        let (fetched, paths) = fetch_missing(&store, &["a", "b", "c"]).unwrap();
        assert_eq!(fetched, ["c"]);
        assert_eq!(paths.len(), 3);
        for path in paths {
            let id = path.file_name().unwrap().to_string_lossy().into_owned();
            assert_eq!(fs_err::read(&path).unwrap(), id.as_bytes());
        }

        // and an entry created that way is an ordinary entry
        let entry = store
            .get_or_insert_with(&item_key("a"), |_| panic!("should have hit"))
            .unwrap();
        assert!(entry.hit);
    }

    #[test]
    fn uncommitted_staging_is_discarded() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoStore::open(tmp.path()).unwrap();

        let dir = {
            let staging = store.stage().unwrap();
            fs_err::write(staging.dir().join("thing"), b"hello").unwrap();
            staging.dir().to_path_buf()
            // dropped without committing
        };

        assert!(!dir.exists());
        assert_eq!(store.stats().unwrap().entries, 0);
    }
}
