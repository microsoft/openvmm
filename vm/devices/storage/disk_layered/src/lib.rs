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

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod bitmap;
pub mod resolve;
pub mod resolver;

pub use bitmap::SectorMarker;

use bitmap::Bitmap;
use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use guestmem::GuestMemory;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi_buffers::OwnedRequestBuffers;
use scsi_buffers::RequestBuffers;
use std::convert::Infallible;
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
    backing: Box<dyn DynLayerIo>,
    visible_sector_count: u64,
    read_cache: bool,
    write_through: bool,
}

/// A single layer which can be attached to a [`LayeredDisk`].
pub struct DiskLayer(Box<dyn DynLayerAttach>);

impl DiskLayer {
    /// Creates a new layer from a backing store.
    pub fn new<T: LayerAttach>(backing: T) -> Self {
        Self(Box::new(backing))
    }

    /// Creates a layer from a disk. The resulting layer is always fully
    /// present.
    pub fn from_disk(disk: Disk) -> Self {
        Self::new(DiskAsLayer(disk))
    }
}

/// Metadata of a particular layer, collected from various [`LayerIo`] APIs.
#[derive(Clone)]
#[expect(missing_docs)] // self-explanatory names
pub struct DiskLayerMetadata {
    pub disk_id: Option<[u8; 16]>,
    pub sector_size: u32,
    pub sector_count: u64,
    pub physical_sector_size: u32,
    pub unmap_behavior: UnmapBehavior,
    pub optimal_unmap_sectors: u32,
    pub read_only: bool,
    pub can_read_cache: bool,
    pub is_fua_respected: bool,
}

// DEVNOTE: this is a transient object, used solely in LayeredDisk::new.
struct AttachedDiskLayer {
    backing: Box<dyn DynLayerIo>,
    meta: DiskLayerMetadata,
}

/// An error returned when creating a [`DiskLayer`].
#[derive(Debug, Error)]
pub enum InvalidLayer {
    /// Failed to attach the layer
    #[error("failed to attach layer")]
    AttachFailed(#[source] anyhow::Error),
    /// Read caching was requested but is not supported.
    #[error("read caching was requested but is not supported")]
    ReadCacheNotSupported,
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
pub struct LayerConfiguration<L = DiskLayer> {
    /// The backing store for the layer.
    pub layer: L,
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
    pub async fn new(
        read_only: bool,
        layers: Vec<LayerConfiguration>,
    ) -> Result<Self, InvalidLayeredDisk> {
        if layers.is_empty() {
            return Err(InvalidLayeredDisk::NoLayers);
        }

        let mut attached_layers: Vec<LayerConfiguration<AttachedDiskLayer>> = {
            let mut attached_layers = Vec::new();

            // layers are attached to one another from the bottom-up, hence the need
            // to iterate in reverse.
            let mut lower_layer_metadata = None;
            for (
                i,
                LayerConfiguration {
                    layer,
                    write_through,
                    read_cache,
                },
            ) in layers.into_iter().enumerate().rev()
            {
                let layer_error = |e| InvalidLayeredDisk::Layer(i, e);

                let layer = layer
                    .0
                    .attach(lower_layer_metadata.take())
                    .await
                    .map_err(|e| layer_error(InvalidLayer::AttachFailed(e)))?;

                let layer_meta = layer.meta.clone();

                attached_layers.push(LayerConfiguration {
                    layer,
                    write_through,
                    read_cache,
                });

                // perform some layer validation prior to attaching subsequent layers
                if read_cache && !layer_meta.can_read_cache {
                    return Err(layer_error(InvalidLayer::ReadCacheNotSupported));
                }
                if !layer_meta.sector_size.is_power_of_two() {
                    return Err(layer_error(InvalidLayer::InvalidSectorSize(
                        layer_meta.sector_size,
                    )));
                }
                if layer_meta.sector_size != attached_layers[0].layer.meta.sector_size {
                    // FUTURE: consider supporting different sector sizes, within reason.
                    return Err(layer_error(InvalidLayer::MismatchedSectorSize {
                        expected: attached_layers[0].layer.meta.sector_size,
                        found: layer_meta.sector_size,
                    }));
                }

                lower_layer_metadata = Some(layer_meta);
            }

            attached_layers.reverse();
            attached_layers
        };

        // perform top-down validation of the layer-stack, collecting various
        // common properties of the stack along the way.
        let mut last_write_through = true;
        let mut is_fua_respected = true;
        let mut optimal_unmap_sectors = 1;
        let mut unmap_must_zero = false;
        let mut disk_id = None;
        let mut unmap_behavior = UnmapBehavior::Zeroes;
        for (
            i,
            &LayerConfiguration {
                ref layer,
                write_through,
                read_cache: _,
            },
        ) in attached_layers.iter().enumerate()
        {
            let layer_error = |e| InvalidLayeredDisk::Layer(i, e);

            if last_write_through {
                if layer.meta.read_only && !read_only {
                    return Err(layer_error(InvalidLayer::ReadOnly));
                }
                is_fua_respected &= layer.meta.is_fua_respected;
                // Merge the unmap behavior. If any affected layer ignores
                // unmap, then force the whole disk to. If all affected layers
                // zero the sectors, then report that the disk zeroes sectors.
                //
                // If there is at least one write-through layer, then unmap only
                // works if the unmap operation will produce the same result in
                // all the layers that are being written to. Otherwise, the
                // guest could see inconsistent disk contents when the write
                // through layer is removed.
                unmap_must_zero |= write_through;
                unmap_behavior = match (unmap_behavior, layer.meta.unmap_behavior) {
                    (UnmapBehavior::Zeroes, UnmapBehavior::Zeroes) => UnmapBehavior::Zeroes,
                    _ if unmap_must_zero => UnmapBehavior::Ignored,
                    (UnmapBehavior::Ignored, _) => UnmapBehavior::Ignored,
                    (_, UnmapBehavior::Ignored) => UnmapBehavior::Ignored,
                    _ => UnmapBehavior::Unspecified,
                };
                optimal_unmap_sectors = optimal_unmap_sectors.max(layer.meta.optimal_unmap_sectors);
            } else if write_through {
                // The write-through layers must all come first.
                return Err(layer_error(InvalidLayer::UselessWriteThrough));
            }
            last_write_through = write_through;
            if disk_id.is_none() {
                disk_id = layer.meta.disk_id;
            }
        }

        if last_write_through {
            return Err(InvalidLayeredDisk::Layer(
                attached_layers.len() - 1,
                InvalidLayer::UselessWriteThrough,
            ));
        }

        let sector_size = attached_layers[0].layer.meta.sector_size;
        let physical_sector_size = attached_layers[0].layer.meta.physical_sector_size;

        let mut last_sector_count = None;
        let sector_counts_rev = attached_layers
            .iter_mut()
            .rev()
            .map(|config| *last_sector_count.insert(config.layer.backing.sector_count()))
            .collect::<Vec<_>>();

        let mut visible_sector_count = !0;
        let layers = attached_layers
            .into_iter()
            .zip(sector_counts_rev.into_iter().rev())
            .map(|(config, sector_count)| {
                let LayerConfiguration {
                    layer,
                    write_through,
                    read_cache,
                } = config;
                visible_sector_count = sector_count.min(visible_sector_count);
                Layer {
                    backing: layer.backing,
                    visible_sector_count,
                    read_cache,
                    write_through,
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

trait DynLayerIo: Send + Sync + Inspect {
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

impl<T: LayerIo> DynLayerIo for T {
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

trait DynLayerAttach: Send + Sync {
    fn attach(
        self: Box<Self>,
        lower_layer_metadata: Option<DiskLayerMetadata>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<AttachedDiskLayer>> + Send>>;
}

impl<T: LayerAttach> DynLayerAttach for T {
    fn attach(
        self: Box<Self>,
        lower_layer_metadata: Option<DiskLayerMetadata>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<AttachedDiskLayer>> + Send>> {
        Box::pin(async move {
            Ok({
                let backing = (*self)
                    .attach(lower_layer_metadata)
                    .await
                    .map_err(|e| anyhow::anyhow!(e.into()))?;
                let can_read_cache = backing.write_no_overwrite().is_some();
                AttachedDiskLayer {
                    meta: DiskLayerMetadata {
                        sector_count: LayerIo::sector_count(&backing),
                        disk_id: backing.disk_id(),
                        is_fua_respected: backing.is_fua_respected(),
                        sector_size: backing.sector_size(),
                        physical_sector_size: backing.physical_sector_size(),
                        unmap_behavior: backing.unmap_behavior(),
                        optimal_unmap_sectors: backing.optimal_unmap_sectors(),
                        read_only: backing.is_logically_read_only(),
                        can_read_cache,
                    },
                    backing: Box::new(backing),
                }
            })
        })
    }
}

/// Transition a layer from an unattached type-state, into an attached
/// type-state, capable of performing [`LayerIo`].
///
/// Layers which do not require a type-state transition on-attach (e.g: those
/// which are pre-initialized with a fixed set of metadata) can simply implement
/// `LayerIo` directly, and leverage the blanket-impl of `impl<T: LayerIo>
/// LayerAttach for T` which simply returns `Self` during the state transition.
pub trait LayerAttach: 'static + Send + Sync {
    /// Error returned if on attach failure.
    type Error: Into<Box<dyn std::error::Error + Send + Sync + 'static>>;
    /// Object implementating [`LayerIo`] after being attached.
    type Layer: LayerIo;

    /// Invoked when the layer is being attached to a layer stack.
    ///
    /// If the layer is being attached on-top of an existing layer,
    /// `lower_layer_metadata` can be used to initialize and/or reconfigure the
    /// layer using the properties of the layer is is being stacked on-top of.
    fn attach(
        self,
        lower_layer_metadata: Option<DiskLayerMetadata>,
    ) -> impl Future<Output = Result<Self::Layer, Self::Error>> + Send;
}

impl<T: LayerIo> LayerAttach for T {
    type Error = Infallible;
    type Layer = Self;
    async fn attach(
        self,
        _lower_layer_metadata: Option<DiskLayerMetadata>,
    ) -> Result<Self, Infallible> {
        Ok(self)
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

    /// Returns true if the layer is logically read only.
    ///
    /// If this returns true, the layer might still be writable via
    /// `write_no_overwrite`, used to populate the layer as a read cache.
    fn is_logically_read_only(&self) -> bool;

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
    ///
    /// # Panics
    ///
    /// The caller must pass a buffer with an integer number of sectors.
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
        let mut bounce_buffers = None::<(OwnedRequestBuffers, GuestMemory)>;
        let sector_count = buffers.len() >> self.sector_shift;
        let mut bitmap = Bitmap::new(sector, sector_count);
        let mut bits_set = 0;
        let mut populate_cache = Vec::new();
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

                let this_buffers = if let Some((bounce_buffers, mem)) = &bounce_buffers {
                    &bounce_buffers.buffer(mem)
                } else {
                    buffers
                };
                let this_buffers = this_buffers.subrange(
                    range.start_sector_within_bitmap() << self.sector_shift,
                    (sectors as usize) << self.sector_shift,
                );

                layer
                    .backing
                    .read(&this_buffers, range.start_sector(), range.view(sectors))
                    .await?;

                bits_set += range.set_count();

                if range.set_count() as u64 != range.len() && layer.read_cache {
                    // Allocate bounce buffers to read into to ensure that we get a stable
                    // copy of the data to populate the cache.
                    bounce_buffers.get_or_insert_with(|| {
                        let mem = GuestMemory::allocate(buffers.len());
                        let owned_buf = OwnedRequestBuffers::linear(0, buffers.len(), true);
                        (owned_buf, mem)
                    });

                    populate_cache.extend(range.unset_iter().map(|range| (layer, range)));
                }
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
        if !populate_cache.is_empty() {
            let (bounce_buffers, mem) = bounce_buffers.unwrap();
            let bounce_buffers = bounce_buffers.buffer(&mem);
            for &(layer, ref range) in &populate_cache {
                assert!(layer.read_cache);
                let offset = ((range.start - sector) as usize) << self.sector_shift;
                let len = ((range.end - range.start) as usize) << self.sector_shift;
                if let Err(err) = layer
                    .backing
                    .write(
                        &bounce_buffers.subrange(offset, len),
                        range.start,
                        false,
                        true,
                    )
                    .await
                {
                    tracelimit::warn_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        sector = range.start,
                        count = range.end - range.start,
                        "failed to populate read cache",
                    );
                }
            }
            let mut mem = mem.into_inner_buf().ok().unwrap();
            for (_, range) in populate_cache {
                // Write this bounced range back to the original buffer. This
                // might be redundant in the presence of multiple cache layers,
                // but this is the simplest implementation.
                let offset = ((range.start - sector) as usize) << self.sector_shift;
                let len = ((range.end - range.start) as usize) << self.sector_shift;
                buffers
                    .subrange(offset, len)
                    .writer()
                    .write(&mem.as_bytes()[offset..][..len])?;
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

    fn is_logically_read_only(&self) -> bool {
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

#[cfg(test)]
mod tests {
    use crate::DiskLayer;
    use crate::LayerConfiguration;
    use crate::LayerIo;
    use crate::LayeredDisk;
    use crate::SectorMarker;
    use crate::WriteNoOverwrite;
    use disk_backend::DiskIo;
    use disk_backend::UnmapBehavior;
    use guestmem::GuestMemory;
    use guestmem::MemoryRead as _;
    use guestmem::MemoryWrite;
    use inspect::Inspect;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use scsi_buffers::OwnedRequestBuffers;
    use std::collections::btree_map::Entry;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[derive(Inspect)]
    #[inspect(skip)]
    struct TestLayer {
        sectors: Mutex<BTreeMap<u64, Data>>,
        sector_count: u64,
    }

    impl TestLayer {
        fn new(sector_count: u64) -> Self {
            Self {
                sectors: Mutex::new(BTreeMap::new()),
                sector_count,
            }
        }
    }

    struct Data(Box<[u8]>);

    impl LayerIo for Arc<TestLayer> {
        fn layer_type(&self) -> &str {
            "test"
        }

        fn sector_count(&self) -> u64 {
            self.sector_count
        }

        fn sector_size(&self) -> u32 {
            512
        }

        fn disk_id(&self) -> Option<[u8; 16]> {
            None
        }

        fn physical_sector_size(&self) -> u32 {
            512
        }

        fn is_fua_respected(&self) -> bool {
            false
        }

        fn is_logically_read_only(&self) -> bool {
            false
        }

        async fn sync_cache(&self) -> Result<(), disk_backend::DiskError> {
            Ok(())
        }

        async fn read(
            &self,
            buffers: &scsi_buffers::RequestBuffers<'_>,
            sector: u64,
            mut marker: SectorMarker<'_>,
        ) -> Result<(), disk_backend::DiskError> {
            let sector_count = buffers.len() / self.sector_size() as usize;
            let sectors = self.sectors.lock();
            for i in sector..sector + sector_count as u64 {
                let Some(data) = sectors.get(&i) else {
                    continue;
                };
                let offset = ((i - sector) * self.sector_size() as u64) as usize;
                buffers
                    .subrange(offset, self.sector_size() as usize)
                    .writer()
                    .write(&data.0)?;
                marker.set(i);
            }
            Ok(())
        }

        async fn write(
            &self,
            buffers: &scsi_buffers::RequestBuffers<'_>,
            sector: u64,
            _fua: bool,
        ) -> Result<(), disk_backend::DiskError> {
            let sector_count = buffers.len() / self.sector_size() as usize;
            let mut sectors = self.sectors.lock();
            for i in sector..sector + sector_count as u64 {
                let offset = ((i - sector) * self.sector_size() as u64) as usize;
                let mut data = Data(vec![0; self.sector_size() as usize].into());
                buffers
                    .subrange(offset, self.sector_size() as usize)
                    .reader()
                    .read(&mut data.0)?;
                sectors.insert(i, data);
            }
            Ok(())
        }

        async fn unmap(
            &self,
            sector: u64,
            count: u64,
            _block_level_only: bool,
            next_is_zero: bool,
        ) -> Result<(), disk_backend::DiskError> {
            if !next_is_zero {
                return Ok(());
            }
            let mut sectors = self.sectors.lock();
            let mut next_sector = sector;
            let end = sector + count;
            while next_sector < end {
                let Some((&sector, _)) = sectors.range_mut(next_sector..).next() else {
                    break;
                };
                if sector >= end {
                    break;
                }
                sectors.remove(&sector);
                next_sector = sector + 1;
            }
            Ok(())
        }

        fn unmap_behavior(&self) -> UnmapBehavior {
            UnmapBehavior::Unspecified
        }

        fn write_no_overwrite(&self) -> Option<impl WriteNoOverwrite> {
            Some(self)
        }
    }

    impl WriteNoOverwrite for Arc<TestLayer> {
        async fn write_no_overwrite(
            &self,
            buffers: &scsi_buffers::RequestBuffers<'_>,
            sector: u64,
        ) -> Result<(), disk_backend::DiskError> {
            let sector_count = buffers.len() / self.sector_size() as usize;
            let mut sectors = self.sectors.lock();
            for i in sector..sector + sector_count as u64 {
                let Entry::Vacant(entry) = sectors.entry(i) else {
                    continue;
                };
                let offset = ((i - sector) * self.sector_size() as u64) as usize;
                let mut data = Data(vec![0; self.sector_size() as usize].into());
                buffers
                    .subrange(offset, self.sector_size() as usize)
                    .reader()
                    .read(&mut data.0)?;
                entry.insert(data);
            }
            Ok(())
        }
    }

    #[async_test]
    async fn test_read_cache() {
        const SIZE: u64 = 2048;
        let bottom = Arc::new(TestLayer::new(SIZE));
        let pattern = |i: u64| {
            let mut acc = (i + 1) * 3;
            Data(
                (0..512)
                    .map(|_| {
                        acc = acc.wrapping_mul(7);
                        acc as u8
                    })
                    .collect::<Vec<_>>()
                    .into(),
            )
        };
        bottom
            .sectors
            .lock()
            .extend((0..SIZE).map(|i| (i, pattern(i))));

        let cache = Arc::new(TestLayer::new(SIZE));
        let cache_cfg = LayerConfiguration {
            layer: DiskLayer::new(cache.clone()),
            read_cache: true,
            write_through: false,
        };
        let bottom_cfg = LayerConfiguration {
            layer: DiskLayer::new(bottom),
            read_cache: false,
            write_through: false,
        };
        let disk = LayeredDisk::new(false, vec![cache_cfg, bottom_cfg])
            .await
            .unwrap();

        let mut mem = GuestMemory::allocate(0x10000);
        let buffers = OwnedRequestBuffers::linear(0, 0x10000, true);

        for i in [0, 2, 4, 6, 8, 0, 2, 4, 6, 8] {
            disk.read_vectored(&buffers.buffer(&mem).subrange(0, 512), i)
                .await
                .unwrap();

            assert_eq!(mem.inner_buf_mut().unwrap()[..512], pattern(i).0[..]);
        }

        assert_eq!(cache.sectors.lock().len(), 5);

        mem.inner_buf_mut().unwrap().fill(0);

        disk.read_vectored(&buffers.buffer(&mem).subrange(0, 15 * 512), 1)
            .await
            .unwrap();

        assert_eq!(cache.sectors.lock().len(), 16);

        for i in 0..15 {
            assert_eq!(
                mem.inner_buf_mut().unwrap()[i as usize * 512..][..512],
                pattern(i + 1).0[..],
                "{i}"
            );
        }
    }
}
