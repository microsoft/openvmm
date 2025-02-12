// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of a buffer manager for managing memory blocks.
// The manager tracks the allocation and deallocation of buffers within a predefined
// memory region without actually allocating underlying memory. Its intended for use in
// single-threaded contexts or when external synchronization is required, as it does not protect its
// state from concurrent access.

use zerocopy::IntoBytes;

#[derive(Debug, Clone, Copy)]
pub struct BufferBlock {
    id: u64,
    offset: usize,
    size: usize,
    signature: u64, // Unique allocator signature
}

pub struct BufferManager {
    free_list: Vec<BufferBlock>,
    allocated: Vec<BufferBlock>,
    next_id: u64,
    signature: u64,
}

impl BufferManager {
    pub fn new(size: usize) -> Self {
        let mut signature: u64 = 0;
        getrandom::getrandom(signature.as_mut_bytes()).expect("crng failure");
        let free_list = vec![BufferBlock {
            id: 0,
            offset: 0,
            size,
            signature,
        }];
        Self {
            free_list,
            allocated: Vec::new(),
            next_id: 1,
            signature,
        }
    }

    pub fn malloc(&mut self, size: usize) -> Option<BufferBlock> {
        if let Some(pos) = self.free_list.iter().position(|block| block.size >= size) {
            let block = self.free_list.remove(pos);
            let allocated_id = self.next_id;

            self.next_id = self.next_id.wrapping_add(1);

            let allocated_block = BufferBlock {
                id: allocated_id,
                offset: block.offset,
                size,
                signature: self.signature,
            };

            self.allocated.push(allocated_block);
            self.allocated.sort_by_key(|b| b.offset);

            if block.size > size {
                self.free_list.insert(
                    pos,
                    BufferBlock {
                        id: 0,
                        offset: block.offset + size,
                        size: block.size - size,
                        signature: self.signature,
                    },
                );
            }

            Some(allocated_block)
        } else {
            None
        }
    }

    pub fn free(&mut self, buffer: BufferBlock) {
        if buffer.signature != self.signature {
            panic!("Attempted to free a buffer from a different allocator");
        }

        if let Some(pos) = self.allocated.iter().position(|&b| b.id == buffer.id) {
            let freed_block = self.allocated.remove(pos);
            self.insert_into_free_list(freed_block);
        }
    }

    fn insert_into_free_list(&mut self, new_block: BufferBlock) {
        // Insert the new block in the sorted list using binary search
        let pos = match self
            .free_list
            .binary_search_by_key(&new_block.offset, |b| b.offset)
        {
            Ok(pos) | Err(pos) => pos, // `Err(pos)` gives us the insertion point
        };
        self.free_list.insert(pos, new_block);

        // Merge with the previous block if adjacent
        if pos > 0
            && self.free_list[pos - 1].offset + self.free_list[pos - 1].size
                == self.free_list[pos].offset
        {
            self.free_list[pos - 1].size += self.free_list[pos].size;
            self.free_list.remove(pos);
        }

        // Merge with the next block if adjacent
        if pos < self.free_list.len() - 1
            && self.free_list[pos].offset + self.free_list[pos].size
                == self.free_list[pos + 1].offset
        {
            self.free_list[pos].size += self.free_list[pos + 1].size;
            self.free_list.remove(pos + 1);
        }

        // Check if the last block can be merged with the preceding block if the new block was inserted at the end
        if pos == self.free_list.len() - 1 && self.free_list.len() > 1 {
            if self.free_list[pos - 1].offset + self.free_list[pos - 1].size
                == self.free_list[pos].offset
            {
                self.free_list[pos - 1].size += self.free_list[pos].size;
                self.free_list.remove(pos);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocator_initialization() {
        let allocator = BufferManager::new(1024);
        assert_eq!(allocator.free_list.len(), 1);
        assert_eq!(allocator.free_list[0].size, 1024);
        assert_eq!(allocator.allocated.len(), 0);
    }

    #[test]
    #[should_panic(expected = "Attempted to free a buffer from a different allocator")]
    fn test_allocator_signature_mismatch() {
        let mut allocator = BufferManager::new(1024);

        let fake_block = BufferBlock {
            id: 0,
            offset: 0,
            size: 1024,
            signature: 0,
        };

        allocator.free(fake_block);
    }

    #[test]
    fn test_malloc_success() {
        let mut allocator = BufferManager::new(1024);
        let result = allocator.malloc(256);
        assert!(result.is_some());
        let buffer = result.unwrap();
        assert_eq!(buffer.offset, 0);
        assert_eq!(allocator.allocated.len(), 1);
        assert_eq!(allocator.free_list.len(), 1);
        assert_eq!(allocator.free_list[0].offset, 256);
        assert_eq!(allocator.free_list[0].size, 768);
    }

    #[test]
    fn test_malloc_fail() {
        let mut allocator = BufferManager::new(512);
        let result = allocator.malloc(1024);
        assert!(result.is_none());
    }

    #[test]
    fn test_free_merging() {
        let mut allocator = BufferManager::new(1024);
        let buffer1 = allocator.malloc(256).unwrap();
        let buffer2 = allocator.malloc(256).unwrap();
        allocator.free(buffer1);
        allocator.free(buffer2);
        assert_eq!(allocator.free_list.len(), 1);
        assert_eq!(allocator.free_list[0].size, 1024);
    }

    #[test]
    fn test_id_wrap_around() {
        let mut allocator = BufferManager::new(1024);
        allocator.next_id = u64::MAX;
        let buffer = allocator.malloc(256).unwrap();
        assert_eq!(buffer.id, u64::MAX);
        assert_eq!(allocator.next_id, 0); // Wrapped around to 0
    }

    #[test]
    fn test_out_of_order_allocation_and_free() {
        let mut allocator = BufferManager::new(1024);

        let buffer1 = allocator.malloc(256).unwrap();
        let buffer2 = allocator.malloc(512).unwrap();
        let buffer3 = allocator.malloc(128).unwrap();

        assert_eq!(buffer1.offset, 0);
        assert_eq!(buffer2.offset, 256);
        assert_eq!(buffer3.offset, 768);

        allocator.free(buffer2); // Free the middle block
        assert_eq!(allocator.free_list.len(), 2);

        let buffer4 = allocator.malloc(512).unwrap();
        assert_eq!(buffer4.offset, 256); // Should reuse freed block
    }

    #[test]
    fn test_freeing_non_contiguous_blocks() {
        let mut allocator = BufferManager::new(1024);

        let buffer1 = allocator.malloc(128).unwrap();
        let _ = allocator.malloc(256).unwrap();
        let buffer3 = allocator.malloc(128).unwrap();

        allocator.free(buffer1);
        allocator.free(buffer3);

        assert_eq!(allocator.free_list.len(), 2); // Two freed blocks should be separate
    }

    #[test]
    fn test_merge_blocks_after_out_of_order_free() {
        let mut allocator = BufferManager::new(1024);

        let buffer1 = allocator.malloc(256).unwrap();
        let buffer2 = allocator.malloc(256).unwrap();
        let buffer3 = allocator.malloc(512).unwrap();

        allocator.free(buffer1);
        allocator.free(buffer2);
        allocator.free(buffer3); // This should merge with previous free blocks

        assert_eq!(allocator.free_list.len(), 1); // Entire buffer should be free again
        assert_eq!(allocator.free_list[0].size, 1024);
    }
}
