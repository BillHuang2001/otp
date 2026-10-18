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

//! The **driver-failure channel**: the C's `driver_failure*` family and `driver_exit`, as the
//! port-closing signal a driver hands up.
//!
//! # Why this is a channel of its own
//!
//! The C driver API lets a driver *close its own port* from inside a callback, with a reason the
//! port's owner observes as a `{'EXIT', Port, Reason}` signal:
//!
//! | the driver calls | the C runs | the reason |
//! |---|---|---|
//! | `driver_failure_eof(port)` | `driver_failure_term(ix, NIL, 1)` (`io.c:7278`) | `normal`, after flushing a `{line,N}` port's buffer |
//! | `driver_failure_atom(port, "enospc")` | `driver_failure_term(ix, <atom>, 0)` (`io.c:7263`) | `enospc` |
//! | `driver_failure_posix(port, EINVAL)` | `driver_failure_atom(ix, erl_errno_id(EINVAL))` (`io.c:7273`) | `einval` |
//! | `driver_failure(port, 42)` | `driver_failure_term(ix, make_small(42), 0)` (`io.c:7258`) | `42` |
//! | `driver_failure(port, 0)` | `driver_failure_term(ix, make_small(0), 1)` — the `eof` flag is `code == 0` | `normal` |
//! | `driver_exit(port, code)` | `unlink_proc` then `driver_failure_term(...)` (`io.c:7234-7255`) | `normal` (`code == 0`) or the errno atom — and **no** message, because the C unlinked the process first |
//!
//! So a failure is not a return value and not a message to the owner: it is a **port-closing
//! request**, which is why it rides this fourth side channel rather than
//! [`PortNotification`](crate::port_task::PortNotification) (that enum stays exactly two variants)
//! or [`PortCompletion`](crate::completion::PortCompletion) (a *message* the owner receives).
//! A driver *latches* one on its [`DriverContext`](crate::driver::DriverContext)
//! ([`DriverContext::signal_failure`](crate::driver::DriverContext::signal_failure)); the port
//! collects it ([`Port::take_failure`](crate::port::Port::take_failure)) and the registry closes
//! the port with its teardown path
//! ([`PortRegistry::reap_failures`](crate::registry::PortRegistry::reap_failures)) before handing
//! the reasons to the runtime ([`PortRegistry::take_failures`](crate::registry::PortRegistry::take_failures)).
//!
//! # Term-free by construction
//!
//! This crate owns no terms, so a reason is carried as the **vocabulary** the C expresses it in: an
//! errno ([`FailureReason::Posix`], whose atom is the crate's single errno→atom rule), a driver
//! atom name ([`FailureReason::Atom`]), an integer ([`FailureReason::Code`]) or the `normal`/eof
//! shape ([`FailureReason::Eof`]/[`FailureReason::Normal`]). The runtime builds the actual signal.
//!
//! # Who the failure addresses
//!
//! [`DriverFailure::signals_owner`] carries the one distinction the C's two entry points make: the
//! `driver_failure*` family delivers a port-exit signal to the connected process (so a trapping
//! owner receives `{'EXIT', Port, Reason}`), while `driver_exit` unlinks that process **before**
//! closing the port, so its owner receives nothing at all. A runtime that rendered an `'EXIT'` for
//! a `driver_exit` would invent a message the oracle never sends, which is why the flag is part of
//! the value rather than inferred from the reason.

use beam_process::PortId;

use crate::completion::InetReason;

/// *Why* a driver closed its own port — the C's whole `driver_failure*` family, and the two shapes
/// `driver_exit` reduces to.
///
/// The variants are deliberately distinct where the C's behaviour differs, even when the rendered
/// reason is the same: [`FailureReason::Eof`] and [`FailureReason::Normal`] both become `normal`,
/// but only the eof shape runs the C's line-buffer flush before the exit signal.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FailureReason {
    /// `driver_failure_eof` / `driver_failure(port, 0)`: the stream ended. The C's `eof` flag
    /// (`driver_failure_term(ix, term, eof)`, `io.c:7196`), which flushes a `{line,N}` port's
    /// partial line (`flush_linebuf_messages`, `io.c:7211`) and renders `normal`.
    Eof,
    /// `driver_exit(port, 0)`: `driver_failure_term(ix, am_normal, 0)` — the `normal` reason with
    /// the eof flag **clear**, so no line-buffer flush happens (`io.c:7246-7247`).
    Normal,
    /// `driver_failure_atom(port, "enospc")`: the driver's own reason atom, carried by name.
    Atom(String),
    /// `driver_failure_posix(port, errno)`: the errno's POSIX name — the C's
    /// `driver_failure_atom(ix, erl_errno_id(err))` (`io.c:7273`).
    Posix(i32),
    /// `driver_failure(port, code)`: the raw integer the driver asked for
    /// (`driver_failure_term(ix, make_small(code), code == 0)`, `io.c:7258`), rendered as that
    /// integer.
    Code(i32),
}

impl FailureReason {
    /// The **reason atom** this failure becomes in Erlang, when it is an atom at all.
    ///
    /// `Eof`/`Normal` are `normal` (the C's `eof ? am_normal : term`), an [`Atom`][Self::Atom] is
    /// its own name, and a [`Posix`][Self::Posix] errno is the lowercase POSIX name from the
    /// crate's single errno→atom rule
    /// ([`InetReason::atom_name`](crate::completion::InetReason::atom_name), the C's
    /// `erl_errno_id` with its `errno_<N>` fallback). A [`Code`][Self::Code] answers `None`: the
    /// oracle sends the **integer**, not an atom.
    pub fn atom_name(&self) -> Option<String> {
        match self {
            FailureReason::Eof | FailureReason::Normal => Some("normal".to_string()),
            FailureReason::Atom(name) => Some(name.clone()),
            FailureReason::Posix(errno) => Some(InetReason::from_errno(*errno).atom_name()),
            FailureReason::Code(_) => None,
        }
    }

    /// The **integer** this failure becomes in Erlang, for the one variant that is an integer.
    pub fn code(&self) -> Option<i32> {
        match self {
            FailureReason::Code(code) => Some(*code),
            _ => None,
        }
    }

    /// Whether this is the C's `eof` shape — the flag that makes the port flush a `{line, N}`
    /// buffer before delivering the exit signal (`io.c:7211`).
    pub fn is_eof(&self) -> bool {
        matches!(self, FailureReason::Eof)
    }
}

/// A driver's request to close its own port: the reason, plus the C's process-unlink distinction.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DriverFailure {
    /// Why the driver closed the port.
    pub reason: FailureReason,
    /// Whether the C delivers a port-exit signal to the port's owner.
    ///
    /// `true` for the `driver_failure*` family (`erts_deliver_port_exit`, `io.c:7221`, which the
    /// owner observes as `{'EXIT', Port, Reason}` while it traps exits), `false` for `driver_exit`
    /// — that entry point runs `unlink_proc(prt, connected)` **first** (`io.c:7244`), so the exit
    /// signal reaches no one.
    pub signals_owner: bool,
}

impl DriverFailure {
    /// `driver_failure_eof(port)` (and `driver_failure(port, 0)`): the eof shape, reason `normal`,
    /// line buffer flushed.
    pub fn eof() -> DriverFailure {
        DriverFailure {
            reason: FailureReason::Eof,
            signals_owner: true,
        }
    }

    /// `driver_failure_atom(port, name)`: the driver's own reason atom.
    pub fn atom(name: impl Into<String>) -> DriverFailure {
        DriverFailure {
            reason: FailureReason::Atom(name.into()),
            signals_owner: true,
        }
    }

    /// `driver_failure_posix(port, errno)`: the errno's POSIX name as the reason atom.
    pub fn posix(errno: i32) -> DriverFailure {
        DriverFailure {
            reason: FailureReason::Posix(errno),
            signals_owner: true,
        }
    }

    /// `driver_failure(port, code)`: the integer reason, or — for the C's `code == 0`, which is its
    /// `eof` flag (`driver_failure_term(ix, make_small(code), code == 0)`, `io.c:7258`) — the eof
    /// shape.
    pub fn error(code: i32) -> DriverFailure {
        if code == 0 {
            return DriverFailure::eof();
        }
        DriverFailure {
            reason: FailureReason::Code(code),
            signals_owner: true,
        }
    }

    /// `driver_exit(port, code)`: the process is unlinked first, so **no** signal is delivered
    /// (`signals_owner == false`), and the reason is `normal` for `code == 0` (with the eof flag
    /// clear) or the errno's name otherwise (`io.c:7244-7255`).
    pub fn exit(code: i32) -> DriverFailure {
        let reason = if code == 0 {
            FailureReason::Normal
        } else {
            FailureReason::Posix(code)
        };
        DriverFailure {
            reason,
            signals_owner: false,
        }
    }

    /// Whether this is the C's `eof` shape (see [`FailureReason::is_eof`]).
    pub fn is_eof(&self) -> bool {
        self.reason.is_eof()
    }
}

/// A port a driver closed, reported to the runtime after the round that observed it — the
/// failure-channel counterpart of [`PortCompletion`](crate::completion::PortCompletion).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PortFailure {
    /// The port that was closed (the C's `prt->common.id`).
    pub port: PortId,
    /// What the driver reported.
    pub failure: DriverFailure,
}

impl PortFailure {
    /// Binds `failure` to `port`.
    pub const fn new(port: PortId, failure: DriverFailure) -> PortFailure {
        PortFailure { port, failure }
    }
}

#[cfg(test)]
#[path = "port_failure_tests.rs"]
mod port_failure_tests;
