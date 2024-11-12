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
#[derive(Debug, Inspect)]
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

    /// Extends this result collection with additional `leaves`, which are
    /// merged as in [`new`](Self::new).
    pub fn extend(&mut self, leaves: &[CpuidLeaf]) {
        self.leaves.extend(leaves);
        *self = Self::new(std::mem::take(&mut self.leaves));
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

    /// Updates an existing result to have the new value
    /// Returns false if the leaf was not found
    pub fn update_result(&mut self, eax: u32, ecx: u32, new_values: &[u32; 4]) -> bool {
        if let Some(x) = self.leaves.iter_mut().find(|x| x.matches(eax, ecx)) {
            x.result = *new_values;
            return true;
        }

        false
    }
}
