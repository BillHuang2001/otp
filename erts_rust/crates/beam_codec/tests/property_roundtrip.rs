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

//! Deterministic property/fuzz corpus for the **whole producible ETF tag set** —
//! the oracle-independent byte-identity half.
//!
//! See [`property_support`] for the shared deterministic generator, the fixed seed
//! and the exact scope of the corpus (which tag kinds are generated, and the two
//! documented exclusions). This file asserts the property:
//!
//! ```text
//!   encode(term)  ->  decode(fresh Heap + AtomTable)  ->  encode  ==  encode(term)
//! ```
//!
//! byte-identically **for every `{minor_version, N}` flavour** (`V0`/`V1`/`V2`) and for
//! the default image, over the whole corpus, and that the corpus really reaches the
//! producible surface (every generated kind and every top-level wire tag appears).
//!
//! The live oracle cross-checks (`term_to_binary(T)` byte-for-byte, the structural
//! `=:=` comparison, and the `{minor_version, 0|1|2}` matrix) live in the sibling
//! `tests/property_oracle.rs`, which includes the same support module.

mod property_support;

use std::collections::BTreeMap;

use beam_codec::{
    MinorVersion, EXPORT_EXT, INTEGER_EXT, LARGE_BIG_EXT, MAP_EXT, NEWER_REFERENCE_EXT,
    NEW_FLOAT_EXT, NEW_FUN_EXT, NEW_PID_EXT, NIL_EXT, SMALL_ATOM_UTF8_EXT, SMALL_BIG_EXT,
    SMALL_INTEGER_EXT, STRING_EXT, V4_PORT_EXT,
};
use property_support::generator::{generate, round_trip};
use property_support::{PROPERTY_TERMS, SEED};

/// `encode -> decode -> encode` is byte-identical for every generated term, under every
/// `{minor_version, N}` flavour — and the corpus really covers the producible tag set.
#[test]
fn generated_corpus_round_trips_byte_identically() {
    let (corpus, kinds) = generate(SEED, PROPERTY_TERMS);
    assert_eq!(corpus.len(), PROPERTY_TERMS);

    let mut checked = 0usize;
    let mut top_tags = BTreeMap::<u8, usize>::new();
    for (i, g) in corpus.iter().enumerate() {
        // Default (V2) image: encode -> decode -> encode.
        round_trip(&g.image, MinorVersion::V2, &format!("term #{i} (V2)"));
        // And each minor-version flavour. Decoding an internal id yields the *external*
        // thing, so re-encoding needs no local-node identity — only the same flavour.
        for (k, m) in [
            (0usize, MinorVersion::V0),
            (1, MinorVersion::V1),
            (2, MinorVersion::V2),
        ] {
            round_trip(&g.minor[k], m, &format!("term #{i} ({m:?})"));
        }
        *top_tags.entry(g.image[1]).or_default() += 1;
        checked += 1;
    }
    assert_eq!(checked, PROPERTY_TERMS);

    // The corpus must genuinely exercise the whole producible surface.
    for kind in [
        "small_boundary",
        "small_random",
        "bignum",
        "float",
        "float_bits",
        "atom",
        "binary",
        "bitstring",
        "byte_list",
        "nil",
        "tuple",
        "proper_list",
        "improper_list",
        "flat_map",
        "hamt_map",
        "binary_key_map",
        "external_pid",
        "external_port",
        "external_ref",
        "internal_pid",
        "internal_port",
        "export_fun",
        "imported_fun",
        "mixed_tuple",
    ] {
        let n = kinds.iter().filter(|k| **k == kind).count();
        assert!(n > 0, "the generated corpus never produced a `{kind}`");
    }
    // Every integer/atom/list/map wire tag must appear at least once at the top level.
    for tag in [
        SMALL_INTEGER_EXT,
        INTEGER_EXT,
        SMALL_BIG_EXT,
        LARGE_BIG_EXT,
        NEW_FLOAT_EXT,
        SMALL_ATOM_UTF8_EXT,
        STRING_EXT,
        NIL_EXT,
        MAP_EXT,
        NEW_PID_EXT,
        V4_PORT_EXT,
        NEWER_REFERENCE_EXT,
        EXPORT_EXT,
        NEW_FUN_EXT,
    ] {
        assert!(
            top_tags.contains_key(&tag),
            "no top-level image carries tag {tag}: {:?}",
            top_tags.keys().collect::<Vec<_>>()
        );
    }
    eprintln!(
        "[beam_codec property] {} terms round-tripped byte-identically under V0/V1/V2 \
         ({} distinct top-level tags)",
        PROPERTY_TERMS,
        top_tags.len()
    );
}
