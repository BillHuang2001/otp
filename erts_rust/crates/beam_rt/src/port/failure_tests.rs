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

//! The A.4 **driver-failure channel** suite.
//!
//! # The oracle rows this pins
//!
//! Measured on OTP 29.1.1 / erts-17.1 with a real C driver compiled at probe time and one fresh
//! port per row a trapping owner (`process_flag(trap_exit,true)`), plus a non-trapping owner and
//! a `monitor(port, P)` watcher:
//!
//! | `port_command` opcode | the driver calls | trapping owner sees | non-trapping owner | watcher |
//! |---|---|---|---|---|
//! | 1 | `driver_failure_eof(P)` | `{'EXIT',P,normal}` | nothing, survives | `{'DOWN',…,normal}` |
//! | 2 | `driver_failure_atom(P,"boom")` | `{'EXIT',P,boom}` | killed with `boom` | `{'DOWN',…,boom}` |
//! | 3 | `driver_failure_posix(P,EACCES)` | `{'EXIT',P,eacces}` | killed with `eacces` | `{'DOWN',…,eacces}` |
//! | 4 | `driver_failure(P,7)` | `{'EXIT',P,7}` | killed with `7` | `{'DOWN',…,7}` |
//! | 6 | `driver_exit(P,0)` | **no `'EXIT'`** | nothing, survives | `{'DOWN',…,normal}` |
//! | 7 | `driver_exit(P,EACCES)` | **no `'EXIT'`** | nothing, survives | `{'DOWN',…,eacces}` |
//!
//! and after every row `erlang:port_info(P)` is `undefined` and `erlang:port_close(P)` raises
//! `error:badarg`.
//!
//! # What is pinned here
//!
//! The **reason mapping** every one of those rows goes through
//! ([`signal_reason`](super::signal_reason)) and the queue state that keeps a failed port's owed
//! messages from letting the pump declare quiescence. The end-to-end route — a real `.so` whose
//! `output` callback calls `driver_failure_atom` through the address handed to it in the loader's
//! command string — is a follow-up for this node (see `src/port/CONTEXT.md` *Known Issues*): it
//! needs the driver symbols the `--lib` link does not export, and the fixture that exercises them
//! lives in `beam_abi`'s own suite (`tests/fixtures/driver_failure.c`), which pins the
//! `PortFailure` each spelling reaches the host with.

use beam_io::{DriverFailure, FailureReason};
use beam_process::PortId;

use super::signal_reason;
use crate::port::exit::{PortSignal, PortSignalKind, PortSignalReason};
use crate::port::PortRegistry;

/// The reason a failure renders, spelled out for every variant.
#[test]
fn every_failure_reason_maps_to_the_oracles_reason() {
    // `driver_failure_eof(P)` and `driver_failure(P,0)`: both are the C's eof shape, `normal`.
    assert_eq!(signal_reason(&FailureReason::Eof), PortSignalReason::Normal);
    // `driver_exit(P,0)` — `normal` with the eof flag clear, but the same rendered reason.
    assert_eq!(
        signal_reason(&FailureReason::Normal),
        PortSignalReason::Normal
    );
    // `driver_failure_atom(P,"boom")` — the driver's own atom.
    assert_eq!(
        signal_reason(&FailureReason::Atom("boom".to_string())),
        PortSignalReason::Atom("boom".to_string())
    );
    // `driver_failure_posix(P,EACCES)` — the errno's lowercase POSIX name, never `{error, ·}`.
    assert_eq!(
        signal_reason(&FailureReason::Posix(libc::EACCES)),
        PortSignalReason::Atom("eacces".to_string())
    );
    // `driver_failure(P,7)` — the bare integer, not an atom.
    assert_eq!(
        signal_reason(&FailureReason::Code(7)),
        PortSignalReason::Integer(7)
    );
}

/// The two driver entry points' own constructors reach the mapped rows above — so the `atom_name`
/// / `code` accessors and this mapping agree on what the C's five spellings are.
#[test]
fn the_driver_entry_points_construct_the_mapped_reasons() {
    assert_eq!(
        signal_reason(&DriverFailure::eof().reason),
        PortSignalReason::Normal
    );
    assert_eq!(
        signal_reason(&DriverFailure::error(0).reason),
        PortSignalReason::Normal,
        "driver_failure(P,0) is the eof shape"
    );
    assert_eq!(
        signal_reason(&DriverFailure::atom("enospc").reason),
        PortSignalReason::Atom("enospc".to_string())
    );
    assert_eq!(
        signal_reason(&DriverFailure::posix(libc::EINVAL).reason),
        PortSignalReason::Atom("einval".to_string())
    );
    assert_eq!(
        signal_reason(&DriverFailure::error(42).reason),
        PortSignalReason::Integer(42)
    );
    assert_eq!(
        signal_reason(&DriverFailure::exit(0).reason),
        PortSignalReason::Normal,
        "driver_exit(P,0) renders normal (it just delivers nothing)"
    );
    assert_eq!(
        signal_reason(&DriverFailure::exit(libc::EACCES).reason),
        PortSignalReason::Atom("eacces".to_string())
    );
}

/// `driver_exit` never signals the owner, and the `driver_failure*` family always does — the one
/// distinction the C's `unlink_proc` makes (`io.c:7244`).
#[test]
fn only_the_failure_family_signals_the_owner() {
    assert!(DriverFailure::eof().signals_owner);
    assert!(DriverFailure::atom("boom").signals_owner);
    assert!(DriverFailure::posix(libc::EACCES).signals_owner);
    assert!(DriverFailure::error(7).signals_owner);
    assert!(!DriverFailure::exit(0).signals_owner);
    assert!(!DriverFailure::exit(libc::EACCES).signals_owner);
}

/// A published failure keeps the pump from declaring quiescence, and an empty one does not
/// invent work: an owed `'EXIT'` is a message a process is still waiting for.
#[test]
fn a_published_failure_is_pending_work_and_an_empty_one_is_not() {
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
    let mut ports = PortRegistry::new();
    assert!(ports.has_nothing_to_poll(), "a fresh registry is quiescent");

    ports.push_port_signals(Vec::new());
    assert!(
        ports.has_nothing_to_poll(),
        "a failure that owes nothing is not pending work"
    );

    ports.push_port_signals(vec![PortSignal {
        port: PortId::new(3),
        to: beam_process::ProcessId::new(1, 0),
        kind: PortSignalKind::Exit,
        reason: PortSignalReason::Atom("boom".to_string()),
    }]);
    assert!(
        !ports.has_nothing_to_poll(),
        "an owed 'EXIT' keeps the pump from declaring quiescence"
    );
    assert_eq!(ports.take_port_signals().len(), 1, "and it is deliverable");
    assert!(ports.has_nothing_to_poll(), "once taken, the registry is idle");
}
