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

//! The resource-term **release seam** — the sound way for a caller that owns the reachability
//! decision to drop the reference a resource TERM holds (the sibling child of `resources.rs`).
//!
//! # Ground truth: the term itself holds a reference
//!
//! `erts/emulator/beam/erl_nif.c`: `enif_make_resource` (`:3197`) builds an `ERTS_MAGIC_REF` through
//! `erts_bld_resource_ref` → `erts_mk_magic_ref` → `erts_bin_inc_refc`, i.e. **the term owns one
//! reference on the resource**. The reference is dropped by the term GC when the term becomes
//! unreachable, and the release that takes the count to zero runs the type's `dtor` (through
//! `nif_resource_dtor_prologue` → `run_resource_dtor`, `:2961-3030`). `enif_get_resource` (`:3261`)
//! resolves the magic ref to the payload.
//!
//! This crate mints that term (`super::enif_make_resource`) and takes that reference
//! (`super::enif_make_resource_binary` likewise), but it has **no GC**: only the host knows which
//! terms are still reachable. The "the term died" signal is therefore a **caller-owned seam** — the
//! same shape as `beam_engine::Machine::retain_refc_binaries`: the caller supplies the reachability
//! predicate, this crate owns the reference-count decrement.
//!
//! # The caller contract (SOUNDNESS — read before calling [`release_resource_terms`])
//!
//! * `predicate(term) == false` asserts *"no NIF, process heap or rooted term can ever read this
//!   resource term again"*. That is a statement about the **caller's** root set, and only the caller
//!   can make it. **A false "unreachable" is a use-after-free**: the term's reference — whose block
//!   is the resource header + payload a NIF may still reach through a `void*` it obtained earlier
//!   from [`super::enif_get_resource`] — is dropped, and if it was the last reference the block is
//!   freed. The term's binding is removed at the same time, so a later `enif_get_resource` on that
//!   term answers `0` rather than reading freed memory; an already-obtained `void*` is not so lucky.
//! * *"reachable"* (a `true` from the predicate, or a term the predicate never sees because it was
//!   minted after the snapshot) is only ever a **leak**, never a wrong result.
//!
//! This is exactly the trade `beam_engine::Machine::retain_refc_binaries` documents for off-heap
//! refc binaries: the predicate **is** the caller's soundness assertion.
//!
//! # What is guaranteed
//!
//! * **No lock is held while the predicate runs.** The live terms are snapshotted under the registry
//!   lock, the predicate is evaluated with the lock released (so a predicate that itself calls
//!   [`resource_terms`] or the `enif_*` entry points cannot deadlock), and the registry is
//!   re-acquired only to take the bindings out.
//! * **Idempotent by term identity, under concurrency.** A binding is taken out of the registry with
//!   a single `remove` under the lock, and only the caller that got it drops the reference, so a term
//!   released twice — by two threads, or by two successive prunes — decrements the count **once** and
//!   never runs the destructor twice.
//! * **Exactly one reference per term is dropped**, through the very same path an explicit
//!   [`super::enif_release_resource`] takes (so the destructor's synchronous timing, count and
//!   environment cannot diverge between the two routes). An object that still has a user reference —
//!   the allocator's, or an `enif_keep_resource` — survives the prune with its count reduced by one
//!   and **no** destructor run; the later `enif_release_resource` runs it once.
//!
//! # What is unchanged
//!
//! The asynchronous (misc aux-work) destructor deferral is still **not** reproduced: the destructor
//! runs synchronously on the thread that dropped the last reference, whoever that is
//! (`resources.rs` → "The destructor's timing").

use super::*;

use libc::c_void;

/// A snapshot of the live resource terms this crate minted.
///
/// Every term here designates a resource object through [`super::enif_get_resource`], and each holds
/// one reference on that object (the magic reference's, see the module docs). The snapshot is taken
/// under the registry lock — which is **not** held while this function returns or afterwards — and is
/// ordered by the term word so a caller sees a deterministic list; the order carries no meaning.
///
/// This is the reachability question's input, not its answer: which of these terms is still live is
/// the caller's decision (see [`release_resource_terms`]).
pub fn resource_terms() -> Vec<Eterm> {
    let mut words: Vec<usize> = with_registry(|r| r.terms.keys().copied().collect());
    // A `HashMap`'s iteration order is unspecified; sorting makes the snapshot reproducible.
    words.sort_unstable();
    words.into_iter().map(Eterm::from_raw).collect()
}

/// Drop the reference held by every live resource term the `predicate` calls **unreachable**, and
/// return the number of terms released.
///
/// For each live resource term for which `predicate(term)` returns `false`:
///
/// 1. its term→object binding is removed (so `enif_get_resource` on that term answers `0` from then
///    on — the binding removal is the structural guard that makes a repeated release a no-op), then
/// 2. **exactly one** reference is dropped from the object — the reference the term took when it was
///    minted, through the same decrement path [`super::enif_release_resource`] uses; and
/// 3. if that was the object's last reference, the type's destructor runs (synchronously) and the
///    block is freed.
///
/// An object that still carries a user reference (the allocator's, or an `enif_keep_resource`)
/// survives with its count reduced by exactly one and its destructor **not** run.
///
/// # The predicate is the caller's soundness assertion
///
/// A `false` ("unreachable") for a term that can still be read is a **use-after-free**; a `true` for
/// a dead term is only a leak. The predicate is evaluated with **no registry lock held**, so it may
/// call back into this module or into any `enif_*` entry point. The whole call is safe under
/// concurrent invocations: a term is released exactly once, whichever thread gets to its binding
/// first.
///
/// # Example (the runtime's shape)
///
/// ```ignore
/// // `rooted` is the engine root set the collection just gathered; a resource term is live iff the
/// // collection can still reach it.
/// let released = release_resource_terms(|term| rooted.contains(&term.raw()));
/// ```
pub fn release_resource_terms<F: Fn(Eterm) -> bool>(predicate: F) -> usize {
    // 1. Snapshot the live term words. The lock is released here: nothing below holds it while the
    //    caller's predicate runs.
    let snapshot: Vec<usize> = with_registry(|r| r.terms.keys().copied().collect());
    if snapshot.is_empty() {
        return 0;
    }

    // 2. The caller's reachability decision, lock-free.
    let unreachable: Vec<usize> = snapshot
        .into_iter()
        .filter(|word| !predicate(Eterm::from_raw(*word)))
        .collect();
    if unreachable.is_empty() {
        return 0;
    }

    // 3. Take the bindings out under the lock. `remove` hands a binding to exactly one caller — a
    //    concurrent prune (or a second pass) finds nothing here and therefore cannot decrement the
    //    same object twice.
    let objects: Vec<*mut c_void> = with_registry(|r| {
        unreachable
            .iter()
            .filter_map(|word| r.terms.remove(word))
            .map(|entry| entry.obj)
            .collect()
    });

    // 4. Drop each term's own reference **outside** the lock: the decrement that reaches zero runs
    //    the destructor, which may call straight back into these entry points (it would deadlock on
    //    the registry lock) and which re-takes that lock itself to forget the object's bindings.
    for obj in &objects {
        // SAFETY: `obj` is a live payload pointer whose term reference is still outstanding. The
        // binding existed until step 3, and a live binding implies the term's own reference (the
        // invariant the C's magic reference rests on), so the count is at least one here and the
        // decrement cannot take a freed block to zero. `release` is the allocator's own documented
        // contract: a payload pointer with an outstanding reference.
        unsafe { enif_release_resource(*obj) };
    }

    objects.len()
}
