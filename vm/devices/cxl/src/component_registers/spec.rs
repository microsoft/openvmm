// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL component-register capability definitions.

/// CXL HDM Decoder capability definitions.
#[expect(missing_docs)] // keep grouped spec modules concise
pub mod hdm_decoder {
    use bitfield_struct::bitfield;
    use inspect::Inspect;

    /// CXL capability ID for the HDM Decoder capability.
    pub const CXL_HDM_DECODER_CAPABILITY_ID: u16 = 0x0005;

    /// CXL capability version for the HDM Decoder capability.
    pub const CXL_HDM_DECODER_CAPABILITY_VERSION: u8 = 0x3;

    /// Byte offset of the HDM Decoder Capability register.
    pub const CXL_HDM_DECODER_CAPABILITY_OFFSET: u16 = 0x00;

    /// Byte offset of the HDM Decoder Global Control register.
    pub const CXL_HDM_DECODER_GLOBAL_CONTROL_OFFSET: u16 = 0x04;

    /// Byte offset of reserved dword after Global Control.
    pub const CXL_HDM_DECODER_RESERVED0_OFFSET: u16 = 0x08;

    /// Byte offset of reserved dword before Decoder 0.
    pub const CXL_HDM_DECODER_RESERVED1_OFFSET: u16 = 0x0C;

    /// Byte offset where Decoder 0 begins.
    pub const CXL_HDM_DECODER_BASE_OFFSET: u16 = 0x10;

    /// Per-decoder register block stride in bytes.
    pub const CXL_HDM_DECODER_STRIDE_BYTES: u16 = 0x20;

    /// Fixed register bytes preceding the first decoder block.
    pub const CXL_HDM_DECODER_HEADER_LENGTH: u16 = 0x10;

    /// Length in bytes of one decoder register block.
    pub const CXL_HDM_DECODER_BLOCK_LENGTH: u16 = 0x20;

    /// Maximum HDM decoder count advertised for CXL devices.
    pub const CXL_HDM_DECODER_MAX_DEVICE_DECODER_COUNT: usize = 10;

    /// Interleave granularity encoding values for Decoder Control.IG.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub enum CxlHdmDecoderInterleaveGranularity {
        Bytes256,
        Bytes512,
        Bytes1024,
        Bytes2048,
        Bytes4096,
        Bytes8192,
        Bytes16384,
    }

    impl CxlHdmDecoderInterleaveGranularity {
        pub const fn bits(self) -> u8 {
            match self {
                Self::Bytes256 => 0x0,
                Self::Bytes512 => 0x1,
                Self::Bytes1024 => 0x2,
                Self::Bytes2048 => 0x3,
                Self::Bytes4096 => 0x4,
                Self::Bytes8192 => 0x5,
                Self::Bytes16384 => 0x6,
            }
        }
    }

    /// Interleave ways encoding values for Decoder Control.IW.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub enum CxlHdmDecoderInterleaveWays {
        Way1,
        Way2,
        Way4,
        Way8,
        Way16,
        Way3,
        Way6,
        Way12,
    }

    impl CxlHdmDecoderInterleaveWays {
        pub const fn bits(self) -> u8 {
            match self {
                Self::Way1 => 0x0,
                Self::Way2 => 0x1,
                Self::Way4 => 0x2,
                Self::Way8 => 0x3,
                Self::Way16 => 0x4,
                Self::Way3 => 0x8,
                Self::Way6 => 0x9,
                Self::Way12 => 0xA,
            }
        }
    }

    /// Coherency mode encoding values for Capability.Supported_Coherency_Modes.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub enum CxlHdmSupportedCoherencyModes {
        Unknown,
        DeviceCoherent,
        HostOnlyCoherent,
        HostOnlyOrDeviceCoherent,
    }

    impl CxlHdmSupportedCoherencyModes {
        pub const fn bits(self) -> u8 {
            match self {
                Self::Unknown => 0b00,
                Self::DeviceCoherent => 0b01,
                Self::HostOnlyCoherent => 0b10,
                Self::HostOnlyOrDeviceCoherent => 0b11,
            }
        }
    }

    /// Returns the Decoder Count field encoding for the given decoder count.
    pub const fn encode_decoder_count(decoder_count: usize) -> Option<u8> {
        match decoder_count {
            1 => Some(0x0),
            2 => Some(0x1),
            4 => Some(0x2),
            6 => Some(0x3),
            8 => Some(0x4),
            10 => Some(0x5),
            12 => Some(0x6),
            14 => Some(0x7),
            16 => Some(0x8),
            20 => Some(0x9),
            24 => Some(0xA),
            28 => Some(0xB),
            32 => Some(0xC),
            _ => None,
        }
    }

    /// HDM Decoder Capability register at offset 0x00.
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlHdmDecoderCapabilityRegister {
        #[bits(4)]
        pub decoder_count: u8,
        #[bits(4)]
        pub target_count: u8,
        pub a11to8_interleave_capable: bool,
        pub a14to12_interleave_capable: bool,
        pub poison_on_decode_error_capable: bool,
        pub interleave_3_6_12_way_capable: bool,
        pub interleave_16_way_capable: bool,
        pub uio_capable: bool,
        #[bits(2)]
        _reserved0: u8,
        #[bits(4)]
        pub uio_capable_decoder_count: u8,
        pub mem_data_nxm_capable: bool,
        #[bits(2)]
        pub supported_coherency_modes: u8,
        #[bits(9)]
        _reserved1: u16,
    }

    /// HDM Decoder Global Control register at offset 0x04.
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlHdmDecoderGlobalControlRegister {
        pub poison_on_decode_error_enable: bool,
        pub hdm_decoder_enable: bool,
        #[bits(30)]
        _reserved: u32,
    }

    /// Writable mask for HDM Decoder Global Control.
    pub const CXL_HDM_DECODER_GLOBAL_CONTROL_WRITABLE_MASK: u32 =
        CxlHdmDecoderGlobalControlRegister::new()
            .with_poison_on_decode_error_enable(true)
            .with_hdm_decoder_enable(true)
            .into_bits();

    /// Per-decoder register offsets within one decoder block.
    pub struct CxlHdmDecoderRegisterOffset;

    impl CxlHdmDecoderRegisterOffset {
        pub const BASE_LOW: u16 = 0x00;
        pub const BASE_HIGH: u16 = 0x04;
        pub const SIZE_LOW: u16 = 0x08;
        pub const SIZE_HIGH: u16 = 0x0C;
        pub const CONTROL: u16 = 0x10;
        pub const DPA_SKIP_LOW: u16 = 0x14;
        pub const DPA_SKIP_HIGH: u16 = 0x18;
    }

    /// HDM Decoder n Base Low register.
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlHdmDecoderBaseLowRegister {
        #[bits(28)]
        _reserved0: u32,
        #[bits(4)]
        pub memory_base_low: u8,
    }

    /// Writable mask for Base Low.
    pub const CXL_HDM_DECODER_BASE_LOW_WRITABLE_MASK: u32 = CxlHdmDecoderBaseLowRegister::new()
        .with_memory_base_low(0xF)
        .into_bits();

    /// HDM Decoder n Size Low register.
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlHdmDecoderSizeLowRegister {
        #[bits(28)]
        _reserved0: u32,
        #[bits(4)]
        pub memory_size_low: u8,
    }

    /// Writable mask for Size Low.
    pub const CXL_HDM_DECODER_SIZE_LOW_WRITABLE_MASK: u32 = CxlHdmDecoderSizeLowRegister::new()
        .with_memory_size_low(0xF)
        .into_bits();

    /// HDM Decoder n Control register.
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlHdmDecoderControlRegister {
        #[bits(4)]
        pub interleave_granularity: u8,
        #[bits(4)]
        pub interleave_ways: u8,
        pub lock_on_commit: bool,
        pub commit: bool,
        pub committed: bool,
        pub error_not_committed: bool,
        pub target_range_type: bool,
        pub bi: bool,
        pub uio: bool,
        #[bits(1)]
        _reserved0: u8,
        #[bits(4)]
        pub upstream_interleave_granularity: u8,
        #[bits(4)]
        pub upstream_interleave_ways: u8,
        #[bits(4)]
        pub interleave_set_position: u8,
        #[bits(4)]
        _reserved1: u8,
    }

    /// Writable mask for Decoder Control RWL fields.
    pub const CXL_HDM_DECODER_CONTROL_WRITABLE_MASK: u32 = CxlHdmDecoderControlRegister::new()
        .with_interleave_granularity(0xF)
        .with_interleave_ways(0xF)
        .with_lock_on_commit(true)
        .with_commit(true)
        .with_target_range_type(true)
        .with_bi(true)
        .with_uio(true)
        .with_upstream_interleave_granularity(0xF)
        .with_upstream_interleave_ways(0xF)
        .with_interleave_set_position(0xF)
        .into_bits();

    /// HDM Decoder n DPA Skip Low register.
    #[derive(Inspect)]
    #[bitfield(u32)]
    pub struct CxlHdmDecoderDpaSkipLowRegister {
        #[bits(28)]
        _reserved0: u32,
        #[bits(4)]
        pub dpa_skip_low: u8,
    }

    /// Writable mask for DPA Skip Low.
    pub const CXL_HDM_DECODER_DPA_SKIP_LOW_WRITABLE_MASK: u32 =
        CxlHdmDecoderDpaSkipLowRegister::new()
            .with_dpa_skip_low(0xF)
            .into_bits();
}
