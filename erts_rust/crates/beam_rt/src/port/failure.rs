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

//! The **driver-failure channel**'s runtime half: `beam_io`'s term-free
//! [`PortFailure`] turned into the `'EXIT'`/`'DOWN'` messages a port that a **driver closed
//! itself** owes its owner and its watchers.
//!
//! # What the C does, and what this half therefore has to
//!
//! A driver closes its own port through the `driver_failure*` family or `driver_exit`
//! (`io.c:7196-7278`). That is not a return value and not a port message: it is a
//! **port-closing request**, so `beam_io` latches it from inside the callback
//! ([`DriverContext::signal_failure`](beam_io::DriverContext::signal_failure)), runs the ordinary
//! teardown (`reap_failures` → `PortRegistry::close`, so the driver's `stop` runs, the port
//! leaves the live map and its `PortNotification::Closed` report is minted) and queues the reason
//! ([`PortRegistry::take_failures`](beam_io::PortRegistry::take_failures)). This module is the
//! half `beam_io` cannot do — it owns no terms and no process table.
//!
//! The **reason** each message renders is [`FailureReason::atom_name`] when that is `Some`
//! (`eof`/`normal` ⇒ `normal`, a driver atom ⇒ its name, an errno ⇒ its POSIX name) and the
//! integer [`FailureReason::code`] when it is `None` (`driver_failure(P, 7)` ⇒ `7`).
//!
//! The **addressee** is [`DriverFailure::signals_owner`]: the `driver_failure*` family delivers
//! a port-exit signal to the connected process, while `driver_exit` runs `unlink_proc` first
//! (`io.c:7244`), so no `'EXIT'` is owed at all — but every **monitor** still gets its `'DOWN'`,
//! because the C's `erts_deliver_port_exit(prt, id, reason, 0, 0)` is one path for links and
//! monitors alike.
//!
//! Measured against the oracle (OTP 29.1.1 / erts-17.1), one fresh port per row:
//!
//! | the driver calls | trapping owner | non-trapping owner | monitor |
//! |---|---|---|---|
//! | `driver_failure_eof(P)` | `{'EXIT',P,normal}` | *nothing*, survives | `{'DOWN',…,normal}` |
//! | `driver_failure_atom(P,"boom")` | `{'EXIT',P,boom}` | **terminated with `boom`** | `{'DOWN',…,boom}` |
//! | `driver_failure_posix(P,EACCES)` | `{'EXIT',P,eacces}` | terminated with `eacces` | `{'DOWN',…,eacces}` |
//! | `driver_failure(P,7)` | `{'EXIT',P,7}` | terminated with `7` | `{'DOWN',…,7}` |
//! | `driver_failure(P,0)` | `{'EXIT',P,normal}` | *nothing*, survives | `{'DOWN',…,normal}` |
//! | `driver_exit(P,0)` | **no `'EXIT'`** | *nothing*, survives | `{'DOWN',…,normal}` |
//! | `driver_exit(P,EACCES)` | **no `'EXIT'`** | *nothing*, survives | `{'DOWN',…,eacces}` |
//!
//! After every one of those rows `erlang:port_info(P)` answers `undefined` and
//! `erlang:port_close(P)` raises `error:badarg` — this module removes the port from the runtime's
//! own metadata, which is what both of those read.
//!
//! # One message per failure, never two
//!
//! The `PortNotification::Closed` report a reaped failure also produces is the port-**number**
//! retirement report, not a second observable. It reaches
//! [`PendingPortSignals::report`](super::exit::PendingPortSignals) through
//! [`PortRegistry::fold_notifications`](super::PortRegistry) and finds nothing to release, because
//! this half publishes straight into the deliverable queue
//! ([`PortRegistry::push_port_signals`](super::PortRegistry)): by the time `take_failures` answers,
//! `beam_io` has already closed **and** reported the port, so there is no second stage to wait
//! for. The bytes a failing callback delivered through `driver_output*` before it failed still
//! reach the owner (`beam_io` re-prepends them: `take_reaped_notifications`), so
//! `{Port,{data,…}}` **then** `{'EXIT',P,Reason}` both appear, once each.
//!
//! # Why the teardown is shared with `close` rather than copied
//!
//! [`PortRegistry::fail_port`] is [`PortRegistry::close`](super::PortRegistry)'s bookkeeping
//! **without** the two steps `beam_io` has already performed: it does not call
//! `registry.close` (the port is gone) and does not call `notify_port_exit` (the driver already
//! saw its port exit — that is what a failure *is*). Everything else — the
//! [`ClosedPortSnapshot`](super::completion::ClosedPortSnapshot) a completion may still need, the
//! exit-signal snapshot, the metadata/extras removal, the purge of the port's retained events and
//! the completion drain — runs in the same order, at the same place, through the same helpers, so
//! the two closers cannot drift.

use beam_io::{DriverFailure, FailureReason, InetReason, PortFailure};
use beam_process::PortId;

use super::exit::PortSignalReason;
use super::PortRegistry;

/// The [`PortSignalReason`] a driver-declared failure becomes.
///
/// [`FailureReason::atom_name`] decides it whenever it answers `Some`; a
/// [`Code`](FailureReason::Code) has no atom and keeps its integer, which is what the oracle
/// sends. An [`Eof`](FailureReason::Eof)/[`Normal`](FailureReason::Normal) shape maps to
/// [`PortSignalReason::Normal`] rather than `Atom("normal")`, so the C's link-family policy
/// (`normal` is ignored by a non-trapping peer instead of killing it) applies to it.
pub(crate) fn signal_reason(reason: &FailureReason) -> PortSignalReason {
    match reason {
        FailureReason::Eof | FailureReason::Normal => PortSignalReason::Normal,
        FailureReason::Atom(name) => PortSignalReason::Atom(name.clone()),
        // The errno's POSIX name, from the crate's single errno→atom rule (`erl_errno_id`'s
        // lowercase name, with its `errno_<N>` fallback).
        FailureReason::Posix(errno) => PortSignalReason::Atom(InetReason::from_errno(*errno).atom_name()),
        FailureReason::Code(code) => PortSignalReason::Integer(*code),
    }
}

impl PortRegistry {
    /// Takes every port a driver closed through its own callback and finishes the teardown the
    /// runtime owes it, publishing the `'EXIT'`/`'DOWN'` messages it owes.
    ///
    /// Drained at **every** site a driver callback can run, exactly as the completion channel is
    /// (see [`super::completion`]): `beam_io`'s own sweep has already closed the port and minted
    /// its report before any of them returns, so a drain is always safe and never sees a live
    /// port.
    ///
    /// Infallible: a failure owes at most two messages, and building the signals cannot fail.
    /// Everything that *can* fail about a closing port (a completion's rendering, a recipient's
    /// own delivery) is reported at that later step, never swallowed here.
    pub(super) fn drain_failures(&mut self) {
        for PortFailure { port, failure } in self.registry.take_failures() {
            self.fail_port(port, failure);
        }
    }

    /// Closes the runtime's side of a port a driver already closed: the exact bookkeeping of
    /// [`PortRegistry::close`](super::PortRegistry), minus the two steps `beam_io` has done.
    fn fail_port(&mut self, id: PortId, failure: DriverFailure) {
        // A completion staged around the teardown still needs its recipient, and the metadata is
        // about to be removed (see [`super::completion::ClosedPortSnapshot`]).
        let snapshot = self.closed_snapshot(id);
        // The signals are read **before** the metadata goes: the link set and the incoming
        // monitors are the only sources of the recipients.
        let signals = self.port_exit_signals(
            id,
            signal_reason(&failure.reason),
            failure.signals_owner,
        );
        self.meta.remove(&id.number());
        // A failed port is `undefined` for `whereis/1`, `badarg` for `port_get_data/1`, and a
        // recycled number starts clean (see `ops::PortExtras`).
        self.extras.forget_port(id.number());
        // Bytes a round harvested for this port but no pump has reported yet are dropped — but a
        // failing callback's own `driver_output*` bytes were re-harvested by `beam_io` with the
        // report, so they are still delivered (once) after this purge.
        self.pending_events
            .retain(|event| event.port() != Some(id));
        self.push_port_signals(signals);
        let completions = self.drain_io_completions(snapshot);
        self.pending_events.extend(completions);
    }
}

#[cfg(test)]
#[path = "failure_tests.rs"]
mod failure_tests;
