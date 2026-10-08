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

//! Heap-sizing / growth-policy evidence for `beam_gc` — the ERTS-shaped heap policy.
//!
//! The in-module tests in `src/policy.rs` pin the arithmetic of the policy helpers. This
//! file drives the **public** API end to end and pins what the runtime actually observes:
//!
//! 1. the curated growth table and the `spawn_opt` rounding (`erts_next_heap_size(x, 0)`;
//!    `erl_process.c:12321-12349`) — the process defaults the oracle reports;
//! 2. the fullsweep cadence (`GEN_GCS >= MAX_GEN_GCS`, `erl_gc.c:694,831`) including the
//!    `fullsweep_after: 0` case, on both mature-generation strategies;
//! 3. the post-collection **growth/shrink** sizing on the copying collector
//!    (`adjust_after_fullsweep`, `erl_gc.c:2061-2098` and the minor-time nursery resize,
//!    `erl_gc.c:1493`): a nursery under pressure grows, a grossly oversized space shrinks,
//!    and `min_heap_words`/`max_heap_words` floor/cap the result;
//! 4. a **relocating** Immix fullsweep (the shrink half of the policy on the mark-region
//!    mature generation) preserving a young→mature reference across the resize;
//! 5. [`HeapInfo`] — the `garbage_collection`-shaped observables derived from a live
//!    collector, in the oracle's item order;
//! 6. an **oracle cross-check** against the system `erl`: the default `min_heap_size`,
//!    `min_bin_vheap_size`, `fullsweep_after` and the item's key order. It SKIPS LOUDLY
//!    when `erl` is unavailable, like the crate's other oracle-style tests.
//!
//! `unsafe` below only dereferences pointers this test allocated itself (through the
//! nursery's own `tuple` builder) or that a collection just wrote; every block carries a
//! `// SAFETY:` rationale.

use std::process::Command;

use beam_gc::prelude::{
    heap_sizes, next_heap_size, AnyCollector, CollectionKind, GARBAGE_COLLECTION_KEYS, GcConfig,
    HeapInfo, HeapPolicy, MatureKind, DEFAULT_MIN_HEAP_WORDS,
};
use beam_term::prelude::{
    arityval, boxed_val, is_arity_value, make_small, signed_value, Eterm,
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A collector of `kind` over `policy`, through the public config seam.
fn any(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
    AnyCollector::from_config(&GcConfig::copying(policy).with_mature(kind)).expect("collector")
}

/// Whether `term` is resident in the selected strategy's mature generation.
fn in_mature(c: &AnyCollector, term: Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains_term(term),
        AnyCollector::Immix(x) => x.mature().contains_term(term),
    }
}

/// The selected strategy's mature-generation capacity, in words.
fn mature_capacity(c: &AnyCollector) -> usize {
    match c {
        AnyCollector::Copying(x) => x.mature().capacity_words(),
        AnyCollector::Immix(x) => x.mature().capacity_words(),
    }
}

/// Allocates a tuple with `elems` in the nursery.
fn build_tuple(c: &mut AnyCollector, elems: &[Eterm]) -> Eterm {
    c.young_mut().heap_mut().tuple(elems).expect("tuple")
}

/// The elements of a tuple `{...}`, in order.
fn read_tuple(term: Eterm) -> Vec<Eterm> {
    // SAFETY: `term` is a live boxed term this test (or a collection) built.
    let header = unsafe { *boxed_val(term) };
    assert!(is_arity_value(header), "not a tuple header");
    let n = arityval(header);
    // SAFETY: elements `1..=n` lie inside the `n`-element tuple just validated.
    (0..n).map(|i| unsafe { *boxed_val(term).add(1 + i) }).collect()
}

/// The small-integer values of a tuple of smalls.
fn tuple_smalls(term: Eterm) -> Vec<i64> {
    read_tuple(term).into_iter().map(signed_value).collect()
}

/// Runs `erl -noshell -eval <expr>` and returns its stdout, or `None` when the oracle is
/// not runnable.
fn erl_eval(expr: &str) -> Option<String> {
    let output = Command::new("erl")
        .arg("-noshell")
        .arg("-eval")
        .arg(expr)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

// ---------------------------------------------------------------------------
// 1. the curated growth table + the `spawn_opt` rounding
// ---------------------------------------------------------------------------

#[test]
fn curated_growth_table_and_spawn_rounding_via_the_public_api() {
    // The table starts at 12 and steps Fibonacci+1 up to 233, then +20 %.
    assert_eq!(heap_sizes()[0], 12);
    assert_eq!(next_heap_size(0, 0), 12);
    assert_eq!(next_heap_size(11, 0), 12);
    assert_eq!(next_heap_size(12, 0), 12);
    assert_eq!(next_heap_size(13, 0), 38);
    assert_eq!(next_heap_size(233, 0), 233);
    assert_eq!(next_heap_size(234, 0), 376);
    assert_eq!(next_heap_size(234, 1), 610);
    assert_eq!(DEFAULT_MIN_HEAP_WORDS, 233);

    // A spawned process's defaults, as the oracle reports them.
    let spawn = HeapPolicy::from_spawn_opts(0, 0, 65_535);
    assert_eq!(spawn.min_heap_words, 233);
    assert_eq!(spawn.min_bin_vheap_words, 46_422);
    assert_eq!(spawn.fullsweep_after, 65_535);

    // A request below the minimum is replaced by the minimum, never rounded past it ...
    assert_eq!(HeapPolicy::from_spawn_opts(1, 1, 7).min_heap_words, 233);
    assert_eq!(HeapPolicy::from_spawn_opts(232, 1, 7).min_heap_words, 233);
    // ... while one at or above it rounds through `erts_next_heap_size(x, 0)`.
    assert_eq!(HeapPolicy::from_spawn_opts(234, 0, 7).min_heap_words, 376);
    assert_eq!(HeapPolicy::from_spawn_opts(377, 0, 7).min_heap_words, 610);

    let rounded = HeapPolicy::from_spawn_opts(500, 50_000, 7);
    assert_eq!(rounded.min_heap_words, 610);
    assert_eq!(rounded.min_bin_vheap_words, next_heap_size(50_000, 0));
    assert!(
        rounded.min_bin_vheap_words > 50_000,
        "a vheap request above the minimum must be rounded up, not truncated"
    );

    // Every stored minimum is a fixed point of the growth table (the C
    // `ASSERT(p->min_heap_size == erts_next_heap_size(p->min_heap_size, 0))`).
    for request in [0usize, 233, 234, 500, 1_000, 46_422, 100_000] {
        let policy = HeapPolicy::from_spawn_opts(request, request, 0);
        assert_eq!(policy.min_heap_words, next_heap_size(policy.min_heap_words, 0));
        assert_eq!(
            policy.min_bin_vheap_words,
            next_heap_size(policy.min_bin_vheap_words, 0)
        );
    }

    // `fullsweep_after` is stored verbatim, including 0.
    assert_eq!(HeapPolicy::from_spawn_opts(233, 0, 0).fullsweep_after, 0);
}

// ---------------------------------------------------------------------------
// 2. the fullsweep cadence
// ---------------------------------------------------------------------------

/// Collects three successive graphs under `policy`, returning `(kind, minor_gcs)` per round.
fn cadence(kind: MatureKind, policy: HeapPolicy) -> Vec<(CollectionKind, u32)> {
    let mut c = any(kind, policy);
    let mut roots: Vec<Eterm> = Vec::new();
    let mut seen = Vec::new();
    for _ in 0..3 {
        let t = build_tuple(&mut c, &[make_small(1), make_small(2)]);
        roots.push(t);
        let collection = c.collect(&mut roots).expect("collect");
        seen.push((collection.kind, c.minor_gcs()));
    }
    seen
}

#[test]
fn fullsweep_after_two_gives_two_minors_then_a_major() {
    let policy = HeapPolicy {
        fullsweep_after: 2,
        ..HeapPolicy::default()
    };
    let expected = vec![
        (CollectionKind::Minor, 1),
        (CollectionKind::Minor, 2),
        (CollectionKind::Major, 0),
    ];
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        assert_eq!(cadence(kind, policy.clone()), expected, "{kind:?}");
    }
}

#[test]
fn fullsweep_after_zero_makes_every_collection_a_fullsweep() {
    let policy = HeapPolicy {
        fullsweep_after: 0,
        ..HeapPolicy::default()
    };
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut c = any(kind, policy.clone());
        let mut roots: Vec<Eterm> = Vec::new();
        for round in 0..2 {
            let t = build_tuple(&mut c, &[make_small(7)]);
            roots.push(t);
            let collection = c.collect(&mut roots).expect("collect");
            assert_eq!(
                collection.kind,
                CollectionKind::Major,
                "{kind:?} round {round} must be a fullsweep"
            );
            // A fullsweep always resets the minor counter.
            assert_eq!(c.minor_gcs(), 0);
        }
    }
}

// ---------------------------------------------------------------------------
// 3. post-collection growth / shrink
// ---------------------------------------------------------------------------

#[test]
fn a_tiny_nursery_grows_under_allocation_pressure() {
    let policy = HeapPolicy {
        initial_young_words: 12,
        ..HeapPolicy::default()
    };
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut c = any(kind, policy.clone());
        assert_eq!(c.young().capacity_words(), 12, "{kind:?}");

        // Fill a good part of the 12-word nursery, then collect a minor.
        let t = build_tuple(&mut c, &[make_small(1), make_small(2)]);
        let mut roots = vec![t];
        let collection = c.collect(&mut roots).expect("collect");
        assert_eq!(collection.kind, CollectionKind::Minor, "{kind:?}");

        // A nursery far too small for the next round steps up the growth table.
        assert!(
            c.young().capacity_words() > 12,
            "{kind:?}: the nursery must grow under pressure, got {}",
            c.young().capacity_words()
        );
        assert_eq!(tuple_smalls(roots[0]), vec![1, 2]);
    }
}

#[test]
fn a_grossly_oversized_nursery_shrinks_after_a_collection() {
    let policy = HeapPolicy {
        initial_young_words: 1_598,
        ..HeapPolicy::default()
    };
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut c = any(kind, policy.clone());
        assert_eq!(c.young().capacity_words(), 1_598, "{kind:?}");

        let t = build_tuple(&mut c, &[make_small(1), make_small(2)]);
        let mut roots = vec![t];
        c.collect(&mut roots).expect("collect");

        // Three live words in a 1598-word nursery is >4x over the request, so the nursery
        // falls back to the curated size of the live set (90 words).
        assert_eq!(c.young().capacity_words(), 90, "{kind:?}");
        assert_eq!(tuple_smalls(roots[0]), vec![1, 2]);
    }
}

#[test]
fn a_grossly_oversized_mature_space_shrinks_after_a_fullsweep() {
    let policy = HeapPolicy {
        fullsweep_after: 0,
        initial_mature_words: 610,
        ..HeapPolicy::default()
    };
    let mut c = any(MatureKind::Copying, policy);
    assert_eq!(mature_capacity(&c), 610);

    let t = build_tuple(&mut c, &[make_small(1), make_small(2)]);
    let mut roots = vec![t];
    let collection = c.collect(&mut roots).expect("collect");
    assert_eq!(collection.kind, CollectionKind::Major);

    // 610 words keeping 3 live + 64 reserve needs 67 -> shrink target 2*67=134 -> curated
    // 142, never more than the old capacity.
    assert_eq!(mature_capacity(&c), 142);
    assert_eq!(tuple_smalls(roots[0]), vec![1, 2]);
}

#[test]
fn the_minimum_floors_and_the_maximum_clamps_a_real_resize() {
    // `min_heap_words` floors a would-be grow (12 words keeping 1 needs 65 -> curated 90,
    // floored to 376).
    let floored = HeapPolicy {
        min_heap_words: 376,
        fullsweep_after: 0,
        initial_mature_words: 12,
        ..HeapPolicy::default()
    };
    let mut c = any(MatureKind::Copying, floored);
    let t = build_tuple(&mut c, &[make_small(1)]);
    let mut roots = vec![t];
    c.collect(&mut roots).expect("collect");
    assert_eq!(mature_capacity(&c), 376, "the minimum floors the grown target");

    // `max_heap_words` clamps a would-be grow (41 live + 64 reserve needs 105 -> curated
    // 142, capped to 100).
    let capped = HeapPolicy {
        max_heap_words: 100,
        fullsweep_after: 0,
        initial_mature_words: 12,
        ..HeapPolicy::default()
    };
    let mut c = any(MatureKind::Copying, capped);
    let elems: Vec<Eterm> = (0..40i64).map(make_small).collect();
    let t = build_tuple(&mut c, &elems);
    let mut roots = vec![t];
    c.collect(&mut roots).expect("collect");
    assert_eq!(mature_capacity(&c), 100, "the maximum clamps the grown target");
    assert_eq!(tuple_smalls(roots[0]), (0..40i64).collect::<Vec<_>>());
}

// ---------------------------------------------------------------------------
// 4. a relocating Immix fullsweep keeps a young→mature reference
// ---------------------------------------------------------------------------

#[test]
fn a_relocating_fullsweep_resizes_the_immix_region_and_keeps_a_young_to_mature_reference() {
    let policy = HeapPolicy {
        initial_mature_words: 610,
        ..HeapPolicy::default()
    };
    let mut c = any(MatureKind::Immix, policy);
    assert_eq!(
        mature_capacity(&c),
        768,
        "610 words rounds up to three 256-word blocks"
    );

    // Round 1: promote an inner tuple into the mature region with a minor.
    let inner = build_tuple(&mut c, &[make_small(1), make_small(2), make_small(3)]);
    let mut scratch = vec![inner];
    c.collect_minor(&mut scratch).expect("minor");
    let inner = scratch[0];
    assert!(in_mature(&c, inner), "the inner tuple is promoted");

    // Round 2: a young tuple referencing the mature object — the young→mature edge the
    // in-place reclaim has to honour — then a fullsweep the policy decides should *shrink*
    // the region, i.e. the relocating path.
    let outer = build_tuple(&mut c, &[inner]);
    let mut roots = vec![inner, outer];
    let before = mature_capacity(&c);
    c.collect_major(&mut roots).expect("relocating fullsweep");

    assert!(
        mature_capacity(&c) < before,
        "the region shrank ({before} -> {})",
        mature_capacity(&c)
    );
    assert_eq!(mature_capacity(&c), 256, "233 words rounds to one block");
    assert_eq!(c.minor_gcs(), 0, "a fullsweep resets the minor counter");
    if let AnyCollector::Immix(x) = &c {
        assert!(
            x.last_immix_stats().is_none(),
            "a relocating fullsweep runs no in-place reclaim"
        );
    }

    // Both roots live in the (new) mature region, and the young→mature edge survived.
    assert!(in_mature(&c, roots[0]), "the inner tuple was relocated");
    assert!(in_mature(&c, roots[1]), "the outer tuple was promoted");
    assert_eq!(tuple_smalls(roots[0]), vec![1, 2, 3]);
    let elems = read_tuple(roots[1]);
    assert_eq!(elems.len(), 1);
    assert_eq!(
        elems[0], roots[0],
        "the outer tuple references the relocated inner tuple"
    );
}

// ---------------------------------------------------------------------------
// 5. HeapInfo — the `garbage_collection` observables
// ---------------------------------------------------------------------------

#[test]
fn heap_info_tracks_the_collector_and_the_policy() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let policy = HeapPolicy::from_spawn_opts(234, 0, 7);
        let mut c = any(kind, policy);

        let info = HeapInfo::of(&c);
        assert_eq!(info.minor_gcs, 0, "{kind:?}");
        assert_eq!(info.fullsweep_after, 7);
        assert_eq!(info.min_heap_size, 376);
        assert_eq!(info.min_bin_vheap_size, 46_422);
        assert_eq!(info.max_heap_size, 0);
        assert_eq!(info.heap_size, c.young().capacity_words());
        assert_eq!(
            info.total_heap_size,
            c.young().capacity_words() + mature_capacity(&c)
        );
        assert!(info.total_heap_size >= info.heap_size);

        // The item's keys come out in the oracle's order.
        let keys: Vec<&str> = info.items().map(|(k, _)| k).to_vec();
        assert_eq!(keys, GARBAGE_COLLECTION_KEYS.to_vec());
        assert_eq!(
            info.items().map(|(_, v)| v),
            [
                info.max_heap_size,
                info.min_bin_vheap_size,
                info.min_heap_size,
                info.fullsweep_after as usize,
                info.minor_gcs as usize,
            ]
        );

        // A minor bumps the counter; a fullsweep resets it.
        let t = build_tuple(&mut c, &[make_small(1)]);
        let mut roots = vec![t];
        let collection = c.collect(&mut roots).expect("collect");
        assert_eq!(collection.kind, CollectionKind::Minor);
        assert_eq!(c.minor_gcs(), 1);
        assert_eq!(HeapInfo::of(&c).minor_gcs, 1);
        assert_eq!(c.heap_info().minor_gcs, 1);

        c.collect_major(&mut roots).expect("major");
        assert_eq!(c.minor_gcs(), 0);
        assert_eq!(HeapInfo::of(&c).minor_gcs, 0);
        assert_eq!(c.heap_info().minor_gcs, 0);

        // The reported bounds follow the policy across a resize.
        assert_eq!(HeapInfo::of(&c).min_heap_size, 376);
        assert_eq!(
            HeapInfo::of(&c).heap_size,
            c.young().capacity_words(),
            "heap_size tracks the current nursery"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. the oracle cross-check
// ---------------------------------------------------------------------------

/// The default heap options and the `garbage_collection` item's key order, read from the
/// system `erl`. SKIPS LOUDLY (no panic) when the oracle is unavailable.
#[test]
fn the_default_heap_options_match_the_system_erlang_oracle() {
    let expr = "io:format(\"~w~n\", [erlang:system_info(garbage_collection)]), halt().";
    let Some(stdout) = erl_eval(expr) else {
        eprintln!(
            "SKIP heap_sizing oracle cross-check: the system `erl` is not available/runnable"
        );
        return;
    };

    // Extract `{Key,Value}` by text — the oracle emits the item as a plain proplist.
    let value_of = |key: &str| -> String {
        let needle = format!("{{{key},");
        let start = stdout
            .find(needle.as_str())
            .unwrap_or_else(|| panic!("oracle output lacks `{key}`: {stdout}"));
        let rest = &stdout[start + needle.len()..];
        let end = rest.find('}').expect("the value is terminated");
        rest[..end].to_string()
    };

    let min_heap = value_of("min_heap_size");
    let min_bin = value_of("min_bin_vheap_size");
    let fullsweep = value_of("fullsweep_after");
    let max_heap = value_of("max_heap_size");

    assert_eq!(min_heap, "233", "oracle min_heap_size");
    assert_eq!(min_bin, "46422", "oracle min_bin_vheap_size");
    assert_eq!(fullsweep, "65535", "oracle fullsweep_after");
    assert_eq!(max_heap, "0", "oracle max_heap_size");

    // The crate's defaults must agree with the oracle's.
    let spawn = HeapPolicy::from_spawn_opts(0, 0, 65_535);
    assert_eq!(spawn.min_heap_words.to_string(), min_heap);
    assert_eq!(spawn.min_bin_vheap_words.to_string(), min_bin);
    assert_eq!(spawn.fullsweep_after.to_string(), fullsweep);
    assert_eq!(DEFAULT_MIN_HEAP_WORDS.to_string(), min_heap);
    assert_eq!(
        HeapPolicy::default().min_bin_vheap_words.to_string(),
        min_bin
    );
    assert_eq!(HeapPolicy::default().fullsweep_after.to_string(), fullsweep);

    // The keys are emitted in the oracle's order, and the crate's item starts with them.
    let order: Vec<usize> = [
        "max_heap_size",
        "min_bin_vheap_size",
        "min_heap_size",
        "fullsweep_after",
    ]
    .iter()
    .map(|k| {
        stdout
            .find(format!("{{{k},").as_str())
            .unwrap_or_else(|| panic!("oracle output lacks `{k}`: {stdout}"))
    })
    .collect();
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "the oracle emits the keys in this order: {stdout}"
    );
    assert_eq!(
        &GARBAGE_COLLECTION_KEYS[..4],
        &[
            "max_heap_size",
            "min_bin_vheap_size",
            "min_heap_size",
            "fullsweep_after"
        ]
    );
}
