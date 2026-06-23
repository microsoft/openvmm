// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL PCIe DVSEC extended capability implementation.

use pci_core::capabilities::extended::PciExtendedCapability;
use pci_core::spec::caps::ExtendedCapabilityId;
use pci_core::spec::caps::dvsec::DvsecExtendedCapabilityHeader;
use pci_core::spec::caps::dvsec::DvsecHeader1;
use pci_core::spec::caps::dvsec::DvsecHeader2;
use std::sync::Arc;

use super::spec::CXL_DVSEC_VENDOR_ID;
use super::spec::cxl_device_dvsec::CXL_DEVICE_DVSEC_CONTROL_WRITABLE_MASK;
use super::spec::cxl_device_dvsec::CXL_DEVICE_DVSEC_CONTROL2_WRITABLE_MASK;
use super::spec::cxl_device_dvsec::CXL_DEVICE_DVSEC_ID;
use super::spec::cxl_device_dvsec::CXL_DEVICE_DVSEC_LENGTH;
use super::spec::cxl_device_dvsec::CXL_DEVICE_DVSEC_RANGE_BASE_LOW_WRITABLE_MASK;
use super::spec::cxl_device_dvsec::CXL_DEVICE_DVSEC_REVISION;
use super::spec::cxl_device_dvsec::CXL_DEVICE_DVSEC_STATUS_RW1C_MASK;
use super::spec::cxl_device_dvsec::CXL_DEVICE_DVSEC_STATUS2_RW1C_MASK;
use super::spec::cxl_device_dvsec::CXL_DEVICE_DVSEC_STATUS2_VOLATILE_HDM_PRESERVATION_ERROR_RW1C_MASK;
use super::spec::cxl_device_dvsec::CxlCacheWriteBackAndInvalidateHandler;
use super::spec::cxl_device_dvsec::CxlDeviceDevsecExtendedCapability;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecCacheSizeUnit;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecCapability;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecCapability2;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecCapability3;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecControl;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecControl2;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecDesiredInterleave;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecLock;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecMediaType;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecMemoryActiveTimeout;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecMemoryClass;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecRangeBaseLow;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecRangeSizeLow;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecRegisterOffset;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecResetTimeout;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecStatus;
use super::spec::cxl_device_dvsec::CxlDeviceDvsecStatus2;
use super::spec::cxl_device_dvsec::CxlResetHandler;
use thiserror::Error;

const CXL_RANGE_GRANULARITY: u64 = 256 * 1024 * 1024;

/// Optional CXL memory range programming input for `with_cxl_memory`.
#[derive(Debug, Copy, Clone)]
pub struct CxlMemoryRange {
    /// Base address of the CXL range.
    pub base: u64,
    /// Size of the CXL range.
    pub size: u64,
}

/// Errors returned when configuring CXL memory ranges.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Error)]
pub enum CxlMemoryConfigError {
    /// Both DVSEC HDM range slots are already configured.
    #[error("both DVSEC HDM range slots are already configured")]
    TooManyHdmRanges,
}

impl Default for CxlDeviceDevsecExtendedCapability {
    fn default() -> Self {
        let capability = CxlDeviceDvsecCapability::new()
            .with_io_capable(true)
            .with_cxl_reset_capable(true)
            .with_cxl_reset_timeout(CxlDeviceDvsecResetTimeout::Seconds1.bits())
            .with_cxl_reset_mem_clr_capable(true);
        let capability2 = CxlDeviceDvsecCapability2::new();
        let capability3 = CxlDeviceDvsecCapability3::new();
        let range1_base_low = CxlDeviceDvsecRangeBaseLow::new();
        let range2_base_low = CxlDeviceDvsecRangeBaseLow::new();
        Self {
            control: CxlDeviceDvsecControl::new().with_io_enable(true),
            status: CxlDeviceDvsecStatus::new(),
            control2: CxlDeviceDvsecControl2::new(),
            status2: CxlDeviceDvsecStatus2::new(),
            lock: CxlDeviceDvsecLock::new(),
            capability,
            capability2,
            capability3,
            range1_size_high: 0,
            range1_size_low: 0,
            range1_base_high: 0,
            range1_base_low,
            range2_size_high: 0,
            range2_size_low: 0,
            range2_base_high: 0,
            range2_base_low,
            cxl_reset_handler: None,
            cxl_cache_write_back_and_invalidate_handler: None,
            reset_baseline_capability: capability,
            reset_baseline_capability2: capability2,
            reset_baseline_capability3: capability3,
            reset_baseline_range1_size_high: 0,
            reset_baseline_range1_size_low: 0,
            reset_baseline_range1_base_high: 0,
            reset_baseline_range1_base_low: range1_base_low,
            reset_baseline_range2_size_high: 0,
            reset_baseline_range2_size_low: 0,
            reset_baseline_range2_base_high: 0,
            reset_baseline_range2_base_low: range2_base_low,
        }
    }
}

impl CxlDeviceDevsecExtendedCapability {
    /// Creates a new CXL DVSEC capability.
    ///
    /// When `cxl_reset_handler` is provided, it is invoked when software
    /// requests a new CXL reset via DVSEC Control2.
    pub fn new(
        cxl_reset_handler: Option<Arc<dyn CxlResetHandler>>,
        cxl_cache_write_back_and_invalidate_handler: Option<
            Arc<dyn CxlCacheWriteBackAndInvalidateHandler>,
        >,
    ) -> Self {
        Self {
            cxl_reset_handler,
            cxl_cache_write_back_and_invalidate_handler,
            ..Self::default()
        }
    }

    /// Enables CXL.cache support and programs Capability2 cache size encoding.
    pub fn with_cxl_cache(
        mut self,
        cache_size_unit: CxlDeviceDvsecCacheSizeUnit,
        cache_size: u8,
    ) -> Self {
        self.capability = self
            .capability
            .with_cache_capable(true)
            .with_cache_writeback_and_invalidate_capable(true);
        let programmed_cache_size = if cache_size_unit == CxlDeviceDvsecCacheSizeUnit::NotReported {
            0
        } else {
            cache_size
        };
        self.capability2 = self
            .capability2
            .with_cache_size_unit(cache_size_unit.bits())
            .with_cache_size(programmed_cache_size);
        self.reset_baseline_capability = self.capability;
        self.reset_baseline_capability2 = self.capability2;
        self.reset_baseline_capability3 = self.capability3;
        self
    }

    /// Enables CXL.mem support when `hdm_size` is valid.
    ///
    /// `range` programming is optional; when present, the range base registers
    /// are updated only if the range encoding is valid.
    pub fn with_cxl_memory(
        mut self,
        hdm_size: u64,
        range: Option<CxlMemoryRange>,
        media_type: CxlDeviceDvsecMediaType,
        memory_class: CxlDeviceDvsecMemoryClass,
        desired_interleave: CxlDeviceDvsecDesiredInterleave,
        memory_active_timeout: CxlDeviceDvsecMemoryActiveTimeout,
    ) -> Result<Self, CxlMemoryConfigError> {
        if !Self::is_valid_memory_size(hdm_size) {
            return Ok(self);
        }

        let range1_enabled = Self::range_is_enabled(self.range1_size_low);
        let range2_enabled = Self::range_is_enabled(self.range2_size_low);
        if range1_enabled && range2_enabled {
            return Err(CxlMemoryConfigError::TooManyHdmRanges);
        }

        let size_low = ((hdm_size >> 28) & 0xf) as u8;
        let range_size_high = (hdm_size >> 32) as u32;
        let range_size_low = CxlDeviceDvsecRangeSizeLow::new()
            .with_memory_info_valid(true)
            .with_memory_active(true)
            .with_media_type(media_type.bits())
            .with_memory_class(memory_class.bits())
            .with_desired_interleave(desired_interleave.bits())
            .with_memory_active_timeout(memory_active_timeout.bits())
            .with_memory_size_low(size_low)
            .into_bits();

        let mut range_base_high = 0;
        let mut range_base_low = CxlDeviceDvsecRangeBaseLow::new();
        if let Some(r) = range {
            if Self::is_valid_memory_range(r) {
                range_base_high = (r.base >> 32) as u32;
                range_base_low = CxlDeviceDvsecRangeBaseLow::new().with_memory_base_low(size_low);
            }
        }

        if !range1_enabled {
            self.range1_size_high = range_size_high;
            self.range1_size_low = range_size_low;
            self.range1_base_high = range_base_high;
            self.range1_base_low = range_base_low;
        } else {
            self.range2_size_high = range_size_high;
            self.range2_size_low = range_size_low;
            self.range2_base_high = range_base_high;
            self.range2_base_low = range_base_low;
        }

        self.refresh_mem_capability();
        self.reset_baseline_capability = self.capability;
        self.reset_baseline_capability2 = self.capability2;
        self.reset_baseline_capability3 = self.capability3;
        self.reset_baseline_range1_size_high = self.range1_size_high;
        self.reset_baseline_range1_size_low = self.range1_size_low;
        self.reset_baseline_range1_base_high = self.range1_base_high;
        self.reset_baseline_range1_base_low = self.range1_base_low;
        self.reset_baseline_range2_size_high = self.range2_size_high;
        self.reset_baseline_range2_size_low = self.range2_size_low;
        self.reset_baseline_range2_base_high = self.range2_base_high;
        self.reset_baseline_range2_base_low = self.range2_base_low;
        Ok(self)
    }

    fn dvsec_len(&self) -> usize {
        usize::from(CXL_DEVICE_DVSEC_LENGTH)
    }

    fn read_dvsec_u32(&self, offset: u16) -> u32 {
        const DVSEC_HEADER1_OFFSET: u16 = DvsecExtendedCapabilityHeader::DVSEC_HEADER1.0;
        const DVSEC_HEADER2_OFFSET: u16 = DvsecExtendedCapabilityHeader::DVSEC_HEADER2.0;

        match offset {
            DVSEC_HEADER1_OFFSET => Self::dvsec_header1().into_bits(),
            DVSEC_HEADER2_OFFSET => {
                u32::from(Self::dvsec_header2().into_bits())
                    | (u32::from(self.capability.into_bits()) << 16)
            }
            CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL_STATUS => {
                u32::from(self.control.into_bits()) | (u32::from(self.status.into_bits()) << 16)
            }
            CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2 => {
                u32::from(self.control2.into_bits()) | (u32::from(self.status2.into_bits()) << 16)
            }
            CxlDeviceDvsecRegisterOffset::DVSEC_LOCK_CAPABILITY2 => {
                u32::from(self.lock.into_bits()) | (u32::from(self.capability2.into_bits()) << 16)
            }
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_SIZE_HIGH => self.range1_size_high,
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_SIZE_LOW => self.range1_size_low,
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_HIGH => self.range1_base_high,
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_LOW => self.range1_base_low.into_bits(),
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE2_SIZE_HIGH => self.range2_size_high,
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE2_SIZE_LOW => self.range2_size_low,
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE2_BASE_HIGH => self.range2_base_high,
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE2_BASE_LOW => self.range2_base_low.into_bits(),
            CxlDeviceDvsecRegisterOffset::DVSEC_CAPABILITY3 => {
                u32::from(self.capability3.into_bits())
            }
            _ => !0,
        }
    }

    fn write_dvsec_u32(&mut self, offset: u16, value: u32) {
        if offset == CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL_STATUS {
            self.handle_control_status_write(value);
            return;
        }

        if offset == CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2 {
            self.handle_control2_status2_write(value);
            return;
        }

        if offset == CxlDeviceDvsecRegisterOffset::DVSEC_LOCK_CAPABILITY2 {
            let requested_lock = (value as u16) & 0x1;
            if requested_lock != 0 {
                self.lock = self.lock.with_config_lock(true);
            }
            return;
        }

        if self.lock.config_lock() {
            return;
        }

        match offset {
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_HIGH => {
                self.range1_base_high = value;
            }
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_LOW => {
                let preserved = self.range1_base_low.into_bits()
                    & !CXL_DEVICE_DVSEC_RANGE_BASE_LOW_WRITABLE_MASK;
                let writable = value & CXL_DEVICE_DVSEC_RANGE_BASE_LOW_WRITABLE_MASK;
                self.range1_base_low = CxlDeviceDvsecRangeBaseLow::from_bits(preserved | writable);
            }
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE2_BASE_HIGH => {
                self.range2_base_high = value;
            }
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE2_BASE_LOW => {
                let preserved = self.range2_base_low.into_bits()
                    & !CXL_DEVICE_DVSEC_RANGE_BASE_LOW_WRITABLE_MASK;
                let writable = value & CXL_DEVICE_DVSEC_RANGE_BASE_LOW_WRITABLE_MASK;
                self.range2_base_low = CxlDeviceDvsecRangeBaseLow::from_bits(preserved | writable);
            }
            _ => {}
        }
    }

    fn reset_state(&mut self) {
        self.control = CxlDeviceDvsecControl::new()
            .with_io_enable(self.reset_baseline_capability.io_capable());
        self.status = CxlDeviceDvsecStatus::new();
        self.control2 = CxlDeviceDvsecControl2::new();
        self.status2 = CxlDeviceDvsecStatus2::new();
        self.lock = CxlDeviceDvsecLock::new();
        self.capability = self.reset_baseline_capability;
        self.capability2 = self.reset_baseline_capability2;
        self.capability3 = self.reset_baseline_capability3;
        self.range1_size_high = self.reset_baseline_range1_size_high;
        self.range1_size_low = self.reset_baseline_range1_size_low;
        self.range1_base_high = self.reset_baseline_range1_base_high;
        self.range1_base_low = self.reset_baseline_range1_base_low;
        self.range2_size_high = self.reset_baseline_range2_size_high;
        self.range2_size_low = self.reset_baseline_range2_size_low;
        self.range2_base_high = self.reset_baseline_range2_base_high;
        self.range2_base_low = self.reset_baseline_range2_base_low;
    }

    fn dvsec_header1() -> DvsecHeader1 {
        DvsecHeader1::new()
            .with_dvsec_vendor_id(CXL_DVSEC_VENDOR_ID)
            .with_dvsec_revision(CXL_DEVICE_DVSEC_REVISION)
            .with_dvsec_length(CXL_DEVICE_DVSEC_LENGTH)
    }

    fn dvsec_header2() -> DvsecHeader2 {
        DvsecHeader2::new().with_dvsec_id(CXL_DEVICE_DVSEC_ID)
    }

    fn is_valid_memory_size(hdm_size: u64) -> bool {
        hdm_size != 0 && hdm_size.is_multiple_of(CXL_RANGE_GRANULARITY)
    }

    fn is_valid_memory_range(range: CxlMemoryRange) -> bool {
        range.base.is_multiple_of(CXL_RANGE_GRANULARITY)
            && range.size != 0
            && range.size.is_multiple_of(CXL_RANGE_GRANULARITY)
    }

    fn range_is_enabled(range_size_low: u32) -> bool {
        CxlDeviceDvsecRangeSizeLow::from_bits(range_size_low).memory_info_valid()
    }

    fn refresh_mem_capability(&mut self) {
        let r1_enabled = Self::range_is_enabled(self.range1_size_low);
        let r2_enabled = Self::range_is_enabled(self.range2_size_low);

        let (mem_capable, hdm_count) = match (r1_enabled, r2_enabled) {
            (false, false) => (false, 0),
            (true, false) | (false, true) => (true, 0x1),
            (true, true) => (true, 0x2),
        };

        self.capability = self
            .capability
            .with_mem_capable(mem_capable)
            .with_hdm_count(hdm_count);
    }

    fn handle_control_status_write(&mut self, value: u32) {
        if !self.lock.config_lock() {
            let requested = (value as u16) & CXL_DEVICE_DVSEC_CONTROL_WRITABLE_MASK;
            let mut next = CxlDeviceDvsecControl::from_bits(requested);

            if !self.capability.cache_capable() {
                next = next.with_cache_enable(false);
            }
            // CXL.io is required to remain enabled.
            next = next.with_io_enable(true);
            if !self.capability.mem_capable() {
                next = next.with_mem_enable(false);
            }
            if !self.capability3.direct_p2p_mem_capable() {
                next = next.with_direct_p2p_mem_enable(false);
            }
            if !self.capability.viral_capable() {
                next = next.with_viral_enable(false);
            }

            self.control = next;
        }

        let clear_mask = ((value >> 16) as u16) & CXL_DEVICE_DVSEC_STATUS_RW1C_MASK;
        if clear_mask != 0 {
            let next_bits = self.status.into_bits() & !clear_mask;
            self.status = CxlDeviceDvsecStatus::from_bits(next_bits);
        }
    }

    fn handle_control2_status2_write(&mut self, value: u32) {
        // Control2 is RW (not RWL), so config_lock must not gate writes here.
        let requested = (value as u16) & CXL_DEVICE_DVSEC_CONTROL2_WRITABLE_MASK;
        let mut next = CxlDeviceDvsecControl2::from_bits(requested);
        let old_cwbi = self.control2.initiate_cache_writeback_and_invalidation();
        let new_cwbi = next.initiate_cache_writeback_and_invalidation();
        let old_reset = self.control2.initiate_cxl_reset();
        let new_reset = next.initiate_cxl_reset();

        if self.capability.cache_writeback_and_invalidate_capable() && new_cwbi && !old_cwbi {
            if let Some(handler) = &self.cxl_cache_write_back_and_invalidate_handler {
                handler.initiate_cache_write_back_and_invalidate();
            }
        }

        if self.capability.cxl_reset_capable() && new_reset && !old_reset {
            if let Some(handler) = &self.cxl_reset_handler {
                handler.initiate_cxl_reset();
            }
        }

        if !self.capability.cxl_reset_mem_clr_capable() {
            next = next.with_cxl_reset_mem_clr_enable(false);
        }
        if !self.capability.cache_writeback_and_invalidate_capable() {
            next = next.with_initiate_cache_writeback_and_invalidation(false);
        }
        if !self
            .capability3
            .volatile_hdm_state_after_hot_reset_configurability()
        {
            next = next.with_desired_volatile_hdm_state_after_hot_reset_configurable(false);
        }
        if !self.capability2.modified_completion_capable() {
            next = next.with_modified_completion_enable(false);
        }

        self.control2 = next
            .with_initiate_cache_writeback_and_invalidation(false)
            .with_initiate_cxl_reset(false);

        let mut clear_mask = ((value >> 16) as u16) & CXL_DEVICE_DVSEC_STATUS2_RW1C_MASK;
        if !self
            .capability3
            .volatile_hdm_state_after_hot_reset_configurability()
        {
            clear_mask &= !CXL_DEVICE_DVSEC_STATUS2_VOLATILE_HDM_PRESERVATION_ERROR_RW1C_MASK;
        }
        if clear_mask != 0 {
            let next_bits = self.status2.into_bits() & !clear_mask;
            self.status2 = CxlDeviceDvsecStatus2::from_bits(next_bits);
        }
    }
}

impl PciExtendedCapability for CxlDeviceDevsecExtendedCapability {
    fn label(&self) -> &str {
        "cxl_device_dvsec"
    }

    fn extended_capability_id(&self) -> u16 {
        ExtendedCapabilityId::DVSEC.0
    }

    fn capability_version(&self) -> u8 {
        1
    }

    fn len(&self) -> usize {
        self.dvsec_len()
    }

    fn read_u32(&self, offset: u16) -> u32 {
        if offset == 0 {
            u32::from(self.extended_capability_id()) | (u32::from(self.capability_version()) << 16)
        } else {
            self.read_dvsec_u32(offset)
        }
    }

    fn write_u32(&mut self, offset: u16, val: u32) {
        if offset != 0 {
            self.write_dvsec_u32(offset, val);
        }
    }

    fn reset(&mut self) {
        self.reset_state();
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "cxl.pci_registers.cxl_device_dvsec")]
        pub struct SavedState {
            #[mesh(1)]
            pub control: u32,
            #[mesh(2)]
            pub status: u32,
            #[mesh(3)]
            pub control2: u32,
            #[mesh(4)]
            pub status2: u32,
            #[mesh(5)]
            pub lock: u32,
            #[mesh(6)]
            pub capability: u32,
            #[mesh(7)]
            pub capability2: u32,
            #[mesh(8)]
            pub capability3: u32,
            #[mesh(9)]
            pub range1_size_high: u32,
            #[mesh(10)]
            pub range1_size_low: u32,
            #[mesh(11)]
            pub range1_base_high: u32,
            #[mesh(12)]
            pub range1_base_low: u32,
            #[mesh(13)]
            pub range2_size_high: u32,
            #[mesh(14)]
            pub range2_size_low: u32,
            #[mesh(15)]
            pub range2_base_high: u32,
            #[mesh(16)]
            pub range2_base_low: u32,
            #[mesh(17)]
            pub reset_baseline_capability: u32,
            #[mesh(18)]
            pub reset_baseline_capability2: u32,
            #[mesh(19)]
            pub reset_baseline_capability3: u32,
            #[mesh(20)]
            pub reset_baseline_range1_size_high: u32,
            #[mesh(21)]
            pub reset_baseline_range1_size_low: u32,
            #[mesh(22)]
            pub reset_baseline_range1_base_high: u32,
            #[mesh(23)]
            pub reset_baseline_range1_base_low: u32,
            #[mesh(24)]
            pub reset_baseline_range2_size_high: u32,
            #[mesh(25)]
            pub reset_baseline_range2_size_low: u32,
            #[mesh(26)]
            pub reset_baseline_range2_base_high: u32,
            #[mesh(27)]
            pub reset_baseline_range2_base_low: u32,
        }
    }

    impl SaveRestore for CxlDeviceDevsecExtendedCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                control: u32::from(self.control.into_bits()),
                status: u32::from(self.status.into_bits()),
                control2: u32::from(self.control2.into_bits()),
                status2: u32::from(self.status2.into_bits()),
                lock: u32::from(self.lock.into_bits()),
                capability: u32::from(self.capability.into_bits()),
                capability2: u32::from(self.capability2.into_bits()),
                capability3: u32::from(self.capability3.into_bits()),
                range1_size_high: self.range1_size_high,
                range1_size_low: self.range1_size_low,
                range1_base_high: self.range1_base_high,
                range1_base_low: self.range1_base_low.into_bits(),
                range2_size_high: self.range2_size_high,
                range2_size_low: self.range2_size_low,
                range2_base_high: self.range2_base_high,
                range2_base_low: self.range2_base_low.into_bits(),
                reset_baseline_capability: u32::from(self.reset_baseline_capability.into_bits()),
                reset_baseline_capability2: u32::from(self.reset_baseline_capability2.into_bits()),
                reset_baseline_capability3: u32::from(self.reset_baseline_capability3.into_bits()),
                reset_baseline_range1_size_high: self.reset_baseline_range1_size_high,
                reset_baseline_range1_size_low: self.reset_baseline_range1_size_low,
                reset_baseline_range1_base_high: self.reset_baseline_range1_base_high,
                reset_baseline_range1_base_low: self.reset_baseline_range1_base_low.into_bits(),
                reset_baseline_range2_size_high: self.reset_baseline_range2_size_high,
                reset_baseline_range2_size_low: self.reset_baseline_range2_size_low,
                reset_baseline_range2_base_high: self.reset_baseline_range2_base_high,
                reset_baseline_range2_base_low: self.reset_baseline_range2_base_low.into_bits(),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            self.control = CxlDeviceDvsecControl::from_bits(state.control as u16);
            self.status = CxlDeviceDvsecStatus::from_bits(state.status as u16);
            self.control2 = CxlDeviceDvsecControl2::from_bits(state.control2 as u16);
            self.status2 = CxlDeviceDvsecStatus2::from_bits(state.status2 as u16);
            self.lock = CxlDeviceDvsecLock::from_bits(state.lock as u16);
            self.capability = CxlDeviceDvsecCapability::from_bits(state.capability as u16);
            self.capability2 = CxlDeviceDvsecCapability2::from_bits(state.capability2 as u16);
            self.capability3 = CxlDeviceDvsecCapability3::from_bits(state.capability3 as u16);
            self.range1_size_high = state.range1_size_high;
            self.range1_size_low = state.range1_size_low;
            self.range1_base_high = state.range1_base_high;
            self.range1_base_low = CxlDeviceDvsecRangeBaseLow::from_bits(state.range1_base_low);
            self.range2_size_high = state.range2_size_high;
            self.range2_size_low = state.range2_size_low;
            self.range2_base_high = state.range2_base_high;
            self.range2_base_low = CxlDeviceDvsecRangeBaseLow::from_bits(state.range2_base_low);
            self.reset_baseline_capability =
                CxlDeviceDvsecCapability::from_bits(state.reset_baseline_capability as u16);
            self.reset_baseline_capability2 =
                CxlDeviceDvsecCapability2::from_bits(state.reset_baseline_capability2 as u16);
            self.reset_baseline_capability3 =
                CxlDeviceDvsecCapability3::from_bits(state.reset_baseline_capability3 as u16);
            self.reset_baseline_range1_size_high = state.reset_baseline_range1_size_high;
            self.reset_baseline_range1_size_low = state.reset_baseline_range1_size_low;
            self.reset_baseline_range1_base_high = state.reset_baseline_range1_base_high;
            self.reset_baseline_range1_base_low =
                CxlDeviceDvsecRangeBaseLow::from_bits(state.reset_baseline_range1_base_low);
            self.reset_baseline_range2_size_high = state.reset_baseline_range2_size_high;
            self.reset_baseline_range2_size_low = state.reset_baseline_range2_size_low;
            self.reset_baseline_range2_base_high = state.reset_baseline_range2_base_high;
            self.reset_baseline_range2_base_low =
                CxlDeviceDvsecRangeBaseLow::from_bits(state.reset_baseline_range2_base_low);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use inspect::Inspect;
    use pci_core::capabilities::extended::PciExtendedCapability;
    use pci_core::spec::caps::dvsec::DvsecExtendedCapabilityHeader;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use vmcore::save_restore::SaveRestore;

    use super::CxlCacheWriteBackAndInvalidateHandler;
    use super::CxlDeviceDevsecExtendedCapability;
    use super::CxlDeviceDvsecCacheSizeUnit;
    use super::CxlDeviceDvsecCapability;
    use super::CxlDeviceDvsecCapability2;
    use super::CxlDeviceDvsecControl;
    use super::CxlDeviceDvsecControl2;
    use super::CxlDeviceDvsecDesiredInterleave;
    use super::CxlDeviceDvsecMediaType;
    use super::CxlDeviceDvsecMemoryActiveTimeout;
    use super::CxlDeviceDvsecMemoryClass;
    use super::CxlDeviceDvsecRangeSizeLow;
    use super::CxlDeviceDvsecRegisterOffset;
    use super::CxlDeviceDvsecStatus2;
    use super::CxlMemoryConfigError;
    use super::CxlMemoryRange;
    use super::CxlResetHandler;

    static CXL_RESET_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static CXL_CWBI_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Inspect)]
    struct TestCxlResetHandler;

    impl CxlResetHandler for TestCxlResetHandler {
        fn initiate_cxl_reset(&self) {
            CXL_RESET_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Inspect)]
    struct TestCxlCacheWriteBackAndInvalidateHandler;

    impl CxlCacheWriteBackAndInvalidateHandler for TestCxlCacheWriteBackAndInvalidateHandler {
        fn initiate_cache_write_back_and_invalidate(&self) {
            CXL_CWBI_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn header_registers_match_required_constants() {
        let cap = CxlDeviceDevsecExtendedCapability::new(None, None);

        assert_eq!(
            cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER1.0),
            0x03c3_1e98
        );
        assert_eq!(
            cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER2.0) & 0xffff,
            0x0000
        );
    }

    #[test]
    fn label_is_cxl_dvsec() {
        let cap = CxlDeviceDevsecExtendedCapability::new(None, None);
        assert_eq!(cap.label(), "cxl_device_dvsec");
    }

    #[test]
    fn default_only_enables_cxl_io() {
        let cap = CxlDeviceDevsecExtendedCapability::new(None, None);
        let header2 = cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER2.0);
        let capability = CxlDeviceDvsecCapability::from_bits((header2 >> 16) as u16);

        assert!(capability.io_capable());
        assert!(!capability.cache_capable());
        assert!(!capability.mem_capable());
        assert_eq!(capability.hdm_count(), 0);
    }

    #[test]
    fn with_cxl_cache_sets_cache_capable() {
        let cap = CxlDeviceDevsecExtendedCapability::new(None, None)
            .with_cxl_cache(CxlDeviceDvsecCacheSizeUnit::Kib64, 0x20);
        let header2 = cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER2.0);
        let capability = CxlDeviceDvsecCapability::from_bits((header2 >> 16) as u16);
        let lock_cap2 = cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_LOCK_CAPABILITY2);
        let capability2 = CxlDeviceDvsecCapability2::from_bits((lock_cap2 >> 16) as u16);

        assert!(capability.cache_capable());
        assert_eq!(
            capability2.cache_size_unit(),
            CxlDeviceDvsecCacheSizeUnit::Kib64.bits()
        );
        assert_eq!(capability2.cache_size(), 0x20);
    }

    #[test]
    fn with_cxl_memory_enables_mem_for_valid_range() {
        let cap = CxlDeviceDevsecExtendedCapability::new(None, None)
            .with_cxl_memory(
                0x4000_0000,
                Some(CxlMemoryRange {
                    base: 0x8000_0000,
                    size: 0x4000_0000,
                }),
                CxlDeviceDvsecMediaType::Cdat,
                CxlDeviceDvsecMemoryClass::Cdat,
                CxlDeviceDvsecDesiredInterleave::Interleave4Kb,
                CxlDeviceDvsecMemoryActiveTimeout::Seconds16,
            )
            .expect("valid range should configure CXL.mem");

        let header2 = cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER2.0);
        let capability = CxlDeviceDvsecCapability::from_bits((header2 >> 16) as u16);

        assert!(capability.mem_capable());
        assert_eq!(capability.hdm_count(), 0x1);

        let size_low = CxlDeviceDvsecRangeSizeLow::from_bits(
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_SIZE_LOW),
        );
        assert!(size_low.memory_info_valid());
        assert!(size_low.memory_active());
        assert_eq!(size_low.media_type(), CxlDeviceDvsecMediaType::Cdat.bits());
        assert_eq!(
            size_low.memory_class(),
            CxlDeviceDvsecMemoryClass::Cdat.bits()
        );
        assert_eq!(
            size_low.desired_interleave(),
            CxlDeviceDvsecDesiredInterleave::Interleave4Kb.bits()
        );
        assert_eq!(
            size_low.memory_active_timeout(),
            CxlDeviceDvsecMemoryActiveTimeout::Seconds16.bits()
        );
        assert_eq!(size_low.memory_size_low(), 0x4);
    }

    #[test]
    fn reset_preserves_configured_mem_capability_and_ranges() {
        let mut cap = CxlDeviceDevsecExtendedCapability::new(None, None)
            .with_cxl_memory(
                0x4000_0000,
                None,
                CxlDeviceDvsecMediaType::VolatileMemory,
                CxlDeviceDvsecMemoryClass::Memory,
                CxlDeviceDvsecDesiredInterleave::NoInterleave,
                CxlDeviceDvsecMemoryActiveTimeout::Seconds1,
            )
            .expect("memory configuration should succeed");

        cap.write_u32(
            CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL_STATUS,
            u32::from(
                CxlDeviceDvsecControl::new()
                    .with_io_enable(true)
                    .with_mem_enable(true)
                    .into_bits(),
            ),
        );

        cap.reset();

        let header2 = cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER2.0);
        let capability = CxlDeviceDvsecCapability::from_bits((header2 >> 16) as u16);
        let control = CxlDeviceDvsecControl::from_bits(
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL_STATUS) as u16,
        );

        assert!(capability.mem_capable());
        assert_eq!(capability.hdm_count(), 0x1);
        assert!(!control.mem_enable());
        assert_eq!(
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_SIZE_LOW),
            0x4000_0003
        );
    }

    #[test]
    fn with_cxl_memory_ignores_invalid_range_programming() {
        let cap = CxlDeviceDevsecExtendedCapability::new(None, None)
            .with_cxl_memory(
                0x4000_0000,
                Some(CxlMemoryRange {
                    base: 0x8000_1000,
                    size: 0x4000_0000,
                }),
                CxlDeviceDvsecMediaType::VolatileMemory,
                CxlDeviceDvsecMemoryClass::Memory,
                CxlDeviceDvsecDesiredInterleave::NoInterleave,
                CxlDeviceDvsecMemoryActiveTimeout::Seconds1,
            )
            .expect("invalid optional range should not fail CXL.mem enable");

        let header2 = cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER2.0);
        let capability = CxlDeviceDvsecCapability::from_bits((header2 >> 16) as u16);

        assert!(capability.mem_capable());
        assert_eq!(
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_HIGH),
            0
        );
    }

    #[test]
    fn with_cxl_memory_enables_mem_without_range() {
        let cap = CxlDeviceDevsecExtendedCapability::new(None, None)
            .with_cxl_memory(
                0x4000_0000,
                None,
                CxlDeviceDvsecMediaType::NonVolatileMemory,
                CxlDeviceDvsecMemoryClass::Storage,
                CxlDeviceDvsecDesiredInterleave::Bytes1024,
                CxlDeviceDvsecMemoryActiveTimeout::Seconds4,
            )
            .expect("range-less CXL.mem should configure successfully");

        let header2 = cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER2.0);
        let capability = CxlDeviceDvsecCapability::from_bits((header2 >> 16) as u16);

        assert!(capability.mem_capable());
        assert_eq!(capability.hdm_count(), 0x1);
    }

    #[test]
    fn with_cxl_memory_second_call_programs_range2() {
        let cap = CxlDeviceDevsecExtendedCapability::new(None, None)
            .with_cxl_memory(
                0x4000_0000,
                Some(CxlMemoryRange {
                    base: 0x8000_0000,
                    size: 0x4000_0000,
                }),
                CxlDeviceDvsecMediaType::Cdat,
                CxlDeviceDvsecMemoryClass::Cdat,
                CxlDeviceDvsecDesiredInterleave::Interleave4Kb,
                CxlDeviceDvsecMemoryActiveTimeout::Seconds16,
            )
            .expect("first range should configure")
            .with_cxl_memory(
                0x4000_0000,
                Some(CxlMemoryRange {
                    base: 0x1_0000_0000,
                    size: 0x4000_0000,
                }),
                CxlDeviceDvsecMediaType::Cdat,
                CxlDeviceDvsecMemoryClass::Cdat,
                CxlDeviceDvsecDesiredInterleave::Interleave4Kb,
                CxlDeviceDvsecMemoryActiveTimeout::Seconds16,
            )
            .expect("second range should configure");

        let header2 = cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER2.0);
        let capability = CxlDeviceDvsecCapability::from_bits((header2 >> 16) as u16);

        assert!(capability.mem_capable());
        assert_eq!(capability.hdm_count(), 0x2);
        assert_ne!(
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE2_SIZE_LOW),
            0
        );
    }

    #[test]
    fn with_cxl_memory_on_both_ranges_sets_hdm_count_two() {
        let cap = CxlDeviceDevsecExtendedCapability::new(None, None)
            .with_cxl_memory(
                0x4000_0000,
                None,
                CxlDeviceDvsecMediaType::VolatileMemory,
                CxlDeviceDvsecMemoryClass::Memory,
                CxlDeviceDvsecDesiredInterleave::NoInterleave,
                CxlDeviceDvsecMemoryActiveTimeout::Seconds1,
            )
            .expect("first range should configure")
            .with_cxl_memory(
                0x4000_0000,
                None,
                CxlDeviceDvsecMediaType::NonVolatileMemory,
                CxlDeviceDvsecMemoryClass::Storage,
                CxlDeviceDvsecDesiredInterleave::Bytes1024,
                CxlDeviceDvsecMemoryActiveTimeout::Seconds4,
            )
            .expect("second range should configure");

        let header2 = cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER2.0);
        let capability = CxlDeviceDvsecCapability::from_bits((header2 >> 16) as u16);

        assert!(capability.mem_capable());
        assert_eq!(capability.hdm_count(), 0x2);
    }

    #[test]
    fn with_cxl_memory_third_call_fails() {
        let cap = CxlDeviceDevsecExtendedCapability::new(None, None)
            .with_cxl_memory(
                0x4000_0000,
                None,
                CxlDeviceDvsecMediaType::Cdat,
                CxlDeviceDvsecMemoryClass::Cdat,
                CxlDeviceDvsecDesiredInterleave::Interleave4Kb,
                CxlDeviceDvsecMemoryActiveTimeout::Seconds16,
            )
            .expect("first range should configure")
            .with_cxl_memory(
                0x4000_0000,
                None,
                CxlDeviceDvsecMediaType::Cdat,
                CxlDeviceDvsecMemoryClass::Cdat,
                CxlDeviceDvsecDesiredInterleave::Interleave4Kb,
                CxlDeviceDvsecMemoryActiveTimeout::Seconds16,
            )
            .expect("second range should configure");

        let result = cap.with_cxl_memory(
            0x4000_0000,
            None,
            CxlDeviceDvsecMediaType::Cdat,
            CxlDeviceDvsecMemoryClass::Cdat,
            CxlDeviceDvsecDesiredInterleave::Interleave4Kb,
            CxlDeviceDvsecMemoryActiveTimeout::Seconds16,
        );

        assert!(matches!(
            result,
            Err(CxlMemoryConfigError::TooManyHdmRanges)
        ));
    }

    #[test]
    fn config_lock_blocks_rw_fields() {
        let mut cap = CxlDeviceDevsecExtendedCapability::new(None, None);

        cap.write_u32(
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_HIGH,
            0xaaaa_5555,
        );
        assert_eq!(
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_HIGH),
            0xaaaa_5555
        );

        cap.write_u32(CxlDeviceDvsecRegisterOffset::DVSEC_LOCK_CAPABILITY2, 0x1);
        cap.write_u32(
            CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_HIGH,
            0x1234_5678,
        );

        assert_eq!(
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_HIGH),
            0xaaaa_5555
        );
    }

    #[test]
    fn config_lock_does_not_block_control2_rw_fields() {
        let mut cap = CxlDeviceDevsecExtendedCapability::new(None, None);

        cap.write_u32(CxlDeviceDvsecRegisterOffset::DVSEC_LOCK_CAPABILITY2, 0x1);
        cap.write_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2, 1 << 0);

        let control2 = CxlDeviceDvsecControl2::from_bits(
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2) as u16,
        );
        assert!(control2.disable_caching());
    }

    #[test]
    fn control_enables_require_corresponding_capabilities() {
        let mut cap = CxlDeviceDevsecExtendedCapability::new(None, None);

        let value = u32::from(
            CxlDeviceDvsecControl::new()
                .with_cache_enable(true)
                .with_mem_enable(true)
                .with_viral_enable(true)
                .into_bits(),
        );

        cap.write_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL_STATUS, value);
        let control = CxlDeviceDvsecControl::from_bits(
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL_STATUS) as u16,
        );

        assert!(!control.cache_enable());
        assert!(!control.mem_enable());
        assert!(!control.viral_enable());
    }

    #[test]
    fn io_enable_is_always_set() {
        let mut cap = CxlDeviceDevsecExtendedCapability::new(None, None);

        let default_control = CxlDeviceDvsecControl::from_bits(
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL_STATUS) as u16,
        );
        assert!(default_control.io_enable());

        // Attempt to clear io_enable; implementation must keep it enabled.
        cap.write_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL_STATUS, 0);
        let control_after_write = CxlDeviceDvsecControl::from_bits(
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL_STATUS) as u16,
        );
        assert!(control_after_write.io_enable());
    }

    #[test]
    fn control2_reset_request_calls_handler() {
        CXL_RESET_HANDLER_CALLS.store(0, Ordering::SeqCst);

        let mut cap =
            CxlDeviceDevsecExtendedCapability::new(Some(Arc::new(TestCxlResetHandler)), None);
        cap.write_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2, 1 << 2);

        assert_eq!(CXL_RESET_HANDLER_CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn control2_reset_request_does_not_call_handler_when_not_capable() {
        CXL_RESET_HANDLER_CALLS.store(0, Ordering::SeqCst);

        let mut cap =
            CxlDeviceDevsecExtendedCapability::new(Some(Arc::new(TestCxlResetHandler)), None);
        cap.capability = cap.capability.with_cxl_reset_capable(false);
        cap.write_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2, 1 << 2);

        assert_eq!(CXL_RESET_HANDLER_CALLS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn control2_cwbi_request_calls_handler_when_capable() {
        CXL_CWBI_HANDLER_CALLS.store(0, Ordering::SeqCst);

        let mut cap = CxlDeviceDevsecExtendedCapability::new(
            None,
            Some(Arc::new(TestCxlCacheWriteBackAndInvalidateHandler)),
        )
        .with_cxl_cache(CxlDeviceDvsecCacheSizeUnit::Kib64, 0x1);

        cap.write_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2, 1 << 1);

        assert_eq!(CXL_CWBI_HANDLER_CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn control2_cwbi_request_does_not_call_handler_when_not_capable() {
        CXL_CWBI_HANDLER_CALLS.store(0, Ordering::SeqCst);

        let mut cap = CxlDeviceDevsecExtendedCapability::new(
            None,
            Some(Arc::new(TestCxlCacheWriteBackAndInvalidateHandler)),
        );
        cap.write_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2, 1 << 1);

        assert_eq!(CXL_CWBI_HANDLER_CALLS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn status2_volatile_hdm_preservation_error_clear_requires_capability3_configurable() {
        let mut cap = CxlDeviceDevsecExtendedCapability::new(None, None);
        cap.status2 = cap.status2.with_volatile_hdm_preservation_error(true);

        // Capability is false by default, so RW1C clear should be ignored.
        cap.write_u32(
            CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2,
            1 << (16 + 3),
        );
        assert!(cap.status2.volatile_hdm_preservation_error());

        // Once the capability is set, the same RW1C write clears the status bit.
        cap.capability3 = cap
            .capability3
            .with_volatile_hdm_state_after_hot_reset_configurability(true);
        cap.write_u32(
            CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2,
            1 << (16 + 3),
        );
        assert!(!cap.status2.volatile_hdm_preservation_error());
    }

    #[test]
    fn status2_clear_bit_only_targets_rw1c_field() {
        let mut cap = CxlDeviceDevsecExtendedCapability::new(None, None);
        cap.capability3 = cap
            .capability3
            .with_volatile_hdm_state_after_hot_reset_configurability(true);
        cap.status2 = CxlDeviceDvsecStatus2::new()
            .with_volatile_hdm_preservation_error(true)
            .with_cxl_reset_error(true)
            .with_cxl_reset_complete(true);

        cap.write_u32(
            CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2,
            1 << (16 + 3),
        );

        assert!(!cap.status2.volatile_hdm_preservation_error());
        assert!(cap.status2.cxl_reset_error());
        assert!(cap.status2.cxl_reset_complete());
    }

    #[test]
    fn save_restore_round_trips_state() {
        let mut cap = CxlDeviceDevsecExtendedCapability::new(None, None)
            .with_cxl_cache(CxlDeviceDvsecCacheSizeUnit::Kib64, 0x20)
            .with_cxl_memory(
                0x4000_0000,
                Some(CxlMemoryRange {
                    base: 0x8000_0000,
                    size: 0x4000_0000,
                }),
                CxlDeviceDvsecMediaType::Cdat,
                CxlDeviceDvsecMemoryClass::Cdat,
                CxlDeviceDvsecDesiredInterleave::Interleave4Kb,
                CxlDeviceDvsecMemoryActiveTimeout::Seconds16,
            )
            .expect("valid range should configure CXL.mem");

        cap.write_u32(
            CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL_STATUS,
            u32::from(
                CxlDeviceDvsecControl::new()
                    .with_cache_enable(true)
                    .with_mem_enable(true)
                    .with_viral_enable(true)
                    .into_bits(),
            ),
        );
        cap.write_u32(CxlDeviceDvsecRegisterOffset::DVSEC_LOCK_CAPABILITY2, 0x1);

        let saved = cap.save().expect("save should succeed");
        let mut restored = CxlDeviceDevsecExtendedCapability::new(None, None);
        restored.restore(saved).expect("restore should succeed");

        assert_eq!(
            restored.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER2.0),
            cap.read_u32(DvsecExtendedCapabilityHeader::DVSEC_HEADER2.0)
        );
        assert_eq!(
            restored.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL_STATUS),
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL_STATUS)
        );
        assert_eq!(
            restored.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2),
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CONTROL2_STATUS2)
        );
        assert_eq!(
            restored.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_LOCK_CAPABILITY2),
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_LOCK_CAPABILITY2)
        );
        assert_eq!(
            restored.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_SIZE_HIGH),
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_SIZE_HIGH)
        );
        assert_eq!(
            restored.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_SIZE_LOW),
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_SIZE_LOW)
        );
        assert_eq!(
            restored.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_HIGH),
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_HIGH)
        );
        assert_eq!(
            restored.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_LOW),
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_RANGE1_BASE_LOW)
        );
        assert_eq!(
            restored.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CAPABILITY3),
            cap.read_u32(CxlDeviceDvsecRegisterOffset::DVSEC_CAPABILITY3)
        );
    }
}
