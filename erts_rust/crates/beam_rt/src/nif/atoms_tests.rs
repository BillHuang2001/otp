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

//! Unit tests for the **shared atom-index hub** ([`super::ForeignAtomHub`]) and its two consumers —
//! the code server's drain ([`crate::codeserver::CodeServer`]) and the per-call
//! [`PendingInterner`] — with no C toolchain, no oracle and no scheduler involved.
//!
//! Each test names the invariant it pins, in the vocabulary of the module docs: the reservation is
//! idempotent, a reservation lands at exactly the reserved index, a runtime-side append with no
//! reservations is adopted in index order, a runtime-side append **with** a reservation re-anchors (and
//! says so through the divergence counter), `has_pending` is true exactly between a reservation and its
//! sync, and the encoder image names the same atom at every index.

use super::*;
use crate::context::PendingInterner;
use crate::codeserver::CodeServer;
use beam_bifs::AtomInterner;

/// A table holding `names`, in order.
fn table(names: &[&str]) -> AtomTable {
    let mut table = AtomTable::new();
    table.seed(names.iter().copied());
    table
}

/// `table`'s name at `index`, as an owned string.
fn name_at(table: &AtomTable, index: u32) -> String {
    table
        .name(Atom::from_raw(index))
        .unwrap_or_else(|| panic!("index {index} names an atom"))
        .to_string()
}

/// A reservation is a *claim* on an index, not an entry: reserving the same name twice answers the
/// same index and states nothing new to commit.
#[test]
fn reserve_is_idempotent() {
    let atoms = table(&["a", "b"]);
    let hub = ForeignAtomHub::new(&atoms);

    let first = hub.reserve("c");
    let again = hub.reserve("c");
    let existing = hub.reserve("a");

    assert_eq!(first, 2, "a new name takes the live table's own next index");
    assert_eq!(again, first, "a second reservation of one name is the same claim");
    assert_eq!(existing, 0, "an already interned name keeps its index");
    assert_eq!(
        hub.table_len(),
        3,
        "the mirror grew by exactly one name, not three"
    );
}

/// The whole point: a reservation is committed at **exactly** the index it was reserved at, so the
/// atom term a foreign thread built on it names the atom it asked for.
#[test]
fn reserve_then_sync_lands_the_name_at_the_reserved_index() {
    let mut atoms = table(&["a", "b"]);
    let hub = ForeignAtomHub::new(&atoms);

    let c = hub.reserve("c");
    let d = hub.reserve("d");
    assert_eq!((c, d), (2, 3));
    assert_eq!(
        atoms.lookup("c"),
        None,
        "a reservation is not in the live table before the sync"
    );

    hub.sync(&mut atoms);

    assert_eq!(atoms.len(), 4);
    assert_eq!(name_at(&atoms, c), "c");
    assert_eq!(name_at(&atoms, d), "d");
    assert_eq!(hub.lookup("c"), Some(c));
    assert_eq!(hub.lookup("d"), Some(d));
    assert_eq!(hub.divergence_count(), 0, "the normal path never diverges");
}

/// `name_of`/`lookup` round-trip, and both answer `None` — never a fabricated index or name — for
/// what the mirror does not reach.
#[test]
fn name_of_and_lookup_round_trip() {
    let atoms = table(&["a", "b"]);
    let hub = ForeignAtomHub::new(&atoms);

    for index in 0..2u32 {
        let name = name_at(&atoms, index);
        assert_eq!(hub.name_of(index).as_deref(), Some(name.as_str()));
        assert_eq!(hub.lookup(&name), Some(index));
    }

    assert_eq!(hub.name_of(2), None, "beyond the mirror's reach");
    assert_eq!(hub.lookup("nope"), None, "lookup never creates");
    assert_eq!(
        atoms.lookup("nope"),
        None,
        "and it did not create it in the live table either"
    );
}

/// A name reserved by a foreign thread is already nameable at the index it will keep — that is what
/// lets an in-worker `atom_name` answer before any sync happens.
#[test]
fn a_reservation_is_nameable_before_it_is_committed() {
    let atoms = table(&["a"]);
    let hub = ForeignAtomHub::new(&atoms);

    let index = hub.reserve("pending");
    assert_eq!(hub.name_of(index).as_deref(), Some("pending"));
    assert_eq!(hub.lookup("pending"), Some(index));
    assert_eq!(
        atoms.name(Atom::from_raw(index)),
        None,
        "the live table does not hold it yet"
    );
}

/// A runtime-side append between two syncs, with no reservations outstanding, is **adopted** in index
/// order — the ordinary shape, which must never count as divergence.
#[test]
fn a_runtime_append_without_reservations_is_adopted_in_index_order() {
    let mut atoms = table(&["a", "b"]);
    let hub = ForeignAtomHub::new(&atoms);

    atoms.intern("c");
    atoms.intern("d");
    hub.sync(&mut atoms);

    assert_eq!(hub.table_len(), 4);
    assert_eq!(hub.name_of(2).as_deref(), Some("c"));
    assert_eq!(hub.name_of(3).as_deref(), Some("d"));
    assert_eq!(hub.lookup("d"), Some(3));
    assert_eq!(
        hub.divergence_count(),
        0,
        "adopting a runtime append moves no reservation"
    );
}

/// A runtime-side append **with** a standing reservation takes the very index that reservation points
/// at, so the sync re-anchors it: the name still reaches the table exactly once, at a real index, and
/// the hub says the window was hit.
#[test]
fn a_runtime_append_with_a_reservation_re_anchors() {
    let mut atoms = table(&["a", "b"]);
    let hub = ForeignAtomHub::new(&atoms);

    let reserved = hub.reserve("foreign");
    assert_eq!(reserved, 2);
    // The runtime appends while the hub still holds the reservation — the drift the re-anchor covers.
    atoms.intern("runtime");

    hub.sync(&mut atoms);

    assert_eq!(
        hub.divergence_count(),
        1,
        "the re-anchor is reported, not hidden"
    );
    assert_eq!(
        hub.lookup("foreign"),
        Some(3),
        "the reservation moved past the runtime's own name"
    );
    assert_eq!(name_at(&atoms, 3), "foreign");
    assert_eq!(name_at(&atoms, 2), "runtime");
    assert_eq!(atoms.len(), 4);
    assert_eq!(
        atoms.lookup("foreign"),
        Some(Atom::from_raw(3)),
        "and it is really in the live table, once"
    );
    // The mirror is positional again: every index names the live table's atom.
    for index in 0..atoms.len() as u32 {
        assert_eq!(hub.name_of(index).as_deref(), Some(name_at(&atoms, index).as_str()));
    }
}

/// The one re-anchor this runtime can reach — the runtime interned the very name that was reserved, so
/// the mirror's length does not move while its tail is rebuilt — still leaves the encoder image naming
/// the same atom at every index.
#[test]
fn a_re_anchor_leaves_the_scratch_agreeing_with_the_mirror() {
    let mut atoms = table(&["a"]);
    let hub = ForeignAtomHub::new(&atoms);

    hub.reserve("shared_name");
    let before = hub.scratch_table();
    assert_eq!(
        before.name(Atom::from_raw(1)).map(str::to_string).as_deref(),
        Some("shared_name")
    );

    // The runtime interns the very name that was reserved: the re-anchor resolves the reservation to
    // the runtime's own index and the mirror's length does not change.
    atoms.intern("shared_name");
    hub.sync(&mut atoms);
    assert_eq!(hub.divergence_count(), 1);
    assert_eq!(hub.table_len(), 2);

    let after = hub.scratch_table();
    assert_eq!(after.len(), hub.table_len());
    for index in 0..hub.table_len() as u32 {
        assert_eq!(
            after.name(Atom::from_raw(index)).map(str::to_string),
            hub.name_of(index)
        );
    }
    assert_eq!(hub.lookup("shared_name"), Some(1));
}

/// `has_pending` is a lock-free signal that is true exactly between a reservation and its commit.
#[test]
fn has_pending_is_true_only_between_reserve_and_sync() {
    let mut atoms = table(&["a"]);
    let hub = ForeignAtomHub::new(&atoms);
    assert!(!hub.has_pending(), "nothing reserved yet");
    assert!(!hub.needs_sync(atoms.len()), "and the mirror is in sync");

    hub.reserve("b");
    assert!(hub.has_pending());
    assert!(hub.needs_sync(atoms.len()));

    hub.sync(&mut atoms);
    assert!(!hub.has_pending(), "a sync commits every reservation");
    assert!(!hub.needs_sync(atoms.len()));

    // A reservation of an already-committed name states nothing new to commit.
    hub.reserve("b");
    assert!(!hub.has_pending());
    assert!(!hub.needs_sync(atoms.len()));

    // A runtime-side append is a reason to sync even with nothing reserved.
    atoms.intern("c");
    assert!(!hub.has_pending());
    assert!(hub.needs_sync(atoms.len()));
}

/// The cached encoder image names the same atom as the hub at every index, and is reused while the
/// name list has not moved (the `names.len()` stamp is the invalidation trigger).
#[test]
fn scratch_table_matches_the_live_tables_names() {
    let mut atoms = table(&["a", "b", "c"]);
    let hub = ForeignAtomHub::new(&atoms);

    hub.reserve("reserved");
    let scratch = hub.scratch_table();
    assert_eq!(scratch.len(), hub.table_len());
    for index in 0..hub.table_len() as u32 {
        assert_eq!(
            scratch.name(Atom::from_raw(index)).map(str::to_string),
            hub.name_of(index),
            "index {index} disagrees between the encoder image and the hub"
        );
        let live = atoms.name(Atom::from_raw(index)).map(str::to_string);
        if live.is_some() {
            assert_eq!(live, hub.name_of(index), "a committed index must match the table");
        }
    }

    // Cached while nothing moved; rebuilt once the name list grew.
    assert!(
        Arc::ptr_eq(&scratch, &hub.scratch_table()),
        "an unchanged name list reuses the image"
    );
    hub.sync(&mut atoms);
    hub.reserve("another");
    assert!(
        !Arc::ptr_eq(&scratch, &hub.scratch_table()),
        "a grown name list invalidates the image"
    );
}

/// The code server is the one place the live table grows, so it drains the hub at the entry of every
/// mutating atom path: a reservation reaches the table at its reserved index **before** the runtime's
/// own `intern` takes a slot, and no re-anchor is ever needed on that path.
#[test]
fn the_code_server_drains_the_hub_before_its_own_append() {
    let mut server = CodeServer::new();
    server.atoms_mut().seed(["a", "b"]);
    let hub = ForeignAtomHub::new(server.atoms());
    server.attach_foreign_atoms(Arc::clone(&hub));
    assert!(server.foreign_atoms().is_some());

    // A foreign thread reserves index 2 …
    let reserved = hub.reserve("foreign");
    assert_eq!(reserved, 2);
    assert_eq!(server.atoms().lookup("foreign"), None);

    // … and the runtime's own append commits it first and then takes index 3.
    let runtime = server.atoms_mut().intern("runtime");

    assert_eq!(runtime.raw(), 3, "the runtime's name lands after the reservation");
    assert_eq!(
        server.atoms().name(Atom::from_raw(reserved)).map(str::to_string).as_deref(),
        Some("foreign")
    );
    assert_eq!(hub.divergence_count(), 0, "the drain makes the re-anchor unnecessary");
    assert!(!hub.has_pending());
}

/// A `&mut AtomTable` held across a foreign reservation is the one window the drain cannot cover: the
/// next mutating entry re-anchors, so the name still lands exactly once (the documented residual).
#[test]
fn an_append_inside_one_borrow_is_re_anchored_at_the_next_entry() {
    let mut server = CodeServer::new();
    server.atoms_mut().seed(["a", "b"]);
    let hub = ForeignAtomHub::new(server.atoms());
    server.attach_foreign_atoms(Arc::clone(&hub));

    {
        // One borrow, two appends: the hub is drained at the entry and sees neither append.
        let table = server.atoms_mut();
        table.intern("runtime_one");
        table.intern("runtime_two");
    }
    let reserved = hub.reserve("foreign");
    assert_eq!(reserved, 2, "the hub's mirror still ends at the drained length");

    // Any later mutating entry re-anchors: the two runtime names are absorbed and the reservation
    // moves behind them.
    let _ = server.atoms_mut();

    assert_eq!(hub.divergence_count(), 1);
    assert_eq!(
        server.atoms().lookup("foreign"),
        hub.lookup("foreign").map(Atom::from_raw)
    );
    assert_eq!(
        server
            .atoms()
            .name(Atom::from_raw(hub.lookup("foreign").expect("reserved")))
            .map(str::to_string)
            .as_deref(),
        Some("foreign"),
        "the name is in the live table exactly once, at the index the hub now reports"
    );
}

/// **One allocator**: with no foreign reservation in the way, a hub-adopted interner and a plain one
/// answer the same indices — and both commit their names to the same places.
#[test]
fn a_hub_adopted_interner_and_a_plain_one_agree_without_reservations() {
    let base = table(&["a", "b"]);

    let mut plain = PendingInterner::new(base.len());
    let plain_indices = [plain.intern("x").raw(), plain.intern("y").raw()];

    let hub = ForeignAtomHub::new(&base);
    let mut adopted = PendingInterner::new(base.len());
    adopted.adopt_hub(Arc::clone(&hub));
    let hub_indices = [adopted.intern("x").raw(), adopted.intern("y").raw()];

    assert_eq!(hub_indices, plain_indices, "the two shapes must agree");
    assert_eq!(hub_indices, [2, 3]);

    // Both commit to the same places, through their own route.
    let mut plain_table = base.clone();
    assert!(!plain.is_empty());
    plain.commit(&mut plain_table);
    assert!(plain.is_empty(), "a plain commit drains its list");

    let mut hub_table = base.clone();
    assert!(!adopted.is_empty(), "the hub reports the outstanding reservation");
    adopted.commit(&mut hub_table);
    assert!(adopted.is_empty(), "a hub commit drains the reservation tail");

    for index in 0..plain_table.len() as u32 {
        assert_eq!(name_at(&plain_table, index), name_at(&hub_table, index));
    }
    assert_eq!(hub.divergence_count(), 0);
}

/// The hub rides in the interner's **shared** cell, so a handle `share`d before the adoption sees it
/// (the shape of a `load_nif/2` call: the BIF layer and the NIF host write one list).
#[test]
fn an_already_shared_handle_sees_the_adopted_hub() {
    let atoms = table(&["a"]);
    let hub = ForeignAtomHub::new(&atoms);

    let mut pending = PendingInterner::new(atoms.len());
    let mut shared = pending.share();
    pending.adopt_hub(Arc::clone(&hub));

    // The second handle reserves through the hub, not through `base + offset`.
    assert_eq!(shared.intern("z").raw(), 1);
    assert!(hub.has_pending(), "the hub saw the second handle's reservation");
    assert!(!pending.is_empty(), "and the first handle reports it too");
}

/// A `Runtime` that never installs a hub keeps `foreign_atoms()` empty — the state every differential
/// and benchmark leg runs in — and installing one makes the runtime's own interning path drain it.
#[test]
fn installing_the_hub_is_off_by_default_and_drained_by_the_runtime() {
    let mut runtime = crate::runtime::Runtime::with_defaults().expect("the default runtime");
    assert!(
        runtime.foreign_atoms().is_none(),
        "a plain runtime must not build the mirror"
    );

    runtime.install_foreign_atom_hub();
    let hub = runtime.foreign_atoms().expect("installed");
    assert_eq!(hub.table_len(), runtime.atoms().len());
    assert!(!hub.has_pending());

    // A reservation made here stands in for a NIF-created thread's: the runtime's own interning path
    // drains the hub, so the name reaches the live table at exactly the index the hub reserved.
    let reserved = hub.reserve("hub_probe_atom");
    runtime.intern("another_runtime_atom");
    assert_eq!(
        runtime
            .atoms()
            .name(Atom::from_raw(reserved))
            .map(str::to_string)
            .as_deref(),
        Some("hub_probe_atom")
    );
    assert_eq!(hub.lookup("hub_probe_atom"), Some(reserved));
    assert!(!hub.has_pending());
}
