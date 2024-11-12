// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;

pub struct IoApicRouting<T: ?Sized>(pub Arc<T>);

impl<T: ?Sized + virt::irqcon::IoApicRouting> chipset::ioapic::IoApicRouting for IoApicRouting<T> {
    fn assert(&self, index: u8) {
        self.0.assert_irq(index);
    }

    fn set_route(&self, index: u8, request: Option<(u64, u32)>) {
        self.0.set_irq_route(
            index,
            request.map(|(address, data)| virt::irqcon::MsiRequest { address, data }),
        )
    }
}
