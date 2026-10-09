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

//! `pub(crate)` error constructors — the crate adds no error type of its own.
//!
//! Every fallible path in this crate returns the workspace error type
//! ([`BeamError`](beam_common::BeamError)); this module only centralises the construction
//! of the failures the process/scheduler layer can raise, exactly as `beam_gc::error` and
//! `beam_message::error` do for their crates. The mapping is fixed by the crate
//! `CONTEXT.md`:
//!
//! * an unknown pid/name/monitor reference ⇒ [`unknown_process`] (`TermNotFound`),
//! * a malformed pid/port/name term, an out-of-range register index, an **already-taken**
//!   local name ([`name_taken`]) or a `register/2` of a pid that is not a live local process
//!   ([`bad_pid`]) ⇒ `badarg`, matching `erlang:register/2`/`erlang:unregister/1` (both raise
//!   `badarg`; an unregister of an unregistered name is `badarg`, **not** `TermNotFound`),
//! * pid/port number-space exhaustion ⇒ [`system_limit`] (`SystemLimit`),
//! * a named **port** monitor (`erlang:monitor(port, Name)`, whose name the local
//!   process-only registry cannot resolve) ⇒ [`named_port_monitor`],
//! * an `erlang:monitor/3` `{tag, Tag}` whose `Tag` is not a heap-independent immediate ⇒
//!   [`unsupported_tag`],
//! * a genuinely unimplemented capability ⇒ [`unsupported`] (a BEAM `error` raise), so a caller
//!   never receives a wrong result in place of a typed diagnostic,
//! * a collection / nursery growth refused because the process is **relocation-exempt** ⇒
//!   [`relocation_exempt`] (also a BEAM `error` raise).
//!
//! Heap exhaustion during delivery is *not* modelled here: it is the underlying
//! `BeamError::Alloc` propagated after a collect-and-retry (see `Process::deliver`).

use beam_common::{BeamError, ExitReason, Raise};

/// The error for a lookup that found no process: an unknown pid, an unregistered name or
/// a dangling monitor reference (`TermNotFound`, which has no BEAM error atom).
pub(crate) fn unknown_process() -> BeamError {
    BeamError::TermNotFound
}

/// The error for a malformed pid/port term, an out-of-range register index, or a pid that is
/// not a live local process (`badarg`).
pub(crate) fn bad_pid() -> BeamError {
    BeamError::BadArg
}

/// The error for a local name that is already registered (`badarg`).
pub(crate) fn name_taken() -> BeamError {
    BeamError::BadArg
}

/// The error raised when the pid or port number space is exhausted (`system_limit`).
pub(crate) fn system_limit() -> BeamError {
    BeamError::SystemLimit
}

/// The typed "not implemented" diagnostic: a BEAM `error` whose reason is the atom
/// `unsupported: <what>`.
///
/// This is the crate's only way to report a genuine capability gap. It is deliberately a
/// [`BeamError::Raise`] rather than a silent default, so a caller always gets a typed error
/// instead of a **wrong result** (the workspace §3 error strategy); the reason string is the
/// single place the `unsupported:` prefix is spelled.
pub(crate) fn unsupported(what: &str) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(format!("unsupported: {what}"))))
}

/// The typed diagnostic for a [`Process::collect`](crate::process::Process::collect) /
/// [`Process::ensure_capacity`](crate::process::Process::ensure_capacity) that was **refused**
/// because the process is **relocation-exempt**.
///
/// A relocation-exempt process's heap must not be relocated: a host holds live terms from it in
/// Rust locals and nested-`Machine` registers that are outside the crate's root set (see
/// [`Process::set_relocation_exempt`](crate::process::Process::set_relocation_exempt)), so a
/// collection against the incomplete root set would recycle live words. There is **no**
/// `CollectionKind::None`, so the refusal cannot be represented as a fake
/// [`Collection`](beam_gc::Collection) — it is raised as this typed `error` instead, exactly as
/// [`unsupported`] reports a capability gap rather than a silent default. The no-op sibling
/// [`Process::collect_if_needed`](crate::process::Process::collect_if_needed) answers `Ok(false)`
/// directly (its return value already means "no collection ran") and never reaches here.
pub(crate) fn relocation_exempt(what: &str) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(format!(
        "relocation_exempt: {what}"
    ))))
}

/// The typed diagnostic for an `erlang:monitor(port, Name)` on a **registered name**
/// (a named port monitor).
///
/// The oracle resolves a name to a process *or* a port, but the local registry
/// (`ProcessTable`'s `names`) holds [`ProcessId`](crate::pid::ProcessId)s only, so a named
/// **port** monitor cannot be resolved faithfully — treating the name as a *process* name
/// (or as an unregistered name) would deliver a wrong `{'DOWN', …}`. Both spellings
/// (`ProcessTable::monitor_term` and `ProcessTable::monitor_named`) share this one reason.
pub(crate) fn named_port_monitor() -> BeamError {
    unsupported("monitor/2: named port monitor (the local name registry holds processes only)")
}

/// The typed diagnostic for an `erlang:monitor/3` `{tag, Tag}` whose `Tag` is **not** a
/// heap-independent immediate.
///
/// A monitor record is a **plain value** the collector never scans (see `monitor.rs`), so a tag
/// term resident in a per-process heap could not be relocated and would dangle after a
/// collection. Only an immediate (an atom, small integer, local pid/port, `nil`) — or a
/// literal-area term — is safe; every other term is refused here rather than silently stored as
/// a word that a later collection invalidates.
pub(crate) fn unsupported_tag() -> BeamError {
    unsupported("monitor/3: tag must be an immediately-representable term (atom/integer/pid/port)")
}
