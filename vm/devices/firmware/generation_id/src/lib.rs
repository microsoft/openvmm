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
    /// Guest physical address of a host-allocated generation ID buffer. When
    /// set, the guest firmware is not expected to report the buffer address
    /// (e.g. Linux direct boot, where there is no firmware).
    #[inspect(hex)]
    fixed_gpa: Option<u64>,
    /// Whether restoring saved state starts a new VM generation (new ID plus
    /// a guest notification), as required when a snapshot may be restored
    /// more than once (clones).
    new_generation_on_restore: bool,

    // Volatile state
    #[inspect(with = "id_to_hex")]
    id: [u8; 16],
    #[inspect(with = "ptr_to_opt_u64")]
    ptr: [Option<u32>; 2],
    /// A guest notification is owed for an ID change made while the device
    /// was not running (during restore). It is raised on the next poll, which
    /// happens when the device starts, after all device state is restored.
    notify_pending: bool,
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
            fixed_gpa: None,
            new_generation_on_restore: false,
            id: initial_generation_id,
            ptr: [None, None],
            notify_pending: false,
            rt: platform,
        }
    }

    /// Use a host-allocated generation ID buffer at `gpa` instead of waiting
    /// for the guest firmware to report one. The current ID is written
    /// immediately.
    pub fn set_fixed_gpa(&mut self, gpa: u64) {
        self.fixed_gpa = Some(gpa);
        self.ptr = gpa_to_ptr(gpa);
        self.update_generation_id();
    }

    /// Configure whether restoring saved state starts a new VM generation.
    ///
    /// When enabled, [`SaveRestore::restore`] discards the saved ID, installs
    /// a fresh random one, and notifies the guest once the device starts, so
    /// that each VM restored from the same snapshot reseeds its RNG
    /// differently.
    ///
    /// [`SaveRestore::restore`]: vmcore::save_restore::SaveRestore::restore
    pub fn set_new_generation_on_restore(&mut self, enable: bool) {
        self.new_generation_on_restore = enable;
    }

    /// Returns the current generation ID.
    pub fn generation_id(&self) -> [u8; 16] {
        self.id
    }

    /// Reset the GenerationId state back to what it was when first constructed.
    pub fn reset(&mut self) {
        // Just reset the pointer, not the ID. Since this is not a "time travel"
        // event, there is no need to change the ID.
        self.ptr = match self.fixed_gpa {
            Some(gpa) => gpa_to_ptr(gpa),
            None => [None; 2],
        };
        self.notify_pending = false;
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
        if std::mem::take(&mut self.notify_pending) {
            tracing::info!("notifying guest of new VM generation ID after restore");
            self.notify_guest();
        }
        while let Poll::Ready(val) = self.rt.generation_id_recv.poll_recv(cx) {
            match val {
                Ok(val) => self.set_and_update_generation_id(val),
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
        self.notify_guest();
    }

    fn notify_guest(&self) {
        // Pulse the interrupt line.
        //
        // Ideally there would be some notification from the firmware to clear
        // the interrupt line, but the current firmware DSDT implementations do
        // not do this.
        self.rt.notify_interrupt.set_level(true);
        self.rt.notify_interrupt.set_level(false);
    }

    /// Replaces the ID with a fresh random one that differs from `previous`.
    fn new_generation(&mut self, previous: [u8; 16]) {
        let mut id = previous;
        while id == previous {
            getrandom::fill(&mut id).expect("rng failure");
        }
        self.id = id;
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

fn id_to_hex(id: &[u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
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
                fixed_gpa: _,
                new_generation_on_restore: _,
                id,
                ptr,
                notify_pending: _,
            } = *self;

            let saved_state = state::SavedState { id, ptr };
            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { id, ptr } = state;

            self.ptr = match self.fixed_gpa {
                Some(gpa) => gpa_to_ptr(gpa),
                None => ptr,
            };

            if self.new_generation_on_restore {
                // Like QEMU's vmgenid post_load: the saved ID belongs to the
                // snapshot, and every restore is a new generation. Publish the
                // new ID now (guest memory is already restored) and defer the
                // notification until the device starts, so it cannot be lost
                // to the restore of other devices' interrupt state.
                self.new_generation(id);
                self.update_generation_id();
                self.notify_pending = true;
                tracing::info!(
                    old_generation_id = id_to_hex(&id),
                    new_generation_id = id_to_hex(&self.id),
                    "restored VM gets a new generation ID"
                );
            } else {
                self.id = id;
            }
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
    use vmcore::save_restore::SaveRestore;

    const GPA: u64 = 0x1000;

    /// Counts rising edges, since the device pulses the line.
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

    fn new_device(
        gm: &GuestMemory,
        id: [u8; 16],
        new_generation_on_restore: bool,
    ) -> (GenerationId, Arc<EdgeCounter>, mesh::Sender<[u8; 16]>) {
        let edges = Arc::new(EdgeCounter::default());
        let (send, recv) = mesh::channel();
        let mut dev = GenerationId::new(
            id,
            GenerationIdRuntimeDeps {
                gm: gm.clone(),
                generation_id_recv: recv,
                notify_interrupt: LineInterrupt::new_with_target("genid", edges.clone(), 0),
            },
        );
        dev.set_fixed_gpa(GPA);
        dev.set_new_generation_on_restore(new_generation_on_restore);
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
    fn fixed_gpa_publishes_id_without_firmware() {
        let gm = GuestMemory::allocate(0x10000);
        let (mut dev, edges, _send) = new_device(&gm, [1; 16], false);
        assert_eq!(guest_id(&gm), [1; 16]);
        poll(&mut dev);
        assert_eq!(edges.count(), 0);

        // The fixed buffer survives reset; no firmware will re-report it.
        dev.reset();
        gm.write_at(GPA, &[0; 16]).unwrap();
        dev.write_to_guest();
        assert_eq!(guest_id(&gm), [1; 16]);
    }

    #[test]
    fn restore_starts_new_generation_and_notifies_on_start() {
        let gm = GuestMemory::allocate(0x10000);
        let (mut template, _, _send) = new_device(&gm, [1; 16], true);

        let mut ids = Vec::new();
        for _ in 0..2 {
            let (mut clone, edges, _send) = new_device(&gm, [2; 16], true);
            clone.restore(template.save().unwrap()).unwrap();
            let id = clone.generation_id();
            assert_ne!(id, [1; 16], "saved (template) ID must not be reused");
            assert_eq!(guest_id(&gm), id, "new ID is published during restore");
            // Not notified until the device starts (first poll), so restoring
            // other devices' interrupt state cannot clobber the notification.
            assert_eq!(edges.count(), 0);
            poll(&mut clone);
            assert_eq!(edges.count(), 1);
            poll(&mut clone);
            assert_eq!(edges.count(), 1, "notify only once");
            ids.push(id);
        }
        assert_ne!(ids[0], ids[1], "each restore is a distinct generation");
    }

    #[test]
    fn restore_without_policy_keeps_saved_id() {
        let gm = GuestMemory::allocate(0x10000);
        let (mut template, _, _send) = new_device(&gm, [1; 16], false);

        let (mut clone, edges, _send) = new_device(&gm, [2; 16], false);
        clone.restore(template.save().unwrap()).unwrap();
        assert_eq!(clone.generation_id(), [1; 16]);
        poll(&mut clone);
        assert_eq!(edges.count(), 0);
    }

    #[test]
    fn host_update_notifies_immediately() {
        let gm = GuestMemory::allocate(0x10000);
        let (mut dev, edges, send) = new_device(&gm, [1; 16], false);
        send.send([3; 16]);
        poll(&mut dev);
        assert_eq!(guest_id(&gm), [3; 16]);
        assert_eq!(edges.count(), 1);
    }
}
