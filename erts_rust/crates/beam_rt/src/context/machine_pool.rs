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
//! every one of those calls and buy nothing. The pool is therefore a **thread-local** (the same
//! mechanism `crate::context::signal` and `crate::nif` already use): the runtime is
//! single-threaded and a [`Machine`] is only ever used on the thread that popped it.
//!
//! # The shape: one slot per callee, and a stack inside the slot
//!
//! The label-map cache holds **one** entry, so a machine is only ever a hit for the module whose
//! map it last built. [`acquire`] therefore looks the callee up in a **module-affine slot
//! table** ([`Pool::index`], an [`FastHashMap`] from the callee image's address to its slot, so
//! the probe is one hash and no scan) and pops that slot's stack; only a slot with no idle
//! machine falls back to [`Machine::new`].
//!
//! * One slot per callee is what makes the probe `O(1)` and keeps a callee's idle machines
//!   reachable while the whole table is under [`MAX_POOLED_MACHINES`]: the measured compile
//!   graphs reach 44-54 distinct callees (a 100 KB stdlib compile) against the old table's 32
//!   *machines*, so the old shape could not hold one per callee at all. Past the total ceiling
//!   the least-recently-used **callee** gives up its idle machines, never a running one.
//! * The stack inside the slot is what makes **recursion** work: nested runs recurse (a nested
//!   run's callee makes its own cross-module calls), and a machine that is *running* is never in
//!   the pool — [`acquire`] has taken it out, and only [`release`] puts it back. So a recursive
//!   entry into the same module pops a second machine of that callee, and both find their way
//!   home afterwards. No machine is ever handed to two live runs.
//! * [`MAX_CALLEE_DEPTH`] bounds a slot's stack so a pathologically deep recursion costs a
//!   bounded amount of retained state (each machine is ~16 KB: the two register files).
//!
//! The affinity tag is a **hint, never an identity claim**: it is the callee image's address,
//! and a recycled address can only hand back a machine whose cache entry is keyed on a
//! different content witness — which misses and rebuilds. A stale hint therefore costs a build,
//! exactly as no pool would, and can never answer with another module's map
//! (`label_cache.rs`'s key is the digest + atom namespace + address, never a run).
//!
//! # What this shape does *not* buy
//!
//! A pool hit saves the machine, **not** the entry module's label map: the one-entry cache holds
//! whatever the previous run filled last — normally one of that run's in-machine callees — so the
//! run's own `Machine::label_map` at entry still misses whenever the machine was reused from a
//! run that entered a callee. Measured on `lib/stdlib/src/ordsets.erl` (release, this host): the
//! pool answered 14 932 of 15 007 acquires (99.5 %) from an entry, yet the bare-route label cache
//! still missed 4 733 times — and those are only **8 %** of its 59 277 fills. The other 92 % are
//! the `Arc` route, one whole-module build per distinct in-machine callee per run, which no pool
//! shape can reach: `Machine::reset` clears the context registry, so every run re-registers its
//! callees and the one-entry cache re-builds them. Removing those needs a multi-entry
//! [`LabelMaps`] (or a context registry that survives `reset`) inside `beam_engine`.
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
use std::collections::VecDeque;

use beam_engine::Machine;
use beam_loader::Module;

use crate::hasher::FastHashMap;

/// How many **callees** the pool keeps a slot for.
///
/// A compile's call graph reaches 44 distinct callee modules over `lib/stdlib/src/ordsets.erl`
/// and 54 over `lib/stdlib/src/string.erl` (measured by [`perf`]), so 96 slots leave room for a
/// graph twice that size before the least-recently-used callee's idle machines are dropped.
///
/// This is the capacity that used to be a *total machine count* (32), which was below the
/// measured distinct-callee set: the 32-entry table could not name a callee at all, so a
/// returning callee's machine had to be looked up with a linear scan and evicted in favour of
/// whoever released last. Keying the pool by callee makes the probe one hash and keeps a live
/// callee's machine reachable, and the retained state is bounded by [`MAX_POOLED_MACHINES`]
/// regardless (each machine is ~16 KB: its two 1 024-word register files).
const MAX_CALLEE_SLOTS: usize = 96;

/// How many idle machines one callee's slot keeps.
///
/// This is the **recursion depth** the pool serves without building: nested runs of the same
/// module are in flight at once during a round trip (`a` calls `b`, `b` calls `a`), and the
/// deepest measured chain is 13 over `lib/stdlib/src/ordsets.erl` and 18 over `maps.erl`. The
/// cap is a bound on one callee monopolizing the pool, not a working limit — and with
/// [`MAX_POOLED_MACHINES`] as small as it is, the total ceiling is what normally binds first.
const MAX_CALLEE_DEPTH: usize = 32;

/// How many machines the whole table keeps — the hard memory ceiling.
///
/// It is deliberately the same residency the 32-machine table this shape replaced retained: a
/// **bigger** pool was measured to cost more than it saves. At 128 the table kept 125 idle
/// machines over `lib/stdlib/src/string.erl` (the compile's own nesting really does reach that
/// many concurrent runs), which pinned 93 extra one-entry label caches for the whole run — and
/// since the cache holds one entry, a returning callee can never reuse them: the
/// `build_label_map` census was identical (1 934 330 vs 1 934 350 over the whole compile) while
/// user CPU time rose 2.7 % (19.29 s → 19.75 s, 5 of 5 paired rounds, quiet host). The pool's
/// value is the *probe*, not the capacity, so the ceiling stays where the old table's residency
/// was: ~512 KB of machines, and idle machines beyond it are given up by
/// least-recently-used callee.
const MAX_POOLED_MACHINES: usize = 32;

/// One callee's slot: its key and the idle machines that last ran it (newest last, so a pop is
/// the most recently released one — LIFO, which is what a recursive round trip re-enters).
///
/// The slot's key **is** every machine's affinity tag — a machine is only ever pushed onto the
/// slot of the callee it just ran, so the slot is the tag and no per-machine copy of it exists.
struct Slot {
    /// The callee image's address (the affinity hint — see the module docs for why a recycled
    /// address is safe).
    key: usize,
    /// The idle machines that last ran this callee, already [`Machine::reset`] with empty
    /// off-heap reservoirs.
    machines: VecDeque<Machine>,
    /// The pool clock at this slot's last acquire/release — eviction's recency key.
    last_use: u64,
}

/// The pool itself: a dense slot list (so a slot can be removed without touching the others'
/// machines) plus the address → slot index map that makes the lookup one hash.
#[derive(Default)]
struct Pool {
    /// The slots, in creation order (a removed slot is swap-removed from the back).
    slots: Vec<Slot>,
    /// `callee image address -> index into `slots`.
    index: FastHashMap<usize, usize>,
    /// A monotone tick, bumped on every acquire and release, ordering the slots by use.
    clock: u64,
    /// How many idle machines the whole table holds (maintained incrementally, so the ceiling
    /// check is `O(1)`).
    machines: usize,
}

thread_local! {
    /// This thread's pool.
    static POOL: RefCell<Pool> = RefCell::new(Pool::default());
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
/// module, or a fresh machine when the callee's slot is empty (or does not exist yet).
///
/// The machine is **out of the pool** until [`release`] puts it back, which is what makes a
/// recursive nested run unambiguous: no machine is ever handed to two live runs.
pub(super) fn acquire(module: &Module) -> Machine {
    let key = module_key(module);
    let recycled = POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        pool.clock += 1;
        let clock = pool.clock;
        let index = *pool.index.get(&key)?;
        let slot = &mut pool.slots[index];
        slot.last_use = clock;
        let machine = slot.machines.pop_back();
        if machine.is_some() {
            pool.machines -= 1;
        }
        machine
    });
    if perf::enabled() {
        perf::acquire(key, module_label(module), recycled.is_some());
    }
    recycled.unwrap_or_else(fresh_machine)
}

/// Recycle the machine a finished nested run of `module` used: reset it, then push it onto its
/// callee's stack (dropping the slot's oldest idle machine past [`MAX_CALLEE_DEPTH`], and the
/// least-recently-used **callee** past [`MAX_CALLEE_SLOTS`]).
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
        pool.clock += 1;
        let clock = pool.clock;
        // How many *idle* machines this release retires besides the one it brings back: the
        // slot's oldest one past the depth cap, or a whole evicted slot's worth.
        let mut retired = 0;
        let slot_depth = if let Some(&index) = pool.index.get(&key) {
            let slot = &mut pool.slots[index];
            slot.last_use = clock;
            if slot.machines.len() >= MAX_CALLEE_DEPTH {
                // This callee's oldest idle machine: the least likely depth to be
                // re-entered, and the one whose label map has been idle longest.
                slot.machines.pop_front();
                retired += 1;
            }
            slot.machines.push_back(machine);
            slot.machines.len()
        } else {
            if pool.slots.len() >= MAX_CALLEE_SLOTS {
                // Drop the least-recently-used callee's whole slot: its machines are idle, so
                // this costs those callees a fresh machine if they come back — never a running
                // one.
                let lru = pool
                    .slots
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, slot)| slot.last_use)
                    .map(|(index, _)| index)
                    .expect("a full slot table has a slot to evict");
                let evicted = pool.slots.swap_remove(lru);
                pool.index.remove(&evicted.key);
                retired += evicted.machines.len();
                // The slot that was last (and has just been moved into `lru`'s place) needs its
                // index corrected, unless the evicted slot *was* the last one.
                if lru < pool.slots.len() {
                    let moved = pool.slots[lru].key;
                    pool.index.insert(moved, lru);
                }
            }
            let index = pool.slots.len();
            pool.slots.push(Slot {
                key,
                machines: VecDeque::from([machine]),
                last_use: clock,
            });
            pool.index.insert(key, index);
            1
        };
        pool.machines = pool.machines + 1 - retired;
        if pool.machines > MAX_POOLED_MACHINES {
            // The whole table is past its ceiling: give up the oldest idle machine of the
            // least-recently-used callee that still has one — never the machine just released,
            // whose slot is by construction the most recently used.
            let lru = pool
                .slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| !slot.machines.is_empty())
                .min_by_key(|(_, slot)| slot.last_use)
                .map(|(index, _)| index);
            if let Some(index) = lru {
                pool.slots[index].machines.pop_front();
                pool.machines -= 1;
                retired += 1;
            }
        }
        if retired > 0 && perf::enabled() {
            perf::evictions(retired);
        }
        if perf::enabled() {
            perf::depth(pool.machines, slot_depth);
        }
    });
}

/// The callee image's address: the pool's affinity hint (never a cache key — see the module
/// docs).
fn module_key(module: &Module) -> usize {
    module as *const Module as usize
}

/// The callee's name, for the reuse counters only (never a pool decision: the affinity key is
/// the image address, and two loads of one `.beam` share a name but not a key).
fn module_label(module: &Module) -> &str {
    module.atom_table.name(module.name).unwrap_or("<unnamed>")
}

/// The pool's **reuse counters**: an env-gated diagnostic (`BEAM_RT_PERF_COUNTERS`, the one gate
/// [`calls::perf_counters`](super::calls::perf_counters) already owns) that says how well the
/// pool is doing — how many acquires it served from a pooled entry, how many had to build, how
/// many idle machines eviction dropped, and which callees account for them.
///
/// A diagnostic build aid, not a runtime feature: every entry point is gated by
/// [`enabled`](perf::enabled), so with the variable unset the only cost is one `OnceLock` load
/// per call. Nothing here changes a pool decision, and the summary is printed by the
/// thread-local's `Drop` — i.e. when the main thread unwinds out of `main`, the route every CLI
/// action takes (`init:stop()`, an `-eval` finishing, a `run` target returning). A process that
/// leaves through `erlang:halt/0,1` skips it.
mod perf {
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};

    /// Whether the counters are on — the dispatch path's own gate, not a second variable.
    pub(super) fn enabled() -> bool {
        super::super::calls::perf_counters::enabled()
    }

    /// One callee's share of the traffic, keyed by module name.
    #[derive(Default)]
    struct Callee {
        /// Acquires naming this callee.
        acquires: u64,
        /// Acquires a pooled entry answered.
        hits: u64,
        /// Acquires that had to build a fresh machine.
        fresh: u64,
    }

    /// Every counter this pool keeps.
    #[derive(Default)]
    pub(super) struct Counters {
        /// Nested runs that asked for a machine.
        acquires: u64,
        /// Acquires a pooled entry answered (the reuse rate's numerator).
        hits: u64,
        /// Acquires that had to build a fresh `Machine`.
        fresh: u64,
        /// Idle machines eviction dropped.
        evictions: u64,
        /// Machines held at the peak.
        peak: usize,
        /// The deepest one callee's stack got.
        peak_depth: usize,
        /// Distinct callee images the pool was asked about.
        keys: HashSet<usize>,
        /// Per-callee traffic, by module name.
        callees: HashMap<String, Callee>,
    }

    impl Drop for Counters {
        fn drop(&mut self) {
            if !enabled() {
                return;
            }
            let total = self.acquires.max(1);
            let mut body = String::new();
            let rows: [(&str, u64); 4] = [
                ("acquires", self.acquires),
                ("pool hits", self.hits),
                ("fresh builds", self.fresh),
                ("evictions", self.evictions),
            ];
            for (label, value) in rows {
                body.push_str(&format!(
                    "{label:24}{value:>12}  {:>6.2}%\n",
                    100.0 * value as f64 / total as f64
                ));
            }
            body.push_str(&format!(
                "{:24}{:>12}\n",
                "distinct callees",
                self.keys.len()
            ));
            body.push_str(&format!("{:24}{:>12}\n", "distinct names", self.callees.len()));
            body.push_str(&format!("{:24}{:>12}\n", "peak pool size", self.peak));
            body.push_str(&format!("{:24}{:>12}\n", "peak callee depth", self.peak_depth));
            // The callees, hottest first — the set the slot table has to hold.
            let mut callees: Vec<_> = self.callees.iter().collect();
            callees.sort_by(|a, b| b.1.acquires.cmp(&a.1.acquires));
            body.push_str("-- per callee (acquires/hits/fresh) --\n");
            for (name, stat) in callees {
                body.push_str(&format!(
                    "{name:24}{:>10}/{:>8}/{:>8}\n",
                    stat.acquires, stat.hits, stat.fresh
                ));
            }
            eprint!("=== beam_rt machine pool counters ===\n{body}");
        }
    }

    thread_local! {
        /// This thread's counters (the pool is thread-local too).
        static COUNTERS: RefCell<Counters> = RefCell::new(Counters::default());
    }

    /// Notes one acquire of `key` (a callee image) named `name`, and how it was served.
    pub(super) fn acquire(key: usize, name: &str, hit: bool) {
        if !enabled() {
            return;
        }
        COUNTERS.with(|counters| {
            let mut counters = counters.borrow_mut();
            counters.acquires += 1;
            counters.keys.insert(key);
            if hit {
                counters.hits += 1;
            } else {
                counters.fresh += 1;
            }
            let callee = counters.callees.entry(name.to_string()).or_default();
            callee.acquires += 1;
            if hit {
                callee.hits += 1;
            } else {
                callee.fresh += 1;
            }
        });
    }

    /// Notes that eviction dropped `count` idle machines.
    pub(super) fn evictions(count: usize) {
        if !enabled() {
            return;
        }
        COUNTERS.with(|counters| counters.borrow_mut().evictions += count as u64);
    }

    /// Notes the idle machines held after a release, and the depth of the slot just released to.
    pub(super) fn depth(total: usize, slot_depth: usize) {
        if !enabled() {
            return;
        }
        COUNTERS.with(|counters| {
            let mut counters = counters.borrow_mut();
            counters.peak = counters.peak.max(total);
            counters.peak_depth = counters.peak_depth.max(slot_depth);
        });
    }
}

/// The pool's own state, for the reuse pins in the sibling suites.
#[cfg(test)]
pub(super) fn pooled_len() -> usize {
    POOL.with(|pool| pool.borrow().machines)
}

/// How many callee slots the pool holds, for the shape pins.
#[cfg(test)]
pub(super) fn pooled_slots() -> usize {
    POOL.with(|pool| pool.borrow().slots.len())
}

/// How many idle machines one callee's slot holds (`0` when it has no slot).
#[cfg(test)]
pub(super) fn pooled_depth(key: usize) -> usize {
    POOL.with(|pool| {
        let pool = pool.borrow();
        pool.index
            .get(&key)
            .map(|&index| pool.slots[index].machines.len())
            .unwrap_or(0)
    })
}

/// The module keys the pool currently holds, one entry per retained machine (slot order, then
/// newest last within the slot), for the affinity pin.
#[cfg(test)]
pub(super) fn pooled_modules() -> Vec<usize> {
    POOL.with(|pool| {
        pool.borrow()
            .slots
            .iter()
            .flat_map(|slot| std::iter::repeat_n(slot.key, slot.machines.len()))
            .collect()
    })
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
            .slots
            .iter()
            .flat_map(|slot| slot.machines.iter())
            .filter(|machine| {
                !machine.refc_binaries().is_empty() || machine.writable_payload_count() != 0
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
    POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        pool.slots.clear();
        pool.index.clear();
        pool.clock = 0;
        pool.machines = 0;
    });
    FRESH_BUILDS.with(|count| count.set(0));
}
