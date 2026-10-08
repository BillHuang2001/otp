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

//! Map-family integration test: load a real `.beam` and run map ops end to end.
//!
//! Drives `beam_loader::load_file` over the committed oracle fixture
//! `tests/fixtures/mapfx.beam` (produced by the system `erlc`, the differential
//! oracle) and executes it on the interpreter: `get_map_elements`,
//! `has_map_fields`, `put_map_assoc` and `put_map_exact`, reached through
//! idiomatic Erlang.
//!
//! See `tests/fixtures/README.md` for how the fixture is regenerated.
//!
//! The flat-map arguments are built by the engine's own flat-map builder and keyed
//! by the **module's** atom table, so the fixture's key operands compare equal.
//! The **hashmap** (HAMT) *source* arguments are hand-written (see [`Hamt`]) so the
//! read path can be exercised against an arbitrary frozen node shape; a hashmap the
//! *engine builds* comes from `put_map_assoc`/`put_map_exact` crossing the flat-map
//! limit (`beam_term::map::write_map`).

use beam_common::BeamError;
use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{load_file, Module};
use beam_term::{cmp, make_atom, make_small, Atom, Eterm};

/// The committed oracle fixture (this crate owns it, unlike `slice.beam`).
const MAPFX_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mapfx.beam");

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

fn load() -> Module {
    load_file(MAPFX_BEAM).expect("load mapfx.beam")
}

/// A flat map keyed by `entries` (names resolved through `module`'s atom table).
fn flatmap(ctx: &mut MinimalContext, module: &Module, entries: &[(&str, i64)]) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = entries
        .iter()
        .map(|(name, value)| (make_atom(atom(module, name)), make_small(*value)))
        .collect();
    terms::make_flatmap(ctx, &pairs).expect("build a flat map")
}

/// Run `fun/arity` with `args`; `ctx` is threaded in so the arena outlives the run.
fn run(
    ctx: &mut MinimalContext,
    module: &Module,
    fun: &str,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    machine.run_export(module, atom(module, fun), args.len() as u32, args, ctx)
}

/// The value bound to `key` in the flat map `map` (exact key equality).
fn entry(map: Eterm, module: &Module, key: &str) -> Option<Eterm> {
    let target = make_atom(atom(module, key));
    terms::map_entries(map)
        .expect("a flat map")
        .into_iter()
        .find(|(candidate, _)| cmp(*candidate, target, &module.atom_table).is_eq())
        .map(|(_, value)| value)
}

// ---------------------------------------------------------------------------
// A minimal in-test hashmap (HAMT) fixture
// ---------------------------------------------------------------------------

/// `MAP_HEADER_TAG_SZ` (`erl_map.h:181-183`, from the frozen `beam_term` layout).
const MAP_HEADER_TAG_SZ: usize = beam_term::MAP_HEADER_TAG_SZ as usize;
/// `MAP_HEADER_ARITY_SZ` (`erl_map.h:181-183`).
const MAP_HEADER_ARITY_SZ: usize = beam_term::MAP_HEADER_ARITY_SZ as usize;

/// `MAP_HEADER_HAMT_HEAD_BITMAP(bitmap)` — `MAKE_MAP_HEADER(Tag = 3, Arity = 1,
/// Val = bitmap)` (`erl_map.h:181-192`).
fn head_bitmap_header(bitmap: u16) -> Eterm {
    let val = usize::from(bitmap);
    let size = (((val << MAP_HEADER_ARITY_SZ) | 1) << MAP_HEADER_TAG_SZ)
        | beam_term::MAP_HEADER_TAG_HAMT_HEAD_BITMAP;
    beam_term::make_header(size, beam_term::MAP_SUBTAG)
}

/// A hand-written **hashmap** (HAMT) fixture.
///
/// No crate in the rewrite can *build* a HAMT (`beam_engine::terms::map_entries`
/// can read one; the builder belongs to `beam_term`/`beam_codec` and does not
/// exist yet), so these tests write the frozen `erl_map.h` node blocks directly.
/// Every block lives in a `Box<[Eterm]>` the fixture keeps alive, so a sub-node
/// pointer stored in a parent node stays valid — moving the `Box` moves the
/// pointer, never the pointed-to block.
#[derive(Default)]
struct Hamt {
    blocks: Vec<Box<[Eterm]>>,
}

impl Hamt {
    fn new() -> Hamt {
        Hamt::default()
    }

    /// Store `words` and tag the block as a **boxed** map node.
    fn node(&mut self, words: Vec<Eterm>) -> Eterm {
        let block = words.into_boxed_slice();
        let term = beam_term::make_boxed(block.as_ptr());
        self.blocks.push(block);
        term
    }

    /// Store `[K, V]` and tag the block as a leaf (`TAG_PRIMARY_LIST`) cons cell.
    fn leaf(&mut self, key: Eterm, value: Eterm) -> Eterm {
        let cell = vec![key, value].into_boxed_slice();
        let term = beam_term::make_list(cell.as_ptr());
        self.blocks.push(cell);
        term
    }

    /// A bitmap **head** node `[MAP_HEADER_HAMT_HEAD_BITMAP(bmp), size, item…]`.
    fn head_bitmap(&mut self, bitmap: u16, size: usize, items: &[Eterm]) -> Eterm {
        assert_eq!(bitmap.count_ones() as usize, items.len(), "one item per set bit");
        let mut words = vec![head_bitmap_header(bitmap), Eterm::from_raw(size)];
        words.extend_from_slice(items);
        self.node(words)
    }

    /// A collision node `[make_arityval(n), item…]` (`erl_map.h:197,205`).
    fn collision(&mut self, items: &[Eterm]) -> Eterm {
        let mut words = vec![beam_term::make_arityval(items.len())];
        words.extend_from_slice(items);
        self.node(words)
    }
}

/// A 32-entry hashmap: a bitmap head whose single occupied slot holds a collision
/// node with the 32 leaves for `keys` (values are the leaf index).
fn hashmap_32(hamt: &mut Hamt, keys: &[Eterm]) -> Eterm {
    assert_eq!(keys.len(), 32);
    let leaves: Vec<Eterm> = keys
        .iter()
        .enumerate()
        .map(|(i, key)| hamt.leaf(*key, make_small(i as i64)))
        .collect();
    let child = hamt.collision(&leaves);
    hamt.head_bitmap(1, 32, &[child])
}

#[test]
fn fixture_exports_and_atoms_are_as_expected() {
    let module = load();
    assert_eq!(module.atom_name(module.name), Some("mapfx"));
    for (name, arity) in [
        ("get", 1),
        ("has", 1),
        ("assoc", 1),
        ("exact", 1),
        ("assoc_exact", 1),
    ] {
        assert!(
            module.export(atom(&module, name), arity).is_some(),
            "{name}/{arity} must be exported"
        );
    }
}

#[test]
fn get_reads_both_keys_into_a_tuple() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let map = flatmap(&mut ctx, &module, &[("a", 10), ("b", 20)]);
    let result = run(&mut ctx, &module, "get", &[map]).expect("get(#{a:=10,b:=20})");
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(10));
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(20));
}

#[test]
fn get_with_a_missing_key_takes_the_fail_label() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let map = flatmap(&mut ctx, &module, &[("a", 10)]);
    // The fail label runs `case_end`, which raises `{case_clause, Map}` uncaught.
    assert!(matches!(
        run(&mut ctx, &module, "get", &[map]).unwrap_err(),
        BeamError::Raise(_)
    ));
}

#[test]
fn has_reports_presence_of_both_keys() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let yes = make_atom(atom(&module, "yes"));
    let no = make_atom(atom(&module, "no"));

    let both = flatmap(&mut ctx, &module, &[("a", 1), ("b", 2)]);
    assert_eq!(run(&mut ctx, &module, "has", &[both]).unwrap(), yes);

    let one = flatmap(&mut ctx, &module, &[("a", 1)]);
    assert_eq!(run(&mut ctx, &module, "has", &[one]).unwrap(), no);

    // A non-map is caught by the `is_map` guard (the same fail label).
    assert_eq!(run(&mut ctx, &module, "has", &[make_small(0)]).unwrap(), no);
}

#[test]
fn assoc_associates_a_new_key() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let source = flatmap(&mut ctx, &module, &[("a", 1)]);
    let result = run(&mut ctx, &module, "assoc", &[source]).expect("assoc");
    assert_ne!(result, source, "the update builds a fresh map");
    assert_eq!(entry(result, &module, "a"), Some(make_small(1)));
    assert_eq!(entry(result, &module, "c"), Some(make_small(3)));
    assert_eq!(terms::map_entries(result).unwrap().len(), 2);
}

#[test]
fn exact_updates_an_existing_key() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let source = flatmap(&mut ctx, &module, &[("a", 1), ("b", 2)]);
    let result = run(&mut ctx, &module, "exact", &[source]).expect("exact");
    assert_eq!(entry(result, &module, "a"), Some(make_small(9)));
    assert_eq!(entry(result, &module, "b"), Some(make_small(2)));
    assert_eq!(terms::map_entries(result).unwrap().len(), 2);
}

#[test]
fn exact_with_a_missing_key_raises_badkey() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let source = flatmap(&mut ctx, &module, &[("b", 2)]);
    // `put_map_exact` carries no fail label, so the reason is `{badkey, a}`.
    assert!(matches!(
        run(&mut ctx, &module, "exact", &[source]).unwrap_err(),
        BeamError::Raise(_)
    ));
}

#[test]
fn assoc_exact_runs_both_updates() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let source = flatmap(&mut ctx, &module, &[("a", 1)]);
    let result = run(&mut ctx, &module, "assoc_exact", &[source]).expect("assoc_exact");
    assert_eq!(entry(result, &module, "a"), Some(make_small(9)));
    assert_eq!(entry(result, &module, "c"), Some(make_small(3)));
    assert_eq!(terms::map_entries(result).unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// Hashmap (HAMT) sources — the read path
// ---------------------------------------------------------------------------

#[test]
fn get_reads_a_hashmap_source() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let mut hamt = Hamt::new();
    let a = hamt.leaf(make_atom(atom(&module, "a")), make_small(10));
    let b = hamt.leaf(make_atom(atom(&module, "b")), make_small(20));
    let map = hamt.head_bitmap(0b0011, 2, &[a, b]);

    // The engine's reader enumerates the HAMT (both keys).
    assert_eq!(terms::map_entries(map).expect("a hashmap is readable").len(), 2);
    // ... and `get_map_elements` reads it exactly like a flat map.
    let result = run(&mut ctx, &module, "get", &[map]).expect("get on a hashmap");
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(10));
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(20));
}

#[test]
fn get_with_a_missing_key_in_a_hashmap_takes_the_fail_label() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let mut hamt = Hamt::new();
    let a = hamt.leaf(make_atom(atom(&module, "a")), make_small(10));
    let map = hamt.head_bitmap(0b0001, 1, &[a]);
    // `get/1`'s `case_end` fail label raises `{case_clause, Map}` uncaught.
    assert!(matches!(
        run(&mut ctx, &module, "get", &[map]).unwrap_err(),
        BeamError::Raise(_)
    ));
}

#[test]
fn has_reports_presence_in_a_hashmap() {
    let module = load();
    let mut ctx = MinimalContext::new();
    let yes = make_atom(atom(&module, "yes"));
    let no = make_atom(atom(&module, "no"));

    let mut both = Hamt::new();
    let a = both.leaf(make_atom(atom(&module, "a")), make_small(1));
    let b = both.leaf(make_atom(atom(&module, "b")), make_small(2));
    let both_map = both.head_bitmap(0b0011, 2, &[a, b]);
    assert_eq!(run(&mut ctx, &module, "has", &[both_map]).unwrap(), yes);

    let mut one = Hamt::new();
    let a = one.leaf(make_atom(atom(&module, "a")), make_small(1));
    let one_map = one.head_bitmap(0b0001, 1, &[a]);
    assert_eq!(run(&mut ctx, &module, "has", &[one_map]).unwrap(), no);
}

// ---------------------------------------------------------------------------
// Hashmap (HAMT) construction — growing a map past the flat-map limit
// ---------------------------------------------------------------------------

#[test]
fn assoc_that_exceeds_the_flat_map_limit_builds_a_hashmap() {
    // A 32-key source map plus the new key `c` makes 33 distinct keys, so the update
    // must build a **hashmap** — and every binding must be readable back.
    let module = load();
    let mut ctx = MinimalContext::new();
    let mut keys = vec![make_atom(atom(&module, "a"))];
    keys.extend((1..=31i64).map(make_small));
    let mut hamt = Hamt::new();
    let map = hashmap_32(&mut hamt, &keys);

    let result = run(&mut ctx, &module, "assoc", &[map]).expect("assoc grows to a 33-key map");
    assert!(terms::is_hashmap(result), "33 keys must be a hashmap");
    assert_eq!(terms::map_entries(result).unwrap().len(), 33);
    assert_eq!(
        beam_term::map::map_size(result, &module.atom_table).unwrap(),
        33
    );

    // Every source key survives with its original value, plus the new `c`.
    // (`hashmap_32` gives the leaf at index `i` the value `i`, so `a` ⇒ 0.)
    assert_eq!(entry(result, &module, "a"), Some(make_small(0)));
    for i in 1..=31i64 {
        assert_eq!(
            beam_term::map::map_get(result, make_small(i), &module.atom_table).unwrap(),
            Some(make_small(i)),
            "key {i}"
        );
    }
    assert_eq!(entry(result, &module, "c"), Some(make_small(3)));
}

#[test]
fn exact_on_a_32_pair_hashmap_builds_an_equivalent_flat_map() {
    // A ≤ 32-pair result still fits the flat-map layout, so `put_map_exact` updates
    // a hashmap source successfully; the *bindings* are identical and the ≤ 32-pair
    // result is written back as a flat map (the layout rule the oracle uses).
    let module = load();
    let mut ctx = MinimalContext::new();
    let mut keys = vec![make_atom(atom(&module, "a"))];
    keys.extend((1..=31i64).map(make_small));
    let mut hamt = Hamt::new();
    let map = hashmap_32(&mut hamt, &keys);

    let result = run(&mut ctx, &module, "exact", &[map]).expect("exact on a <=32-pair hashmap");
    assert!(!terms::is_hashmap(result), "32 pairs is a flat map");
    assert_eq!(entry(result, &module, "a"), Some(make_small(9)));
    assert_eq!(terms::map_entries(result).unwrap().len(), 32);
}
