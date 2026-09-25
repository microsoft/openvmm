// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interrupt-status delivery of the virtio-mmio transport.
//!
//! In [`VirtioMmioInterruptMode::Legacy`] mode the guest reads
//! `INTERRUPT_STATUS` and acknowledges it through `INTERRUPT_ACK`. On Linux,
//! used-buffer acknowledgements may also arrive through an accelerated
//! doorbell on `INTERRUPT_ACK`; a used-buffer generation count keeps such a
//! deferred acknowledgement from clearing a completion that the guest has not
//! read yet. In [`VirtioMmioInterruptMode::SharedStatus`] mode the status lives
//! in a guest-memory word, and each zero-to-nonzero transition pulses the
//! interrupt line.

use super::InterruptState;
use super::MmioTransport;
use super::VirtioMmioDevice;
use crate::DynVirtioDevice;
use crate::spec::VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER;
#[cfg(target_os = "linux")]
use crate::spec::mmio::VirtioMmioRegister;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
#[cfg(target_os = "linux")]
use pal_async::driver::PollImpl;
#[cfg(target_os = "linux")]
use pal_async::fd::PollFdReady;
#[cfg(target_os = "linux")]
use pal_async::interest::InterestSlot;
#[cfg(target_os = "linux")]
use pal_async::interest::PollEvents;
#[cfg(target_os = "linux")]
use pal_async::task::Task;
#[cfg(target_os = "linux")]
use pal_event::Event;
use parking_lot::Mutex;
#[cfg(target_os = "linux")]
use std::future::poll_fn;
#[cfg(target_os = "linux")]
use std::os::fd::AsFd;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::sync::Arc;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::save_restore::RestoreError;

/// The driver bound of the MMIO transport constructors. On Linux the driver
/// must also wait for the accelerated `INTERRUPT_ACK` doorbell.
#[cfg(target_os = "linux")]
pub(super) use pal_async::driver::SpawnDriver as MmioDriver;
#[cfg(not(target_os = "linux"))]
pub(super) use pal_async::task::Spawn as MmioDriver;

/// Interrupt-status delivery used by a virtio-mmio transport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VirtioMmioInterruptMode {
    /// Virtio MMIO status reads, acknowledgement writes, and a level interrupt.
    #[default]
    Legacy,
    /// A shared atomic status word and a pulse on each zero-to-nonzero transition.
    SharedStatus { status_gpa: u64 },
}

impl VirtioMmioDevice {
    /// Creates an MMIO transport after masking guest-visible device features.
    pub fn new_with_disabled_features(
        device: Box<dyn DynVirtioDevice>,
        driver: &impl MmioDriver,
        guest_memory: GuestMemory,
        interrupt: LineInterrupt,
        doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
        mmio_gpa: u64,
        mmio_len: u64,
        disabled_features: u64,
    ) -> std::io::Result<Self> {
        Self::new_with_disabled_features_and_interrupt_mode(
            device,
            driver,
            guest_memory,
            interrupt,
            doorbell_registration,
            mmio_gpa,
            mmio_len,
            disabled_features,
            VirtioMmioInterruptMode::Legacy,
        )
    }
}

/// Interrupt-status state in addition to the legacy status word.
#[derive(Inspect)]
pub(super) struct StatusDelivery {
    used_buffer_generation: u64,
    observed_used_buffer_generation: Option<u64>,
    shared_status: Option<SharedInterruptStatus>,
}

impl StatusDelivery {
    /// Applies a status update in shared-status mode, or tracks used-buffer
    /// generations in legacy mode. Returns `true` if the update is complete.
    pub(super) fn update(&mut self, interrupt: &LineInterrupt, is_set: bool, bits: u32) -> bool {
        if let Some(shared_status) = &self.shared_status {
            if is_set {
                let old_status = shared_status
                    .fetch_or(bits)
                    .expect("validated shared interrupt-status memory became inaccessible");
                if old_status == 0 {
                    interrupt.set_level(true);
                    interrupt.set_level(false);
                }
            }
            return true;
        }

        if bits & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER != 0 {
            if is_set {
                self.used_buffer_generation = self.used_buffer_generation.wrapping_add(1);
            } else {
                self.observed_used_buffer_generation = None;
            }
        }
        false
    }
}

#[derive(Inspect)]
struct SharedInterruptStatus {
    #[inspect(skip)]
    guest_memory: GuestMemory,
    #[inspect(hex)]
    gpa: u64,
}

impl SharedInterruptStatus {
    fn update(&self, operation: impl Fn(u32) -> u32) -> Result<u32, GuestMemoryError> {
        let mut current = self.guest_memory.read_plain::<u32>(self.gpa)?;
        loop {
            let new = operation(current);
            match self.guest_memory.compare_exchange(self.gpa, current, new)? {
                Ok(_) => return Ok(current),
                Err(actual) => current = actual,
            }
        }
    }

    fn load(&self) -> Result<u32, GuestMemoryError> {
        self.update(|current| current)
    }

    fn store(&self, value: u32) -> Result<u32, GuestMemoryError> {
        self.update(|_| value)
    }

    fn fetch_or(&self, bits: u32) -> Result<u32, GuestMemoryError> {
        self.update(|current| current | bits)
    }
}

/// Validates the interrupt mode of a new transport and returns its
/// interrupt-status state and its `INTERRUPT_ACK` doorbell setup.
pub(super) fn setup(
    device: &dyn DynVirtioDevice,
    guest_memory: &GuestMemory,
    interrupt_mode: VirtioMmioInterruptMode,
) -> std::io::Result<(StatusDelivery, AckDoorbellSetup)> {
    #[cfg(target_os = "linux")]
    let supports_accelerated_doorbells = device.supports_accelerated_doorbells();
    #[cfg(not(target_os = "linux"))]
    let _ = device;
    let shared_status = match interrupt_mode {
        VirtioMmioInterruptMode::Legacy => None,
        VirtioMmioInterruptMode::SharedStatus { status_gpa } => {
            if !status_gpa.is_multiple_of(size_of::<u32>() as u64) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("shared interrupt-status GPA {status_gpa:#x} is not naturally aligned"),
                ));
            }
            let shared_status = SharedInterruptStatus {
                guest_memory: guest_memory.clone(),
                gpa: status_gpa,
            };
            shared_status.store(0).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("shared interrupt-status GPA {status_gpa:#x} is inaccessible: {error}"),
                )
            })?;
            Some(shared_status)
        }
    };
    Ok((
        StatusDelivery {
            used_buffer_generation: 0,
            observed_used_buffer_generation: None,
            shared_status,
        },
        AckDoorbellSetup {
            #[cfg(target_os = "linux")]
            enabled: interrupt_mode == VirtioMmioInterruptMode::Legacy
                && supports_accelerated_doorbells,
        },
    ))
}

/// Whether a new transport accelerates used-buffer acknowledgements.
pub(super) struct AckDoorbellSetup {
    #[cfg(target_os = "linux")]
    enabled: bool,
}

impl AckDoorbellSetup {
    /// Registers the `INTERRUPT_ACK` doorbell, if enabled and available.
    #[cfg(target_os = "linux")]
    pub(super) fn register(
        self,
        driver: &impl MmioDriver,
        doorbell_registration: &Option<Arc<dyn DoorbellRegistration>>,
        mmio_gpa: u64,
        interrupt_state: &Arc<Mutex<InterruptState>>,
    ) -> InterruptAck {
        let (interrupt_ack_event, interrupt_ack_task, interrupt_ack_doorbell) = if self.enabled
            && let Some(registration) = doorbell_registration
        {
            let event = Event::new();
            match registration.register_doorbell(
                mmio_gpa + VirtioMmioRegister::INTERRUPT_ACK.0 as u64,
                Some(VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER.into()),
                Some(4),
                &event,
            ) {
                Ok(doorbell) => match driver.new_dyn_fd_ready(event.as_fd().as_raw_fd()) {
                    Ok(ready) => {
                        let interrupt_ack_event = event.clone();
                        let task = driver.spawn(
                            "virtio-mmio-interrupt-ack",
                            InterruptAckWait {
                                ready,
                                event,
                                interrupt_state: interrupt_state.clone(),
                            }
                            .run(),
                        );
                        (Some(interrupt_ack_event), Some(task), Some(doorbell))
                    }
                    Err(_) => (None, None, None),
                },
                Err(_) => (None, None, None),
            }
        } else {
            (None, None, None)
        };
        InterruptAck {
            _task: interrupt_ack_task,
            _doorbell: interrupt_ack_doorbell,
            event: interrupt_ack_event,
        }
    }

    /// Registers the `INTERRUPT_ACK` doorbell, if enabled and available.
    #[cfg(not(target_os = "linux"))]
    pub(super) fn register(
        self,
        _driver: &impl MmioDriver,
        _doorbell_registration: &Option<Arc<dyn DoorbellRegistration>>,
        _mmio_gpa: u64,
        _interrupt_state: &Arc<Mutex<InterruptState>>,
    ) -> InterruptAck {
        InterruptAck {}
    }
}

/// The accelerated `INTERRUPT_ACK` doorbell of a transport, if registered.
#[derive(Default)]
pub(super) struct InterruptAck {
    #[cfg(target_os = "linux")]
    _task: Option<Task<()>>,
    #[cfg(target_os = "linux")]
    _doorbell: Option<Box<dyn Send + Sync>>,
    #[cfg(target_os = "linux")]
    event: Option<Event>,
}

impl InterruptAck {
    /// Applies a used-buffer acknowledgement that arrived through the doorbell.
    fn acknowledge(&self, state: &mut InterruptState) {
        #[cfg(target_os = "linux")]
        if let Some(event) = &self.event {
            if event.try_wait() {
                state.acknowledge_used_buffer();
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = state;
    }
}

#[cfg(target_os = "linux")]
struct InterruptAckWait {
    ready: PollImpl<dyn PollFdReady>,
    event: Event,
    interrupt_state: Arc<Mutex<InterruptState>>,
}

#[cfg(target_os = "linux")]
impl InterruptAckWait {
    async fn run(mut self) {
        loop {
            poll_fn(|cx| {
                self.ready
                    .poll_fd_ready(cx, InterestSlot::Read, PollEvents::IN)
            })
            .await;
            self.ready.clear_fd_ready(InterestSlot::Read);
            let mut state = self.interrupt_state.lock();
            if self.event.try_wait() {
                state.acknowledge_used_buffer();
            }
        }
    }
}

impl InterruptState {
    fn read_status(&mut self) -> u32 {
        if let Some(shared_status) = &self.delivery.shared_status {
            return shared_status
                .load()
                .expect("validated shared interrupt-status memory became inaccessible");
        }
        if self.status & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER != 0 {
            self.delivery.observed_used_buffer_generation =
                Some(self.delivery.used_buffer_generation);
        }
        self.status
    }

    #[cfg(target_os = "linux")]
    fn acknowledge_used_buffer(&mut self) {
        if self.delivery.shared_status.is_some() {
            return;
        }
        if self.delivery.observed_used_buffer_generation
            == Some(self.delivery.used_buffer_generation)
        {
            self.update(false, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);
        } else {
            self.delivery.observed_used_buffer_generation = None;
        }
    }

    /// Restores the saved interrupt status.
    pub(super) fn restore_status(&mut self, status: u32) -> Result<(), RestoreError> {
        if let Some(shared_status) = &self.delivery.shared_status {
            shared_status
                .store(status)
                .map_err(|error| RestoreError::InvalidSavedState(error.into()))?;
            self.interrupt.set_level(false);
        } else {
            self.status = status;
            self.delivery.used_buffer_generation = 0;
            self.delivery.observed_used_buffer_generation =
                (self.status & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER != 0).then_some(0);
            self.interrupt.set_level(self.status != 0);
        }
        Ok(())
    }
}

impl MmioTransport {
    fn lock_interrupt_state(&self) -> parking_lot::MutexGuard<'_, InterruptState> {
        let mut state = self.interrupt_state.lock();
        self.interrupt_ack.acknowledge(&mut state);
        state
    }

    pub(super) fn read_interrupt_status(&self) -> u32 {
        self.lock_interrupt_state().read_status()
    }

    pub(super) fn reset_interrupt_state(&self) {
        let mut state = self.lock_interrupt_state();
        if let Some(shared_status) = &state.delivery.shared_status {
            shared_status
                .store(0)
                .expect("validated shared interrupt-status memory became inaccessible");
        }
        state.status = 0;
        state.delivery.used_buffer_generation = 0;
        state.delivery.observed_used_buffer_generation = None;
        state.interrupt.set_level(false);
    }
}

impl Drop for MmioTransport {
    fn drop(&mut self) {
        let state = self.interrupt_state.lock();
        if let Some(shared_status) = &state.delivery.shared_status {
            let _ = shared_status.store(0);
        }
        state.interrupt.set_level(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use vmcore::line_interrupt::LineSetTarget;

    #[derive(Default)]
    struct CountingInterruptTarget {
        high: AtomicBool,
        pulses: AtomicUsize,
    }

    impl LineSetTarget for CountingInterruptTarget {
        fn set_irq(&self, _vector: u32, high: bool) {
            self.high.store(high, Ordering::SeqCst);
            if high {
                self.pulses.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    fn shared_interrupt_state(
        guest_memory: &GuestMemory,
        target: Arc<CountingInterruptTarget>,
    ) -> InterruptState {
        InterruptState {
            interrupt: LineInterrupt::new_with_target("shared-status-test", target, 0),
            status: 0,
            delivery: StatusDelivery {
                used_buffer_generation: 0,
                observed_used_buffer_generation: None,
                shared_status: Some(SharedInterruptStatus {
                    guest_memory: guest_memory.clone(),
                    gpa: 0,
                }),
            },
        }
    }

    #[test]
    fn shared_status_coalesces_bits_and_pulses_on_zero_transition() {
        let guest_memory = GuestMemory::allocate(0x1000);
        let target = Arc::new(CountingInterruptTarget::default());
        let mut state = shared_interrupt_state(&guest_memory, target.clone());

        state.update(true, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);
        state.update(true, VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE);
        state.update(true, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);

        assert_eq!(state.read_status(), 3);
        assert_eq!(target.pulses.load(Ordering::SeqCst), 1);
        assert!(!target.high.load(Ordering::SeqCst));

        let consumed = state
            .delivery
            .shared_status
            .as_ref()
            .unwrap()
            .store(0)
            .unwrap();
        assert_eq!(consumed, 3);
        state.update(true, VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE);
        assert_eq!(target.pulses.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn shared_status_completion_racing_exchange_is_not_lost() {
        const ITERATIONS: usize = 1000;

        let guest_memory = GuestMemory::allocate(0x1000);
        let shared_status = Arc::new(SharedInterruptStatus {
            guest_memory,
            gpa: 0,
        });
        let barrier = Arc::new(Barrier::new(2));
        let publisher = {
            let shared_status = shared_status.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                for _ in 0..ITERATIONS {
                    barrier.wait();
                    shared_status
                        .fetch_or(VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER)
                        .unwrap();
                    barrier.wait();
                }
            })
        };

        for _ in 0..ITERATIONS {
            shared_status.store(0).unwrap();
            barrier.wait();
            let consumed = shared_status.store(0).unwrap();
            barrier.wait();
            let pending = shared_status.load().unwrap();
            assert_eq!(
                (consumed | pending) & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER,
                VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER
            );
        }
        publisher.join().unwrap();
    }

    #[test]
    fn shared_status_reset_clears_pending_state_without_interrupt() {
        let guest_memory = GuestMemory::allocate(0x1000);
        let target = Arc::new(CountingInterruptTarget::default());
        let state = MmioTransport {
            fixed_mmio_region: ("test", 0..=0xfff),
            device_id: 0,
            vendor_id: 0,
            interrupt_state: Arc::new(Mutex::new(shared_interrupt_state(
                &guest_memory,
                target.clone(),
            ))),
            interrupt_ack: InterruptAck::default(),
        };
        state
            .interrupt_state
            .lock()
            .update(true, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);
        let pulses = target.pulses.load(Ordering::SeqCst);

        state.reset_interrupt_state();

        assert_eq!(state.read_interrupt_status(), 0);
        assert_eq!(target.pulses.load(Ordering::SeqCst), pulses);
        assert!(!target.high.load(Ordering::SeqCst));
    }

    #[test]
    fn shared_status_teardown_clears_pending_state() {
        let guest_memory = GuestMemory::allocate(0x1000);
        let target = Arc::new(CountingInterruptTarget::default());
        let transport = MmioTransport {
            fixed_mmio_region: ("test", 0..=0xfff),
            device_id: 0,
            vendor_id: 0,
            interrupt_state: Arc::new(Mutex::new(shared_interrupt_state(
                &guest_memory,
                target.clone(),
            ))),
            interrupt_ack: InterruptAck::default(),
        };
        transport
            .interrupt_state
            .lock()
            .update(true, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);

        drop(transport);

        assert_eq!(guest_memory.read_plain::<u32>(0).unwrap(), 0);
        assert!(!target.high.load(Ordering::SeqCst));
    }
}
