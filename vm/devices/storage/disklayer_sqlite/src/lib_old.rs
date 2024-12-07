#![forbid(unsafe_code)]
#![warn(missing_docs)]

use blocking::unblock;
use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use parking_lot::Mutex;
use rusqlite::Connection;
use scsi_buffers::RequestBuffers;
use std::path::Path;
use std::sync::Arc;

pub mod resolver;

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
CREATE TABLE IF NOT EXISTS sectors (
    -- schema includes a minimal "fast path" for skipping all-zero
    -- and all-one sectors.
    --
    -- if len == 0: represents all 0x00 sector
    -- if len == 1: represents all 0xff sector
    --
    -- otherwise, data has len == SECTOR_SIZE, and contains the raw
    -- sector data.
    sector INTEGER NOT NULL,
    data   BLOB NOT NULL,
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
CREATE TABLE IF NOT EXISTS meta (
    metadata TEXT NOT NULL -- stored as JSON
)
"#;

    #[derive(Debug, PartialEq, PartialOrd, Eq, Ord, Serialize, Deserialize, Inspect)]
    pub enum DiskKind {
        /// A standard, raw disk.
        ///
        /// - Writes are persisted.
        /// - Reads return existing data.
        Raw,
        /// A differencing disk on-top of an existing read-only disk.
        ///
        /// - Writes are persisted to the differencing disk, leaving the
        ///   underlying disk untouched.
        /// - Reads return data from the differencing disk, only reading from
        ///   the underlying disk if the sector hasn't been modified.
        Diff,
        /// A read-through cache on-top of an existing disk.
        ///
        /// - Reads check if the requested data is already in the cache before
        ///   reading from the underlying disk.
        /// - Writes are passed through to the underlying disk implementation.
        ReadCache,
    }

    #[derive(Debug, PartialEq, PartialOrd, Eq, Ord, Serialize, Deserialize, Inspect)]
    pub struct DiskMeta {
        pub disk_kind: DiskKind,
        pub sector_count: u64,
        pub sector_size: u32,
        pub physical_sector_size: u32,
        pub disk_id: Option<[u8; 16]>,
    }
}

/// Disk backend implementation backed by a SQLite database file.
#[derive(Inspect)]
pub struct SqliteDisk {
    #[inspect(skip)]
    conn: Arc<Mutex<Connection>>,
    meta: schema::DiskMeta,
    read_only: bool,
    lower: Disk,
}

impl SqliteDisk {
    /// Makes a new blank SQLite disk of `size` bytes.
    pub fn new(len: u64, dbhd_path: &Path, read_only: bool) -> Result<Self, anyhow::Error> {
        // // the choice of `sector_size` here was chosen entirely arbitrarily.
        // Self::new_inner(
        //     Arc::new(ZeroDisk::new(512, len)?),
        //     dbhd_path,
        //     read_only,
        //     true,
        // )
        todo!()
    }

    /// Makes a new SQLite diff disk on top of `lower`.
    ///
    /// Writes will be collected in SQLite, but reads will go to the lower disk
    /// for sectors that have not yet been overwritten.
    pub fn diff(lower: Disk, dbhd_path: &Path, read_only: bool) -> Result<Self, anyhow::Error> {
        Self::new_inner(lower, dbhd_path, read_only, false)
    }

    fn new_inner(
        lower: Disk,
        dbhd_path: &Path,
        read_only: bool,
        lower_is_zero: bool,
    ) -> Result<Self, anyhow::Error> {
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
        let conn = Connection::open(dbhd_path)?;

        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute(schema::DEFINE_TABLE_SECTORS, [])?;
        conn.execute(schema::DEFINE_TABLE_METADATA, [])?;

        let meta = {
            let sector_count = lower.sector_count();
            let sector_size = lower.sector_size();
            let physical_sector_size = lower.physical_sector_size();
            let disk_id = lower.disk_id();

            schema::DiskMeta {
                disk_kind: if lower_is_zero {
                    schema::DiskKind::Raw
                } else {
                    schema::DiskKind::Diff
                },
                sector_count,
                sector_size,
                physical_sector_size,
                disk_id,
            }
        };

        let meta_existing: Option<schema::DiskMeta> = {
            use rusqlite::OptionalExtension;

            let data: Option<String> = conn
                .query_row("SELECT json_extract(metadata, '$') FROM meta", [], |row| {
                    row.get(0)
                })
                .optional()?;

            data.as_deref().map(serde_json::from_str).transpose()?
        };

        if let Some(meta_existing) = meta_existing {
            // FUTURE: we may want to support some leeway here, (e.g: handling
            // cases where the underlying disk has been resized, or had its
            // sector sizes tweaked, or tweaked its disk id, etc...), but for
            // now, we'll take the strict approach of requiring an identical
            // configuration.
            if meta_existing != meta {
                anyhow::bail!(
                    "invalid disk configuration. expected: {:?}, found: {:?}",
                    meta_existing,
                    meta
                )
            }
        } else {
            // this is a fresh fisk
            conn.execute(
                "INSERT OR REPLACE INTO meta VALUES (json(?))",
                [serde_json::to_string(&meta).unwrap()],
            )?;
        };

        Ok(SqliteDisk {
            conn: Arc::new(Mutex::new(conn)),
            meta,
            read_only,
            lower,
        })
    }
}

#[allow(clippy::large_enum_variant)]
enum SectorKind {
    AllZero,
    AllOne,
    Data(Vec<u8>),
}

// FUTURE: pass RequestBuffers into this function directly, and avoid the alloc
// overhead.
fn read_sectors(
    conn: Arc<Mutex<Connection>>,
    sector_size: u32,
    start_sector: u64,
    end_sector: u64,
) -> Result<Vec<(u64, SectorKind)>, rusqlite::Error> {
    let conn = conn.lock();

    let mut select_stmt = conn.prepare_cached(
        "SELECT sector, data
        FROM sectors
        WHERE sector >= ? AND sector < ?
        ORDER BY sector ASC",
    )?;
    let mut rows = select_stmt.query(rusqlite::params![start_sector, end_sector])?;

    // reuse the same blob handle across all reads.
    //
    // this handle isn't easy to persist across multiple calls to
    // `read_sectors`, due to the lifetime restrictions imposed by using a
    // `Arc<Mutex<Connection>>`.
    let Some(first_row) = rows.next()? else {
        return Ok(Vec::new());
    };
    let mut sector: u64 = first_row.get(0)?;
    let mut blob = conn.blob_open(
        rusqlite::DatabaseName::Main,
        "sectors",
        "data",
        sector as i64,
        true,
    )?;

    let mut buf = vec![0; sector_size as usize];
    let mut res = Vec::new();
    loop {
        let data = match blob.len() {
            0 => SectorKind::AllZero,
            1 => SectorKind::AllOne,
            len => {
                if len != sector_size as usize {
                    return Err(rusqlite::Error::BlobSizeError);
                }
                blob.read_at(&mut buf, 0)?;
                SectorKind::Data(buf.clone())
            }
        };

        res.push((sector, data));

        let Some(next_row) = rows.next()? else {
            break;
        };

        sector = next_row.get(0)?;
        blob.reopen(sector as i64)?;
    }

    Ok(res)
}

// FUTURE: pass RequestBuffers into this function directly, and avoid the alloc
// overhead.
fn write_sectors(
    conn: Arc<Mutex<Connection>>,
    sector_size: u32,
    mut sector: u64,
    buf: Vec<u8>,
) -> Result<(), rusqlite::Error> {
    let mut conn = conn.lock();

    let tx = conn.transaction()?;
    {
        let mut stmt =
            tx.prepare_cached("INSERT OR REPLACE INTO sectors (sector, data) VALUES (?, ?)")?;

        let chunks = buf.chunks_exact(sector_size as usize);
        if !chunks.remainder().is_empty() {
            return Err(rusqlite::Error::BlobSizeError);
        }
        for chunk in chunks {
            let chunk = if chunk.iter().all(|x| *x == 0) {
                &[]
            } else if chunk.iter().all(|x| *x == 0xff) {
                &[0]
            } else {
                chunk
            };

            stmt.execute(rusqlite::params![sector, chunk])?;
            sector += 1;
        }
    }
    tx.commit()?;

    Ok(())
}

fn unmap_sectors(
    conn: Arc<Mutex<Connection>>,
    sector_offset: u64,
    sector_count: u64,
    lower_is_zero: bool,
) -> Result<(), rusqlite::Error> {
    let mut conn = conn.lock();

    if lower_is_zero {
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
                let zero_blob = &[];
                stmt.execute(rusqlite::params![sector, zero_blob])?;
            }
        }
        tx.commit()?;
    }

    Ok(())
}

impl DiskIo for SqliteDisk {
    fn disk_type(&self) -> &str {
        "sqlite"
    }

    fn sector_count(&self) -> u64 {
        self.meta.sector_count
    }

    fn sector_size(&self) -> u32 {
        self.meta.sector_size
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.meta.disk_id
    }

    fn physical_sector_size(&self) -> u32 {
        self.meta.physical_sector_size
    }

    fn is_fua_respected(&self) -> bool {
        true
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        let count = (buffers.len() / self.meta.sector_size as usize) as u64;
        tracing::debug!(sector, count, "read");

        // Always read the full lower and then overlay the changes.
        // Optimizations are possible, but some heuristics are necessary to
        // avoid lots of small reads when the disk is "Swiss cheesed".
        //
        // Box the future because otherwise it won't fit in this StackFuture.
        Box::pin(self.lower.read_vectored(buffers, sector)).await?;

        let valid_sectors = unblock({
            let conn = self.conn.clone();
            let end_sector = sector + (buffers.len() as u64 / self.meta.sector_size as u64);
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
                SectorKind::AllOne => writer.fill(0xff, self.meta.sector_size as usize)?,
                SectorKind::Data(data) => writer.write(&data)?,
            };
        }

        Ok(())
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        assert!(!self.read_only);

        let count = buffers.len() / self.meta.sector_size as usize;
        tracing::debug!(sector, count, "write");

        let buf = buffers.reader().read_all()?;
        unblock({
            let conn = self.conn.clone();
            let sector_size = self.meta.sector_size;
            move || write_sectors(conn, sector_size, sector, buf)
        })
        .await
        .map_err(|e| DiskError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        Ok(())
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        tracing::debug!("sync_cache");

        (self.conn.lock())
            .pragma_update(None, "wal_checkpoint", "FULL")
            .map_err(|e| DiskError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        Ok(())
    }

    async fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        tracing::debug!(sector_offset, sector_count, "unmap");

        unblock({
            let conn = self.conn.clone();
            let lower_is_zero = matches!(self.meta.disk_kind, schema::DiskKind::Raw);
            move || unmap_sectors(conn, sector_offset, sector_count, lower_is_zero)
        })
        .await
        .map_err(|e| DiskError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        Ok(())
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        1
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        UnmapBehavior::Zeroes
    }
}
