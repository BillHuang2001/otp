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

//! The nested-run [`Machine`] pool: recycle a finished nested run's machine instead of
//! building a fresh one.
//!
//! [`RuntimeContext::run_export_blocking`](super::RuntimeContext) and
//! [`run_label_blocking`](super::RuntimeContext) drive one cross-module callee (or one local
//! `make_fun3` closure) per **nested run**. A fresh [`Machine::new`] per call throws away the
//! per-machine state the engine had already built, and the most expensive piece is the
//! **label-map cache** ([`Machine::label_maps`], `beam_engine/src/machine/label_cache.rs`):
//! [`build_label_map`](beam_engine::interpreter) is a whole-module value — a traversal of every
//! function's label table plus a resolution of the module's entire import table — so a fresh
//! machine pays `O(total instructions in the callee)` on **every** cross-module call into it.
//! The `x`/`fr` register files (1 024 words each) are also reallocated per call.
//!
//! # Where the pool lives, and why not on the context
//!
//! A pool field on [`RuntimeContext`](super::RuntimeContext) cannot serve the hottest route:
//! [`FunBridge::nested`](super::bridges) — the `beam_bifs::FunCaller` seam every applied fun
//! reaches (`lists:map/2`, `lists:foldl/3`, `maps:map/2`, …) — builds a **brand-new
//! `RuntimeContext`** for each applied fun, so a per-context free list would start empty on
//! every one of those calls and buy nothing. The pool is therefore a **thread-local**: the
//! runtime is single-threaded and a [`Machine`] is only ever used on the thread that popped it,
//! so a thread-local stack is reachable from a freshly built nested context too (the same
//! mechanism `crate::context::signal` and `crate::nif` already use).
//!
//! # Module affinity
//!
//! The label-map cache holds **one** entry, so a machine is only a hit for the module whose map
//! it last built. [`acquire`] therefore pops the pooled machine whose *last run was the same
//! module*, and only falls back to [`Machine::new`] when the pool has none for that callee. One
//! slot per callee is what turns "one whole-module build per cross-module call" into "one per
//! callee, ever" for a compile of any size.
//!
//! The affinity tag is a **hint, never an identity claim**: it is the callee image's address,
//! and a recycled address can only hand back a machine whose cache entry is keyed on a
//! different content witness — which misses and rebuilds. A stale hint therefore costs a build,
//! exactly as no pool would, and can never answer with another module's map
//! (`label_cache.rs`'s key is the digest + atom namespace + address, never a run).
//!
//! # Recursion, and why one slot is not enough
//!
//! Nested runs **recurse** (a nested run's callee makes its own cross-module calls), and a
//! machine that is running is never in the pool — [`acquire`] has taken it out and only
//! [`release`] puts it back. So a recursive entry into the same module simply creates another
//! machine, and no machine is ever handed to two live runs. The pool is a bounded stack
//! ([`MACHINE_POOL_CAPACITY`]) with least-recently-released eviction, so a deep or wide call
//! graph costs a bounded amount of retained state (each entry is one register file pair plus one
//! module's label map).
//!
//! # What a pooled machine must not retain
//!
//! [`release`] **resets** the machine ([`Machine::reset`], which NIL-fills the dirty register
//! regions and clears the stack, frames, catch frames, literals, contexts, suspense latches and
//! raise state — but deliberately keeps the label-map cache) and asserts that its off-heap
//! reservoirs are **empty**:
//!
//! * `refc_binaries`/`writable_payloads` are drained by
//!   [`adopt_nested_payloads`](super::RuntimeContext) — unconditionally, on the returning *and*
//!   the raising path — **before** the machine is released, because while the machine held those
//!   handles it was a `RefCount > 1` owner of every payload it built and the refc prune would
//!   have retained exactly the payloads the run was asking about. The drain is sound at that
//!   point only because `Runtime::adopt_escaped_binaries` has already cloned every handle into
//!   the runtime's escaped-payload store, so each payload keeps a live owner.
//! * No term survives a release either: `reset` refills exactly the register slots a writer
//!   reported (everything outside that region is already `NIL` by the engine's refill
//!   invariant) and clears the Erlang stack, so a recycled machine starts with no reachable term
//!   — and the only state it carries over, the label-map entry, holds no term at all.
//!
//! A [`debug_assert`] (rather than a silent skip) is what makes a regression of that drain a
//! loud test failure instead of a payload retained by an unseen owner.

use std::cell::RefCell;

use beam_engine::Machine;
use beam_loader::Module;

/// How many finished nested-run machines are kept for reuse.
///
/// A compile's call graph reaches a few dozen distinct callee modules (52 measured over a
/// 100 KB stdlib compile), but only a handful are hot at any one nesting depth. Thirty-two slots
/// cover the measured distinct-callee set: at eight slots a 100 KB compile still built 2 720
/// machines over 486 286 nested runs, at thirty-two it builds 334 — while the retained state
/// stays bounded (each entry is one register-file pair plus one module's label map). Beyond the
/// capacity the least-recently-released entry is dropped, which costs exactly what a fresh
/// machine cost before the pool existed.
const MACHINE_POOL_CAPACITY: usize = 32;

/// One recycled machine plus the affinity tag that makes it a hit.
struct Pooled {
    /// The finished nested run's machine, already [`Machine::reset`] and with empty off-heap
    /// reservoirs.
    machine: Machine,
    /// The address of the callee image the machine last ran — the affinity hint (see the module
    /// docs for why a recycled address is safe).
    last_module: usize,
}

thread_local! {
    /// The free stack, oldest first (a hit is looked up from the back).
    static POOL: RefCell<Vec<Pooled>> = const { RefCell::new(Vec::new()) };
}

// How many nested runs have had to build a **fresh** machine because the pool held none for
// their callee — the counter behind the reuse pins (test builds only; a production build has
// no counter and no extra work at all, [`fresh_machine`] is then a plain [`Machine::new`]).
#[cfg(test)]
thread_local! {
    static FRESH_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A freshly built machine, counted in a test build so a pin can prove the pool answered a
/// request instead of the allocator.
fn fresh_machine() -> Machine {
    #[cfg(test)]
    FRESH_BUILDS.with(|count| count.set(count.get() + 1));
    Machine::new()
}

/// A machine to run one nested run of `module` on: the pooled one that last ran that very
/// module, or a fresh machine when the pool holds none for it.
///
/// The machine is **out of the pool** until [`release`] puts it back, which is what makes a
/// recursive nested run unambiguous: no machine is ever handed to two live runs.
pub(super) fn acquire(module: &Module) -> Machine {
    let key = module_key(module);
    let recycled = POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        let position = pool
            .iter()
            .rposition(|pooled| pooled.last_module == key)?;
        Some(pool.remove(position).machine)
    });
    recycled.unwrap_or_else(fresh_machine)
}

/// Recycle the machine a finished nested run of `module` used: reset it, then push it onto the
/// free stack (evicting the least-recently-released entry when the pool is full).
///
/// The caller must have drained the machine's off-heap reservoirs first — see the module docs;
/// the assertion here is what keeps that invariant honest.
pub(super) fn release(mut machine: Machine, module: &Module) {
    debug_assert!(
        machine.refc_binaries().is_empty(),
        "a recycled nested-run machine must own no off-heap refc payload (the epilogue drains \
         the reservoir before releasing it)"
    );
    debug_assert_eq!(
        machine.writable_payload_count(),
        0,
        "a recycled nested-run machine must own no writable-append registration"
    );
    machine.reset();
    let key = module_key(module);
    POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        if pool.len() >= MACHINE_POOL_CAPACITY {
            pool.remove(0);
        }
        pool.push(Pooled {
            machine,
            last_module: key,
        });
    });
}

/// The callee image's address: the pool's affinity hint (never a cache key — see the module
/// docs).
fn module_key(module: &Module) -> usize {
    module as *const Module as usize
}

/// The pool's own state, for the reuse pins in the sibling suites.
#[cfg(test)]
pub(super) fn pooled_len() -> usize {
    POOL.with(|pool| pool.borrow().len())
}

/// The module keys the pool currently holds, oldest first, for the affinity pin.
#[cfg(test)]
pub(super) fn pooled_modules() -> Vec<usize> {
    POOL.with(|pool| pool.borrow().iter().map(|p| p.last_module).collect())
}

/// How many machines in the pool own an off-heap handle — a non-empty `refc_binaries`
/// reservoir or a live writable-append registration.
///
/// [`release`]'s `debug_assert!`s already fail loudly on the release that carries such a
/// handle, so this is the same invariant stated *after* the fact: a pin can assert the pool is
/// clean without depending on the assertion firing.
#[cfg(test)]
pub(super) fn pooled_reservoir_owners() -> usize {
    POOL.with(|pool| {
        pool.borrow()
            .iter()
            .filter(|pooled| {
                !pooled.machine.refc_binaries().is_empty()
                    || pooled.machine.writable_payload_count() != 0
            })
            .count()
    })
}

/// How many fresh machines the pool had to build on this thread since the last [`clear`].
#[cfg(test)]
pub(super) fn fresh_builds() -> usize {
    FRESH_BUILDS.with(std::cell::Cell::get)
}

/// Empties the pool and resets the fresh-build counter, so a pin starts from a known state (the
/// pool is thread-local and a test thread is reused across the cases of one suite).
#[cfg(test)]
pub(super) fn clear() {
    POOL.with(|pool| pool.borrow_mut().clear());
    FRESH_BUILDS.with(|count| count.set(0));
}
