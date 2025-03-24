// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bitvec::order::Lsb0;
use bitvec::slice::BitSlice;
use guestmem::LockedPages;
use parking_lot::RwLock;
use safeatomic::AtomicSliceOps;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use vmcore::interrupt::Interrupt;

pub(crate) struct ChannelBitmap {
    interrupt_page: LockedPages,
    channel_table: RwLock<Vec<Option<Interrupt>>>,
}

const INTERRUPT_PAGE_SIZE: usize = 2048;

/// Helper for using the channel bitmap with pre-Win8 versions of vmbus. Keeps track of the
/// interrupt page and a mapping of channels by event flag so they can be signalled when the
/// shared interrupt arrives.
impl ChannelBitmap {
    /// Creates a new `ChannelBitmap`.
    pub fn new(interrupt_page: LockedPages) -> Self {
        Self {
            interrupt_page,
            channel_table: RwLock::new(vec![None; crate::channels::MAX_CHANNELS]),
        }
    }

    /// Registers a channel to be signaled when the bit corresponding to `event_flag` is set in
    /// the receive page.
    pub fn register_channel(&self, event_flag: u16, event: Interrupt) {
        let mut channel_table = self.channel_table.write();
        channel_table[event_flag as usize] = Some(event);
    }

    /// Removes a channel from the list of signalable channels.
    pub fn unregister_channel(&self, event_flag: u16) {
        let mut channel_table = self.channel_table.write();
        channel_table[event_flag as usize] = None;
    }

    /// Handles the shared interrupt by signaling all channels whose bit is set in the receive page.
    /// All bits in the receive page are cleared during this operation.
    pub fn handle_shared_interrupt(&self) {
        let bitmap = BitSlice::<_, Lsb0>::from_slice(self.get_recv_page());
        let channel_table = self.channel_table.read();

        for event_flag in bitmap.iter_ones() {
            bitmap.set_aliased(event_flag, false);
            let event = channel_table.get(event_flag);
            if let Some(Some(event)) = event {
                event.deliver();
            } else {
                tracelimit::warn_ratelimited!(event_flag, "Guest signaled unknown channel");
            }
        }
    }

    /// Sets a channel's bit to signal the guest.
    pub fn set_flag(&self, event_flag: u16) {
        let bitmap = BitSlice::<_, Lsb0>::from_slice(self.get_send_page());
        bitmap.set_aliased(event_flag as usize, true);
    }

    /// Creates an interrupt that sets the specified channel bitmap bit before signalling the guest,
    /// or returns the guest interrupt if the channel bitmap is not in use.
    pub fn create_interrupt(
        channel_bitmap: &Option<Arc<ChannelBitmap>>,
        interrupt: Interrupt,
        event_flag: u16,
    ) -> Interrupt {
        if let Some(channel_bitmap) = channel_bitmap {
            let channel_bitmap = channel_bitmap.clone();
            Interrupt::from_fn(move || {
                channel_bitmap.set_flag(event_flag);
                interrupt.deliver();
            })
        } else {
            interrupt
        }
    }

    /// Gets the host-to-guest half of the interrupt page.
    fn get_send_page(&self) -> &[AtomicU64] {
        self.interrupt_page.pages()[0][..INTERRUPT_PAGE_SIZE]
            .as_atomic_slice()
            .unwrap()
    }

    /// Gets the guest-to-host half of the interrupt page.
    fn get_recv_page(&self) -> &[AtomicU64] {
        self.interrupt_page.pages()[0][INTERRUPT_PAGE_SIZE..]
            .as_atomic_slice()
            .unwrap()
    }
}
