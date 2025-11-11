// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits for working with MSI interrupts.

use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::Weak;
use vmcore::interrupt::Interrupt;

/// Trait implemented by targets capable of receiving MSI interrupts.
pub trait MsiInterruptTarget: Send + Sync {
    /// Creates a new interrupt object.
    fn new_interrupt(&self) -> Box<dyn MsiControl>;
}

/// Trait modelling an individual MSI interrupt.
pub trait MsiControl: Send {
    /// Enables the interrupt, so that signaling the interrupt delivers an MSI
    /// to the specified address/data pair.
    fn enable(&mut self, address: u64, data: u32);

    /// Disables the interrupt, allowing the backing object to release resources
    /// associated with this MSI.
    fn disable(&mut self);

    /// Signals the interrupt.
    ///
    /// The caller must ensure that the interrupt is enabled with the same
    /// address/data pair before calling this method. The address/data is
    /// provided here redundantly for the convenience of the implementation.
    fn signal(&mut self, address: u64, data: u32);

    // FUTURE: add mechanisms to use an OS event object for signal. This is
    // necessary to support sending interrupts across processes.
    //
    // This is complicated by differing requirements for the different backends,
    // e.g. KVM supports taking whatever eventfd you want, while WHP wants to be
    // the one to provide the event object.
}

impl<T: Send + FnMut(u64, u32)> MsiControl for T {
    fn enable(&mut self, _address: u64, _data: u32) {}

    fn disable(&mut self) {}

    fn signal(&mut self, address: u64, data: u32) {
        (*self)(address, data)
    }
}

/// A set of message-signaled interrupts that have yet to be connected to a
/// backing interrupt controller.
pub struct MsiInterruptSet {
    interrupts: Vec<Weak<Mutex<MsiInterruptState>>>,
}

impl MsiInterruptSet {
    /// Creates a new empty set of message signaled interrupts.
    pub fn new() -> Self {
        Self {
            interrupts: Vec::new(),
        }
    }

    /// Returns the number of interrupts in the set.
    pub fn len(&self) -> usize {
        self.interrupts.len()
    }

    /// Connects the interrupts created with `builder()` to the given target
    /// interrupt controller.
    pub fn connect(self, target: &dyn MsiInterruptTarget) {
        for interrupt in self.interrupts.into_iter().filter_map(|i| i.upgrade()) {
            let mut control = target.new_interrupt();
            let mut state = interrupt.lock();
            if let Some((address, data)) = state.address_data {
                control.enable(address, data);
                if state.pending {
                    control.signal(address, data);
                    state.pending = false;
                }
            }
            state.control = Some(control);
        }
    }
}

/// Trait for registering message-signaled interrupts for a device.
pub trait RegisterMsi: Send {
    /// Returns a new message-signaled interrupt for this device.
    fn new_msi(&mut self) -> MsiInterrupt;
}

impl RegisterMsi for MsiInterruptSet {
    fn new_msi(&mut self) -> MsiInterrupt {
        let state = Arc::new(Mutex::new(MsiInterruptState {
            pending: false,
            address_data: None,
            control: None,
        }));
        self.interrupts.push(Arc::downgrade(&state));
        MsiInterrupt { state }
    }
}

/// A message-signaled interrupt.
#[derive(Debug, Inspect)]
pub struct MsiInterrupt {
    state: Arc<Mutex<MsiInterruptState>>,
}

#[derive(Inspect)]
struct MsiInterruptState {
    pending: bool,
    #[inspect(with = "inspect_address_data")]
    address_data: Option<(u64, u32)>,
    #[inspect(skip)]
    control: Option<Box<dyn MsiControl>>,
}

fn inspect_address_data(address_data: &Option<(u64, u32)>) -> String {
    match address_data {
        Some((address, data)) => format!("address: {:#x}, data: {:#x}", address, data),
        None => "None".to_string(),
    }
}

impl std::fmt::Debug for MsiInterruptState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsiInterruptState")
            .field("pending", &self.pending)
            .field("address_data", &self.address_data)
            .field("control", &"<MsiControl>")
            .finish()
    }
}

impl MsiInterrupt {
    /// Enables the interrupt.
    ///
    /// If `set_pending`, or if the internal pending state is set, then delivers
    /// the interrupt immediately.
    pub fn enable(&mut self, address: u64, data: u32, set_pending: bool) {
        let mut state = self.state.lock();
        let state = &mut *state;
        state.pending |= set_pending;
        state.address_data = Some((address, data));
        if let Some(control) = &mut state.control {
            control.enable(address, data);
            if state.pending {
                control.signal(address, data);
                state.pending = false;
            }
        }
    }

    /// Disables the interrupt.
    ///
    /// Interrupt deliveries while the interrupt is disabled will set an
    /// internal pending state.
    pub fn disable(&mut self) {
        let mut state = self.state.lock();
        state.address_data = None;
        if let Some(control) = &mut state.control {
            control.disable();
        }
    }

    /// Clears any internal pending state and returns it.
    pub fn drain_pending(&mut self) -> bool {
        let mut state = self.state.lock();
        std::mem::take(&mut state.pending)
    }

    /// Returns an object that can be used to deliver the interrupt.
    pub fn interrupt(&mut self) -> Interrupt {
        let state = self.state.clone();
        Interrupt::from_fn(move || {
            let mut state = state.lock();
            let state = &mut *state;
            if let Some((control, (address, data))) = state.control.as_mut().zip(state.address_data)
            {
                control.signal(address, data);
            } else {
                state.pending = true;
            }
        })
    }
}
