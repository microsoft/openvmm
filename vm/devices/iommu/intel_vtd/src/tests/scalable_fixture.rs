// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use iommu_common::IommuTranslator;

pub(super) const ROOT: u64 = 0x1000;
pub(super) const LOWER: u64 = 0x2000;
pub(super) const UPPER: u64 = 0x3000;
pub(super) const DIRECTORY: u64 = 0x10000;
pub(super) const PASID_TABLE: u64 = 0x40000;
pub(super) const SL_ROOT: u64 = 0x50000;
pub(super) const GPA: u64 = 0x70000;
pub(super) const UNMAPPED: u64 = 0x100000;
pub(super) const IOVA: u64 = 0x1234_5678_9abc;

pub(super) fn final_ecap() -> EcapReg {
    EcapReg::from(ECAP_VALUE)
}

pub(super) fn put(gm: &GuestMemory, address: u64, words: &[u64]) {
    for (index, word) in words.iter().enumerate() {
        gm.write_at(address + index as u64 * 8, &word.to_le_bytes())
            .unwrap();
    }
}

pub(super) fn walk(gm: &GuestMemory, levels: u8, leaf: u8, iova: u64, gpa: u64) -> Vec<u64> {
    walk_at(gm, SL_ROOT, levels, leaf, iova, gpa)
}

pub(super) fn walk_at(
    gm: &GuestMemory,
    root: u64,
    levels: u8,
    leaf: u8,
    iova: u64,
    gpa: u64,
) -> Vec<u64> {
    let mut entries = Vec::new();
    for level in (leaf..=levels).rev() {
        let table = root + u64::from(levels - level) * 4096;
        // Deliberately independent of SlPte's index/address helpers.
        let index = (iova >> (12 + 9 * (level - 1))) & 511;
        let address = table + index * 8;
        let value = if level == leaf {
            gpa | 3 | if leaf > 1 { 1 << 7 } else { 0 }
        } else {
            (table + 4096) | 3
        };
        put(gm, address, &[value]);
        entries.push(address);
    }
    entries
}

pub(super) struct Fixture {
    pub(super) dev: IntelVtdDevice,
    pub(super) gm: GuestMemory,
    pub(super) rid: u16,
    pub(super) entries: [u64; 4],
}

impl Fixture {
    pub(super) fn new(rid: u16, pasid: u32, pdts: u8) -> Self {
        let gm = GuestMemory::allocate(UNMAPPED as usize);
        let root = ROOT + u64::from(rid >> 8) * 16 + if rid & 128 == 0 { 0 } else { 8 };
        let table = if rid & 128 == 0 { LOWER } else { UPPER };
        let context = table + u64::from(rid & 127) * 32;
        let directory = DIRECTORY + u64::from(pasid >> 6) * 8;
        let pasid_entry = PASID_TABLE + u64::from(pasid & 63) * 64;
        put(&gm, root, &[table | 1]);
        put(
            &gm,
            context,
            &[
                DIRECTORY | 1 | (u64::from(pdts) << 9),
                u64::from(pasid),
                0,
                0,
            ],
        );
        put(&gm, directory, &[PASID_TABLE | 1]);
        put(
            &gm,
            pasid_entry,
            &[SL_ROOT | 1 | (2 << 2) | (2 << 6), 0xbeef, 0, 0, 0, 0, 0, 0],
        );
        walk(&gm, 4, 1, IOVA, GPA);
        let (mut dev, _) = IntelVtdDevice::new(
            gm.clone(),
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            Arc::new(TestSignalMsi),
        );
        write64(&mut dev, 0x020, ROOT | (1 << 10));
        write32(&mut dev, 0x018, (1 << 30) | (1 << 31));
        Self {
            dev,
            gm,
            rid,
            entries: [root, context, directory, pasid_entry],
        }
    }

    pub(super) fn word(&self, address: u64) -> u64 {
        u64::from_le(self.gm.read_plain(address).unwrap())
    }

    pub(super) fn fpd(&self, mask: u8) {
        for stage in 1..4 {
            let address = self.entries[stage];
            let value = (self.word(address) & !2) | (u64::from((mask >> (stage - 1)) & 1) << 1);
            put(&self.gm, address, &[value]);
        }
    }

    pub(super) fn translation_context(&self, iova: u64) -> TranslationContext {
        TranslationContext {
            source_id: self.rid,
            iova,
            mode: TranslationTableMode::SCALABLE,
            fpd: false,
        }
    }

    pub(super) fn resolve(&self) -> TranslationDescriptor {
        self.dev
            .shared
            .resolve_scalable(ROOT, self.translation_context(IOVA), final_ecap())
            .unwrap()
    }

    pub(super) fn translate(
        &self,
        iova: u64,
        write: bool,
    ) -> Result<u64, iommu_common::TranslationFault<VtdFault>> {
        self.dev
            .shared
            .translator()
            .translate(self.rid, iova, write, |gpa| {
                // Invalidation's write lock must also drain the DMA operation.
                assert!(self.dev.shared.state.try_write().is_none());
                gpa
            })
    }

    pub(super) fn assert_fault(&mut self, iova: u64, write: bool, reason: u8, suppressed: bool) {
        self.assert_fault_using(iova, write, reason, suppressed, None);
    }

    pub(super) fn translate_legacy(&self, iova: u64, write: bool) -> u64 {
        self.dev
            .shared
            .translator()
            .translate(self.rid, iova, write, |gpa| {
                assert!(self.dev.shared.state.try_write().is_none());
                gpa
            })
            .unwrap()
    }

    pub(super) fn assert_legacy_fault(
        &mut self,
        iova: u64,
        write: bool,
        reason: u8,
        suppressed: bool,
    ) {
        self.assert_fault_with_capabilities(
            iova,
            write,
            reason,
            suppressed,
            EcapReg::from(ECAP_VALUE),
        );
    }

    pub(super) fn assert_fault_with_capabilities(
        &mut self,
        iova: u64,
        write: bool,
        reason: u8,
        suppressed: bool,
        ecap: EcapReg,
    ) {
        self.assert_fault_using(iova, write, reason, suppressed, Some(ecap));
    }

    fn assert_fault_using(
        &mut self,
        iova: u64,
        write: bool,
        reason: u8,
        suppressed: bool,
        ecap: Option<EcapReg>,
    ) {
        write32(&mut self.dev, 0x12c, 1 << 31);
        write32(&mut self.dev, 0x034, u32::MAX);
        let previous = (read64(&mut self.dev, 0x120), read64(&mut self.dev, 0x128));
        let translator = self.dev.shared.translator();
        let op = |_| panic!("DMA operation ran on a fault");
        let fault = match ecap {
            Some(ecap) => translator.translate_with_capabilities(self.rid, iova, write, ecap, op),
            None => translator.translate(self.rid, iova, write, op),
        }
        .unwrap_err();
        assert_eq!(fault.iova, iova);
        assert_eq!(fault.error.source_id(), self.rid);
        assert_eq!(fault.error.fault_address(), iova);
        assert_eq!(fault.error.fault_reason().0, reason, "{fault:?}");
        assert_eq!(fault.error.fpd(), suppressed, "{fault:?}");
        let raw = (read64(&mut self.dev, 0x120), read64(&mut self.dev, 0x128));
        if suppressed {
            assert_eq!(raw, previous);
            assert_eq!(read32(&mut self.dev, 0x034) & 3, 0);
        } else {
            // FI retains the original request at 4KB granularity, with bits
            // above the largest advertised AGAW reserved (§11.4.7.6).
            // PP/PASID/PRIV/EXE stay zero: RID_PASID is not an explicit tag.
            assert_eq!(raw.0, iova & 0x0000_ffff_ffff_f000);
            assert_eq!(
                raw.1,
                (1 << 63)
                    | (u64::from(!write) << 62)
                    | (u64::from(reason) << 32)
                    | u64::from(self.rid)
            );
            assert_eq!(read32(&mut self.dev, 0x034) & 3, 2);
        }
    }

    pub(super) fn legacy(&mut self, levels: u8) {
        let context = LOWER + u64::from(self.rid & 255) * 16;
        put(
            &self.gm,
            ROOT + u64::from(self.rid >> 8) * 16,
            &[LOWER | 1, 0],
        );
        put(
            &self.gm,
            context,
            &[SL_ROOT | 1, u64::from(levels - 2) | (0xbeef << 8)],
        );
        write64(&mut self.dev, 0x020, ROOT);
        write32(&mut self.dev, 0x018, (1 << 30) | (1 << 31));
    }

    pub(super) fn set_levels(&self, levels: u8) {
        let address = self.entries[3];
        put(
            &self.gm,
            address,
            &[(self.word(address) & !(7 << 2)) | (u64::from(levels - 2) << 2)],
        );
    }
}
