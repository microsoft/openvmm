// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL HDM Decoder component capability implementation.

use super::CxlComponentRegister;
use super::spec::hdm_decoder::CXL_HDM_DECODER_BASE_OFFSET;
use super::spec::hdm_decoder::CXL_HDM_DECODER_BLOCK_LENGTH;
use super::spec::hdm_decoder::CXL_HDM_DECODER_CAPABILITY_ID;
use super::spec::hdm_decoder::CXL_HDM_DECODER_CAPABILITY_OFFSET;
use super::spec::hdm_decoder::CXL_HDM_DECODER_CAPABILITY_VERSION;
use super::spec::hdm_decoder::CXL_HDM_DECODER_GLOBAL_CONTROL_OFFSET;
use super::spec::hdm_decoder::CXL_HDM_DECODER_GLOBAL_CONTROL_WRITABLE_MASK;
use super::spec::hdm_decoder::CXL_HDM_DECODER_HEADER_LENGTH;
use super::spec::hdm_decoder::CXL_HDM_DECODER_RESERVED0_OFFSET;
use super::spec::hdm_decoder::CXL_HDM_DECODER_RESERVED1_OFFSET;
use super::spec::hdm_decoder::CXL_HDM_DECODER_STRIDE_BYTES;
use super::spec::hdm_decoder::CxlHdmDecoderBaseLowRegister;
use super::spec::hdm_decoder::CxlHdmDecoderCapabilityRegister;
use super::spec::hdm_decoder::CxlHdmDecoderControlRegister;
use super::spec::hdm_decoder::CxlHdmDecoderDpaSkipLowRegister;
use super::spec::hdm_decoder::CxlHdmDecoderGlobalControlRegister;
use super::spec::hdm_decoder::CxlHdmDecoderInterleaveGranularity;
use super::spec::hdm_decoder::CxlHdmDecoderInterleaveWays;
use super::spec::hdm_decoder::CxlHdmDecoderRegisterOffset;
use super::spec::hdm_decoder::CxlHdmDecoderSizeLowRegister;
use super::spec::hdm_decoder::CxlHdmSupportedCoherencyModes;
use super::spec::hdm_decoder::encode_decoder_count;
use crate::spec::CXL_HPA_ALIGNMENT;
use crate::spec::CxlComponentRegisterType;
use inspect::Inspect;
use thiserror::Error;
use tracing::info;

/// Fixed HDM range and interleave configuration for Decoder 0.
#[derive(Debug, Copy, Clone)]
pub struct CxlHdmDecoderFixedConfig {
    /// Host physical base address of the HDM range.
    pub hdm_base: u64,
    /// Length in bytes of the HDM range.
    pub hdm_size: u64,
    /// Fixed Decoder 0 interleave granularity.
    pub interleave_granularity: CxlHdmDecoderInterleaveGranularity,
    /// Fixed Decoder 0 interleave ways.
    pub interleave_ways: CxlHdmDecoderInterleaveWays,
}

/// Configures static capability bits for the HDM Decoder capability register.
#[derive(Debug, Copy, Clone)]
pub struct CxlHdmDecoderCapabilityOptions {
    /// Raw Target Count encoding for Capability bits 7:4.
    pub target_count_encoding: u8,
    /// Capability bit 8: supports address-bit interleave `11:8`.
    pub a11to8_interleave_capable: bool,
    /// Capability bit 9: supports address-bit interleave `14:12`.
    pub a14to12_interleave_capable: bool,
    /// Capability bit 10: supports poison decode responses on errors.
    pub poison_on_decode_error_capable: bool,
    /// Capability bit 11: supports 3/6/12-way interleave modes.
    pub interleave_3_6_12_way_capable: bool,
    /// Capability bit 12: supports 16-way interleave mode.
    pub interleave_16_way_capable: bool,
    /// Capability bit 13: supports UIO decode semantics.
    pub uio_capable: bool,
    /// Raw UIO-capable decoder count encoding for capability bits 19:16.
    pub uio_capable_decoder_count: u8,
    /// Capability bit 20: supports MemData-NXM responses.
    pub mem_data_nxm_capable: bool,
    /// Capability bits 22:21: supported coherency mode encoding.
    pub supported_coherency_modes: CxlHdmSupportedCoherencyModes,
}

impl Default for CxlHdmDecoderCapabilityOptions {
    fn default() -> Self {
        Self {
            target_count_encoding: 0,
            a11to8_interleave_capable: true,
            a14to12_interleave_capable: true,
            poison_on_decode_error_capable: false,
            interleave_3_6_12_way_capable: false,
            interleave_16_way_capable: false,
            uio_capable: true,
            uio_capable_decoder_count: 0,
            mem_data_nxm_capable: true,
            supported_coherency_modes: CxlHdmSupportedCoherencyModes::DeviceCoherent,
        }
    }
}

/// Error returned while constructing an HDM Decoder capability block.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Error)]
pub enum CxlHdmDecoderCapabilityError {
    /// Fixed range is invalid (must be non-zero and 256-MiB aligned).
    #[error("fixed HDM range must be non-zero and 256-MiB aligned")]
    InvalidFixedRange,
    /// Too many fixed decoder ranges were configured.
    #[error("too many fixed HDM decoders configured")]
    TooManyFixedDecoders,
}

#[derive(Default, Copy, Clone, Inspect)]
struct CxlHdmDecoderRegisterBlock {
    base_low: CxlHdmDecoderBaseLowRegister,
    base_high: u32,
    size_low: CxlHdmDecoderSizeLowRegister,
    size_high: u32,
    control: CxlHdmDecoderControlRegister,
    dpa_skip_low: CxlHdmDecoderDpaSkipLowRegister,
    dpa_skip_high: u32,
}

/// CXL HDM Decoder capability register block.
#[derive(Inspect)]
pub struct CxlHdmDecoderCapability {
    capability: CxlHdmDecoderCapabilityRegister,
    global_control: CxlHdmDecoderGlobalControlRegister,
    #[inspect(skip)]
    decoders: Vec<CxlHdmDecoderRegisterBlock>,
}

impl CxlHdmDecoderCapability {
    /// Creates an HDM Decoder capability with default capability bits.
    ///
    /// No fixed HDM decoders are added by default. Use `with_hdm` to add one
    /// or more fixed decoder ranges after construction.
    pub fn new() -> Result<Self, CxlHdmDecoderCapabilityError> {
        let options = CxlHdmDecoderCapabilityOptions::default();
        let this = Self {
            capability: CxlHdmDecoderCapabilityRegister::new()
                .with_decoder_count(0)
                .with_target_count(options.target_count_encoding & 0xF)
                .with_a11to8_interleave_capable(options.a11to8_interleave_capable)
                .with_a14to12_interleave_capable(options.a14to12_interleave_capable)
                .with_poison_on_decode_error_capable(options.poison_on_decode_error_capable)
                .with_interleave_3_6_12_way_capable(options.interleave_3_6_12_way_capable)
                .with_interleave_16_way_capable(options.interleave_16_way_capable)
                .with_uio_capable(options.uio_capable)
                .with_uio_capable_decoder_count(options.uio_capable_decoder_count & 0xF)
                .with_mem_data_nxm_capable(options.mem_data_nxm_capable)
                .with_supported_coherency_modes(options.supported_coherency_modes.bits()),
            global_control: CxlHdmDecoderGlobalControlRegister::new()
                .with_poison_on_decode_error_enable(false)
                .with_hdm_decoder_enable(false),
            decoders: Vec::new(),
        };
        Ok(this)
    }

    fn push_decoder(
        &mut self,
        block: CxlHdmDecoderRegisterBlock,
    ) -> Result<(), CxlHdmDecoderCapabilityError> {
        let decoder_count = self.decoders.len() + 1;
        let decoder_count_encoding = encode_decoder_count(decoder_count)
            .ok_or(CxlHdmDecoderCapabilityError::TooManyFixedDecoders)?;

        self.capability = self.capability.with_decoder_count(decoder_count_encoding);
        self.decoders.push(block);
        Ok(())
    }

    /// Adds a programmable HDM decoder slot.
    pub fn with_decoder_slot(
        &mut self,
        interleave_granularity: CxlHdmDecoderInterleaveGranularity,
        interleave_ways: CxlHdmDecoderInterleaveWays,
    ) -> Result<(), CxlHdmDecoderCapabilityError> {
        self.push_decoder(CxlHdmDecoderRegisterBlock {
            base_low: CxlHdmDecoderBaseLowRegister::new(),
            base_high: 0,
            size_low: CxlHdmDecoderSizeLowRegister::new(),
            size_high: 0,
            control: CxlHdmDecoderControlRegister::new()
                .with_interleave_granularity(interleave_granularity.bits())
                .with_interleave_ways(interleave_ways.bits())
                .with_lock_on_commit(true)
                .with_commit(false)
                .with_committed(false)
                .with_error_not_committed(false),
            dpa_skip_low: CxlHdmDecoderDpaSkipLowRegister::new(),
            dpa_skip_high: 0,
        })
    }

    /// Adds one fixed HDM decoder range.
    pub fn with_hdm(
        &mut self,
        fixed: CxlHdmDecoderFixedConfig,
    ) -> Result<(), CxlHdmDecoderCapabilityError> {
        if fixed.hdm_size == 0
            || !fixed.hdm_base.is_multiple_of(CXL_HPA_ALIGNMENT)
            || !fixed.hdm_size.is_multiple_of(CXL_HPA_ALIGNMENT)
        {
            return Err(CxlHdmDecoderCapabilityError::InvalidFixedRange);
        }

        let fixed_decoder = CxlHdmDecoderRegisterBlock {
            base_low: CxlHdmDecoderBaseLowRegister::new()
                .with_memory_base_low(((fixed.hdm_base >> 28) & 0xF) as u8),
            base_high: (fixed.hdm_base >> 32) as u32,
            size_low: CxlHdmDecoderSizeLowRegister::new()
                .with_memory_size_low(((fixed.hdm_size >> 28) & 0xF) as u8),
            size_high: (fixed.hdm_size >> 32) as u32,
            control: CxlHdmDecoderControlRegister::new()
                .with_interleave_granularity(fixed.interleave_granularity.bits())
                .with_interleave_ways(fixed.interleave_ways.bits())
                .with_lock_on_commit(true)
                .with_commit(true)
                .with_committed(true)
                .with_error_not_committed(false),
            dpa_skip_low: CxlHdmDecoderDpaSkipLowRegister::new(),
            dpa_skip_high: 0,
        };

        self.push_decoder(fixed_decoder)
    }

    fn decoder_base_and_size(block: &CxlHdmDecoderRegisterBlock) -> Option<(u64, u64)> {
        let base = (u64::from(block.base_high) << 32)
            | (u64::from(block.base_low.memory_base_low()) << 28);
        let size = (u64::from(block.size_high) << 32)
            | (u64::from(block.size_low.memory_size_low()) << 28);
        if size == 0 {
            return None;
        }

        Some((base, size))
    }

    /// Resolves an MMIO access to an enabled committed decoder.
    ///
    /// Returns the decoder index and offset within that decoder's range when
    /// `addr..addr+len` is fully contained in an enabled committed range.
    pub fn find_enabled_decoder_for_access(&self, addr: u64, len: usize) -> Option<(usize, u64)> {
        if !self.global_control.hdm_decoder_enable() {
            return None;
        }

        let access_len = u64::try_from(len).ok()?;
        let access_end = addr.checked_add(access_len)?;

        for (index, block) in self.decoders.iter().enumerate() {
            if !block.control.committed() {
                continue;
            }

            let Some((base, size)) = Self::decoder_base_and_size(block) else {
                continue;
            };
            let Some(range_end) = base.checked_add(size) else {
                continue;
            };

            if addr >= base && access_end <= range_end {
                return Some((index, addr - base));
            }
        }

        None
    }
    fn len_for_decoder_count(decoder_count: usize) -> u16 {
        CXL_HDM_DECODER_HEADER_LENGTH + (decoder_count as u16) * CXL_HDM_DECODER_BLOCK_LENGTH
    }

    fn decode_decoder_offset(offset: u16) -> Option<(usize, u16)> {
        let relative = offset.checked_sub(CXL_HDM_DECODER_BASE_OFFSET)?;
        let index = usize::from(relative / CXL_HDM_DECODER_STRIDE_BYTES);
        let within = relative % CXL_HDM_DECODER_STRIDE_BYTES;
        Some((index, within))
    }

    fn read_decoder_u32(block: &CxlHdmDecoderRegisterBlock, within: u16) -> Option<u32> {
        match within {
            CxlHdmDecoderRegisterOffset::BASE_LOW => Some(block.base_low.into_bits()),
            CxlHdmDecoderRegisterOffset::BASE_HIGH => Some(block.base_high),
            CxlHdmDecoderRegisterOffset::SIZE_LOW => Some(block.size_low.into_bits()),
            CxlHdmDecoderRegisterOffset::SIZE_HIGH => Some(block.size_high),
            CxlHdmDecoderRegisterOffset::CONTROL => Some(block.control.into_bits()),
            CxlHdmDecoderRegisterOffset::DPA_SKIP_LOW => Some(block.dpa_skip_low.into_bits()),
            CxlHdmDecoderRegisterOffset::DPA_SKIP_HIGH => Some(block.dpa_skip_high),
            _ => None,
        }
    }

    fn write_decoder_u32(block: &mut CxlHdmDecoderRegisterBlock, within: u16, value: u32) -> bool {
        // When lock-on-commit is active and the decoder is committed, all
        // decoder-register writes are blocked (BASE/SIZE/CONTROL/DPA_SKIP).
        if block.control.lock_on_commit() && block.control.committed() {
            info!(
                register_offset = within,
                value,
                "HDM decoder write rejected: decoder is lock-on-commit and already committed"
            );
            return false;
        }

        match within {
            CxlHdmDecoderRegisterOffset::BASE_LOW => {
                block.base_low = CxlHdmDecoderBaseLowRegister::from_bits(value);
                info!(
                    register = "BASE_LOW",
                    raw_value = value,
                    memory_base_low = block.base_low.memory_base_low(),
                    "HDM decoder BASE_LOW programmed"
                );
                true
            }
            CxlHdmDecoderRegisterOffset::BASE_HIGH => {
                block.base_high = value;
                info!(
                    register = "BASE_HIGH",
                    raw_value = value,
                    "HDM decoder BASE_HIGH programmed"
                );
                true
            }
            CxlHdmDecoderRegisterOffset::SIZE_LOW => {
                block.size_low = CxlHdmDecoderSizeLowRegister::from_bits(value);
                info!(
                    register = "SIZE_LOW",
                    raw_value = value,
                    memory_size_low = block.size_low.memory_size_low(),
                    "HDM decoder SIZE_LOW programmed"
                );
                true
            }
            CxlHdmDecoderRegisterOffset::SIZE_HIGH => {
                block.size_high = value;
                info!(
                    register = "SIZE_HIGH",
                    raw_value = value,
                    "HDM decoder SIZE_HIGH programmed"
                );
                true
            }
            CxlHdmDecoderRegisterOffset::CONTROL => {
                let requested = CxlHdmDecoderControlRegister::from_bits(value);
                let commit_requested = requested.commit();
                let committed = block.control.committed() || commit_requested;
                block.control = CxlHdmDecoderControlRegister::new()
                    .with_interleave_granularity(requested.interleave_granularity())
                    .with_interleave_ways(requested.interleave_ways())
                    .with_lock_on_commit(block.control.lock_on_commit())
                    .with_commit(block.control.commit() || commit_requested)
                    .with_committed(committed)
                    .with_error_not_committed(false);
                info!(
                    register = "CONTROL",
                    raw_value = value,
                    commit_requested,
                    commit = block.control.commit(),
                    committed = block.control.committed(),
                    lock_on_commit = block.control.lock_on_commit(),
                    interleave_granularity = block.control.interleave_granularity(),
                    interleave_ways = block.control.interleave_ways(),
                    "HDM decoder CONTROL programmed"
                );
                true
            }
            CxlHdmDecoderRegisterOffset::DPA_SKIP_LOW => {
                block.dpa_skip_low = CxlHdmDecoderDpaSkipLowRegister::from_bits(value);
                info!(
                    register = "DPA_SKIP_LOW",
                    raw_value = value,
                    "HDM decoder DPA_SKIP_LOW programmed"
                );
                true
            }
            CxlHdmDecoderRegisterOffset::DPA_SKIP_HIGH => {
                block.dpa_skip_high = value;
                info!(
                    register = "DPA_SKIP_HIGH",
                    raw_value = value,
                    "HDM decoder DPA_SKIP_HIGH programmed"
                );
                true
            }
            _ => false,
        }
    }
}

impl CxlComponentRegister for CxlHdmDecoderCapability {
    fn label(&self) -> &str {
        "cxl-hdm-decoder-capability"
    }

    fn register_type(&self) -> CxlComponentRegisterType {
        CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER
    }

    fn capability_id(&self) -> u16 {
        CXL_HDM_DECODER_CAPABILITY_ID
    }

    fn capability_version(&self) -> u8 {
        CXL_HDM_DECODER_CAPABILITY_VERSION
    }

    fn len(&self) -> u16 {
        Self::len_for_decoder_count(self.decoders.len())
    }

    fn read_u32(&self, offset: u16) -> Option<u32> {
        if offset >= self.len() || !offset.is_multiple_of(4) {
            return None;
        }

        match offset {
            CXL_HDM_DECODER_CAPABILITY_OFFSET => Some(self.capability.into_bits()),
            CXL_HDM_DECODER_GLOBAL_CONTROL_OFFSET => Some(self.global_control.into_bits()),
            CXL_HDM_DECODER_RESERVED0_OFFSET | CXL_HDM_DECODER_RESERVED1_OFFSET => Some(0),
            x if x >= CXL_HDM_DECODER_BASE_OFFSET => {
                let (index, within) = Self::decode_decoder_offset(x)?;
                let block = self.decoders.get(index)?;
                Self::read_decoder_u32(block, within)
            }
            _ => None,
        }
    }

    fn write_u32(&mut self, offset: u16, value: u32) -> bool {
        if offset >= self.len() || !offset.is_multiple_of(4) {
            return false;
        }

        match offset {
            CXL_HDM_DECODER_GLOBAL_CONTROL_OFFSET => {
                self.global_control = CxlHdmDecoderGlobalControlRegister::from_bits(
                    value & CXL_HDM_DECODER_GLOBAL_CONTROL_WRITABLE_MASK,
                );
                info!(
                    raw_value = value,
                    masked_value = value & CXL_HDM_DECODER_GLOBAL_CONTROL_WRITABLE_MASK,
                    hdm_decoder_enable = self.global_control.hdm_decoder_enable(),
                    poison_on_decode_error_enable =
                        self.global_control.poison_on_decode_error_enable(),
                    "HDM global control programmed"
                );
                true
            }
            CXL_HDM_DECODER_RESERVED0_OFFSET | CXL_HDM_DECODER_RESERVED1_OFFSET => false,
            x if x >= CXL_HDM_DECODER_BASE_OFFSET => {
                let Some((index, within)) = Self::decode_decoder_offset(x) else {
                    return false;
                };
                let Some(block) = self.decoders.get_mut(index) else {
                    return false;
                };
                Self::write_decoder_u32(block, within, value)
            }
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.global_control = CxlHdmDecoderGlobalControlRegister::new()
            .with_poison_on_decode_error_enable(false)
            .with_hdm_decoder_enable(false);
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
        #[mesh(package = "cxl.component_registers.hdm_cap")]
        pub struct SavedDecoderState {
            #[mesh(1)]
            pub base_low: u32,
            #[mesh(2)]
            pub base_high: u32,
            #[mesh(3)]
            pub size_low: u32,
            #[mesh(4)]
            pub size_high: u32,
            #[mesh(5)]
            pub control: u32,
            #[mesh(6)]
            pub dpa_skip_low: u32,
            #[mesh(7)]
            pub dpa_skip_high: u32,
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "cxl.component_registers.hdm_cap")]
        pub struct SavedState {
            #[mesh(1)]
            pub global_control: u32,
            #[mesh(2)]
            pub decoders: Vec<SavedDecoderState>,
        }
    }

    impl SaveRestore for CxlHdmDecoderCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                global_control: self.global_control.into_bits(),
                decoders: self
                    .decoders
                    .iter()
                    .map(|decoder| state::SavedDecoderState {
                        base_low: decoder.base_low.into_bits(),
                        base_high: decoder.base_high,
                        size_low: decoder.size_low.into_bits(),
                        size_high: decoder.size_high,
                        control: decoder.control.into_bits(),
                        dpa_skip_low: decoder.dpa_skip_low.into_bits(),
                        dpa_skip_high: decoder.dpa_skip_high,
                    })
                    .collect(),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            if state.decoders.len() != self.decoders.len() {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "hdm decoder count mismatch: saved {}, current {}",
                    state.decoders.len(),
                    self.decoders.len()
                )));
            }

            self.global_control =
                CxlHdmDecoderGlobalControlRegister::from_bits(state.global_control);

            for (decoder, saved) in self.decoders.iter_mut().zip(state.decoders) {
                decoder.base_low = CxlHdmDecoderBaseLowRegister::from_bits(saved.base_low);
                decoder.base_high = saved.base_high;
                decoder.size_low = CxlHdmDecoderSizeLowRegister::from_bits(saved.size_low);
                decoder.size_high = saved.size_high;
                decoder.control = CxlHdmDecoderControlRegister::from_bits(saved.control);
                decoder.dpa_skip_low =
                    CxlHdmDecoderDpaSkipLowRegister::from_bits(saved.dpa_skip_low);
                decoder.dpa_skip_high = saved.dpa_skip_high;
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmcore::save_restore::SaveRestore;

    fn fixed_config() -> CxlHdmDecoderFixedConfig {
        CxlHdmDecoderFixedConfig {
            hdm_base: 0x1000_0000,
            hdm_size: 0x1000_0000,
            interleave_granularity: CxlHdmDecoderInterleaveGranularity::Bytes256,
            interleave_ways: CxlHdmDecoderInterleaveWays::Way1,
        }
    }

    #[test]
    fn metadata_and_capability_dword() {
        let mut cap = CxlHdmDecoderCapability::new().expect("new should succeed");
        cap.with_hdm(fixed_config()).expect("fixed config valid");
        assert_eq!(
            cap.register_type(),
            CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER
        );
        assert_eq!(cap.capability_id(), CXL_HDM_DECODER_CAPABILITY_ID);
        assert_eq!(cap.capability_version(), CXL_HDM_DECODER_CAPABILITY_VERSION);
        assert_eq!(
            cap.len(),
            CXL_HDM_DECODER_HEADER_LENGTH + CXL_HDM_DECODER_BLOCK_LENGTH
        );

        let cap_dword = cap
            .read_u32(CXL_HDM_DECODER_CAPABILITY_OFFSET)
            .expect("capability dword should exist");
        assert_eq!(cap_dword & 0xF, 0x0);
    }

    #[test]
    fn fixed_decoder_control_exposes_read_only_committed_state() {
        let mut cap = CxlHdmDecoderCapability::new().expect("new should succeed");
        cap.with_hdm(fixed_config()).expect("fixed config valid");
        let control_offset = CXL_HDM_DECODER_BASE_OFFSET + CxlHdmDecoderRegisterOffset::CONTROL;

        let control = cap
            .read_u32(control_offset)
            .expect("control dword should exist");
        assert_ne!(control & (1u32 << 8), 0);
        assert_ne!(control & (1u32 << 9), 0);
        assert_ne!(control & (1u32 << 10), 0);
    }

    #[test]
    fn fixed_decoder_rejects_programming_writes() {
        let mut cap = CxlHdmDecoderCapability::new().expect("new should succeed");
        cap.with_hdm(fixed_config()).expect("fixed config valid");
        let base_high_offset = CXL_HDM_DECODER_BASE_OFFSET + CxlHdmDecoderRegisterOffset::BASE_HIGH;
        let control_offset = CXL_HDM_DECODER_BASE_OFFSET + CxlHdmDecoderRegisterOffset::CONTROL;

        assert!(!cap.write_u32(base_high_offset, 0xfeed_beef));
        assert!(!cap.write_u32(control_offset, 0xffff_ffff));
        let base_high = cap
            .read_u32(base_high_offset)
            .expect("base high should exist");
        assert_eq!(base_high, 0x0);
    }

    #[test]
    fn save_restore_roundtrip_preserves_decoder_state() {
        let mut cap = CxlHdmDecoderCapability::new().expect("new should succeed");
        cap.with_hdm(fixed_config()).expect("fixed config valid");

        let saved = cap.save().expect("save should succeed");

        let mut restored = CxlHdmDecoderCapability::new().expect("new should succeed");
        restored
            .with_hdm(fixed_config())
            .expect("fixed config valid");
        restored.restore(saved).expect("restore should succeed");
        assert_eq!(
            restored.read_u32(CXL_HDM_DECODER_GLOBAL_CONTROL_OFFSET),
            Some(0)
        );
    }

    #[test]
    fn save_restore_should_preserve_global_control_runtime_state() {
        let mut cap = CxlHdmDecoderCapability::new().expect("new should succeed");
        cap.with_hdm(fixed_config()).expect("fixed config valid");

        let enabled = CxlHdmDecoderGlobalControlRegister::new()
            .with_hdm_decoder_enable(true)
            .into_bits();
        assert!(cap.write_u32(CXL_HDM_DECODER_GLOBAL_CONTROL_OFFSET, enabled));
        assert_eq!(
            cap.read_u32(CXL_HDM_DECODER_GLOBAL_CONTROL_OFFSET),
            Some(enabled)
        );

        let saved = cap.save().expect("save should succeed");
        let mut restored = CxlHdmDecoderCapability::new().expect("new should succeed");
        restored
            .with_hdm(fixed_config())
            .expect("fixed config valid");
        restored.restore(saved).expect("restore should succeed");

        // This currently fails: restore forces global control back to defaults.
        assert_eq!(
            restored.read_u32(CXL_HDM_DECODER_GLOBAL_CONTROL_OFFSET),
            Some(enabled)
        );
    }

    #[test]
    fn enabled_decoder_lookup_requires_global_enable() {
        let mut cap = CxlHdmDecoderCapability::new().expect("new should succeed");
        cap.with_hdm(fixed_config()).expect("fixed config valid");
        assert_eq!(cap.find_enabled_decoder_for_access(0x1000_0100, 4), None);

        assert!(cap.write_u32(CXL_HDM_DECODER_GLOBAL_CONTROL_OFFSET, 1u32 << 1));
        assert_eq!(
            cap.find_enabled_decoder_for_access(0x1000_0100, 4),
            Some((0, 0x100))
        );
    }

    #[test]
    fn with_hdm_can_add_multiple_fixed_ranges() {
        let mut cap = CxlHdmDecoderCapability::new().expect("new should succeed");

        cap.with_hdm(fixed_config()).expect("fixed config valid");
        assert!(cap.write_u32(CXL_HDM_DECODER_GLOBAL_CONTROL_OFFSET, 1u32 << 1));

        cap.with_hdm(CxlHdmDecoderFixedConfig {
            hdm_base: 0x2000_0000,
            hdm_size: 0x1000_0000,
            interleave_granularity: CxlHdmDecoderInterleaveGranularity::Bytes256,
            interleave_ways: CxlHdmDecoderInterleaveWays::Way1,
        })
        .expect("adding a second fixed range should succeed");

        assert_eq!(
            cap.len(),
            CXL_HDM_DECODER_HEADER_LENGTH + (2 * CXL_HDM_DECODER_BLOCK_LENGTH)
        );

        assert_eq!(
            cap.find_enabled_decoder_for_access(0x2000_0100, 4),
            Some((1, 0x100))
        );
    }

    #[test]
    fn programmable_decoder_commit_sets_committed_and_locks_when_configured() {
        let mut cap = CxlHdmDecoderCapability::new().expect("new should succeed");
        cap.with_decoder_slot(
            CxlHdmDecoderInterleaveGranularity::Bytes256,
            CxlHdmDecoderInterleaveWays::Way1,
        )
        .expect("slot creation should succeed");

        let base_low_offset = CXL_HDM_DECODER_BASE_OFFSET + CxlHdmDecoderRegisterOffset::BASE_LOW;
        let control_offset = CXL_HDM_DECODER_BASE_OFFSET + CxlHdmDecoderRegisterOffset::CONTROL;

        assert!(cap.write_u32(base_low_offset, 0x1));
        let control_commit = CxlHdmDecoderControlRegister::new()
            .with_commit(true)
            .into_bits();
        assert!(cap.write_u32(control_offset, control_commit));

        let control_bits = cap
            .read_u32(control_offset)
            .expect("control dword should exist");
        let control = CxlHdmDecoderControlRegister::from_bits(control_bits);
        assert!(control.commit());
        assert!(control.committed());
        assert!(control.lock_on_commit());

        assert!(!cap.write_u32(base_low_offset, 0x2));
        assert!(!cap.write_u32(control_offset, 0));
    }

    #[test]
    fn programmable_decoder_lock_blocks_base_size_and_dpa_programming() {
        let mut cap = CxlHdmDecoderCapability::new().expect("new should succeed");
        cap.with_decoder_slot(
            CxlHdmDecoderInterleaveGranularity::Bytes256,
            CxlHdmDecoderInterleaveWays::Way1,
        )
        .expect("slot creation should succeed");

        let base_low_offset = CXL_HDM_DECODER_BASE_OFFSET + CxlHdmDecoderRegisterOffset::BASE_LOW;
        let base_high_offset = CXL_HDM_DECODER_BASE_OFFSET + CxlHdmDecoderRegisterOffset::BASE_HIGH;
        let size_low_offset = CXL_HDM_DECODER_BASE_OFFSET + CxlHdmDecoderRegisterOffset::SIZE_LOW;
        let size_high_offset = CXL_HDM_DECODER_BASE_OFFSET + CxlHdmDecoderRegisterOffset::SIZE_HIGH;
        let dpa_skip_low_offset =
            CXL_HDM_DECODER_BASE_OFFSET + CxlHdmDecoderRegisterOffset::DPA_SKIP_LOW;
        let dpa_skip_high_offset =
            CXL_HDM_DECODER_BASE_OFFSET + CxlHdmDecoderRegisterOffset::DPA_SKIP_HIGH;
        let control_offset = CXL_HDM_DECODER_BASE_OFFSET + CxlHdmDecoderRegisterOffset::CONTROL;

        // Program once before commit to prove writes are accepted while unlocked.
        assert!(cap.write_u32(base_low_offset, 0x1));
        assert!(cap.write_u32(base_high_offset, 0x2));
        assert!(cap.write_u32(size_low_offset, 0x3));
        assert!(cap.write_u32(size_high_offset, 0x4));
        assert!(cap.write_u32(dpa_skip_low_offset, 0x5));
        assert!(cap.write_u32(dpa_skip_high_offset, 0x6));

        let control_commit = CxlHdmDecoderControlRegister::new()
            .with_commit(true)
            .into_bits();
        assert!(cap.write_u32(control_offset, control_commit));

        // Once committed with lock-on-commit set, all decoder-programming writes reject.
        assert!(!cap.write_u32(base_low_offset, 0x11));
        assert!(!cap.write_u32(base_high_offset, 0x22));
        assert!(!cap.write_u32(size_low_offset, 0x33));
        assert!(!cap.write_u32(size_high_offset, 0x44));
        assert!(!cap.write_u32(dpa_skip_low_offset, 0x55));
        assert!(!cap.write_u32(dpa_skip_high_offset, 0x66));
    }
}
