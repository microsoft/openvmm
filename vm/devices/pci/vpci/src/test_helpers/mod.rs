// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test helpers.

#![expect(missing_docs)]

use parking_lot::Mutex;
use pci_core::msi::SignalMsi;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::RegisterInterruptError;
use vmcore::vpci_msi::VpciInterruptParameters;

#[derive(Debug, Clone)]
pub struct TestVpciInterruptController {
    inner: Arc<TestVpciInterruptControllerInner>,
}

impl TestVpciInterruptController {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(TestVpciInterruptControllerInner {
                mapping_table: Mutex::new(MsiInterruptMappingTable::new()),
                msi_requests: Mutex::new(VecDeque::new()),
            }),
        })
    }

    pub fn signal_msi(&self) -> Arc<dyn SignalMsi> {
        self.inner.clone()
    }

    pub fn get_next_interrupt(&self) -> Option<RequestedInterrupt> {
        self.inner.msi_requests.lock().pop_front()
    }

    pub fn retarget_interrupt(
        &self,
        address: u64,
        data: u32,
        params: &VpciInterruptParameters<'_>,
    ) {
        self.inner
            .mapping_table
            .lock()
            .retarget_interrupt(address, data, params)
    }

    pub fn deliver_interrupt(&self, address: u64, data: u32) {
        self.inner.deliver_interrupt(address, data)
    }
}

#[derive(Debug)]
pub struct RequestedInterrupt {
    pub vector: u32,
    pub destination: u32,
}

#[derive(Debug)]
struct TestVpciInterruptControllerInner {
    mapping_table: Mutex<MsiInterruptMappingTable>,
    msi_requests: Mutex<VecDeque<RequestedInterrupt>>,
}

impl TestVpciInterruptControllerInner {
    fn deliver_interrupt(&self, address: u64, data: u32) {
        if let Some(request) = self.mapping_table.lock().translate_interrupt(address, data) {
            self.msi_requests.lock().push_back(request);
        }
    }
}

impl SignalMsi for TestVpciInterruptControllerInner {
    fn signal_msi(&self, rid: u32, address: u64, data: u32) {
        assert_eq!(rid, 0);
        self.deliver_interrupt(address, data);
    }
}

impl MapVpciInterrupt for TestVpciInterruptController {
    async fn register_interrupt(
        &self,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError> {
        self.inner
            .mapping_table
            .lock()
            .register_interrupt(vector_count, params)
    }

    async fn unregister_interrupt(&self, address: u64, data: u32) {
        self.inner
            .mapping_table
            .lock()
            .unregister_interrupt(address, data)
    }
}

#[derive(Debug)]
struct MsiInterrupt {
    _address: u64,
    _data: u32,
    base_vector: u32,
    _vector_count: u32,
    multicast: bool,
    target_processor: Arc<Mutex<u32>>,
}

/// An implementation of an VPCI interrupt mapping table for implementing VPCI
/// devices without explicit hypervisor support (except for exiting on the
/// retarget interrupt hypercall).
#[derive(Debug, Default)]
struct MsiInterruptMappingTable {
    interrupts: BTreeMap<u16, MsiInterrupt>,
}

impl MsiInterruptMappingTable {
    fn new() -> Self {
        Self {
            interrupts: BTreeMap::new(),
        }
    }

    fn interrupt_address_from_index(index: u16) -> u64 {
        // Per Intel spec, set the upper bits to FEE.
        // Set lower bits to the specified index, shifted to avoid the bits
        // that actually mean something (redirection hint / destination mode).
        0x00000000FEE00000 | ((index as u64) << 2)
    }

    fn interrupt_index_from_address(address: u64) -> u16 {
        u16::try_from((address >> 2) & 0xffff).unwrap()
    }

    fn retarget_interrupt(
        &mut self,
        address: u64,
        _data: u32,
        params: &VpciInterruptParameters<'_>,
    ) {
        let index = Self::interrupt_index_from_address(address);
        let interrupt = self.interrupts.get_mut(&index);
        if interrupt.is_none() {
            return;
        }
        let interrupt = interrupt.unwrap();
        interrupt.base_vector = params.vector;
        interrupt.multicast = params.multicast;
        if let Some(target_processor) = params.target_processors.first() {
            *interrupt.target_processor.lock() = *target_processor;
        }
    }

    fn translate_interrupt(&self, address: u64, data: u32) -> Option<RequestedInterrupt> {
        let index = Self::interrupt_index_from_address(address);
        let interrupt = self.interrupts.get(&index)?;
        if data > (u8::MAX as u32 - interrupt.base_vector) {
            return None;
        }
        let vector = interrupt.base_vector + data;
        let target_processor = *interrupt.target_processor.lock();
        Some(RequestedInterrupt {
            vector,
            destination: target_processor,
        })
    }

    fn register_interrupt(
        &mut self,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError> {
        if vector_count == 0 || params.target_processors.is_empty() {
            return Err(RegisterInterruptError::new("invalid input"));
        }
        for i in 0..500 {
            if let std::collections::btree_map::Entry::Vacant(e) = self.interrupts.entry(i) {
                let address = Self::interrupt_address_from_index(i);
                e.insert(MsiInterrupt {
                    _address: address,
                    _data: 0,
                    base_vector: params.vector,
                    _vector_count: vector_count,
                    multicast: params.multicast,
                    target_processor: Arc::new(Mutex::new(params.target_processors[0])),
                });

                return Ok(MsiAddressData { address, data: 0 });
            }
        }

        Err(RegisterInterruptError::new("out of interrupts"))
    }

    fn unregister_interrupt(&mut self, address: u64, _data: u32) {
        let index = Self::interrupt_index_from_address(address);
        self.interrupts.remove(&index);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::test_helpers::TestVpciInterruptController;
    use pal_async::async_test;

    #[async_test]
    async fn verify_simple_interrupt() {
        let test_intc = TestVpciInterruptController::new();
        let interrupt = test_intc
            .register_interrupt(
                1,
                &VpciInterruptParameters {
                    vector: 0x21,
                    multicast: false,
                    target_processors: &[0],
                },
            )
            .await;
        let MsiAddressData {
            address: vector,
            data,
        } = interrupt.unwrap();
        assert_eq!(vector, 0xfee00000);
        assert_eq!(data, 0);

        test_intc.deliver_interrupt(0xfee00000, 0);
        let delivered = test_intc.get_next_interrupt().unwrap();
        assert_eq!(delivered.vector, 0x21);
        assert_eq!(delivered.destination, 0);

        assert!(test_intc.get_next_interrupt().is_none());
    }

    #[async_test]
    async fn verify_multi_interrupt() {
        let test_intc = TestVpciInterruptController::new();
        let interrupt = test_intc
            .register_interrupt(
                2,
                &VpciInterruptParameters {
                    vector: 0x21,
                    multicast: false,
                    target_processors: &[0],
                },
            )
            .await;
        let MsiAddressData {
            address: vector,
            data,
        } = interrupt.unwrap();
        assert_eq!(vector, 0xfee00000);
        assert_eq!(data, 0);

        test_intc.deliver_interrupt(0xfee00000, 0);
        test_intc.deliver_interrupt(0xfee00000, 1);
        let delivered = test_intc.get_next_interrupt().unwrap();
        assert_eq!(delivered.vector, 0x21);
        assert_eq!(delivered.destination, 0);
        let delivered = test_intc.get_next_interrupt().unwrap();
        assert_eq!(delivered.vector, 0x22);
        assert_eq!(delivered.destination, 0);

        assert!(test_intc.get_next_interrupt().is_none());
    }

    #[async_test]
    async fn verify_two_interrupts() {
        let test_intc = TestVpciInterruptController::new();
        let interrupt = test_intc
            .register_interrupt(
                1,
                &VpciInterruptParameters {
                    vector: 0x21,
                    multicast: false,
                    target_processors: &[0],
                },
            )
            .await;
        let MsiAddressData {
            address: vector,
            data,
        } = interrupt.unwrap();
        assert_eq!(vector, 0xfee00000);
        assert_eq!(data, 0);
        let interrupt = test_intc
            .register_interrupt(
                1,
                &VpciInterruptParameters {
                    vector: 0x27,
                    multicast: false,
                    target_processors: &[1],
                },
            )
            .await;
        let MsiAddressData {
            address: vector,
            data,
        } = interrupt.unwrap();
        assert_eq!(vector, 0xfee00004);
        assert_eq!(data, 0);

        test_intc.deliver_interrupt(0xfee00000, 0);
        let delivered = test_intc.get_next_interrupt().unwrap();
        assert_eq!(delivered.vector, 0x21);
        assert_eq!(delivered.destination, 0);

        test_intc.deliver_interrupt(0xfee00004, 0);
        let delivered = test_intc.get_next_interrupt().unwrap();
        assert_eq!(delivered.vector, 0x27);
        assert_eq!(delivered.destination, 1);

        assert!(test_intc.get_next_interrupt().is_none());
    }

    #[async_test]
    async fn verify_retarget() {
        let test_intc = TestVpciInterruptController::new();
        let interrupt = test_intc
            .register_interrupt(
                1,
                &VpciInterruptParameters {
                    vector: 0x21,
                    multicast: false,
                    target_processors: &[0],
                },
            )
            .await;
        let MsiAddressData {
            address: vector,
            data,
        } = interrupt.unwrap();
        assert_eq!(vector, 0xfee00000);
        assert_eq!(data, 0);

        test_intc.deliver_interrupt(0xfee00000, 0);
        let delivered = test_intc.get_next_interrupt().unwrap();
        assert_eq!(delivered.vector, 0x21);
        assert_eq!(delivered.destination, 0);

        test_intc.retarget_interrupt(
            0xfee00000,
            0,
            &VpciInterruptParameters {
                vector: 0x21,
                multicast: false,
                target_processors: &[2, 3],
            },
        );
        test_intc.deliver_interrupt(0xfee00000, 0);
        let delivered = test_intc.get_next_interrupt().unwrap();
        assert_eq!(delivered.vector, 0x21);
        assert_eq!(delivered.destination, 2);

        assert!(test_intc.get_next_interrupt().is_none());
    }
}
