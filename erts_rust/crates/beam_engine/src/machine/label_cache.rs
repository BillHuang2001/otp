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
//! the module it just ran pays again for a value it already had. Measured on the
//! `cowboy_bstr:to_lower/1` decomposition workload: 3.6 % of a timed call, paid by
//! every `run_*` entry.
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
//! # The invalidation rule
//!
//! The cache holds **one** entry, and that entry is **content-keyed, not run-keyed**:
//!
//! * a **miss** — a different module, a different digest or atom namespace, a recycled
//!   address, or no entry at all — *replaces* the entry; the previous map is released
//!   once the callers still holding an `Arc` to it are done with it;
//! * it is **not** cleared by [`Machine::reset`](crate::Machine). The value belongs to
//!   the module, not to a run, so a machine that runs one module repeatedly builds its
//!   map once and every later run is a hit;
//! * an entry filled through the `Arc` route keeps the module alive for as long as the
//!   entry exists; an entry filled through the bare route never answers for an address
//!   whose digest and atom namespace do not both match, so an address it does not hold
//!   alive can only ever hit a module with the identical content witness.
//!
//! **The residual:** one entry means a machine that alternates two modules across run
//! entries — an entry module plus one in-machine cross-module callee is exactly such
//! an alternation, because registering the callee fills the same one entry — rebuilds
//! on each switch back. That costs what it cost before this cache existed; it is never
//! fewer *or* more builds, and never a wrong map.

use std::sync::Arc;

use beam_loader::Module;
use beam_term::AtomSnapshot;

use crate::interpreter::{build_label_map, LabelMap};

/// The all-zero digest: `Module::default()`'s checksum, i.e. "this module was not
/// loaded from a `.beam` file and has no content identity". A module with this digest
/// is never cached (see the module docs).
const NO_DIGEST: [u8; 16] = [0u8; 16];

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

/// The one cached map: its key, the module handle the `Arc` route filled it with (if
/// it was that route), and the map itself.
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
}

/// The machine's **label-map cache**: one [`LabelMap`] for the module most recently
/// entered, so a re-entered module does not rebuild it.
///
/// See the module docs for the key, the two routes and the invalidation rule.
pub(crate) struct LabelMaps {
    entry: Option<Entry>,
}

impl LabelMaps {
    /// An empty cache (every first probe is a miss).
    pub(crate) fn new() -> LabelMaps {
        LabelMaps { entry: None }
    }

    /// The label map for `module` — the bare `&Module` route.
    ///
    /// A hit is one key comparison (40 bytes, no hash, no allocation) and an `Arc`
    /// clone; a miss builds the map and installs it as the entry. A synthetic module
    /// is built every time and never installed.
    #[inline]
    pub(crate) fn resolve(&mut self, module: &Module) -> Arc<LabelMap> {
        match LabelKey::of(module) {
            None => Arc::new(build_label_map(module)),
            Some(key) => match self.probe(key) {
                Some(map) => map,
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
            Some(map) => {
                if let Some(entry) = &mut self.entry {
                    if entry.held.is_none() {
                        entry.held = Some(Arc::clone(module));
                    }
                }
                map
            }
            None => self.fill(key, Some(Arc::clone(module)), module),
        }
    }

    /// The cached map for `key`, if the entry is keyed on it — the hot path.
    #[inline]
    fn probe(&self, key: LabelKey) -> Option<Arc<LabelMap>> {
        let entry = self.entry.as_ref()?;
        (entry.key == key).then(|| Arc::clone(&entry.map))
    }

    /// Build `module`'s map and install it as the entry (replacing whatever it held).
    fn fill(&mut self, key: LabelKey, held: Option<Arc<Module>>, module: &Module) -> Arc<LabelMap> {
        let map = Arc::new(build_label_map(module));
        self.entry = Some(Entry {
            key,
            held,
            map: Arc::clone(&map),
        });
        map
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

    use beam_loader::{Function, Instr, Label, Module};

    use super::{LabelKey, LabelMaps, NO_DIGEST};

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

    #[test]
    fn the_entry_is_replaced_on_every_miss() {
        let a = module_with(&[1], [1; 16]);
        let b = module_with(&[1, 2], [2; 16]);
        let mut cache = LabelMaps::new();
        cache.resolve(&a);
        let b_map = cache.resolve(&b);
        assert_eq!(b_map.resolve(Label(2)).unwrap(), (0, 1), "B's own map");
        // Back to A: a miss, and A's map is correct again (never B's).
        assert_eq!(cache.resolve(&a).resolve(Label(1)).unwrap(), (0, 0));
        assert!(
            cache.resolve(&a).resolve(Label(2)).is_err(),
            "A defines no label 2"
        );
    }

    /// The ABA hazard the bare route has to survive: the entry names an address, the
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
