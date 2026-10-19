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

//! Integration evidence for the **distributed-spawn reservation seam** of [`ProcessTable`]:
//! claim a local pid identity, install the process into exactly that identity, or roll the claim
//! back without leaking.
//!
//! # The blocker this pins
//!
//! An inbound `DOP_SPAWN_REQUEST` (`dist.c:2919-3025`) asks this node to create a process and then
//! report the created pid in `{spawn_reply, Ref, To, Flags, From}` (`dist.c:1882-1900`). The Rust
//! runtime's inbound distribution path can create processes but had no way to create the one a
//! spawn request asks for **while naming the pid it will report** — a `Process` (the PCB) carries
//! no `ProcessEntry` (the host keys that by pid exactly as it does for `spawn`), so the host needs
//! the identity *before* it installs its own per-process entry, and needs to abandon the attempt
//! without leaking a number or a slot. `ProcessTable::{reserve_remote, spawn_reserved,
//! cancel_reservation}` is that seam; [`crate::reserve`] (the module) states the C derivation.
//!
//! # What the C actually does (so nothing is claimed that is not there)
//!
//! `dist.c:2993-3025` validates the message, parses the spawn options (`opts_error == 1` ⇒ a
//! `spawn_reply` carrying `badopt`), then calls `erl_create_process` and — on failure with
//! `so.error_code == SYSTEM_LIMIT` — answers `spawn_reply` with `system_limit`. **The pid is
//! allocated inside that call** (`erts_ptab_new_process`) and reported by the created process
//! itself (`erl_process.c:12934-12951`, `so.tag = am_spawn_reply`). There is **no** pid
//! pre-reservation on that path, so the Rust claim is a *seam the split creation path needs*, not a
//! C behaviour: only the error vocabulary (`badarg` / `system_limit`) is reproduced.
//!
//! # What each group of checks pins
//!
//! * the happy path — the installed process occupies **exactly** the reserved `(number, serial)`,
//!   the claim is gone, and the number left the reuse list and did not come back;
//! * a claim **blocks** [`ProcessTable::alloc_pid`] from handing the number out (and a second
//!   allocation still works) — the property the whole seam exists for;
//! * every refusal of [`ProcessTable::reserve_remote`]: a live number, a claimed number,
//!   an out-of-range number/serial (`system_limit`), a foreign node, and an uninstalled local node;
//! * every refusal of a **stale** handle (consumed, cancelled, foreign-table, fabricated) installing
//!   nothing;
//! * the rollback pins: a cancel makes the number allocatable again exactly **once** (a duplicated
//!   reuse entry would hand out one identity to two processes), and a failed `spawn_reserved`
//!   releases the claim instead of leaking it;
//! * a many-round reserve/cancel loop leaving the slot count and the reuse list length **stable**.
//!
//! # Determinism and coverage limits
//!
//! Every check is a pure table assertion — no timing, no threads, no `erl`. The oracle is
//! deliberately **not** driven here: a `DOP_SPAWN_REQUEST` receipt needs two connected nodes exchanging
//! `DOP_*` control messages, which is not expressible through a single `erl -noshell -eval` process
//! (the same reason the differential corpus's `wire` group covers the ETF codec only). The
//! substitute is the crate-level semantic pin against the C described above: the shape of the
//! reported pid, the `badarg`/`system_limit` vocabulary, and the "a claimed pid is not a live
//! process" rule (`is_process_alive/1` of a claimed-but-uncreated pid is `false` in the C, where the
//! pid does not exist yet).

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_gc::HeapPolicy;
use beam_process::prelude::*;
use beam_term::prelude::{
    external_pid_size_words, make_atom, write_external_pid, Atom, AtomTable, Eterm,
};

/// Asserts the failure is exactly `badarg` (the C's answer for a malformed, refused or stale spawn
/// request).
///
/// [`BeamError`] has no `PartialEq`, so the verdict is matched — which is also the stronger check:
/// a `TermNotFound` or a substituted `Raise` would pass an `is_err()`.
fn assert_badarg<T: std::fmt::Debug>(result: Result<T, BeamError>) {
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("expected badarg, got {other:?}"),
    }
}

/// Asserts the failure is exactly `system_limit` (the C's `SYSTEM_LIMIT` on this path,
/// `dist.c:3020-3023`).
fn assert_system_limit<T: std::fmt::Debug>(result: Result<T, BeamError>) {
    match result {
        Err(BeamError::SystemLimit) => {}
        other => panic!("expected system_limit, got {other:?}"),
    }
}

/// A process table with **this** node installed, the atom table its [`SignalAtoms`] were interned
/// from, and a scratch heap for building the external-pid terms a distribution-derived claim
/// arrives as.
struct Fixture {
    table: ProcessTable,
    names: AtomTable,
    heap: Heap,
    /// This node's name atom (what [`ProcessTable::set_local_node`] was given).
    node: Atom,
}

impl Fixture {
    /// An empty table whose local node is `me@host`.
    fn new() -> Fixture {
        let mut names = AtomTable::new();
        let atoms = SignalAtoms::from_table(&mut names);
        let node = names.intern("me@host");
        let mut table = ProcessTable::new(atoms);
        table.set_local_node(node);
        Fixture {
            table,
            names,
            heap: Heap::new(4096).expect("a scratch heap for external-pid terms"),
            node,
        }
    }

    /// Builds an external pid term on the scratch heap (the frozen
    /// `[header, node, creation, data]` thing).
    fn external_pid(&mut self, node: Eterm, number: u32, serial: u32) -> Eterm {
        // SAFETY: `alloc_words` hands out `external_pid_size_words()` (4) exclusively-owned,
        // writable words.
        let p = unsafe {
            self.heap
                .alloc_words(external_pid_size_words())
                .expect("the scratch heap has room")
        };
        // SAFETY: `p` addresses the four words just handed out and `write_external_pid` writes all
        // of them before the pointer escapes this block; `node` is a live atom term.
        unsafe { write_external_pid(p, node, number, serial, 0) }
    }

    /// A [`RemotePid`] naming **this** node — the identity to claim.
    fn local_identity(&mut self, number: u32, serial: u32) -> RemotePid {
        let node = make_atom(self.node);
        let term = self.external_pid(node, number, serial);
        RemotePid::from_eterm(term).expect("an external pid")
    }

    /// A [`RemotePid`] naming **another** node (the answer must be `badarg`).
    fn foreign_identity(&mut self, number: u32, serial: u32) -> RemotePid {
        let other = self.names.intern("other@host");
        let node = make_atom(other);
        let term = self.external_pid(node, number, serial);
        RemotePid::from_eterm(term).expect("an external pid")
    }

    /// Claims `(number, serial)` — asserted to succeed.
    fn reserve_local(&mut self, number: u32, serial: u32) -> Reservation {
        let identity = self.local_identity(number, serial);
        self.table
            .reserve_remote(identity)
            .expect("the identity is claimable")
    }

    /// The wire-facing claim, unasserted, so a test can pin the exact error.
    fn try_reserve_local(&mut self, number: u32, serial: u32) -> Result<Reservation, BeamError> {
        let identity = self.local_identity(number, serial);
        self.table.reserve_remote(identity)
    }

    /// Spawns a process with the default heap policy and asserts it succeeded.
    fn spawn(&mut self) -> ProcessId {
        self.table
            .spawn_default()
            .expect("the table can create a process")
    }
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

/// `reserve_remote` ⇒ `spawn_reserved` installs a process at **exactly** the reserved identity, and
/// nothing of the claim survives.
#[test]
fn the_spawned_process_occupies_exactly_the_reserved_identity() {
    let mut f = Fixture::new();
    let free_before = f.table.free_count();

    let reservation = f.reserve_local(0, 0);
    assert_eq!(reservation.pid(), ProcessId::new(0, 0));
    assert_eq!(reservation.number(), 0);
    assert_eq!(reservation.serial(), 0);

    let pid = f
        .table
        .spawn_reserved(reservation, HeapPolicy::default())
        .expect("the reserved identity is claimable");
    assert_eq!(pid, reservation.pid(), "the reply carries this pid");
    assert_eq!(pid.to_eterm(), ProcessId::new(0, 0).to_eterm());

    // The process is live, at that identity and no other.
    assert!(f.table.contains(pid));
    assert_eq!(f.table.get(pid).expect("live").pid(), pid);
    assert_eq!(f.table.len(), 1);

    // The claim is gone, and the number never entered the reuse list (it was a *new* number).
    assert!(!f.table.is_reserved(0));
    assert_eq!(f.table.reserved_count(), 0);
    assert!(f.table.reserved_pids().is_empty());
    assert_eq!(f.table.free_count(), free_before);
    assert_eq!(f.table.slot_count(), 1);

    // The identity is not handed out again.
    assert_ne!(f.table.alloc_pid().expect("the table can grow").number(), 0);
}

/// A claimed identity is **not** a live process — the reply's pid exists as a term, not as a
/// process, until the claim is consumed.
#[test]
fn a_claim_creates_no_process() {
    let mut f = Fixture::new();
    let reservation = f.reserve_local(2, 0);
    assert!(!f.table.is_process_alive(reservation.pid()));
    assert!(f.table.get(reservation.pid()).is_none());
    assert_eq!(f.table.len(), 0);
    assert_eq!(
        f.table.slot_count(),
        0,
        "a claim is not an allocation: no slot is materialised"
    );
    // ... and the ordinary allocation path is what a plain spawn would use.
    assert_eq!(f.spawn(), ProcessId::new(0, 0));
    assert!(!f.table.is_process_alive(reservation.pid()));
}

// ---------------------------------------------------------------------------
// A claim blocks the ordinary allocation path
// ---------------------------------------------------------------------------

/// A claim keeps `alloc_pid` off the reserved number — the property the seam exists for — while a
/// second allocation still works.
#[test]
fn a_claim_keeps_alloc_pid_off_the_reserved_number() {
    let mut f = Fixture::new();
    let zero = f.spawn();
    let one = f.spawn();
    assert_eq!(f.spawn(), ProcessId::new(2, 0));
    assert_eq!((zero.number(), one.number()), (0, 1));

    // Free number 1: its slot's serial is bumped, so this is the identity the reuse list holds.
    f.table.take(one).expect("one is live");
    assert_eq!(f.table.free_count(), 1);
    let bumped = ProcessId::new(1, one.serial() + 1);

    let reservation = f.reserve_local(1, (one.serial() + 1) as u32);
    assert_eq!(reservation.pid(), bumped);
    assert_eq!(
        f.table.free_count(),
        0,
        "the claim takes the number out of the reuse list"
    );

    let next = f.table.alloc_pid().expect("the table can still grow");
    assert_ne!(next.number(), 1, "a claimed number is never handed out");
    assert_eq!(next.number(), 3);
    assert_eq!(
        f.table
            .alloc_pid()
            .expect("the table can still grow")
            .number(),
        4,
        "a second allocation still works"
    );
}

/// Two claims on different numbers coexist and both install.
#[test]
fn two_reservations_of_different_numbers_coexist() {
    let mut f = Fixture::new();
    let five = f.reserve_local(5, 0);
    let seven = f.reserve_local(7, 0);
    assert_ne!(five.id(), seven.id(), "each claim gets its own id");
    assert_eq!(f.table.reserved_count(), 2);
    let mut claimed = f.table.reserved_pids();
    claimed.sort();
    assert_eq!(
        claimed,
        vec![ProcessId::new(5, 0), ProcessId::new(7, 0)],
        "both identities are claimed"
    );

    let first = f
        .table
        .spawn_reserved(five, HeapPolicy::default())
        .expect("the first claim installs");
    let second = f
        .table
        .spawn_reserved(seven, HeapPolicy::default())
        .expect("the second claim installs");
    assert_eq!(first, ProcessId::new(5, 0));
    assert_eq!(second, ProcessId::new(7, 0));
    assert_eq!(f.table.reserved_count(), 0);
    assert!(f.table.contains(first) && f.table.contains(second));
    assert_eq!(f.table.len(), 2);
}

// ---------------------------------------------------------------------------
// Every refusal of `reserve_remote`
// ---------------------------------------------------------------------------

/// Reserving a number that names a **live** process is `badarg`.
#[test]
fn reserving_a_live_number_is_badarg() {
    let mut f = Fixture::new();
    let pid = f.spawn();
    let identity = f.local_identity(0, pid.serial() as u32);
    assert_badarg(f.table.reserve_remote(identity));
    assert_eq!(f.table.reserved_count(), 0, "nothing was claimed");
    assert_eq!(f.table.free_count(), 0, "the reuse list is untouched");
}

/// Reserving an already-claimed number is `badarg` — with the same serial **and** a different one
/// (one slot cannot hold two claims).
#[test]
fn reserving_a_claimed_number_is_badarg() {
    let mut f = Fixture::new();
    let reservation = f.reserve_local(2, 0);
    assert_badarg(f.try_reserve_local(2, 0));
    assert_badarg(f.try_reserve_local(2, 9));
    assert_badarg(f.table.reserve_identity(ProcessId::new(2, 3)));
    assert_eq!(f.table.reserved_count(), 1, "the first claim survives");
    // ... and the surviving claim still installs.
    assert_eq!(
        f.table
            .spawn_reserved(reservation, HeapPolicy::default())
            .expect("the first claim installs"),
        ProcessId::new(2, 0)
    );
}

/// A number or serial outside the pid space is `system_limit` (the C's `SYSTEM_LIMIT`).
#[test]
fn an_out_of_range_identity_is_system_limit() {
    let mut f = Fixture::new();
    // The number half is reachable from the wire (a `RemotePid`'s number is a `u32`).
    let too_big = (MAX_PROCESS_NUMBER + 1) as u32;
    assert_system_limit(f.try_reserve_local(too_big, 0));
    // The serial half is not (`MAX_PROCESS_SERIAL` is `u32::MAX`), so it is pinned on the local
    // spelling, where the identity is a `u64`.
    assert_system_limit(
        f.table
            .reserve_identity(ProcessId::new(0, MAX_PROCESS_SERIAL + 1)),
    );
    assert_system_limit(
        f.table
            .reserve_identity(ProcessId::new(MAX_PROCESS_NUMBER + 1, 0)),
    );
    assert_eq!(f.table.reserved_count(), 0, "no claim was recorded");
    // The exact bounds are accepted.
    assert!(f
        .table
        .reserve_identity(ProcessId::new(MAX_PROCESS_NUMBER, MAX_PROCESS_SERIAL))
        .is_ok());
}

/// A pid whose node is not this node is `badarg`, and so is any claim on a table that has **no**
/// local node installed (the identity cannot be verified at all — documented in `reserve.rs`).
#[test]
fn a_wrong_or_uninstalled_node_is_badarg() {
    let mut f = Fixture::new();
    let foreign = f.foreign_identity(3, 0);
    assert_badarg(f.table.reserve_remote(foreign));
    assert_eq!(f.table.reserved_count(), 0);

    // A table that never installed its node: `set_local_node` is what makes the check possible.
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    let mut bare = ProcessTable::new(atoms);
    assert_eq!(bare.local_node(), None);
    let identity = f.local_identity(3, 0);
    assert_badarg(bare.reserve_remote(identity));
    assert_eq!(bare.reserved_count(), 0);
    // The node-less spelling needs no node, but a *claimed* identity still cannot be re-claimed.
    let reservation = bare
        .reserve_identity(ProcessId::new(3, 0))
        .expect("a local identity needs no node check");
    assert_badarg(bare.reserve_remote(f.local_identity(3, 0)));
    assert_eq!(bare.reserved_count(), 1);
    assert_eq!(reservation.pid(), ProcessId::new(3, 0));
}

// ---------------------------------------------------------------------------
// Stale handles never install anything
// ---------------------------------------------------------------------------

/// A consumed, cancelled or foreign handle is `badarg` and installs **no** process.
#[test]
fn a_stale_handle_installs_nothing() {
    let mut f = Fixture::new();

    // consumed
    let reservation = f.reserve_local(0, 0);
    let pid = f
        .table
        .spawn_reserved(reservation, HeapPolicy::default())
        .expect("the first spawn installs");
    assert_badarg(f.table.spawn_reserved(reservation, HeapPolicy::default()));
    assert_badarg(f.table.cancel_reservation(reservation));
    assert_eq!(f.table.len(), 1, "no second process was installed");
    assert_eq!(f.table.slot_count(), 1);
    assert!(f.table.contains(pid));
    assert_eq!(f.table.free_count(), 0);

    // cancelled
    let cancelled = f.reserve_local(4, 0);
    f.table
        .cancel_reservation(cancelled)
        .expect("cancel succeeds");
    assert_badarg(f.table.spawn_reserved(cancelled, HeapPolicy::default()));
    assert!(!f.table.contains(ProcessId::new(4, 0)));
    assert_eq!(f.table.len(), 1);

    // foreign — minted by another table (the id spaces of two fresh tables start at the same
    // value, so this is exactly the collision the instance tag exists to refuse)
    let mut other = Fixture::new();
    let foreign = other.reserve_local(6, 0);
    assert_badarg(f.table.spawn_reserved(foreign, HeapPolicy::default()));
    assert_badarg(f.table.cancel_reservation(foreign));
    assert_eq!(f.table.reserved_count(), 0);
    assert!(!f.table.contains(ProcessId::new(6, 0)));
    assert_eq!(f.table.len(), 1);
    // ... while the handle still works in its own table.
    assert_eq!(
        other
            .table
            .spawn_reserved(foreign, HeapPolicy::default())
            .expect("the owning table accepts it"),
        ProcessId::new(6, 0)
    );
}

// ---------------------------------------------------------------------------
// Rollback: a cancel returns the number exactly once
// ---------------------------------------------------------------------------

/// `cancel_reservation` returns the number to the reuse list, a later `alloc_pid` hands it out with
/// the slot's bumped serial, and the stale handle installs nothing.
#[test]
fn cancel_makes_the_number_allocatable_again() {
    let mut f = Fixture::new();
    let _zero = f.spawn();
    let one = f.spawn();
    f.table.take(one).expect("one is live");
    assert_eq!(f.table.free_count(), 1);
    let identity = ProcessId::new(1, one.serial() + 1);

    let reservation = f.reserve_local(1, (one.serial() + 1) as u32);
    assert_eq!(f.table.free_count(), 0);

    f.table
        .cancel_reservation(reservation)
        .expect("the cancel succeeds");
    assert_eq!(f.table.free_count(), 1, "the number is back in the list");
    assert!(!f.table.is_reserved(1));

    let handed_out = f.table.alloc_pid().expect("the number is reusable");
    assert_eq!(
        handed_out, identity,
        "a cancelled number comes back with the slot's bumped serial"
    );

    // The stale handle can neither install a process nor cancel again.
    assert_badarg(f.table.spawn_reserved(reservation, HeapPolicy::default()));
    assert_badarg(f.table.cancel_reservation(reservation));
    assert_ne!(
        f.table.alloc_pid().expect("the table can grow").number(),
        1,
        "the identity is never handed out a second time"
    );
    assert_eq!(f.table.len(), 1);
}

/// A **double** cancel is `badarg` and leaves exactly one reuse-list entry — the leak pin: a
/// duplicated entry would hand one identity to two processes.
#[test]
fn a_double_cancel_leaves_exactly_one_free_entry() {
    let mut f = Fixture::new();
    let _zero = f.spawn();
    let one = f.spawn();
    f.table.take(one).expect("one is live");
    let slots = f.table.slot_count();

    let reservation = f.reserve_local(1, (one.serial() + 1) as u32);
    assert_eq!(f.table.free_count(), 0);

    f.table
        .cancel_reservation(reservation)
        .expect("the first cancel succeeds");
    assert_eq!(f.table.free_count(), 1);
    assert_badarg(f.table.cancel_reservation(reservation));
    assert_eq!(
        f.table.free_count(),
        1,
        "a second cancel adds nothing: the number must appear once"
    );
    assert_eq!(f.table.slot_count(), slots, "and no slot was added");

    // The observable consequence: number 1 is handed out once and never again.
    let first = f.table.alloc_pid().expect("the number is reusable");
    let second = f.table.alloc_pid().expect("the table can grow");
    assert_eq!(first.number(), 1);
    assert_ne!(second.number(), 1);
}

/// A `spawn_reserved` that fails **releases** the claim: no leak, no stray slot, and the occupant
/// of the number is untouched.
#[test]
fn a_failed_spawn_releases_the_claim_instead_of_leaking_it() {
    let mut f = Fixture::new();
    let reservation = f.reserve_local(3, 0);
    assert_eq!(f.table.reserved_count(), 1);

    // Force the failure the way the C's `SYSTEM_LIMIT`/`invalid_message` arms would surface it:
    // the identity became live in the meantime, through the public insertion path.
    let squatter =
        Process::with_policy(ProcessId::new(3, 0), HeapPolicy::default()).expect("a fresh process");
    f.table.insert(squatter).expect("insert succeeds");

    assert_badarg(f.table.spawn_reserved(reservation, HeapPolicy::default()));
    assert_eq!(
        f.table.reserved_count(),
        0,
        "the claim was released, not left behind"
    );
    assert!(!f.table.is_reserved(3));
    assert_eq!(
        f.table.free_count(),
        0,
        "an occupied number is never returned to the reuse list"
    );
    assert_eq!(f.table.len(), 1, "no second process was installed");
    assert_eq!(
        f.table.get(ProcessId::new(3, 0)).map(Process::pid),
        Some(ProcessId::new(3, 0)),
        "the occupant is untouched"
    );
    assert_badarg(f.table.cancel_reservation(reservation));
}

/// Many reserve/cancel rounds leave the table's geometry **stable** — no slot and no reuse-list
/// entry accumulates.
#[test]
fn many_reserve_and_cancel_rounds_leak_nothing() {
    let mut f = Fixture::new();
    let mut pids = Vec::new();
    for _ in 0..4 {
        pids.push(f.spawn());
    }
    for pid in &pids {
        f.table.take(*pid).expect("the process is live");
    }
    let slots = f.table.slot_count();
    let free = f.table.free_count();
    assert_eq!((slots, free), (4, 4));

    for _round in 0..50 {
        for pid in &pids {
            let serial = (pid.serial() + 1) as u32;
            let reservation = f.reserve_local(pid.number() as u32, serial);
            assert!(f.table.is_reserved(pid.number()));
            f.table
                .cancel_reservation(reservation)
                .expect("cancel succeeds");
            assert!(!f.table.is_reserved(pid.number()));
            assert_eq!(f.table.slot_count(), slots, "no slot accumulated");
            assert_eq!(f.table.free_count(), free, "no reuse entry accumulated");
        }
    }
    assert_eq!(f.table.reserved_count(), 0);
    assert_eq!(f.table.len(), 0);
}
