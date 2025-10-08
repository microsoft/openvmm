// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]
// UNSAFETY: Manipulating IoBuffers for the crate to use.
#![expect(unsafe_code)]
#![expect(missing_docs)]

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use guestmem::GuestMemory;
use guestmem::ranges::PagedRange;
use scsi_buffers::BounceBuffer;
use scsi_buffers::IoBuffer;
use scsi_buffers::RequestBuffers;
use xtask_fuzz::fuzz_target;

const GUEST_MEM_SIZE: usize = 0x1000 * 100;

#[derive(Debug, Arbitrary)]
struct BounceBufferCase {
    #[arbitrary(with = |u: &mut Unstructured<'_>| u.int_in_range(0..=0x40000))]
    size: usize,
}

impl BounceBufferCase {
    fn to_bounce_buffer(&self) -> BounceBuffer {
        BounceBuffer::new(self.size)
    }
}

#[derive(Debug, Arbitrary)]
struct PagedRangeCase {
    is_write: bool,
    start: usize,
    len: usize,
    backing: Vec<u64>,
}

impl PagedRangeCase {
    fn to_paged_range(&self) -> Option<PagedRange<'_>> {
        PagedRange::new(self.start, self.len, &self.backing[..])
    }
}

#[derive(Debug, Arbitrary)]
enum ScsiBufferFuzzCase {
    RequestBuffers(PagedRangeCase),
    BounceBuffer(BounceBufferCase),
}

// simulate that likely usecase of 'allocate IoBuffer, then pass it down to a
// libc function or system call expecting an iovec-like thing'
//
// The 'A' here is completely arbitrary. It can be any value. The goal here is
// only to have the fuzzer access the buffer according to the the ptr and
// length. We could get rid of the 'A' and instead cause the access by reading
// from the buffer, that may be less confusing.
fn access_all_io_buffers(io_buffers: &[IoBuffer<'_>]) {
    for buffer in io_buffers.iter() {
        // SAFETY: we know that the buffer is valid for the lifetime of the IoBuffer,
        // and writing arbitrary data to it is ok, as it's just bytes.
        unsafe { (buffer.as_ptr() as *mut u8).write_bytes(b'A', buffer.len()) }
    }
}

// This fuzzer targets both the RequestBuffers and BounceBuffer implementations as they
// rely on unsafe code to function while presenting a safe abstraction.
fn do_fuzz(fuzz_case: ScsiBufferFuzzCase) {
    // No need to populate memory for SCSI buffers
    let test_guest_mem = GuestMemory::allocate(GUEST_MEM_SIZE);

    match fuzz_case {
        ScsiBufferFuzzCase::RequestBuffers(range) => {
            let paged_range = range.to_paged_range();

            if let Some(paged_range) = paged_range {
                let request_buffer =
                    RequestBuffers::new(&test_guest_mem, paged_range, range.is_write);
                let locked_io_buffers = request_buffer.lock(range.is_write);
                if let Ok(locked_io_buffers) = locked_io_buffers {
                    let io_buffers = locked_io_buffers.io_vecs();
                    access_all_io_buffers(io_buffers);
                }
            }
        }
        ScsiBufferFuzzCase::BounceBuffer(buffer) => {
            let mut bounce_buffer = buffer.to_bounce_buffer();
            let bytes = bounce_buffer.as_mut_bytes();
            bytes.fill(b'A');
            let io_buffers = bounce_buffer.io_vecs();
            access_all_io_buffers(io_buffers);
        }
    }
}

fuzz_target!(|fuzz_case: ScsiBufferFuzzCase| do_fuzz(fuzz_case));
