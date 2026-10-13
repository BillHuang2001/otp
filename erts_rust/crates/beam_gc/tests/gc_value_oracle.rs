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

//! **GC value-oracle** — differential evidence that the per-process collector is
//! *value-transparent*.
//!
//! Decommissioning the legacy C collector (`erts/emulator/beam/erl_gc.c` + `copy.c`) needs
//! proof that the Rust collector preserves every **observable value**. `beam_gc` cannot run
//! `erlc`-produced bytecode (it depends only on `beam_common`/`beam_term`/`beam_alloc`/`libc`;
//! a `beam_engine`/`beam_loader`/`beam_rt` edge would be a forbidden backward cycle), so the
//! strongest oracle buildable *here* is a **term-level** one:
//!
//! 1. **build** a term of a given shape on a real [`AnyCollector`] heap (the collector's own
//!    young nursery);
//! 2. **churn** it through a long-lived, allocation-heavy workload that forces many
//!    collections (unreachable garbage allocated every round, the term kept alive as the
//!    sole root), so the term is evacuated/re-copied repeatedly;
//! 3. **encode** the surviving term with the External Term Format codec
//!    ([`beam_codec::encode_term`]) and require it to be **byte-identical** to what the
//!    **system Erlang** produces for the equivalent expression
//!    (`iolist_to_binary(term_to_binary(E))`, printed as uppercase hex).
//!
//! The oracle expression for every shape is *deterministic* (no pid/reference/fun/float, no
//! atom-keyed map — the oracle's flat-map key order is by **internal atom index**, which this
//! crate cannot reproduce; integer-keyed maps order identically in both) so the bytes are
//! stable and comparable.
//!
//! Every shape is run through **both** mature strategies — the default copying
//! [`Collector`](beam_gc::Collector) (`GcConfig::copying`) and the mark-region
//! [`GenImmixCollector`](beam_gc::GenImmixCollector) (`GcConfig::immix`) — so the oracle covers
//! whichever mature generation a policy configures. The corpus spans every interior the copier
//! (`src/copy.rs`/`src/live.rs`) moves: an immediate, an atom, a cons list, a boxed tuple with a
//! nested tuple, a flat map (≤32 integer keys), a HAMT map (>32 integer keys), a large
//! off-heap refc binary, a **sub-binary** window (`binary:part/3` — the same 5-word
//! `ErlSubBits` the copier overrides `object_words` for), and a bignum.
//!
//! Where a value is **not** oracle-comparable (internal counters, per-runtime heap sizes) the
//! companion tests below pin an *invariant* instead — see
//! [`gc_counters_reclamation_and_capacity_invariants`] and
//! [`an_offheap_refc_binary_is_byte_preserved_and_freed_exactly_once`].
//!
//! # Oracle / skip behaviour
//!
//! The oracle is `$ERTS_ORACLE` when that names an executable file, otherwise `erl` on
//! `$PATH`. When no oracle is available the oracle-backed test **SKIPS LOUDLY** (`eprintln!`
//! then `return`) exactly like `tests/heap_sizing.rs` — never a panic, never a silent pass — so
//! the file still builds and passes on a machine without Erlang. The invariant tests need no
//! oracle and always run.
//!
//! # `unsafe`
//!
//! `unsafe` only dereferences pointers this test allocated and wrote itself (through the
//! nursery's own bump allocator) or that a collection just relocated; every block carries a
//! `// SAFETY:` rationale.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_alloc::{AllocRef, AllocatorConfig, CarrierAllocator, RefcBinary};
use beam_codec::encode_term;
use beam_gc::prelude::{
    AnyCollector, GcConfig, GcStats, HeapPolicy, MatureKind, OffHeap,
};
use beam_term::prelude::{
    big_size_words, bin_ref_parts, boxed_val, is_arity_value, is_boxed, is_list, is_refc_binary,
    make_atom, make_small, map_words_capacity, ptr_val, refc_bytes, write_big, write_map,
    write_refc_binary, AtomTable, Eterm, ERL_REFC_BITS_SIZE, NIL,
};

// ---------------------------------------------------------------------------
// The shape corpus
// ---------------------------------------------------------------------------

/// One term shape under test: its name (for diagnostics), the deterministic Erlang
/// expression the oracle evaluates, and nothing else — the Rust term is built by
/// [`build_shape`], keyed on the same name.
struct Shape {
    name: &'static str,
    /// A deterministic Erlang expression (no pid/ref/fun/float, integer-keyed maps only)
    /// whose value the oracle's `term_to_binary/1` must match the Rust term's.
    oracle_expr: &'static str,
}

/// Every shape the copier must be value-transparent over.
const SHAPES: &[Shape] = &[
    Shape {
        name: "small_int",
        oracle_expr: "42",
    },
    Shape {
        name: "atom",
        oracle_expr: "hello",
    },
    Shape {
        name: "cons_list",
        oracle_expr: "[100, 200, 300, 400, 500]",
    },
    Shape {
        name: "boxed_tuple",
        oracle_expr: "{alpha, 1, {nested, 2}, beta}",
    },
    Shape {
        name: "flat_map",
        oracle_expr: "maps:from_list([{I, I * 3} || I <- lists:seq(0, 15)])",
    },
    Shape {
        name: "hamt_map",
        oracle_expr: "maps:from_list([{I, I * 7} || I <- lists:seq(0, 39)])",
    },
    Shape {
        name: "refc_binary",
        oracle_expr: "list_to_binary(lists:seq(1, 200))",
    },
    Shape {
        name: "sub_binary",
        oracle_expr: "binary:part(list_to_binary(lists:seq(1, 200)), 10, 40)",
    },
    Shape {
        name: "bignum",
        oracle_expr: "123456789012345678901234567890",
    },
];

/// The atoms the corpus terms use; interned in a fresh [`AtomTable`] so the codec can render
/// their names. (Atom *names* are what `term_to_binary` emits, so the encoding is independent
/// of the interning order — unlike a flat map's key order, see the module docs.)
const CORPUS_ATOMS: [&str; 4] = ["hello", "alpha", "nested", "beta"];

/// The 200-byte payload the `refc_binary`/`sub_binary` shapes build — exactly
/// `list_to_binary(lists:seq(1, 200))`.
fn payload_bytes() -> Vec<u8> {
    (1u32..=200).map(|i| i as u8).collect()
}

// ---------------------------------------------------------------------------
// Collector scaffolding (the explicit-pin pattern of `offheap_liveness.rs`)
// ---------------------------------------------------------------------------

/// Both mature strategies, in a fixed order.
const BOTH: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// A policy with a nursery large enough for the biggest shape (`hamt_map` reserves
/// `map_words_capacity(40)` words) and a mature generation sized per strategy (the mark-region
/// space rounds every object up to a whole 16-word line, so it needs a larger *word* budget).
fn policy_for(kind: MatureKind) -> HeapPolicy {
    let mature = match kind {
        MatureKind::Copying => 8_192,
        MatureKind::Immix => 32_768,
    };
    HeapPolicy {
        initial_young_words: 4_096,
        initial_mature_words: mature,
        ..HeapPolicy::default()
    }
}

/// Builds the collector for `kind` through the **explicit** pin, asserting the pin really beat
/// the automatic rule.
fn collector(kind: MatureKind) -> AnyCollector {
    let config = match kind {
        MatureKind::Copying => GcConfig::copying(policy_for(kind)),
        MatureKind::Immix => GcConfig::immix(policy_for(kind)),
    };
    let c = AnyCollector::from_config(&config).expect("collector construction");
    assert_eq!(c.kind(), kind, "the explicit pin must select the requested strategy");
    c
}

/// Whether `term` is resident in the strategy's mature generation.
fn in_mature(c: &AnyCollector, term: Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains_term(term),
        AnyCollector::Immix(x) => x.mature().contains_term(term),
    }
}

/// The strategy's mature-generation capacity, in words.
fn mature_capacity(c: &AnyCollector) -> usize {
    match c {
        AnyCollector::Copying(x) => x.mature().capacity_words(),
        AnyCollector::Immix(x) => x.mature().capacity_words(),
    }
}

/// The strategy's mature-generation base address.
fn mature_base(c: &AnyCollector) -> usize {
    match c {
        AnyCollector::Copying(x) => x.mature().base() as usize,
        AnyCollector::Immix(x) => x.mature().base() as usize,
    }
}

// ---------------------------------------------------------------------------
// Building a corpus term on the collector's own nursery
// ---------------------------------------------------------------------------

/// Builds the Rust equivalent of `shape` on `c`'s young nursery and returns its root.
///
/// `keepalive` receives any off-heap [`RefcBinary`] the shape's term references: the on-heap
/// `BinRef.val` word is a raw pointer into that payload, so the test must own the payload for
/// as long as the term is used (a collection never moves or frees it — that is the point of
/// the off-heap shapes).
fn build_shape(
    shape: &str,
    c: &mut AnyCollector,
    atoms: &AtomTable,
    keepalive: &mut Vec<RefcBinary>,
) -> Eterm {
    match shape {
        "small_int" => make_small(42),
        "atom" => make_atom(lookup(atoms, "hello")),
        "cons_list" => {
            // `[100, 200, 300, 400, 500]`, built tail-first.
            let mut list = NIL;
            for v in [500i64, 400, 300, 200, 100] {
                list = c
                    .young_mut()
                    .heap_mut()
                    .cons(make_small(v), list)
                    .expect("a cons cell fits the nursery");
            }
            list
        }
        "boxed_tuple" => {
            // `{alpha, 1, {nested, 2}, beta}` — the inner 2-tuple is a boxed child slot the
            // copier must forward, not copy verbatim.
            let inner = c
                .young_mut()
                .heap_mut()
                .tuple(&[make_atom(lookup(atoms, "nested")), make_small(2)])
                .expect("the inner tuple fits");
            c.young_mut()
                .heap_mut()
                .tuple(&[
                    make_atom(lookup(atoms, "alpha")),
                    make_small(1),
                    inner,
                    make_atom(lookup(atoms, "beta")),
                ])
                .expect("the outer tuple fits")
        }
        "flat_map" => build_map(c, atoms, 16, 3),
        "hamt_map" => build_map(c, atoms, 40, 7),
        "refc_binary" => {
            let payload = RefcBinary::new(&payload_bytes()).expect("off-heap payload");
            let root = write_refc_pair(c, &payload, 0, payload.len() * 8);
            keepalive.push(payload);
            root
        }
        "sub_binary" => {
            // `binary:part(Bin, 10, 40)` — a 40-byte window starting at byte 10, i.e. bits
            // 80..400 of the same off-heap payload (`base` = the payload start, `start` = the
            // window's bit offset), exactly the sub-binary the oracle produces.
            let payload = RefcBinary::new(&payload_bytes()).expect("off-heap payload");
            let root = write_refc_pair(c, &payload, 10 * 8, 40 * 8);
            keepalive.push(payload);
            root
        }
        "bignum" => {
            // `123456789012345678901234567890`, little-endian `u64` limbs (the exact digits the
            // oracle's `SMALL_BIG_EXT` payload decodes to).
            let digits = [0xC373_E0EE_4E3F_0AD2u64, 0x018E_E90F_F6u64];
            // SAFETY: exactly `big_size_words(digits.len())` exclusively-owned words are
            // reserved at `hp`, the `write_big` contract; every word is written before the
            // root escapes and the block stays live in the nursery.
            unsafe {
                let hp = c
                    .young_mut()
                    .heap_mut()
                    .alloc_words(big_size_words(digits.len()))
                    .expect("the bignum block fits");
                write_big(hp, false, &digits)
            }
        }
        other => panic!("unknown shape {other}"),
    }
}

/// Interns the corpus atoms and looks one up (the table is the test's own, so a miss is a bug).
fn lookup<'a>(atoms: &'a AtomTable, name: &str) -> beam_term::Atom {
    atoms.lookup(name).unwrap_or_else(|| panic!("atom {name} must be interned"))
}

/// Writes the `n`-pair `i -> i * mult` integer-keyed map at the nursery top and returns its
/// root. `n <= MAP_SMALL_MAP_LIMIT` is a flat map, `n >` it a HAMT; the capacity
/// (`map_words_capacity`) is reserved, which is an upper bound — `write_map` writes the map
/// contiguously and the unused tail is never reachable from the root, so a collection never
/// scans it.
fn build_map(c: &mut AnyCollector, atoms: &AtomTable, n: usize, mult: i64) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (0..n as i64)
        .map(|i| (make_small(i), make_small(i * mult)))
        .collect();
    let cap = map_words_capacity(n);
    // SAFETY: `cap = map_words_capacity(n)` exclusively-owned words are reserved at `hp`,
    // exactly the reservation `write_map` documents; the map is written contiguously at the
    // base and stays live in the nursery.
    unsafe {
        let hp = c
            .young_mut()
            .heap_mut()
            .alloc_words(cap)
            .expect("the map reservation fits the nursery");
        write_map(hp, &pairs, atoms).expect("the map is well-formed")
    }
}

/// Writes the 8-word `[BinRef][ErlSubBits]` pair for `payload` (window `offset_bits..+size_bits`)
/// at the nursery top and returns its root (the `ErlSubBits`).
fn write_refc_pair(c: &mut AnyCollector, payload: &RefcBinary, offset_bits: usize, size_bits: usize) -> Eterm {
    // SAFETY: exactly `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved at `hp`;
    // `payload.data_ptr()` is a live, word-aligned pointer to `payload.len()` bytes of
    // off-heap storage kept alive by the caller's `keepalive` for the whole test, the window
    // `offset_bits..offset_bits + size_bits` lies inside it, and `null` is the (empty)
    // off-heap chain link.
    unsafe {
        let hp = c
            .young_mut()
            .heap_mut()
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("8 words for the refc pair");
        write_refc_binary(
            hp,
            payload.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.data_ptr(),
            offset_bits,
            size_bits,
        )
    }
}

// ---------------------------------------------------------------------------
// The allocation-heavy churn (forces many relocations)
// ---------------------------------------------------------------------------

/// The number of churn rounds the value-oracle drives per shape/strategy.
const CHURN_ROUNDS: usize = 32;

/// Runs `rounds` collection rounds over `roots`, filling the nursery with **unreachable
/// garbage** before each round so the term is repeatedly evacuated under pressure.
///
/// Every 8th round is a forced **major** (fullsweep), so the term — by then resident in the
/// mature generation — is relocated again rather than merely left in place by a minor; the
/// remaining rounds use the policy-driven [`AnyCollector::collect`], which escalates a
/// would-not-fit minor to a major exactly as the runtime does.
fn churn(c: &mut AnyCollector, roots: &mut [Eterm], rounds: usize) -> GcStats {
    let mut filler = 0i64;
    for round in 0..rounds {
        // Fill the nursery with garbage cons cells that no root reaches, so a collection
        // reclaims real words. A 2-word `cons` never overruns: `should_collect(2)` reports
        // true as soon as fewer than two words remain.
        let mut guard = 0usize;
        while !c.should_collect(2) {
            c.young_mut()
                .heap_mut()
                .cons(make_small(filler & 0x3f), NIL)
                .expect("a garbage cons fits while should_collect(2) is false");
            filler += 1;
            guard += 1;
            assert!(guard < 1 << 22, "the nursery never filled: should_collect is stuck");
        }

        if round % 8 == 7 {
            c.collect_major(roots).expect("the fullsweep fits the live set");
        } else {
            c.collect(roots).expect("the policy-driven collection fits the live set");
        }
    }
    c.stats()
}

// ---------------------------------------------------------------------------
// The system-Erlang oracle
// ---------------------------------------------------------------------------

/// Whether `path` is an executable file.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The first executable `name` on `$PATH`.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .find(|dir| !dir.as_os_str().is_empty() && is_executable(&dir.join(name)))
        .map(|dir| dir.join(name))
}

/// Resolves the oracle `erl`: `$ERTS_ORACLE` when it names an executable file, else `erl` on
/// `$PATH`. `None` means the oracle is genuinely unavailable (the caller skips loudly).
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = env::var_os("ERTS_ORACLE") {
        let candidate = PathBuf::from(value);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    find_in_path("erl")
}

/// Parses uppercase/lowercase hex into bytes; `None` on an odd length or a non-hex digit.
fn parse_hex(hex: &str) -> Option<Vec<u8>> {
    let bytes = hex.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    bytes
        .chunks_exact(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        })
        .collect()
}

/// Runs `erl -noshell -eval <expr>` and returns its stdout, or `None` when it cannot be run.
fn erl_eval(oracle: &Path, expr: &str) -> Option<String> {
    let output = Command::new(oracle)
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

/// The oracle's `term_to_binary(expr)` bytes, printed as uppercase hex by `binary:encode_hex/1`
/// and parsed back here. A run that cannot be executed or whose output is not valid hex is a
/// **hard failure** (the oracle was resolved, so it must answer).
fn oracle_term_bytes(oracle: &Path, expr: &str) -> Vec<u8> {
    let program = format!(
        "B = term_to_binary({expr}), io:format(\"~s~n\", [binary:encode_hex(B)]), halt()."
    );
    let stdout = erl_eval(oracle, &program)
        .unwrap_or_else(|| panic!("the oracle failed to evaluate `{expr}`"));
    let hex = stdout.trim();
    parse_hex(hex).unwrap_or_else(|| panic!("the oracle emitted non-hex output for `{expr}`: {stdout:?}"))
}

// ---------------------------------------------------------------------------
// The value-transparency oracle
// ---------------------------------------------------------------------------

/// **The core proof.** For every shape, under both mature strategies: the term built on the
/// `beam_gc` heap encodes byte-identically to the system Erlang's `term_to_binary`, and still
/// does so after `CHURN_ROUNDS` collections that relocate it — so the collector is
/// value-transparent over every interior the copier moves.
#[test]
fn gc_preserves_every_shape_byte_identically_to_the_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!(
            "SKIP gc_value_oracle: no `erl` oracle ($ERTS_ORACLE unset/non-executable and no \
             `erl` on $PATH) — the value-transparency cross-check did not run"
        );
        return;
    };

    let mut atoms = AtomTable::new();
    for name in CORPUS_ATOMS {
        atoms.intern(name);
    }

    for kind in BOTH {
        for shape in SHAPES {
            let expected = oracle_term_bytes(&oracle, shape.oracle_expr);

            let mut c = collector(kind);
            let mut keepalive = Vec::new();
            let root = build_shape(shape.name, &mut c, &atoms, &mut keepalive);

            // The freshly built term must already agree with the oracle — otherwise a later
            // disagreement could be a codec/construction bug rather than a GC one.
            let before = encode_term(root, &atoms).expect("the freshly built term encodes");
            assert_eq!(
                before, expected,
                "{kind:?} {}: the freshly built term must already match the oracle",
                shape.name
            );

            // Address before any collection (only a movable term has one).
            let movable = is_boxed(root) || is_list(root);
            let addr_before = if movable { ptr_val(root) as usize } else { 0 };

            let mut roots = vec![root];
            let stats = churn(&mut c, &mut roots, CHURN_ROUNDS);

            let after = encode_term(roots[0], &atoms).expect("the surviving term encodes");

            // (a) live data survives byte-identically: equal to the oracle AND to the
            //     pre-collection image.
            assert_eq!(
                after, expected,
                "{kind:?} {}: the term must still match the oracle after {CHURN_ROUNDS} \
                 collections",
                shape.name
            );
            assert_eq!(
                after, before,
                "{kind:?} {}: collections must be value-transparent",
                shape.name
            );

            // The term was *relocated* (not merely left in place): a movable root left the
            // nursery for the mature generation and its address changed.
            if movable {
                assert!(
                    !c.young().contains_term(roots[0]),
                    "{kind:?} {}: the root stayed in the nursery",
                    shape.name
                );
                assert!(
                    in_mature(&c, roots[0]),
                    "{kind:?} {}: the root was not relocated into the mature generation",
                    shape.name
                );
                assert_ne!(
                    ptr_val(roots[0]) as usize,
                    addr_before,
                    "{kind:?} {}: the root must have moved to a new address",
                    shape.name
                );
            }
            // The workload really did produce reclaimable garbage.
            assert!(
                stats.words_reclaimed > 0,
                "{kind:?} {}: the garbage-producing workload must reclaim words",
                shape.name
            );
            // The off-heap payloads of the refc shapes were kept alive by the test.
            assert_eq!(
                keepalive.len(),
                usize::from(shape.name.ends_with("binary")),
                "{kind:?} {}: exactly the off-heap shapes own a payload",
                shape.name
            );

            eprintln!(
                "gc_value_oracle: {kind:?} {:<12} OK ({} bytes)",
                shape.name,
                after.len()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Invariants where a value is NOT oracle-comparable
// ---------------------------------------------------------------------------

/// Pins the collector invariants a `term_to_binary` comparison cannot express: internal
/// counters, reclamation, and heap capacities. Direction-only assertions (never exact
/// values) so this stays green if the collector's sizing changes.
#[test]
fn gc_counters_reclamation_and_capacity_invariants() {
    for kind in BOTH {
        let mut atoms = AtomTable::new();
        for name in CORPUS_ATOMS {
            atoms.intern(name);
        }

        // (b) dead data is reclaimed, and (c) the counters move in the right direction.
        {
            let mut c = collector(kind);
            let mut keepalive = Vec::new();
            let root = build_shape("cons_list", &mut c, &atoms, &mut keepalive);
            let mut roots = vec![root];
            // A reachable term plus a chunk of unreachable garbage.
            let garbage = c
                .young_mut()
                .heap_mut()
                .tuple(&[make_small(1), make_small(2), make_small(3)])
                .expect("garbage tuple fits");

            let s0 = c.stats();
            let collection = c.collect(&mut roots).expect("the first collection fits");
            let s1 = c.stats();

            // (b) the round that saw unreachable data reclaims words.
            assert!(
                collection.stats.words_reclaimed > 0,
                "{kind:?}: a garbage-producing round must reclaim words"
            );
            assert!(
                s1.words_reclaimed > s0.words_reclaimed,
                "{kind:?}: the reclaimed counter must advance"
            );
            // (c) `GcStats` is cumulative and monotone; the collection it ran is counted.
            assert!(
                s1.minor_collections >= s0.minor_collections
                    && s1.major_collections >= s0.major_collections,
                "{kind:?}: the collection counters never go backwards"
            );
            assert!(
                s1.minor_collections + s1.major_collections
                    > s0.minor_collections + s0.major_collections,
                "{kind:?}: a collection increments exactly one counter"
            );
            // A forced minor strictly advances `minor_gcs()` (`GEN_GCS`); a fullsweep resets it.
            let s_before_minor = c.stats();
            let g_before_minor = c.minor_gcs();
            c.collect_minor(&mut roots).expect("a forced minor fits");
            assert!(
                c.stats().minor_collections > s_before_minor.minor_collections,
                "{kind:?}: a forced minor increments `minor_collections`"
            );
            assert!(
                c.minor_gcs() > g_before_minor,
                "{kind:?}: a forced minor strictly advances `minor_gcs()`"
            );
            let s_before_major = c.stats();
            c.collect_major(&mut roots).expect("a forced major fits");
            assert!(
                c.stats().major_collections > s_before_major.major_collections,
                "{kind:?}: a forced major increments `major_collections`"
            );
            assert_eq!(
                c.minor_gcs(),
                0,
                "{kind:?}: a fullsweep resets `minor_gcs()` (`GEN_GCS`)"
            );
            let _ = garbage; // the tuple is deliberately unreachable after the first round
        }

        // (d) capacities stay bounded across a long loop (no leak): the nursery and the
        //     mature generation converge to a plateau, so the second half is no larger.
        {
            let mut c = collector(kind);
            let mut keepalive = Vec::new();
            let root = build_shape("hamt_map", &mut c, &atoms, &mut keepalive);
            let mut roots = vec![root];

            let half = 64usize;
            churn(&mut c, &mut roots, half);
            let mid_young = c.young().capacity_words();
            let mid_mature = mature_capacity(&c);

            churn(&mut c, &mut roots, half);
            let end_young = c.young().capacity_words();
            let end_mature = mature_capacity(&c);

            assert!(
                end_young <= mid_young,
                "{kind:?}: the nursery capacity must plateau, not grow (mid {mid_young} -> end {end_young})"
            );
            assert!(
                end_mature <= mid_mature,
                "{kind:?}: the mature capacity must plateau, not grow (mid {mid_mature} -> end {end_mature})"
            );
            // And the mature generation always holds at least the live term.
            assert!(
                c.mature_used_words() > 0,
                "{kind:?}: the live term must be resident in the mature generation"
            );
            eprintln!(
                "gc_value_oracle: {kind:?} capacity plateau — nursery {end_young} words, mature \
                 {end_mature} words"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (e) off-heap reclamation
// ---------------------------------------------------------------------------

/// An isolated carrier allocator, so the test can observe a refc binary's payload being freed
/// through `AllocStats` without racing the process-wide registry allocator.
fn test_allocator(kind: MatureKind) -> AllocRef {
    let name = match kind {
        MatureKind::Copying => "gc-value-oracle-refc-copy",
        MatureKind::Immix => "gc-value-oracle-refc-immix",
    };
    CarrierAllocator::new(AllocatorConfig::builder().name(name).build())
        .expect("allocator construction maps nothing")
        .share()
}

/// Builds the container root `{handle, 42}` and returns `(container, handle)`, where `handle`
/// is the 8-word `[BinRef][ErlSubBits]` pair for `payload` (a full-binary window).
fn container_with_refc(c: &mut AnyCollector, payload: &RefcBinary) -> (Eterm, Eterm) {
    let handle = write_refc_pair(c, payload, 0, payload.len() * 8);
    let container = c
        .young_mut()
        .heap_mut()
        .tuple(&[handle, make_small(42)])
        .expect("the container tuple fits");
    (container, handle)
}

/// Element 0 of the container tuple — the on-heap off-heap handle.
fn container_handle(container: Eterm) -> Eterm {
    // SAFETY: `container` is a live boxed 2-tuple this test built, so its header word and
    // element 0 are readable.
    let header = unsafe { *boxed_val(container) };
    assert!(is_arity_value(header), "the root must be the container tuple");
    assert_eq!(beam_term::arityval(header), 2, "the container is a 2-tuple");
    // SAFETY: element 0 lies inside the live 2-element tuple just validated.
    unsafe { *boxed_val(container).add(1) }
}

/// **Invariant (e).** An off-heap refc binary's payload is **byte-preserved** across a move
/// (the on-heap handle relocates, the off-heap pointer word is left verbatim, the reference
/// count never moves) and **freed exactly once** when its owning container becomes
/// unreachable — observed through an isolated allocator's live-block and free-call counters.
#[test]
fn an_offheap_refc_binary_is_byte_preserved_and_freed_exactly_once() {
    const REFC_BYTES: &[u8] = b"an off-heap refc binary kept alive across every collection";

    for kind in BOTH {
        let alloc = test_allocator(kind);
        let payload = RefcBinary::with_allocator(alloc.clone(), REFC_BYTES).expect("payload");
        assert_eq!(alloc.stats().live_blocks, 1, "{kind:?}: one live buffer");
        assert_eq!(payload.refcount(), 1, "{kind:?}");

        // The off-heap list owns a second reference (slot 0); the test keeps its own, so the
        // single decrement a sweep performs is observable.
        let mut off_heap = OffHeap::new();
        let slot = off_heap.insert(payload.clone());
        assert_eq!(slot, 0, "{kind:?}: the first slot");
        assert_eq!(payload.refcount(), 2, "{kind:?}");
        assert_eq!(off_heap.len(), 1, "{kind:?}");

        let mut c = collector(kind);
        let (container, handle) = container_with_refc(&mut c, &payload);
        let handle_before = boxed_val(handle) as usize;
        let mut roots = vec![container];

        // A mix of policy-driven rounds and a forced fullsweep: the container *and* the handle
        // inside it are relocated every round, the off-heap pointer word stays verbatim, the
        // payload bytes are untouched and the reference count is stable.
        //
        // We deliberately do **not** assert that the handle's *address* changes on every round:
        // an address is an allocator artifact, not an observable value (the carrier allocator
        // may hand a later round a block an earlier round's `nursery.collapse`/mature swap
        // freed, which really does reproduce an earlier address). What IS asserted is where a
        // relocation is provable — round 0 promotes the whole term out of the nursery into the
        // pre-existing mature space (`in_mature` + a distinct live block ⇒ a distinct address),
        // and a fullsweep allocates its fresh region while the old one is still live, so the
        // region base must change.
        for round in 0..6 {
            let major = round == 5;
            let base_before = mature_base(&c);
            if major {
                c.collect_major(&mut roots).expect("major");
            } else {
                c.collect(&mut roots).expect("collection");
            }
            let ctx = format!("{kind:?} round {round}");

            let container_after = container_handle(roots[0]);
            assert!(
                in_mature(&c, container_after),
                "{ctx}: the handle must be relocated into the mature generation"
            );
            if round == 0 {
                // The nursery and the mature space are two *simultaneously live* allocations,
                // so the promotion provably changed the handle's address.
                assert_ne!(
                    boxed_val(container_after) as usize,
                    handle_before,
                    "{ctx}: the promotion out of the nursery must move the handle"
                );
            }
            if major && kind == MatureKind::Copying {
                // A *copying* fullsweep always evacuates into a freshly allocated region (the
                // old one is still live during the copy), so the region base must change. The
                // mark-region mature generation instead reclaims **in place** when the policy
                // keeps its capacity, which deliberately leaves an address unchanged — that is
                // a legitimate path, and only the *value* is observable (asserted above).
                assert_ne!(
                    mature_base(&c),
                    base_before,
                    "{ctx}: a copying fullsweep relocates into a freshly allocated region"
                );
            }
            // SAFETY: `container_after` is the live, copied `ErlSubBits`.
            unsafe {
                assert!(is_refc_binary(container_after), "{ctx}: still a refc binary");
                assert_eq!(
                    refc_bytes(container_after),
                    REFC_BYTES,
                    "{ctx}: the off-heap payload bytes are byte-preserved"
                );
            }
            assert_eq!(payload.refcount(), 2, "{ctx}: a collection never touches the refcount");
            assert_eq!(off_heap.overhead_bytes(), REFC_BYTES.len(), "{ctx}");
        }

        // The container is dropped from the roots, so a fullsweep reclaims the handle; the
        // now-unreachable off-heap entry is then released exactly once.
        let mut none: [Eterm; 0] = [];
        c.collect_major(&mut none).expect("major");
        assert_eq!(c.young().used_words(), 0, "{kind:?}: the nursery is empty");
        assert_eq!(
            off_heap.sweep(|_| false),
            1,
            "{kind:?}: exactly one entry released"
        );
        assert!(off_heap.is_empty(), "{kind:?}");
        assert_eq!(
            payload.refcount(),
            1,
            "{kind:?}: the release decremented the count exactly once"
        );
        // The payload is still readable through the surviving test handle: no use-after-free.
        assert_eq!(payload.as_bytes(), REFC_BYTES, "{kind:?}: the payload bytes are intact");
        assert_eq!(off_heap.sweep(|_| false), 0, "{kind:?}: nothing left to sweep");

        // Dropping the last reference frees the buffer exactly once — a leak would leave a live
        // block, a double free would trip the allocator's plausibility check.
        let before = alloc.stats();
        assert_eq!(before.live_blocks, 1, "{kind:?}");
        drop(payload);
        let after = alloc.stats();
        assert_eq!(after.live_blocks, 0, "{kind:?}: the payload's buffer was freed");
        assert_eq!(
            after.free_calls,
            before.free_calls + 1,
            "{kind:?}: the buffer was freed exactly once"
        );
    }
}

// ---------------------------------------------------------------------------
// A guard on the off-heap pointer word the copier must leave verbatim
// ---------------------------------------------------------------------------

/// A belt-and-braces check that the refc pair's `BinRef.val` word — the raw off-heap pointer —
/// is bit-for-bit what the nursery wrote, after a move. This is the one word a mis-sized or
/// mis-aligned copy would silently corrupt while the encoded bytes still looked plausible.
#[test]
fn the_offheap_pointer_word_is_left_verbatim_by_a_move() {
    let alloc = test_allocator(MatureKind::Copying);
    let payload = RefcBinary::with_allocator(alloc, b"pointer-word probe payload").expect("payload");

    let mut c = collector(MatureKind::Copying);
    let (container, handle) = container_with_refc(&mut c, &payload);
    // SAFETY: `handle` is the live `ErlSubBits` just written; `orig` (word 4) is a live `BinRef`
    // whose `val` word (word 1) is in-bounds.
    let val_before = unsafe {
        let orig = *boxed_val(handle).add(4);
        // SAFETY: `orig` is the live `BinRef` term just written.
        *boxed_val(orig).add(1)
    };
    assert_eq!(
        val_before.raw(),
        payload.data_ptr() as usize,
        "the nursery's `BinRef.val` word names the off-heap payload"
    );

    let mut roots = vec![container];
    c.collect_minor(&mut roots).expect("minor");
    c.collect_major(&mut roots).expect("major");

    let handle_after = container_handle(roots[0]);
    // SAFETY: `handle_after` is the live, copied `ErlSubBits`.
    let (val, next) = unsafe {
        let orig = *boxed_val(handle_after).add(4);
        assert!(in_mature(&c, orig), "the BinRef must be forwarded as well");
        bin_ref_parts(orig)
    };
    assert_eq!(
        val as usize,
        payload.data_ptr() as usize,
        "the off-heap pointer word must be byte-identical after a move"
    );
    assert_eq!(next as usize, 0, "the (empty) chain link must be left byte-identical");
}
