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
//!   `encode` + `finish`).
//! * `DECODE_FRESH_ALLOCS` — one `binary_to_term`-shaped decode ([`decode_term`])
//!   into a fresh [`Heap`] and a fresh [`AtomTable`].
//! * `DECODE_WARM_ALLOCS` — the same decode into an [`AtomTable`] that already holds
//!   every atom of the payload, so `DECODE_FRESH_ALLOCS - DECODE_WARM_ALLOCS` is
//!   exactly the `AtomTable::intern` share of a cold decode.
//!
//! Re-derive all three in seconds from the repository root:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_codec --test alloc_count -- --nocapture
//! ```
//!
//! then read the single `[beam_codec alloc_count] …` line and its `encode allocs`,
//! `decode (fresh table) allocs` and `decode (warm table) allocs` fields.
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
/// Exactly **1**, because the encoder reserves its output image up front and never grows it.
/// `Encoder::new_body` reserves `DEFAULT_IMAGE_CAPACITY` (128) bytes, above the 101-byte image.
/// The payload's two lists are byte lists, so `encode_list` writes them as `STRING_EXT` directly.
/// Both binaries are byte-aligned, so no `repack_bits` scratch buffer is allocated either.
/// A per-term allocation on the wire path, or a byte-unaligned binary, would make it regress.
const ENCODE_ALLOCS: u64 = 1;

/// One `decode_term` into a fresh `Heap` + a fresh `AtomTable`.
///
/// Exactly **13**: **3** the decoder's own per-container buffers plus **10** the `AtomTable`.
/// Each container buffer is pre-sized from its wire count clamped to
/// `CONTAINER_PREALLOC_CAP` (64), so each of them is one allocation:
/// the 3-tuple's element buffer (3 slots), the 5-pair map's pair buffer (5 slots)
/// and `beam_term::map::write_map`'s dedup-and-sort buffer.
/// The payload's two lists are byte lists, so they arrive as `STRING_EXT` and build no buffer.
/// The binary, the float and the smalls allocate nothing.
/// The remaining **10** belong to the `AtomTable`: one shared `Arc<str>` per novel name (6).
/// The other 4 are the two containers' growth: two `Vec` growths and two `HashMap` growths.
/// `intern` stores each novel name once as a shared `Arc<str>`.
/// Both the ordered `names` view and the `index` map reference that one allocation.
/// A per-term buffer added to the wire path would make it regress.
/// So would an `intern` that materializes each name once per container again.
const DECODE_FRESH_ALLOCS: u64 = 13;

/// One `decode_term` into a `Heap` + an `AtomTable` that already holds the atoms.
///
/// Exactly **3** — the decoder's own allocation floor, the same 3 a fresh decode spends:
/// one pre-sized buffer each for the tuple, the map pairs and `write_map`'s
/// dedup-and-sort scratch.
/// Every name the image carries is already interned.
/// `AtomTable::intern` then takes its `index.get` early return and allocates nothing.
/// The `AtomTable` share is therefore zero by construction.
/// The way this regresses is a new per-container buffer in the decoder, or a
/// container buffer that grows again instead of being pre-sized.
const DECODE_WARM_ALLOCS: u64 = 3;

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
