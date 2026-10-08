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

//! End-to-end proof of the **map iterator** `enif_*` family (workspace contract A.3).
//!
//! `tests/fixtures/nif_mapiter.c` is a real C NIF — compiled at test time with the system `cc`
//! against the system `erl_nif.h` — whose seven `enif_map_iterator_*` calls are **undefined** in the
//! `.so` and are bound at `dlopen(RTLD_NOW)` against this test binary's dynamic symbol table
//! (`build.rs` adds `-Wl,--export-dynamic`). A successful load therefore already proves those
//! `#[no_mangle]` definitions exist and are callable; the assertions below pin the *observable
//! behaviour*, above all **the order the iterator walks**.
//!
//! # The oracle's contracts reproduced here (ground truth `erts/emulator/beam/erl_nif.c:3773-3952`)
//!
//! * `enif_map_iterator_create` positions at `FIRST` (pair 1) or `LAST` (pair `size`); it is `0`
//!   for a term that is not a map, and an *empty* flat map leaves the iterator in the C's own
//!   documented "bad shape" (`offset = size - 1` wraps, so `FIRST` gives `idx = 1` and `LAST`
//!   `idx = 0` — both head *and* tail, and neither yields a pair).
//! * `enif_map_iterator_get_pair` answers **only** for `0 < idx <= size`; at HEAD/TAIL it returns
//!   `0` and leaves the out-parameters untouched.
//! * `enif_map_iterator_next`/`prev` step the counter and report whether the iterator is still
//!   inside the range (`idx <= size` / `idx > 0`), so a walk runs off the end into TAIL (forward)
//!   or HEAD (backward).
//! * The walk order is the map's **storage order** — for a flat map (≤ 32 pairs) the map's own
//!   key/value arrays, i.e. the canonical (term/sorted) key order `maps:from_list/1` produces; for
//!   a hash map the C's `hashmap_iterator_next` DFS order (`erl_map.c:2346`).
//!
//! # Cross-checks
//!
//! * The expected order for every case is derived **independently on the Rust side** by building the
//!   same map with `beam_term::map::write_map` and reading it back with
//!   `beam_term::map::map_pairs_storage_order` — the order the C's iterator walk yields.
//! * [`the_oracle_agrees_on_the_flat_map_key_order`] additionally runs the **real system Erlang**
//!   over a tiny module generated in `$TMPDIR`: it builds the same map with `maps:from_list/1` and
//!   prints `maps:keys/1`. That is the ground-truth key order, and the fixture's C-side observation
//!   must equal it. The cross-check skips **loudly** when `erlc`/`erl` are unavailable.
//!
//! # Honest scope
//!
//! The `enif_map_iterator_*` C surface reports failures as the integer `0` (there is no typed error
//! channel across this C ABI), so "the C's failure code" *is* the refusal here; every such refusal
//! exercised by the fixture is asserted explicitly rather than papered over:
//! `create` on a non-map plus `get_pair` on the untouched iterator
//! ([`a_non_map_is_refused`]), and `get_pair` at HEAD/TAIL
//! ([`empty_map_reports_head_and_tail_without_yielding_a_pair`]).
//!
//! `enif_map_iterator_destroy` is idempotent and safe on a *never-created* (all-zero) iterator:
//! it applies the same `is_term_word` guard the read paths do, so a zeroed `it.map` is a no-op, and
//! `destroy` on a *created* iterator (both flat and hash arms) runs in every other NIF here. The
//! fixture's `nonmap/0` double-destroys a never-created iterator to pin this
//! ([`a_non_map_is_refused`]).
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system `erl_nif.h`; `compile_so` prints a loud
//! `SKIP:` banner and returns `None` when either is missing. When the toolchain **is** present every
//! assertion is hard.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::nif::loader::load;
use beam_abi::{NifEnv, NifHost};
use beam_term::atom::AtomTable;
use beam_term::map::{map_pairs_storage_order, map_words_capacity, write_map};
use beam_term::prelude::{
    car, cdr, is_list, is_nil, is_small, make_small, signed_value, tuple_element, tuple_size, Eterm,
};

use common::{compile_so, skip, TestHost};

/// The committed C fixture (see the file header for what it proves).
const FIXTURE_C: &str = include_str!("fixtures/nif_mapiter.c");

/// The fixture's `ERL_NIF_INIT` name — the loader requires it to match the calling module.
const MODULE: &str = "test_nif_mapiter";

// ---------------------------------------------------------------------------------------------
// Helpers — the same shape as the sibling `nif_ioq.rs` / `nif_env.rs` integration tests
// ---------------------------------------------------------------------------------------------

/// Compile the committed fixture; `None` (after a loud skip) when the toolchain is absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so(MODULE, FIXTURE_C)
}

/// `None` (after a loud skip) when the compiled `.so` path is not valid UTF-8.
fn so_path(so: &Path) -> Option<String> {
    match so.to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// A host plus the fixture loaded against it.
fn loaded(path: &str) -> (TestHost, beam_abi::nif::loader::NifLibrary) {
    let mut host = TestHost::new();
    let lib = load(path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");
    (host, lib)
}

/// The elements of the `n`-tuple `t` the fixture returned.
#[track_caller]
fn tuple_of(t: Eterm, n: usize) -> Vec<Eterm> {
    // SAFETY: `t` is a live tuple term produced by the fixture.
    let size = unsafe { tuple_size(t) };
    assert_eq!(size, n, "expected a {n}-tuple, got {t:?}");
    (0..n)
        // SAFETY: `i < size` for every `i` in the range.
        .map(|i| unsafe { tuple_element(t, i) })
        .collect()
}

/// The element as an `i64` small integer.
#[track_caller]
fn int_of(t: Eterm) -> i64 {
    assert!(is_small(t), "expected a small integer, got {t:?}");
    signed_value(t)
}

/// Every element of `elems` as an `i64` small integer.
#[track_caller]
fn ints(elems: &[Eterm]) -> Vec<i64> {
    elems.iter().copied().map(int_of).collect()
}

/// A proper list of small integers as a `Vec<i64>`.
#[track_caller]
fn int_list(t: Eterm) -> Vec<i64> {
    let mut out = Vec::new();
    let mut cur = t;
    while !is_nil(cur) {
        assert!(is_list(cur), "expected a proper list, got {cur:?}");
        // SAFETY: `cur` is a live cons cell.
        let (head, tail) = unsafe { (car(cur), cdr(cur)) };
        out.push(int_of(head));
        cur = tail;
    }
    out
}

/// An Erlang key list `[k0, k1, ...]` allocated on the host heap.
fn key_list(host: &mut TestHost, keys: &[i64]) -> Eterm {
    let mut list = Eterm::nil();
    for k in keys.iter().rev() {
        list = host
            .heap()
            .cons(make_small(*k), list)
            .expect("enough heap words for the key list");
    }
    list
}

/// The expected walk order, derived **independently** on the Rust side: build the same map
/// `K -> K*100` with `beam_term::map::write_map` and read it back in storage order (which is what
/// the C's iterator walk yields).
fn expected_pairs(keys: &[i64]) -> Vec<(i64, i64)> {
    let mut heap = beam_alloc::Heap::new(1 << 16).expect("a test heap is allocatable");
    let atoms = AtomTable::new();
    let pairs: Vec<(Eterm, Eterm)> = keys
        .iter()
        .map(|k| (make_small(*k), make_small(k * 100)))
        .collect();
    // SAFETY: `map_words_capacity(pairs.len())` fresh words, exclusively owned by `heap`, which
    // outlives every term derived from it. Only small integers (immediates) end up in the map, so
    // the pairs read back below carry no reference into `heap`.
    let map = unsafe {
        let hp = heap
            .alloc_words(map_words_capacity(pairs.len()))
            .expect("enough words for the map");
        write_map(hp, &pairs, &atoms).expect("a well-formed map")
    };
    map_pairs_storage_order(map)
        .expect("a well-formed map")
        .into_iter()
        .map(|(k, v)| (int_of(k), int_of(v)))
        .collect()
}

/// The `{Created, SizeField, Head0, Tail0, HeadEnd, TailEnd, Keys, Values}` tuple the fixture's
/// `forward/1` and `backward/1` return, decoded.
struct Walk {
    created: i64,
    size_field: i64,
    head0: i64,
    tail0: i64,
    head_end: i64,
    tail_end: i64,
    keys: Vec<i64>,
    values: Vec<i64>,
}

impl Walk {
    #[track_caller]
    fn of(got: Eterm) -> Walk {
        let e = tuple_of(got, 8);
        Walk {
            created: int_of(e[0]),
            size_field: int_of(e[1]),
            head0: int_of(e[2]),
            tail0: int_of(e[3]),
            head_end: int_of(e[4]),
            tail_end: int_of(e[5]),
            keys: int_list(e[6]),
            values: int_list(e[7]),
        }
    }

    /// The `(key, value)` pairs the C side actually observed.
    fn pairs(&self) -> Vec<(i64, i64)> {
        self.keys
            .iter()
            .copied()
            .zip(self.values.iter().copied())
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// The flat-map walk (both entry points), against the independently derived storage order
// ---------------------------------------------------------------------------------------------

/// A small, deliberately unsorted key set: the fixture's C-side observation must be the *canonical*
/// (sorted) order rather than the order the fixture passed in.
const FLAT_KEYS: [i64; 6] = [7, 1, 40, 3, 22, 11];

#[test]
fn forward_walk_reports_the_storage_order_and_runs_off_into_the_tail() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let list = key_list(&mut host, &FLAT_KEYS);
    let mut env = NifEnv::new(&mut host);

    let got = lib.call(&mut env, "forward", 1, &[list]).expect("forward/1");
    let walk = Walk::of(got);
    let want = expected_pairs(&FLAT_KEYS);
    assert_eq!(walk.created, 1, "create(FIRST) on a map succeeds");
    assert_eq!(walk.size_field, 6, "the frozen struct's `size` field is the pair count");
    // `create(FIRST)` sits on pair 1 of 6: neither head nor tail.
    assert_eq!((walk.head0, walk.tail0), (0, 0));
    // After the walk ran past the last pair: TAIL, not HEAD.
    assert_eq!((walk.head_end, walk.tail_end), (0, 1));
    assert_eq!(
        walk.pairs(),
        want,
        "the C walk must report the map's storage (sorted) order, not the input order"
    );
    // The values really are the keys' partners, not a shuffled or stale copy.
    assert!(walk.values.iter().zip(&walk.keys).all(|(v, k)| *v == k * 100));
    assert_eq!(walk.keys, vec![1, 3, 7, 11, 22, 40], "term order for small ints");
}

#[test]
fn backward_walk_from_last_is_the_exact_reverse_order() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let list = key_list(&mut host, &FLAT_KEYS);
    let mut env = NifEnv::new(&mut host);

    let got = lib.call(&mut env, "backward", 1, &[list]).expect("backward/1");
    let walk = Walk::of(got);
    let mut want = expected_pairs(&FLAT_KEYS);
    want.reverse();
    assert_eq!(walk.created, 1, "create(LAST) on a map succeeds");
    assert_eq!(walk.size_field, 6);
    // `create(LAST)` sits on the last pair: again neither head nor tail.
    assert_eq!((walk.head0, walk.tail0), (0, 0));
    // After walking backwards off pair 1: HEAD, not TAIL.
    assert_eq!((walk.head_end, walk.tail_end), (1, 0));
    assert_eq!(walk.pairs(), want, "the backward walk is the reverse storage order");
}

// ---------------------------------------------------------------------------------------------
// The empty map: create succeeds, reports head AND tail, and yields no pair
// ---------------------------------------------------------------------------------------------

#[test]
fn empty_map_reports_head_and_tail_without_yielding_a_pair() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let mut env = NifEnv::new(&mut host);

    let got = lib.call(&mut env, "empty", 0, &[]).expect("empty/0");
    let elems = tuple_of(got, 10);
    // {Created1, Head1, Tail1, PairRet1, PairKeyKept1, Created2, Head2, Tail2, PairRet2, PairKeyKept2}
    assert_eq!(
        ints(&elems),
        vec![1, 1, 1, 0, 1, 1, 1, 1, 0, 1],
        "both entries create successfully on an empty map; both are head *and* tail and neither \
         yields a pair, leaving the caller's out-parameters untouched"
    );
}

// ---------------------------------------------------------------------------------------------
// A non-map: `create` refuses, and the iterator stays safe to destroy (twice)
// ---------------------------------------------------------------------------------------------

/// `create` refusals: a non-map, and `get_pair` on the untouched (zeroed) iterator.
///
/// The fixture's `nonmap/0` also calls `enif_map_iterator_destroy` **twice** on that never-created
/// (all-zero) iterator, so this test pins that `destroy` is a safe no-op there: it applies the same
/// `is_term_word` guard the read paths do, so a zeroed `map == 0` is never treated as a boxed term.
/// `destroy` on a *created* iterator is covered by every other test in this file.
#[test]
fn a_non_map_is_refused() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let mut env = NifEnv::new(&mut host);

    let got = lib.call(&mut env, "nonmap", 0, &[]).expect("nonmap/0");
    let elems = tuple_of(got, 2);
    // {CreateRet, PairRet} — the C's `0` for a non-map, and `get_pair` on the untouched, zeroed
    // iterator is the index guard's `0`. Reaching this assertion proves the read path refused
    // without reading a null map.
    assert_eq!(ints(&elems), vec![0, 0]);
}

// ---------------------------------------------------------------------------------------------
// A >32-pair map (the hash arm) walks in `beam_term`'s storage order too
// ---------------------------------------------------------------------------------------------

#[test]
fn hash_map_walk_matches_the_rust_storage_order() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    // 40 pairs crosses `MAP_SMALL_MAP_LIMIT` (32), so the fixture builds a HAMT/champ map and the
    // iterator takes the hash arm (which caches the storage order at `create`).
    let keys: Vec<i64> = (0..40).rev().collect();
    let list = key_list(&mut host, &keys);
    let mut env = NifEnv::new(&mut host);

    let got = lib.call(&mut env, "forward", 1, &[list]).expect("forward/1");
    let walk = Walk::of(got);
    let want = expected_pairs(&keys);
    assert_eq!(want.len(), 40);
    assert_eq!(walk.created, 1);
    assert_eq!(walk.size_field, 40);
    assert_eq!((walk.head0, walk.tail0), (0, 0));
    assert_eq!((walk.head_end, walk.tail_end), (0, 1));
    assert_eq!(
        walk.pairs(),
        want,
        "the hash arm must reproduce beam_term's storage order (the C's DFS walk order)"
    );
    // Sanity: a hash map's storage order is *not* the sorted key order, so this case really
    // exercises the hash arm rather than degenerating into the flat one.
    let mut sorted = want.clone();
    sorted.sort();
    assert_ne!(want, sorted, "a 40-key map must not walk in plain sorted order");
}

// ---------------------------------------------------------------------------------------------
// The oracle: the real system Erlang's own key order for the same map
// ---------------------------------------------------------------------------------------------

/// One line of the oracle module's output: `<size> <keys list>`.
#[track_caller]
fn parse_oracle_line(line: &str) -> (i64, Vec<i64>) {
    let line = line.trim();
    let (size, list) = line
        .split_once(' ')
        .unwrap_or_else(|| panic!("unexpected oracle output: {line:?}"));
    let size: i64 = size
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("unexpected oracle size {size:?}: {e}"));
    let list = list.trim();
    let inner = list
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or_else(|| panic!("unexpected oracle key list: {list:?}"));
    let keys: Vec<i64> = if inner.trim().is_empty() {
        Vec::new()
    } else {
        inner
            .split(',')
            .map(|k| {
                k.trim()
                    .parse()
                    .unwrap_or_else(|e| panic!("unexpected oracle key {k:?}: {e}"))
            })
            .collect()
    };
    (size, keys)
}

/// Run the **real** Erlang over a module generated in `$TMPDIR` that builds `keys` into a map with
/// `maps:from_list/1` and prints `{size, maps:keys/1}`. `None` (after a loud skip) when `erlc`/`erl`
/// are unavailable or fail.
fn oracle_key_order(keys: &[i64]) -> Option<Vec<i64>> {
    let dir = std::env::temp_dir().join(format!("beam_abi-mapiter-oracle-{}", std::process::id()));
    if let Err(err) = std::fs::create_dir_all(&dir) {
        skip(&format!("mapiter oracle: cannot create scratch dir {dir:?}: {err}"));
        return None;
    }
    let module = "nif_mapiter_oracle";
    let key_literal = format!(
        "[{}]",
        keys.iter()
            .map(|k| k.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let source = format!(
        "-module({module}).\n\
         -export([main/0]).\n\n\
         main() ->\n\
         \x20   Keys = {key_literal},\n\
         \x20   M = maps:from_list([{{K, K * 100}} || K <- Keys]),\n\
         \x20   io:format(\"~w ~w~n\", [maps:size(M), maps:keys(M)]).\n"
    );
    let src = dir.join(format!("{module}.erl"));
    if let Err(err) = std::fs::write(&src, source) {
        skip(&format!("mapiter oracle: cannot write {src:?}: {err}"));
        return None;
    }
    // `erlc -o <dir> <src>`: compiling (rather than `-eval`) keeps the workload a real module, as
    // the objective requires, and proves the oracle program is ordinary, checkable Erlang.
    let compiled = Command::new("erlc").arg("-o").arg(&dir).arg(&src).output();
    match compiled {
        Err(err) => {
            skip(&format!("mapiter oracle: cannot run erlc ({err})"));
            return None;
        }
        Ok(out) if !out.status.success() => {
            skip(&format!(
                "mapiter oracle: erlc failed (exit {:?}):\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
            return None;
        }
        Ok(_) => {}
    }
    let run = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-s")
        .arg(module)
        .arg("main")
        .arg("-s")
        .arg("init")
        .arg("stop")
        .output();
    let output = match run {
        Err(err) => {
            skip(&format!("mapiter oracle: cannot run erl ({err})"));
            return None;
        }
        Ok(out) if !out.status.success() => {
            skip(&format!(
                "mapiter oracle: erl failed (exit {:?}):\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
            return None;
        }
        Ok(out) => out,
    };
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let line = stdout.lines().rev().find(|l| l.trim().starts_with(|c: char| c.is_ascii_digit()));
    let Some(line) = line else {
        skip(&format!("mapiter oracle: no result line in `erl` output: {stdout:?}"));
        return None;
    };
    let (size, order) = parse_oracle_line(line);
    assert_eq!(
        size,
        keys.len() as i64,
        "the oracle built a map of the same size (its key list is distinct)"
    );
    Some(order)
}

#[test]
fn the_oracle_agrees_on_the_flat_map_key_order() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    // The ground truth first: what does the *real* system ERTS say the key order is?
    let Some(oracle) = oracle_key_order(&FLAT_KEYS) else { return };

    let (mut host, lib) = loaded(&path);
    let list = key_list(&mut host, &FLAT_KEYS);
    let mut env = NifEnv::new(&mut host);
    let got = lib.call(&mut env, "forward", 1, &[list]).expect("forward/1");
    let walk = Walk::of(got);

    // The oracle's own canonical order is sorted for a small flat map; assert that too, so the
    // comparison is not vacuous (`maps:keys/1` on a >32-key map is *not* sorted).
    let mut sorted = FLAT_KEYS.to_vec();
    sorted.sort_unstable();
    assert_eq!(oracle, sorted, "the oracle's small-map key order is term order");
    assert_eq!(
        walk.keys, oracle,
        "the C-side iterator's observed key order must equal the real ERTS's own key order"
    );
    assert_eq!(walk.size_field, oracle.len() as i64);
}
