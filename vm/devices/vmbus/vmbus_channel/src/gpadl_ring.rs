// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GPADL-backed ring buffers.

use crate::ChannelClosed;
use crate::RawAsyncChannel;
use crate::SignalVmbusChannel;
use crate::bus::OpenData;
use crate::bus::OpenRequest;
use crate::channel::DeviceResources;
use crate::gpadl::GpadlMapView;
use crate::gpadl::GpadlView;
use crate::gpadl::UnknownGpadlId;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use guestmem::LockedPages;
use pal_async::driver::Driver;
use ring::IncomingRing;
use ring::OutgoingRing;
use std::fmt::Debug;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU32;
use vmbus_ring as ring;
use vmcore::interrupt::Interrupt;
use vmcore::notify::PolledNotify;

/// A GPADL view that has exactly one page-aligned range.
#[derive(Clone)]
pub struct AlignedGpadlView {
    gpadl: GpadlView,
    offset: u32,
    len: u32,
}

impl AlignedGpadlView {
    /// Validates that `gpadl` is aligned and wraps it.
    pub fn new(gpadl: GpadlView) -> Result<Self, GpadlView> {
        if gpadl.range_count() != 1 {
            return Err(gpadl);
        }
        let range = gpadl.first().unwrap();
        if !range.len().is_multiple_of(ring::PAGE_SIZE) || range.offset() != 0 {
            return Err(gpadl);
        }
        let count = range.gpns().len() as u32;
        Ok(AlignedGpadlView {
            gpadl,
            offset: 0,
            len: count,
        })
    }

    /// Splits the range into two aligned ranges at the page number `offset`.
    pub fn split(
        self,
        offset: u32,
    ) -> Result<(AlignedGpadlView, AlignedGpadlView), AlignedGpadlView> {
        if offset == 0 || self.len <= offset {
            return Err(self);
        }
        let left = AlignedGpadlView {
            gpadl: self.gpadl.clone(),
            offset: 0,
            len: offset,
        };
        let right = AlignedGpadlView {
            gpadl: self.gpadl,
            offset,
            len: self.len - offset,
        };
        Ok((left, right))
    }

    /// Returns the GPN array for this range.
    pub fn gpns(&self) -> &[u64] {
        &self.gpadl.first().unwrap().gpns()
            [self.offset as usize..self.offset as usize + self.len as usize]
    }
}

#[derive(Clone)]
struct GpadlPagedMemory {
    _gpadl: AlignedGpadlView,
    pages: LockedPages,
}

impl GpadlPagedMemory {
    fn new(gpadl: AlignedGpadlView, mem: &GuestMemory) -> Result<Self, GuestMemoryError> {
        // Store the data gpns twice in a row to make lookup easier.
        let gpns: Vec<u64> = gpadl
            .gpns()
            .iter()
            .chain(gpadl.gpns().iter().skip(1))
            .copied()
            .collect();
        let pages = mem.lock_gpns(false, &gpns)?;
        Ok(Self {
            _gpadl: gpadl,
            pages,
        })
    }
}

impl ring::PagedMemory for GpadlPagedMemory {
    fn control(&self) -> &[AtomicU8; ring::PAGE_SIZE] {
        self.pages.pages()[0]
    }

    #[inline]
    fn data(&self, page: usize) -> &[AtomicU8; ring::PAGE_SIZE] {
        self.pages.pages()[page + 1]
    }

    fn data_page_count(&self) -> usize {
        (self.pages.pages().len() - 1) / 2
    }
}

impl Debug for GpadlPagedMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpadlPagedMemory").finish()
    }
}

/// An implementation of [`vmbus_ring::RingMem`] backed by an aligned GPADL
/// view.
#[derive(Debug, Clone)]
pub struct GpadlRingMem {
    ring: ring::PagedRingMem<GpadlPagedMemory>,
}

impl GpadlRingMem {
    /// Creates a new ring memory backed by `gpadl` and `mem`.
    pub fn new(gpadl: AlignedGpadlView, mem: &GuestMemory) -> Result<Self, GuestMemoryError> {
        Ok(Self {
            ring: ring::PagedRingMem::new(GpadlPagedMemory::new(gpadl, mem)?),
        })
    }
}

impl ring::RingMem for GpadlRingMem {
    #[inline]
    fn len(&self) -> usize {
        self.ring.len()
    }

    #[inline]
    fn read_at(&self, addr: usize, data: &mut [u8]) {
        self.ring.read_at(addr, data)
    }

    #[inline]
    fn write_at(&self, addr: usize, data: &[u8]) {
        self.ring.write_at(addr, data)
    }

    #[inline]
    fn read_aligned(&self, addr: usize, data: &mut [u8]) {
        self.ring.read_aligned(addr, data)
    }

    #[inline]
    fn write_aligned(&self, addr: usize, data: &[u8]) {
        self.ring.write_aligned(addr, data)
    }

    #[inline]
    fn control(&self) -> &[AtomicU32; vmbus_ring::CONTROL_WORD_COUNT] {
        self.ring.control()
    }
}

/// A ring buffer error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// invalid ring buffer gpadl
    #[error("invalid ring buffer gpadl")]
    InvalidRingGpadl,
    /// gpadl ID is invalid
    #[error(transparent)]
    UnknownGpadlId(#[from] UnknownGpadlId),
    /// memory error accessing the ring
    #[error(transparent)]
    Memory(#[from] GuestMemoryError),
    /// ring buffer error
    #[error(transparent)]
    Ring(#[from] ring::Error),
    /// driver error
    #[error("io driver error")]
    Driver(#[source] std::io::Error),
}

/// Creates a set of incoming and outgoing rings for a channel.
pub fn make_rings(
    mem: &GuestMemory,
    gpadl_map: &GpadlMapView,
    open_data: &OpenData,
) -> Result<(IncomingRing<GpadlRingMem>, OutgoingRing<GpadlRingMem>), Error> {
    let gpadl = AlignedGpadlView::new(gpadl_map.map(open_data.ring_gpadl_id)?)
        .map_err(|_| Error::InvalidRingGpadl)?;
    let (in_gpadl, out_gpadl) = gpadl
        .split(open_data.ring_offset)
        .map_err(|_| Error::InvalidRingGpadl)?;
    Ok((
        IncomingRing::new(GpadlRingMem::new(in_gpadl, mem)?)?,
        OutgoingRing::new(GpadlRingMem::new(out_gpadl, mem)?)?,
    ))
}

/// Creates a raw channel from input parameters passed to [`crate::channel::VmbusDevice::open`].
pub fn gpadl_channel(
    driver: &(impl Driver + ?Sized),
    resources: &DeviceResources,
    open_request: &OpenRequest,
    channel_idx: u16,
) -> Result<RawAsyncChannel<GpadlRingMem>, Error> {
    let (in_ring, out_ring) = make_rings(
        resources.offer_resources.ring_memory(open_request),
        &resources.gpadl_map,
        &open_request.open_data,
    )?;

    let event = Box::new(GpadlChannelSignal {
        event: resources.channels[channel_idx as usize]
            .event
            .clone()
            .pollable(driver)
            .map_err(Error::Driver)?,

        interrupt: open_request.interrupt.clone(),
    });

    Ok(RawAsyncChannel {
        in_ring,
        out_ring,
        signal: event,
    })
}

struct GpadlChannelSignal {
    event: PolledNotify,
    interrupt: Interrupt,
}

impl SignalVmbusChannel for GpadlChannelSignal {
    fn signal_remote(&self) {
        self.interrupt.deliver();
    }

    fn poll_for_signal(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), ChannelClosed>> {
        // Use the event directly for incoming signals, without ever returning
        // [`ChannelClosed`]. It is expected that the caller will handle the
        // channel closed case explicitly, in
        // [`crate::channel::VmbusDevice::close`], rather than relying on getting a
        // failure when reading or writing a packet in a ring buffer.
        self.event.poll_wait(cx).map(Ok)
    }
}
