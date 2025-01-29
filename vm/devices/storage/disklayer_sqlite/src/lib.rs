// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQLite-backed disk layer implementation.
//!
//! At this time, **this layer is only designed for use in dev/test scenarios!**
//!
//! # DISCLAIMER: Stability
//!
//! There are no stability guarantees around the on-disk data format! The schema
//! can and will change without warning!
//!
//! # DISCLAIMER: Performance
//!
//! This implementation has only been minimally optimized! Don't expect to get
//! incredible perf from this disk backend!
//!
//! Notably:
//!
//! - Data is stored within a single `sectors` table as tuples of `(sector:
//!   INTEGER, sector_data: BLOB(sector_size))`. All data is accessed in
//!   `sector_size` chunks (i.e: without performing any kind of adjacent-sector
//!   coalescing).
//! - Reads and writes currently allocate many temporary `Vec<u8>` buffers per
//!   operation, without any buffer reuse.
//!
//! These design choices were made with simplicity and expediency in mind, given
//! that the primary use-case for this backend is for dev/test scenarios. If
//! performance ever becomes a concern, there are various optimizations that
//! should be possible to implement here, though quite frankly, investing in a
//! cross-platform QCOW2 or VHDX disk backend is likely a far more worthwhile
//! endeavor.
//!
//! # Context
//!
//! In late 2024, OpenVMM was missing a _cross-platform_ disk backend that
//! supported the following key features:
//!
//! - Used a dynamically-sized file as the disks's backing store
//! - Supported snapshots / differencing disks
//!
//! While OpenVMM will eventually need to support for one or more of the current
//! "industry standard" virtual disk formats that supports these features (e.g:
//! QCOW2, VHDX), we really wanted some sort of "stop-gap" solution to unblock
//! various dev/test use-cases.
//!
//! And thus, `disklayer_sqlite` was born!
//!
//! The initial implementation took less than a day to get up and running, and
//! worked "well enough" to support the dev/test scenarios we were interested
//! in, such as:
//!
//! - Having a cross-platform _sparsely allocated_ virtual disk file.
//! - Having a _persistent_ diff-disk on-top of an existing disk (as opposed to
//!   `ramdiff`, which is in-memory and _ephemeral_)
//! - Having a "cache" layer for JIT-accessed disks, such as `disk_blob`
//!
//! The idea of using SQLite as a backing store - while wacky - proved to be an
//! excellent way to quickly bring up a dynamically-sized, sparsely-allocated
//! disk format for testing in OpenVMM.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod auto_cache;
pub mod resolver;

use anyhow::Context;
use blocking::unblock;
use disk_backend::DiskError;
use disk_backend::UnmapBehavior;
use disk_layered::LayerAttach;
use disk_layered::LayerIo;
use disk_layered::SectorMarker;
use disk_layered::WriteNoOverwrite;
use futures::lock::Mutex;
use futures::lock::OwnedMutexGuard;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use rusqlite::Connection;
use scsi_buffers::RequestBuffers;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

/// Formatting parameters provided to [`FormatOnAttachSqliteDiskLayer::new`].
///
/// Optional parameters which are not provided will be determined by reading the
/// metadata of the layer being attached to.
#[derive(Inspect, Copy, Clone)]
pub struct IncompleteFormatParams {
    /// Should the layer be considered logically read only (i.e: a cache layer)
    pub logically_read_only: bool,
    /// The size of the layer in bytes.
    pub len: Option<u64>,
}

/// Formatting parameters provided to [`SqliteDiskLayer::new`]
#[derive(Inspect, Copy, Clone)]
pub struct FormatParams {
    /// Should the layer be considered logically read only (i.e: a cache layer)
    pub logically_read_only: bool,
    /// The size of the layer in bytes. Must be divisible by `sector_size`.
    pub len: u64,
    /// The size of each sector.
    pub sector_size: u32,
}

/// A disk layer backed by sqlite, which lazily infers its topology from the
/// layer it is being stacked on-top of.
pub struct FormatOnAttachSqliteDiskLayer {
    dbhd_path: PathBuf,
    read_only: bool,
    format_dbhd: IncompleteFormatParams,
}

impl FormatOnAttachSqliteDiskLayer {
    /// Create a new sqlite-backed disk layer, which is formatted when it is
    /// attached.
    pub fn new(dbhd_path: PathBuf, read_only: bool, format_dbhd: IncompleteFormatParams) -> Self {
        Self {
            dbhd_path,
            read_only,
            format_dbhd,
        }
    }
}

/// A disk layer backed entirely by sqlite.
#[derive(Inspect)]
pub struct SqliteDiskLayer {
    #[inspect(skip)]
    conn: Arc<Mutex<Connection>>, // FUTURE: switch to connection-pool instead
    meta: schema::DiskMeta,
}

impl SqliteDiskLayer {
    /// Create a new sqlite-backed disk layer.
    pub fn new(
        dbhd_path: &Path,
        read_only: bool,
        format_dbhd: Option<FormatParams>,
    ) -> anyhow::Result<Self> {
        // DEVNOTE: sqlite _really_ want to be in control of opening the file,
        // since it also wants to read/write to the runtime "sidecar" files that
        // get created when accessing the DB (i.e: the `*-shm` and `*-wal`
        // files)
        //
        // This will make it tricky to sandbox SQLite in the future...
        //
        // One idea: maybe we could implement a small SQLite `vfs` shim that
        // lets use pre-open those particular files on the caller side, and hand
        // them to sqlite when requested (vs. having it `open()` them itself?)
        let conn = Connection::open_with_flags(dbhd_path, {
            use rusqlite::OpenFlags;

            let mut flags = OpenFlags::SQLITE_OPEN_NO_MUTEX;

            if read_only {
                flags |= OpenFlags::SQLITE_OPEN_READ_ONLY;
            } else {
                flags |= OpenFlags::SQLITE_OPEN_READ_WRITE;
            }

            // FUTURE: if/when the VFS layer is implemented, it _may_ be worth
            // removing this flag entirely, and relying on the VFS to ensure
            // that the (possibly blank) db file has been created. Emphasis on
            // the word "may", as its unclear what the best approach will be
            // until if/when we have more of the VFS infrastructure in place.
            if format_dbhd.is_some() {
                flags |= OpenFlags::SQLITE_OPEN_CREATE
            }

            flags
        })?;

        let meta = if let Some(FormatParams {
            logically_read_only,
            len,
            sector_size,
        }) = format_dbhd
        {
            use rusqlite::config::DbConfig;

            // Wipe any existing contents.
            //
            // see https://www.sqlite.org/c3ref/c_dbconfig_defensive.html#sqlitedbconfigresetdatabase
            conn.set_db_config(DbConfig::SQLITE_DBCONFIG_RESET_DATABASE, true)?;
            conn.execute("VACUUM", ())?;
            conn.set_db_config(DbConfig::SQLITE_DBCONFIG_RESET_DATABASE, false)?;

            // Set core database config, and initialize table structure
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.execute(schema::DEFINE_TABLE_SECTORS, [])?;
            conn.execute(schema::DEFINE_TABLE_METADATA, [])?;

            if len % sector_size as u64 != 0 {
                anyhow::bail!(
                    "failed to format: len={len} must be multiple of sector_size={sector_size}"
                );
            }
            let sector_count = len / sector_size as u64;

            let meta = schema::DiskMeta {
                logically_read_only,
                sector_count,
                sector_size,
            };

            conn.execute(
                "INSERT INTO meta VALUES (json(?))",
                [serde_json::to_string(&meta).unwrap()],
            )?;

            meta
        } else {
            use rusqlite::OptionalExtension;
            let data: String = conn
                .query_row("SELECT json_extract(metadata, '$') FROM meta", [], |row| {
                    row.get(0)
                })
                .optional()?
                .context("missing `meta` table")?;
            serde_json::from_str(&data)?
        };

        Ok(SqliteDiskLayer {
            conn: Arc::new(Mutex::new(conn)),
            meta,
        })
    }

    async fn write_maybe_overwrite(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        overwrite: bool,
    ) -> Result<(), DiskError> {
        assert!(!(overwrite && self.meta.logically_read_only));

        let count = buffers.len() / self.meta.sector_size as usize;
        tracing::trace!(sector, count, "write");

        let buf = buffers.reader().read_all()?;
        unblock({
            let conn = self.conn.clone().lock_owned().await;
            let sector_size = self.meta.sector_size;
            move || write_sectors(conn, sector_size, sector, buf, overwrite)
        })
        .await
        .map_err(|e| DiskError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        Ok(())
    }
}

impl LayerAttach for FormatOnAttachSqliteDiskLayer {
    type Error = anyhow::Error;
    type Layer = SqliteDiskLayer;

    async fn attach(
        self,
        lower_layer_metadata: Option<disk_layered::DiskLayerMetadata>,
    ) -> Result<Self::Layer, Self::Error> {
        let len = {
            let lower_len = lower_layer_metadata
                .as_ref()
                .map(|m| m.sector_count * m.sector_size as u64);
            self.format_dbhd
                .len
                .or(lower_len)
                .context("no base layer to infer sector_count from")?
        };
        // FUTURE: make sector-size configurable
        let sector_size = lower_layer_metadata.map(|x| x.sector_size).unwrap_or(512);

        SqliteDiskLayer::new(
            &self.dbhd_path,
            self.read_only,
            Some(FormatParams {
                logically_read_only: self.format_dbhd.logically_read_only,
                len,
                sector_size,
            }),
        )
    }
}

impl LayerIo for SqliteDiskLayer {
    fn layer_type(&self) -> &str {
        "sqlite"
    }

    fn sector_count(&self) -> u64 {
        self.meta.sector_count
    }

    fn sector_size(&self) -> u32 {
        self.meta.sector_size
    }

    fn is_logically_read_only(&self) -> bool {
        self.meta.logically_read_only
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        self.meta.sector_size
    }

    fn is_fua_respected(&self) -> bool {
        false
    }

    async fn read(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        mut marker: SectorMarker<'_>,
    ) -> Result<(), DiskError> {
        let sector_count = (buffers.len() / self.meta.sector_size as usize) as u64;
        let end_sector = sector + sector_count;
        tracing::trace!(sector, sector_count, "read");
        if end_sector > self.meta.sector_count {
            return Err(DiskError::IllegalBlock);
        }

        let valid_sectors = unblock({
            let conn = self.conn.clone().lock_owned().await;
            let end_sector = sector + sector_count;
            let sector_size = self.meta.sector_size;
            move || read_sectors(conn, sector_size, sector, end_sector)
        })
        .await
        .map_err(|e| DiskError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        for (s, data) in valid_sectors {
            let offset = (s - sector) as usize * self.meta.sector_size as usize;
            let subrange = buffers.subrange(offset, self.meta.sector_size as usize);
            let mut writer = subrange.writer();
            match data {
                SectorKind::AllZero => writer.zero(self.meta.sector_size as usize)?,
                SectorKind::Data(data) => writer.write(&data)?,
            };

            marker.set(s);
        }

        Ok(())
    }

    async fn write(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        self.write_maybe_overwrite(buffers, sector, true).await
    }

    fn write_no_overwrite(&self) -> Option<impl WriteNoOverwrite> {
        Some(self)
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        tracing::trace!("sync_cache");

        unblock({
            let mut conn = self.conn.clone().lock_owned().await;
            move || -> rusqlite::Result<()> {
                // https://sqlite-users.sqlite.narkive.com/LX75NOma/forcing-a-manual-fsync-in-wal-normal-mode
                conn.pragma_update(None, "synchronous", "FULL")?;
                {
                    let tx = conn.transaction()?;
                    tx.pragma_update(None, "user_version", "0")?;
                }
                conn.pragma_update(None, "synchronous", "NORMAL")?;
                Ok(())
            }
        })
        .await
        .map_err(|e| DiskError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))
    }

    async fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        _block_level_only: bool,
        next_is_zero: bool,
    ) -> Result<(), DiskError> {
        tracing::trace!(sector_offset, sector_count, "unmap");
        if sector_offset + sector_count > self.meta.sector_count {
            return Err(DiskError::IllegalBlock);
        }

        unblock({
            let conn = self.conn.clone().lock_owned().await;
            move || unmap_sectors(conn, sector_offset, sector_count, next_is_zero)
        })
        .await
        .map_err(|e| DiskError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        Ok(())
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        UnmapBehavior::Zeroes
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        1
    }
}

impl WriteNoOverwrite for SqliteDiskLayer {
    async fn write_no_overwrite(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        self.write_maybe_overwrite(buffers, sector, false).await
    }
}

enum SectorKind {
    AllZero,
    Data(Vec<u8>),
}

// FUTURE: read from sqlite directly into `RequestBuffers`.
fn read_sectors(
    conn: OwnedMutexGuard<Connection>,
    sector_size: u32,
    start_sector: u64,
    end_sector: u64,
) -> anyhow::Result<Vec<(u64, SectorKind)>> {
    let mut select_stmt = conn.prepare_cached(
        "SELECT sector, data
        FROM sectors
        WHERE sector >= ? AND sector < ?
        ORDER BY sector ASC",
    )?;
    let mut rows = select_stmt.query(rusqlite::params![start_sector, end_sector])?;

    let mut res = Vec::new();
    while let Some(row) = rows.next()? {
        let sector: u64 = row.get(0)?;
        let data: Option<&[u8]> = row.get_ref(1)?.as_blob_or_null()?;
        let data = if let Some(data) = data {
            if data.len() != sector_size as usize {
                anyhow::bail!(
                    "db contained sector with unexpected size (expected={}, found={}, sector={:#x})",
                    sector_size,
                    data.len(),
                    sector
                )
            }
            SectorKind::Data(data.into())
        } else {
            SectorKind::AllZero
        };
        res.push((sector, data));
    }

    Ok(res)
}

// FUTURE: write into sqlite directly from `RequestBuffers`.
fn write_sectors(
    mut conn: OwnedMutexGuard<Connection>,
    sector_size: u32,
    mut sector: u64,
    buf: Vec<u8>,
    overwrite: bool,
) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    {
        let mut stmt = if overwrite {
            tx.prepare_cached("INSERT OR REPLACE INTO sectors (sector, data) VALUES (?, ?)")?
        } else {
            tx.prepare_cached("INSERT OR IGNORE INTO sectors (sector, data) VALUES (?, ?)")?
        };

        let chunks = buf.chunks_exact(sector_size as usize);
        assert!(chunks.remainder().is_empty());
        for chunk in chunks {
            if chunk.iter().all(|x| *x == 0) {
                stmt.execute(rusqlite::params![sector, rusqlite::types::Null])?;
            } else {
                stmt.execute(rusqlite::params![sector, chunk])?;
            };

            sector += 1;
        }
    }
    tx.commit()?;

    Ok(())
}

fn unmap_sectors(
    mut conn: OwnedMutexGuard<Connection>,
    sector_offset: u64,
    sector_count: u64,
    next_is_zero: bool,
) -> Result<(), rusqlite::Error> {
    if next_is_zero {
        let mut clear_stmt =
            conn.prepare_cached("DELETE FROM sectors WHERE sector BETWEEN ? AND ?")?;
        clear_stmt.execute(rusqlite::params![
            sector_offset,
            sector_offset + sector_count - 1
        ])?;
    } else {
        let tx = conn.transaction()?;
        {
            let mut stmt =
                tx.prepare_cached("INSERT OR REPLACE INTO sectors (sector, data) VALUES (?, ?)")?;

            for sector in sector_offset..(sector_offset + sector_count) {
                stmt.execute(rusqlite::params![sector, rusqlite::types::Null])?;
            }
        }
        tx.commit()?;
    }

    Ok(())
}

mod schema {
    use inspect::Inspect;
    use serde::Deserialize;
    use serde::Serialize;

    // DENOTE: SQLite actually saves the _plaintext_ of CREATE TABLE
    // statements in its file format, which makes it a pretty good place to
    // stash inline comments about the schema being used
    //
    // DEVNOTE: the choice to use the len of the blob as a marker for all
    // zero / all one sectors has not been profiled relative to other
    // implementation (e.g: having a third "kind" column).
    pub const DEFINE_TABLE_SECTORS: &str = r#"
CREATE TABLE sectors (
    -- if data is NULL, that indicates an all-zero sector.
    -- otherwise, data has len == SECTOR_SIZE, containing the sector data.
    sector INTEGER NOT NULL,
    data   BLOB,
    PRIMARY KEY (sector)
)
"#; // TODO?: enforce sqlite >3.37.0 so we can use STRICT

    // DEVNOTE: Given that this is a singleton table, we might as well use JSON
    // + serde to store whatever metadata we want here, vs. trying to bend our
    // metadata structure to sqlite's native data types.
    //
    // Using JSON (vs, say, protobuf) has the added benefit of allowing existing
    // external sqlite tooling to more easily read and manipulate the metadata
    // using sqlite's built-in JSON handling functions.
    pub const DEFINE_TABLE_METADATA: &str = r#"
CREATE TABLE meta (
    metadata TEXT NOT NULL -- stored as JSON
)
"#;

    #[derive(Debug, PartialEq, PartialOrd, Eq, Ord, Serialize, Deserialize, Inspect)]
    pub struct DiskMeta {
        pub logically_read_only: bool,
        pub sector_count: u64,
        pub sector_size: u32,
    }
}
