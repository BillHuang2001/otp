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

//! The **private atom space** of the NIF-created threads — the `Mutex`-guarded name→index space
//! the process-independent NIF host ([`super::thread_env::ForeignHost`]) builds its terms against
//! (contract **A.3**).
//!
//! # Why the indices are private, and why that is sound
//!
//! `enif_make_atom` can *create* an atom, and a NIF-created thread (`enif_thread_create`) is not
//! inside a NIF call: it cannot reach the per-call
//! [`PendingInterner`](crate::context::PendingInterner) that predicts an index as `base + offset`,
//! because that prediction needs the live table's length at the call's start and the "nothing else
//! interns meanwhile" guarantee a call path has, and a foreign thread has neither.
//!
//! The atom's identity, however, never has to be a **live** index, because nothing built on such a
//! thread reaches the runtime as a term. The one crossing is
//! [`NifHost::send`](beam_abi::NifHost::send), and it converts the message to a complete **External
//! Term Format image by atom name** ([`ForeignHost::encode_image`](super::thread_env::ForeignHost)),
//! which the runtime decodes against its own live table — interning the names itself. A foreign
//! index is therefore only ever meaningful *inside* the foreign space, and the space can be private.
//!
//! Reserving indices in the live table's own index space (the previous design) was unsound: the
//! reservation was a claim on an index the runtime's own appenders could reach first, and the
//! reconciliation that followed re-anchored an index a term already carried — so a live table's
//! `Atom` could end up naming a *different* atom than the one it was built for. That is a wrong
//! atom, not an acceptable residual.
//!
//! # The model
//!
//! One hub per runtime, shared as an `Arc` by every host that runtime's factory opens:
//!
//! * `names[i]` is the name of private index `i`; `by_name` is its inverse. Both grow **only** by
//!   appending ([`ForeignAtomHub::reserve`]), so an index handed out once keeps its name for the
//!   life of the hub — indices are never renumbered and never move.
//! * [`ForeignAtomHub::scratch_table`] is a cached dense [`AtomTable`] seeded with `names` in order,
//!   i.e. a table in which private index `i` names the same atom as the hub does. It is what
//!   `beam_codec`'s [`Encoder`](beam_codec::Encoder) reads, so an encoded image spells every atom by
//!   its own name — the handoff is by name, and encoding stays byte-correct without the private
//!   indices ever leaving the space.
//! * Nothing here touches the runtime's live table: the hub holds no reference to it and the code
//!   server knows nothing about the hub. A name a foreign thread creates enters the live table when
//!   a decoded image carries it, and only then.
//!
//! # What cannot cross back, and that is a boundary rather than a gap
//!
//! A term built on a foreign host is **not** a term of this runtime: its atoms carry private
//! indices, so `beam_term`'s index equality would compare them against live atoms wrongly. Such a
//! term is therefore valid only on the host that built it — it may be read back there
//! (`enif_get_atom`, `enif_compare` within that same space) and sent from that host (which encodes
//! it by name), and must never be handed to the runtime's own term machinery. The one shape that
//! could smuggle one in — `enif_send` from a *call-path* env carrying a term built on a foreign
//! host — is refused by construction: the call host encodes against the live table, the private
//! index names no live atom there, and the encode is the typed refusal rather than a wrong atom.
//!
//! # Locking
//!
//! One `std::sync::Mutex` per hub (the C's one global atom lock), which is what makes the space
//! shareable by the several threads a NIF may create. `unsafe` is not involved anywhere: `beam_rt`
//! is `#![deny(unsafe_code)]`. A **poisoned** lock — a panic while a holder had it — is recovered
//! (`into_inner`) rather than propagated: the state is an append-only name vector plus its inverse
//! map and a cache, and every mutation is a push followed by an insert, so a recovery cannot read a
//! half-written shape.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use beam_term::AtomTable;

/// A **private** atom index space: name → index for everything the NIF-created threads of one
/// runtime have created.
///
/// Allocated once per runtime (`crate::Runtime::install_nif_thread_env`) and shared as an [`Arc`]:
/// every host the installed factory opens holds the same space, so the threads of one runtime agree
/// on an atom's private index. `Send + Sync` because that is the whole point — see the module docs.
pub(crate) struct ForeignAtomHub {
    /// The space, behind its one lock.
    state: Mutex<HubState>,
}

/// The mutable half of a [`ForeignAtomHub`] — everything under the hub's one lock.
struct HubState {
    /// Index → name, in index order. `names.len()` is the index the next new name gets. Append-only:
    /// an index handed out by [`ForeignAtomHub::reserve`] keeps its name forever.
    names: Vec<String>,
    /// Name → index, covering exactly `names`.
    by_name: HashMap<String, u32>,
    /// A cached dense [`AtomTable`] whose `0..names.len()` names are exactly `names`, together with
    /// the `names.len()` stamp it was built at (see [`ForeignAtomHub::scratch_table`]).
    scratch: Option<(usize, Arc<AtomTable>)>,
}

impl ForeignAtomHub {
    /// Build an **empty** private space.
    ///
    /// Empty on purpose: nothing of the live table is copied, mirrored or referenced, so there is no
    /// state here that could drift out of step with it. The first index handed out is `0`, and it is
    /// a private index — not the live table's.
    pub(crate) fn new() -> Arc<ForeignAtomHub> {
        Arc::new(ForeignAtomHub {
            state: Mutex::new(HubState {
                names: Vec::new(),
                by_name: HashMap::new(),
                scratch: None,
            }),
        })
    }

    /// `name`'s index in this private space, creating it (at the next free index) on demand.
    ///
    /// Idempotent: a known name keeps the index it was given, so a NIF that interns the same atom
    /// twice builds the same term twice. Takes the hub's one lock and never touches the live table.
    pub(crate) fn reserve(&self, name: &str) -> u32 {
        let mut st = self.lock();
        Self::push_locked(&mut st, name)
    }

    /// The index of an **already known** name — never creates one.
    ///
    /// `None` is the honest answer for a name this space has never seen: a private space knows only
    /// what its own threads created, where the C's `enif_make_existing_atom` reads the one global
    /// table. An under-answer (the C's own `0`, "no such atom") rather than a wrong index, and
    /// `beam_abi`'s per-env atom record covers a name interned earlier in the same env.
    pub(crate) fn lookup(&self, name: &str) -> Option<u32> {
        let st = self.lock();
        st.by_name.get(name).copied()
    }

    /// The name at private `index`, or `None` when the space does not reach that index.
    ///
    /// This is what lets a foreign thread read back an atom it created (and one an earlier thread
    /// created), i.e. `enif_get_atom`/`enif_get_atom_length` answer by name without touching — or
    /// locking — the runtime's live table.
    pub(crate) fn name_of(&self, index: u32) -> Option<String> {
        let st = self.lock();
        st.names.get(index as usize).cloned()
    }

    /// A dense [`AtomTable`] in which this space's indices name this space's atoms — the table a
    /// foreign thread **encodes** a term against.
    ///
    /// `beam_codec`'s [`Encoder`](beam_codec::Encoder) resolves every atom through a table, so this
    /// is what turns private indices back into names on the wire: an image built over it spells each
    /// atom by name, which is exactly what the runtime's decoder needs (it interns the names into the
    /// live table itself). Nothing of the live table is involved, and no lookup ever crosses back.
    ///
    /// Rebuilt lazily and cached under the hub's lock, invalidated by the `names.len()` stamp: names
    /// are only ever appended, so a moved length is exactly "the image is stale". (Unlike a mirror of a
    /// mutable table, no index can be *replaced* here, so the length is a complete stamp.) Nothing is
    /// built in [`ForeignAtomHub::new`] — a runtime with no foreign thread pays nothing.
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

    /// Push `name` into the space if it is not there, answering its index — the one append.
    ///
    /// The caller holds the lock (every call site is inside a `MutexGuard`).
    fn push_locked(st: &mut HubState, name: &str) -> u32 {
        if let Some(index) = st.by_name.get(name) {
            return *index;
        }
        // The space is bounded by the number of distinct atoms one runtime's NIF threads create,
        // which is a `usize`-to-`u32` narrowing only in a space no process could allocate.
        debug_assert!(st.names.len() < u32::MAX as usize, "the private space is not exhausted");
        let index = st.names.len() as u32;
        let owned = name.to_string();
        st.names.push(owned.clone());
        st.by_name.insert(owned, index);
        index
    }

    /// The hub's one lock, with a **poisoned** lock recovered rather than propagated.
    ///
    /// The state behind it is an append-only name vector plus its inverse map and a length-stamped
    /// cache, so a panic under the lock cannot leave a shape a recovery would misread: an interrupted
    /// append can at worst leave a name known by index and not by name, which the next `reserve` of
    /// that name repairs by appending it again (a duplicate *name*, never a renamed index).
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
