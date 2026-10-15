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

//! The **delivery** half of `prim_tty:tty_select/2` — the read-select a `prim_tty` reader arms
//! before it announces itself.
//!
//! [`super::PortRegistry`] owns the runtime's one poller, so this module — a child of the port
//! layer, exactly like [`super::watch`] — is where a registered descriptor's readiness enters the
//! runtime. The *answer surface* (`tty_select/2`'s resource check and its `ok`) stays in
//! [`crate::native::prim_tty`], which owns the caller's arguments.
//!
//! # The C ground truth (`erts/emulator/nifs/common/prim_tty_nif.c`)
//!
//! `tty_select_nif` (`:1244-1264`) is three statements:
//!
//! ```c
//! if (!enif_get_resource(env, argv[0], tty_rt, (void **)&tty))
//!     return enif_make_badarg(env);
//! enif_select(env, tty->ifd, ERL_NIF_SELECT_READ, tty, NULL, argv[1]);
//! enif_self(env, &tty->reader); enif_monitor_process(env, tty, &tty->reader, NULL);
//! return atom_ok;
//! ```
//!
//! Three things follow, and all three are reproduced here:
//!
//! * **the descriptor is fd 0.** `tty_create_nif` (`:1009-1013`) hardcodes `tty->ifd = 0` (it is
//!   only ever assigned `tty->ofd` in the `__WIN32__` arm), so a handle created from `stdout`
//!   *or* `stderr` selects on **stdin**. Nothing about the handle's own stream is consulted.
//! * **the recipient is the calling process.** `enif_select` is called with no `pid`/`msg_env`
//!   argument, so `enif_select_x` uses the env's own process (`env->proc->common.id`), and with no
//!   `ERL_NIF_SELECT_CUSTOM_MSG` the message is the 4-tuple `{select, Resource, Ref, EventAtom}`
//!   built by `prepare_select_msg` (`erl_check_io.c:731-761`) with `ready_input` as the event
//!   atom; `send_select_msg` (`:836-858`) queues it with `am_system` as the sender.
//! * **`enif_select`'s return value is discarded** — the NIF answers `ok` whether or not the
//!   registration was accepted, so a refused arming is *not* a raise.
//!
//! # The select is **one-shot**
//!
//! ERTS clears the interest as it queues the message: `erts_poll_wait`'s `ERTS_EV_TYPE_NIF` arm
//! sets `state->driver.nif->in.pid = NIL` and `state->events &= ~revents`
//! (`erl_check_io.c:2135-2151`), so the descriptor is polled for readiness exactly once per
//! `enif_select` call. This module reproduces that literally: [`TtyWatch::take_ready`]
//! **de-registers** the key as it reports it. `prim_tty:reader/1`
//! (`lib/kernel/src/prim_tty.erl:440-452`) calls `tty_select/2` **once** for its whole lifetime, so
//! a level-triggered re-report would make an always-readable descriptor (stdin on `/dev/null`,
//! where the differential harness runs every case) wake the reader on every poll round for ever.
//!
//! # The reader dying disarms the select
//!
//! `tty_select_nif` also monitors the reader (`enif_monitor_process`) and its
//! `tty_monitor_down` callback (`:1266-1276`) does `enif_select(..., ERL_NIF_SELECT_STOP, ...)`.
//! [`TtyWatch::prune_dead`] is that arm: a registration whose owner is no longer in the live
//! process table is dropped and de-registered, so a dead reader can neither keep the runtime
//! polling for ever nor accumulate an undeliverable message per round.

use std::collections::HashMap;
use std::os::fd::RawFd;

use beam_common::Result;
use beam_io::{Interest, Poller, Token};
use beam_process::ProcessId;
use beam_term::Eterm;

use crate::error;

/// The descriptor `tty_select/2` arms on — the C's hardcoded `tty->ifd = 0` (stdin), for a handle
/// built from `stdout` exactly as for one built from `stderr`.
pub(crate) const TTY_IFD: RawFd = 0;

/// The highest poller key of the tty-select reservation; keys are handed out **downwards** from
/// here.
///
/// It must sit below [`super::watch::SIGNAL_KEY`] (`u64::MAX - 1`) and the poller's own
/// [`beam_io::WAKER_KEY`] (`u64::MAX`), and the whole reservation must stay above the port-number
/// space (the port table only hands out numbers below `1 << 60`) — which [`TTY_KEY_FLOOR`] pins.
const TTY_KEY_TOP: u64 = u64::MAX - 2;

/// The lowest key the reservation may use. A registry that somehow ran through this many
/// concurrent registrations reports the typed `system_limit` rather than wrapping into the port
/// number space (where a tty event would be mistaken for a port's).
const TTY_KEY_FLOOR: u64 = 1 << 60;

/// One armed read interest: the descriptor, the process the select message belongs to, and the two
/// terms that message must carry.
///
/// `handle`/`reference` are **stabilised** terms (copied onto the runtime's never-reset arena by
/// [`crate::Runtime::stabilize_term`] before they are stored): a raw `Eterm` held across a
/// scheduler pass belongs to a heap a collection may relocate, so it is never stored here.
#[derive(Debug)]
struct TtyRegistration {
    /// The poller token, needed to de-register.
    token: Token,
    /// The descriptor `token` names (always [`TTY_IFD`]; carried so the de-registration is
    /// self-contained).
    fd: RawFd,
    /// The calling process the select message is addressed to (`enif_select`'s implicit recipient).
    owner: ProcessId,
    /// The opaque tty handle (`argv[0]`), as `prepare_select_msg` puts it in the tuple.
    handle: Eterm,
    /// `argv[1]` — `prim_tty:reader/1`'s `ReaderRef`, which the reader matches the message on.
    reference: Eterm,
}

/// The runtime's registrations of `prim_tty:tty_select/2` reads with its one port poller.
///
/// Owned by [`super::PortRegistry`] (the poller's owner) rather than by the NIF floor, because a
/// poller token is only meaningful for the poll set it was minted from.
#[derive(Debug)]
pub(crate) struct TtyWatch {
    /// The armed registrations, keyed by poller key.
    registrations: HashMap<u64, TtyRegistration>,
    /// The next key to hand out (counts down from [`TTY_KEY_TOP`]).
    next_key: u64,
}

impl TtyWatch {
    /// A watch with nothing armed.
    pub(crate) fn new() -> TtyWatch {
        TtyWatch {
            registrations: HashMap::new(),
            next_key: TTY_KEY_TOP,
        }
    }

    /// Whether any read interest is armed — the "something for the poll round to wait on" half
    /// [`super::PortRegistry::has_nothing_to_poll`] needs, alongside the live ports and the signal
    /// self-pipe.
    pub(crate) fn is_armed(&self) -> bool {
        !self.registrations.is_empty()
    }

    /// Whether `key` names one of this watch's registrations — the filter that keeps a tty event
    /// out of the port scheduler's readiness batch.
    pub(crate) fn owns(&self, key: u64) -> bool {
        self.registrations.contains_key(&key)
    }

    /// Arms a one-shot read interest on [`TTY_IFD`] for `owner` — the C's `enif_select` call.
    ///
    /// # Errors
    ///
    /// The poller's own registration failure (the OS refuses the descriptor — fd 0 closed, say) or
    /// the typed `system_limit` when the key reservation is exhausted. The C **discards**
    /// `enif_select`'s answer, so the caller of `tty_select/2` maps a failure to "nothing armed"
    /// and still answers `ok` (see [`crate::native::prim_tty`]).
    pub(crate) fn arm(
        &mut self,
        poller: &mut Poller,
        owner: ProcessId,
        handle: Eterm,
        reference: Eterm,
    ) -> Result<()> {
        if self.next_key <= TTY_KEY_FLOOR {
            return Err(error::unsupported(
                "prim_tty: the tty-select poller key reservation is exhausted",
            ));
        }
        let key = self.next_key;
        let token = poller.register(TTY_IFD, Interest::READABLE, key)?;
        self.next_key = key - 1;
        self.registrations.insert(
            key,
            TtyRegistration {
                token,
                fd: TTY_IFD,
                owner,
                handle,
                reference,
            },
        );
        Ok(())
    }

    /// The registrations whose poller keys are in `keys`, **disarming** each — the C's one-shot
    /// delivery (`erl_check_io.c:2135-2151` clears the interest as the message is queued).
    ///
    /// A key this watch does not own is skipped: a poll round's batch also carries live ports' and
    /// the signal self-pipe's events.
    ///
    /// # Errors
    ///
    /// The poller's own de-registration failure.
    pub(crate) fn take_ready(
        &mut self,
        poller: &mut Poller,
        keys: &[u64],
    ) -> Result<Vec<(ProcessId, Eterm, Eterm)>> {
        let mut delivered = Vec::new();
        for key in keys {
            let Some(registration) = self.registrations.remove(key) else {
                continue;
            };
            poller.deregister(registration.fd, registration.token)?;
            delivered.push((registration.owner, registration.handle, registration.reference));
        }
        Ok(delivered)
    }

    /// Drops (and de-registers) every registration whose owner `live` denies — the C's
    /// `tty_monitor_down` `ERL_NIF_SELECT_STOP` arm.
    ///
    /// # Errors
    ///
    /// The poller's own de-registration failure.
    pub(crate) fn prune_dead(
        &mut self,
        poller: &mut Poller,
        mut live: impl FnMut(ProcessId) -> bool,
    ) -> Result<()> {
        let dead: Vec<u64> = self
            .registrations
            .iter()
            .filter(|(_, registration)| !live(registration.owner))
            .map(|(key, _)| *key)
            .collect();
        for key in dead {
            if let Some(registration) = self.registrations.remove(&key) {
                poller.deregister(registration.fd, registration.token)?;
            }
        }
        Ok(())
    }
}

impl Default for TtyWatch {
    fn default() -> TtyWatch {
        TtyWatch::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::{make_small, Eterm};
    use std::collections::HashSet;

    /// A poller and a watch with one registration armed, plus the terms it carries.
    fn armed() -> (Poller, TtyWatch, ProcessId, Eterm, Eterm) {
        let mut poller = Poller::new().expect("a poller");
        let mut watch = TtyWatch::new();
        let owner = ProcessId::new(7, 0);
        let handle = make_small(11);
        let reference = make_small(22);
        watch
            .arm(&mut poller, owner, handle, reference)
            .expect("fd 0 is registrable in a test process");
        (poller, watch, owner, handle, reference)
    }

    #[test]
    fn a_fresh_watch_is_not_armed_and_owns_nothing() {
        let watch = TtyWatch::new();
        assert!(!watch.is_armed());
        assert!(!watch.owns(0));
        assert!(!watch.owns(TTY_KEY_TOP));
    }

    #[test]
    fn arming_reserves_a_key_and_owns_it() {
        let (_poller, watch, ..) = armed();
        assert!(watch.is_armed());
        assert!(watch.owns(TTY_KEY_TOP), "the first key is the top of the range");
        // The reservation stays outside the port-number space and below the signal sentinel.
        assert!(TTY_KEY_TOP < super::super::watch::SIGNAL_KEY);
        assert!(TTY_KEY_FLOOR > 1 << 59, "keys never wrap into the port-number space");
    }

    #[test]
    fn a_readiness_report_is_delivered_exactly_once() {
        let (mut poller, mut watch, owner, handle, reference) = armed();
        let delivered = watch
            .take_ready(&mut poller, &[TTY_KEY_TOP])
            .expect("a delivery");
        assert_eq!(delivered.len(), 1);
        let (delivered_owner, delivered_handle, delivered_reference) = delivered[0];
        assert_eq!(delivered_owner, owner);
        assert_eq!(delivered_handle, handle);
        assert_eq!(delivered_reference, reference);
        // One-shot: the interest is disarmed, so a second round reports nothing.
        assert!(!watch.is_armed());
        assert!(watch
            .take_ready(&mut poller, &[TTY_KEY_TOP])
            .expect("nothing armed")
            .is_empty());
    }

    #[test]
    fn a_key_the_watch_does_not_own_is_ignored() {
        let (mut poller, mut watch, ..) = armed();
        assert!(watch
            .take_ready(&mut poller, &[42])
            .expect("a foreign key is skipped")
            .is_empty());
        assert!(watch.is_armed(), "the armed registration is untouched");
    }

    #[test]
    fn a_dead_owner_is_pruned_and_a_live_one_is_kept() {
        let (mut poller, mut watch, owner, ..) = armed();
        let live: HashSet<ProcessId> = HashSet::new();
        watch
            .prune_dead(&mut poller, |pid| live.contains(&pid))
            .expect("a prune");
        assert!(!watch.is_armed(), "{owner:?} is gone, so the select is disarmed");

        let (mut poller, mut watch, owner, ..) = armed();
        let mut live = HashSet::new();
        live.insert(owner);
        watch
            .prune_dead(&mut poller, |pid| live.contains(&pid))
            .expect("a prune");
        assert!(watch.is_armed(), "a live owner keeps its registration");
    }
}
