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

//! The **label-map cache** behind [`Machine::label_map`](crate::Machine) and
//! [`Machine::label_map_arc`](crate::Machine).
//!
//! [`build_label_map`](crate::interpreter) is a whole-module build: one traversal of
//! every function's `BTreeMap` label table plus one resolution of the module's entire
//! import table. It is a **pure function of the module's immutable content** — the
//! label slots come from `Function::labels`, the import table from `Module::imports`
//! resolved through the module's own load-time atom table — so a run that re-enters
//! the module it just ran would pay again for a value it already had. With this cache
//! the build happens **once per machine per module** while that module stays resident
//! in the cache, and every later probe for it is a hit (`MEASURED.md` carries the
//! per-module build costs).
//!
//! # Identity: the `Digest` plus the atom namespace plus the address — never content
//!
//! A *content* key would cost at least the build it replaces (hashing the decoded IR
//! is `src/jit/cache/key.rs`'s `ModuleKey::Content` arm). This cache keys on
//! **identity** instead, in the same two-part shape and with the same vocabulary:
//!
//! ```text
//! struct LabelKey {
//!     digest: [u8; 16],     // ModuleKey::Digest: the module's load-time MD5, O(1)
//!     atoms: AtomSnapshot,  // the atom namespace it was loaded against, O(1)
//!     address: usize,       // the module image the map's import owner check needs
//! }
//! ```
//!
//! * **`digest`** is [`Module::checksum`] — a 16-byte `Copy`, no hash and no
//!   allocation. It covers exactly the chunks that determine this map: `Code` (the
//!   label slots) and `AtU8`/`ImpT` (the import table). Two modules with the same
//!   digest were loaded from the same bytes and therefore produce the same map; a
//!   module with a different digest can never hit.
//! * **`atoms`** is [`Module::atom_table`]'s [`AtomSnapshot`] — three scalars
//!   witnessing the module's own index → name mapping. The digest says nothing about
//!   which *index* the module's table gave an import's atoms, and the cached import
//!   table holds the **names** it resolved; a second load of one `.beam` against a
//!   different table (a shared runtime namespace, a growing table) numbers them
//!   differently, so witnessing the namespace makes that a miss. A `Module::clone`
//!   mints a fresh table identity, so a clone is a miss too — an extra build, never a
//!   wrong map.
//! * **`address`** is the module image the map was built from. [`LabelMap`]'s import
//!   table answers only when it is asked about *its own* module (`imports_owner`), so
//!   a matching address is what makes the cached import table usable; any other
//!   address is a miss, which re-derives the table for the new image.
//!
//! Those three fields together prove **image identity**, which is why a hit is
//! indistinguishable from a fresh build: `digest` covers exactly the chunks the map is
//! derived from, `atoms` witnesses the namespace the import *names* were resolved
//! against, and `address` is the image [`LabelMap`]'s `imports_owner` check compares.
//! A cached map is therefore byte-identical to a fresh [`build_label_map`] of the same
//! image, and the rule that follows from it is absolute: **the cache may never key on
//! anything runtime-mutable** — no heap address outside the module image, no register or
//! process state — because those would let one entry answer with another image's map,
//! which is the only failure mode a cache of this shape has.
//!
//! # The two routes, and why neither can produce a stale hit
//!
//! * **The `Arc` route** ([`Machine::label_map_arc`](crate::Machine)) is the one that
//!   has the module handle: it stores the `Arc<Module>` clone in the entry, so the
//!   module cannot be dropped while the cache holds it and its address cannot be
//!   recycled — ABA-free by construction. Every caller that owns an `Arc<Module>` (the
//!   module-context registry) uses it.
//! * **The bare `&Module` route** ([`Machine::label_map`](crate::Machine)) is the one
//!   the frozen public run entries can offer, because `run_export`/`run_label`/
//!   `Machine::drive` take a borrowed `&Module` the machine may not own. It holds no
//!   handle, so the entry's address *can* be recycled by a later allocation. That is
//!   safe because the address is never the whole key: a recycled address holding
//!   different bytes has a different digest and a different atom namespace, so the
//!   probe misses and the map is rebuilt for the new image.
//!
//! A **synthetic** module — the `Module::default()`-shaped image a hand-built test or
//! an engine benchmark uses, whose checksum is all zero — has no honest content
//! identity at all (that is the JIT cache key's `Content` arm, which has to hash the
//! IR to identify one). It is therefore **never cached**: every run of it builds its
//! map, exactly as before this cache existed.
//!
//! # Retention: a bounded LRU, content-keyed and not run-keyed
//!
//! The cache holds up to [`LABEL_MAP_CAPACITY`] entries in a `Vec`, each carrying a
//! monotone recency `stamp`:
//!
//! * a **hit** — the same module again — answers the entry's `Arc<LabelMap>` and bumps
//!   that entry's stamp. The `Vec` is deliberately **never reordered on a hit**: a
//!   move-to-front would memmove up to `capacity` entries on every probe, which is the
//!   inner-loop regression `Machine::local_callees`' memo (`local_callee.rs`) measured
//!   and rejected for exactly this reason. A stamp costs one `u64` store on the probe's
//!   own cache line, and the probe itself is a linear comparison of 40-byte `Copy` keys
//!   (no hash, no allocation);
//! * a **miss** — a different module, a different digest or atom namespace, a recycled
//!   address, or no entry — *builds* the map, inserts it as most-recently-used and, when
//!   that takes the entry count past the capacity, evicts the entry with the **smallest**
//!   stamp (the least recently used one). Only the eviction path pays a second scan;
//! * an eviction is a **performance event, never a correctness one**: the evicted entry is
//!   released (its map survives for as long as the callers still holding an `Arc` to it
//!   need it) and the next probe for that module is a miss that rebuilds the
//!   byte-identical map;
//! * it is **not** cleared by [`Machine::reset`](crate::Machine). The value belongs to
//!   the module, not to a run, so a machine that runs one module repeatedly builds its
//!   map once and every later run is a hit;
//! * an entry filled through the `Arc` route keeps the module alive for as long as the
//!   entry exists; an entry filled through the bare route never answers for an address
//!   whose digest and atom namespace do not both match, so an address it does not hold
//!   alive can only ever hit a module with the identical content witness.
//!
//! **Why a cache and not one entry.** A machine alternates modules by construction: every
//! run re-registers its in-machine cross-module callees in the module-context registry
//! (`Machine::register_context`, which is cleared by [`Machine::reset`](crate::Machine)),
//! and each registration resolves its callee's map through the `Arc` route. With a single
//! entry that alternation rebuilds a whole-module label map per distinct callee per run.
//!
//! **Why 32.** The working set of a compile-shaped run is its entry module plus the
//! in-machine callees of the current phase (an in-process `string.erl` compile touches
//! of the order of 10–20 distinct modules per phase), so 32 leaves slack for a phase
//! boundary without ever being the reason a live module is missing. Retention is one
//! `Arc<LabelMap>` per entry — plus the `Arc<Module>` handle the `Arc` route adds, which
//! the caller owns in any case — and label maps are **per machine** copies, never shared
//! across machines, so the bound is `capacity × live machines`: tens of KB per machine,
//! not unbounded growth with the number of modules a program runs.

use std::sync::Arc;

use beam_loader::Module;
use beam_term::AtomSnapshot;

use crate::interpreter::{build_label_map, LabelMap};

/// The all-zero digest: `Module::default()`'s checksum, i.e. "this module was not
/// loaded from a `.beam` file and has no content identity". A module with this digest
/// is never cached (see the module docs).
const NO_DIGEST: [u8; 16] = [0u8; 16];

/// How many label maps one [`Machine`](crate::Machine) retains — the per-machine bound
/// of the LRU (see the module docs for why 32).
///
/// The cache is **per machine**, so the workspace's retention bound is
/// `LABEL_MAP_CAPACITY × live machines`: one small per-module vector per entry, no
/// cross-machine sharing and no growth with the number of distinct modules a program
/// runs. A `#[cfg(test)]` constructor ([`LabelMaps::with_capacity`]) narrows it so the
/// eviction policy is pinnable with a handful of modules.
pub(crate) const LABEL_MAP_CAPACITY: usize = 32;
/// The identity of one cached label map (see the module docs for why these three
/// fields are the honest key and why none of them needs a hash).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LabelKey {
    /// The module's load-time MD5 ([`Module::checksum`]).
    pub(crate) digest: [u8; 16],
    /// The atom namespace the module's imports were resolved against.
    pub(crate) atoms: AtomSnapshot,
    /// The address of the module image the map was built from.
    pub(crate) address: usize,
}

impl LabelKey {
    /// The key of `module`, or `None` when it has no honest content identity (a
    /// synthetic, all-zero-checksum image) and must therefore not be cached.
    #[inline]
    fn of(module: &Module) -> Option<LabelKey> {
        let digest = module.checksum();
        if digest == NO_DIGEST {
            return None;
        }
        Some(LabelKey {
            digest,
            atoms: module.atom_table.snapshot(),
            address: module as *const Module as usize,
        })
    }
}

/// One cached map: its key, the module handle the `Arc` route filled it with (if it was
/// that route), the map itself, and when it was last used.
struct Entry {
    /// The identity the map was built for.
    key: LabelKey,
    /// The module handle, when the entry was filled through the `Arc` route. Held so
    /// the module cannot be dropped — and its address cannot be recycled — while the
    /// entry names it.
    held: Option<Arc<Module>>,
    /// The built map. An `Arc` (not a plain map) so a call site can hold it across the
    /// `&mut self` calls a run makes, which is what keeps the borrow checker out of
    /// the driver.
    map: Arc<LabelMap>,
    /// The entry's recency: the value of [`LabelMaps::next_stamp`] at its latest use, so
    /// the entry with the **smallest** stamp is the least recently used one. A stamp
    /// (rather than an order in the vector) is what lets a hit update recency in place,
    /// with no `Vec` reordering on the probe path.
    stamp: u64,
}

/// The machine's **label-map cache**: a bounded LRU of up to `capacity` already-built
/// module-wide label indexes, so a re-entered module does not rebuild its own.
///
/// See the module docs for the key, the two routes, why a hit can never be stale, and
/// why recency is kept as a stamp instead of an ordering of the entries.
pub(crate) struct LabelMaps {
    /// The live entries, at most `capacity` of them. Order is meaningless — recency is
    /// [`Entry::stamp`], never a position — so an eviction is a `swap_remove`.
    entries: Vec<Entry>,
    /// The per-machine bound ([`LABEL_MAP_CAPACITY`] in production).
    capacity: usize,
    /// The stamp the next use will take; monotone, so a later use always compares
    /// greater than an earlier one.
    next_stamp: u64,
}

impl LabelMaps {
    /// An empty cache (every first probe is a miss) bounded by
    /// [`LABEL_MAP_CAPACITY`].
    pub(crate) fn new() -> LabelMaps {
        LabelMaps::bounded(LABEL_MAP_CAPACITY)
    }

    /// An empty cache holding at most `capacity` maps — the one constructor body.
    fn bounded(capacity: usize) -> LabelMaps {
        LabelMaps {
            entries: Vec::new(),
            // A zero-capacity cache could never answer a probe, so it would be a
            // pure cost: one entry is the smallest useful bound (and is exactly the
            // one-entry behaviour this cache had before it became an LRU).
            capacity: capacity.max(1),
            next_stamp: 0,
        }
    }

    /// Test-only: a cache bounded by `capacity`, so the eviction policy can be pinned
    /// with a handful of modules instead of [`LABEL_MAP_CAPACITY`]'s worth.
    #[cfg(test)]
    pub(crate) fn with_capacity(capacity: usize) -> LabelMaps {
        LabelMaps::bounded(capacity)
    }

    /// Test-only: how many entries are resident (never above the capacity).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The label map for `module` — the bare `&Module` route.
    ///
    /// A hit is a linear compare of the entries' keys (40 bytes each, `Copy`, no hash,
    /// no allocation) plus one `Arc` clone; a miss builds the map and inserts it as the
    /// most recently used entry, evicting the least recently used one if the cache is
    /// full. A synthetic module is built every time and never installed.
    #[inline]
    pub(crate) fn resolve(&mut self, module: &Module) -> Arc<LabelMap> {
        match LabelKey::of(module) {
            None => Arc::new(build_label_map(module)),
            Some(key) => match self.probe(key) {
                Some((_, map)) => map,
                None => self.fill(key, None, module),
            },
        }
    }

    /// The label map for `module` — the `Arc` route, which also keeps the module alive
    /// for as long as the entry names it.
    ///
    /// The key comparison is the same one the bare route makes (equal keys imply the
    /// same image at the same address, so the two routes cannot disagree); `held` adds
    /// the liveness guarantee, and is (re)installed on a hit whose entry was filled
    /// through the bare route so the guarantee holds no matter which route filled it.
    pub(crate) fn resolve_arc(&mut self, module: &Arc<Module>) -> Arc<LabelMap> {
        let Some(key) = LabelKey::of(module) else {
            return Arc::new(build_label_map(module));
        };
        match self.probe(key) {
            Some((index, map)) => {
                if let Some(entry) = self.entries.get_mut(index) {
                    if entry.held.is_none() {
                        entry.held = Some(Arc::clone(module));
                    }
                }
                map
            }
            None => self.fill(key, Some(Arc::clone(module)), module),
        }
    }

    /// The cached map for `key` and its entry index, if an entry is keyed on it — the
    /// hot path.
    ///
    /// A hit is also a *use*, so it takes the next stamp (making that entry the most
    /// recently used one) and returns the clone the caller needs anyway.
    #[inline]
    fn probe(&mut self, key: LabelKey) -> Option<(usize, Arc<LabelMap>)> {
        let index = self.entries.iter().position(|entry| entry.key == key)?;
        let stamp = self.take_stamp();
        let entry = self.entries.get_mut(index)?;
        entry.stamp = stamp;
        Some((index, Arc::clone(&entry.map)))
    }

    /// Build `module`'s map and install it as the most recently used entry, evicting the
    /// least recently used one when the cache is already full.
    fn fill(&mut self, key: LabelKey, held: Option<Arc<Module>>, module: &Module) -> Arc<LabelMap> {
        let map = Arc::new(build_label_map(module));
        let stamp = self.take_stamp();
        if self.entries.len() >= self.capacity {
            self.evict_lru();
        }
        self.entries.push(Entry {
            key,
            held,
            map: Arc::clone(&map),
            stamp,
        });
        map
    }

    /// The next recency stamp. `wrapping_add` is safe: should the counter ever wrap
    /// (2^64 uses), two entries could compare the wrong way round, which picks the wrong
    /// victim once — an extra rebuild, never a wrong map.
    #[inline]
    fn take_stamp(&mut self) -> u64 {
        let stamp = self.next_stamp;
        self.next_stamp = self.next_stamp.wrapping_add(1);
        stamp
    }

    /// Drop the least recently used entry (the smallest stamp). Only the eviction path
    /// scans, and `swap_remove` keeps the removal O(1) — the entries carry no order.
    fn evict_lru(&mut self) -> Option<Entry> {
        let mut victim = None;
        let mut oldest = u64::MAX;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.stamp < oldest {
                oldest = entry.stamp;
                victim = Some(index);
            }
        }
        Some(self.entries.swap_remove(victim?))
    }
}

impl Default for LabelMaps {
    fn default() -> LabelMaps {
        LabelMaps::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use beam_loader::{Function, Import, ImportRef, Instr, Label, Module};

    use crate::interpreter::{
        build_label_map, label_map_builds, reset_label_map_builds, LabelMap,
    };

    use super::{LabelKey, LabelMaps, LABEL_MAP_CAPACITY, NO_DIGEST};

    /// A module with one function holding `labels` (each mapped to its own code
    /// offset) and checksum `digest`.
    fn module_with(labels: &[u32], digest: [u8; 16]) -> Module {
        let mut module = Module::default();
        let name = module.atom_table.intern("m");
        module.name = name;
        module.checksum = digest;
        let mut table = BTreeMap::new();
        for (offset, label) in labels.iter().enumerate() {
            table.insert(Label(*label), offset);
        }
        module.functions.push(Function {
            func_label: Label(1),
            entry_label: Label(2),
            module: name,
            name,
            arity: 0,
            code: vec![Instr::Return],
            blocks: Vec::new(),
            labels: table,
        });
        module
    }

    /// `module` plus the two imports the equality test resolves (`lists:reverse/1` and
    /// `lists:duplicate/2`), both of whose atoms its own table holds.
    fn with_imports(mut module: Module) -> Module {
        let lists = module.atom_table.intern("lists");
        let reverse = module.atom_table.intern("reverse");
        let duplicate = module.atom_table.intern("duplicate");
        module.imports.push(Import {
            module: lists,
            function: reverse,
            arity: 1,
        });
        module.imports.push(Import {
            module: lists,
            function: duplicate,
            arity: 2,
        });
        module
    }
    #[test]
    fn a_second_probe_of_the_same_module_is_a_hit() {
        let module = module_with(&[1, 2, 3], [7; 16]);
        let mut cache = LabelMaps::new();

        let first = cache.resolve(&module);
        let second = cache.resolve(&module);
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "the second probe must answer with the cached map"
        );
        assert_eq!(second.resolve(Label(3)).unwrap(), (0, 2), "the map is intact");
    }

    #[test]
    fn a_synthetic_module_is_never_cached() {
        // `Module::default()`'s checksum is all zero: no content identity, so every
        // probe builds.
        let module = module_with(&[1], NO_DIGEST);
        assert!(LabelKey::of(&module).is_none());
        let mut cache = LabelMaps::new();
        let first = cache.resolve(&module);
        let second = cache.resolve(&module);
        assert!(
            !std::sync::Arc::ptr_eq(&first, &second),
            "a synthetic module must be rebuilt on every probe"
        );
        assert_eq!(second.resolve(Label(1)).unwrap(), (0, 0));
    }

    /// A re-probe is a **hit**, not a replacement: with a multi-entry LRU, going back to
    /// a module probed earlier answers that module's own cached map.
    #[test]
    fn a_second_probe_of_an_earlier_module_is_a_hit() {
        let a = module_with(&[1], [1; 16]);
        let b = module_with(&[1, 2], [2; 16]);
        let mut cache = LabelMaps::new();
        let a_first = cache.resolve(&a);
        let b_map = cache.resolve(&b);
        assert_eq!(b_map.resolve(Label(2)).unwrap(), (0, 1), "B's own map");
        // Back to A: a hit, and A's map is correct (never B's).
        let a_again = cache.resolve(&a);
        assert!(
            std::sync::Arc::ptr_eq(&a_first, &a_again),
            "A is still resident, so the return to A must hit"
        );
        assert_eq!(a_again.resolve(Label(1)).unwrap(), (0, 0));
        assert!(
            cache.resolve(&a).resolve(Label(2)).is_err(),
            "A defines no label 2"
        );
        assert_eq!(cache.len(), 2, "both modules are resident");
    }

    /// Alternating a working set that fits builds **each module exactly once**; the same
    /// alternation against a one-entry cache rebuilds every module on every pass.
    ///
    /// This is the shape a compile has: a machine enters its entry module, then each
    /// in-machine cross-module callee (the module-context registry resolves every one of
    /// them through the `Arc` route), then the entry module again — and `Machine::reset`
    /// clears the registry on every run, so a nested run re-registers them all. Both
    /// routes are exercised, because they share one cache.
    ///
    /// The counts come from the `cfg(test)` `build_label_map` tally, so the evidence is
    /// deterministic rather than timed: `C` builds at capacity `C`, `2 * C` at capacity
    /// 1 (the one-entry cache this replaced).
    #[test]
    fn alternation_over_a_wide_working_set_builds_each_module_once() {
        /// The working set: more than two modules, fewer than [`LABEL_MAP_CAPACITY`].
        const C: usize = 6;

        let modules: Vec<Module> = (0..C)
            .map(|i| module_with(&[1, 2, 3], [i as u8 + 1; 16]))
            .collect();
        let handles: Vec<Arc<Module>> = (0..C)
            .map(|i| Arc::new(module_with(&[1, 2, 3], [i as u8 + 1; 16])))
            .collect();

        // The bare `&Module` route, at the LRU's capacity: one build per module.
        let mut cache = LabelMaps::with_capacity(C);
        reset_label_map_builds();
        for _pass in 0..2 {
            for module in &modules {
                assert_eq!(
                    cache.resolve(module).resolve(Label(1)).unwrap(),
                    (0, 0),
                    "every pass answers with that module's own map"
                );
            }
        }
        assert_eq!(
            label_map_builds(),
            C,
            "capacity {C} holds the whole working set, so each module builds once and \
             every return to it hits (2 passes x {C} modules)"
        );
        assert_eq!(cache.len(), C, "and all {C} entries are resident");

        // The same working set against a one-entry cache: the pre-LRU behaviour, one
        // rebuild per alternation.
        let mut one = LabelMaps::with_capacity(1);
        reset_label_map_builds();
        for _pass in 0..2 {
            for module in &modules {
                one.resolve(module);
            }
        }
        assert_eq!(
            label_map_builds(),
            2 * C,
            "a one-entry cache rebuilds on every alternation: 2 passes x {C} modules"
        );
        assert_eq!(one.len(), 1, "and never holds more than its one entry");

        // The `Arc` route shares the same cache and the same policy: one build per
        // module over two full passes, and the handle re-installed on every hit.
        let mut arc_cache = LabelMaps::with_capacity(C);
        reset_label_map_builds();
        for _pass in 0..2 {
            for module in &handles {
                assert_eq!(arc_cache.resolve_arc(module).resolve(Label(3)).unwrap(), (0, 2));
            }
        }
        assert_eq!(
            label_map_builds(),
            C,
            "the Arc route (what the module-context registry uses) hits too"
        );
        assert_eq!(arc_cache.len(), C);

        let mut one_arc = LabelMaps::with_capacity(1);
        reset_label_map_builds();
        for _pass in 0..2 {
            for module in &handles {
                one_arc.resolve_arc(module);
            }
        }
        assert_eq!(
            label_map_builds(),
            2 * C,
            "a one-entry Arc-route cache rebuilds on every alternation as well"
        );
    }

    /// The production constructor's bound is [`LABEL_MAP_CAPACITY`]: one module more than
    /// the bound evicts exactly one entry, so a machine's retention is bounded by the
    /// const rather than by how many modules its program runs.
    #[test]
    fn the_production_cache_is_bounded_by_its_capacity_const() {
        let modules: Vec<Module> = (0..=LABEL_MAP_CAPACITY)
            .map(|i| module_with(&[1], [i as u8 + 1; 16]))
            .collect();
        let mut cache = LabelMaps::new();
        for module in &modules {
            cache.resolve(module);
        }
        assert_eq!(
            cache.len(),
            LABEL_MAP_CAPACITY,
            "one entry more than the capacity evicts the least recently used one"
        );
    }

    /// The cache evicts the **least recently used** entry, only that one, and never
    /// exceeds its capacity — and a re-inserted module becomes the most recently used.
    #[test]
    fn the_least_recently_used_entry_is_evicted() {
        let a = module_with(&[1], [1; 16]);
        let b = module_with(&[2], [2; 16]);
        let c = module_with(&[3], [3; 16]);
        let d = module_with(&[4], [4; 16]);

        let mut cache = LabelMaps::with_capacity(3);
        reset_label_map_builds();
        cache.resolve(&a);
        cache.resolve(&b);
        cache.resolve(&c);
        assert_eq!(label_map_builds(), 3, "three distinct modules, three builds");
        assert_eq!(cache.len(), 3, "the cache is full");

        // A is used again, so B — not A — is now the least recently used entry.
        cache.resolve(&a);
        assert_eq!(label_map_builds(), 3, "re-probing A is a hit");

        // D's insert must evict B.
        cache.resolve(&d);
        assert_eq!(label_map_builds(), 4, "D is a miss");
        assert_eq!(cache.len(), 3, "and the cache still never exceeds its capacity");

        // A, C and D survived: all hits.
        cache.resolve(&a);
        cache.resolve(&c);
        cache.resolve(&d);
        assert_eq!(label_map_builds(), 4, "the LRU victim was B, so A, C and D hit");

        // B was the victim, so it rebuilds — which makes B the most recently used entry
        // and A (the least recently used survivor) the next victim.
        assert_eq!(
            cache.resolve(&b).resolve(Label(2)).unwrap(),
            (0, 0),
            "the rebuilt map is B's own"
        );
        assert_eq!(label_map_builds(), 5, "B had been evicted, so it rebuilds");
        assert_eq!(cache.len(), 3);
        cache.resolve(&a);
        assert_eq!(
            label_map_builds(),
            6,
            "the re-inserted B really is the most recently used entry: A, the least \
             recently used survivor, is the victim instead"
        );
        assert_eq!(cache.len(), 3, "and the bound holds throughout");
    }

    /// A cached map is the map a fresh build produces.
    ///
    /// `LabelMap` derives no `PartialEq` (and `src/interpreter/` is outside this node's
    /// remit), so the equality is pinned through the map's whole observable surface:
    /// every label id in and around the module's id space — defined, undefined, and the
    /// `NO_FAIL` id `0` — and every import index plus one past the end. Both must answer
    /// identically, successes *and* the typed errors.
    #[test]
    fn a_cached_map_equals_a_fresh_build() {
        let module = with_imports(module_with(&[1, 3, 7], [4; 16]));
        let mut cache = LabelMaps::new();
        let cached = cache.resolve(&module);
        let fresh: LabelMap = build_label_map(&module);

        for id in 0..=9u32 {
            match (cached.resolve(Label(id)), fresh.resolve(Label(id))) {
                (Ok(cached_target), Ok(fresh_target)) => assert_eq!(
                    cached_target, fresh_target,
                    "label {id} resolves to the same target in both maps"
                ),
                (Err(cached_error), Err(fresh_error)) => assert_eq!(
                    cached_error.to_string(),
                    fresh_error.to_string(),
                    "label {id} is the same typed error in both maps"
                ),
                (cached, fresh) => panic!("label {id}: cached {cached:?} vs fresh {fresh:?}"),
            }
        }

        let normalize = |result: beam_common::Result<(&str, &str, u32)>| {
            result
                .map(|(module, function, arity)| (module.to_string(), function.to_string(), arity))
                .map_err(|error| error.to_string())
        };
        for index in 0..=module.imports.len() as u32 {
            assert_eq!(
                normalize(cached.import_target(&module, ImportRef(index))),
                normalize(fresh.import_target(&module, ImportRef(index))),
                "import {index} (one past the end included) answers identically"
            );
        }
        assert_eq!(
            normalize(cached.import_target(&module, ImportRef(0))),
            Ok(("lists".to_string(), "reverse".to_string(), 1)),
            "and the shared answer is the import's own name"
        );
    }    /// The ABA hazard the bare route has to survive: the entry names an address, the
    /// module at that address is replaced with different content, and the probe must
    /// not answer with the stale map.
    ///
    /// Writing the replacement **through the same allocation** makes the collision
    /// deterministic (`*boxed = other` reuses the address), which is exactly the
    /// situation a freed module's recycled address creates.
    #[test]
    fn a_recycled_address_with_different_content_misses() {
        let mut boxed = Box::new(module_with(&[1], [1; 16]));
        let address = &*boxed as *const Module as usize;
        let mut cache = LabelMaps::new();
        assert_eq!(cache.resolve(&boxed).resolve(Label(1)).unwrap(), (0, 0));

        // Same address, different bytes (a different digest *and* different labels).
        *boxed = module_with(&[1, 2, 3], [3; 16]);
        assert_eq!(
            &*boxed as *const Module as usize,
            address,
            "the replacement reuses the allocation, so the address is recycled"
        );
        let map = cache.resolve(&boxed);
        assert_eq!(
            map.resolve(Label(3)).unwrap(),
            (0, 2),
            "the probe must build the new image's map, never hand back the stale one"
        );
    }

    /// The same content witness reached through the `Arc` route keeps the module alive
    /// — the address cannot be recycled while the entry holds the handle.
    #[test]
    fn the_arc_route_holds_the_module_alive() {
        use std::sync::Arc;

        let module = Arc::new(module_with(&[1, 2], [5; 16]));
        let weak = Arc::downgrade(&module);
        let mut cache = LabelMaps::new();
        let first = cache.resolve_arc(&module);
        drop(module);
        assert!(
            weak.upgrade().is_some(),
            "the cache's entry must keep the module alive"
        );
        let second = cache.resolve_arc(&weak.upgrade().expect("still alive"));
        assert!(std::sync::Arc::ptr_eq(&first, &second), "and it is a hit");
    }

    /// A hit on an entry the bare route filled installs the handle, so the `Arc`
    /// route's liveness guarantee does not depend on which route filled the entry.
    #[test]
    fn an_arc_hit_on_a_bare_entry_takes_the_handle() {
        use std::sync::Arc;

        let module = Arc::new(module_with(&[1], [9; 16]));
        let mut cache = LabelMaps::new();
        cache.resolve(&module);
        let weak = Arc::downgrade(&module);
        let _map = cache.resolve_arc(&module);
        drop(module);
        assert!(
            weak.upgrade().is_some(),
            "the Arc route's hit must install the handle"
        );
    }

    #[test]
    fn a_fresh_atom_namespace_at_a_recycled_address_is_a_miss() {
        // Same address, same digest — but a **fresh atom table**, which is exactly what
        // a second load of the same `.beam` against another namespace produces (and what
        // a `Module::clone` produces directly: it mints a new table identity). The cached
        // import table holds the *names* the first table gave those atoms, so this must
        // not be a hit.
        let mut boxed = Box::new(module_with(&[1], [1; 16]));
        let mut cache = LabelMaps::new();
        let first = cache.resolve(&boxed);
        *boxed = module_with(&[1], [1; 16]);
        let second = cache.resolve(&boxed);
        assert!(
            !std::sync::Arc::ptr_eq(&first, &second),
            "a new atom namespace must miss, even at the same address and digest"
        );
        assert_eq!(second.resolve(Label(1)).unwrap(), (0, 0), "and be correct");
    }
}
