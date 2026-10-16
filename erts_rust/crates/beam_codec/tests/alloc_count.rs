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

//! **Deterministic allocation attribution for the ETF codec.**
//!
//! This binary installs a thread-local counting global allocator (see
//! [`attrib_support`]) and pins the *exact* number of heap allocations the codec
//! performs for the benchmark payload of
//! `erts_rust/tools/bench/src/workloads/copy_bound.rs::module_source` — the `T` in
//!
//! ```erlang
//! payload() ->
//!     #{a => [1,2,3,4,5,6,7,8,9,10],
//!       b => {ok, <<"hello world">>, 3.5},
//!       c => <<1,2,3,4,5,6,7,8,9,10,11,12>>,
//!       d => "a char list",
//!       e => -42}.
//! ```
//!
//! It is the deterministic counterpart of the wall-clock attribution in
//! `examples/etf_attrib.rs`: **no timing is asserted here** (a timer assertion
//! would be flaky), only counts — so a regression that adds a per-term allocation
//! on the wire path fails this test immediately.
//!
//! Three measurements are pinned, because they answer three different questions:
//!
//! * `ENCODE_ALLOCS` — one `term_to_binary`-shaped encode ([`Encoder::new`] +
//!   `encode` + `finish`). Because [`Writer`] starts with zero capacity, this is
//!   dominated by the output buffer's *growth* (`Vec` doubling).
//! * `DECODE_FRESH_ALLOCS` — one `binary_to_term`-shaped decode ([`decode_term`])
//!   into a fresh [`Heap`] and a fresh [`AtomTable`]: the decoder's own
//!   per-container `Vec`s **plus** the atom table's name storage (one `String` into
//!   the name list and one into the lookup index per newly interned atom, plus the
//!   index's own growth).
//! * `DECODE_WARM_ALLOCS` — the same decode into an [`AtomTable`] that already
//!   holds every atom of the payload. `DECODE_FRESH_ALLOCS - DECODE_WARM_ALLOCS` is
//!   exactly the `AtomTable::intern` share of a cold decode — the part of the cost
//!   that belongs to `beam_term`, not to this crate.
//!
//! Deterministic only: the whole binary is a pure function of the codec's code.

mod attrib_support;

use beam_alloc::Heap;
use beam_term::prelude::AtomTable;

use attrib_support::{
    build_payload, census, counted, decode_image, encode_image, Census, HEAP_WORDS,
    PAYLOAD_ATOM_NAMES, PAYLOAD_KEY_NAMES,
};

/// One `Encoder::new` + `encode` + `finish` on the `etf_roundtrip` payload.
///
/// Before the writer-capacity work this was **7**: five `Vec` doublings for the
/// 101-byte image (`8,16,32,64,128`) plus one `repack_bits` buffer per binary
/// (two of them). The codec now reserves the image up front and writes a
/// byte-aligned binary straight from its source, so the encode allocates once.
const ENCODE_ALLOCS: u64 = 1;

/// One `decode_term` into a fresh `Heap` + a fresh `AtomTable`.
///
/// **21** before: 5 the decoder's own per-container vectors (see
/// `DECODE_WARM_ALLOCS`) and **16** the `AtomTable`'s (`intern` stores each of the
/// six names twice and the index grows) — i.e. 76% of the decode's allocations
/// belong to `beam_term`, not to this crate.
const DECODE_FRESH_ALLOCS: u64 = 21;

/// One `decode_term` into a `Heap` + an `AtomTable` that already holds the atoms.
///
/// This is the decoder's own allocation floor: the `Vec`s `decode_tuple`/
/// `decode_list`/`Tag::Map` build before copying the elements onto the heap.
/// Pinned so a regression that adds another per-term allocation is caught.
const DECODE_WARM_ALLOCS: u64 = 5;

#[test]
fn the_codec_allocation_counts_are_pinned() {
    let mut heap = Heap::new(HEAP_WORDS).expect("a small payload heap");
    let mut atoms = AtomTable::new();
    let term = build_payload(&mut heap, &mut atoms);

    // ---- shape / census (a self-check that the fixture is the benchmark's payload) ----
    let mut c = Census::default();
    census(term, &mut c);
    assert_eq!(c, Census::payload(), "the payload census is the benchmark's payload");

    // ---- encode ----
    let (image, encode_allocs) = counted(|| encode_image(term, &atoms).expect("the payload encodes"));
    assert_eq!(encode_allocs, ENCODE_ALLOCS, "encode allocations");

    // ---- decode into a fresh heap + a fresh AtomTable ----
    // The heap and the table outlive the counted closure: the decoded term borrows the
    // heap's storage, so dropping it there would leave a dangling `Eterm`.
    let mut dec_heap = Heap::new(HEAP_WORDS).expect("a small payload heap");
    let mut dec_atoms = AtomTable::new();
    let (decoded, decode_fresh_allocs) = counted(|| {
        decode_image(&image, &mut dec_heap, &mut dec_atoms).expect("our own image decodes")
    });
    assert_eq!(decode_fresh_allocs, DECODE_FRESH_ALLOCS, "fresh decode allocations");

    // The round trip is faithful.
    let mut c2 = Census::default();
    census(decoded, &mut c2);
    assert_eq!(c, c2, "the decoded payload has the payload's shape");

    // ---- decode into an AtomTable that already holds the payload's atoms ----
    let mut warm_atoms = AtomTable::new();
    for name in PAYLOAD_KEY_NAMES.iter().copied().chain(PAYLOAD_ATOM_NAMES[5..].iter().copied()) {
        warm_atoms.intern(name);
    }
    let mut warm_heap = Heap::new(HEAP_WORDS).expect("a small payload heap");
    let (_, decode_warm_allocs) = counted(|| {
        decode_image(&image, &mut warm_heap, &mut warm_atoms).expect("the image decodes warm")
    });
    assert_eq!(decode_warm_allocs, DECODE_WARM_ALLOCS, "warm decode allocations");

    eprintln!(
        "[beam_codec alloc_count] image = {} bytes; census = {} nodes {:?}; \
         encode allocs = {}; decode (fresh table) allocs = {}; decode (warm table) allocs = {} \
         (AtomTable::intern share = {})",
        image.len(),
        c.total(),
        c,
        encode_allocs,
        decode_fresh_allocs,
        decode_warm_allocs,
        decode_fresh_allocs - decode_warm_allocs,
    );
}
