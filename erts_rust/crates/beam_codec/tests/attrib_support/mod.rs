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

//! Shared support for the codec's deterministic (and wall-clock) attribution.
//!
//! Included by the integration test `tests/alloc_count.rs` and, through
//! `#[path]`, by the manually-run binary `examples/etf_attrib.rs`. It is a
//! directory module rather than a `tests/*.rs` file on purpose: cargo would
//! auto-discover a top-level `.rs` file in `tests/` as its own test target (the
//! same reason `property_support/` is a directory).
//!
//! It owns three things, so neither consumer duplicates them:
//!
//! * the thread-local **counting global allocator** and its `counted`
//!   measurement helper;
//! * the **benchmark payload** — the `T` of the `copy_bound` workload's
//!   `etf_roundtrip` case, built here from `copy_bound.rs`'s own constants so the
//!   Rust and Erlang sides cannot drift;
//! * the **term census** that proves the fixture really is that payload.
#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{
    is_atom, is_list, is_nil, is_small, make_atom, make_small, AtomTable, Eterm, NIL,
};

use beam_codec::term::{
    bitstring_bytes, cons_head, cons_tail, flatmap_parts, float_value, make_binary, make_float,
    make_map, tuple_arity, tuple_element,
};
use beam_codec::Encoder;

// ---------------------------------------------------------------------------
// The counting global allocator
// ---------------------------------------------------------------------------

thread_local! {
    /// Allocations (including reallocations) performed by the current thread.
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

/// Wraps [`System`] and counts every request on the thread that made it.
///
/// The counter is thread-local so a test binary's other (parallel) tests cannot
/// pollute a measurement, and it is *always* on (no enable/disable flag to leak):
/// a measurement is the delta of two reads on the measuring thread.
pub struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: `System` is the process allocator; the layout is forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr`/`layout` came from a matching `System` call.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // A `Vec` growth is one `realloc` — counted once, exactly as the allocator sees it.
        ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: `ptr`/`layout` came from a matching `System` call and `new_size` is the
        // requested new size.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// The process's allocator for any crate that includes this module.
///
/// `#[global_allocator]` is a crate-wide declaration, so it applies whether this
/// module is included by the integration test or by the attribution example.
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// The number of allocations this thread has made so far.
pub fn allocs() -> u64 {
    ALLOCS.with(|c| c.get())
}

/// Runs `f` and returns `(result, allocations)`.
pub fn counted<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = allocs();
    let value = f();
    (value, allocs() - before)
}

// ---------------------------------------------------------------------------
// The benchmark payload (mirrors `copy_bound.rs`'s constants exactly)
// ---------------------------------------------------------------------------

/// `PAYLOAD_LIST_LEN`.
pub const PAYLOAD_LIST_LEN: i64 = 10;
/// `PAYLOAD_BIN_BYTES` — `<<1,2,…,12>>`.
pub const PAYLOAD_BIN_BYTES: i64 = 12;
/// `PAYLOAD_FLOAT`.
pub const PAYLOAD_FLOAT: f64 = 3.5;
/// `PAYLOAD_STRING`.
pub const PAYLOAD_STRING: &str = "a char list";
/// `PAYLOAD_NEG`.
pub const PAYLOAD_NEG: i64 = -42;
/// `PAYLOAD_KEYS`.
pub const PAYLOAD_KEYS: usize = 5;

/// The payload's keys, in the order `payload/0` writes them.
pub const PAYLOAD_KEY_NAMES: [&str; PAYLOAD_KEYS] = ["a", "b", "c", "d", "e"];

/// Every atom the payload contains (`ok` is the tuple's first element).
pub const PAYLOAD_ATOM_NAMES: [&str; 6] = ["a", "b", "c", "d", "e", "ok"];

/// A heap big enough for the payload and a decoded copy of it.
pub const HEAP_WORDS: usize = 1024;

/// Builds the payload term exactly as `payload/0` describes it.
///
/// All six of its atoms (the five keys plus `ok`) end up interned in `atoms`, so the
/// returned term encodes without any further interning.
pub fn build_payload(heap: &mut Heap, atoms: &mut AtomTable) -> Eterm {
    let list = {
        let mut tail = NIL;
        for n in (1..=PAYLOAD_LIST_LEN).rev() {
            tail = heap.cons(make_small(n), tail).expect("heap room for the list");
        }
        tail
    };
    let ok = make_atom(atoms.intern("ok"));
    let hello = make_binary(heap, b"hello world").expect("heap room for the tuple binary");
    let float = make_float(heap, PAYLOAD_FLOAT).expect("heap room for the float");
    let tuple = heap
        .tuple(&[ok, hello, float])
        .expect("heap room for the tuple");
    let binary: Vec<u8> = (1..=PAYLOAD_BIN_BYTES as u8).collect();
    let bin = make_binary(heap, &binary).expect("heap room for the payload binary");
    let string = {
        let mut tail = NIL;
        for &b in PAYLOAD_STRING.as_bytes().iter().rev() {
            tail = heap
                .cons(make_small(i64::from(b)), tail)
                .expect("heap room for the string");
        }
        tail
    };
    let values = [list, tuple, bin, string, make_small(PAYLOAD_NEG)];
    let pairs: Vec<(Eterm, Eterm)> = PAYLOAD_KEY_NAMES
        .iter()
        .zip(values)
        .map(|(name, value)| (make_atom(atoms.intern(name)), value))
        .collect();
    make_map(heap, &pairs, atoms).expect("heap room for the map")
}

/// Encodes `term` exactly as `term_to_binary/1` does and returns the image.
pub fn encode_image(term: Eterm, atoms: &AtomTable) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new(atoms);
    encoder.encode(term)?;
    Ok(encoder.finish())
}

/// Decodes `image` exactly as `binary_to_term/1` does.
pub fn decode_image(
    image: &[u8],
    heap: &mut Heap,
    atoms: &mut AtomTable,
) -> Result<Eterm> {
    beam_codec::decode_term(image, heap, atoms)
}

// ---------------------------------------------------------------------------
// The term census
// ---------------------------------------------------------------------------

/// A term census, by tag family, of the payload's structure.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Census {
    pub small: u64,
    pub atom: u64,
    pub nil: u64,
    pub cons: u64,
    pub tuple: u64,
    pub map: u64,
    pub binary: u64,
    pub float: u64,
}

impl Census {
    /// The number of term nodes counted.
    pub fn total(&self) -> u64 {
        self.small
            + self.atom
            + self.nil
            + self.cons
            + self.tuple
            + self.map
            + self.binary
            + self.float
    }

    /// The payload's expected census: 5 keys + `ok` = 6 atoms; 10 list elements +
    /// 11 string bytes + the `-42` = 22 smalls; two proper-list NILs; 21 cons cells;
    /// one 3-tuple; one 5-pair map; two binaries; one float.
    pub fn payload() -> Census {
        Census {
            small: 22,
            atom: 6,
            nil: 2,
            cons: 21,
            tuple: 1,
            map: 1,
            binary: 2,
            float: 1,
        }
    }
}

/// Walks `term` and counts its nodes by tag family. Panics on a term the payload
/// cannot contain, so the census can never silently under-count.
pub fn census(term: Eterm, c: &mut Census) {
    if is_small(term) {
        c.small += 1;
        return;
    }
    if is_atom(term) {
        c.atom += 1;
        return;
    }
    if is_nil(term) {
        c.nil += 1;
        return;
    }
    if is_list(term) {
        let mut cursor = term;
        while is_list(cursor) {
            c.cons += 1;
            census(cons_head(cursor).expect("a cons head"), c);
            cursor = cons_tail(cursor).expect("a cons tail");
        }
        // `NIL` for a proper list (the payload's only list shape); anything else would
        // be an improper tail, which the payload never has.
        assert!(is_nil(cursor), "the payload has no improper list");
        c.nil += 1;
        return;
    }
    if let Ok(arity) = tuple_arity(term) {
        c.tuple += 1;
        for index in 0..arity {
            census(tuple_element(term, index).expect("a tuple element"), c);
        }
        return;
    }
    if let Ok((keys, values)) = flatmap_parts(term) {
        c.map += 1;
        for key in keys {
            census(*key, c);
        }
        for value in values {
            census(*value, c);
        }
        return;
    }
    if float_value(term).is_ok() {
        c.float += 1;
        return;
    }
    if bitstring_bytes(term).is_ok() {
        c.binary += 1;
        return;
    }
    panic!("the payload contains a term family the census does not model");
}
