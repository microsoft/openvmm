// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL DVSEC register abstractions.

mod cxl_device_dvsec;
mod cxl_port_dvsec;
mod flex_bus_port_dvsec;
mod register_locator_dvsec;
pub use cxl_device_dvsec::CxlCacheWriteBackAndInvalidateHandler;
pub use cxl_device_dvsec::CxlDeviceDevsecExtendedCapability;
pub use cxl_device_dvsec::CxlResetHandler;
pub use cxl_port_dvsec::CxlPortDvsecExtendedCapability;
pub use flex_bus_port_dvsec::CxlFlexBusPortDvsecExtendedCapability;
pub use register_locator_dvsec::CxlRegisterLocatorDvsecExtendedCapability;
