// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPUID definitions.

use inspect::Inspect;

/// A CPUID result.
///
/// This may define a partial result if some mask bits are zero. This is used to
/// provide an update on top of another CPUID result provided elsewhere (e.g. by
/// the hypervisor).
#[derive(Debug, Copy, Clone)]
pub struct CpuidLeaf {
    /// The CPUID function/leaf, provided in eax.
    pub function: u32,
    /// The CPUID index/subleaf, provided in ecx. If `None`, any index value is
    /// accepted.
    pub index: Option<u32>,
    /// The result.
    pub result: [u32; 4],
    /// The bits of the result that are valid.
    pub mask: [u32; 4],
}

impl CpuidLeaf {
    /// Returns a new result for the given function.
    pub fn new(function: u32, result: [u32; 4]) -> Self {
        Self {
            function,
            index: None,
            result,
            mask: [!0; 4],
        }
    }

    /// Updates the result to be for specific `index` and returns it.
    pub fn indexed(self, index: u32) -> Self {
        Self {
            index: Some(index),
            ..self
        }
    }

    /// Updates the result to be partial with the provided mask.
    pub fn masked(self, mask: [u32; 4]) -> Self {
        Self { mask, ..self }
    }

    fn cmp_key(&self, other: &Self) -> std::cmp::Ordering {
        (self.function, self.index).cmp(&(other.function, other.index))
    }

    /// Returns true if this result is intended for the given `eax` and `ecx`
    /// input values.
    pub fn matches(&self, eax: u32, ecx: u32) -> bool {
        self.function == eax && (self.index.is_none() || self.index == Some(ecx))
    }

    /// Applies this result to `result`, replacing bits in `result` with
    /// `self.result` when the corresponding bits in `self.mask` are set.
    pub fn apply(&self, result: &mut [u32; 4]) {
        for ((x, y), m) in result.iter_mut().zip(self.result).zip(self.mask) {
            *x &= !m;
            *x |= y & m;
        }
    }

    fn inspect_kv(&self) -> (String, impl '_ + Inspect) {
        let key = if let Some(index) = self.index {
            format!("{:#x}/{:#x}", self.function, index)
        } else {
            format!("{:#x}", self.function)
        };
        (
            key,
            inspect::adhoc(|req| {
                let mut resp = req.respond();
                resp.hex("eax", self.result[0])
                    .hex("ebx", self.result[1])
                    .hex("ecx", self.result[2])
                    .hex("edx", self.result[3]);
                if self.mask != [!0, !0, !0, !0] {
                    resp.hex("eax_mask", self.mask[0])
                        .hex("ebx_mask", self.mask[1])
                        .hex("ecx_mask", self.mask[2])
                        .hex("edx_mask", self.mask[3]);
                }
            }),
        )
    }
}

/// A collection of CPUID results.
#[derive(Debug, Inspect, Default)]
pub struct CpuidLeafSet {
    #[inspect(
        flatten,
        with = "|x| inspect::iter_by_key(x.iter().map(|y| y.inspect_kv()))"
    )]
    leaves: Vec<CpuidLeaf>,
}

impl CpuidLeafSet {
    /// Returns a new result set.
    ///
    /// `leaves` may contain multiple results for the same function and index.
    /// In this case, they are merged internally (respecting their mask bits),
    /// with later leaves overriding earlier ones.
    pub fn new(mut leaves: Vec<CpuidLeaf>) -> Self {
        // Sort and combine entries. Note that this must be a stable sort to
        // preserve ordering.
        leaves.sort_by(|x, y| x.cmp_key(y));
        leaves.dedup_by(|right, left| {
            if left.cmp_key(right).is_ne() {
                return false;
            }
            right.apply(&mut left.result);
            for (x, y) in left.mask.iter_mut().zip(right.mask) {
                *x |= y;
            }
            true
        });
        Self { leaves }
    }

    /// Returns the merged leaves.
    pub fn into_leaves(self) -> Vec<CpuidLeaf> {
        self.leaves
    }

    /// Returns the merged leaves.
    pub fn leaves(&self) -> &[CpuidLeaf] {
        &self.leaves
    }

    /// Returns the result value to return for inputs `eax` and `ecx`.
    ///
    /// `default` provides the base value which is used for a missing leaf or
    /// for any bits of the result whose mask bits are clear.
    pub fn result(&self, eax: u32, ecx: u32, default: &[u32; 4]) -> [u32; 4] {
        let mut result = *default;
        if let Some(x) = self.leaves.iter().find(|x| x.matches(eax, ecx)) {
            x.apply(&mut result);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    const DEFAULT: [u32; 4] = [0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444];

    // Test: Look up a leaf that has no entry in the set.
    // Expected: The caller's default result is returned unchanged.
    #[test]
    fn missing_leaf_returns_default() {
        let set = CpuidLeafSet::new(vec![CpuidLeaf::new(1, [1, 2, 3, 4])]);

        assert_eq!(set.result(2, 0, &DEFAULT), DEFAULT);
    }

    // Test: Look up a leaf whose entry masks ECX bit 20 (set in the default)
    //       and bit 22 (clear in the default), with the entry clearing bit 20
    //       and setting every other bit.
    // Expected: Bit 20 is cleared and bit 22 is set; all other bits come from
    //       the default.
    #[test]
    fn masked_leaf_overrides_only_masked_bits() {
        let set = CpuidLeafSet::new(vec![CpuidLeaf::new(1, [0, 0, !(1 << 20), 0]).masked([
            0,
            0,
            1 << 20 | 1 << 22,
            0,
        ])]);

        let result = set.result(1, 0, &DEFAULT);

        assert_eq!(result, [DEFAULT[0], DEFAULT[1], 0x3363_3333, DEFAULT[3]]);
    }

    // Test: Merge two entries for the same leaf and subleaf, where the later
    //       entry overlaps some of the earlier entry's bits.
    // Expected: Each entry's masked bits come from that entry, with the later
    //       entry winning where they overlap; bits in neither mask come
    //       from the default.
    #[test]
    fn later_leaf_overrides_earlier_leaf_within_its_mask() {
        let set = CpuidLeafSet::new(vec![
            CpuidLeaf::new(7, [0xaaaa_0000, 0, 0, 0]).masked([0xffff_0000, 0, 0, 0]),
            CpuidLeaf::new(7, [0x00bb_00cc, 0, 0, 0]).masked([0x00ff_00ff, 0, 0, 0]),
        ]);

        assert_eq!(
            set.result(7, 0, &[0x1234_5678, 0, 0, 0])[0],
            0xaabb_56cc,
            "later entry must win only within its mask"
        );
    }

    // Test: Look up different subleaves of a leaf that has subleaf-specific
    //       entries.
    // Expected: Each entry applies only to its own subleaf; other subleaves
    //       return the default.
    #[test]
    fn indexed_leaf_matches_only_its_subleaf() {
        let set = CpuidLeafSet::new(vec![
            CpuidLeaf::new(0xb, [1, 1, 1, 1]).indexed(0),
            CpuidLeaf::new(0xb, [2, 2, 2, 2]).indexed(1),
        ]);

        assert_eq!(set.result(0xb, 0, &DEFAULT), [1, 1, 1, 1]);
        assert_eq!(set.result(0xb, 1, &DEFAULT), [2, 2, 2, 2]);
        assert_eq!(set.result(0xb, 2, &DEFAULT), DEFAULT);
    }

    // Test: Look up a leaf whose entry accepts any subleaf.
    // Expected: The entry applies to every subleaf of that leaf.
    #[test]
    fn unindexed_leaf_matches_every_subleaf() {
        let set = CpuidLeafSet::new(vec![CpuidLeaf::new(1, [5, 6, 7, 8])]);

        for ecx in [0, 1, 0xffff_ffff] {
            assert_eq!(
                set.result(1, ecx, &DEFAULT),
                [5, 6, 7, 8],
                "subleaf {ecx:#x}"
            );
        }
    }

    // Test: Build a set from two entries for leaf 7 subleaf 0 (merged) and an
    //       unmasked entry for leaf 1, then list and consume its leaves.
    // Expected: Both APIs return the merged values, masks, and subleaf indices.
    #[test]
    fn leaf_set_lists_merged_leaves_and_masks() {
        let set = CpuidLeafSet::new(vec![
            CpuidLeaf::new(7, [0xa, 0, 0, 0])
                .indexed(0)
                .masked([0xf, 0, 0, 0]),
            CpuidLeaf::new(1, [0x906ea, 0, 0, 0]),
            CpuidLeaf::new(7, [0xb0, 0, 0, 0])
                .indexed(0)
                .masked([0xf0, 0, 0, 0]),
        ]);

        let expected = [
            (1, None, [0x906ea, 0, 0, 0], [!0; 4]),
            (7, Some(0), [0xba, 0, 0, 0], [0xff, 0, 0, 0]),
        ];
        let leaf_state = |leaf: &CpuidLeaf| (leaf.function, leaf.index, leaf.result, leaf.mask);
        assert_eq!(
            set.leaves().iter().map(leaf_state).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(set.result(7, 0, &[0; 4])[0], 0xba);
        assert_eq!(
            set.into_leaves().iter().map(leaf_state).collect::<Vec<_>>(),
            expected
        );
    }
}
