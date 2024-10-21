// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! BAR Management.

use crate::spec::cfg_space;
use inspect::Inspect;

/// A parsed BAR mapping.
#[derive(Debug, Inspect)]
pub struct BarMapping {
    /// Associated BAR register index
    pub index: u8,
    /// Base address of the mapping
    pub base_address: u64,
    /// Length of the mapping
    pub len: u64,
}

/// A set of parsed BAR mappings.
#[derive(Debug, Default)]
pub struct BarMappings(Vec<BarMapping>);

impl Inspect for BarMappings {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut res = req.respond();
        for bar in self.0.iter() {
            res.field(
                &format!("bar{}", bar.index),
                format!("{:#010x?}:{:#x?}", bar.base_address, bar.len),
            );
        }
    }
}

impl BarMappings {
    /// Parses a set of BARs into mappings.
    pub fn parse(base_addresses: &[u32; 6], bar_masks: &[u32; 6]) -> Self {
        let mut mappings = Vec::new();
        let mut i = 0;
        while i < base_addresses.len() {
            let bar_address;
            let mut bar_mask;
            let len;
            if bar_masks[i] & cfg_space::BarEncodingBits::TYPE_64_BIT.bits() != 0 {
                bar_mask = (bar_masks[i + 1] as u64) << 32 | bar_masks[i] as u64;
                bar_address = (base_addresses[i + 1] as u64) << 32 | base_addresses[i] as u64;
                len = 2;
            } else {
                bar_mask = bar_masks[i] as i32 as i64 as u64; // sign extend
                bar_address = base_addresses[i] as u64;
                len = 1;
            };
            bar_mask &= !0xf;
            if bar_mask != 0 {
                mappings.push(BarMapping {
                    index: i as u8,
                    base_address: bar_address & bar_mask,
                    len: !bar_mask + 1,
                });
            }
            i += len;
        }
        Self(mappings)
    }

    /// Finds a BAR + offset by address.
    pub fn find(&self, address: u64) -> Option<(u8, u16)> {
        for bar_mapping in self.0.iter() {
            if address >= bar_mapping.base_address
                && address - bar_mapping.base_address < bar_mapping.len
            {
                return Some((
                    bar_mapping.index,
                    (address - bar_mapping.base_address).try_into().unwrap(),
                ));
            }
        }
        None
    }

    /// Gets the base address configured for `bar`.
    pub fn get(&self, bar: u8) -> Option<u64> {
        for bar_mapping in self.0.iter() {
            if bar_mapping.index == bar {
                return Some(bar_mapping.base_address);
            }
        }
        None
    }

    /// Returns an iterator through the mappings.
    pub fn iter(&self) -> impl Iterator<Item = &BarMapping> {
        self.0.iter()
    }
}
