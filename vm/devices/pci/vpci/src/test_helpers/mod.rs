// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test helpers.

#![expect(missing_docs)]

use parking_lot::Mutex;
use pci_core::msi::SignalMsi;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::collections::btree_map;
use std::sync::Arc;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::RegisterInterruptError;
use vmcore::vpci_msi::VpciInterruptParameters;
use vmcore::vpci_msi::VpciInterruptState;

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
    fn signal_msi(&self, _devid: Option<u32>, address: u64, data: u32) {
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

    async fn interrupt_state(
        &self,
        msi: MsiAddressData,
    ) -> Result<VpciInterruptState, RegisterInterruptError> {
        self.inner.mapping_table.lock().interrupt_state(msi)
    }

    async fn restore_interrupt(
        &self,
        msi: MsiAddressData,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<(), RegisterInterruptError> {
        self.inner
            .mapping_table
            .lock()
            .restore_interrupt(msi, vector_count, params)
    }

    fn supports_save_restore(&self) -> bool {
        true
    }
}

#[derive(Debug)]
struct MsiInterrupt {
    base_vector: u32,
    multicast: bool,
    /// Never empty; delivery goes to the first entry.
    target_processors: Vec<u32>,
}

/// An implementation of an VPCI interrupt mapping table for implementing VPCI
/// devices without explicit hypervisor support (except for exiting on the
/// retarget interrupt hypercall).
#[derive(Debug, Default)]
struct MsiInterruptMappingTable {
    interrupts: BTreeMap<u16, MsiInterrupt>,
}

impl MsiInterruptMappingTable {
    const MAX_INTERRUPTS: u16 = 500;

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

    /// Returns the table index `msi` names, if `msi` has the exact form
    /// [`Self::register_interrupt`] hands out.
    fn index_from_msi(msi: MsiAddressData) -> Option<u16> {
        let index = Self::interrupt_index_from_address(msi.address);
        (index < Self::MAX_INTERRUPTS
            && msi.address == Self::interrupt_address_from_index(index)
            && msi.data == 0)
            .then_some(index)
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
        if !params.target_processors.is_empty() {
            interrupt.target_processors = params.target_processors.to_vec();
        }
    }

    fn translate_interrupt(&self, address: u64, data: u32) -> Option<RequestedInterrupt> {
        let index = Self::interrupt_index_from_address(address);
        let interrupt = self.interrupts.get(&index)?;
        if data > (u8::MAX as u32 - interrupt.base_vector) {
            return None;
        }
        let vector = interrupt.base_vector + data;
        Some(RequestedInterrupt {
            vector,
            destination: interrupt.target_processors[0],
        })
    }

    fn new_entry(
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiInterrupt, RegisterInterruptError> {
        if vector_count == 0 || params.target_processors.is_empty() {
            return Err(RegisterInterruptError::new("invalid input"));
        }
        Ok(MsiInterrupt {
            base_vector: params.vector,
            multicast: params.multicast,
            target_processors: params.target_processors.to_vec(),
        })
    }

    fn register_interrupt(
        &mut self,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError> {
        let entry = Self::new_entry(vector_count, params)?;
        for i in 0..Self::MAX_INTERRUPTS {
            if let btree_map::Entry::Vacant(e) = self.interrupts.entry(i) {
                e.insert(entry);
                return Ok(MsiAddressData {
                    address: Self::interrupt_address_from_index(i),
                    data: 0,
                });
            }
        }

        Err(RegisterInterruptError::new("out of interrupts"))
    }

    fn unregister_interrupt(&mut self, address: u64, _data: u32) {
        let index = Self::interrupt_index_from_address(address);
        self.interrupts.remove(&index);
    }

    fn interrupt_state(
        &self,
        msi: MsiAddressData,
    ) -> Result<VpciInterruptState, RegisterInterruptError> {
        let interrupt = Self::index_from_msi(msi)
            .and_then(|index| self.interrupts.get(&index))
            .ok_or_else(|| RegisterInterruptError::new("interrupt not registered"))?;
        Ok(VpciInterruptState {
            vector: interrupt.base_vector,
            multicast: interrupt.multicast,
            target_processors: interrupt.target_processors.clone(),
        })
    }

    fn restore_interrupt(
        &mut self,
        msi: MsiAddressData,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<(), RegisterInterruptError> {
        let entry = Self::new_entry(vector_count, params)?;
        let index = Self::index_from_msi(msi)
            .ok_or_else(|| RegisterInterruptError::new("invalid address"))?;
        match self.interrupts.entry(index) {
            btree_map::Entry::Vacant(e) => {
                e.insert(entry);
                Ok(())
            }
            btree_map::Entry::Occupied(_) => Err(RegisterInterruptError::new("index in use")),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::test_helpers::TestVpciInterruptController;
    use pal_async::async_test;
    use vmcore::vpci_msi::VpciInterruptMapper;
    use vmcore::vpci_msi::VpciInterruptState;

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

    fn deliver(intc: &TestVpciInterruptController, msi: MsiAddressData, offset: u32) -> (u32, u32) {
        intc.deliver_interrupt(msi.address, msi.data + offset);
        let delivered = intc.get_next_interrupt().unwrap();
        assert!(intc.get_next_interrupt().is_none());
        (delivered.vector, delivered.destination)
    }

    struct Registered {
        msi: MsiAddressData,
        vector_count: u32,
    }

    /// Registers three interrupts on `intc`, retargets the last one and
    /// unregisters the middle one, leaving a gap in the table.
    async fn populate(intc: &VpciInterruptMapper) -> Vec<Registered> {
        let mut registered = Vec::new();
        for (vector, vector_count, target) in [(0x30, 2, 1), (0x40, 1, 2), (0x50, 4, 3)] {
            let msi = intc
                .register_interrupt(
                    vector_count,
                    &VpciInterruptParameters {
                        vector,
                        multicast: false,
                        target_processors: &[target],
                    },
                )
                .await
                .unwrap();
            registered.push(Registered { msi, vector_count });
        }
        let gap = registered.remove(1);
        intc.unregister_interrupt(gap.msi.address, gap.msi.data)
            .await;
        registered
    }

    #[async_test]
    async fn verify_save_restore_round_trip() {
        let old = TestVpciInterruptController::new();
        let old_mapper = VpciInterruptMapper::new(old.clone());
        assert!(old_mapper.supports_save_restore());
        let registered = populate(&old_mapper).await;
        old.retarget_interrupt(
            registered[1].msi.address,
            registered[1].msi.data,
            &VpciInterruptParameters {
                vector: 0x58,
                multicast: true,
                target_processors: &[5, 6],
            },
        );

        let mut saved = Vec::new();
        for r in &registered {
            saved.push(old_mapper.interrupt_state(r.msi).await.unwrap());
        }
        assert_eq!(
            saved,
            [
                VpciInterruptState {
                    vector: 0x30,
                    multicast: false,
                    target_processors: vec![1],
                },
                VpciInterruptState {
                    vector: 0x58,
                    multicast: true,
                    target_processors: vec![5, 6],
                },
            ]
        );

        let new = TestVpciInterruptController::new();
        let new_mapper = VpciInterruptMapper::new(new.clone());
        for (r, state) in registered.iter().zip(&saved) {
            new_mapper
                .restore_interrupt(
                    r.msi,
                    r.vector_count,
                    &VpciInterruptParameters {
                        vector: state.vector,
                        multicast: state.multicast,
                        target_processors: &state.target_processors,
                    },
                )
                .await
                .unwrap();
        }

        for (r, state) in registered.iter().zip(&saved) {
            assert_eq!(new_mapper.interrupt_state(r.msi).await.unwrap(), *state);
            for offset in 0..r.vector_count {
                assert_eq!(deliver(&new, r.msi, offset), deliver(&old, r.msi, offset));
            }
        }

        // A new registration takes the free index, never a restored one.
        let fresh = new_mapper
            .register_interrupt(
                1,
                &VpciInterruptParameters {
                    vector: 0x60,
                    multicast: false,
                    target_processors: &[0],
                },
            )
            .await
            .unwrap();
        assert!(registered.iter().all(|r| r.msi != fresh));
        assert_eq!(deliver(&new, fresh, 0), (0x60, 0));
    }

    #[async_test]
    async fn verify_restore_refuses_invalid() {
        let intc = TestVpciInterruptController::new();
        let mapper = VpciInterruptMapper::new(intc.clone());
        let registered = populate(&mapper).await;
        let taken = registered[0].msi;
        let free = MsiAddressData {
            address: 0xfee00004,
            data: 0,
        };
        let params = VpciInterruptParameters {
            vector: 0x70,
            multicast: false,
            target_processors: &[0],
        };

        let cases = [
            ("occupied index", taken, 1, &params),
            (
                "address outside the window",
                MsiAddressData {
                    address: 0xfed00004,
                    ..free
                },
                1,
                &params,
            ),
            (
                "address with low bits set",
                MsiAddressData {
                    address: 0xfee00005,
                    ..free
                },
                1,
                &params,
            ),
            (
                "nonzero data",
                MsiAddressData { data: 1, ..free },
                1,
                &params,
            ),
            ("zero vector count", free, 0, &params),
            (
                "no target",
                free,
                1,
                &VpciInterruptParameters {
                    target_processors: &[],
                    ..params
                },
            ),
        ];
        for (name, msi, vector_count, params) in cases {
            assert!(
                mapper
                    .restore_interrupt(msi, vector_count, params)
                    .await
                    .is_err(),
                "{name}"
            );
        }

        // Nothing changed: the occupied entry still routes as before and the
        // free index is still free.
        assert_eq!(deliver(&intc, taken, 0), (0x30, 1));
        assert!(mapper.interrupt_state(free).await.is_err());
        intc.deliver_interrupt(free.address, free.data);
        assert!(intc.get_next_interrupt().is_none());
    }

    #[async_test]
    async fn verify_interrupt_state_of_unknown_interrupt() {
        let intc = TestVpciInterruptController::new();
        let mapper = VpciInterruptMapper::new(intc);
        let registered = populate(&mapper).await;
        let MsiAddressData { address, data } = registered[0].msi;
        for msi in [
            MsiAddressData {
                address: 0xfee00004,
                data,
            },
            MsiAddressData {
                address,
                data: data + 1,
            },
        ] {
            assert!(mapper.interrupt_state(msi).await.is_err(), "{msi:?}");
        }
    }
}
