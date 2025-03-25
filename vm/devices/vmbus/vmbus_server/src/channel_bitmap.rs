// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guestmem::LockedPages;
use parking_lot::RwLock;
use safeatomic::AtomicSliceOps;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
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
        let bitmap = AtomicBitmap::new(self.get_recv_page());
        let channel_table = self.channel_table.read();
        bitmap.scan_and_clear(|event_flag| {
            let event = channel_table.get(event_flag);
            if let Some(Some(event)) = event {
                event.deliver();
            } else {
                tracelimit::warn_ratelimited!(event_flag, "Guest signaled unknown channel");
            }
        });
    }

    /// Sets a channel's bit to signal the guest.
    pub fn set_flag(&self, event_flag: u16) {
        let bitmap = AtomicBitmap::new(self.get_send_page());
        bitmap.set(event_flag as usize);
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

/// Helper class for atomically operating on a large bitmap.
struct AtomicBitmap<'a> {
    bits: &'a [AtomicU64],
}

const BITS_PER_WORD: usize = size_of::<AtomicU64>() * 8;

impl<'a> AtomicBitmap<'a> {
    /// Creates a new bitmap using the specified slice as storage.
    pub fn new(bits: &'a [AtomicU64]) -> Self {
        Self { bits }
    }

    /// Sets the bit at the specified index.
    pub fn set(&self, index: usize) {
        let bit = 1 << (index % BITS_PER_WORD);
        self.bits[index / BITS_PER_WORD].fetch_or(bit, Ordering::SeqCst);
    }

    /// Calls the provided callback for all bits currently set, clearing them in the process.
    pub fn scan_and_clear(&self, mut callback: impl FnMut(usize)) {
        for (word_index, word) in self.bits.iter().enumerate() {
            // Retrieve and clear the current word atomically.
            let mut value = word.swap(0, Ordering::SeqCst);

            // Scan the current word.
            while value != 0 {
                let index = value.trailing_zeros();
                value &= !(1u64 << index);
                let index = word_index * BITS_PER_WORD + (index as usize);
                callback(index);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_atomic_bitmap() {
        let bits: [AtomicU64; 128] = [0; 128].map(AtomicU64::new);
        let bitmap = AtomicBitmap::new(&bits);
        let mut expected_bits = [0u64; 128];

        bitmap.set(5);
        expected_bits[0] = 1u64 << 5;
        compare_bits(&bits, &expected_bits);

        bitmap.set(500);
        expected_bits[7] = 1u64 << 52;
        compare_bits(&bits, &expected_bits);

        let mut set = vec![];
        bitmap.scan_and_clear(|index| set.push(index));

        assert_eq!(set, vec![5, 500]);
        expected_bits = [0u64; 128];
        compare_bits(&bits, &expected_bits);
    }

    fn compare_bits(bits: &[AtomicU64; 128], expected_bits: &[u64; 128]) {
        bits.iter().zip(expected_bits).for_each(|(left, right)| {
            assert_eq!(left.load(Ordering::Acquire), *right);
        })
    }
}
