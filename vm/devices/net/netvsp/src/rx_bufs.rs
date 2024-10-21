// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data structure for tracking receive buffer state.

use thiserror::Error;

/// State of networking receive buffers.
pub struct RxBuffers {
    /// Chains together rx receive buffers that are used as part of the same
    /// VMBus request. `state[i]` specifies the index of the next receive buffer
    /// in the request, or `END` if `i` is the last buffer. The beginning of
    /// each chain has `state[id] & START_MASK == START_MASK`. `INVALID`
    /// indicates the buffer is not in use.
    state: Vec<u32>,
}

const START_MASK: u32 = 0x80000000;
const INVALID: u32 = !START_MASK;
const END: u32 = !1 & !START_MASK;

#[derive(Debug, Error)]
#[error("suballocation is already in use")]
pub struct SubAllocationInUse;

impl RxBuffers {
    pub fn new(count: u32) -> Self {
        Self {
            state: (0..count).map(|_| INVALID).collect(),
        }
    }

    pub fn is_free(&self, id: u32) -> bool {
        self.state[id as usize] == INVALID
    }

    pub fn allocate<I: Iterator<Item = u32> + Clone>(
        &mut self,
        ids: impl IntoIterator<Item = u32, IntoIter = I>,
    ) -> Result<(), SubAllocationInUse> {
        let ids = ids.into_iter();
        let first = ids.clone().next().unwrap();
        let next_ids = ids.clone().skip(1).chain(std::iter::once(END));
        for (n, (id, next_id)) in ids.clone().zip(next_ids).enumerate() {
            if self.state[id as usize] != INVALID {
                for id in ids.take(n) {
                    self.state[id as usize] = INVALID;
                }
                return Err(SubAllocationInUse);
            }
            self.state[id as usize] = next_id;
        }
        self.state[first as usize] |= START_MASK;
        Ok(())
    }

    pub fn free(&mut self, id: u32) -> Option<FreeIterator<'_>> {
        let next = self.state.get(id as usize)?;
        if next & START_MASK == 0 {
            return None;
        }
        Some(FreeIterator {
            id,
            state: &mut self.state,
        })
    }

    pub fn allocated(&self) -> RxIterator<'_> {
        RxIterator {
            id: 0,
            chained_rx_id: &self.state,
        }
    }
}

pub struct RxIterator<'a> {
    id: usize,
    chained_rx_id: &'a Vec<u32>,
}

impl<'a> Iterator for RxIterator<'a> {
    type Item = ReadIterator<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.id < self.chained_rx_id.len() {
            let id = self.id;
            self.id += 1;
            if self.chained_rx_id[id] & START_MASK != 0 {
                return Some(ReadIterator {
                    id: id as u32,
                    state: self.chained_rx_id,
                });
            }
        }
        None
    }
}

pub struct ReadIterator<'a> {
    id: u32,
    state: &'a Vec<u32>,
}

impl Iterator for ReadIterator<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        let id = self.id;
        if id == END {
            return None;
        }
        self.id = self.state[id as usize] & !START_MASK;
        Some(id)
    }
}

pub struct FreeIterator<'a> {
    id: u32,
    state: &'a mut Vec<u32>,
}

impl Iterator for FreeIterator<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        let id = self.id;
        if id == END {
            return None;
        }
        self.id = self.state[id as usize] & !START_MASK;
        self.state[id as usize] = INVALID;
        Some(id)
    }
}

impl Drop for FreeIterator<'_> {
    fn drop(&mut self) {
        while self.next().is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::RxBuffers;

    #[test]
    fn test_rx_bufs() {
        let mut bufs = RxBuffers::new(20);
        bufs.allocate([0, 1, 2]).unwrap();
        bufs.allocate([6, 9, 5]).unwrap();
        bufs.allocate([3, 10, 15, 0, 4]).unwrap_err();
        bufs.allocate([3, 10, 12]).unwrap();
        assert!(!bufs.is_free(1));
        assert!(!bufs.is_free(3));
        assert!(bufs.is_free(4));
        assert!(bufs.free(9).is_none());
        assert!(bufs.free(12).is_none());
        assert!(bufs.free(6).unwrap().eq([6, 9, 5]));
        assert!(bufs
            .allocated()
            .map(Vec::from_iter)
            .eq([[0, 1, 2], [3, 10, 12]]));
    }
}
