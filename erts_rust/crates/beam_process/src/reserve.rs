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

//! Distributed-spawn **reservation** bookkeeping: claim a local pid identity *before* the process
//! it names exists.
//!
//! # The blocker this seam removes
//!
//! An inbound `DOP_SPAWN_REQUEST` (`dist.c:2919-3025`) asks this node to create a process and then
//! report the created pid back in `{spawn_reply, Ref, To, Flags, From}`. The Rust runtime's
//! inbound distribution path (`beam_rt`'s `dist/inbound.rs`) can create processes
//! ([`ProcessTable::spawn`](crate::ptab::ProcessTable::spawn)) but had no way to create the one a
//! spawn request asks for **while naming the pid it will report**: a `Process` (the PCB) carries
//! no `ProcessEntry` (the host keys that by pid exactly as it does for `spawn`), so the host must
//! know the pid *before* it installs its own per-process entry — and it must be able to abandon the
//! whole attempt without leaking a pid number or a table slot. [`Reservations`] is that claim
//! table; [`Reservation`] is the handle the caller carries across the inbound control message.
//!
//! # The C's ground truth (and what it does *not* have)
//!
//! `dist.c:2993-3025` validates the message (`tuple_arity != 6` ⇒ `invalid_message`), parses the
//! spawn options (`erts_parse_spawn_opts` failure with `opts_error == 1` ⇒ a `spawn_reply` carrying
//! `badopt`; `opts_error > 1` ⇒ `invalid_message`), then calls `erl_create_process` with
//! `so.tag = am_spawn_reply` and — on failure with `so.error_code == SYSTEM_LIMIT` — answers
//! `spawn_reply` with `system_limit`. **There is no pid pre-reservation anywhere on that path**:
//! the executing node allocates the pid *inside* `erl_create_process`
//! (`erts_ptab_new_process`, `erl_ptab.c:560-630`, whose own "slot reserved" comment is the ptab's
//! atomic slot grab, not a spawn request reservation) and reports `p->common.id` in the reply
//! (`erl_process.c:12934-12951`). This module therefore invents **no C behaviour**: it is the
//! *Rust-side* seam the runtime needs because its creation path is split (claim an identity, build
//! the reply, then install the process and the host's entry) where the C's is a single call. Only
//! the C's **error vocabulary** is reproduced: `badarg` for anything malformed, stale or refused,
//! `system_limit` for an exhausted pid number space.
//!
//! # The reply-path contract
//!
//! A caller serves a spawn request as:
//!
//! ```text
//! let r = table.reserve_remote(<identity>)?;        // claim, nothing installed
//! ...                                               // (opts already parsed: a failure here cancels)
//! let pid = table.spawn_reserved(r, policy)?;       // install at exactly r.pid()
//! // reply: {spawn_reply, Ref, To, Flags, pid.to_eterm()} on the peer's arena
//! ```
//!
//! [`Reservation::pid`] is the pid the reply must carry, and
//! [`ProcessId::to_eterm`](crate::pid::ProcessId::to_eterm) is how the reply's `From` element is
//! built — the reservation is what makes that pid known *before* the process exists. An aborted
//! request is [`ProcessTable::cancel_reservation`](crate::ptab::ProcessTable::cancel_reservation)
//! (the `badopt` / `invalid_message` arm), which returns the number to the table's reuse list so a
//! later allocation may hand it out again.
//!
//! # Failure modes (all typed, never a wrong result)
//!
//! | situation | error |
//! |---|---|
//! | the identity's node is not this node, or no local node is installed | `badarg` |
//! | the number/serial is out of the pid space | `system_limit` |
//! | the number names a live process, or is already claimed | `badarg` |
//! | a stale / already-consumed / cancelled / foreign-table handle | `badarg` |
//!
//! # No `Drop`
//!
//! A reservation is **explicitly** consumed
//! ([`ProcessTable::spawn_reserved`](crate::ptab::ProcessTable::spawn_reserved)) or
//! cancelled ([`ProcessTable::cancel_reservation`](crate::ptab::ProcessTable::cancel_reservation)):
//! there is no table reachable from [`Reservation`], so no destructor could release a claim, and
//! silently releasing on drop would make an abandoned handle indistinguishable from a live one. The
//! guarantee is instead the one that matters and is pinned by the crate's tests: a stale handle can
//! never install a process, and a released claim never leaks a pid number or a slot.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/dist.c` (`DOP_SPAWN_REQUEST` /
//! `DOP_SPAWN_REPLY` arms, `erts_dsig_send_spawn_reply`), `erl_process.c` (the creation call) and
//! `erl_ptab.c` (the pid number space).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use beam_common::Result;
use beam_term::prelude::{make_atom, Atom, Eterm};

use crate::error;
use crate::pid::{ProcessId, MAX_PROCESS_NUMBER, MAX_PROCESS_SERIAL};

/// The tag of the next [`Reservations`] instance created in this OS process.
///
/// [`Reservation`] carries its owner's tag so a handle from one table is a **typed `badarg`** in
/// another even when both tables happen to mint the same reservation id. Without it a foreign
/// handle whose `(id, pid)` matched a live claim by coincidence could consume the wrong claim.
static NEXT_TAG: AtomicU64 = AtomicU64::new(1);

/// A claim on a local pid **identity** whose process does not exist yet.
///
/// Issued by [`Reservations::claim`] (through
/// [`ProcessTable::reserve_remote`](crate::ptab::ProcessTable::reserve_remote)) and consumed by
/// [`ProcessTable::spawn_reserved`](crate::ptab::ProcessTable::spawn_reserved) or
/// [`ProcessTable::cancel_reservation`](crate::ptab::ProcessTable::cancel_reservation). It is
/// `Copy`, and a copied handle is just as stale as the original after the claim is consumed — the
/// claim is the state, the handle only names it.
///
/// [`Reservation::pid`] is the pid the inbound `DOP_SPAWN_REQUEST` reply must carry; it is **not**
/// a live process until the claim is consumed (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Reservation {
    /// The tag of the [`Reservations`] instance that issued this handle.
    table: u64,
    /// The monotonically increasing reservation id, unique within that instance.
    id: u64,
    /// The claimed identity.
    pid: ProcessId,
}

impl Reservation {
    /// The reservation id (monotonically increasing within its table, never reused).
    pub fn id(self) -> u64 {
        self.id
    }

    /// The claimed identity — the pid the spawn reply reports.
    pub fn pid(self) -> ProcessId {
        self.pid
    }

    /// The claimed process-table number.
    pub fn number(self) -> u64 {
        self.pid.number()
    }

    /// The claimed serial.
    pub fn serial(self) -> u64 {
        self.pid.serial()
    }
}

/// The reservation bookkeeping of one [`ProcessTable`](crate::ptab::ProcessTable).
///
/// A self-contained plain-value store: it owns no term and no heap, so it is `Send + Sync` (the
/// SMP [`SharedProcessTable`](crate::smp::SharedProcessTable) `unsafe impl Send/Sync` keeps
/// holding) and adds **no** GC root.
///
/// # Invariants
///
/// * At most one claim per process-table **number** — a claim is on a number, and the serial it
///   carries is the serial the eventual process will be installed with. Two claims with the same
///   number and different serials would both have to occupy that one slot, so the second is
///   refused.
/// * `Reservations` never talks to the slot table: whether the number is **live** and whether it
///   is in the reuse list are the table's business (see the `ptab.rs` entry points).
#[derive(Debug)]
pub struct Reservations {
    /// This instance's tag (see [`NEXT_TAG`]).
    tag: u64,
    /// The next reservation id to hand out.
    next_id: u64,
    /// Claimed identities by reservation id.
    claims: HashMap<u64, ProcessId>,
    /// The reverse index: claimed process-table number ⇒ reservation id.
    by_number: HashMap<u64, u64>,
}

impl Reservations {
    /// An empty claim table with a fresh instance tag.
    pub fn new() -> Reservations {
        Reservations {
            tag: NEXT_TAG.fetch_add(1, Ordering::Relaxed),
            next_id: 0,
            claims: HashMap::new(),
            by_number: HashMap::new(),
        }
    }

    /// Claims `pid` for a not-yet-created process.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::SystemLimit`] when `pid`'s number or serial is outside the pid space
    /// ([`MAX_PROCESS_NUMBER`]/[`MAX_PROCESS_SERIAL`] — the C's `SYSTEM_LIMIT`), and
    /// [`beam_common::BeamError::BadArg`] when the number is already claimed. Whether the number is **live** in
    /// the slot table is the caller's check (this store is not allowed to see it).
    pub fn claim(&mut self, pid: ProcessId) -> Result<Reservation> {
        check_identity(pid)?;
        if self.by_number.contains_key(&pid.number()) {
            return Err(error::bad_arg());
        }
        let id = self.next_id;
        // A wrap would re-issue a live id, which a stale handle could then resolve: refuse it as
        // space exhaustion rather than recycle ids.
        self.next_id = id.checked_add(1).ok_or_else(error::system_limit)?;
        self.claims.insert(id, pid);
        self.by_number.insert(pid.number(), id);
        Ok(Reservation {
            table: self.tag,
            id,
            pid,
        })
    }

    /// The identity `handle` names, if it is still a **live** claim of this table.
    ///
    /// Validates all three halves of the handle — the owner tag, the id and the identity — so a
    /// foreign, stale or fabricated handle cannot resolve.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] for an unknown, stale or foreign handle.
    pub fn validate(&self, handle: Reservation) -> Result<ProcessId> {
        if handle.table != self.tag {
            return Err(error::bad_arg());
        }
        match self.claims.get(&handle.id) {
            Some(&claimed) if claimed == handle.pid => Ok(claimed),
            _ => Err(error::bad_arg()),
        }
    }

    /// Consumes the claim `handle` names and returns the identity it held.
    ///
    /// This is the one release primitive: `spawn_reserved` and `cancel_reservation` both go
    /// through it, so a handle can be released at most once (the second attempt is `badarg`) and
    /// the caller owns the follow-up (install the process, or return the number to the reuse
    /// list).
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] for an unknown, stale or foreign handle.
    pub fn release(&mut self, handle: Reservation) -> Result<ProcessId> {
        let pid = self.validate(handle)?;
        self.claims.remove(&handle.id);
        self.by_number.remove(&pid.number());
        Ok(pid)
    }

    /// Drops the claim on `number`, if any — the "the identity is no longer available" hook.
    ///
    /// Used by [`ProcessTable::insert`](crate::ptab::ProcessTable::insert): a live process
    /// installed at a claimed number makes the claim meaningless, and every later
    /// `spawn_reserved` on that handle must answer `badarg` instead of clobbering the occupant.
    /// Returns the identity the dropped claim held.
    pub fn release_number(&mut self, number: u64) -> Option<ProcessId> {
        let id = self.by_number.remove(&number)?;
        self.claims.remove(&id)
    }

    /// Whether `number` is claimed by a live reservation.
    pub fn is_reserved(&self, number: u64) -> bool {
        self.by_number.contains_key(&number)
    }

    /// The claimed identities, in unspecified order.
    pub fn pids(&self) -> impl Iterator<Item = ProcessId> + '_ {
        self.claims.values().copied()
    }

    /// The number of live claims.
    pub fn len(&self) -> usize {
        self.claims.len()
    }

    /// Whether no identity is claimed.
    pub fn is_empty(&self) -> bool {
        self.claims.is_empty()
    }
}

impl Default for Reservations {
    fn default() -> Reservations {
        Reservations::new()
    }
}

/// Refuses a `pid` outside the process-table number/serial space (`system_limit`).
///
/// The C's `SYSTEM_LIMIT` (`dist.c:3020-3023` reports it as `spawn_reply` / `system_limit`), which
/// the process layer spells [`beam_common::BeamError::SystemLimit`]. A [`RemotePid`]'s parts are `u32`s, so the
/// **serial** half is unreachable from the wire today ([`MAX_PROCESS_SERIAL`] is `u32::MAX`) while
/// the **number** half is not ([`MAX_PROCESS_NUMBER`] is `2^28 - 1`); the check is written against
/// the `u64` identity so a wider input, or a narrower limit, stays covered.
///
/// [`RemotePid`]: crate::remote::RemotePid
///
/// # Errors
///
/// Returns [`beam_common::BeamError::SystemLimit`] when the number or the serial is out of range.
pub(crate) fn check_identity(pid: ProcessId) -> Result<()> {
    if pid.number() > MAX_PROCESS_NUMBER || pid.serial() > MAX_PROCESS_SERIAL {
        return Err(error::system_limit());
    }
    Ok(())
}

/// Refuses a pid identity whose node is not this node (`badarg`).
///
/// A reservation claims an identity **of this node**, so a `DOP_SPAWN_REQUEST`-derived pid naming
/// another node is a malformed request, not a local one — accepting it would install a process
/// under an identity the rest of the runtime reads as foreign (a wrong result), so it is refused.
///
/// `local` is the atom the runtime installed with
/// [`ProcessTable::set_local_node`](crate::ptab::ProcessTable::set_local_node); `node` is the
/// sysname **atom term** of the pid block. Both are immediates in the one shared atom namespace the
/// runtime owns, so they compare by value.
///
/// **The unset case is a `badarg`, not a guess**: with no local node installed the node identity
/// cannot be verified at all, and a reservation is exactly the operation that must not proceed on
/// an unverified identity. Every in-tree runtime installs the node at boot (beam_rt's
/// `Runtime::new`), so no live caller meets it.
///
/// # Errors
///
/// Returns [`beam_common::BeamError::BadArg`] when no local node is installed or `node` is not it.
pub(crate) fn check_local_node(local: Option<Atom>, node: Eterm) -> Result<()> {
    match local {
        Some(atom) if make_atom(atom) == node => Ok(()),
        _ => Err(error::bad_arg()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;

    /// A claim of the given number/serial, for the unit tests (which need no table).
    fn pid(number: u64, serial: u64) -> ProcessId {
        ProcessId::new(number, serial)
    }

    /// Asserts the failure is exactly `badarg` (the C's error for a refused spawn request).
    ///
    /// [`BeamError`] carries no `PartialEq`, so the verdict is matched rather than compared — and
    /// matching the **variant** is the stronger check anyway: a `TermNotFound` or a substituted
    /// `Raise` would pass an `is_err()`.
    fn assert_badarg<T: std::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::BadArg) => {}
            other => panic!("expected badarg, got {other:?}"),
        }
    }

    /// Asserts the failure is exactly `system_limit` (the C's `SYSTEM_LIMIT` on this path).
    fn assert_system_limit<T: std::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::SystemLimit) => {}
            other => panic!("expected system_limit, got {other:?}"),
        }
    }

    #[test]
    fn reservation_ids_are_monotonic_and_never_reused() {
        let mut claims = Reservations::new();
        let first = claims.claim(pid(1, 0)).expect("a fresh number");
        let second = claims.claim(pid(2, 0)).expect("a fresh number");
        assert!(second.id() > first.id(), "ids increase");
        // The first handle is consumed, so the id it held is never reachable again.
        assert_eq!(
            claims.release(first).expect("the claim resolves"),
            pid(1, 0)
        );
        let reused = claims.claim(pid(1, 7)).expect("the number is free again");
        assert!(reused.id() > second.id(), "a re-claim gets a fresh id");
    }

    #[test]
    fn a_claim_records_the_identity_and_the_handle_reads_it_back() {
        let mut claims = Reservations::new();
        let handle = claims.claim(pid(41, 9)).expect("a fresh number");
        assert_eq!(handle.pid(), pid(41, 9));
        assert_eq!(handle.number(), 41);
        assert_eq!(handle.serial(), 9);
        // The reply-path contract: the handle's pid is a pid term (the reply's `From`).
        assert_eq!(handle.pid().to_eterm(), pid(41, 9).to_eterm());
        assert_eq!(
            ProcessId::from_eterm(handle.pid().to_eterm()).expect("an internal pid immediate"),
            pid(41, 9)
        );
        assert!(claims.is_reserved(41));
        assert_eq!(claims.len(), 1);
        assert!(!claims.is_empty());
        assert_eq!(claims.pids().collect::<Vec<_>>(), vec![pid(41, 9)]);
        assert_eq!(
            claims.validate(handle).expect("the claim resolves"),
            pid(41, 9)
        );
    }

    #[test]
    fn an_empty_store_claims_nothing() {
        let claims = Reservations::new();
        assert_eq!(claims.len(), 0);
        assert!(claims.is_empty());
        assert!(!claims.is_reserved(0));
        assert_eq!(claims.pids().count(), 0);
        // `Default` is the same store.
        assert_eq!(Reservations::default().len(), 0);
    }

    #[test]
    fn a_second_claim_of_the_same_number_is_badarg() {
        let mut claims = Reservations::new();
        let _first = claims.claim(pid(5, 1)).expect("a fresh number");
        // The same identity, and the same number with a different serial, are both refused: one
        // slot cannot hold two claims.
        assert_badarg(claims.claim(pid(5, 1)));
        assert_badarg(claims.claim(pid(5, 2)));
        assert_eq!(claims.len(), 1, "the refused claims recorded nothing");
    }

    #[test]
    fn an_out_of_range_identity_is_system_limit() {
        let mut claims = Reservations::new();
        assert_system_limit(claims.claim(pid(MAX_PROCESS_NUMBER + 1, 0)));
        assert_system_limit(claims.claim(pid(0, MAX_PROCESS_SERIAL + 1)));
        assert!(
            claims.is_empty(),
            "a refused identity leaves no claim behind"
        );
        // The exact bounds are accepted.
        assert!(claims.claim(pid(MAX_PROCESS_NUMBER, 0)).is_ok());
        assert!(claims.claim(pid(0, MAX_PROCESS_SERIAL)).is_ok());
    }

    #[test]
    fn release_consumes_the_claim_exactly_once() {
        let mut claims = Reservations::new();
        let handle = claims.claim(pid(3, 3)).expect("a fresh number");
        assert_eq!(
            claims.release(handle).expect("the claim resolves"),
            pid(3, 3)
        );
        assert!(claims.is_empty());
        assert!(!claims.is_reserved(3));
        // The handle is stale now — a copied handle included.
        assert_badarg(claims.release(handle));
        assert_badarg(claims.release(handle));
        assert_badarg(claims.validate(handle));
        // And the number may be claimed again.
        assert!(claims.claim(pid(3, 4)).is_ok());
    }

    #[test]
    fn a_foreign_handle_is_badarg_in_another_table() {
        let mut mine = Reservations::new();
        let mut theirs = Reservations::new();
        let handle = theirs.claim(pid(7, 0)).expect("a fresh number");
        assert_badarg(mine.validate(handle));
        assert_badarg(mine.release(handle));
        assert!(mine.is_empty());
        // The claim it really names is untouched.
        assert_eq!(
            theirs.release(handle).expect("the claim resolves"),
            pid(7, 0)
        );
        // … and a handle minted by this table is never valid in the other one either.
        let own = mine.claim(pid(7, 0)).expect("a fresh number");
        assert_badarg(theirs.release(own));
        assert_eq!(mine.release(own).expect("the claim resolves"), pid(7, 0));
    }

    #[test]
    fn a_fabricated_handle_is_badarg() {
        let mut claims = Reservations::new();
        let handle = claims.claim(pid(9, 0)).expect("a fresh number");
        let wrong_id = Reservation {
            id: handle.id() + 1,
            ..handle
        };
        let wrong_pid = Reservation {
            pid: pid(9, 5),
            ..handle
        };
        assert_badarg(claims.release(wrong_id));
        assert_badarg(claims.release(wrong_pid));
        assert_eq!(claims.len(), 1, "the fabricated handles released nothing");
        assert_eq!(
            claims.release(handle).expect("the claim resolves"),
            pid(9, 0)
        );
    }

    #[test]
    fn release_number_drops_the_claim_by_number_only_once() {
        let mut claims = Reservations::new();
        let handle = claims.claim(pid(11, 2)).expect("a fresh number");
        assert_eq!(claims.release_number(11), Some(pid(11, 2)));
        assert!(!claims.is_reserved(11));
        assert_eq!(claims.release_number(11), None, "already dropped");
        assert_badarg(claims.release(handle));
    }

    #[test]
    fn the_store_is_send_and_sync() {
        // The SMP `SharedProcessTable`'s `unsafe impl Send/Sync` relies on every field being
        // `Send + Sync`; this pins the new one.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Reservations>();
        assert_send_sync::<Reservation>();
    }

    #[test]
    fn an_unset_or_foreign_node_is_badarg() {
        let mut names = beam_term::prelude::AtomTable::new();
        let me = names.intern("me@host");
        let other = names.intern("other@host");
        assert_badarg(check_local_node(None, make_atom(me)));
        assert!(check_local_node(Some(me), make_atom(me)).is_ok());
        assert_badarg(check_local_node(Some(me), make_atom(other)));
        // A non-atom node word (outside the wire contract) is refused the same way.
        assert_badarg(check_local_node(
            Some(me),
            beam_term::prelude::make_small(1),
        ));
    }
}
