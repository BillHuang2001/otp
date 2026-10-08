// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

use super::*;

use crate::block::{BlockFlags, BlockHdr};

/// A `UNIT_SIZE`-aligned byte arena standing in for a carrier's mapping, carved
/// into blocks exactly the way `Carrier::subdivide_into` does it: a header at the
/// block start and the payload `BLOCK_HEADER_SIZE` bytes later.
#[repr(align(16))]
struct Mem([u64; 256]); // 2048 bytes

struct Carrier {
    mem: Box<Mem>,
}

impl Carrier {
    fn new() -> Carrier {
        Carrier {
            mem: Box::new(Mem([0u64; 256])),
        }
    }

    fn base(&mut self) -> *mut u8 {
        self.mem.0.as_mut_ptr().cast::<u8>()
    }

    /// Writes a block header of `size` bytes at `block_start` (a byte offset from
    /// the arena base) and returns that block's payload address.
    fn block(&mut self, block_start: usize, size: usize) -> *mut u8 {
        let base = self.base();
        let hdr = BlockHdr {
            size,
            payload_offset: 0,
            class: 0,
            carrier: ptr::null_mut(),
            flags: BlockFlags::empty().bits(),
        };
        assert!(
            block_start + size <= 2048,
            "the arena must hold every block"
        );
        // SAFETY: `block_start + BLOCK_HEADER_SIZE` lies inside the 2048-byte arena
        // for every call in these tests, the arena is 16-aligned and nothing else
        // aliases it.
        unsafe { BlockHeader::store(base.add(block_start), hdr) };
        // SAFETY: the payload starts inside the arena, right after the header.
        unsafe { base.add(block_start + BLOCK_HEADER_SIZE) }
    }
}

#[test]
fn new_list_is_empty_and_grows_on_reserve() {
    let mut lists = FreeList::new();
    assert_eq!(lists.class_count(), 0);
    assert_eq!(lists.free_blocks(0), 0);
    lists.reserve(2, 1).expect("class 2 is in range");
    assert_eq!(lists.class_count(), 3);
    for class in 0..3 {
        assert_eq!(lists.free_blocks(class), 0);
        assert_eq!(lists.largest_free(class), 0);
    }
    // SAFETY: no node was pushed, so popping an empty class yields `None`.
    assert!(unsafe { lists.pop(0) }.is_none());
    // An out-of-range class has no list at all.
    // SAFETY: `pop` checks the class index before touching any node.
    assert!(unsafe { lists.pop(usize::MAX) }.is_none());
}

#[test]
fn reserve_rejects_an_unencodable_class() {
    let mut lists = FreeList::new();
    assert!(matches!(
        lists.reserve(MAX_CLASS_COUNT, 1),
        Err(BeamError::BadArg)
    ));
    assert!(lists.reserve(MAX_CLASS_COUNT - 1, 1).is_ok());
}

#[test]
fn push_pop_is_lifo_and_tracks_counts() {
    let mut carrier = Carrier::new();
    let ptrs: Vec<*mut u8> = (0..4)
        .map(|index| carrier.block(index * MIN_FREE_BLOCK_SIZE, MIN_FREE_BLOCK_SIZE))
        .collect();
    let mut lists = FreeList::new();
    lists.reserve(0, 4).expect("class 0");
    for &ptr in &ptrs {
        // SAFETY: each payload is a distinct, 16-aligned free block of 32 bytes
        // whose header records its size.
        unsafe { lists.push(0, ptr) };
    }
    assert_eq!(lists.free_blocks(0), 4);
    assert_eq!(lists.largest_free(0), MIN_FREE_BLOCK_SIZE);
    for &expected in ptrs.iter().rev() {
        // SAFETY: the nodes are on class 0's list and each is popped once.
        let popped = unsafe { lists.pop(0) }.expect("non-empty");
        assert_eq!(popped, expected, "LIFO order");
    }
    assert_eq!(lists.free_blocks(0), 0);
    assert_eq!(lists.largest_free(0), 0);
    // SAFETY: the list is empty.
    assert!(unsafe { lists.pop(0) }.is_none());
}

#[test]
fn classes_are_independent() {
    let mut carrier = Carrier::new();
    let (a0, a1) = (carrier.block(0, 32), carrier.block(32, 32));
    let (b0, b1) = (carrier.block(64, 32), carrier.block(96, 32));
    let mut lists = FreeList::new();
    lists.reserve(1, 2).expect("class 1");
    // SAFETY: distinct, 16-aligned free blocks with real headers, per class.
    unsafe {
        lists.push(0, a0);
        lists.push(0, a1);
        lists.push(1, b0);
        lists.push(1, b1);
    }
    assert_eq!(lists.free_blocks(0), 2);
    assert_eq!(lists.free_blocks(1), 2);
    assert_eq!(lists.free_blocks(2), 0);
    // SAFETY: class 1's list is non-empty.
    assert_eq!(unsafe { lists.pop(1) }, Some(b1));
    assert_eq!(lists.free_blocks(0), 2, "class 0 is untouched");
    assert_eq!(lists.free_blocks(1), 1);
    // SAFETY: both lists are non-empty.
    assert_eq!(unsafe { lists.pop(0) }, Some(a1));
    assert_eq!(unsafe { lists.pop(1) }, Some(b0));
    assert_eq!(unsafe { lists.pop(0) }, Some(a0));
    assert_eq!(lists.free_blocks(0), 0);
    assert_eq!(lists.free_blocks(1), 0);
}

#[test]
fn pop_then_push_reuses_the_same_node() {
    let mut carrier = Carrier::new();
    let ptrs = [carrier.block(0, 32), carrier.block(32, 32)];
    let mut lists = FreeList::new();
    lists.reserve(0, 2).expect("class 0");
    // SAFETY: two distinct 16-aligned free blocks with real headers.
    unsafe {
        lists.push(0, ptrs[0]);
        lists.push(0, ptrs[1]);
    }
    // SAFETY: the list is non-empty.
    let first = unsafe { lists.pop(0) }.expect("non-empty");
    // SAFETY: `first` was just removed from the list and is free.
    unsafe { lists.push(0, first) };
    assert_eq!(lists.free_blocks(0), 2);
    // SAFETY: the list is non-empty, and popping twice empties it.
    assert_eq!(unsafe { lists.pop(0) }, Some(first));
    assert_eq!(unsafe { lists.pop(0) }, Some(ptrs[0]));
    assert_eq!(lists.free_blocks(0), 0);
}

#[test]
fn push_reads_the_size_out_of_the_block_header() {
    let mut carrier = Carrier::new();
    let small = carrier.block(0, 32);
    let big = carrier.block(32, 64);
    let mut lists = FreeList::new();
    lists.reserve(0, 2).expect("class 0");
    // SAFETY: both payloads are free blocks whose headers record 32 and 64 bytes.
    unsafe {
        lists.push(0, small);
        lists.push(0, big);
    }
    assert_eq!(lists.largest_free(0), 64, "the header size is what counts");
    assert_eq!(lists.free_blocks(0), 2);
    // SAFETY: both nodes are on class 0's list.
    assert_eq!(unsafe { FreeList::block_size(big) }, 64);
    // A 48-byte request can only be served by the 64-byte block.
    // SAFETY: class 0 is reserved and its nodes are intact free blocks.
    assert_eq!(unsafe { lists.take_fit(0, 48) }, Some(big));
    assert_eq!(lists.free_blocks(0), 1);
}

#[test]
fn insert_records_the_block_size_in_the_header() {
    let mut carrier = Carrier::new();
    // The header starts out claiming 32 bytes ...
    let ptr = carrier.block(0, 32);
    let mut lists = FreeList::new();
    lists.reserve(0, 1).expect("class 0");
    // SAFETY: `ptr` is a free block with a header written by `BlockHeader::store`.
    unsafe { lists.insert(0, ptr, 80) };
    // ... but inserting it with an explicit size makes that size the recorded one.
    // SAFETY: `ptr` is still a free block on class 0's list.
    assert_eq!(unsafe { FreeList::block_size(ptr) }, 80);
    assert_eq!(lists.largest_free(0), 80);
    assert_eq!(lists.free_blocks(0), 1);
    // SAFETY: as above.
    unsafe { FreeList::set_size(ptr, 96) };
    // SAFETY: as above.
    assert_eq!(unsafe { FreeList::block_size(ptr) }, 96);
}

#[test]
fn take_fit_picks_the_best_fit_within_the_search_window() {
    let mut carrier = Carrier::new();
    let p32 = carrier.block(0, 32);
    let p48 = carrier.block(32, 48);
    let p64 = carrier.block(80, 64);
    let p80 = carrier.block(144, 80);
    let mut lists = FreeList::new();
    lists.reserve(0, 4).expect("class 0");
    // SAFETY: four distinct free blocks of 32/48/64/80 bytes with real headers.
    unsafe {
        lists.insert(0, p32, 32);
        lists.insert(0, p48, 48);
        lists.insert(0, p64, 64);
        lists.insert(0, p80, 80);
    }
    // List order (head first) is now 80, 64, 48, 32.
    assert_eq!(lists.free_blocks(0), 4);
    assert_eq!(lists.largest_free(0), 80);

    // An exact match wins immediately.
    // SAFETY: class 0 is reserved and its nodes are intact free blocks.
    assert_eq!(unsafe { lists.take_fit(0, 48) }, Some(p48));
    // 32 is too small, so the *smallest* remaining block that fits (64, not 80) is
    // the good fit.
    // SAFETY: as above.
    assert_eq!(unsafe { lists.take_fit(0, 40) }, Some(p64));
    // SAFETY: as above.
    assert_eq!(unsafe { lists.take_fit(0, 32) }, Some(p32));
    assert_eq!(lists.free_blocks(0), 1);
    assert_eq!(lists.largest_free(0), 80);
    // Nothing left can serve a request bigger than the largest block.
    // SAFETY: as above.
    assert!(unsafe { lists.take_fit(0, 96) }.is_none());
    assert_eq!(lists.free_blocks(0), 1, "a rejected search takes nothing");
    // ... and the last block is still reachable by a smaller request.
    // SAFETY: as above.
    assert_eq!(unsafe { lists.take_fit(0, 33) }, Some(p80));
    assert_eq!(lists.free_blocks(0), 0);
    assert_eq!(lists.largest_free(0), 0);
}

#[test]
fn take_fit_falls_back_to_the_first_fit_beyond_the_search_window() {
    let mut carrier = Carrier::new();
    let p48 = carrier.block(0, 48);
    let p32a = carrier.block(48, 32);
    let p32b = carrier.block(80, 32);
    let p32c = carrier.block(112, 32);
    let mut lists = FreeList::new();
    lists.reserve(0, 4).expect("class 0");
    // Filing the 48-byte block *first* puts it beyond the three-node search window:
    // list order (head first) is 32, 32, 32, 48.
    // SAFETY: four distinct free blocks of 48/32/32/32 bytes with real headers.
    unsafe {
        lists.insert(0, p48, 48);
        lists.insert(0, p32a, 32);
        lists.insert(0, p32b, 32);
        lists.insert(0, p32c, 32);
    }
    // The window holds only 32-byte blocks, which cannot serve 40 bytes; the exact
    // largest-block record proves the 48-byte block exists, so the search continues
    // and finds it (first fit beyond the window) rather than reporting "no fit".
    // SAFETY: class 0 is reserved and its nodes are intact free blocks.
    assert_eq!(unsafe { lists.take_fit(0, 40) }, Some(p48));
    assert_eq!(lists.free_blocks(0), 3);
    assert_eq!(lists.largest_free(0), 32);
    // SAFETY: as above.
    assert!(
        unsafe { lists.take_fit(0, 40) }.is_none(),
        "only 32s remain"
    );
    assert_eq!(lists.free_blocks(0), 3);
    // SAFETY: as above.
    assert_eq!(unsafe { lists.take_fit(0, 32) }, Some(p32c));
}

#[test]
fn remove_unlinks_a_specific_block() {
    let mut carrier = Carrier::new();
    let p32 = carrier.block(0, 32);
    let p48 = carrier.block(32, 48);
    let p64 = carrier.block(80, 64);
    let p80 = carrier.block(144, 80);
    let mut lists = FreeList::new();
    lists.reserve(0, 4).expect("class 0");
    // SAFETY: four distinct free blocks with real headers (list order 80/64/48/32).
    unsafe {
        lists.insert(0, p32, 32);
        lists.insert(0, p48, 48);
        lists.insert(0, p64, 64);
        lists.insert(0, p80, 80);
    }
    // Remove the block in the middle of the list.
    // SAFETY: every block of class 0's list is intact.
    assert!(unsafe { lists.remove(0, p48) });
    assert_eq!(lists.free_blocks(0), 3);
    assert_eq!(lists.largest_free(0), 80);
    // Removing it again is a no-op (an unlinked node has null links).
    // SAFETY: `p48` is on no free list at all now.
    assert!(!unsafe { lists.remove(0, p48) });
    assert_eq!(lists.free_blocks(0), 3);
    // The survivors are exactly the other three, and the removed one is not among
    // them (it would have been preferred by the exact-size search).
    // SAFETY: class 0's remaining nodes are intact free blocks.
    assert_eq!(unsafe { lists.take_fit(0, 48) }, Some(p64));
    // SAFETY: as above.
    assert_eq!(unsafe { lists.take_fit(0, 32) }, Some(p32));
    // SAFETY: as above.
    assert_eq!(unsafe { lists.take_fit(0, 32) }, Some(p80));
    assert_eq!(lists.free_blocks(0), 0);
}

#[test]
fn remove_rejects_a_block_that_is_not_on_the_class() {
    let mut carrier = Carrier::new();
    let a = carrier.block(0, 32);
    let b = carrier.block(32, 32);
    let mut lists = FreeList::new();
    lists.reserve(1, 2).expect("classes 0 and 1");
    // SAFETY: two distinct free blocks with real headers.
    unsafe {
        lists.insert(0, a, 32);
        lists.insert(1, b, 32);
    }
    // `a` is the head of class 0, so asking class 1 to remove it must be refused
    // (its null `prev` proves it is not a node of class 1's list).
    // SAFETY: `a` is not a node of class 1's list.
    assert!(!unsafe { lists.remove(1, a) });
    assert_eq!(lists.free_blocks(0), 1, "class 0 is untouched");
    assert_eq!(lists.free_blocks(1), 1, "class 1 is untouched");
    // SAFETY: a null node and an untracked class are both refused without touching
    // memory.
    assert!(!unsafe { lists.remove(0, ptr::null_mut()) });
    // SAFETY: class 99 was never reserved.
    assert!(!unsafe { lists.remove(99, a) });
    // The real removals still work.
    // SAFETY: both nodes are on their lists.
    assert!(unsafe { lists.remove(0, a) });
    assert!(unsafe { lists.remove(1, b) });
    assert_eq!(lists.free_blocks(0), 0);
    assert_eq!(lists.free_blocks(1), 0);
}

#[test]
fn coalescing_two_adjacent_blocks_lets_a_bigger_request_fit() {
    let mut carrier = Carrier::new();
    // Two physically adjacent 32-byte blocks: `a` spans [0, 32) and `b` [32, 64).
    let a = carrier.block(0, 32);
    let b = carrier.block(32, 32);
    let mut lists = FreeList::new();
    lists.reserve(0, 2).expect("class 0");
    // SAFETY: two adjacent free blocks with real headers.
    unsafe {
        lists.insert(0, a, 32);
        lists.insert(0, b, 32);
    }
    assert_eq!(lists.free_blocks(0), 2);
    assert_eq!(lists.largest_free(0), 32);
    // Neither 32-byte block can satisfy a 48-byte request ...
    // SAFETY: class 0's nodes are intact free blocks.
    assert!(unsafe { lists.take_fit(0, 48) }.is_none());
    assert_eq!(lists.free_blocks(0), 2, "a rejected search takes nothing");

    // ... so coalesce them: `b` is absorbed by `a`, growing it to the 64 bytes the
    // pair spans, and the bigger block is re-filed with its new size.
    // SAFETY: `b` is a node of class 0's list.
    assert!(unsafe { lists.remove(0, b) });
    // SAFETY: `a` is a node of class 0's list and now spans 64 bytes.
    assert!(unsafe { lists.refile(0, 0, a, 64) });
    // SAFETY: `a` is still a free block.
    assert_eq!(unsafe { FreeList::block_size(a) }, 64);
    assert_eq!(lists.free_blocks(0), 1, "the two blocks became one");
    assert_eq!(lists.largest_free(0), 64);

    // The merged block can serve the request the two separate blocks could not.
    // SAFETY: class 0's remaining node is an intact 64-byte free block.
    assert_eq!(unsafe { lists.take_fit(0, 48) }, Some(a));
    assert_eq!(lists.free_blocks(0), 0);
    assert_eq!(lists.largest_free(0), 0);
}

#[test]
fn refile_moves_a_block_between_classes() {
    let mut carrier = Carrier::new();
    let a = carrier.block(0, 64);
    let mut lists = FreeList::new();
    lists.reserve(1, 1).expect("classes 0 and 1");
    // SAFETY: `a` is a free block with a real header.
    unsafe { lists.insert(0, a, 32) };
    assert_eq!(lists.largest_free(0), 32);
    // A coalesced block that outgrew its old class moves to the bigger one.
    // SAFETY: `a` is a node of class 0's list and now spans 64 bytes.
    assert!(unsafe { lists.refile(0, 1, a, 64) });
    assert_eq!(lists.free_blocks(0), 0);
    assert_eq!(lists.largest_free(0), 0);
    assert_eq!(lists.free_blocks(1), 1);
    assert_eq!(lists.largest_free(1), 64);
    // SAFETY: class 1's node is an intact 64-byte free block.
    assert_eq!(unsafe { lists.take_fit(1, 64) }, Some(a));
    assert_eq!(lists.free_blocks(1), 0);

    // A re-file into an untracked class is refused without losing the block.
    // SAFETY: `a` is a free block on no list at all now.
    unsafe { lists.insert(0, a, 32) };
    // SAFETY: as above (`a` is a node of class 0's list).
    assert!(!unsafe { lists.refile(0, 99, a, 64) });
    assert_eq!(lists.free_blocks(0), 1, "the block stayed where it was");
    // SAFETY: `a` is still filed in class 0.
    assert_eq!(unsafe { FreeList::block_size(a) }, 32);
    // A re-file of a block that is not on the source class is refused too.
    // SAFETY: `a` is now on no list at all.
    assert!(unsafe { lists.remove(0, a) });
    // SAFETY: as above.
    assert!(!unsafe { lists.refile(0, 1, a, 64) });
    assert_eq!(lists.free_blocks(0), 0);
    assert_eq!(lists.free_blocks(1), 0);
}

#[test]
fn largest_free_is_exact_across_inserts_and_removals() {
    let mut carrier = Carrier::new();
    let p32 = carrier.block(0, 32);
    let p64 = carrier.block(32, 64);
    let p80 = carrier.block(96, 80);
    let mut lists = FreeList::new();
    lists.reserve(0, 3).expect("class 0");
    // SAFETY: three distinct free blocks with real headers.
    unsafe {
        lists.insert(0, p32, 32);
        lists.insert(0, p64, 64);
        lists.insert(0, p80, 80);
    }
    assert_eq!(lists.largest_free(0), 80);
    // Removing the largest block forces a rescan: the record must follow.
    // SAFETY: every node of class 0's list is intact.
    assert!(unsafe { lists.remove(0, p80) });
    assert_eq!(lists.largest_free(0), 64);
    // SAFETY: as above.
    assert!(unsafe { lists.remove(0, p64) });
    assert_eq!(lists.largest_free(0), 32);
    // SAFETY: as above.
    assert!(unsafe { lists.remove(0, p32) });
    assert_eq!(lists.largest_free(0), 0);
    // An untracked class reports nothing rather than guessing.
    assert_eq!(lists.largest_free(7), 0);
    assert_eq!(lists.free_blocks(7), 0);
}

#[test]
fn empty_and_untracked_classes_report_zero_and_serve_nothing() {
    let mut lists = FreeList::new();
    lists.reserve(1, 1).expect("class 1");
    assert_eq!(lists.free_blocks(1), 0);
    assert_eq!(lists.largest_free(1), 0);
    assert_eq!(lists.free_blocks(7), 0, "class 7 was never reserved");
    assert_eq!(lists.largest_free(7), 0);
    // SAFETY: an empty class and an untracked class both yield `None` without
    // touching memory.
    assert!(unsafe { lists.take_fit(1, MIN_FREE_BLOCK_SIZE) }.is_none());
    // SAFETY: as above.
    assert!(unsafe { lists.take_fit(7, MIN_FREE_BLOCK_SIZE) }.is_none());
    // SAFETY: as above.
    assert!(unsafe { lists.take_fit(1, usize::MAX) }.is_none());
    // SAFETY: as above.
    assert!(unsafe { lists.pop(1) }.is_none());
}

#[test]
fn filing_and_unfiling_never_grows_the_bookkeeping() {
    // `FreeList`'s only heap allocation is the per-class slot vector, and `reserve`
    // is the only method that touches it. Filing (`insert`/`link_front`), unfiling
    // (`unlink`/`remove`/`take_fit`/`pop`) and re-filing (`refile`) write only into
    // the free blocks' own payloads and headers, so once the class is reserved the
    // slot vector can neither grow nor be reallocated.
    let mut carrier = Carrier::new();
    let small = carrier.block(0, 32);
    let big = carrier.block(32, 64);
    let mut lists = FreeList::new();
    lists.reserve(0, 2).expect("class 0");
    let classes = lists.class_count();
    let capacity = lists.slot_capacity();

    for _ in 0..256 {
        // SAFETY: both payloads are free blocks with real headers.
        unsafe {
            lists.insert(0, small, 32);
            lists.insert(0, big, 64);
        }
        assert_eq!(lists.free_blocks(0), 2);
        assert_eq!(lists.largest_free(0), 64);
        // SAFETY: both nodes are on class 0's list.
        assert_eq!(unsafe { lists.take_fit(0, 48) }, Some(big));
        // SAFETY: as above.
        assert_eq!(unsafe { lists.take_fit(0, 32) }, Some(small));
        assert_eq!(lists.free_blocks(0), 0);
        assert_eq!(lists.largest_free(0), 0);
    }

    assert_eq!(lists.class_count(), classes);
    assert_eq!(
        lists.slot_capacity(),
        capacity,
        "the per-class bookkeeping must not have been reallocated"
    );
}
