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

//! The two request-path caches for lookups that are **pure functions of immutable loaded IR**:
//! the local-fun descriptor search behind [`RuntimeContext::local_fun_target`] and the
//! callee-admission verdict behind [`ExecutionContext::resolve_external_call`].
//!
//! [`RuntimeContext::local_fun_target`]: super::RuntimeContext
//! [`ExecutionContext::resolve_external_call`]: beam_engine::ExecutionContext::resolve_external_call
//!
//! Both lookups are paid once per *call*, not once per module: a single `compile:file/2` of
//! `lib/stdlib/src/string.erl` issues ~3.99 million in-machine cross-module entries and ~486
//! thousand applied `make_fun3` closures, and each of them used to re-derive a value that had
//! not changed since the module was loaded.
//!
//! # What is cached, and the identity it is keyed on
//!
//! * [`local_fun_descriptor`] — `(uniq, index) → (module handle, FunT descriptor)`. A local
//!   closure's term carries only its per-module `uniq` and its module-level lambda `index`, so
//!   resolving it means searching **every loaded module's** whole `FunT` table. The answer is a
//!   property of the *loaded module set*, so the index is invalidated by that set changing and
//!   by nothing else (see below). It is a **global** index, not a per-module one, because the
//!   key names no module.
//! * [`callee_admission`] — `(module identity, function atom, arity) → (call-site verdict,
//!   export label)`. Both halves are pure functions of one module's immutable IR (its `ImpT`,
//!   its `Code` chunk and its `ExpT`), so the entry cannot go stale while the module image is
//!   the same — no invalidation is needed at all, only an identity that a *different* image can
//!   never hit.
//!
//! # Why thread-local
//!
//! [`FunBridge::nested`](super::bridges) — the `beam_bifs::FunCaller` seam every applied fun
//! reaches (`lists:map/2`, `lists:foldl/3`, `maps:fold/3`, …) — builds a **brand-new
//! [`RuntimeContext`](super::RuntimeContext) for each applied fun, so a cache field on the
//! context would start empty on every one of those calls and buy nothing. The runtime is
//! single-threaded and a [`Module`] handle is only ever read on the thread that resolved it, so
//! a thread-local is the right home — the same mechanism [`machine_pool`](super::machine_pool)
//! and [`signal`](super::signal) already use.
//!
//! # The module-set generation (the descriptor index only)
//!
//! [`CodeServer::modules`](crate::codeserver::CodeServer::modules) is an uncached iterator over
//! the whole module map and the code server does not expose its `loaded_generation` counter, so
//! the generation is taken from the helper that *is* stamped with it:
//! [`CodeServer::fun_module_snapshot`](crate::codeserver::CodeServer::fun_module_snapshot)
//! documents that "an unchanged module set hands every caller the **same** [`Rc`]", and that it
//! is rebuilt exactly when a load (`insert_from_file`/`insert_loaded`) or a removal
//! (`forget_module`) bumps the generation. The index therefore stamps itself with that `Rc` and
//! keeps a clone of it:
//!
//! * the clone is what makes the test **exact** rather than probabilistic — a recycled address
//!   could otherwise make a fresh snapshot compare equal to a freed one, while a snapshot this
//!   cache keeps alive can never be freed, so a new one always has a different address;
//! * the index is built from [`CodeServer::modules`], **not** from the snapshot's own list,
//!   because the snapshot deliberately skips a module with no `.beam` path (a
//!   `code:load_binary/3` image) while the scan this cache replaces covers **every** loaded
//!   module. Using the snapshot as a list would silently make such a module's closures
//!   unresolvable.
//!
//! So a mid-run `code:load_file/1`, `code:load_binary/3` or `erlang:delete_module/1` is visible
//! to the very next lookup — the one direction that must not be missed, since a stale *negative*
//! answer turns a resolvable closure into [`BeamError::BadArg`](beam_common::BeamError).
//!
//! # What is deliberately **not** cached
//!
//! * `resolve_external_call`'s **overall** `Ok(Some)/Ok(None)` answer. That verdict folds in
//!   [`RuntimeContext::host_serves`](super::RuntimeContext), whose NIF arm
//!   ([`NifRegistry::is_loaded`](crate::nif::NifRegistry)) is **runtime-mutable**: a mid-run
//!   `erlang:load_nif/2` flips it for every key of that module, which is why the dispatch memo
//!   ([`dispatch`](super::dispatch)) keeps the NIF registry out of its own answer too. Caching
//!   the seam's answer would let an in-machine entry survive a `load_nif`, entering a module
//!   body whose functions are now NIF-implemented — a wrong result.
//! * `host_serves` itself, in any form: its floor/registry half is keyed on the *call's* strings,
//!   so hashing a key for it costs about what looking it up costs.
//! * [`CodeServer::module_handle`](crate::codeserver::CodeServer::module_handle)'s name lookup:
//!   different key (`&str`), and it must observe a reload.
//!
//! # Never a wrong answer, at worst a rebuild
//!
//! Every path here recomputes rather than guessing: an uncacheable module (a synthetic image
//! with no content identity) bypasses the cache; a full map is cleared, not trusted; and a
//! descriptor the index does not hold is looked up by the plain scan, which is exactly the
//! pre-cache behaviour.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use beam_loader::{FunDescriptor, Label, Module};
use beam_term::{Atom, AtomSnapshot};

use crate::codeserver::CodeServer;
use crate::hasher::FastHashMap;

/// The all-zero digest: [`Module::default`]'s checksum, i.e. "this image was not loaded from a
/// `.beam` file and has no content identity". Such a module is never cached (the same rule
/// `beam_engine`'s label-map cache applies, `machine/label_cache.rs`'s `NO_DIGEST`).
const NO_DIGEST: [u8; 16] = [0u8; 16];

/// How many callee-admission entries are kept before the map is cleared.
///
/// One entry per distinct `(callee module, function, arity)` the program has entered in-machine;
/// a whole `compile:file/2` of `lib/stdlib/src/string.erl` reaches a few hundred, so this leaves
/// an order of magnitude of headroom while keeping the retained state small (a key is an
/// [`AtomSnapshot`] plus 28 bytes, a value two words). Exceeding it costs one clear, i.e. what
/// the cache-less path paid on **every** call.
const ADMISSION_CAPACITY: usize = 8192;

/// The identity of one module **image**: its address, its load-time MD5 and the atom namespace
/// it was decoded against.
///
/// This is `beam_engine`'s `LabelKey` discipline (`machine/label_cache.rs`) minus the fields
/// this cache does not need:
///
/// * **`address`** separates two images that happen to share content (and makes the common
///   single-image case a single word compare);
/// * **`digest`** is [`Module::checksum`] — a `Copy` 16-byte witness of the chunks that decide
///   every answer here (`AtU8`/`ImpT`/`Code`/`ExpT`/`FunT`). A recycled address holding a
///   different image has a different digest, so it can never hit an entry built for the old one;
/// * **`atoms`** is the module's own atom-table snapshot, because the answers name atoms
///   (`call_site_reading_imports` resolves the import's module/function names) and a second load
///   of one `.beam` against a different namespace numbers them differently.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct ModuleKey {
    /// The module image's address.
    address: usize,
    /// [`Module::checksum`] — never all-zero (see [`NO_DIGEST`]).
    digest: [u8; 16],
    /// The module's own atom namespace at load time.
    atoms: AtomSnapshot,
}

impl ModuleKey {
    /// The key of `module`, or `None` for an image with no content identity (which is therefore
    /// never cached).
    fn of(module: &Module) -> Option<ModuleKey> {
        let digest = module.checksum();
        if digest == NO_DIGEST {
            return None;
        }
        Some(ModuleKey {
            address: module as *const Module as usize,
            digest,
            atoms: module.atom_table.snapshot(),
        })
    }
}

/// The in-machine cross-module seam's cached admission verdict for one
/// `(module image, function atom, arity)` — the two pure halves of
/// [`resolve_external_call`](beam_engine::ExecutionContext::resolve_external_call)'s guard chain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct CalleeAdmission {
    /// Whether the callee's own code reads the call site it runs at
    /// ([`callee_observes_its_call_site`](super::apply::callee_observes_its_call_site)), i.e.
    /// whether it must stay on the synchronous `call_bif` route.
    pub(super) observes_call_site: bool,
    /// [`Module::export`]'s answer — the entry label the synchronous `run_export` would enter,
    /// or `None` for a function the module does not export (the synchronous path's `undef`).
    pub(super) label: Option<Label>,
}

/// The cached callee-admission verdict of `(module, function, arity)`, computing it with
/// `compute` on a miss.
///
/// `compute` must be a **pure function of `module`'s immutable IR** — it is the cache's whole
/// soundness premise (see the module docs). It runs at most once per distinct key between two
/// clears of the map, i.e. once per callee the program enters rather than once per call.
pub(super) fn callee_admission(
    module: &Module,
    function: Atom,
    arity: u32,
    compute: impl FnOnce() -> CalleeAdmission,
) -> CalleeAdmission {
    let Some(key) = ModuleKey::of(module) else {
        #[cfg(test)]
        bump_admission(|stats| stats.bypassed += 1);
        return compute();
    };
    let entry = (key, function, arity);
    if let Some(hit) = ADMISSIONS.with(|cache| cache.borrow().get(&entry).copied()) {
        #[cfg(test)]
        bump_admission(|stats| stats.hits += 1);
        return hit;
    }
    let verdict = compute();
    ADMISSIONS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= ADMISSION_CAPACITY {
            cache.clear();
        }
        cache.insert(entry, verdict);
    });
    #[cfg(test)]
    bump_admission(|stats| stats.misses += 1);
    verdict
}

/// The defining module (a handle the code server retains) and the `FunT` descriptor of the
/// **local** (`make_fun3`) closure identified by `(uniq, index)`; `None` when no loaded module
/// defines one.
///
/// The first call after any load or removal rebuilds the whole `(uniq, index)` index from
/// [`CodeServer::modules`] in that map's own iteration order, keeping the first entry for a
/// duplicate key — which is exactly what the plain per-module scan this replaces answered
/// (`descriptor_of` on each module, first match wins). Every later call is one hash probe.
pub(super) fn local_fun_descriptor(
    code_server: &CodeServer,
    uniq: u64,
    index: u32,
) -> Option<(Arc<Module>, FunDescriptor)> {
    // The code server's cached snapshot is this cache's generation stamp (see the module docs);
    // a steady loaded set hands back the very same `Rc`, so this is a borrow plus a refcount.
    let generation = code_server.fun_module_snapshot();
    DESCRIPTORS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let stale = match cache.as_ref() {
            Some(index) => !Rc::ptr_eq(&index.generation, &generation),
            None => true,
        };
        if stale {
            let mut by_key = FastHashMap::default();
            for module in code_server.modules() {
                for descriptor in module.funs() {
                    by_key
                        .entry((u64::from(descriptor.uniq), descriptor.index))
                        .or_insert_with(|| (Arc::clone(module), *descriptor));
                }
            }
            #[cfg(test)]
            bump_descriptor(|stats| stats.rebuilds += 1);
            *cache = Some(DescriptorIndex {
                generation,
                by_key,
            });
        }
        let found = cache
            .as_ref()
            .and_then(|cached| cached.by_key.get(&(uniq, index)).cloned());
        #[cfg(test)]
        bump_descriptor(|stats| {
            if found.is_some() {
                stats.hits += 1
            } else {
                stats.misses += 1
            }
        });
        found
    })
}

/// The descriptor index plus the snapshot whose identity is the module-set generation it was
/// built at.
struct DescriptorIndex {
    /// Held (not merely compared) so that a superseded snapshot can never be freed and have its
    /// address reused — the test is [`Rc::ptr_eq`], which is exact only while both sides live.
    generation: Rc<Vec<(Arc<Module>, PathBuf)>>,
    /// `(uniq, index) → (defining module, `FunT` descriptor)`.
    by_key: FastHashMap<(u64, u32), (Arc<Module>, FunDescriptor)>,
}

thread_local! {
    /// The callee-admission entries (see [`callee_admission`]).
    static ADMISSIONS: RefCell<FastHashMap<(ModuleKey, Atom, u32), CalleeAdmission>> =
        RefCell::new(FastHashMap::default());
    /// The local-fun descriptor index (see [`local_fun_descriptor`]).
    static DESCRIPTORS: RefCell<Option<DescriptorIndex>> = const { RefCell::new(None) };
}

/// The caches' own event counts, for the pins in this module's suite.
#[cfg(test)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Stats {
    /// Lookups served from the cache.
    hits: u64,
    /// Lookups the cache did not hold (and so computed).
    misses: u64,
    /// Descriptor-index builds (one per observed module-set generation).
    rebuilds: u64,
    /// Lookups that bypassed the cache entirely (a module with no content identity).
    bypassed: u64,
}

#[cfg(test)]
thread_local! {
    static ADMISSION_STATS: std::cell::Cell<Stats> = const {
        std::cell::Cell::new(Stats { hits: 0, misses: 0, rebuilds: 0, bypassed: 0 })
    };
    static DESCRIPTOR_STATS: std::cell::Cell<Stats> = const {
        std::cell::Cell::new(Stats { hits: 0, misses: 0, rebuilds: 0, bypassed: 0 })
    };
}

#[cfg(test)]
fn bump_admission(update: impl FnOnce(&mut Stats)) {
    ADMISSION_STATS.with(|cell| {
        let mut stats = cell.get();
        update(&mut stats);
        cell.set(stats);
    });
}

#[cfg(test)]
fn bump_descriptor(update: impl FnOnce(&mut Stats)) {
    DESCRIPTOR_STATS.with(|cell| {
        let mut stats = cell.get();
        update(&mut stats);
        cell.set(stats);
    });
}

/// The callee-admission cache's counters — `(hits, misses, bypassed)` (test-only).
#[cfg(test)]
pub(super) fn admission_stats() -> (u64, u64, u64) {
    ADMISSION_STATS.with(|cell| {
        let stats = cell.get();
        (stats.hits, stats.misses, stats.bypassed)
    })
}

/// The descriptor index's counters — `(hits, misses, rebuilds)` (test-only).
#[cfg(test)]
pub(super) fn descriptor_stats() -> (u64, u64, u64) {
    DESCRIPTOR_STATS.with(|cell| {
        let stats = cell.get();
        (stats.hits, stats.misses, stats.rebuilds)
    })
}

/// Empties both caches so a pin starts from a known state (they are thread-local and a test
/// thread is reused across the cases of one suite).
#[cfg(test)]
pub(super) fn clear() {
    ADMISSIONS.with(|cache| cache.borrow_mut().clear());
    DESCRIPTORS.with(|cache| *cache.borrow_mut() = None);
    ADMISSION_STATS.with(|cell| cell.set(Stats::default()));
    DESCRIPTOR_STATS.with(|cell| cell.set(Stats::default()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codeserver::RecordedName;
    use crate::context::apply::callee_observes_its_call_site;
    use beam_loader::Import;

    /// A synthetic module image with a content identity (so it is cacheable) and the given
    /// `(uniq, index, label)` lambdas in its `FunT`.
    fn module_with_funs(name: &str, funs: &[(u32, u32, u32)]) -> Module {
        let mut module = Module::default();
        let name_atom = module.atom_table.intern(name);
        module.name = name_atom;
        module.checksum = [7u8; 16];
        module.funs = funs
            .iter()
            .map(|&(uniq, index, label)| FunDescriptor {
                function: name_atom,
                arity: 1,
                label: Label(label),
                index,
                num_free: 0,
                uniq,
            })
            .collect();
        module
    }

    /// A code server holding exactly `modules`, keyed by the name each one carries.
    fn code_server(modules: Vec<Module>) -> CodeServer {
        let mut server = CodeServer::new();
        for module in modules {
            let name = module
                .atom_name(module.name)
                .expect("the module names itself")
                .to_string();
            server.insert_loaded(&name, module, RecordedName::Str(format!("{name}.beam")));
        }
        server
    }

    /// The index resolves a lambda, and a steady loaded set is served from it after one build.
    #[test]
    fn a_steady_module_set_rebuilds_the_descriptor_index_once() {
        clear();
        let server = code_server(vec![
            module_with_funs("first", &[(11, 0, 100)]),
            module_with_funs("second", &[(22, 3, 200)]),
        ]);
        let (found, descriptor) =
            local_fun_descriptor(&server, 22, 3).expect("second's lambda resolves");
        assert_eq!(descriptor.label, Label(200));
        assert_eq!(descriptor.uniq, 22);
        assert_eq!(found.atom_name(found.name), Some("second"));
        assert_eq!(descriptor_stats(), (1, 0, 1), "one build, then the hit itself");
        for _ in 0..8 {
            let (again, _) = local_fun_descriptor(&server, 22, 3).expect("still resolves");
            assert!(Arc::ptr_eq(&found, &again), "the same code-server handle");
        }
        assert_eq!(descriptor_stats(), (9, 0, 1), "no rebuild while nothing loads");
        assert!(
            local_fun_descriptor(&server, 99, 9).is_none(),
            "an unknown lambda is still the caller's BadArg"
        );
        assert_eq!(descriptor_stats(), (9, 1, 1), "a miss costs no rebuild either");
    }

    /// A load made **after** the index was built is visible to the very next lookup, and a
    /// removal likewise.
    #[test]
    fn a_load_and_a_removal_both_invalidate_the_descriptor_index() {
        clear();
        let mut server = code_server(vec![module_with_funs("first", &[(11, 0, 100)])]);
        assert!(local_fun_descriptor(&server, 44, 7).is_none());
        assert_eq!(
            descriptor_stats(),
            (0, 1, 1),
            "a negative answer is not cached, but its build was"
        );
        server.insert_loaded(
            "late",
            module_with_funs("late", &[(44, 7, 300)]),
            RecordedName::Str("late.beam".to_string()),
        );
        let (module, descriptor) =
            local_fun_descriptor(&server, 44, 7).expect("the newly loaded module resolves");
        assert_eq!(descriptor.label, Label(300));
        assert_eq!(module.atom_name(module.name), Some("late"));
        assert_eq!(
            descriptor_stats(),
            (1, 1, 2),
            "the load's generation bump forces a rebuild, so the new module is found"
        );
        assert!(
            server.forget_module("late"),
            "the module is in the cache before the removal"
        );
        assert!(
            local_fun_descriptor(&server, 44, 7).is_none(),
            "a forgotten module must no longer resolve its closures"
        );
        assert_eq!(descriptor_stats(), (1, 2, 3), "the removal forces a rebuild too");
    }

    /// The seam's two halves are cached together, and the cached verdict equals the uncached one.
    #[test]
    fn the_callee_admission_verdict_is_cached_and_matches_the_uncached_predicate() {
        clear();
        let mut module = Module::default();
        let name = module.atom_table.intern("nifmodule");
        module.name = name;
        module.checksum = [9u8; 16];
        let erlang = module.atom_table.intern("erlang");
        let load_nif = module.atom_table.intern("load_nif");
        module.imports = vec![Import {
            module: erlang,
            function: load_nif,
            arity: 2,
        }];
        let function = module.atom_table.intern("wrapper");
        let uncached = callee_observes_its_call_site(&module, function, 0);
        assert!(
            uncached,
            "a module importing erlang:load_nif/2 refuses every function"
        );
        for _ in 0..3 {
            let cached = callee_admission(&module, function, 0, || CalleeAdmission {
                observes_call_site: uncached,
                label: module.export(function, 0),
            });
            assert_eq!(
                cached,
                CalleeAdmission {
                    observes_call_site: uncached,
                    label: None
                },
                "the cached verdict must equal the predicate's own answer"
            );
        }
        assert_eq!(
            admission_stats(),
            (2, 1, 0),
            "one computation, then two hits — never a second scan"
        );
    }

    /// A **different image at the same address** must miss: the digest (and the atom namespace)
    /// is part of the key, not just the address.
    #[test]
    fn a_different_image_at_the_same_address_does_not_hit() {
        clear();
        let mut module = Module::default();
        let name = module.atom_table.intern("recycled");
        module.name = name;
        module.checksum = [1u8; 16];
        let function = module.atom_table.intern("f");
        let key = ModuleKey::of(&module).expect("a checksummed image has a key");
        let recorded = CalleeAdmission {
            observes_call_site: false,
            label: Some(Label(17)),
        };
        ADMISSIONS.with(|cache| {
            cache.borrow_mut().insert((key, function, 0), recorded);
        });
        // The same image is a hit …
        assert_eq!(
            ADMISSIONS.with(|cache| cache.borrow().get(&(key, function, 0)).copied()),
            Some(recorded)
        );
        // … a different digest at that very address is not …
        let other_content = ModuleKey {
            digest: [2u8; 16],
            ..key
        };
        assert_eq!(
            ADMISSIONS.with(|cache| cache.borrow().get(&(other_content, function, 0)).copied()),
            None,
            "a recycled address holding different bytes must miss"
        );
        // … and neither is the same digest decoded against another atom namespace.
        let mut other_atoms = Module::default();
        let other_name = other_atoms.atom_table.intern("recycled");
        other_atoms.name = other_name;
        let other_namespace = ModuleKey {
            address: key.address,
            digest: key.digest,
            atoms: other_atoms.atom_table.snapshot(),
        };
        assert_ne!(other_namespace, key, "a second load numbers atoms differently");
    }

    /// A module with no content identity (`Module::default`'s all-zero checksum) is never
    /// cached, so a hand-built image can never occupy an entry.
    #[test]
    fn a_synthetic_image_bypasses_the_cache() {
        clear();
        let module = Module::default();
        for _ in 0..2 {
            let verdict = callee_admission(&module, Atom::MAX, 0, || CalleeAdmission {
                observes_call_site: false,
                label: None,
            });
            assert_eq!(verdict.label, None);
        }
        assert_eq!(
            admission_stats(),
            (0, 0, 2),
            "two lookups, both bypassed — nothing was stored"
        );
        assert_eq!(
            ADMISSIONS.with(|cache| cache.borrow().len()),
            0,
            "a synthetic module must leave no entry behind"
        );
    }
}
