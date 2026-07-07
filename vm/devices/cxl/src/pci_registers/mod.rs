// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL DVSEC register abstractions.

mod cxl_device_dvsec;
mod cxl_port_dvsec;
mod flex_bus_port_dvsec;
mod register_locator_dvsec;
pub mod spec;

pub use spec::cxl_device_dvsec::CxlDeviceDevsecExtendedCapability;
pub use spec::cxl_port_dvsec::CxlPortDvsecExtendedCapability;
pub use spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecExtendedCapability;
pub use spec::register_locator_dvsec::CxlRegisterLocatorDvsecExtendedCapability;
