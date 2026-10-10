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

//! Live oracle cross-checks for the deterministic property corpus.
//!
//! Every generated term is also rendered to Erlang **source**; the renderable subset
//! is handed to the system `erl` in a few large batches, which prints `term_to_binary(T)`
//! per term. We then assert, per term:
//!
//! 1. our `encode(term)` is **byte-identical** to the oracle's image;
//! 2. our **decoder** accepts the oracle's image and re-encodes it to the same bytes;
//! 3. the two decodes are structurally equal (`terms_equal`, atoms by name);
//! 4. the oracle accepts **our** image (`term_to_binary(binary_to_term(ours))`).
//!
//! and for the `{minor_version, 0|1|2}` matrix, our `minor` image equals the oracle's
//! `term_to_binary(T, [{minor_version, N}])` byte-for-byte.
//!
//! The shared generator/renderer/oracle plumbing lives in [`property_support`]; the
//! oracle resolution mirrors `tools/differential`: `$ERTS_ORACLE` if executable, else
//! `erl` on `$PATH`; when none exists the test prints a loud skip note and returns — a
//! skip, never a silent pass.

mod property_support;

use beam_alloc::Heap;
use beam_codec::{decode_term, encode_term, FLOAT_EXT, VERSION_MAGIC};
use beam_term::prelude::AtomTable;
use property_support::compare::terms_equal;
use property_support::generator::{generate, Generated};
use property_support::oracle::{chunks, erl_bytes, oracle_or_skip, parse_binary_list, run_erl};
use property_support::{HEAP_WORDS, ORACLE_LIMIT, PROPERTY_TERMS, SEED};

/// The renderable corpus is handed to `erl` in a few large batches; each image must be
/// **byte-identical** to ours, our decoder must read the oracle's exact bytes, the two
/// decodes must be structurally equal, and the oracle must accept our image.
#[test]
fn oracle_agrees_on_the_generated_corpus() {
    let Some(oracle) = oracle_or_skip("oracle_agrees_on_the_generated_corpus") else {
        return;
    };
    let (corpus, _) = generate(SEED, PROPERTY_TERMS);
    let renderable: Vec<&Generated> = corpus
        .iter()
        .filter(|g| g.source.is_some())
        .take(ORACLE_LIMIT)
        .collect();
    assert!(
        renderable.len() >= 100,
        "only {} renderable terms in {} — the generator lost its source-renderable kinds",
        renderable.len(),
        PROPERTY_TERMS
    );

    // --- Direction 1: oracle -> us ------------------------------------------------
    let sources: Vec<String> = renderable
        .iter()
        .map(|g| g.source.clone().expect("filtered on `is_some`"))
        .collect();
    let mut compared = 0usize;
    for (start, end) in chunks(&sources, 30_000) {
        let eval = format!(
            "io:format(\"~w~n\", [ [ term_to_binary(T) || T <- [{}] ] ])",
            sources[start..end].join(",")
        );
        let images = parse_binary_list(&run_erl(&oracle, &eval));
        assert_eq!(
            images.len(),
            end - start,
            "the oracle printed {} images for a {}-entry batch",
            images.len(),
            end - start
        );
        for (k, bi) in images.iter().enumerate() {
            let g = renderable[start + k];
            assert!(
                bi.first() == Some(&VERSION_MAGIC),
                "oracle image {bi:?} lacks the version byte"
            );
            assert_eq!(
                bi,
                &g.image,
                "source {} produced a different image\n oracle = {bi:?}\n ours   = {:?}",
                g.source.as_deref().unwrap_or(""),
                g.image
            );
            // Our decoder reads the oracle's exact bytes and reproduces them.
            let mut atoms = AtomTable::new();
            let mut h1 = Heap::new(HEAP_WORDS).expect("heap");
            let t1 = decode_term(bi, &mut h1, &mut atoms)
                .unwrap_or_else(|e| panic!("our decoder rejected the oracle image {bi:?}: {e:?}"));
            assert_eq!(
                encode_term(t1, &atoms).expect("re-encode the oracle image"),
                *bi,
                "our decoder does not reproduce the oracle image {bi:?}"
            );
            // Structural equality with our own decode of our own image.
            let mut h2 = Heap::new(HEAP_WORDS).expect("heap");
            let t2 = decode_term(&g.image, &mut h2, &mut atoms)
                .unwrap_or_else(|e| panic!("our decoder rejected our image {:?}: {e:?}", g.image));
            assert!(
                terms_equal(t1, t2, &atoms),
                "our decode of the oracle image differs structurally from our own image\n\
                 \x20 oracle = {bi:?}\n\x20 ours   = {:?}",
                g.image
            );
            compared += 1;
        }
    }

    // --- Direction 2: us -> oracle ------------------------------------------------
    let ours: Vec<String> = renderable.iter().map(|g| erl_bytes(&g.image)).collect();
    for (start, end) in chunks(&ours, 30_000) {
        let eval = format!(
            "io:format(\"~w~n\", [ [ term_to_binary(binary_to_term(list_to_binary(B))) \
             || B <- [{}] ] ])",
            ours[start..end].join(",")
        );
        let images = parse_binary_list(&run_erl(&oracle, &eval));
        assert_eq!(
            images.len(),
            end - start,
            "the oracle printed {} images",
            images.len()
        );
        for (k, bi) in images.iter().enumerate() {
            let g = renderable[start + k];
            assert_eq!(
                bi, &g.image,
                "the oracle's decode/encode of our image is not our image\n ours = {:?}\n oracle = {bi:?}",
                g.image
            );
        }
    }
    eprintln!(
        "[beam_codec property] the oracle confirmed {compared} generated terms byte-for-byte \
         (both directions) over {} renderable terms",
        renderable.len()
    );
}

/// For the renderable corpus, our `encode_term_with_minor_version` must equal the
/// oracle's `term_to_binary(T, [{minor_version, N}])` byte-for-byte for `N` 0/1/2.
/// (The only documented exclusion — a non-finite float under V0 — cannot occur: the
/// generator never emits a non-finite float, and such a `NEW_FLOAT_EXT`/`FLOAT_EXT`
/// image is rejected on decode anyway.)
#[test]
fn oracle_agrees_on_the_minor_version_matrix() {
    let Some(oracle) = oracle_or_skip("oracle_agrees_on_the_minor_version_matrix") else {
        return;
    };
    let (corpus, _) = generate(SEED, PROPERTY_TERMS);
    let renderable: Vec<&Generated> = corpus
        .iter()
        .filter(|g| g.source.is_some())
        .take(ORACLE_LIMIT)
        .collect();
    let sources: Vec<String> = renderable
        .iter()
        .map(|g| g.source.clone().expect("filtered on `is_some`"))
        .collect();

    for (k, opt) in [
        (0usize, "{minor_version,0}"),
        (1, "{minor_version,1}"),
        (2, "{minor_version,2}"),
    ] {
        let mut exact = 0usize;
        for (start, end) in chunks(&sources, 30_000) {
            let eval = format!(
                "io:format(\"~w~n\", [ [ term_to_binary(T,[{opt}]) || T <- [{}] ] ])",
                sources[start..end].join(",")
            );
            let images = parse_binary_list(&run_erl(&oracle, &eval));
            assert_eq!(
                images.len(),
                end - start,
                "the oracle printed {} images",
                images.len()
            );
            for (j, bi) in images.iter().enumerate() {
                let g = renderable[start + j];
                assert_eq!(
                    bi,
                    &g.minor[k],
                    "{opt}: our minor-version image differs from the oracle\n source = {}\n \
                     oracle = {bi:?}\n ours   = {:?}",
                    g.source.as_deref().unwrap_or(""),
                    g.minor[k]
                );
                exact += 1;
            }
        }
        eprintln!(
            "[beam_codec property] minor_version {k}: {exact} images byte-identical to the oracle"
        );
    }
    // `V0` is the only flavour that may emit the legacy 32-bit `INTEGER_EXT`/`FLOAT_EXT`
    // wire forms differently; assert the corpus actually reached `FLOAT_EXT` at least
    // once so the legacy path is covered.
    assert!(
        renderable.iter().any(|g| g.minor[0].contains(&FLOAT_EXT)),
        "the V0 corpus never emitted a FLOAT_EXT — the legacy float path is uncovered"
    );
}
