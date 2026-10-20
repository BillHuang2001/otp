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

//! The **shared atom-index hub** — the one allocator of atom indices once a thread that is not
//! inside a NIF call can create atoms (contract **A.3**).
//!
//! # The problem it solves
//!
//! `enif_make_atom` can *create* an atom, and the created atom must land in the runtime's **one
//! live** [`AtomTable`] at a real live index: a private/crate-local index space would hand a NIF an
//! atom term carrying a **wrong** atom index — a wrong *term*, forbidden. A NIF-created thread
//! (`enif_thread_create`) is not inside a NIF call, so it cannot reach the per-call
//! [`PendingInterner`](crate::context::PendingInterner) that predicts an index as `base + offset`:
//! that prediction needs the table's length at the call's start and the "nothing else interns
//! meanwhile" guarantee a call path has, and a foreign thread has neither.
//!
//! The C emulator's answer is one global, locked atom table (`erts_atom_*`): *every* writer takes the
//! same lock, so indices are handed out in one serialized order. [`ForeignAtomHub`] is the same idea
//! for the one piece this runtime cannot serialize — it is a `Mutex`-guarded **mirror of the live
//! table's index space plus a reservation tail**, and once it is installed (it is attached to the
//! [`CodeServer`](crate::codeserver::CodeServer), which is the only place the live table grows) it is
//! the single allocator of atom indices for every writer:
//!
//! * a foreign thread **reserves** an index under the lock ([`ForeignAtomHub::reserve`]) — cheap, and
//!   immediately correct *as an index*: a reservation is `names.len()`, which is the live table's own
//!   next index;
//! * the runtime **commits** reservations when it next touches the table ([`ForeignAtomHub::sync`],
//!   called at the entry of `CodeServer::atoms_mut`/`intern_pending` — *before* the runtime's own
//!   append, so the two can never claim the same index);
//! * a per-call [`PendingInterner`](crate::context::PendingInterner) that has adopted the hub
//!   (`PendingInterner::adopt_hub`) reserves through it too, so the BIF/NIF call path and the foreign
//!   thread draw from **one** index space instead of two lists that could hand out the same index.
//!
//! # The model
//!
//! `HubState::names` is a **dense, positional prefix of the live table**: `names[i]` is the name of
//! atom index `i`. `names.len()` is therefore the live table's next free index, which is what a
//! reservation returns. `committed` splits the list:
//!
//! * `names[..committed]` — names the live table **already holds**, at exactly those indices;
//! * `names[committed..]` — names reserved since the last `sync`, i.e. an index tail the live table
//!   does not have yet.
//!
//! Invariants (asserted by `debug_assert!`s and pinned by this module's tests):
//!
//! 1. `committed <= names.len()`.
//! 2. `names.len() - committed` is the number of reserved-but-uncommitted names, and the `pending`
//!    counter maintained under the lock equals it — so `has_pending()` is exactly "a reservation is
//!    outstanding" and costs one relaxed atomic load (no mutex).
//! 3. `by_name` maps **exactly** the names in `names` to their indices.
//! 4. After a `sync`, `committed == names.len() == atoms.len()`, and every name in `names` is in the
//!    live table at its own index.
//!
//! # Drift, and the honest residual
//!
//! The live table also grows from the runtime's own side (`CodeServer::atoms_mut`), and a runtime-side
//! append between two `sync`s takes the very index a standing reservation points at. [`sync`] therefore
//! **re-anchors**: it re-reserves each outstanding name, so its index moves, and bumps the divergence
//! counter ([`ForeignAtomHub::divergence_count`]). Nothing observable can be wrong from that
//! re-anchor: the only consumer of a reservation is a message term that the foreign thread converts to
//! an **ETF image by atom name** before anything leaves the thread (that conversion is the NIF
//! thread-env host's job — see `beam_abi/src/nif/thread_env.rs`), and an in-worker `atom_name` goes
//! through this hub, which the same re-anchor updates consistently. The residual is therefore *"an
//! index a foreign thread observed before a re-anchor may afterwards name a different atom"* — never a
//! term that leaves the thread, and never a name missing from the live table.
//!
//! The runtime keeps that window as small as it can be made: the `CodeServer` drains the hub
//! *unconditionally* at the entry of every mutating atom path, so a runtime-side append is always
//! preceded by a `sync`; the re-anchor is the safety net for an append made while a caller holds a raw
//! `&mut AtomTable` across a foreign reservation (a long interning loop inside one borrow), and for any
//! future path that mutates the table without going through the code server.
//!
//! # Locking
//!
//! One `std::sync::Mutex` per hub (the C's one global atom lock). `unsafe` is not involved anywhere:
//! `beam_rt` is `#![deny(unsafe_code)]`. A **poisoned** lock — a panic while a holder had it — is
//! recovered (`into_inner`) rather than propagated, because this hub holds no state a panic could
//! leave half-written: every mutation is a push plus a counter bump, and the mirror is re-validated
//! against the live table on every `sync`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use beam_term::{Atom, AtomTable};

/// The mirror of the live atom table's index space plus its reservation tail.
///
/// Allocated once per runtime (`crate::Runtime::install_foreign_atom_hub`) and shared as an [`Arc`]:
/// the runtime's own atom paths hold it through the [`CodeServer`](crate::codeserver::CodeServer) and
/// a foreign thread holds it through the NIF thread-env host factory. `Send + Sync` because that is
/// the whole point — see the module docs.
pub(crate) struct ForeignAtomHub {
    /// The mirror and its reservation tail, behind the hub's one lock.
    state: Mutex<HubState>,
    /// The number of reserved-but-uncommitted names — the lock-free `has_pending` signal.
    ///
    /// Maintained **under the lock** (so it can never disagree with `names.len() - committed` for
    /// longer than one lock hold) and read without it: the runtime's per-call fast path must not take
    /// a mutex to learn that there is nothing to do.
    pending: AtomicUsize,
    /// `HubState::committed`, mirrored here so [`ForeignAtomHub::needs_sync`] can decide "the live
    /// table moved and the hub must absorb it" without taking the lock.
    committed: AtomicUsize,
    /// How many times [`ForeignAtomHub::sync`] had to re-anchor standing reservations because the
    /// live table grew underneath them.
    ///
    /// Never observable behaviour (nothing prints it, nothing branches on it): it is the pin for the
    /// claim that the *normal* path — reserve, then sync with no interrupting runtime append — never
    /// diverges, and the alarm for a change that widened the window.
    divergence: AtomicUsize,
}

/// The mutable half of a [`ForeignAtomHub`] — everything under the hub's one lock.
struct HubState {
    /// Index → name for every index the hub knows, in index order (a dense prefix of the live
    /// table). `names.len()` is the index the next reservation gets.
    names: Vec<String>,
    /// Name → index, covering exactly `names`.
    by_name: HashMap<String, u32>,
    /// How many of `names` the live table **already holds** (a prefix).
    committed: usize,
    /// A cached dense `AtomTable` whose `0..names.len()` names are exactly `names`, together with the
    /// `names.len()` stamp it was built at (see [`ForeignAtomHub::scratch_table`]).
    scratch: Option<(usize, Arc<AtomTable>)>,
}

impl ForeignAtomHub {
    /// Build a hub mirroring `atoms`, the live table, as of now.
    ///
    /// The mirror is a dense positional prefix of `atoms` (`names[i]` names `Atom(i)`), and
    /// `committed == atoms.len()`: nothing is reserved yet.
    pub(crate) fn new(atoms: &AtomTable) -> Arc<ForeignAtomHub> {
        let mut st = HubState {
            names: Vec::new(),
            by_name: HashMap::new(),
            committed: 0,
            scratch: None,
        };
        Self::mirror_locked(&mut st, atoms);
        let committed = st.committed;
        let hub = ForeignAtomHub {
            state: Mutex::new(st),
            pending: AtomicUsize::new(0),
            committed: AtomicUsize::new(committed),
            divergence: AtomicUsize::new(0),
        };
        debug_assert_eq!(
            hub.table_len(),
            atoms.len(),
            "a fresh hub mirrors the whole live table"
        );
        Arc::new(hub)
    }

    /// Reserve `name`'s index: an existing (or already reserved) name keeps its index, a new one takes
    /// `names.len()` — the live table's own next index whenever the hub is in sync.
    ///
    /// This is the allocator every writer uses once the hub is installed; it takes the hub's one lock
    /// (the C's one global atom lock) and never touches the live table.
    pub(crate) fn reserve(&self, name: &str) -> u32 {
        let mut st = self.lock();
        let next = st.names.len();
        let index = Self::push_locked(&mut st, name);
        if index as usize == next {
            // A genuinely new name: one more than `committed` outstanding reservation. Bumped under
            // the lock, so `has_pending()` can never answer "nothing to commit" while an
            // uncommitted name exists.
            self.pending.fetch_add(1, Ordering::Relaxed);
        }
        index
    }

    /// The index of an **already known** name — never creates one.
    ///
    /// `None` is the honest answer for a name neither the live table (as of the last `sync`) nor a
    /// standing reservation holds; this method never fabricates an index.
    ///
    /// The lookup half of the seam the `enif_*` host on a foreign thread answers
    /// (`enif_get_atom`/`enif_make_existing_atom`) with; it has no runtime-side caller yet, so it is
    /// `#[cfg(test)]`-consumed today and kept part of the hub's surface deliberately.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn lookup(&self, name: &str) -> Option<u32> {
        let st = self.lock();
        st.by_name.get(name).copied()
    }

    /// The name at `index`, or `None` when the hub's mirror does not reach that index.
    ///
    /// Covers the committed prefix **and** the reservation tail — a name a foreign thread reserved a
    /// moment ago is already nameable at the index it will keep. This is what lets such a thread answer
    /// `enif_get_atom` for an atom it created itself without touching (or locking) the live table, and
    /// what keeps the index it hands out truthful.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn name_of(&self, index: u32) -> Option<String> {
        let st = self.lock();
        st.names.get(index as usize).cloned()
    }

    /// The hub's known index space: `table_len()` names, indices `0..table_len()`.
    pub(crate) fn table_len(&self) -> usize {
        self.lock().names.len()
    }

    /// Whether a reservation is outstanding — one relaxed atomic load, **no** lock.
    ///
    /// This is the runtime per-call fast path's gate: with no foreign activity at all, the answer is
    /// `false` and a registered BIF/NIF call pays nothing beyond the load.
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Relaxed) > 0
    }

    /// Whether [`ForeignAtomHub::sync`] has anything to do against a live table of length `live_len`:
    /// an outstanding reservation to commit, or a runtime-side append to absorb.
    ///
    /// Two relaxed loads, no lock — which is what lets the runtime's atom paths call it
    /// unconditionally.
    pub(crate) fn needs_sync(&self, live_len: usize) -> bool {
        self.has_pending() || self.committed.load(Ordering::Relaxed) != live_len
    }

    /// **The one place a reservation becomes real**: append every uncommitted name to `atoms`.
    ///
    /// 1. **Reconcile.** If the live table grew (or, impossibly for an append-only table, shrank) since
    ///    the last sync, its new names took the indices standing reservations were pointing at.
    ///    Reservations are therefore dropped out of `by_name`, the runtime's names are absorbed in
    ///    index order, and each reserved name is re-reserved — so the mirror is a true dense prefix
    ///    again, at the cost of the reserved indices moving (see the module docs' residual).
    /// 2. **Commit.** Every name in `committed..names.len()` is interned into `atoms`; in the
    ///    non-drift case it lands at exactly its predicted index (`debug_assert!`ed).
    /// 3. `committed` becomes `names.len()` and the pending counter is cleared.
    pub(crate) fn sync(&self, atoms: &mut AtomTable) {
        let mut st = self.lock();
        if st.committed != atoms.len() {
            if st.committed < st.names.len() {
                // The runtime outran a standing reservation: this one really moves.
                self.divergence.fetch_add(1, Ordering::Relaxed);
            }
            let committed = st.committed.min(st.names.len());
            // The reserved names, in reservation order, out of the mirror and out of the index.
            let reserved: Vec<String> = st.names.split_off(committed);
            for name in &reserved {
                st.by_name.remove(name);
            }
            // Absorb the runtime's own appends, in index order (dense and positional).
            for index in committed..atoms.len() {
                let Some(name) = atoms.name(Atom::from_raw(index as u32)) else {
                    debug_assert!(false, "an index below the atom table's length names an atom");
                    break;
                };
                Self::push_locked(&mut st, name);
            }
            st.committed = st.names.len();
            // Re-reserve each outstanding name: a name the runtime already interned resolves to the
            // runtime's own index (which is the point of the re-anchor), a genuinely new one takes
            // the tail again.
            for name in reserved {
                Self::push_locked(&mut st, &name);
            }
            // The indices below the tail may have been *replaced* without the length moving, which
            // the length stamp cannot see: drop the cached encoder image.
            st.scratch = None;
        }
        debug_assert!(st.committed <= st.names.len());
        // Commit the tail. `atoms.intern` gives an existing name its existing index, so a stale
        // reservation can never append a duplicate.
        let mut exact = true;
        for index in st.committed..st.names.len() {
            let name = st.names[index].clone();
            let landed = atoms.intern(&name).raw() as usize;
            debug_assert_eq!(
                landed, index,
                "a committed reservation must land at the index it was reserved at"
            );
            if landed != index {
                exact = false;
            }
        }
        st.committed = st.names.len();
        self.committed.store(st.committed, Ordering::Relaxed);
        self.pending.store(0, Ordering::Relaxed);
        if !exact {
            // Unreachable for a well-formed append-only table. If it ever happens, the mirror is no
            // longer positional, so rebuild it from the live table instead of handing out indices
            // that would name the wrong atoms.
            self.divergence.fetch_add(1, Ordering::Relaxed);
            Self::mirror_locked(&mut st, atoms);
            self.committed.store(st.committed, Ordering::Relaxed);
        }
    }

    /// A dense [`AtomTable`] whose `0..names.len()` names are exactly this hub's — the table a
    /// foreign thread needs to **encode** a term as an ETF image (an encoder resolves every atom's
    /// name through it).
    ///
    /// Rebuilt lazily and cached under the hub's lock, invalidated by the `names.len()` stamp (and
    /// explicitly by a re-anchor, whose replacements the stamp cannot see): names are only ever
    /// appended, so a moved length is exactly "the image is stale". Nothing is built in
    /// [`ForeignAtomHub::new`] — a runtime with no foreign thread pays nothing for a table that would
    /// duplicate the live one. This is the encoder image a foreign thread's `term_to_binary`
    /// equivalent builds terms against; no runtime-side caller needs it yet, so it is
    /// `#[cfg(test)]`-consumed today.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn scratch_table(&self) -> Arc<AtomTable> {
        let mut st = self.lock();
        if let Some((stamp, table)) = &st.scratch {
            if *stamp == st.names.len() {
                return Arc::clone(table);
            }
        }
        let mut table = AtomTable::new();
        table.seed(st.names.iter().map(String::as_str));
        let table = Arc::new(table);
        st.scratch = Some((st.names.len(), Arc::clone(&table)));
        table
    }

    /// How many times a `sync` had to re-anchor standing reservations (see the module docs).
    ///
    /// Test-only: bookkeeping for the claim that the normal reserve→sync path never diverges, not
    /// observable behaviour.
    #[cfg(test)]
    pub(crate) fn divergence_count(&self) -> usize {
        self.divergence.load(Ordering::Relaxed)
    }

    /// Reset the mirror to exactly `atoms`, discarding any reservation tail — the recovery shape
    /// ([`ForeignAtomHub::new`]'s build and the unreachable "the mirror is not positional" repair).
    ///
    /// The caller holds the lock (every call site is inside a `MutexGuard`).
    fn mirror_locked(st: &mut HubState, atoms: &AtomTable) {
        st.names = Vec::with_capacity(atoms.len());
        st.by_name = HashMap::with_capacity(atoms.len());
        st.scratch = None;
        for index in 0..atoms.len() {
            // `AtomTable::name` answers `Some` for every index below the table's length (its name
            // vector is exactly `len` entries long), so the mirror is dense by construction. A hole
            // would break the positional invariant, so it ends the scan and leaves the rest to the
            // next `sync`'s reconciliation rather than being papered over.
            let Some(name) = atoms.name(Atom::from_raw(index as u32)) else {
                debug_assert!(false, "an index below the atom table's length names an atom");
                break;
            };
            Self::push_locked(st, name);
        }
        st.committed = st.names.len();
    }

    /// Push `name` into the mirror if it is not there, answering its index — the one append.
    ///
    /// The caller holds the lock (every call site is inside a `MutexGuard`).
    fn push_locked(st: &mut HubState, name: &str) -> u32 {
        if let Some(index) = st.by_name.get(name) {
            return *index;
        }
        let index = st.names.len() as u32;
        let owned = name.to_string();
        st.names.push(owned.clone());
        st.by_name.insert(owned, index);
        index
    }

    /// The hub's one lock, with a **poisoned** lock recovered rather than propagated.
    ///
    /// The state behind it is a plain mirror plus counters and every method re-validates it against
    /// the live table, so a panic under the lock cannot leave a shape a recovery would misread.
    fn lock(&self) -> MutexGuard<'_, HubState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// `ForeignAtomHub` **must** be shareable across the threads a NIF creates (that is its purpose), so
/// its `Send + Sync`-ness is a compile-time requirement, not an implementation detail.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ForeignAtomHub>();
    assert_send_sync::<Arc<ForeignAtomHub>>();
};

#[cfg(test)]
#[path = "atoms_tests.rs"]
mod tests;
