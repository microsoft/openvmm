// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of Generation ID services (shared across both PCAT and UEFI)

#![forbid(unsafe_code)]

pub mod device;
pub mod resolver;

use guestmem::GuestMemory;
use inspect::InspectMut;
use mesh::RecvError;
use std::task::Context;
use std::task::Poll;
use vmcore::line_interrupt::LineInterrupt;

/// Various runtime objects used by the GenerationId device.
#[expect(missing_docs)] // self-explanatory fields
pub struct GenerationIdRuntimeDeps {
    pub gm: GuestMemory,
    pub generation_id_recv: mesh::Receiver<[u8; 16]>,
    pub notify_interrupt: LineInterrupt,
}

/// Device providing initial and dynamic Generation ID update capabilities.
#[derive(InspectMut)]
#[inspect(extra = "GenerationId::inspect_extra")]
pub struct GenerationId {
    // Runtime glue
    #[inspect(skip)]
    rt: GenerationIdRuntimeDeps,

    // Static configuration
    /// Address, within `rt.gm`, of a buffer provided by the platform instead
    /// of reported by firmware (e.g. Linux direct boot).
    #[inspect(hex)]
    fixed_address: Option<u64>,

    // Volatile state
    id: [u8; 16],
    #[inspect(with = "ptr_to_opt_u64")]
    ptr: [Option<u32>; 2],
}

impl GenerationId {
    fn inspect_extra(&mut self, resp: &mut inspect::Response<'_>) {
        resp.field_mut_with("force_update", |update| {
            let v = if let Some(update) = update {
                let update = update.parse()?;
                if update {
                    let mut id = [0; 16];
                    getrandom::fill(&mut id).unwrap();
                    self.set_and_update_generation_id(id);
                    tracing::info!("Force updated genid...");
                }
                update
            } else {
                false
            };
            Result::<_, std::str::ParseBoolError>::Ok(v.to_string())
        });
    }
}

impl GenerationId {
    /// Construct a new GenerationId device.
    pub fn new(initial_generation_id: [u8; 16], platform: GenerationIdRuntimeDeps) -> Self {
        Self {
            fixed_address: None,
            id: initial_generation_id,
            ptr: [None, None],
            rt: platform,
        }
    }

    /// Use a platform-provided buffer at `address` within
    /// `GenerationIdRuntimeDeps::gm` instead of waiting for the firmware to
    /// report one. The current ID is written immediately.
    pub fn set_fixed_address(&mut self, address: u64) {
        self.fixed_address = Some(address);
        self.ptr = gpa_to_ptr(address);
        self.update_generation_id();
    }

    /// Reset the GenerationId state back to what it was when first constructed.
    pub fn reset(&mut self) {
        // Just reset the pointer, not the ID. Since this is not a "time travel"
        // event, there is no need to change the ID.
        self.ptr = self.initial_ptr();
    }

    /// Writes the current generation ID to guest memory, if the buffer
    /// location is known.
    pub fn write_to_guest(&self) {
        self.update_generation_id();
    }

    /// Update the low bits of the generation id pointer.
    pub fn write_generation_id_low(&mut self, data: u32) {
        self.set_ptr(0, data)
    }

    /// Update the high bits of the generation id pointer.
    pub fn write_generation_id_high(&mut self, data: u32) {
        self.set_ptr(1, data)
    }

    /// Polls for new generation IDs.
    pub fn poll(&mut self, cx: &mut Context<'_>) {
        while let Poll::Ready(val) = self.rt.generation_id_recv.poll_recv(cx) {
            match val {
                Ok(val) => {
                    tracing::info!("new VM generation ID from host");
                    self.set_and_update_generation_id(val)
                }
                Err(RecvError::Closed) => break,
                Err(err) => {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "Error receiving generation ID"
                    );
                    break;
                }
            }
        }
    }

    fn initial_ptr(&self) -> [Option<u32>; 2] {
        match self.fixed_address {
            Some(address) => gpa_to_ptr(address),
            None => [None; 2],
        }
    }

    /// Attempts to write the current generation ID into guest memory at
    /// the specified location by the BIOS (PCAT/UEFI).
    fn update_generation_id(&self) {
        if let Some(ptr) = ptr_to_opt_u64(&self.ptr) {
            if let Err(e) = self.rt.gm.write_at(ptr, &self.id) {
                tracelimit::error_ratelimited!(
                    error = &e as &dyn std::error::Error,
                    "failed to write generation ID"
                )
            }
        }
    }

    /// Sets and updates generation id
    fn set_and_update_generation_id(&mut self, val: [u8; 16]) {
        self.id = val;
        self.update_generation_id();

        // Pulse the interrupt line.
        //
        // Ideally there would be some notification from the firmware to clear
        // the interrupt line, but the current firmware DSDT implementations do
        // not do this.
        self.rt.notify_interrupt.set_level(true);
        self.rt.notify_interrupt.set_level(false);
    }

    fn set_ptr(&mut self, index: usize, data: u32) {
        self.ptr[index] = Some(data);
        self.update_generation_id();
    }
}

/// Convert the lo/hi pair of genid prt registers to a single u64, returning
/// `None` if either ho/lo register has yet to be set.
fn ptr_to_opt_u64(ptr: &[Option<u32>; 2]) -> Option<u64> {
    Some(((ptr[1]? as u64) << 32) | ptr[0]? as u64)
}

fn gpa_to_ptr(gpa: u64) -> [Option<u32>; 2] {
    [Some(gpa as u32), Some((gpa >> 32) as u32)]
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;

        #[derive(Protobuf)]
        #[mesh(package = "firmware.generation_id")]
        pub struct SavedState {
            #[mesh(1)]
            pub id: [u8; 16],
            #[mesh(2)]
            pub ptr: [Option<u32>; 2],
        }
    }

    impl SaveRestore for GenerationId {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let Self {
                rt: _,
                fixed_address: _,
                id,
                ptr,
            } = *self;

            let saved_state = state::SavedState { id, ptr };
            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { id, ptr } = state;

            self.id = id;
            self.ptr = ptr;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Waker;
    use vmcore::line_interrupt::LineSetTarget;

    const GPA: u64 = 0x1000;

    #[derive(Default)]
    struct EdgeCounter(AtomicUsize);

    impl EdgeCounter {
        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl LineSetTarget for EdgeCounter {
        fn set_irq(&self, _vector: u32, high: bool) {
            if high {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    fn new_device(gm: &GuestMemory) -> (GenerationId, Arc<EdgeCounter>, mesh::Sender<[u8; 16]>) {
        let edges = Arc::new(EdgeCounter::default());
        let (send, recv) = mesh::channel();
        let mut dev = GenerationId::new(
            [1; 16],
            GenerationIdRuntimeDeps {
                gm: gm.clone(),
                generation_id_recv: recv,
                notify_interrupt: LineInterrupt::new_with_target("genid", edges.clone(), 0),
            },
        );
        dev.set_fixed_address(GPA);
        (dev, edges, send)
    }

    fn guest_id(gm: &GuestMemory) -> [u8; 16] {
        let mut id = [0; 16];
        gm.read_at(GPA, &mut id).unwrap();
        id
    }

    fn poll(dev: &mut GenerationId) {
        dev.poll(&mut Context::from_waker(Waker::noop()));
    }

    #[test]
    fn fixed_address_publishes_id_without_firmware() {
        let gm = GuestMemory::allocate(0x10000);
        let (mut dev, edges, _send) = new_device(&gm);
        assert_eq!(guest_id(&gm), [1; 16]);
        poll(&mut dev);
        assert_eq!(edges.count(), 0);

        // The fixed buffer survives reset.
        dev.reset();
        gm.write_at(GPA, &[0; 16]).unwrap();
        dev.write_to_guest();
        assert_eq!(guest_id(&gm), [1; 16]);
    }

    #[test]
    fn update_queued_before_start_applies_on_first_poll() {
        let gm = GuestMemory::allocate(0x10000);
        let (mut dev, edges, send) = new_device(&gm);
        send.send([3; 16]);
        poll(&mut dev);
        assert_eq!(guest_id(&gm), [3; 16]);
        assert_eq!(edges.count(), 1);
        poll(&mut dev);
        assert_eq!(edges.count(), 1);
    }
}
