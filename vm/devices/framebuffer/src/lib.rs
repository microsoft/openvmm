// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Framebuffer device.
//!
//! Provides interfaces for reading, mapping, and managing the format of the framebuffer.
//!
//! The function [`framebuffer`] is used to create two structs:
//! [`Framebuffer`], which is used to map an area of memory for the guest to
//! write to, and [`FramebufferAccess`], which is transformed into a [`View`]
//! to allow the VNC server to read from that memory.
//! In OpenVMM, the Framebuffer is used to create a [`FramebufferDevice`]
//! and a [`FramebufferLocalControl`] which share state using an inner mutex.
//! The latter implements [`FramebufferControl`] which provides the necessary
//! interfaces for a video device to control the framebuffer.
//! In Underhill, the format sender is extracted from the framebuffer and used
//! to create a different struct that implements the same trait.
//!
//! This is separate from the synthetic device because its lifetime is separate
//! from that of the synthetic video VMBus channel.

#![forbid(unsafe_code)]

use anyhow::Context;
use chipset_device::ChipsetDevice;
use guestmem::GuestMemory;
use guestmem::MappableGuestMemory;
use guestmem::MemoryMapper;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use mesh::MeshPayload;
use mesh::payload::Protobuf;
use parking_lot::Mutex;
use sparse_mmap::Mappable;
use sparse_mmap::SparseMapping;
use std::convert::Infallible;
use std::io;
use std::sync::Arc;
use video_core::FramebufferControl;
use video_core::FramebufferFormat;
use video_core::ResolvedFramebuffer;
use video_core::SharedFramebufferHandle;
use vm_resource::ResolveResource;
use vm_resource::kind::FramebufferHandleKind;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateRoot;

fn default_framebuffer_format() -> FramebufferFormat {
    FramebufferFormat {
        width: 1024,
        height: 768,
        bytes_per_line: 1024 * 4,
        offset: 0,
    }
}

/// The framebuffer size. In the future, this will be variable.
// TODO: Make framebuffer size variable. See DetermineSynthVideoVramSize() in OS repo
pub const FRAMEBUFFER_SIZE: usize = 8 * 1024 * 1024; // 8 MB

/// Creates a framebuffer and an object that can be used to read from it.
/// The framebuffer should be allocated prior to calling this function.
///
/// * `vram`:   [`Mappable`] (`OwnedFd`/`OwnedHandle`) that can be mapped into guest memory
///   and a `SparseMapping` for the VNC server to read from. In OpenVMM, this
///   is created by `alloc_shared_memory`. In Underhill, this is `/dev/mshv_vtl_low`.
///
/// * `len`:    The amount of memory that was allocated for the framebuffer.
///
/// * `offset`: The `file_offset` that should be used when reading the framebuffer.
///   In OpenVMM, this should be 0. In Underhill, this is the GPA of the
///   VTL2 framebuffer mapping.
pub fn framebuffer(
    vram: Mappable,
    len: usize,
    offset: u64,
) -> io::Result<(Framebuffer, FramebufferAccess)> {
    assert_eq!(
        len, FRAMEBUFFER_SIZE,
        "no framebuffer size flexibility for now"
    );

    let (send, recv) = mesh::channel();

    let fb = Framebuffer {
        vram: vram.try_clone()?,
        len,
        format_send: send,
    };
    let access = FramebufferAccess {
        vram,
        len,
        format_recv: recv,
        offset,
    };
    Ok((fb, access))
}

/// The video framebuffer to be provided to the device.
#[derive(Debug, MeshPayload)]
pub struct Framebuffer {
    vram: Mappable,
    len: usize,
    format_send: mesh::Sender<FramebufferFormat>,
}

impl Framebuffer {
    /// Get the size of the framebuffer
    pub fn len(&self) -> usize {
        self.len
    }

    /// Extract format sender, consuming the framebuffer
    pub fn format_send(self) -> mesh::Sender<FramebufferFormat> {
        self.format_send
    }
}

/// An accessor for the framebuffer. Can be sent cross-process via mesh.
#[derive(Debug, MeshPayload)]
pub struct FramebufferAccess {
    vram: Mappable,
    len: usize,
    format_recv: mesh::Receiver<FramebufferFormat>,
    offset: u64,
}

impl FramebufferAccess {
    /// Maps the framebuffer view.
    pub fn view(self) -> io::Result<View> {
        let mapping = SparseMapping::new(self.len)?;
        mapping.map_file(0, self.len, &self.vram, self.offset, false)?;
        Ok(View {
            mapping,
            format_recv: self.format_recv,
            format: None,
            vram: self.vram,
            len: self.len,
            offset: self.offset,
        })
    }
}

/// A mapped view of the framebuffer.
#[derive(Debug)]
pub struct View {
    mapping: SparseMapping,
    format_recv: mesh::Receiver<FramebufferFormat>,
    format: Option<FramebufferFormat>,
    vram: Mappable,
    len: usize,
    offset: u64,
}

impl View {
    /// Reads a line within the framebuffer.
    pub fn read_line(&mut self, line: u16, data: &mut [u8]) {
        if let Some(format) = &self.format {
            if let Some(offset) = (line as usize)
                .checked_mul(format.bytes_per_line)
                .and_then(|x| x.checked_add(format.offset))
            {
                let len = std::cmp::min(data.len(), format.width * 4);
                let _ = self.mapping.read_at(offset, &mut data[..len]);
                return;
            }
        }
        data.fill(0);
    }

    /// Returns the current resolution.
    pub fn resolution(&mut self) -> (u16, u16) {
        // Get any framebuffer updates.
        //
        // FUTURE-use a channel/port type that throws away all but the last
        // message to avoid possible high memory use.
        while let Ok(format) = self.format_recv.try_recv() {
            self.format = Some(format);
        }
        if let Some(format) = &self.format {
            (format.width as u16, format.height as u16)
        } else {
            (1, 1)
        }
    }

    /// Gets the framebuffer access back.
    pub fn access(self) -> FramebufferAccess {
        // Put the current format at the head of the channel.
        let (send, recv) = mesh::channel();
        if let Some(format) = self.format {
            send.send(format);
        }
        send.bridge(self.format_recv);
        FramebufferAccess {
            vram: self.vram,
            len: self.len,
            format_recv: recv,
            offset: self.offset,
        }
    }
}

/// A chipset device for the framebuffer.
#[derive(InspectMut)]
pub struct FramebufferDevice {
    #[inspect(flatten)]
    inner: Arc<Mutex<FramebufferInner>>,
    #[inspect(hex)]
    len: usize,
}

/// Used to control a framebuffer running in the same process
#[derive(Clone)]
pub struct FramebufferLocalControl {
    inner: Arc<Mutex<FramebufferInner>>,
    len: usize,
}

#[derive(Inspect)]
struct FramebufferInner {
    #[inspect(skip)]
    _mem_fixed: Option<Box<dyn MappableGuestMemory>>,
    #[inspect(skip)]
    framebuffer: Option<Framebuffer>,
    mapping_state: Option<MappingState>,
    format: FramebufferFormat,
    #[inspect(skip)]
    mapper: Box<dyn MemoryMapper>,
}

#[derive(Inspect)]
struct MappingState {
    gpa: u64,
    subrange: MemoryRange,
    #[inspect(skip)]
    mem: Box<dyn MappableGuestMemory>,
}

/// Saved state.
#[derive(Debug, Clone, Protobuf, SavedStateRoot)]
#[mesh(package = "framebuffer")]
pub struct SavedState {
    #[mesh(1)]
    mapping: Option<SavedMappingState>,
    #[mesh(2)]
    format: FramebufferFormat,
}

#[derive(Debug, Clone, Protobuf)]
#[mesh(package = "framebuffer")]
struct SavedMappingState {
    #[mesh(1)]
    gpa: u64,
    #[mesh(2)]
    subrange: MemoryRange,
}

impl FramebufferDevice {
    /// Creates a new framebuffer device from the specified framebuffer
    /// using the given mapper. Optionally creates a second mapping that does
    /// not move once the VM is started. This can be used fo VTL2 to read from.
    pub fn new(
        mapper: Box<dyn MemoryMapper>,
        framebuffer: Framebuffer,
        framebuffer_gpa_base_fixed: Option<u64>,
    ) -> anyhow::Result<Self> {
        let len = framebuffer.len;

        // If the framebuffer is going to be used by VTL2, make a secondary mapping
        // for the VNC server to read that doesn't move once the vm is started.
        // This allows VTL2 to avoid remapping its framebuffer view if VTL0 moves it.
        let mem_fixed = if let Some(gpa) = framebuffer_gpa_base_fixed {
            let (mut mem, region) = mapper
                .new_region(len, "framebuffer-vtl2".to_owned())
                .context("failed to create vtl2 framebuffer memory region")?;

            region
                .map(0, &framebuffer.vram, 0, len, true)
                .context("failed to map vtl2 framebuffer memory region")?;

            mem.map_to_guest(gpa, true)?;
            Some(mem)
        } else {
            None
        };

        // Send the initial framebuffer format.
        let format = default_framebuffer_format();
        framebuffer.format_send.send(format);

        Ok(Self {
            inner: Arc::new(Mutex::new(FramebufferInner {
                _mem_fixed: mem_fixed,
                mapping_state: None,
                format,
                framebuffer: Some(framebuffer),
                mapper,
            })),
            len,
        })
    }

    /// Gets the inner framebuffer back.
    pub fn into_framebuffer(self) -> Framebuffer {
        self.inner.lock().framebuffer.take().unwrap()
    }

    /// Gets the control plane for the framebuffer.
    pub fn control(&self) -> FramebufferLocalControl {
        FramebufferLocalControl {
            inner: self.inner.clone(),
            len: self.len,
        }
    }
}

impl ChangeDeviceState for FramebufferDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        let mut inner = self.inner.lock();
        inner.mapping_state = Default::default();
        if let Some(mut state) = inner.mapping_state.take() {
            state.mem.unmap_from_guest();
        }

        // TODO: clear VRAM
    }
}

impl ChipsetDevice for FramebufferDevice {}

impl SaveRestore for FramebufferDevice {
    type SavedState = SavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        let inner = self.inner.lock();
        let mapping = inner.mapping_state.as_ref().map(
            |MappingState {
                 gpa,
                 subrange,
                 mem: _,
             }| SavedMappingState {
                gpa: *gpa,
                subrange: *subrange,
            },
        );
        Ok(SavedState {
            format: inner.format,
            mapping,
        })
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        let SavedState { mapping, format } = state;
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        inner.format = format;
        if let Some(mapping) = mapping {
            inner
                .map(mapping.gpa, Some(mapping.subrange))
                .context("failed to map VRAM to guest")
                .map_err(RestoreError::Other)?;
        }

        inner
            .framebuffer
            .as_mut()
            .unwrap()
            .format_send
            .send(inner.format);
        Ok(())
    }
}

impl FramebufferInner {
    fn map(&mut self, gpa: u64, framebuffer_range: Option<MemoryRange>) -> anyhow::Result<()> {
        if let Some(mut state) = self.mapping_state.take() {
            state.mem.unmap_from_guest();
        }

        let Some(framebuffer) = &self.framebuffer else {
            return Ok(());
        };

        let framebuffer_range =
            framebuffer_range.unwrap_or_else(|| MemoryRange::new(0..framebuffer.len as u64));

        let (mut mem, region) = self
            .mapper
            .new_region(framebuffer_range.len() as usize, "framebuffer".to_owned())
            .context("failed to create framebuffer region")?;

        region
            .map(
                0,
                &framebuffer.vram,
                framebuffer_range.start(),
                framebuffer_range.len() as usize,
                true,
            )
            .context("failed to map framebuffer memory")?;

        mem.map_to_guest(gpa, true)
            .context("failed to map VRAM to guest")?;
        self.mapping_state = Some(MappingState {
            gpa,
            subrange: framebuffer_range,
            mem,
        });

        tracing::debug!("Mapped VRAM to guest at address {:#x}", gpa);

        Ok(())
    }
}

impl FramebufferLocalControl {
    /// Maps the framebuffer to the guest at the specified GPA.
    ///
    /// `framebuffer_range` is an optional subrange of the framebuffer to map.
    pub fn map(&mut self, gpa: u64, framebuffer_range: Option<MemoryRange>) {
        if let Err(err) = self.inner.lock().map(gpa, framebuffer_range) {
            tracing::error!(
                gpa,
                error = err.as_ref() as &dyn std::error::Error,
                "failed to map framebuffer to guest"
            );
        }
    }

    /// Unmaps the framebuffer from the guest.
    pub fn unmap(&mut self) {
        let mut inner = self.inner.lock();
        if let Some(mut state) = inner.mapping_state.take() {
            state.mem.unmap_from_guest();
        }
    }

    /// Returns the size of the framebuffer in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Updates the framebuffer format.
    pub fn set_format(&mut self, format: FramebufferFormat) {
        let mut inner = self.inner.lock();
        let inner = &mut *inner;

        if inner.format != format {
            inner.format = format;
            if let Some(framebuffer) = &mut inner.framebuffer {
                framebuffer.format_send.send(inner.format);
            }
        }
    }

    /// Gets a `GuestMemory` object that can be used to access the framebuffer
    /// memory.
    pub fn memory(&self) -> io::Result<GuestMemory> {
        let inner = self.inner.lock();
        let framebuffer = inner
            .framebuffer
            .as_ref()
            .expect("framebuffer is still active");
        let mapping = SparseMapping::new(framebuffer.len())?;
        mapping.map_file(0, framebuffer.len(), &framebuffer.vram, 0, true)?;
        Ok(GuestMemory::new("framebuffer", mapping))
    }
}

// On the host the mapping is done immediately, but we still use the async trait
// so the video device doesn't have to be aware of the underlying implementation.
#[async_trait::async_trait]
impl FramebufferControl for FramebufferLocalControl {
    async fn map(&mut self, gpa: u64) {
        self.map(gpa, None);
    }
    async fn unmap(&mut self) {
        self.unmap();
    }
    async fn set_format(&mut self, format: FramebufferFormat) {
        self.set_format(format);
    }
}

impl ResolveResource<FramebufferHandleKind, SharedFramebufferHandle> for FramebufferLocalControl {
    type Output = ResolvedFramebuffer;
    type Error = Infallible;

    fn resolve(
        &self,
        _resource: SharedFramebufferHandle,
        _input: (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(self.clone().into())
    }
}
