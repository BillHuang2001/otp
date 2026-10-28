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

//! The **label/import index** of one module image, built lazily and shared.
//!
//! [`build_label_map`] is a whole-module build: one traversal of every function's
//! `BTreeMap` label table plus one resolution of the module's entire import table.
//! It is a **pure function of the image's immutable content** — the label slots come
//! from `Function::labels`, the import table from `Module::imports` resolved through
//! the module's own load-time atom table — so a second consumer of the same image
//! would pay again for a value the image already determines.
//!
//! [`SharedLabelMap`] is the per-image slot that makes the build happen **at most
//! once per image**: [`Module::label_map`](crate::ir::Module::label_map) hands out an
//! `Arc<LabelMap>` that every consumer of that image shares, instead of one copy per
//! consumer (the execution engine used to keep a per-machine cache, re-deriving the
//! same immutable data once per machine).
//!
//! A [`Module::clone`](crate::ir::Module) is a **different image at a different
//! address**, so its slot starts empty and it derives its own map: the map's import
//! table answers only for the image it was built from ([`LabelMap::import_target`]'s
//! `imports_owner` check), and cloning a built map would leave that owner naming the
//! original.

use std::cell::Cell;
use std::fmt;
use std::sync::{Arc, OnceLock};

use beam_common::{BeamError, Result};

use crate::ir::{ImportRef, Module};
use crate::operand::Label;

/// The label → `(function index, code index)` and import index of **one** module
/// image.
///
/// Built once per image by [`build_label_map`] and shared through
/// [`Module::label_map`](crate::ir::Module::label_map); see the module docs for why
/// one build per image is enough (the map is a pure function of the image's
/// immutable content).
pub struct LabelMap {
    slots: Slots,
    /// `imports[ImportRef.0]`: the resolved `(module, function, arity)` of each
    /// `Module::imports` entry (see [`ImportTarget`] and [`resolve_imports`]).
    imports: ImportTable,
    /// The address of the [`Module`] `imports` was resolved from; `0` (an address
    /// no live `Module` can occupy) marks the table as unusable, so the lookup
    /// falls back to the per-call [`import_target`] and can never feed a name from
    /// a different module.
    imports_owner: usize,
}

/// One `Module::imports` entry, resolved **once per image** into the
/// `(module name, function name, arity)` triple an execution engine's `call_bif`
/// takes.
///
/// Resolving one import costs two atom-table lookups, and an interpreter used to
/// repeat them for *every* `gc_bif`/`bif`/`call_ext*` — the hottest instruction
/// class (measured: ~5 ns per BIF call, against ~0.5 ns for a cache hit). The two
/// names are stored **owned** rather than borrowed from the [`Module`]: the map is
/// also reached through a raw pointer inside a non-generic `#[repr(C)]` JIT runtime,
/// so the map cannot carry the module's lifetime. Copying the two names once per
/// image is the whole price of the cache.
struct ImportTarget {
    /// The imported module's name (the string `call_bif` dispatches on).
    module: Box<str>,
    /// The imported function's name.
    function: Box<str>,
    /// The import's declared arity (what `call_bif` is called with).
    arity: u32,
}

/// The per-image import table: `table[ImportRef.0]` is the resolved target, or the
/// reason string of the typed error that resolving that import produced (an atom
/// the module's atom table does not hold — see [`import_target`]). An index past
/// the end is the typed `call: import out of range`, exactly as before.
type ImportTable = Vec<std::result::Result<ImportTarget, &'static str>>;

/// The two backing representations of a [`LabelMap`].
enum Slots {
    /// `slots[label.0]` over ids `0..=max`, each entry the target **packed** into
    /// one word ([`pack_target`]); `slots[0]` is the unused `NO_FAIL` sentinel and
    /// stays [`UNUSED_LABEL`].
    ///
    /// Packing the `(func index, code index)` pair into a single `u64` is what makes
    /// [`LabelMap::resolve`] — one of the hottest non-dispatch operations in an
    /// execution loop (every branch, `test`-fail, `call`, `select_val` and `return`
    /// resolves one) — a single 8-byte load and one compare: the previous
    /// `Option<(u32, u32)>` slot was 12 bytes, so every lookup paid a
    /// `base + index * 12` `lea` chain *and* a separate discriminant-byte load and
    /// branch, and the table was 1.5x larger.
    Dense(Vec<u64>),
    /// `(label id, target)` sorted by id, for a sparse id space.
    Sparse(Vec<(u32, (u32, u32))>),
}

/// The "this label is not defined" sentinel of the dense slot table
/// ([`Slots::Dense`]).
///
/// A packed target is `func_index << 32 | code_index`, so the sentinel collides
/// only with the pair `(u32::MAX, u32::MAX)`: neither a module's function table nor
/// a function's code array can be `u32::MAX` entries long, and [`build_label_map`]
/// never emits a target with either half `u32::MAX` for a table that small. So a
/// defined label and the sentinel can never be confused.
const UNUSED_LABEL: u64 = u64::MAX;

/// Pack a `(func index, code index)` label target into one word for
/// [`Slots::Dense`] (the inverse of the unpacking [`LabelMap::resolve`] does).
#[inline]
const fn pack_target(target: (u32, u32)) -> u64 {
    ((target.0 as u64) << 32) | target.1 as u64
}

/// Unpack a [`Slots::Dense`] word back into its `(func index, code index)` target.
#[inline]
const fn unpack_target(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

impl LabelMap {
    /// An empty index: every lookup is the typed [`BeamError::BadArg`], and every
    /// import falls back to the uncached [`import_target`].
    pub fn new() -> LabelMap {
        LabelMap {
            slots: Slots::Dense(Vec::new()),
            imports: Vec::new(),
            imports_owner: 0,
        }
    }

    /// Record `label -> (function index, code index)`, an existing entry for the
    /// same label being replaced (labels are module-unique, so this only matters
    /// for a malformed/duplicate table — and it matches the previous last-wins
    /// `BTreeMap::insert`).
    pub fn insert(&mut self, label: Label, target: (u32, u32)) {
        match &mut self.slots {
            Slots::Dense(slots) => {
                let index = label.0 as usize;
                if index >= slots.len() {
                    slots.resize(index + 1, UNUSED_LABEL);
                }
                slots[index] = pack_target(target);
            }
            Slots::Sparse(entries) => match entries.binary_search_by_key(&label.0, |(id, _)| *id) {
                Ok(found) => match entries.get_mut(found) {
                    Some(entry) => entry.1 = target,
                    None => {}
                },
                Err(at) => entries.insert(at, (label.0, target)),
            },
        }
    }

    /// Resolve `label` to its `(function index, code index)`; a label the module
    /// does not define is the typed [`BeamError::BadArg`] (`badarg`, the same error
    /// an execution engine's unknown-label sentinel produces).
    #[inline]
    pub fn resolve(&self, label: Label) -> Result<(u32, u32)> {
        match &self.slots {
            Slots::Dense(slots) => match slots.get(label.0 as usize) {
                Some(&packed) if packed != UNUSED_LABEL => Ok(unpack_target(packed)),
                _ => Err(BeamError::BadArg),
            },
            Slots::Sparse(entries) => {
                match entries.binary_search_by_key(&label.0, |(id, _)| *id) {
                    Ok(found) => entries
                        .get(found)
                        .map(|(_, target)| *target)
                        .ok_or(BeamError::BadArg),
                    Err(_) => Err(BeamError::BadArg),
                }
            }
        }
    }

    /// Resolve an import reference to `(module name, function name, arity)`.
    ///
    /// The hot path of every `gc_bif`/`bif`/`call_ext*`: a **cache hit** is one
    /// address check, one bounds check and three loads from the per-image import
    /// table (`ImportTarget`) instead of the two atom-table lookups
    /// [`import_target`] performs per call.
    ///
    /// Nothing about the *semantics* changes: the table only answers when it was
    /// built from **this** module (`imports_owner`), and a table miss — an index
    /// past the end (`call: import out of range`), or an import whose module /
    /// function atom is not in the module's table (`call: import module atom` /
    /// `call: import function atom`) — is the exact typed error
    /// [`import_target`] produces for the same import. A map this cache does not
    /// cover (not built from this module: [`LabelMap::new`], `From`, ...) falls
    /// straight through to [`import_target`], so the two can never disagree.
    #[inline]
    pub fn import_target<'a>(
        &'a self,
        module: &'a Module,
        target: ImportRef,
    ) -> Result<(&'a str, &'a str, u32)> {
        if self.imports_owner == module as *const Module as usize {
            match self.imports.get(target.0 as usize) {
                Some(Ok(entry)) => {
                    return Ok((&entry.module, &entry.function, entry.arity));
                }
                Some(Err(reason)) => return Err(crate::error::unsupported(reason)),
                None => return Err(crate::error::unsupported("call: import out of range")),
            }
        }
        import_target(module, target)
    }

    /// The map's own heap retention, for capacity accounting.
    ///
    /// This counts the bytes the map itself allocates — the slot table's capacity
    /// (`capacity * size_of::<u64>()` for the dense form, `capacity *
    /// size_of::<(u32, (u32, u32))>()` for the sparse one) plus the import table's
    /// entry capacity plus the byte length of each boxed import name. It does **not**
    /// count the (immutable, already-owned) module the map was derived from, which
    /// the map only borrows information from.
    pub fn retained_bytes(&self) -> usize {
        let slots = match &self.slots {
            Slots::Dense(slots) => slots.capacity() * std::mem::size_of::<u64>(),
            Slots::Sparse(entries) => {
                entries.capacity() * std::mem::size_of::<(u32, (u32, u32))>()
            }
        };
        let imports = self.imports.capacity()
            * std::mem::size_of::<std::result::Result<ImportTarget, &'static str>>();
        let names: usize = self
            .imports
            .iter()
            .filter_map(|entry| entry.as_ref().ok())
            .map(|target| target.module.len() + target.function.len())
            .sum();
        slots + imports + names
    }
}

impl Default for LabelMap {
    fn default() -> LabelMap {
        LabelMap::new()
    }
}

impl<const N: usize> From<[(Label, (u32, u32)); N]> for LabelMap {
    fn from(pairs: [(Label, (u32, u32)); N]) -> LabelMap {
        let mut map = LabelMap::new();
        for (label, target) in pairs {
            map.insert(label, target);
        }
        map
    }
}

/// One lazily built, per-image label index, shared by every consumer of the image.
///
/// This is the field [`Module`] carries so that the whole-module [`build_label_map`]
/// happens **at most once in the image's lifetime**: the first
/// [`SharedLabelMap::get`] initializes it, every later call hands back a clone of the
/// same `Arc`, and every consumer (every execution machine) therefore shares one map
/// instead of re-deriving the same immutable data.
///
/// The map is derived only from the image's own immutable content, so it is not part
/// of the image's content identity and is never serialized, hashed or compared.
pub struct SharedLabelMap(OnceLock<Arc<LabelMap>>);

impl SharedLabelMap {
    /// An unbuilt slot: the first [`SharedLabelMap::get`] builds the map.
    pub const fn empty() -> SharedLabelMap {
        SharedLabelMap(OnceLock::new())
    }

    /// The image's label index, building it from `module` on first use.
    ///
    /// The build happens **exactly once** in the image's lifetime; every later call
    /// is a clone of the stored `Arc`.
    pub fn get(&self, module: &Module) -> Arc<LabelMap> {
        self.0
            .get_or_init(|| Arc::new(build_label_map(module)))
            .clone()
    }
}

/// A cloned [`Module`] is a **different image at a different address**, so it must
/// derive its own map.
///
/// Cloning the built map would leave its `imports_owner` naming the original image,
/// which would make [`LabelMap::import_target`] answer a *clone's* import query from
/// the *original's* resolved names — a wrong name the owner check exists to prevent.
/// Starting empty instead re-derives the map from the clone's own content.
impl Clone for SharedLabelMap {
    fn clone(&self) -> SharedLabelMap {
        SharedLabelMap::empty()
    }
}

impl fmt::Debug for SharedLabelMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedLabelMap")
            .field("built", &self.0.get().is_some())
            .finish()
    }
}

impl Default for SharedLabelMap {
    fn default() -> SharedLabelMap {
        SharedLabelMap::empty()
    }
}

// A per-thread counter of `build_label_map` calls.
//
// It makes the sharing observable: an image builds its map once and every consumer of
// that image sees the same allocation, so a test can assert the count. It is **not**
// `cfg(test)`: a dependent crate's unit tests compile this crate without its
// `cfg(test)`, and those tests need the counter. One thread-local increment per
// whole-module build (at most once per image) is why it is safe to leave in
// production builds.
thread_local! {
    static LABEL_MAP_BUILDS: Cell<usize> = const { Cell::new(0) };
}

/// The current thread's [`build_label_map`] call count.
pub fn label_map_builds() -> usize {
    LABEL_MAP_BUILDS.with(|c| c.get())
}

/// Reset the current thread's [`build_label_map`] call counter.
pub fn reset_label_map_builds() {
    LABEL_MAP_BUILDS.with(|c| c.set(0));
}

/// The upper bound on the label-count hint [`build_label_map`] reserves its entry
/// buffer from.
///
/// The hint is `Module::code_version`'s `label_count` — the `Code` chunk header's
/// label count, which is **exact** for a real `.beam` (28 for `tailcall.beam`, 1257
/// for `lists.beam`, 1195 for `string.beam`, i.e. the whole buffer is filled without
/// a reallocation). It is clamped because the count comes from the file: an absurd
/// header must not turn into an eager multi-megabyte reservation. The reservation is
/// only a hint in any case — the buffer still grows to whatever the module's label
/// tables actually hold (and a hand-built `Module` reports `0`, which is why the
/// clamp is a *cap*, never a requirement).
const LABEL_HINT_CAP: usize = 8192;

/// The initial capacity [`build_label_map`] reserves for its entry buffer.
fn label_hint(module: &Module) -> usize {
    (module.code_version.label_count as usize).min(LABEL_HINT_CAP)
}

/// Build the whole-module label/import index of `module`.
///
/// This is the function behind [`Module::label_map`](crate::ir::Module::label_map) and
/// therefore runs **at most once per image**; call it directly only when a map is
/// wanted without (or besides) the image's shared slot.
pub fn build_label_map(module: &Module) -> LabelMap {
    LABEL_MAP_BUILDS.with(|c| c.set(c.get() + 1));
    // ONE traversal of the module's label tables, into an entry buffer that is then
    // either packed into the dense slot table or kept as the sorted sparse list.
    //
    // The traversal — not the table filling — is what this costs: `Function::labels`
    // is a `BTreeMap`, so reaching each element is a chase into a separately allocated
    // node. The buffer's `len()` and the running `max` give the two facts the
    // dense/sparse decision needs (`count` and `max`), so one walk answers both.
    // Measured interleaved on one host (a reused scratch harness, 5k-20k builds per run):
    //
    // | module | labels | per build |
    // |---|---|---|
    // | `lists.beam` | 1256 | 6467 ns |
    // | `string.beam` | 1194 | 5974 ns |
    // | `tailcall.beam` | 27 | 193 ns |
    //
    // A build happens once per **image**, never once per consumer and never once per
    // run: `Module::label_map` serves it from the image's `SharedLabelMap`.
    //
    // The entries come out in **label-id order** within each function: a `BTreeMap`
    // iterates in key order, so the concatenation of the per-function tables is ordered
    // except across function boundaries. The dense branch does not care (it indexes by
    // label id), and the sparse branch sorts explicitly, so a duplicated label id is
    // last-wins either way.
    //
    // A real `.beam` numbers its labels `1..=count`, so `max + 1 == count + 1` and the
    // dense table is exactly as large as the label set (verified across the installed
    // OTP: density is exactly 1.0); the sparse form is for a pathological id space.
    let mut entries: Vec<(u32, (u32, u32))> = Vec::with_capacity(label_hint(module));
    let mut max = 0u32;
    for (func_index, function) in module.functions.iter().enumerate() {
        let func_index = func_index as u32;
        for (label, &code_index) in &function.labels {
            entries.push((label.0, (func_index, code_index as u32)));
            if label.0 > max {
                max = label.0;
            }
        }
    }
    let count = entries.len();
    let imports = resolve_imports(module);
    let imports_owner = module as *const Module as usize;
    if max as u64 <= (count as u64).saturating_mul(4) + 64 {
        let mut slots: Vec<u64> = vec![UNUSED_LABEL; max as usize + 1];
        for (label, target) in &entries {
            slots[*label as usize] = pack_target(*target);
        }
        LabelMap {
            slots: Slots::Dense(slots),
            imports,
            imports_owner,
        }
    } else {
        let mut sorted = entries;
        sorted.sort_unstable_by_key(|(id, _)| *id);
        LabelMap {
            slots: Slots::Sparse(sorted),
            imports,
            imports_owner,
        }
    }
}

/// Resolve **every** import of `module` once, producing the per-image import table
/// [`LabelMap::import_target`] looks up.
///
/// Each entry is either the resolved [`ImportTarget`] or the reason string of the
/// typed error resolution produced, so the cached path reproduces
/// [`import_target`]'s errors exactly (a missing module/function atom is
/// remembered per import, never substituted by a different name).
fn resolve_imports(module: &Module) -> ImportTable {
    module
        .imports
        .iter()
        .map(|import| {
            let module_name = module
                .atom_name(import.module)
                .ok_or("call: import module atom")?;
            let function_name = module
                .atom_name(import.function)
                .ok_or("call: import function atom")?;
            Ok(ImportTarget {
                module: module_name.into(),
                function: function_name.into(),
                arity: import.arity,
            })
        })
        .collect()
}

/// Resolve an import reference to `(module name, function name, arity)` — **the
/// uncached fallback**.
///
/// This is the per-call resolution an interpreter used to run for every
/// `gc_bif`/`bif`/`call_ext*` (two atom-table lookups and the `Vec` bounds check).
/// [`LabelMap::import_target`] answers from the per-image cache instead; this
/// function stays the definition of the import semantics and the path taken when
/// the cache cannot answer (a map not built from this module), so both agree
/// exactly — including on the three typed errors below.
pub fn import_target(module: &Module, target: ImportRef) -> Result<(&str, &str, u32)> {
    let import = module
        .imports
        .get(target.0 as usize)
        .ok_or_else(|| crate::error::unsupported("call: import out of range"))?;
    let module_name = module
        .atom_name(import.module)
        .ok_or_else(|| crate::error::unsupported("call: import module atom"))?;
    let function_name = module
        .atom_name(import.function)
        .ok_or_else(|| crate::error::unsupported("call: import function atom"))?;
    Ok((module_name, function_name, import.arity))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::instr::Instr;
    use crate::ir::{Function, Import};

    /// A one-function image whose `labels` table maps `labels[i]` to code index `i`.
    fn module_with(labels: &[u32]) -> Module {
        let mut module = Module::default();
        let name = module.atom_table.intern("m");
        module.name = name;
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

    /// `module` plus one import whose atoms its own table holds.
    fn with_import(mut module: Module, module_name: &str, function_name: &str, arity: u32) -> Module {
        let imported_module = module.atom_table.intern(module_name);
        let imported_function = module.atom_table.intern(function_name);
        module.imports.push(Import {
            module: imported_module,
            function: imported_function,
            arity,
        });
        module
    }

    #[test]
    fn one_image_is_built_once_for_every_consumer() {
        let module = module_with(&[1, 2, 3]);
        reset_label_map_builds();
        let first = module.label_map();
        assert_eq!(label_map_builds(), 1, "the first consumer builds the map");
        let mut handles = vec![first.clone()];
        for _ in 0..7 {
            let next = module.label_map();
            assert!(
                Arc::ptr_eq(&first, &next),
                "every consumer of one image shares the one allocation"
            );
            handles.push(next);
        }
        assert_eq!(label_map_builds(), 1, "eight consumers, one build");
        assert_eq!(
            Arc::strong_count(&first),
            handles.len() + 2,
            "the image's slot holds one handle, `first` one, every consumer another"
        );
        assert_eq!(first.resolve(Label(3)).unwrap(), (0, 2));
    }

    #[test]
    fn two_images_get_two_maps() {
        let a = module_with(&[1]);
        let b = module_with(&[1]);
        reset_label_map_builds();
        let map_a = a.label_map();
        let map_b = b.label_map();
        assert_eq!(label_map_builds(), 2, "two images, two builds");
        assert!(
            !Arc::ptr_eq(&map_a, &map_b),
            "one image's map is never another image's map"
        );
    }

    #[test]
    fn a_clone_is_a_different_image() {
        let module = with_import(module_with(&[1, 2, 3]), "lists", "reverse", 1);
        reset_label_map_builds();
        let original = module.label_map();
        assert_eq!(label_map_builds(), 1);

        let clone = module.clone();
        let cloned = clone.label_map();
        assert_eq!(
            label_map_builds(),
            2,
            "a clone is a different image, so it derives its own map"
        );
        assert!(!Arc::ptr_eq(&original, &cloned));

        // The clone's map answers for the clone's own content.
        let fresh = build_label_map(&clone);
        for label in [1, 2, 3] {
            assert_eq!(
                cloned.resolve(Label(label)).unwrap(),
                fresh.resolve(Label(label)).unwrap(),
                "the clone's map resolves the clone's labels"
            );
        }
        assert_eq!(cloned.import_target(&clone, ImportRef(0)).unwrap(), ("lists", "reverse", 1));
    }

    #[test]
    fn imports_answer_only_for_their_own_image() {
        let a = with_import(module_with(&[1]), "lists", "reverse", 1);
        let b = with_import(module_with(&[2]), "maps", "get", 2);
        let map_a = a.label_map();

        assert_eq!(
            map_a.import_target(&a, ImportRef(0)).unwrap(),
            ("lists", "reverse", 1),
            "the owning image answers from the cached table"
        );
        assert_eq!(
            map_a.import_target(&b, ImportRef(0)).unwrap(),
            import_target(&b, ImportRef(0)).unwrap(),
            "another image's query falls back and cannot see stale names"
        );
        assert_eq!(
            map_a.import_target(&b, ImportRef(0)).unwrap(),
            ("maps", "get", 2)
        );
        assert!(map_a.import_target(&b, ImportRef(9)).is_err());
        assert!(map_a.import_target(&a, ImportRef(9)).is_err());
    }

    #[test]
    fn retained_bytes_counts_the_table() {
        let small = module_with(&[1]);
        let labels: Vec<u32> = (1..=64).collect();
        let large = module_with(&labels);
        let small_map = small.label_map();
        let large_map = large.label_map();

        assert!(
            small_map.retained_bytes() >= 2 * std::mem::size_of::<u64>(),
            "a two-slot dense table is at least two words"
        );
        assert!(
            large_map.retained_bytes() > small_map.retained_bytes(),
            "the retention grows with the label count"
        );
    }
}
