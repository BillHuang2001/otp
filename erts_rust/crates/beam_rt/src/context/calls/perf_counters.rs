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

//! Env-gated counters for the registered-call dispatch path (`BEAM_RT_PERF_COUNTERS=1`).
//!
//! A diagnostic build aid, not a runtime feature: every counter sits behind
//! [`enabled`], which reads a `OnceLock` once per call site, and nothing here
//! *changes* a dispatch decision — the counters observe the same prologue the
//! uninstrumented build runs. With the variable unset the only cost is one
//! `Relaxed` load of a `OnceLock` per call site.
//!
//! The summary is printed by the thread-local's `Drop`, i.e. when the main
//! thread unwinds out of `main` — which is the route every CLI action takes
//! (`init:stop()`, an `-eval` finishing, a `run` target returning). A process
//! that leaves through `erlang:halt/0,1` skips it.

use core::cell::RefCell;
use std::sync::OnceLock;

use std::collections::HashMap;

/// Whether the counters are on (`BEAM_RT_PERF_COUNTERS` set to anything non-empty).
pub(crate) fn enabled() -> bool {
    // A unit test cannot turn the shipping gate on for itself: the gate is a process-wide
    // `OnceLock`, already `false` by the time any test runs (every dispatch site reads it). The
    // thread-local override is test-only and read first, so a test can observe **this thread's**
    // counters without touching any other test's.
    #[cfg(test)]
    if test_override::forced() {
        return true;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("BEAM_RT_PERF_COUNTERS").is_some_and(|value| !value.is_empty())
    })
}

/// Test-only override of [`enabled`], scoped to the calling thread.
#[cfg(test)]
pub(crate) mod test_override {
    use core::cell::Cell;
    thread_local! {
        /// Whether this thread's counters were forced on by a test.
        static FORCED: Cell<bool> = Cell::new(false);
    }
    /// Whether this thread forced the counters on.
    pub(crate) fn forced() -> bool {
        FORCED.with(Cell::get)
    }
    /// Force this thread's counters on (`true`) or back off (`false`).
    pub(crate) fn force(on: bool) {
        FORCED.with(|cell| cell.set(on));
    }
}

/// Test-only: this thread's `(call_bif, nif_probe)` counts, read under
/// [`test_override::force`]-`n` so a case can assert a **delta** across one call.
///
/// Nothing in the shipping build can read the counters (they exist to be printed by the
/// thread-local's `Drop`), so this is the one way to pin their accounting from a unit test.
#[cfg(test)]
pub(crate) fn probe_counts() -> (u64, u64) {
    COUNTERS.with(|counters| {
        let counters = counters.borrow();
        (counters.call_bif, counters.nif_probe)
    })
}

/// What the counters observed, and where the time goes.
#[derive(Default)]
pub(crate) struct Counters {
    /// `RuntimeContext` dispatch-memo hits.
    pub(crate) dispatch_hit: u64,
    /// `RuntimeContext` dispatch-memo misses (a full prologue: registry lookup + rosters).
    pub(crate) dispatch_miss: u64,
    /// Misses whose slot was still empty (a context's first use of that slot).
    pub(crate) dispatch_cold: u64,
    /// Misses whose slot held a **different** key (a direct-mapped collision).
    pub(crate) dispatch_evict: u64,
    /// `call_bif` entries.
    pub(crate) call_bif: u64,
    /// `call_bif` entries where the NIF probe actually ran (a non-empty NIF registry).
    pub(crate) nif_probe: u64,
    /// NIF probe hits.
    pub(crate) nif_hit: u64,
    /// Calls the runtime-native floor served.
    pub(crate) native_hit: u64,
    /// Calls the `beam_bifs` registry served (`call_registered`).
    pub(crate) registered: u64,
    /// Calls that fell through to an external module load (`run_export`).
    pub(crate) external: u64,
    /// `call_registered`'s arms.
    pub(crate) arm_fun_caller: u64,
    /// The process-dictionary arm.
    pub(crate) arm_dict: u64,
    /// The process-table arm.
    pub(crate) arm_process_hook: u64,
    /// The NIF-loading arm.
    pub(crate) arm_nif_loader: u64,
    /// The record-source arm.
    pub(crate) arm_record_source: u64,
    /// The `[safe]` export-gate arm.
    pub(crate) arm_export_gate: u64,
    /// The message-delivery arm.
    pub(crate) arm_send_hook: u64,
    /// The module-info arm.
    pub(crate) arm_module_info: u64,
    /// The persistent-term arm.
    pub(crate) arm_persistent_term: u64,
    /// The runtime-hooks arm.
    pub(crate) arm_runtime_hooks: u64,
    /// Calls that installed the shared `TableRegistry` cell.
    pub(crate) tables_held: u64,
    /// Calls that installed a lent `TableRegistry`.
    pub(crate) tables_lent: u64,
    /// Calls that took the pooled BIF heap at a non-zero mark (pool still owned).
    pub(crate) pool_dirty: u64,
    /// Calls whose `take_bif_frontier` retired a region.
    pub(crate) pool_grown: u64,
    /// Calls that interned at least one name (the `intern_pending` commit).
    pub(crate) intern_commit: u64,
    /// Results that were foreign containers (`result_is_foreign_container`).
    pub(crate) foreign_result: u64,
    /// Results moved out of the pool (`rehome_container_result`).
    pub(crate) rehomed: u64,
    /// Results that held a reference (the pool was not rewound).
    pub(crate) ref_held: u64,
    /// Calls whose pooled heap was rewound.
    pub(crate) rewound: u64,
    /// `graph_holds_reference` invocations and the objects they visited.
    pub(crate) walk_calls: u64,
    /// Objects the reference walk visited.
    pub(crate) walk_visits: u64,
    /// Registered calls whose `reentrancy_capable` answered `true`.
    pub(crate) reentrant: u64,

    /// The `-eval`/boot `RunOutcome` calls that asked `host_serves`.
    pub(crate) per_key: HashMap<(String, String, u32), u64>,
}

impl Drop for Counters {
    fn drop(&mut self) {
        if !enabled() {
            return;
        }
        let total = self.call_bif.max(1);
        let mut body = String::new();
        let rows: [(&str, u64); 33] = [
            ("dispatch memo cold", self.dispatch_cold),
            ("dispatch memo collision", self.dispatch_evict),
            ("call_bif", self.call_bif),
            ("  nif probe", self.nif_probe),
            ("  nif hit", self.nif_hit),
            ("  native floor hit", self.native_hit),
            ("  registered (call_registered)", self.registered),
            ("  external module load", self.external),
            ("dispatch memo hit", self.dispatch_hit),
            ("dispatch memo miss", self.dispatch_miss),
            ("arm fun_caller", self.arm_fun_caller),
            ("arm dict", self.arm_dict),
            ("arm process_hook", self.arm_process_hook),
            ("arm nif_loader", self.arm_nif_loader),
            ("arm record_source", self.arm_record_source),
            ("arm export_gate", self.arm_export_gate),
            ("arm send_hook", self.arm_send_hook),
            ("arm module_info", self.arm_module_info),
            ("arm persistent_term", self.arm_persistent_term),
            ("arm runtime_hooks", self.arm_runtime_hooks),
            ("reentrant call", self.reentrant),
            ("tables cell held", self.tables_held),
            ("tables lent", self.tables_lent),
            ("pool at non-zero mark", self.pool_dirty),
            ("pool region retired", self.pool_grown),
            ("intern commit", self.intern_commit),
            ("foreign container result", self.foreign_result),
            ("rehomed", self.rehomed),
            ("reference held", self.ref_held),
            ("rewound", self.rewound),
            ("reference walk calls", self.walk_calls),
            ("reference walk visits", self.walk_visits),
            ("per-key pairs", self.per_key.len() as u64),
        ];
        for (label, value) in rows {
            if label == "call_bif" {
                body.push_str(&format!("{label:34}{value:>12}\n"));
            } else {
                body.push_str(&format!(
                    "{label:34}{value:>12}  {:>6.2}%\n",
                    100.0 * value as f64 / total as f64
                ));
            }
        }
        // The twenty hottest keys, by call count — the shape a cache must hold.
        let mut keys: Vec<_> = self.per_key.iter().collect();
        keys.sort_by(|a, b| b.1.cmp(a.1));
        body.push_str("-- hottest keys --\n");
        for ((module, function, arity), count) in keys.into_iter().take(24) {
            body.push_str(&format!("{module}:{function}/{arity:<4}{count:>12}\n"));
        }
        eprint!("=== beam_rt perf counters ===\n{body}");
    }
}

thread_local! {
    /// This thread's counters.
    static COUNTERS: RefCell<Counters> = RefCell::new(Counters::default());
}

/// Bumps one counter, when the counters are on.
pub(crate) fn bump(pick: impl FnOnce(&mut Counters) -> &mut u64) {
    if !enabled() {
        return;
    }
    COUNTERS.with(|counters| {
        let mut counters = counters.borrow_mut();
        *pick(&mut counters) += 1;
    });
}

/// Adds a whole walk's visit count at once, when the counters are on.
///
/// [`RuntimeContext::graph_holds_reference`] counts its visits in a local `u64` and calls this
/// **once per walk**: a `bump` per visited object would put an `enabled()` load and a
/// thread-local `borrow_mut` in the innermost loop, which is a real share of the loop body on the
/// 10^8 objects a large in-process compile visits.
///
/// [`RuntimeContext::graph_holds_reference`]: super::RuntimeContext::graph_holds_reference
pub(crate) fn add_visits(visits: u64) {
    if !enabled() || visits == 0 {
        return;
    }
    COUNTERS.with(|counters| counters.borrow_mut().walk_visits += visits);
}

/// Records one call for `(module, function, arity)`, when the counters are on.
pub(crate) fn note_key(module: &str, function: &str, arity: u32) {
    if !enabled() {
        return;
    }
    COUNTERS.with(|counters| {
        let mut counters = counters.borrow_mut();
        *counters
            .per_key
            .entry((module.to_string(), function.to_string(), arity))
            .or_insert(0) += 1;
    });
}
