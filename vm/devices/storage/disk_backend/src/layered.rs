#![allow(missing_docs)]

mod bitmap;

pub use bitmap::SectorMarker;

use crate::Disk;
use crate::DiskError;
use crate::DiskIo;
use crate::Unmap;
use bitmap::Bitmap;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;

/// A disk composed of multiple layers.
#[derive(Inspect)]
pub struct LayeredDisk {
    #[inspect(iter_by_index)]
    layers: Vec<Layer>,
    is_read_only: bool,
    is_fua_respected: bool,
    sector_shift: u32,
    disk_id: Option<[u8; 16]>,
    physical_sector_size: u32,
    optimal_unmap_sectors: Option<u32>,
}

#[derive(Inspect)]
struct Layer {
    backing: Box<dyn DynLayer>,
    visible_sector_count: u64,
    behavior: LayerBehavior,
}

/// The caching behavior of the layer.
#[derive(Clone, Debug, Inspect, Default)]
pub struct LayerBehavior {
    /// Writes are written both to this layer and the next one.
    pub write_through: bool,
    /// Reads that miss this layer are written back to this layer.
    pub read_cache: bool,
}

pub struct DiskLayer {
    backing: Box<dyn DynLayer>,
    behavior: LayerBehavior,
    disk_id: Option<[u8; 16]>,
    is_fua_respected: bool,
    is_read_only: bool,
    sector_size: u32,
    physical_sector_size: u32,
    sector_count: u64,
    unmap_behavior: UnmapBehavior,
    optimal_unmap_sectors: u32,
}

impl DiskLayer {
    pub fn new<T: LayerIo>(backing: T, behavior: LayerBehavior) -> Result<Self, Infallible> {
        if behavior.read_cache && backing.write_no_overwrite().is_none() {
            todo!()
        }
        if (behavior.read_cache || behavior.write_through) && backing.is_read_only() {
            todo!()
        }
        Ok(Self {
            disk_id: backing.disk_id(),
            is_fua_respected: backing.is_fua_respected(),
            sector_size: backing.sector_size(),
            physical_sector_size: backing.physical_sector_size(),
            sector_count: backing.sector_count(),
            unmap_behavior: backing.unmap_behavior(),
            optimal_unmap_sectors: backing.optimal_unmap_sectors(),
            is_read_only: backing.is_read_only(),
            behavior,
            backing: Box::new(backing),
        })
    }

    /// Creates a layer from a disk. The resulting layer is always fully
    /// present.
    pub fn from_disk(disk: Disk) -> Result<Self, Infallible> {
        Self::new(
            DiskAsLayer(disk),
            LayerBehavior {
                write_through: false,
                read_cache: false,
            },
        )
    }
}

impl LayeredDisk {
    pub fn new(layers: Vec<DiskLayer>) -> Result<Self, Infallible> {
        // Collect the common properties of the layers.
        let mut last_write_through = true;
        let mut is_read_only = false;
        let mut is_fua_respected = true;
        let mut optimal_unmap_sectors = Some(1);
        let mut unmap_must_zero = false;
        let mut disk_id = None;
        for layer in &layers {
            if layer.sector_size != layers[0].sector_size {
                todo!();
            }

            if layer.behavior.write_through {
                // If using write-through, then unmap only works if the unmap
                // operation will produce the same result in all the layers that
                // are being written to. Otherwise, the guest could see
                // inconsistent disk contents when the write through layer is
                // removed.
                unmap_must_zero = true;
                // The write-through layers must all come first.
                if !last_write_through {
                    todo!();
                }
            }
            if last_write_through {
                is_read_only = layer.is_read_only;
                is_fua_respected &= layer.is_fua_respected;
                let unmap = match layer.unmap_behavior {
                    UnmapBehavior::Zeroes => true,
                    UnmapBehavior::Unspecified => !unmap_must_zero,
                    UnmapBehavior::Ignored => false,
                };
                if !unmap {
                    optimal_unmap_sectors = None;
                } else if let Some(n) = &mut optimal_unmap_sectors {
                    *n = (*n).max(layer.optimal_unmap_sectors);
                }
            }
            last_write_through = layer.behavior.write_through;
            if disk_id.is_none() {
                disk_id = layer.disk_id;
            }
        }
        if last_write_through {
            todo!();
        }

        let sector_size = layers[0].sector_size;
        if !sector_size.is_power_of_two() {
            todo!();
        }

        let physical_sector_size = layers[0].physical_sector_size;

        let mut visible_sector_count = !0;
        let layers = layers
            .into_iter()
            .map(|layer| {
                visible_sector_count = layer.sector_count.min(visible_sector_count);
                Layer {
                    behavior: layer.behavior,
                    backing: layer.backing,
                    visible_sector_count,
                }
            })
            .collect::<Vec<_>>();

        Ok(Self {
            is_fua_respected,
            is_read_only,
            sector_shift: sector_size.trailing_zeros(),
            disk_id,
            physical_sector_size,
            optimal_unmap_sectors,
            layers,
        })
    }
}

trait DynLayer: Send + Sync + Inspect {
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
        lower_is_zero: bool,
    ) -> Pin<Box<dyn '_ + Future<Output = Result<(), DiskError>> + Send>>;

    fn wait_resize(&self, sector_count: u64) -> Pin<Box<dyn '_ + Future<Output = u64> + Send>>;
}

impl<T: LayerIo> DynLayer for T {
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
        lower_is_zero: bool,
    ) -> Pin<Box<dyn '_ + Future<Output = Result<(), DiskError>> + Send>> {
        Box::pin(self.unmap(sector, count, block_level_only, lower_is_zero))
    }

    fn wait_resize(&self, sector_count: u64) -> Pin<Box<dyn '_ + Future<Output = u64> + Send>> {
        Box::pin(self.wait_resize(sector_count))
    }
}

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

    /// Returns true if the `fua` parameter to [`LayerIo::write_vectored`] is
    /// respected by the backing store by ensuring that the IO is immediately
    /// committed to disk.
    fn is_fua_respected(&self) -> bool;

    /// Returns true if the disk is read only.
    fn is_read_only(&self) -> bool;

    /// Issues an asynchronous flush operation to the disk.
    fn sync_cache(&self) -> impl Future<Output = Result<(), DiskError>> + Send;

    fn read(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        bitmap: SectorMarker<'_>,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;

    fn write(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;

    fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
        lower_is_zero: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;

    fn unmap_behavior(&self) -> UnmapBehavior;

    fn optimal_unmap_sectors(&self) -> u32 {
        1
    }

    fn write_no_overwrite(&self) -> Option<impl WriteNoOverwrite> {
        None::<NoIdet>
    }

    /// Waits for the disk sector size to be different than the specified value.
    fn wait_resize(&self, sector_count: u64) -> impl Future<Output = u64> + Send {
        let _ = sector_count;
        std::future::pending()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnmapBehavior {
    Ignored,
    Unspecified,
    Zeroes,
}

enum NoIdet {}

pub trait WriteNoOverwrite: Send + Sync {
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

pub trait UnmapLayer: Send + Sync {
    fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
        lower_is_zero: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;
}

impl<T: UnmapLayer> UnmapLayer for &T {
    fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
        lower_is_zero: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send {
        (*self).unmap(sector, count, block_level_only, lower_is_zero)
    }
}

impl UnmapLayer for NoIdet {
    async fn unmap(
        &self,
        _sector: u64,
        _count: u64,
        _block_level_only: bool,
        _lower_is_zero: bool,
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
        false
    }

    fn is_read_only(&self) -> bool {
        self.is_read_only
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        let sector_count = buffers.len() >> self.sector_shift;
        let mut bitmap = Bitmap::new(sector, sector_count);
        let mut bits_set = 0;
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
            layer.backing.write(&buffers, sector, fua, false).await?;
            if !layer.behavior.write_through {
                break;
            }
        }
        Ok(())
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        for layer in &self.layers {
            layer.backing.sync_cache().await?;
            if !layer.behavior.write_through {
                break;
            }
        }
        Ok(())
    }

    fn unmap(&self) -> Option<impl Unmap> {
        self.optimal_unmap_sectors.map(|_| self)
    }

    fn wait_resize(&self, sector_count: u64) -> impl Future<Output = u64> + Send {
        self.layers[0].backing.wait_resize(sector_count)
    }
}

impl Unmap for LayeredDisk {
    async fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        block_level_only: bool,
    ) -> Result<(), DiskError> {
        for (layer, next_layer) in self
            .layers
            .iter()
            .zip(self.layers.iter().map(Some).skip(1).chain([None]))
        {
            let lower_is_zero = if let Some(next_layer) = next_layer {
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
                .unmap(sector_offset, sector_count, block_level_only, lower_is_zero)
                .await?;
            if !layer.behavior.write_through {
                break;
            }
        }
        Ok(())
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.optimal_unmap_sectors.unwrap()
    }
}

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

    fn read(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        mut bitmap: SectorMarker<'_>,
    ) -> impl Future<Output = Result<(), DiskError>> + Send {
        async move {
            bitmap.set_all();
            self.0.read_vectored(buffers, sector).await
        }
    }

    fn write(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send {
        async move { self.0.write_vectored(buffers, sector, fua).await }
    }

    fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
        _lower_is_zero: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send {
        async move {
            if let Some(unmap) = self.0.unmap() {
                unmap.unmap(sector, count, block_level_only).await?;
            }
            Ok(())
        }
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        if self.0.unmap().is_some() {
            UnmapBehavior::Unspecified
        } else {
            UnmapBehavior::Ignored
        }
    }
}
