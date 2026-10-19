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

//! The runtime half of **NIF process monitors** (contract A.3): the armed set and the death
//! delivery.
//!
//! # Ground truth: the resource holds the monitor, the emulator fires `down` from the death path
//!
//! `erts/emulator/beam/erl_nif.c`:
//!
//! * `enif_monitor_process` (`:3954-4011`) sends a monitor signal on behalf of a **resource**
//!   (`:3988-4005`) and inserts the monitor into the resource object's own monitor tree
//!   (`:3983-3986`); the internal reference it was registered under is what the caller stores and
//!   what `enif_demonitor_process` looks up (`:4046`).
//! * `erts_fire_nif_monitor` (`:3058-3115`) is the **death side**: the emulator calls it once per
//!   armed monitor when the target process dies, unlinks the monitor from the resource's tree
//!   (`:3091`), takes a protective reference on the resource (`:3092`) and calls the resource
//!   type's `down` callback with the **recorded target pid** (`:3105-3108`).
//! * `run_resource_dtor`/`dtor_demonitor` (`:2974-3030`, `:2940-2948`) — a dying resource cancels
//!   its monitors and runs **no** `down`, so a monitor whose resource was destroyed never fires.
//! * `enif_demonitor_process` (`:4019-4052`) cancels: `0` when it found and removed the monitor,
//!   `1` when it did not.
//!
//! # The Rust shape, and why the runtime needs an index at all
//!
//! `beam_abi` owns the resource side of that pair: [`NifMonitorIndex`] is keyed by the very
//! [`MonitorId`] (the C's 24-byte internal-reference block) the ABI's monitor table is keyed by,
//! and the ABI's [`deliver_resource_down_id`] does the delivery exactly once (it takes the binding
//! out under its own lock before it runs the callback).
//!
//! What the ABI cannot do is *observe a death*: the process table, and therefore the pid→death
//! observation, belongs to this runtime. So the runtime keeps the inverse index —
//! `monitor → target pid`, recorded when the host arms the monitor — and its
//! [`ProcessExitObserver`](beam_process::ProcessExitObserver) asks it, once per death, for the
//! monitors armed on that pid (see [`crate::runtime::ets_owner`]).
//!
//! The index is deliberately **no more granular than the ABI's own table**: a monitor is
//! identified by the same reference block `beam_abi` stores in the caller's `ErlNifMonitor`, so a
//! pair of monitors the ABI cannot tell apart is a pair this index cannot tell apart either — one
//! identity, one entry, and `enif_demonitor_process` behaves identically on both sides.
//!
//! # Exactly once, never dangling
//!
//! * **Exactly once**: [`NifMonitorIndex::deliver_for`] *removes* the entries before it delivers
//!   any of them, so a second death of the same pid (a reused pid number, a duplicated
//!   notification) delivers nothing; and the ABI's own take-out-before-callback makes a second
//!   delivery of one monitor a no-op even if it were attempted. A **demonitored** or
//!   **destroyed-resource** monitor is removed from this index too (demonitor) or delivers nothing
//!   because the ABI's table no longer holds it (destroy).
//! * **Never dangling**: the index stores only plain values — a [`MonitorId`] and a
//!   [`ProcessId`] — so nothing in it points into a dying heap, and a pid is an immediate
//!   (`beam_process::pid`). A monitor whose resource was destroyed while armed is pruned by
//!   [`NifMonitorIndex::prune_stale`] at the next arm, so a dead id cannot accumulate.
//! * **Ordering**: monitors armed on one dying pid are delivered in the order of their `MonitorId`
//!   bytes — deterministic, and not the arm order (the C delivers in monitor-tree order). Nothing
//!   observable depends on it: each delivery is one independent `down` call.
//!
//! # What is deliberately NOT here
//!
//! A **remote** pid never reaches this index: the host hook answers
//! [`NifMonitorResult::NotAlive`](beam_abi::nif::host::NifMonitorResult::NotAlive) for a pid that
//! is not a live local process (this runtime has no monitor arm across distribution), so no entry
//! is ever armed for a node this runtime cannot observe dying.

use std::collections::{HashMap, HashSet};

use beam_abi::nif::enif::resources::{deliver_resource_down_id, resource_monitors, MonitorId};
use beam_process::ProcessId;
use beam_term::Eterm;

/// The monitors a NIF resource has armed, by target process — the runtime's inverse of the ABI's
/// `monitor → resource` table.
///
/// Shared through an `Rc<RefCell<..>>` between the NIF host (which arms and demonitors) and the
/// process-exit observer (which delivers), exactly like
/// [`OwnerIndex`](crate::runtime::ets_owner::OwnerIndex).
#[derive(Default)]
pub(crate) struct NifMonitorIndex {
    /// Every armed monitor and the pid it watches. One entry per `enif_monitor_process` the ABI
    /// accepted (the ABI's own table is what decides whether a delivery has a `down` to run).
    armed: HashMap<MonitorId, ProcessId>,
}

impl NifMonitorIndex {
    /// An empty index.
    pub(crate) fn new() -> NifMonitorIndex {
        NifMonitorIndex::default()
    }

    /// Records that `id` watches the process `target`.
    ///
    /// The sweep runs **before** the insert, because `enif_monitor_process` calls the host hook
    /// *before* the ABI records the monitor ([`super::nif`] — the ABI's `record_monitor` runs
    /// after the hook returns), so the id being armed is not yet in [`resource_monitors`] and a
    /// sweep that ran afterwards would drop the entry it was just given.
    pub(crate) fn arm(&mut self, id: MonitorId, target: ProcessId) {
        self.prune_stale();
        self.armed.insert(id, target);
    }

    /// Forgets `id`, reporting whether it was armed here — the runtime half of
    /// `enif_demonitor_process` (`erl_nif.c:4019-4052`).
    ///
    /// A `false` is not a failure on its own: the ABI consults its own monitor table first
    /// (`take_monitor`) and answers the C's `0` for a monitor it removed even when this index had
    /// already pruned the entry.
    pub(crate) fn remove(&mut self, id: MonitorId) -> bool {
        self.armed.remove(&id).is_some()
    }

    /// Takes out every monitor armed on `target`, in [`MonitorId`] order (see the module docs on
    /// ordering).
    pub(crate) fn take_by_owner(&mut self, target: ProcessId) -> Vec<MonitorId> {
        let mut ids: Vec<MonitorId> = self
            .armed
            .iter()
            .filter(|(_, armed)| **armed == target)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        for id in &ids {
            self.armed.remove(id);
        }
        ids
    }

    /// Delivers the resource type's `down` callback for every monitor armed on `dead` — the
    /// runtime half of `erts_fire_nif_monitor` (`erl_nif.c:3058-3115`) — and reports how many
    /// callbacks ran.
    ///
    /// `pid_term` is the dead process's own pid term, which is what the C hands the callback as
    /// `ErlNifPid` (`omon->other.item`, `:3106`); a pid is an immediate, so the term stays valid
    /// after the dying heap is released.
    ///
    /// A monitor this index holds but the ABI does not (its resource was destroyed while armed)
    /// delivers nothing and counts nothing — the C's `run_resource_dtor` cancels silently
    /// (`:2974-3030`) — while still leaving this index, so no id lingers.
    pub(crate) fn deliver_for(&mut self, dead: ProcessId, pid_term: Eterm) -> usize {
        // The overwhelmingly common case: no NIF ever armed a monitor, so a death costs a length
        // test (this runs for every process death in the runtime).
        if self.is_empty() {
            return 0;
        }
        self.take_by_owner(dead)
            .into_iter()
            .filter(|id| deliver_resource_down_id(*id, pid_term))
            .count()
    }

    /// Drops every entry the ABI no longer tracks — a resource destroyed while its monitor was
    /// armed (`run_resource_dtor`, `:2974-3030`), or one this index missed the demonitor for.
    ///
    /// Without this, an id could sit here forever and be reported as armed long after its resource
    /// was gone.
    pub(crate) fn prune_stale(&mut self) {
        if self.is_empty() {
            return;
        }
        let live: HashSet<MonitorId> = resource_monitors().into_iter().map(|(id, _)| id).collect();
        self.retain_armed(&live);
    }

    /// The pure half of [`prune_stale`](Self::prune_stale): keep only the monitors in `live`.
    ///
    /// Split out so the pruning rule is testable without a live resource in the ABI's tables.
    fn retain_armed(&mut self, live: &HashSet<MonitorId>) {
        self.armed.retain(|id, _| live.contains(id));
    }

    /// How many monitors are armed (test/diagnostic).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.armed.len()
    }

    /// Whether no monitor is armed — the fast path [`deliver_for`](Self::deliver_for) takes.
    pub(crate) fn is_empty(&self) -> bool {
        self.armed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_process::RefAllocator;

    /// A `MonitorId` of the shape `enif_monitor_process` arms — a **real** internal reference, so
    /// it passes the ABI's own `ref_block_of` check.
    ///
    /// One id per test: the block a monitor is keyed by is the reference thing's header, node and
    /// id-space words, so two references this runtime mints compare equal here. That is the ABI's
    /// own granularity (see the module docs), and the index is deliberately no finer.
    fn monitor_id(heap: &mut Heap) -> MonitorId {
        let mut refs = RefAllocator::new();
        match refs.next().to_eterm(heap) {
            Ok(term) => MonitorId::from_term(term).expect("an internal reference"),
            Err(error) => panic!("a 64-word heap fits a reference thing: {error:?}"),
        }
    }

    /// The index's basic contract: a monitor is armed once, removed once, and a repeated removal
    /// answers the C's `enif_demonitor_process` miss.
    #[test]
    fn the_index_arms_and_removes_by_monitor_id() {
        let mut heap = Heap::new(64).expect("a test heap");
        let id = monitor_id(&mut heap);
        let target = ProcessId::new(1, 0);
        let mut index = NifMonitorIndex::new();
        assert!(index.is_empty());

        index.arm(id, target);
        assert_eq!(index.len(), 1);
        assert!(!index.is_empty());

        assert!(index.remove(id), "a monitor this index armed is removed");
        assert!(
            !index.remove(id),
            "a second demonitor finds nothing — the C's `1`"
        );
        assert!(index.is_empty());
    }

    /// `take_by_owner` filters by the armed pid: only the dead target's monitors come out, and a
    /// second death of the same pid takes nothing.
    #[test]
    fn take_by_owner_moves_only_the_dead_targets_monitors() {
        let mut heap = Heap::new(64).expect("a test heap");
        let id = monitor_id(&mut heap);
        let dead = ProcessId::new(7, 0);
        let alive = ProcessId::new(8, 0);
        let mut index = NifMonitorIndex::new();
        index.arm(id, dead);

        assert!(
            index.take_by_owner(alive).is_empty(),
            "a live pid's death takes nothing"
        );
        assert_eq!(index.len(), 1, "the monitor is still armed");
        assert_eq!(index.take_by_owner(dead), vec![id]);
        assert!(index.is_empty());
        assert!(
            index.take_by_owner(dead).is_empty(),
            "a second death of the same pid takes nothing"
        );

        // The same monitor armed on the *other* pid is taken by that pid instead — the filter is
        // the recorded target, not the id.
        index.arm(id, alive);
        assert!(index.take_by_owner(dead).is_empty());
        assert_eq!(index.take_by_owner(alive), vec![id]);
    }

    /// The empty index is the death path's fast exit: nothing is delivered, nothing is taken.
    #[test]
    fn deliver_for_an_empty_index_delivers_nothing() {
        let mut index = NifMonitorIndex::new();
        assert_eq!(index.deliver_for(ProcessId::new(1, 0), Eterm::nil()), 0);
        assert!(index.is_empty());
    }

    /// A monitor whose resource the ABI no longer tracks delivers nothing (the C's destroyed
    /// resource, `run_resource_dtor`) and still leaves the index — no dangling entry, no callback
    /// counted, and a repeat delivers nothing.
    #[test]
    fn deliver_for_an_untracked_monitor_delivers_nothing_and_removes_it() {
        let mut heap = Heap::new(64).expect("a test heap");
        let dead = ProcessId::new(3, 0);
        let id = monitor_id(&mut heap);
        let mut index = NifMonitorIndex::new();
        index.arm(id, dead);

        assert_eq!(
            index.deliver_for(dead, dead.to_eterm()),
            0,
            "no resource type is behind this id, so no `down` runs"
        );
        assert!(
            index.is_empty(),
            "the entry is gone, not left for a later delivery"
        );
        assert_eq!(
            index.deliver_for(dead, dead.to_eterm()),
            0,
            "exactly once: a repeated delivery finds nothing"
        );
    }

    /// Pruning keeps only the monitors the ABI still tracks — and the arm-time sweep really runs,
    /// so a monitor whose resource was destroyed while armed cannot linger.
    #[test]
    fn pruning_keeps_only_the_monitors_the_abi_still_tracks() {
        let mut heap = Heap::new(64).expect("a test heap");
        let tracked = monitor_id(&mut heap);
        let mut index = NifMonitorIndex::new();
        index.arm(tracked, ProcessId::new(1, 0));

        let nothing: HashSet<MonitorId> = HashSet::new();
        let mut stale = NifMonitorIndex::new();
        stale.armed.insert(tracked, ProcessId::new(1, 0));
        stale.retain_armed(&nothing);
        assert!(stale.is_empty(), "an untracked monitor is swept away");

        index.retain_armed(&HashSet::from([tracked]));
        assert_eq!(index.len(), 1, "a tracked monitor survives the sweep");

        // The arm-time sweep: the ABI tracks no monitor in this test, so arming sweeps the entry
        // that was there and then installs the new one.
        let mut swept = NifMonitorIndex::new();
        swept.armed.insert(tracked, ProcessId::new(2, 0));
        swept.arm(tracked, ProcessId::new(4, 0));
        assert_eq!(swept.len(), 1, "the sweep ran, then the arm inserted");
        assert_eq!(
            swept.take_by_owner(ProcessId::new(4, 0)),
            vec![tracked],
            "the freshly armed entry is the one that survived"
        );
    }
}
