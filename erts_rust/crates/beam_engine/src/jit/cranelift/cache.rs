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

//! The Cranelift tier's **retained-artifact store**.
//!
//! [`CraneliftJit::compile`](super::CraneliftJit) lowers every IR function of a module
//! into a [`CompiledFunctions`] artifact that owns a Cranelift [`JITModule`] — i.e. an
//! executable mapping plus raw pointers into it. This module keeps such an artifact
//! **alive across entries**: compile once, reuse it by a sound module identity, keep it
//! mapped until the thread drops it or a test clears it. That is what makes every entry
//! of this tier ([`super::run_outcome`], [`super::run_label_outcome`],
//! [`super::run_resume_outcome`], [`JitBackend::run_export`](crate::jit::JitBackend::run_export))
//! both cheap on a repeat and **resumable**: a `receive` that parks the process leaves
//! the compiled code — and the label map the artifact carries — in place for the resume.
//!
//! # Thread-local, for the same reason the baseline cache is
//!
//! A [`CompiledFunctions`] holds raw pointers into its own executable mapping and is
//! therefore neither `Send` nor `Sync`, which rules out a process-global `static`. Each
//! thread owns its own store ([`ARTIFACTS`]), so a handle never crosses a thread
//! boundary.
//!
//! # The identity is the crate's ONE module identity: `CacheKey`
//!
//! The store keys on [`crate::jit::cache::CacheKey`] — the very key the baseline tier's
//! [`CodeCache`](crate::jit::cache::CodeCache) uses: the module's own `ModuleKey` (its
//! **load-time MD5** for a real `.beam`, else the [`module_fingerprint`] content hash of
//! a synthetic module) plus its **atom-namespace snapshot**
//! ([`beam_term::AtomTable::snapshot`], three scalars, O(1)).
//!
//! Sharing one scheme is deliberate: the namespace half is load-bearing for exactly the
//! reason it is for the baseline tier — this tier bakes atom **immediates** (`make_atom`
//! words) into the `move`/`test`/`select_val` fast paths it emits (and into the
//! `true`/`false` word a comparison fast path returns), so the very same `.beam` loaded
//! against a *different* namespace must be a **miss**; sharing one artifact across two
//! namespaces would be a wrong answer, never merely a stale one. And a question the
//! crate answers in one place cannot drift between the two native tiers.
//!
//! A synthetic (digest-less) module is therefore stored under its content fingerprint,
//! exactly as the baseline cache stores it: a second entry of the same hand-built module
//! recompiles nothing. The lookup then pays the fingerprint hash, which is the documented
//! price of caching a module that carries no O(1) identity.
//!
//! [`module_fingerprint`]: crate::jit::cache::module_fingerprint
//!
//! # Re-entrancy
//!
//! A run that re-enters this store while the borrow is live (a BIF or fun application
//! that itself runs a Cranelift export on this thread) must never panic on a `RefCell`
//! double-borrow: [`handle`] uses `try_borrow_mut` and compiles **uncached** on `Err`,
//! exactly as `crate::jit::cache::run_baseline_cached` does for its own store.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use beam_common::Result;
use beam_loader::Module;
use cranelift_frontend::FunctionBuilderContext;

use super::codegen::compile_module;
use super::CompiledFunctions;
use crate::jit::cache::CacheKey;

thread_local! {
    /// The per-thread store of compiled Cranelift artifacts.
    ///
    /// Thread-local because a [`CompiledFunctions`] owns an executable mapping and raw
    /// pointers into it, so it is neither `Send` nor `Sync`.
    static ARTIFACTS: RefCell<ArtifactCache> = RefCell::new(ArtifactCache::new());
}

/// The compile-once/run-many store: each entry **owns** its artifact, and therefore its
/// executable mapping, so the code stays mapped (and unmoved) for as long as the entry
/// lives.
struct ArtifactCache {
    /// Compiled artifacts by module identity; each entry owns its executable mapping.
    ///
    /// Held behind an [`Rc`] so a caller can take a cheap **handle**
    /// ([`handle`]) and keep the artifact alive after the store's borrow is released —
    /// which is required, because the driver runs with that handle while a re-entrant
    /// path may touch the store again.
    entries: HashMap<CacheKey, Rc<CompiledFunctions>>,
    /// How many compilations this store has performed (one per distinct identity).
    compiles: u64,
}

impl ArtifactCache {
    /// An empty store.
    fn new() -> ArtifactCache {
        ArtifactCache {
            entries: HashMap::new(),
            compiles: 0,
        }
    }

    /// Drop every artifact (freeing its executable mapping) and reset the counter.
    ///
    /// Test-only, with its [`clear`] wrapper: no library path releases the store (an
    /// entry lives until its thread ends), so the reset door exists for the suite's
    /// isolation and its retention evidence.
    #[cfg(test)]
    fn clear(&mut self) {
        self.entries.clear();
        self.compiles = 0;
    }
}

/// The compiled artifact for `module`, compiling and retaining it on first sight.
///
/// A **hit** (an equal [`CacheKey`]) hands back an [`Rc`] clone of the already-compiled
/// artifact and performs no compilation; a **miss** compiles the module once with
/// [`compile_module`], records the compilation and stores the result.
///
/// The handle is an owned [`Rc`], so the store's borrow is released **before** this
/// function returns and the driver runs with the artifact (and its executable mapping)
/// held alive by the caller — the same discipline
/// `crate::jit::cache::CodeCache::compiled_handle` uses for the baseline tier.
///
/// A re-entrant call — one made while this thread already holds the store's borrow —
/// compiles **uncached** (`try_borrow_mut` answers `Err`): a `RefCell` double-borrow is
/// not a diagnosable runtime error, and a compile is bounded by exactly the cost the
/// one-shot entry paid. It is always correct, merely not retained.
///
/// A compile failure propagates as the typed [`BeamError`](beam_common::BeamError) the
/// backend returns and inserts no partial entry, so a later call retries.
pub(super) fn handle(module: &Module) -> Result<Rc<CompiledFunctions>> {
    let key = CacheKey::of(module);
    ARTIFACTS.with(|cell| {
        let mut cache = match cell.try_borrow_mut() {
            Ok(cache) => cache,
            // Re-entrant call: the store is already borrowed by this thread's own
            // run. Compile uncached rather than panic (and never insert: the borrow
            // is not ours to extend).
            Err(_) => {
                return Ok(Rc::new(compile_module(
                    module,
                    &mut FunctionBuilderContext::new(),
                )?))
            }
        };
        if let Some(existing) = cache.entries.get(&key) {
            return Ok(Rc::clone(existing));
        }
        let compiled = Rc::new(compile_module(module, &mut FunctionBuilderContext::new())?);
        cache.compiles += 1;
        cache.entries.insert(key, Rc::clone(&compiled));
        Ok(compiled)
    })
}

/// The number of artifacts currently retained on this thread.
///
/// Introspection for the in-module suite (the evidence that a second run of the same
/// module recompiled nothing). A **re-entrant** call — one made from inside a run that
/// already holds the store's borrow — observes `0` rather than panicking; no compile or
/// run path consults this figure. Test-gated like the baseline store's
/// `fingerprint_calls` counter, because the library itself has no consumer.
#[cfg(test)]
pub(super) fn len() -> usize {
    ARTIFACTS.with(|cell| cell.try_borrow().map_or(0, |cache| cache.entries.len()))
}

/// The number of compilations this thread's store has performed.
///
/// A compile-once/run-many sequence increments this exactly once per distinct module
/// identity, which is what lets the suite prove the second run did not recompile. As
/// with [`len`], a re-entrant call observes `0` rather than panicking.
#[cfg(test)]
pub(super) fn compile_count() -> u64 {
    ARTIFACTS.with(|cell| cell.try_borrow().map_or(0, |cache| cache.compiles))
}

/// Drop every retained artifact (freeing its executable mapping) and reset the compile
/// counter.
///
/// This is the store's only release path, mirroring `clear_baseline_cache`: an entry
/// otherwise lives until its thread ends. A **re-entrant** call (the store is already
/// borrowed) is a documented no-op, never a panic — the same discipline
/// `crate::jit::cache::remember_module` follows.
#[cfg(test)]
pub(super) fn clear() {
    ARTIFACTS.with(|cell| {
        if let Ok(mut cache) = cell.try_borrow_mut() {
            cache.clear();
        }
    });
}
