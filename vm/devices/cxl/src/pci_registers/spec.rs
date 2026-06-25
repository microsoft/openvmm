// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL PCIe DVSEC register definitions.

/// Common CXL DVSEC vendor ID.
pub const CXL_DVSEC_VENDOR_ID: u16 = 0x1e98;

/// CXL Designated Vendor-Specific Extended Capability (DVSEC).
#[expect(missing_docs)] // keep parity with grouped spec modules without forcing doc lint churn
pub mod cxl_device_dvsec {
    use bitfield_struct::bitfield;
    use inspect::Inspect;
    use std::sync::Arc;

    /// Callback interface for handling CXL reset requests.
    pub trait CxlResetHandler: Send + Sync + Inspect {
        /// Called when a new CXL reset request is initiated.
        fn initiate_cxl_reset(&self);
    }

    /// Callback interface for handling cache writeback+invalidate requests.
    pub trait CxlCacheWriteBackAndInvalidateHandler: Send + Sync + Inspect {
        /// Called when cache writeback+invalidate is initiated.
        fn initiate_cache_write_back_and_invalidate(&self);
    }

    /// Media_Type encodings for DVSEC CXL Range Size Low.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub enum CxlDeviceDvsecMediaType {
        /// 000b = Volatile memory.
        VolatileMemory,
        /// 001b = Non-volatile memory.
        NonVolatileMemory,
        /// 010b = Memory characteristics via CDAT.
        Cdat,
    }

    impl CxlDeviceDvsecMediaType {
        pub const fn bits(self) -> u8 {
            match self {
                Self::VolatileMemory => 0b000,
                Self::NonVolatileMemory => 0b001,
                Self::Cdat => 0b010,
            }
        }
    }

    /// Memory_Class encodings for DVSEC CXL Range Size Low.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub enum CxlDeviceDvsecMemoryClass {
        /// 000b = Memory class (e.g. DRAM).
        Memory,
        /// 001b = Storage class.
        Storage,
        /// 010b = Characteristics via CDAT.
        Cdat,
    }

    impl CxlDeviceDvsecMemoryClass {
        pub const fn bits(self) -> u8 {
            match self {
                Self::Memory => 0b000,
                Self::Storage => 0b001,
                Self::Cdat => 0b010,
            }
        }
    }

    /// Desired_Interleave encodings for DVSEC CXL Range Size Low.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub enum CxlDeviceDvsecDesiredInterleave {
        /// 00h = No Interleave.
        NoInterleave,
        /// 01h = 256-byte granularity.
        Granularity256Bytes,
        /// 02h = 4-KB interleave.
        Interleave4Kb,
        /// 03h = 512 bytes.
        Bytes512,
        /// 04h = 1024 bytes.
        Bytes1024,
        /// 05h = 2048 bytes.
        Bytes2048,
        /// 06h = 8192 bytes.
        Bytes8192,
        /// 07h = 16384 bytes.
        Bytes16384,
    }

    impl CxlDeviceDvsecDesiredInterleave {
        pub const fn bits(self) -> u8 {
            match self {
                Self::NoInterleave => 0x00,
                Self::Granularity256Bytes => 0x01,
                Self::Interleave4Kb => 0x02,
                Self::Bytes512 => 0x03,
                Self::Bytes1024 => 0x04,
                Self::Bytes2048 => 0x05,
                Self::Bytes8192 => 0x06,
                Self::Bytes16384 => 0x07,
            }
        }
    }

    /// Memory_Active_Timeout encodings for DVSEC CXL Range Size Low.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub enum CxlDeviceDvsecMemoryActiveTimeout {
        /// 000b = 1 second.
        Seconds1,
        /// 001b = 4 seconds.
        Seconds4,
        /// 010b = 16 seconds.
        Seconds16,
        /// 011b = 64 seconds.
        Seconds64,
        /// 100b = 256 seconds.
        Seconds256,
    }

    impl CxlDeviceDvsecMemoryActiveTimeout {
        pub const fn bits(self) -> u8 {
            match self {
                Self::Seconds1 => 0b000,
                Self::Seconds4 => 0b001,
                Self::Seconds16 => 0b010,
                Self::Seconds64 => 0b011,
                Self::Seconds256 => 0b100,
            }
        }
    }

    /// CXL reset timeout encodings for DVSEC CXL Capability.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub enum CxlDeviceDvsecResetTimeout {
        /// 000b = 10 ms.
        Milliseconds10,
        /// 001b = 100 ms.
        Milliseconds100,
        /// 010b = 1 second.
        Seconds1,
        /// 011b = 10 seconds.
        Seconds10,
        /// 100b = 100 seconds.
        Seconds100,
    }

    impl CxlDeviceDvsecResetTimeout {
        pub const fn bits(self) -> u8 {
            match self {
                Self::Milliseconds10 => 0b000,
                Self::Milliseconds100 => 0b001,
                Self::Seconds1 => 0b010,
                Self::Seconds10 => 0b011,
                Self::Seconds100 => 0b100,
            }
        }
    }

    /// Cache size unit encodings for DVSEC CXL Capability2.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub enum CxlDeviceDvsecCacheSizeUnit {
        /// 0h = Cache size is not reported.
        NotReported,
        /// 1h = 64 KB.
        Kib64,
        /// 2h = 1 MB.
        Mib1,
    }

    impl CxlDeviceDvsecCacheSizeUnit {
        pub const fn bits(self) -> u8 {
            match self {
                Self::NotReported => 0x0,
                Self::Kib64 => 0x1,
                Self::Mib1 => 0x2,
            }
        }
    }

    open_enum::open_enum! {
        /// Dword offsets in the CXL DVSEC register block.
        ///
        /// The PCIe Extended Capability Header at offset `0x00` is managed by the
        /// caller. DVSEC Header1 (`0x04`) and DVSEC Header2 (`0x08`) are defined in
        /// `pci_core::spec::caps::dvsec::DvsecExtendedCapabilityHeader`.
        ///
        /// This table starts at the first CXL-specific packed dword (`0x0C`).
        pub enum CxlDeviceDvsecRegisterOffset: u16 {
            /// Packed CXL Control + CXL Status dword offset.
            DVSEC_CONTROL_STATUS = 0x0c,
            /// Packed CXL Control2 + CXL Status2 dword offset.
            DVSEC_CONTROL2_STATUS2 = 0x10,
            /// Packed CXL Lock + CXL Capability2 dword offset.
            DVSEC_LOCK_CAPABILITY2 = 0x14,
            /// CXL Range1 Size High dword offset.
            DVSEC_RANGE1_SIZE_HIGH = 0x18,
            /// CXL Range1 Size Low dword offset.
            DVSEC_RANGE1_SIZE_LOW = 0x1c,
            /// CXL Range1 Base High dword offset.
            DVSEC_RANGE1_BASE_HIGH = 0x20,
            /// CXL Range1 Base Low dword offset.
            DVSEC_RANGE1_BASE_LOW = 0x24,
            /// CXL Range2 Size High dword offset.
            DVSEC_RANGE2_SIZE_HIGH = 0x28,
            /// CXL Range2 Size Low dword offset.
            DVSEC_RANGE2_SIZE_LOW = 0x2c,
            /// CXL Range2 Base High dword offset.
            DVSEC_RANGE2_BASE_HIGH = 0x30,
            /// CXL Range2 Base Low dword offset.
            DVSEC_RANGE2_BASE_LOW = 0x34,
            /// CXL Capability3 dword offset.
            DVSEC_CAPABILITY3 = 0x38,
        }
    }

    /// CXL DVSEC revision.
    pub const CXL_DEVICE_DVSEC_REVISION: u8 = 0x3;
    /// CXL DVSEC structure length in bytes.
    pub const CXL_DEVICE_DVSEC_LENGTH: u16 = 0x3c;
    /// CXL DVSEC ID value.
    pub const CXL_DEVICE_DVSEC_ID: u16 = 0x0000;

    /// Writable mask for the DVSEC CXL Control register.
    ///
    /// Defined from the writable fields in `CxlDeviceDvsecControl` for readability.
    pub const CXL_DEVICE_DVSEC_CONTROL_WRITABLE_MASK: u16 = CxlDeviceDvsecControl::new()
        .with_cache_enable(true)
        .with_mem_enable(true)
        .with_cache_sf_coverage(0x1f)
        .with_cache_sf_granularity(0x07)
        .with_cache_clean_eviction(true)
        .with_direct_p2p_mem_enable(true)
        .with_viral_enable(true)
        .into_bits();

    /// Writable mask for the DVSEC CXL Control2 register.
    ///
    /// Defined from the writable fields in `CxlDeviceDvsecControl2` for readability.
    pub const CXL_DEVICE_DVSEC_CONTROL2_WRITABLE_MASK: u16 = CxlDeviceDvsecControl2::new()
        .with_disable_caching(true)
        .with_initiate_cache_writeback_and_invalidation(true)
        .with_initiate_cxl_reset(true)
        .with_cxl_reset_mem_clr_enable(true)
        .with_desired_volatile_hdm_state_after_hot_reset_configurable(true)
        .with_modified_completion_enable(true)
        .into_bits();

    /// Writable mask for the DVSEC CXL Range Base Low register.
    ///
    /// Defined from the writable fields in `CxlDeviceDvsecRangeBaseLow` for readability.
    pub const CXL_DEVICE_DVSEC_RANGE_BASE_LOW_WRITABLE_MASK: u32 =
        CxlDeviceDvsecRangeBaseLow::new()
            .with_memory_base_low(0x0f)
            .into_bits();

    /// RW1C mask for the DVSEC CXL Status register.
    ///
    /// Defined from the RW1C fields in `CxlDeviceDvsecStatus` for readability.
    pub const CXL_DEVICE_DVSEC_STATUS_RW1C_MASK: u16 = CxlDeviceDvsecStatus::new()
        .with_viral_status(true)
        .into_bits();

    /// RW1C mask for the DVSEC CXL Status2 register.
    ///
    /// Defined from the RW1C fields in `CxlDeviceDvsecStatus2` for readability.
    pub const CXL_DEVICE_DVSEC_STATUS2_RW1C_MASK: u16 = CxlDeviceDvsecStatus2::new()
        .with_volatile_hdm_preservation_error(true)
        .into_bits();

    /// RW1C mask for DVSEC Status2 `volatile_hdm_preservation_error`.
    pub const CXL_DEVICE_DVSEC_STATUS2_VOLATILE_HDM_PRESERVATION_ERROR_RW1C_MASK: u16 =
        CxlDeviceDvsecStatus2::new()
            .with_volatile_hdm_preservation_error(true)
            .into_bits();

    /// DVSEC CXL Capability register (offset `0x0A`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlDeviceDvsecCapability {
        pub cache_capable: bool,
        pub io_capable: bool,
        pub mem_capable: bool,
        pub mem_hwinit_mode: bool,
        #[bits(2)]
        pub hdm_count: u8,
        pub cache_writeback_and_invalidate_capable: bool,
        pub cxl_reset_capable: bool,
        /// Encoded using `CxlDeviceDvsecResetTimeout::bits()`.
        #[bits(3)]
        pub cxl_reset_timeout: u8,
        pub cxl_reset_mem_clr_capable: bool,
        pub tsp_capable: bool,
        pub multiple_logical_device: bool,
        pub viral_capable: bool,
        pub pm_init_completion_reporting_capable: bool,
    }

    /// DVSEC CXL Control register (offset `0x0C`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlDeviceDvsecControl {
        pub cache_enable: bool,
        pub io_enable: bool,
        pub mem_enable: bool,
        #[bits(5)]
        pub cache_sf_coverage: u8,
        #[bits(3)]
        pub cache_sf_granularity: u8,
        pub cache_clean_eviction: bool,
        pub direct_p2p_mem_enable: bool,
        #[bits(1)]
        _reserved0: u8,
        pub viral_enable: bool,
        #[bits(1)]
        _reserved1: u8,
    }

    /// DVSEC CXL Status register (offset `0x0E`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlDeviceDvsecStatus {
        #[bits(14)]
        _reserved0: u16,
        pub viral_status: bool,
        _reserved1: bool,
    }

    /// DVSEC CXL Control2 register (offset `0x10`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlDeviceDvsecControl2 {
        pub disable_caching: bool,
        pub initiate_cache_writeback_and_invalidation: bool,
        pub initiate_cxl_reset: bool,
        pub cxl_reset_mem_clr_enable: bool,
        pub desired_volatile_hdm_state_after_hot_reset_configurable: bool,
        pub modified_completion_enable: bool,
        #[bits(10)]
        _reserved: u16,
    }

    /// DVSEC CXL Status2 register (offset `0x12`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlDeviceDvsecStatus2 {
        pub cache_invalid: bool,
        pub cxl_reset_complete: bool,
        pub cxl_reset_error: bool,
        pub volatile_hdm_preservation_error: bool,
        #[bits(11)]
        _reserved: u16,
        pub power_management_initialization_complete: bool,
    }

    /// DVSEC CXL Lock register (offset `0x14`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlDeviceDvsecLock {
        pub config_lock: bool,
        #[bits(15)]
        _reserved: u16,
    }

    /// DVSEC CXL Capability2 register (offset `0x16`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlDeviceDvsecCapability2 {
        #[bits(4)]
        pub cache_size_unit: u8,
        #[bits(2)]
        pub fallback_capability: u8,
        pub modified_completion_capable: bool,
        pub no_clean_writeback: bool,
        #[bits(8)]
        pub cache_size: u8,
    }

    /// DVSEC CXL Range Size Low register layout (offset `0x1C`/`0x2C`).
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlDeviceDvsecRangeSizeLow {
        pub memory_info_valid: bool,
        pub memory_active: bool,
        #[bits(3)]
        pub media_type: u8,
        #[bits(3)]
        pub memory_class: u8,
        #[bits(5)]
        pub desired_interleave: u8,
        #[bits(3)]
        pub memory_active_timeout: u8,
        pub memory_active_degraded: bool,
        #[bits(11)]
        _reserved: u16,
        #[bits(4)]
        pub memory_size_low: u8,
    }

    /// DVSEC CXL Range Base Low register layout (offset `0x24`/`0x34`).
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlDeviceDvsecRangeBaseLow {
        #[bits(28)]
        _reserved: u32,
        #[bits(4)]
        pub memory_base_low: u8,
    }

    /// DVSEC CXL Capability3 register (offset `0x38`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlDeviceDvsecCapability3 {
        pub default_volatile_hdm_state_after_cold_reset: bool,
        pub default_volatile_hdm_state_after_warm_reset: bool,
        pub default_volatile_hdm_state_after_hot_reset: bool,
        pub volatile_hdm_state_after_hot_reset_configurability: bool,
        pub direct_p2p_mem_capable: bool,
        #[bits(11)]
        _reserved: u16,
    }

    /// CXL PCIe Designated Vendor-Specific Extended Capability (DVSEC).
    #[derive(Clone, Inspect)]
    pub struct CxlDeviceDevsecExtendedCapability {
        pub(crate) control: CxlDeviceDvsecControl,
        pub(crate) status: CxlDeviceDvsecStatus,
        pub(crate) control2: CxlDeviceDvsecControl2,
        pub(crate) status2: CxlDeviceDvsecStatus2,
        pub(crate) lock: CxlDeviceDvsecLock,
        pub(crate) capability: CxlDeviceDvsecCapability,
        pub(crate) capability2: CxlDeviceDvsecCapability2,
        pub(crate) capability3: CxlDeviceDvsecCapability3,
        pub(crate) range1_size_high: u32,
        pub(crate) range1_size_low: u32,
        pub(crate) range1_base_high: u32,
        pub(crate) range1_base_low: CxlDeviceDvsecRangeBaseLow,
        pub(crate) range2_size_high: u32,
        pub(crate) range2_size_low: u32,
        pub(crate) range2_base_high: u32,
        pub(crate) range2_base_low: CxlDeviceDvsecRangeBaseLow,
        pub(crate) cxl_reset_handler: Option<Arc<dyn CxlResetHandler>>,
        pub(crate) cxl_cache_write_back_and_invalidate_handler:
            Option<Arc<dyn CxlCacheWriteBackAndInvalidateHandler>>,
        pub(crate) reset_baseline_capability: CxlDeviceDvsecCapability,
        pub(crate) reset_baseline_capability2: CxlDeviceDvsecCapability2,
        pub(crate) reset_baseline_capability3: CxlDeviceDvsecCapability3,
        pub(crate) reset_baseline_range1_size_high: u32,
        pub(crate) reset_baseline_range1_size_low: u32,
        pub(crate) reset_baseline_range1_base_high: u32,
        pub(crate) reset_baseline_range1_base_low: CxlDeviceDvsecRangeBaseLow,
        pub(crate) reset_baseline_range2_size_high: u32,
        pub(crate) reset_baseline_range2_size_low: u32,
        pub(crate) reset_baseline_range2_base_high: u32,
        pub(crate) reset_baseline_range2_base_low: CxlDeviceDvsecRangeBaseLow,
    }
}

/// CXL Port Designated Vendor-Specific Extended Capability (DVSEC).
pub mod cxl_port_dvsec {
    use bitfield_struct::bitfield;
    use inspect::Inspect;

    open_enum::open_enum! {
        /// Dword offsets in the CXL Port DVSEC register block.
        ///
        /// The PCIe Extended Capability Header at offset `0x00` is managed by the
        /// caller. DVSEC Header1 (`0x04`) and DVSEC Header2 (`0x08`) are defined in
        /// `pci_core::spec::caps::dvsec::DvsecExtendedCapabilityHeader`.
        pub enum CxlPortDvsecRegisterOffset : u16 {
            /// Packed DVSEC Header2 + CXL Port Extension Status dword offset.
            DVSEC_HEADER2_PORT_EXTENSION_STATUS = 0x08,
            /// Packed CXL Port Control Extensions + Alternate Bus Base/Limit dword offset.
            DVSEC_PORT_CONTROL_EXTENSIONS_ALT_BUS_BASE_LIMIT = 0x0c,
            /// Packed Alternate Memory Base + Alternate Memory Limit dword offset.
            DVSEC_ALT_MEMORY_BASE_LIMIT = 0x10,
            /// Packed Alternate Prefetchable Memory Base + Limit dword offset.
            DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT = 0x14,
            /// Alternate Prefetchable Memory Base High dword offset.
            DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_HIGH = 0x18,
            /// Alternate Prefetchable Memory Limit High dword offset.
            DVSEC_ALT_PREFETCHABLE_MEMORY_LIMIT_HIGH = 0x1c,
            /// CXL RCRB Base dword offset.
            DVSEC_CXL_RCRB_BASE = 0x20,
            /// CXL RCRB Base High dword offset.
            DVSEC_CXL_RCRB_BASE_HIGH = 0x24,
        }
    }

    /// CXL Port DVSEC revision.
    pub const CXL_PORT_DVSEC_REVISION: u8 = 0x0;
    /// CXL Port DVSEC structure length in bytes.
    pub const CXL_PORT_DVSEC_LENGTH: u16 = 0x28;
    /// CXL Port DVSEC ID value.
    pub const CXL_PORT_DVSEC_ID: u16 = 0x0003;

    /// Writable mask for the CXL Port Control Extensions register.
    pub const CXL_PORT_DVSEC_CONTROL_WRITABLE_MASK: u16 = CxlPortDvsecControl::new()
        .with_unmask_sbr(true)
        .with_unmask_link_disable(true)
        .with_alt_memory_and_id_space_enable(true)
        .with_alt_bme(true)
        .with_uio_to_hdm_enable(true)
        .with_viral_enable(true)
        .into_bits();

    /// RW1C mask for the CXL Port Extension Status register.
    pub const CXL_PORT_DVSEC_STATUS_RW1C_MASK: u16 = CxlPortDvsecStatus::new()
        .with_viral_status(true)
        .into_bits();

    /// Writable mask for Alternate Memory Base and Limit low bits.
    pub const CXL_PORT_DVSEC_ALT_MEMORY_BASE_LIMIT_WRITABLE_MASK: u16 =
        CxlPortDvsecAltMemoryBase::new()
            .with_alt_mem_base(0x0fff)
            .into_bits();

    /// Writable mask for Alternate Prefetchable Memory Base and Limit low bits.
    pub const CXL_PORT_DVSEC_ALT_PREFETCHABLE_MEMORY_BASE_LIMIT_WRITABLE_MASK: u16 =
        CxlPortDvsecAltPrefetchableMemoryBase::new()
            .with_alt_prefetch_mem_base(0x0fff)
            .into_bits();

    /// Writable mask for CXL RCRB Base.
    pub const CXL_PORT_DVSEC_CXL_RCRB_BASE_WRITABLE_MASK: u32 = CxlPortDvsecRcrbBase::new()
        .with_cxl_rcrb_enable(true)
        .with_cxl_rcrb_base_address_low(0x7ffff)
        .into_bits();

    /// CXL Port Extension Status register (offset `0x0A`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlPortDvsecStatus {
        pub port_power_management_initialization_complete: bool,
        #[bits(13)]
        _reserved0: u16,
        pub viral_status: bool,
        _reserved1: bool,
    }

    /// CXL Port Control Extensions register (offset `0x0C`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlPortDvsecControl {
        pub unmask_sbr: bool,
        pub unmask_link_disable: bool,
        pub alt_memory_and_id_space_enable: bool,
        pub alt_bme: bool,
        pub uio_to_hdm_enable: bool,
        #[bits(9)]
        _reserved0: u16,
        pub viral_enable: bool,
        _reserved1: bool,
    }

    /// Alternate Memory Base register (offset `0x10`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlPortDvsecAltMemoryBase {
        #[bits(4)]
        _reserved: u8,
        #[bits(12)]
        pub alt_mem_base: u16,
    }

    /// Alternate Memory Limit register (offset `0x12`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlPortDvsecAltMemoryLimit {
        #[bits(4)]
        _reserved: u8,
        #[bits(12)]
        pub alt_mem_limit: u16,
    }

    /// Alternate Prefetchable Memory Base register (offset `0x14`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlPortDvsecAltPrefetchableMemoryBase {
        #[bits(4)]
        _reserved: u8,
        #[bits(12)]
        pub alt_prefetch_mem_base: u16,
    }

    /// Alternate Prefetchable Memory Limit register (offset `0x16`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlPortDvsecAltPrefetchableMemoryLimit {
        #[bits(4)]
        _reserved: u8,
        #[bits(12)]
        pub alt_prefetch_mem_limit: u16,
    }

    /// CXL RCRB Base register (offset `0x20`).
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlPortDvsecRcrbBase {
        pub cxl_rcrb_enable: bool,
        #[bits(12)]
        _reserved: u16,
        #[bits(19)]
        pub cxl_rcrb_base_address_low: u32,
    }

    /// CXL Port PCIe Designated Vendor-Specific Extended Capability (DVSEC).
    #[derive(Clone, Inspect)]
    pub struct CxlPortDvsecExtendedCapability {
        pub(crate) status: CxlPortDvsecStatus,
        pub(crate) control: CxlPortDvsecControl,
        pub(crate) alt_bus_base: u8,
        pub(crate) alt_bus_limit: u8,
        pub(crate) alt_mem_base: CxlPortDvsecAltMemoryBase,
        pub(crate) alt_mem_limit: CxlPortDvsecAltMemoryLimit,
        pub(crate) alt_prefetch_mem_base: CxlPortDvsecAltPrefetchableMemoryBase,
        pub(crate) alt_prefetch_mem_limit: CxlPortDvsecAltPrefetchableMemoryLimit,
        pub(crate) alt_prefetch_mem_base_high: u32,
        pub(crate) alt_prefetch_mem_limit_high: u32,
        pub(crate) cxl_rcrb_base: CxlPortDvsecRcrbBase,
        pub(crate) cxl_rcrb_base_high: u32,
        pub(crate) supports_uio_to_hdm_enable: bool,
        pub(crate) supports_viral: bool,
    }
}

/// CXL Flex Bus Port Designated Vendor-Specific Extended Capability (DVSEC).
pub mod flex_bus_port_dvsec {
    use bitfield_struct::bitfield;
    use inspect::Inspect;

    open_enum::open_enum! {
        /// Dword offsets in the CXL Flex Bus Port DVSEC register block.
        ///
        /// The PCIe Extended Capability Header at offset `0x00` is managed by the
        /// caller. DVSEC Header1 (`0x04`) and DVSEC Header2 (`0x08`) are defined in
        /// `pci_core::spec::caps::dvsec::DvsecExtendedCapabilityHeader`.
        pub enum CxlFlexBusPortDvsecRegisterOffset : u16 {
            /// Packed DVSEC Header2 + Flex Bus Port Capability dword offset.
            DVSEC_HEADER2_CAPABILITY = 0x08,
            /// Packed Flex Bus Port Control + Flex Bus Port Status dword offset.
            DVSEC_CONTROL_STATUS = 0x0c,
            /// Flex Bus Port Received Modified TS Data Phase1 dword offset.
            DVSEC_RECEIVED_MODIFIED_TS_DATA_PHASE1 = 0x10,
            /// Flex Bus Port Capability2 dword offset.
            DVSEC_CAPABILITY2 = 0x14,
            /// Flex Bus Port Control2 dword offset.
            DVSEC_CONTROL2 = 0x18,
            /// Flex Bus Port Status2 dword offset.
            DVSEC_STATUS2 = 0x1c,
        }
    }

    /// CXL Flex Bus Port DVSEC revision.
    pub const CXL_FLEX_BUS_PORT_DVSEC_REVISION: u8 = 0x3;
    /// CXL Flex Bus Port DVSEC structure length in bytes.
    pub const CXL_FLEX_BUS_PORT_DVSEC_LENGTH: u16 = 0x20;
    /// CXL Flex Bus Port DVSEC ID value.
    pub const CXL_FLEX_BUS_PORT_DVSEC_ID: u16 = 0x0007;

    /// Writable mask for the Flex Bus Port Control register.
    pub const CXL_FLEX_BUS_PORT_DVSEC_CONTROL_WRITABLE_MASK: u16 =
        CxlFlexBusPortDvsecControl::new()
            .with_cache_enable(true)
            .with_mem_enable(true)
            .with_cxl_68b_flit_and_vh_enable(true)
            .with_cxl_multi_logical_device_enable(true)
            .with_disable_rcd_training(true)
            .with_retimer1_present(true)
            .with_retimer2_present(true)
            .with_cxl_latency_optimized_256b_flit_enable(true)
            .with_cxl_pbr_flit_enable(true)
            .into_bits();

    /// RW1CS mask for the Flex Bus Port Status register.
    pub const CXL_FLEX_BUS_PORT_DVSEC_STATUS_RW1CS_MASK: u16 = CxlFlexBusPortDvsecStatus::new()
        .with_even_half_failed(true)
        .with_cxl_correctable_protocol_id_framing_error(true)
        .with_cxl_uncorrectable_protocol_id_framing_error(true)
        .with_cxl_unexpected_protocol_id_dropped(true)
        .with_cxl_retimers_present_mismatch(true)
        .with_flex_bus_enables_phase2_mismatched(true)
        .into_bits();

    /// Writable mask for the Flex Bus Port Control2 register.
    pub const CXL_FLEX_BUS_PORT_DVSEC_CONTROL2_WRITABLE_MASK: u32 =
        CxlFlexBusPortDvsecControl2::new()
            .with_nop_hint_enable(true)
            .into_bits();

    /// Flex Bus Port Capability register (offset `0x0A`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlFlexBusPortDvsecCapability {
        pub cache_capable: bool,
        pub io_capable: bool,
        pub mem_capable: bool,
        #[bits(2)]
        _reserved0: u8,
        pub cxl_68b_flit_and_vh_capable: bool,
        pub cxl_multi_logical_device_capable: bool,
        #[bits(6)]
        _reserved1: u8,
        pub cxl_latency_optimized_256b_flit_capable: bool,
        pub cxl_pbr_flit_capable: bool,
        _reserved2: bool,
    }

    /// Flex Bus Port Control register (offset `0x0C`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlFlexBusPortDvsecControl {
        pub cache_enable: bool,
        pub io_enable: bool,
        pub mem_enable: bool,
        pub cxl_sync_hdr_bypass_enable: bool,
        pub drift_buffer_enable: bool,
        pub cxl_68b_flit_and_vh_enable: bool,
        pub cxl_multi_logical_device_enable: bool,
        pub disable_rcd_training: bool,
        pub retimer1_present: bool,
        pub retimer2_present: bool,
        #[bits(3)]
        _reserved0: u8,
        pub cxl_latency_optimized_256b_flit_enable: bool,
        pub cxl_pbr_flit_enable: bool,
        _reserved1: bool,
    }

    /// Flex Bus Port Status register (offset `0x0E`).
    #[derive(Inspect)]
    #[bitfield(u16)]
    pub struct CxlFlexBusPortDvsecStatus {
        pub cache_enabled: bool,
        pub io_enabled: bool,
        pub mem_enabled: bool,
        pub cxl_sync_hdr_bypass_enabled: bool,
        pub drift_buffer_enabled: bool,
        pub cxl_68b_flit_and_vh_enabled: bool,
        pub cxl_multi_logical_device_enabled: bool,
        pub even_half_failed: bool,
        pub cxl_correctable_protocol_id_framing_error: bool,
        pub cxl_uncorrectable_protocol_id_framing_error: bool,
        pub cxl_unexpected_protocol_id_dropped: bool,
        pub cxl_retimers_present_mismatch: bool,
        pub flex_bus_enables_phase2_mismatched: bool,
        pub cxl_latency_optimized_256b_flit_enabled: bool,
        pub cxl_pbr_flit_enabled: bool,
        pub cxl_io_throttle_required_at_64gt: bool,
    }

    /// Flex Bus Port Received Modified TS Data Phase1 register (offset `0x10`).
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlFlexBusPortDvsecReceivedModifiedTsDataPhase1 {
        #[bits(24)]
        pub received_flex_bus_data_phase1: u32,
        #[bits(8)]
        _reserved: u8,
    }

    /// Flex Bus Port Capability2 register (offset `0x14`).
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlFlexBusPortDvsecCapability2 {
        pub nop_hint_capable: bool,
        pub streamlined_port: bool,
        #[bits(30)]
        _reserved: u32,
    }

    /// Flex Bus Port Control2 register (offset `0x18`).
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlFlexBusPortDvsecControl2 {
        pub nop_hint_enable: bool,
        #[bits(31)]
        _reserved: u32,
    }

    /// Flex Bus Port Status2 register (offset `0x1C`).
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlFlexBusPortDvsecStatus2 {
        #[bits(2)]
        pub nop_hint_info: u8,
        pub streamlined_port: bool,
        #[bits(29)]
        _reserved: u32,
    }

    /// CXL Flex Bus Port PCIe Designated Vendor-Specific Extended Capability (DVSEC).
    #[derive(Clone, Inspect)]
    pub struct CxlFlexBusPortDvsecExtendedCapability {
        pub(crate) capability: CxlFlexBusPortDvsecCapability,
        pub(crate) control: CxlFlexBusPortDvsecControl,
        pub(crate) status: CxlFlexBusPortDvsecStatus,
        pub(crate) received_modified_ts_data_phase1:
            CxlFlexBusPortDvsecReceivedModifiedTsDataPhase1,
        pub(crate) capability2: CxlFlexBusPortDvsecCapability2,
        pub(crate) control2: CxlFlexBusPortDvsecControl2,
        pub(crate) status2: CxlFlexBusPortDvsecStatus2,
        pub(crate) reset_baseline_capability: CxlFlexBusPortDvsecCapability,
        pub(crate) reset_baseline_capability2: CxlFlexBusPortDvsecCapability2,
    }
}

/// CXL Register Locator Designated Vendor-Specific Extended Capability (DVSEC).
pub mod register_locator_dvsec {
    use bitfield_struct::bitfield;
    use inspect::Inspect;

    /// Register BIR encodings used in Register Locator entries.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct CxlRegisterLocatorRegisterBir(u8);

    impl CxlRegisterLocatorRegisterBir {
        /// 000b = Base Address Register 10h.
        pub const BAR_10H: Self = Self(0b000);
        /// 001b = Base Address Register 14h.
        pub const BAR_14H: Self = Self(0b001);
        /// 010b = Base Address Register 18h.
        pub const BAR_18H: Self = Self(0b010);
        /// 011b = Base Address Register 1Ch.
        pub const BAR_1CH: Self = Self(0b011);
        /// 100b = Base Address Register 20h.
        pub const BAR_20H: Self = Self(0b100);
        /// 101b = Base Address Register 24h.
        pub const BAR_24H: Self = Self(0b101);

        /// Returns the encoded BIR bits.
        pub const fn bits(self) -> u8 {
            self.0
        }
    }

    /// Register block identifiers used in Register Locator entries.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct CxlRegisterLocatorRegisterBlockIdentifier(u8);

    impl CxlRegisterLocatorRegisterBlockIdentifier {
        /// Entry is empty/invalid.
        pub const EMPTY: Self = Self(0x00);
        /// Component Register block.
        pub const COMPONENT_REGISTERS: Self = Self(0x01);
        /// BAR Virtualization ACL registers.
        pub const BAR_VIRTUALIZATION_ACL_REGISTERS: Self = Self(0x02);
        /// CXL Device registers.
        pub const CXL_DEVICE_REGISTERS: Self = Self(0x03);
        /// CPMU registers.
        pub const CPMU_REGISTERS: Self = Self(0x04);
        /// CHMU registers.
        pub const CHMU_REGISTERS: Self = Self(0x05);
        /// Designated Vendor Specific register block.
        pub const DESIGNATED_VENDOR_SPECIFIC: Self = Self(0xff);

        /// Returns the encoded register-block identifier bits.
        pub const fn bits(self) -> u8 {
            self.0
        }
    }

    /// Dword offsets in the CXL Register Locator DVSEC register block.
    ///
    /// The PCIe Extended Capability Header at offset `0x00` is managed by the
    /// caller. DVSEC Header1 (`0x04`) and DVSEC Header2 (`0x08`) are defined in
    /// `pci_core::spec::caps::dvsec::DvsecExtendedCapabilityHeader`.
    pub struct CxlRegisterLocatorDvsecRegisterOffset;

    impl CxlRegisterLocatorDvsecRegisterOffset {
        /// Packed reserved upper-half + DVSEC Header2 lower-half.
        pub const DVSEC_HEADER2: u16 = 0x08;
        /// First Register Block Offset Low dword.
        pub const FIRST_REGISTER_BLOCK_OFFSET_LOW: u16 = 0x0c;
        /// Byte stride for each Register Block entry (`low` + `high`).
        pub const REGISTER_BLOCK_STRIDE: u16 = 0x08;
        /// Base structure length in bytes with no entries.
        pub const BASE_LENGTH: u16 = 0x0c;
    }

    /// CXL Register Locator DVSEC revision.
    pub const CXL_REGISTER_LOCATOR_DVSEC_REVISION: u8 = 0x0;
    /// CXL Register Locator DVSEC ID value.
    pub const CXL_REGISTER_LOCATOR_DVSEC_ID: u16 = 0x0008;

    /// Register Offset Low register layout.
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlRegisterLocatorDvsecRegisterOffsetLow {
        #[bits(3)]
        pub register_bir: u8,
        #[bits(5)]
        _reserved0: u8,
        #[bits(8)]
        pub register_block_identifier: u8,
        #[bits(16)]
        pub register_block_offset_low: u16,
    }

    /// One Register Locator register-block entry.
    #[derive(Clone, Inspect)]
    pub struct CxlRegisterLocatorDvsecRegisterBlockEntry {
        pub(crate) offset_low: CxlRegisterLocatorDvsecRegisterOffsetLow,
        pub(crate) offset_high: u32,
    }

    /// CXL Register Locator PCIe Designated Vendor-Specific Extended Capability (DVSEC).
    #[derive(Clone, Default, Inspect)]
    pub struct CxlRegisterLocatorDvsecExtendedCapability {
        #[inspect(skip)]
        pub(crate) register_blocks: Vec<CxlRegisterLocatorDvsecRegisterBlockEntry>,
        #[inspect(skip)]
        pub(crate) reset_baseline_register_blocks: Vec<CxlRegisterLocatorDvsecRegisterBlockEntry>,
    }
}
