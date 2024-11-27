// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A layered disk implementation, [`LayeredDisk`].
//!
//! A layered disk is a disk composed of multiple layers. Each layer is a block
//! device made up of sectors, but with the added per-sector state of whether
//! the sector is present or not. When reading a sector, the layered disk will
//! read from the topmost layer that has the sector present. When writing, the
//! disk will write to the topmost layer.
//!
//! A layer can also have caching behavior. If a layer is configured to cache
//! reads, then sectors that are read from lower layers are written back to the
//! layer. If a layer is configured to write through, then writes are written to
//! the layer and the next layer. These can be useful to implement simple
//! persistent and non-persistent caches, primarily designed for lazily
//! populating local backing stores from remote sources.
//!
//! Missing from this implementation is write-back caching and cache eviction,
//! which would be needed for caches that are smaller than the disk. These
//! require potentially complicated cache management policies and are probably
//! best implemented in a separate disk implementation.

mod bitmap;
pub mod resolve;
pub mod resolver;

pub use bitmap::SectorMarker;

use bitmap::Bitmap;
use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;

/// A disk composed of multiple layers.
#[derive(Inspect)]
pub struct LayeredDisk {
    #[inspect(iter_by_index)]
    layers: Vec<Layer>,
    read_only: bool,
    is_fua_respected: bool,
    sector_shift: u32,
    disk_id: Option<[u8; 16]>,
    physical_sector_size: u32,
    unmap_behavior: UnmapBehavior,
    optimal_unmap_sectors: u32,
}

#[derive(Inspect)]
struct Layer {
    backing: Box<dyn DynLayer>,
    visible_sector_count: u64,
    read_cache: bool,
    write_through: bool,
}

/// A disk layer, for use in [`LayeredDisk`].
pub struct DiskLayer {
    backing: Box<dyn DynLayer>,
    disk_id: Option<[u8; 16]>,
    is_fua_respected: bool,
    read_only: bool,
    sector_size: u32,
    physical_sector_size: u32,
    unmap_behavior: UnmapBehavior,
    optimal_unmap_sectors: u32,
    can_read_cache: bool,
}

/// An error returned when creating a [`DiskLayer`].
#[derive(Debug, Error)]
pub enum InvalidLayer {
    /// Read caching was requested but is not supported.
    #[error("read caching was requested but is not supported")]
    ReadCacheNotSupported,
    /// Caching was requested but the layer is read only.
    #[error("caching was requested but the layer is read only")]
    ReadOnlyCache,
    /// The sector size is invalid.
    #[error("sector size {0} is invalid")]
    InvalidSectorSize(u32),
    /// The sector size of the layers do not match.
    #[error("mismatched sector size {found}, expected {expected}")]
    MismatchedSectorSize {
        /// The expected sector size.
        expected: u32,
        /// The sector size found in the layer.
        found: u32,
    },
    /// A write-through layer is preceeded by a layer that is not write-through, or
    /// the last layer is write-through.
    #[error("nothing to write through")]
    UselessWriteThrough,
    /// Writing to the layered disk would require this layer to be writable.
    #[error("read only layer in a writable disk")]
    ReadOnly,
}

impl DiskLayer {
    /// Creates a new layer from a backing store.
    pub fn new<T: LayerIo>(backing: T) -> Self {
        let can_read_cache = backing.write_no_overwrite().is_some();
        Self {
            disk_id: backing.disk_id(),
            is_fua_respected: backing.is_fua_respected(),
            sector_size: backing.sector_size(),
            physical_sector_size: backing.physical_sector_size(),
            unmap_behavior: backing.unmap_behavior(),
            optimal_unmap_sectors: backing.optimal_unmap_sectors(),
            read_only: backing.is_read_only(),
            can_read_cache,
            backing: Box::new(backing),
        }
    }

    /// Creates a layer from a disk. The resulting layer is always fully
    /// present.
    pub fn from_disk(disk: Disk) -> Self {
        Self::new(DiskAsLayer(disk))
    }
}

/// An error returned when creating a [`LayeredDisk`].
#[derive(Debug, Error)]
pub enum InvalidLayeredDisk {
    /// No layers were configured.
    #[error("no layers were configured")]
    NoLayers,
    /// An error occurred in a layer.
    #[error("invalid layer {0}")]
    Layer(usize, #[source] InvalidLayer),
}

/// A configuration for a layer in a [`LayeredDisk`].
pub struct LayerConfiguration {
    /// The backing store for the layer.
    pub layer: DiskLayer,
    /// Writes are written both to this layer and the next one.
    pub write_through: bool,
    /// Reads that miss this layer are written back to this layer.
    pub read_cache: bool,
}

impl LayeredDisk {
    /// Creates a new layered disk from a list of layers.
    ///
    /// The layers must be ordered from top to bottom, with the top layer being
    /// the first in the list.
    pub fn new(
        read_only: bool,
        mut layers: Vec<LayerConfiguration>,
    ) -> Result<Self, InvalidLayeredDisk> {
        if layers.is_empty() {
            return Err(InvalidLayeredDisk::NoLayers);
        }

        // Collect the common properties of the layers.
        let mut last_write_through = true;
        let mut is_fua_respected = true;
        let mut optimal_unmap_sectors = 1;
        let mut unmap_must_zero = false;
        let mut disk_id = None;
        let mut unmap_behavior = UnmapBehavior::Zeroes;
        for (i, config) in layers.iter().enumerate() {
            let layer_error = |e| InvalidLayeredDisk::Layer(i, e);
            if config.read_cache && !config.layer.can_read_cache {
                return Err(layer_error(InvalidLayer::ReadCacheNotSupported));
            }
            if (config.read_cache || config.write_through) && config.layer.read_only {
                return Err(layer_error(InvalidLayer::ReadOnlyCache));
            }
            if !config.layer.sector_size.is_power_of_two() {
                return Err(layer_error(InvalidLayer::InvalidSectorSize(
                    config.layer.sector_size,
                )));
            }
            if config.layer.sector_size != layers[0].layer.sector_size {
                // FUTURE: consider supporting different sector sizes, within reason.
                return Err(layer_error(InvalidLayer::MismatchedSectorSize {
                    expected: layers[0].layer.sector_size,
                    found: config.layer.sector_size,
                }));
            }

            if last_write_through {
                if config.layer.read_only && !read_only {
                    return Err(layer_error(InvalidLayer::ReadOnly));
                }
                is_fua_respected &= config.layer.is_fua_respected;
                // Merge the unmap behavior. If any affected layer ignores
                // unmap, then force the whole disk to. If all affected layers
                // zero the sectors, then report that the disk zeroes sectors.
                //
                // If there is at least one write-through layer, then unmap only
                // works if the unmap operation will produce the same result in
                // all the layers that are being written to. Otherwise, the
                // guest could see inconsistent disk contents when the write
                // through layer is removed.
                unmap_must_zero |= config.write_through;
                unmap_behavior = match (unmap_behavior, config.layer.unmap_behavior) {
                    (UnmapBehavior::Zeroes, UnmapBehavior::Zeroes) => UnmapBehavior::Zeroes,
                    _ if unmap_must_zero => UnmapBehavior::Ignored,
                    (UnmapBehavior::Ignored, _) => UnmapBehavior::Ignored,
                    (_, UnmapBehavior::Ignored) => UnmapBehavior::Ignored,
                    _ => UnmapBehavior::Unspecified,
                };
                optimal_unmap_sectors =
                    optimal_unmap_sectors.max(config.layer.optimal_unmap_sectors);
            } else if config.write_through {
                // The write-through layers must all come first.
                return Err(layer_error(InvalidLayer::UselessWriteThrough));
            }
            last_write_through = config.write_through;
            if disk_id.is_none() {
                disk_id = config.layer.disk_id;
            }
        }
        if last_write_through {
            return Err(InvalidLayeredDisk::Layer(
                layers.len() - 1,
                InvalidLayer::UselessWriteThrough,
            ));
        }

        let sector_size = layers[0].layer.sector_size;
        let physical_sector_size = layers[0].layer.physical_sector_size;

        let mut last_sector_count = None;
        let sector_counts_rev = layers
            .iter_mut()
            .rev()
            .map(|config| {
                config.layer.backing.attach(last_sector_count);
                *last_sector_count.insert(config.layer.backing.sector_count())
            })
            .collect::<Vec<_>>();

        let mut visible_sector_count = !0;
        let layers = layers
            .into_iter()
            .zip(sector_counts_rev.into_iter().rev())
            .map(|(config, sector_count)| {
                visible_sector_count = sector_count.min(visible_sector_count);
                Layer {
                    backing: config.layer.backing,
                    visible_sector_count,
                    read_cache: config.read_cache,
                    write_through: config.write_through,
                }
            })
            .collect::<Vec<_>>();

        Ok(Self {
            is_fua_respected,
            read_only,
            sector_shift: sector_size.trailing_zeros(),
            disk_id,
            physical_sector_size,
            unmap_behavior,
            optimal_unmap_sectors,
            layers,
        })
    }
}

trait DynLayer: Send + Sync + Inspect {
    fn attach(&mut self, lower_sector_count: Option<u64>);

    fn sector_count(&self) -> u64;

    fn read<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'_>,
        sector: u64,
        bitmap: SectorMarker<'a>,
    ) -> Pin<Box<dyn 'a + Future<Output = Result<(), DiskError>> + Send>>;

    fn write<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'_>,
        sector: u64,
        fua: bool,
        no_overwrite: bool,
    ) -> Pin<Box<dyn 'a + Future<Output = Result<(), DiskError>> + Send>>;

    fn sync_cache(&self) -> Pin<Box<dyn '_ + Future<Output = Result<(), DiskError>> + Send>>;

    fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
        next_is_zero: bool,
    ) -> Pin<Box<dyn '_ + Future<Output = Result<(), DiskError>> + Send>>;

    fn wait_resize(&self, sector_count: u64) -> Pin<Box<dyn '_ + Future<Output = u64> + Send>>;
}

impl<T: LayerIo> DynLayer for T {
    fn attach(&mut self, lower_sector_count: Option<u64>) {
        self.on_attach(lower_sector_count);
    }

    fn sector_count(&self) -> u64 {
        self.sector_count()
    }

    fn read<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'_>,
        sector: u64,
        bitmap: SectorMarker<'a>,
    ) -> Pin<Box<dyn 'a + Future<Output = Result<(), DiskError>> + Send>> {
        Box::pin(async move { self.read(buffers, sector, bitmap).await })
    }

    fn write<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'_>,
        sector: u64,
        fua: bool,
        no_overwrite: bool,
    ) -> Pin<Box<dyn 'a + Future<Output = Result<(), DiskError>> + Send>> {
        Box::pin(async move {
            if no_overwrite {
                self.write_no_overwrite()
                    .unwrap()
                    .write_no_overwrite(buffers, sector)
                    .await
            } else {
                self.write(buffers, sector, fua).await
            }
        })
    }

    fn sync_cache(&self) -> Pin<Box<dyn '_ + Future<Output = Result<(), DiskError>> + Send>> {
        Box::pin(self.sync_cache())
    }

    fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
        next_is_zero: bool,
    ) -> Pin<Box<dyn '_ + Future<Output = Result<(), DiskError>> + Send>> {
        Box::pin(self.unmap(sector, count, block_level_only, next_is_zero))
    }

    fn wait_resize(&self, sector_count: u64) -> Pin<Box<dyn '_ + Future<Output = u64> + Send>> {
        Box::pin(self.wait_resize(sector_count))
    }
}

/// Metadata and IO for disk layers.
pub trait LayerIo: 'static + Send + Sync + Inspect {
    /// Returns the layer type name as a string.
    ///
    /// This is used for diagnostic purposes.
    fn layer_type(&self) -> &str;

    /// Returns the current sector count.
    ///
    /// For some backing stores, this may change at runtime. If it does, then
    /// the backing store must also implement [`DiskIo::wait_resize`].
    fn sector_count(&self) -> u64;

    /// Returns the logical sector size of the backing store.
    ///
    /// This must not change at runtime.
    fn sector_size(&self) -> u32;

    /// Optionally returns a 16-byte identifier for the disk, if there is a
    /// natural one for this backing store.
    ///
    /// This may be exposed to the guest as a unique disk identifier.
    /// This must not change at runtime.
    fn disk_id(&self) -> Option<[u8; 16]>;

    /// Returns the physical sector size of the backing store.
    ///
    /// This must not change at runtime.
    fn physical_sector_size(&self) -> u32;

    /// Returns true if the `fua` parameter to [`LayerIo::write`] is
    /// respected by the backing store by ensuring that the IO is immediately
    /// committed to disk.
    fn is_fua_respected(&self) -> bool;

    /// Returns true if the layer is read only.
    fn is_read_only(&self) -> bool;

    /// Issues an asynchronous flush operation to the disk.
    fn sync_cache(&self) -> impl Future<Output = Result<(), DiskError>> + Send;

    /// Reads sectors from the layer.
    ///
    /// `marker` is used to specify which sectors have been read. Those that are
    /// not read will be passed to the next layer, or zeroed if there are no
    /// more layers.
    fn read(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        marker: SectorMarker<'_>,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;

    /// Writes sectors to the layer.
    fn write(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;

    /// Unmap sectors from the layer.
    ///
    /// If `next_is_zero` is true, then the next layer's content's are known to
    /// be zero. A layer can use this information to just discard the sectors
    /// rather than putting them in the zero state (which make take more space).
    fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
        next_is_zero: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;

    /// Returns the behavior of the unmap operation.
    fn unmap_behavior(&self) -> UnmapBehavior;

    /// Returns the optimal granularity for unmaps, in sectors.
    fn optimal_unmap_sectors(&self) -> u32 {
        1
    }

    /// Optionally returns a write-no-overwrite implementation.
    fn write_no_overwrite(&self) -> Option<impl WriteNoOverwrite> {
        None::<NoIdet>
    }

    /// Waits for the disk sector size to be different than the specified value.
    fn wait_resize(&self, sector_count: u64) -> impl Future<Output = u64> + Send {
        let _ = sector_count;
        std::future::pending()
    }

    /// Called when the layer is attached to a disk. The sector count of the
    /// next lower layer is provided for the layer to optionally size/resize
    /// itself.
    fn on_attach(&mut self, lower_sector_count: Option<u64>) {
        let _ = lower_sector_count;
    }
}

enum NoIdet {}

/// Writes to the layer without overwriting existing data.
pub trait WriteNoOverwrite: Send + Sync {
    /// Write to the layer without overwriting existing data. Existing sectors
    /// must be preserved.
    ///
    /// This is used to support read caching, where the data being written may
    /// be stale by the time it is written back to the layer.
    fn write_no_overwrite(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;
}

impl<T: WriteNoOverwrite> WriteNoOverwrite for &T {
    fn write_no_overwrite(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> impl Future<Output = Result<(), DiskError>> + Send {
        (*self).write_no_overwrite(buffers, sector)
    }
}

impl WriteNoOverwrite for NoIdet {
    async fn write_no_overwrite(
        &self,
        _buffers: &RequestBuffers<'_>,
        _sector: u64,
    ) -> Result<(), DiskError> {
        unreachable!()
    }
}

impl DiskIo for LayeredDisk {
    fn disk_type(&self) -> &str {
        "layered"
    }

    fn sector_count(&self) -> u64 {
        self.layers[0].backing.sector_count()
    }

    fn sector_size(&self) -> u32 {
        1 << self.sector_shift
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.disk_id
    }

    fn physical_sector_size(&self) -> u32 {
        self.physical_sector_size
    }

    fn is_fua_respected(&self) -> bool {
        self.is_fua_respected
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        let sector_count = buffers.len() >> self.sector_shift;
        let mut bitmap = Bitmap::new(sector, sector_count);
        let mut bits_set = 0;
        // FUTURE: queue the reads to the layers in parallel.
        'done: for (i, layer) in self.layers.iter().enumerate() {
            if bits_set == sector_count {
                break;
            }
            for mut range in bitmap.unset_iter() {
                let end = if i == 0 {
                    // The visible sector count of the first layer is unknown,
                    // since it could change at any time.
                    range.end_sector()
                } else {
                    // Restrict the range to the visible sector count of the
                    // layer; sectors beyond this are logically zero.
                    let end = range.end_sector().min(layer.visible_sector_count);
                    if range.start_sector() == end {
                        break 'done;
                    }
                    end
                };

                let sectors = end - range.start_sector();

                let buffers = buffers.subrange(
                    range.start_sector_within_bitmap() << self.sector_shift,
                    (sectors as usize) << self.sector_shift,
                );

                layer
                    .backing
                    .read(&buffers, range.start_sector(), range.view(sectors))
                    .await?;

                bits_set += range.set_count();

                // TODO: populate read cache(s). Note that we need to detect
                // this will be necessary before performing the read and bounce
                // buffer into a stable buffer in case the bufferes are in guest
                // memory (which could be mutated by the guest or other IOs).
            }
        }
        if bits_set != sector_count {
            for range in bitmap.unset_iter() {
                let len = (range.len() as usize) << self.sector_shift;
                buffers
                    .subrange(range.start_sector_within_bitmap() << self.sector_shift, len)
                    .writer()
                    .zero(len)?;
            }
        }
        Ok(())
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        for layer in &self.layers {
            layer.backing.write(buffers, sector, fua, false).await?;
            if !layer.write_through {
                break;
            }
        }
        Ok(())
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        for layer in &self.layers {
            layer.backing.sync_cache().await?;
            if !layer.write_through {
                break;
            }
        }
        Ok(())
    }

    fn wait_resize(&self, sector_count: u64) -> impl Future<Output = u64> + Send {
        self.layers[0].backing.wait_resize(sector_count)
    }

    async fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        block_level_only: bool,
    ) -> Result<(), DiskError> {
        if self.unmap_behavior == UnmapBehavior::Ignored {
            return Ok(());
        }

        for (layer, next_layer) in self
            .layers
            .iter()
            .zip(self.layers.iter().map(Some).skip(1).chain([None]))
        {
            let next_is_zero = if let Some(next_layer) = next_layer {
                // Sectors beyond the layer's visible sector count are logically
                // zero.
                //
                // FUTURE: consider splitting the unmap operation into multiple
                // operations across this boundary.
                sector_offset >= next_layer.visible_sector_count
            } else {
                true
            };

            layer
                .backing
                .unmap(sector_offset, sector_count, block_level_only, next_is_zero)
                .await?;
            if !layer.write_through {
                break;
            }
        }
        Ok(())
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        self.unmap_behavior
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.optimal_unmap_sectors
    }
}

/// A disk layer wrapping a full disk.
#[derive(Inspect)]
#[inspect(transparent)]
struct DiskAsLayer(Disk);

impl LayerIo for DiskAsLayer {
    fn layer_type(&self) -> &str {
        "disk"
    }

    fn sector_count(&self) -> u64 {
        self.0.sector_count()
    }

    fn sector_size(&self) -> u32 {
        self.0.sector_size()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.0.disk_id()
    }

    fn physical_sector_size(&self) -> u32 {
        self.0.physical_sector_size()
    }

    fn is_fua_respected(&self) -> bool {
        self.0.is_fua_respected()
    }

    fn is_read_only(&self) -> bool {
        self.0.is_read_only()
    }

    fn sync_cache(&self) -> impl Future<Output = Result<(), DiskError>> + Send {
        self.0.sync_cache()
    }

    async fn read(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        mut bitmap: SectorMarker<'_>,
    ) -> Result<(), DiskError> {
        // The disk is fully populated.
        bitmap.set_all();
        self.0.read_vectored(buffers, sector).await
    }

    async fn write(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.0.write_vectored(buffers, sector, fua).await
    }

    fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
        _lower_is_zero: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send {
        self.0.unmap(sector, count, block_level_only)
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        self.0.unmap_behavior()
    }
}
