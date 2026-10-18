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

//! Contract A.4 — the **`driver_output*` shapes** and the **driver-failure channel**, end to end
//! over a real, `dlopen`ed C driver (`tests/fixtures/output_shape_driver.c`, contract A.4's frozen
//! driver C ABI).
//!
//! # What this file pins
//!
//! The fixture's `output` callback reads the first command byte as an opcode and makes the
//! `driver_output*`/`driver_failure*` call that opcode names (the whole table is in the fixture's
//! file header), so one `port_command` drives exactly one scripted C call site and the resulting
//! `{Port, {data, _}}` / port-exit pair is the observable:
//!
//! * **the split** — one [`ReadMessage`] per `driver_output*` call, the `hbuf`/`hlen` bytes apart
//!   from `buf`/`len`, with the C's `buf == NULL` (`body: None`) kept distinct from a present,
//!   empty body (`body: Some(vec![])`). Two calls in one callback are **two** messages.
//! * **the failure channel** — a driver that closes its own port ([`PortFailure`]), its reason
//!   ([`FailureReason::Posix`]/[`Atom`]/[`Code`]/[`Eof`]/[`Normal`]), the `driver_exit`
//!   distinction ([`DriverFailure::signals_owner`] `false` ⇒ the owner is told **nothing**), the
//!   bytes handed up **before** the failure still reaching the owner ahead of the close, and a
//!   delivery attempted **after** the failure being refused — the bytes reach nobody, and the
//!   fixture's own log shows the delivery call's return code, the C's **`0`** (`rc=0`; the closing
//!   short-circuit at `io.c:6400-6401`, not the invalid-handle `-1` of `io.c:6396-6397`) — see
//!   `a_delivery_after_a_failure_answers_the_c_zero_and_reaches_nobody`.
//!
//! # The measured oracle (OTP 29.1.1 / erts-17.1)
//!
//! Probed with a real `output_shape_driver` port under the system `erl` — `erl_ddll:load_driver`
//! + `open_port({spawn_driver,"output_shape_driver"},[binary])` + `port_command(P,<<Op>>)`, with
//! `process_flag(trap_exit,true)` and a 200 ms `after` — one line per opcode (`-` = nothing
//! arrived; `data:`/`exit:` are the message stream in arrival order):
//!
//! ```text
//!  0 data:bin()                          8  exit:42
//!  1 data:list()                         9  exit:normal
//!  2 data:list(686472)                  10  -                      (driver_exit/0: unlinked)
//!  3 data:split(686472,)                11  -                      (driver_exit/ENOSPC)
//!  4 data:split(686472,626f6479)        12  data:bin(68656c6c6f) exit:einval
//!  5 data:bin(68656c6c6f) data:bin(57)  13  exit:einval
//!  6 exit:einval                        14  exit:enospc
//!  7 exit:enospc                        15  data:bin(616263646566)
//! ```
//!
//! `the_rendered_rows_match_the_erl_oracle` re-derives that table **live** from the system runtime
//! and compares it with this crate's own rendering, so the table cannot rot.
//!
//! # Mode-free on purpose
//!
//! `beam_io` renders no terms: [`ReadMessage`] carries the split and `beam_rt` decides whether the
//! owner's `Data` becomes a flat int list (`[]` port mode) or `[H…|<<Body>>]`
//! (`[binary]` mode, `deliver_read_message`, `erts/emulator/beam/io.c:3363-3442`). The Rust-side
//! table below is therefore **one** table; the oracle cross-check pins the `[binary]` column,
//! which is the one mode that distinguishes `body: None` from `body: Some(vec![])` at all.
//!
//! # Skip-vs-fail policy
//!
//! [`shape_fixture_so`] prints a **loud** `SKIP:` banner and answers `None` when `cc` or the OTP
//! include directory (`erl_driver.h`) is missing, and every test returns on that. A **load**
//! failure is never a skip (it means the `driver_*` host symbols are not in this test binary's
//! dynamic symbol table — a wiring bug), and neither is a non-zero exit from the oracle escript.

mod cdriver;
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_common::BeamError;
use beam_io::prelude::*;
use beam_process::PortId;

use cdriver::*;

/// The `ErlDrvEntry.driver_name` of `tests/fixtures/output_shape_driver.c` — the only name the
/// object may be loaded as, and the one the oracle escript spawns.
const DRIVER: &str = "output_shape_driver";

/// The longest a legitimately slow round may take before the test fails (a panic, never a silent
/// pass): the fixture does no I/O, so anything above this is a hang.
const ROUND_TIMEOUT: Duration = Duration::from_secs(30);

/// The wall-clock budget for one oracle escript session. `escript` alternates 16 opcodes, each
/// with its own port and its own 200 ms idle window, so the session is ~7 s on an idle host.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------------------------------------
// Driving one scripted opcode through a fresh port
// ---------------------------------------------------------------------------------------------

/// Everything one scripted opcode exposed, driven through a **fresh** port of its own.
///
/// One port per row is not a convenience: the failure rows close the port from inside the driver,
/// so a shared port would make the second row's command answer `badarg` instead of running the
/// driver.
struct Driven {
    /// The reactor and registry the port lives in, so a caller can probe it afterwards.
    poller: Poller,
    /// The registry the port lives in (kept so the caller can command/control it afterwards).
    registry: PortRegistry,
    /// The port the row was driven on.
    id: PortId,
    /// The full [`ReadMessage`]s the round reported, in arrival order (the split preserved).
    reads: Vec<ReadMessage>,
    /// The `{header, bytes, eol}` line frames the round staged (empty unless line mode is on).
    frames: Vec<(Vec<u8>, Vec<u8>, bool)>,
    /// The failures [`PortRegistry::take_failures`] reported, in order.
    failures: Vec<PortFailure>,
    /// Every notification the round reported for this port, in order (the `Data`/`Closed` order).
    notifications: Vec<PortNotification>,
    /// How many [`PortNotification::Closed`] reports the round minted for this port.
    closed: usize,
    /// Whether the port was still live after the round.
    still_live: bool,
    /// The fixture's callback log after the round (`start`/`output op=N`/`call=…`/`stop`).
    log: Vec<String>,
}

/// Commands `op` at the fixture through a fresh port, runs one port-task round, and reports what
/// the round exposed.
///
/// `line` sets `{line, N}` framing with `N` as the limit (`Some(4)` is the fixture's
/// `[{line,4}]` column) **before** the command is queued, exactly as `open_port(..., [{line,N}])`
/// would have it from the start.
fn drive(so: &Path, op: u8, line: Option<usize>) -> Driven {
    let probe = logged_port_command(&format!("abi-failure-op{op:02}"));
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) =
        open_named_port(so, DRIVER, &probe.command, host);
    if let Some(max) = line {
        registry
            .set_line_mode(id, Some(max))
            .expect("the port is live");
    }
    registry
        .port_command(&mut poller, id, &[op], PortCommandOptions::EMPTY)
        .expect("the port accepts the command byte");
    let outcome = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the port-task round runs");
    let reads = reads_of(&outcome, id);
    let closed = closed_of(&outcome, id);
    let notifications = outcome.notifications.clone();
    let frames = registry
        .take_line_notifications()
        .into_iter()
        .map(|frame| (frame.header, frame.bytes, frame.eol))
        .collect();
    let failures = registry.take_failures();
    let still_live = registry.contains(id);
    let log = log_lines(&probe.log);
    Driven {
        poller,
        registry,
        id,
        reads,
        frames,
        failures,
        notifications,
        closed,
        still_live,
        log,
    }
}

/// One [`ReadMessage`] with `header` apart from `body` (`None` = the C's `buf == NULL`).
fn read(header: &[u8], body: Option<&[u8]>) -> ReadMessage {
    ReadMessage {
        header: header.to_vec(),
        body: body.map(<[u8]>::to_vec),
    }
}

/// The fixture log a row must have appended: `start`, the opcode, its `call=` lines, then the
/// `stop` the *ordinary* close path ran.
fn expected_log(op: u8, calls: &[&str], closed: bool) -> Vec<String> {
    let mut lines = vec!["start".to_string(), format!("output op={op}")];
    lines.extend(calls.iter().map(|call| (*call).to_string()));
    if closed {
        lines.push("stop".to_string());
    }
    lines
}

// ---------------------------------------------------------------------------------------------
// The output shapes: one ReadMessage per driver_output* call
// ---------------------------------------------------------------------------------------------

/// Ops `0`..`5` and `15` — the `driver_output`/`driver_output2` shapes, one row each.
///
/// The oracle column for every row is the table in this file's header. The Rust table is the same
/// for a `[]` and a `[binary]` port, because this crate carries the **split** and not the term
/// (`read`'s `None`/`Some(vec![])` distinction *is* the C's `buf == NULL` vs `""`, and it is
/// `beam_rt` that renders one as `[H…]` and the other as `[H…|<<>>]`).
#[test]
fn the_output_shapes_are_one_read_message_per_driver_call() {
    let Some(so) = shape_fixture_so() else {
        return;
    };

    // (opcode, the reads the round must report, the fixture's own `call=` log lines)
    let rows: [(u8, Vec<ReadMessage>, &[&str]); 7] = [
        (0, vec![read(&[], Some(b""))], &["call=driver_output_empty rc=0"]),
        (1, vec![read(&[], None)], &["call=driver_output_null rc=0"]),
        (2, vec![read(b"hdr", None)], &["call=driver_output2_null_body rc=0"]),
        (3, vec![read(b"hdr", Some(b""))], &["call=driver_output2_empty_body rc=0"]),
        (4, vec![read(b"hdr", Some(b"body"))], &["call=driver_output2_body rc=0"]),
        (
            5,
            vec![read(&[], Some(b"hello")), read(&[], Some(b"W"))],
            &["call=driver_output_hello rc=0", "call=driver_output_W rc=0"],
        ),
        (15, vec![read(&[], Some(b"abcdef"))], &["call=driver_output_six rc=0"]),
    ];

    for (op, expected, calls) in rows {
        let mut driven = drive(&so, op, None);
        assert_eq!(
            driven.reads, expected,
            "op {op}: one ReadMessage per driver_output* call, header and body (and a NULL body \
             from an empty one) kept exactly as the driver made them"
        );
        assert!(
            driven.frames.is_empty(),
            "op {op}: a port in raw mode reports through PortNotification, never as a line frame"
        );
        assert!(
            driven.failures.is_empty(),
            "op {op}: the driver delivered data and closed nothing, got {:?}",
            driven.failures
        );
        assert_eq!(driven.closed, 0, "op {op}: the port is still open");
        assert!(driven.still_live, "op {op}: the port is still in the registry");
        assert_eq!(
            driven.log,
            expected_log(op, calls, false),
            "op {op}: the fixture made exactly the calls the opcode names"
        );
        driven
            .registry
            .close(&mut driven.poller, driven.id)
            .expect("the live port closes");
    }
}

// ---------------------------------------------------------------------------------------------
// The `{line, 4}` column: a full buffer only emits when the NEXT byte arrives
// ---------------------------------------------------------------------------------------------

/// With `{line, 4}`, a read whose body stays under (or exactly at) the limit stages **no** frame,
/// an over-limit read stages one `noeol` chunk of four bytes, and a second read filling the
/// buffer still stages nothing — the C's `LINEBUF_FULL` only flushes when the next byte arrives.
///
/// Oracle column (`open_port(..., [binary, {line, 4}])`): ops `0`..`4` produce no
/// `{Port,{data,{noeol|eol,_}}}` at all (the bodies are `""`/`NULL`/`""`/`"body"` — `"body"` is
/// exactly four bytes, so the buffer is full and still held), op `5` produces exactly one
/// `{noeol,"hell"}` (leaving `"o"` + the `"W"` of the second call unflushed), and op `15` produces
/// exactly one `{noeol,"abcd"}` (leaving `"ef"`).
#[test]
fn line_mode_frames_only_the_reads_that_overrun_the_limit() {
    let Some(so) = shape_fixture_so() else {
        return;
    };

    // (opcode, the frames the round must stage)
    let rows: [(u8, Vec<(Vec<u8>, Vec<u8>, bool)>); 7] = [
        // An empty body, no body at all, `"hdr"` alone, `"hdr"` + `""`, `"hdr"` + `"body"`:
        // none of them completes or overruns a four-byte line, so none of them stages a frame.
        (0, vec![]),
        (1, vec![]),
        (2, vec![]),
        (3, vec![]),
        (4, vec![]),
        // "hello" is five bytes: the fifth byte flushes the full buffer as `noeol`, and the
        // second call's "W" does not complete a line of its own.
        (5, vec![(Vec::new(), b"hell".to_vec(), false)]),
        (15, vec![(Vec::new(), b"abcd".to_vec(), false)]),
    ];

    for (index, (op, expected)) in rows.into_iter().enumerate() {
        let mut driven = drive(&so, op, Some(4));
        assert_eq!(
            driven.frames, expected,
            "row {index} (op {op}) in {{line, 4}} mode: the frames, with the header of the read \
             that produced each one; a full buffer emits only when the next byte arrives"
        );
        assert!(
            driven.reads.is_empty(),
            "row {index} (op {op}): a line-mode port reports NO PortNotification — the frames are \
             the channel"
        );
        assert!(
            driven.failures.is_empty(),
            "row {index} (op {op}): the driver closed nothing"
        );
        assert_eq!(
            driven.closed, 0,
            "row {index} (op {op}): the port is still open"
        );
        driven
            .registry
            .close(&mut driven.poller, driven.id)
            .expect("the live port closes");
    }
}

// ---------------------------------------------------------------------------------------------
// The failure channel, row by row
// ---------------------------------------------------------------------------------------------

/// One scripted `driver_failure*`/`driver_exit` row.
struct FailureRow {
    /// The opcode the fixture's `output` callback interprets as this call.
    op: u8,
    /// The failure the driver latched.
    failure: DriverFailure,
    /// `FailureReason::atom_name()` — the reason atom the owner observes, `None` for the integer.
    atom: Option<&'static str>,
    /// `FailureReason::code()` — the integer reason, `None` for every atom-shaped reason.
    code: Option<i32>,
    /// `FailureReason::is_eof()` — the C's line-buffer-flushing flag.
    is_eof: bool,
    /// The fixture's own `call=` log lines.
    calls: &'static [&'static str],
}

/// Every `driver_failure*`/`driver_exit` shape: the reason the owner is told, the C's
/// `signals_owner` distinction, the ordinary close path, and the port being gone afterwards.
///
/// Measured oracle (see the file header): `6` → `einval`, `7` → `enospc`, `8` → the integer `42`,
/// `9` → `normal` (the eof shape), `10` → **nothing** (`driver_exit(p,0)` unlinks the connected
/// process before closing), `11` → **nothing** (`driver_exit(p,ENOSPC)`, same unlink).
#[test]
fn every_failure_reason_closes_the_port_and_signals_the_owner_as_the_c_does() {
    let Some(so) = shape_fixture_so() else {
        return;
    };

    let rows = [
        FailureRow {
            op: 6,
            failure: DriverFailure::posix(libc::EINVAL),
            atom: Some("einval"),
            code: None,
            is_eof: false,
            calls: &["call=driver_failure_posix rc=0"],
        },
        FailureRow {
            op: 7,
            failure: DriverFailure::atom("enospc"),
            atom: Some("enospc"),
            code: None,
            is_eof: false,
            calls: &["call=driver_failure_atom rc=0"],
        },
        FailureRow {
            op: 8,
            failure: DriverFailure::error(42),
            atom: None,
            code: Some(42),
            is_eof: false,
            calls: &["call=driver_failure rc=0"],
        },
        FailureRow {
            op: 9,
            failure: DriverFailure::eof(),
            atom: Some("normal"),
            code: None,
            is_eof: true,
            calls: &["call=driver_failure_eof rc=0"],
        },
        FailureRow {
            op: 10,
            failure: DriverFailure::exit(0),
            atom: Some("normal"),
            code: None,
            is_eof: false,
            calls: &["call=driver_exit_zero rc=0"],
        },
        FailureRow {
            op: 11,
            failure: DriverFailure::exit(libc::ENOSPC),
            atom: Some("enospc"),
            code: None,
            is_eof: false,
            calls: &["call=driver_exit_enospc rc=0"],
        },
    ];

    for row in rows {
        let mut driven = drive(&so, row.op, None);
        let op = row.op;

        // The reason, as the runtime renders it: the `driver_failure*` family signals the owner,
        // `driver_exit` does not (it unlinked the process first, so the oracle sends nothing).
        let signals_owner = !matches!(op, 10 | 11);
        assert_eq!(
            driven.failures,
            vec![PortFailure::new(driven.id, row.failure.clone())],
            "op {op}: exactly one failure, for this port, carrying the reason the driver latched"
        );
        let reported = &driven.failures[0].failure;
        assert_eq!(
            reported.signals_owner, signals_owner,
            "op {op}: driver_exit unlinks the connected process first, so its owner observes \
             nothing; every driver_failure* delivers a port exit"
        );
        assert_eq!(
            reported.reason.atom_name().as_deref(),
            row.atom,
            "op {op}: the atom-shaped reason (the C's `erl_errno_id` for a posix errno, the \
             driver's own name for driver_failure_atom, `normal` for the eof and zero-code shapes)"
        );
        assert_eq!(
            reported.reason.code(),
            row.code,
            "op {op}: `driver_failure(port, 42)` sends the integer, not an atom"
        );
        assert_eq!(
            reported.reason.is_eof(),
            row.is_eof,
            "op {op}: only `driver_failure_eof` (and `driver_failure(port, 0)`) is the eof shape"
        );

        // The *same* round reports no data, closes the port exactly once, and retires nothing a
        // second time — the `Closed` report is the number-retirement event, not a second death.
        assert!(
            driven.reads.is_empty(),
            "op {op}: this callback made no driver_output* call"
        );
        assert!(
            driven.frames.is_empty(),
            "op {op}: a raw-mode port stages no line frame"
        );
        assert_eq!(
            driven.closed, 1,
            "op {op}: a reaped port is reported as Closed exactly once"
        );
        assert!(
            !driven.still_live,
            "op {op}: a driver that closed its port takes it out of the live map"
        );

        // The port is gone: every further interaction is the oracle's `badarg`.
        let err = driven
            .registry
            .port_command(&mut driven.poller, driven.id, &[0], PortCommandOptions::EMPTY)
            .expect_err("a reaped port refuses a command");
        assert!(
            matches!(err, BeamError::BadArg),
            "op {op}: port_command on a reaped port is the typed badarg, got {err:?}"
        );
        let err = driven
            .registry
            .port_control(&mut driven.poller, driven.id, "x", &[])
            .expect_err("a reaped port refuses a control op");
        assert!(
            matches!(err, BeamError::BadArg),
            "op {op}: port_control on a reaped port is the typed badarg, got {err:?}"
        );
        assert_eq!(
            driven.registry.take_failures(),
            Vec::new(),
            "op {op}: the failure channel is drained — a second take reports nothing"
        );

        // The teardown ran the **ordinary** close path, so the driver's `stop` callback did.
        assert_eq!(
            driven.log,
            expected_log(op, row.calls, true),
            "op {op}: the driver's stop callback ran as part of the port's teardown"
        );
    }
}

/// `driver_output("hello",5)` **then** `driver_failure_posix(EINVAL)`: the bytes handed up before
/// the failure still reach the owner, ahead of the close, in the same round.
///
/// This is the pinned oracle row `{data, <<"hello">>}` then `{'EXIT', P, einval}`: the C delivers
/// `driver_output` synchronously (`deliver_read_message`) and the failure only *requests* a close,
/// so closing the port must not drop the staged read.
#[test]
fn data_delivered_before_a_failure_reaches_the_owner_ahead_of_the_close() {
    let Some(so) = shape_fixture_so() else {
        return;
    };
    let driven = drive(&so, 12, None);

    assert_eq!(
        driven.reads,
        vec![read(&[], Some(b"hello"))],
        "the bytes the same callback handed up before it failed still reach the owner"
    );
    assert_eq!(
        driven.failures,
        vec![PortFailure::new(
            driven.id,
            DriverFailure::posix(libc::EINVAL)
        )],
        "the failure the driver latched is the posix reason, once"
    );
    assert_eq!(
        driven.closed, 1,
        "the reaped port is reported as Closed exactly once, in the same round"
    );

    // The order inside the round's notifications is the oracle's message order: data first.
    let data_at = driven
        .notifications
        .iter()
        .position(|notification| {
            matches!(notification, PortNotification::Data { port, .. } if *port == driven.id)
        })
        .expect("the round reported the pre-failure read");
    let closed_at = driven
        .notifications
        .iter()
        .position(|notification| {
            matches!(notification, PortNotification::Closed { port } if *port == driven.id)
        })
        .expect("the round reported the close");
    assert!(
        data_at < closed_at,
        "the data must be listed BEFORE the close (the oracle's `{{data,…}}` then \
         `{{'EXIT',P,einval}}`), got {at:?}",
        at = (data_at, closed_at)
    );

    assert!(
        !driven.still_live,
        "the port the driver closed is gone from the live map"
    );
    assert_eq!(
        driven.log,
        expected_log(
            12,
            &[
                "call=driver_output_hello rc=0",
                "call=driver_failure_posix rc=0"
            ],
            true
        ),
        "the fixture delivered, then failed, and the teardown ran its stop"
    );
}

/// A delivery attempted **after** the failure answers the C's `0` and reaches the owner as nothing
/// at all: only the bytes handed up *before* the failure are delivered.
///
/// Op `13` attempts a `driver_output2` after `driver_failure_posix`, op `14` a plain
/// `driver_output` after `driver_failure_atom("enospc")`, so both delivery entry points are
/// covered — and the return code is observed on the fixture's **own** side of the ABI, not
/// inferred.
///
/// **The return code is the C's**, because a driver can observe it: `driver_output2` tests the
/// handle first (`-1` iff `erts_drvport2port_state` reports `ERTS_INVALID_ERL_DRV_PORT`,
/// `io.c:6396-6397`) and then short-circuits on `ERTS_PORT_SFLG_CLOSING` with **`0`**
/// (`io.c:6400-6401`; `driver_output_binary` `io.c:6351-6352`, `driver_outputv` `io.c:6473-6474`;
/// `driver_output` *is* `driver_output2(ix, NULL, 0, buf, len)`, `io.c:6442-6446`). A port whose
/// driver just failed is on its way out (`driver_failure_term`, `io.c:7196-7224`) but still
/// resolves — it is `EXITING`, not yet `FREE`/`INITIALIZING` (`erl_port.h:739-766`) — so the
/// delivery is dropped with a **success** report.
///
/// Measured against the system runtime (OTP 29.1.1 / erts-17.1) with this fixture's opcodes: op
/// `13` logs `call=driver_failure_posix rc=0` then `call=driver_output2_after_failure rc=0`, and op
/// `14` `call=driver_failure_atom rc=0` then `call=driver_output_after_failure rc=0`, with no
/// `{Port,{data,_}}` in either event stream. The measurement uses a copy of the fixture whose log
/// path lives in a **static** buffer, because the real `erts` runs the driver's `stop`
/// *synchronously inside* `driver_failure*` (so the committed fixture's `stop` frees the per-port
/// state the next `log_call` would read). That ordering is this crate's one divergence here — the
/// reaped port's `stop` runs after the round, hence after both `call=` lines — and it is not
/// observable to the owner.
#[test]
fn a_delivery_after_a_failure_answers_the_c_zero_and_reaches_nobody() {
    let Some(so) = shape_fixture_so() else {
        return;
    };

    let rows: [(u8, DriverFailure, &[&str]); 2] = [
        (
            13,
            DriverFailure::posix(libc::EINVAL),
            &[
                "call=driver_failure_posix rc=0",
                "call=driver_output2_after_failure rc=0",
            ],
        ),
        (
            14,
            DriverFailure::atom("enospc"),
            &[
                "call=driver_failure_atom rc=0",
                "call=driver_output_after_failure rc=0",
            ],
        ),
    ];

    for (op, failure, calls) in rows {
        let driven = drive(&so, op, None);
        assert!(
            driven.reads.is_empty(),
            "op {op}: the post-failure delivery must reach nobody"
        );
        assert!(
            driven.frames.is_empty(),
            "op {op}: a raw-mode port stages no line frame"
        );
        assert_eq!(
            driven.failures,
            vec![PortFailure::new(driven.id, failure)],
            "op {op}: the reason the driver latched is reported once"
        );
        assert_eq!(driven.closed, 1, "op {op}: the port is reported Closed once");
        assert!(
            !driven.still_live,
            "op {op}: the reaped port is out of the live map"
        );
        assert_eq!(
            driven.log,
            expected_log(op, calls, true),
            "op {op}: the fixture's own rc=0 is the proof the post-failure delivery reported the \
             C's closing code (`io.c:6400-6401`) while dropping the bytes"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The live oracle cross-check
// ---------------------------------------------------------------------------------------------

/// Runs the compiled fixture under the **system** Erlang and returns one `op rendering` line per
/// scripted opcode.
///
/// The escript loads the object with `erl_ddll:load_driver/2`, opens one
/// `open_port({spawn_driver, Name}, [binary])` per opcode, sends the opcode byte with
/// `port_command/2`, and collects `{P,{data,D}}` / `{'EXIT',P,R}` until a 200 ms idle window
/// (`process_flag(trap_exit,true)`). Each event is normalized so the two sides can be compared as
/// text:
///
/// * a binary body → `data:bin(<hex>)` (the `[binary]` port mode's bare tail);
/// * a proper list → `data:list(<hex>)` (the C's `buf == NULL` header-only / empty reply);
/// * the improper `[H…|<<Bin>>]` → `data:split(<hex header>,<hex body>)` — the mode's split shape,
///   which is what makes `body: None` and `body: Some(vec![])` distinguishable at all;
/// * an exit signal → `exit:<atom name>` or `exit:<integer>`;
/// * nothing at all → `-`.
///
/// `is_list/1` is a *shallow* test (a cons whose tail is a binary answers `true`), so the
/// proper/improper distinction is the walk, not the guard.
fn oracle_stdout(script: &Path, so: &Path) -> Option<String> {
    let script = script.to_path_buf();
    let so = so.to_path_buf();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let out = Command::new("escript").arg(&script).arg(&so).output();
        let _ = tx.send(out);
    });

    match rx.recv_timeout(ORACLE_TIMEOUT) {
        Err(_) => {
            support::skip(&format!(
                "oracle check skipped: the `escript` session did not answer within {ORACLE_TIMEOUT:?}"
            ));
            None
        }
        Ok(Err(err)) => {
            support::skip(&format!(
                "oracle check skipped: could not spawn `escript` ({err}); is escript on PATH?"
            ));
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "the oracle escript failed (exit {:?}) — a non-zero exit is a divergence, never a \
                 skip:\n--- stdout ---\n{}\n--- stderr ---\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// The oracle escript. Its first line **must** be a `#!` line: without one `escript` mis-parses
/// the file.
const ORACLE_ESCRIPT: &str = r#"#!/usr/bin/env escript
main([SoPath]) ->
    Dir = filename:dirname(SoPath),
    Name = filename:basename(SoPath, ".so"),
    ok = erl_ddll:load_driver(Dir, Name),
    process_flag(trap_exit, true),
    lists:foreach(
      fun(Op) ->
              Events = run(Name, Op),
              io:format("~w ~s~n", [Op, render(Events)])
      end, lists:seq(0, 15)).

run(Name, Op) ->
    P = open_port({spawn_driver, Name}, [binary]),
    true = port_command(P, <<Op>>),
    Events = collect(P),
    drain(P),
    Events.

collect(P) ->
    receive
        {P, {data, Data}} -> [{data, Data} | collect(P)];
        {'EXIT', P, Reason} -> [{exit, Reason} | collect(P)]
    after 200 ->
            []
    end.

drain(P) ->
    try port_close(P) catch _:_ -> ok end,
    drain_msgs(P, 200).

drain_msgs(P, T) ->
    receive
        {P, _} -> drain_msgs(P, T);
        {'EXIT', P, _} -> drain_msgs(P, T)
    after T ->
            ok
    end.

render([]) ->
    "-";
render(Events) ->
    lists:flatten(lists:join(" ", [render_event(E) || E <- Events])).

render_event({data, D}) ->
    "data:" ++ render_data(D);
render_event({exit, R}) when is_atom(R) ->
    "exit:" ++ atom_to_list(R);
render_event({exit, R}) when is_integer(R) ->
    "exit:" ++ integer_to_list(R).

render_data(D) when is_binary(D) ->
    "bin(" ++ hex(D) ++ ")";
render_data(D) when is_list(D) ->
    case split_prefix(D, []) of
        {Prefix, []} ->
            "list(" ++ hex(list_to_binary(Prefix)) ++ ")";
        {Prefix, Tail} when is_binary(Tail) ->
            "split(" ++ hex(list_to_binary(Prefix)) ++ "," ++ hex(Tail) ++ ")"
    end.

split_prefix([H | T], Acc) when is_integer(H) ->
    split_prefix(T, [H | Acc]);
split_prefix([], Acc) ->
    {lists:reverse(Acc), []};
split_prefix(Bin, Acc) when is_binary(Bin) ->
    {lists:reverse(Acc), Bin}.

hex(Bin) ->
    lists:flatten([io_lib:format("~2.16.0b", [B]) || <<B>> <= Bin]).
"#;

/// This crate's rendering of one driven row — the exact function the oracle escript mirrors.
fn rendered_row(driven: &Driven) -> String {
    let mut parts: Vec<String> = driven.reads.iter().map(render_read).collect();
    for failure in &driven.failures {
        if let Some(text) = render_exit(&failure.failure) {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" ")
    }
}

/// `data:list(<hex header>)` (the C's `buf == NULL`), `data:bin(<hex body>)` (a bare tail) or
/// `data:split(<hex header>,<hex body>)` (a header plus a present tail) — the `[binary]` port's
/// three term shapes.
fn render_read(read: &ReadMessage) -> String {
    match &read.body {
        None => format!("data:list({})", hex(&read.header)),
        Some(body) if read.header.is_empty() => format!("data:bin({})", hex(body)),
        Some(body) => format!("data:split({},{})", hex(&read.header), hex(body)),
    }
}

/// `exit:<atom>` / `exit:<integer>` for a failure that reaches the owner; `None` for the
/// `driver_exit` shape, whose owner the C unlinked before closing the port.
fn render_exit(failure: &DriverFailure) -> Option<String> {
    if !failure.signals_owner {
        return None;
    }
    match failure.reason.atom_name() {
        Some(name) => Some(format!("exit:{name}")),
        None => failure.reason.code().map(|code| format!("exit:{code}")),
    }
}

/// Lowercase hex, two digits per byte — the same spelling `~2.16.0b` produces on the other side.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Parses the escript's `op rendering` lines into `op -> rendering`.
fn parse_oracle_rows(stdout: &str) -> Vec<(u8, String)> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (op, rendering) = line
                .split_once(' ')
                .unwrap_or_else(|| panic!("an `op rendering` line, got {line:?}"));
            let op: u8 = op
                .trim()
                .parse()
                .unwrap_or_else(|err| panic!("the opcode in {line:?}: {err}"));
            (op, rendering.trim_end().to_string())
        })
        .collect()
}

/// Writes the oracle escript into a fresh scratch directory and answers its path.
fn write_oracle_escript() -> PathBuf {
    let dir = support::scratch_dir("abi-failure-oracle");
    std::fs::create_dir_all(&dir).expect("a scratch dir is creatable");
    let script = dir.join("oracle.escript");
    std::fs::write(&script, ORACLE_ESCRIPT).expect("the oracle escript is writable");
    script
}

/// Is `tool` an executable on `PATH`?
fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(tool))
            .any(|candidate| candidate.is_file())
    })
}

/// The whole `driver_output*` + failure table, cross-checked against the **real** system Erlang.
///
/// This is the live version of the file header's measured table: the oracle escript runs every
/// opcode through the compiled fixture under the system runtime and prints a normalized rendering
/// per opcode, and the Rust side renders the very rows the other tests in this file assert. The
/// two line sets must be **equal** — which is what pins, against the real system, that
/// `body: None` and `body: Some(vec![])` are different messages (op `2` vs `3`), that two calls in
/// one callback are two messages (op `5`), that a `driver_exit` owner is told nothing (ops `10`,
/// `11`) and that the integer reason stays an integer (op `8`).
#[test]
fn the_rendered_rows_match_the_erl_oracle() {
    let Some(so) = shape_fixture_so() else {
        return;
    };
    if !on_path("escript") || !on_path("erl") {
        support::skip("the oracle cross-check needs `erl` and `escript` on PATH");
        return;
    }

    // The Rust side: every opcode 0..=15 through its own port, rendered the same way.
    let mut rust_rows: Vec<(u8, String)> = Vec::new();
    for op in 0..=15u8 {
        let mut driven = drive(&so, op, None);
        rust_rows.push((op, rendered_row(&driven)));
        if driven.still_live {
            driven
                .registry
                .close(&mut driven.poller, driven.id)
                .expect("the live port closes");
        }
    }

    let script = write_oracle_escript();
    let Some(stdout) = oracle_stdout(&script, &so) else {
        return; // the oracle is unavailable; the skip was printed loudly above
    };
    let oracle_rows = parse_oracle_rows(&stdout);

    assert_eq!(
        oracle_rows.len(),
        16,
        "the oracle prints one line per opcode 0..=15, got {oracle_rows:?}\n{stdout}"
    );
    assert_eq!(
        rust_rows, oracle_rows,
        "the Rust rows and the system Erlang's rows must agree event for event \
         (data:/exit: in arrival order, `-` for nothing)"
    );
}

/// A reaped port's number is retired by its `Closed` report, so the id a failure freed is handed
/// out again — and the round that reaped it is the only place that happens.
///
/// (An id-space property this file would otherwise leave implicit: the failure rows above all
/// assert the port left the live map, and this one asserts the *number* came back.)
#[test]
fn a_reaped_ports_number_is_released_back_to_the_id_space() {
    let Some(so) = shape_fixture_so() else {
        return;
    };
    let mut driven = drive(&so, 6, None);
    assert!(!driven.still_live, "the failing row reaped its port");
    assert_eq!(driven.closed, 1, "the round minted the close report");

    // The registry is empty again, and a fresh open gets number `0`: the failed row's number was
    // released by the report (never by the close, which only queues it).
    assert!(
        driven.registry.is_empty(),
        "no port is left registered after the reaped row"
    );
    let (host, _state) = TestPortHost::new();
    let driver = load_named_driver(&so, DRIVER, "", Box::new(host));
    let reopened = driven
        .registry
        .open(&mut driven.poller, Box::new(driver))
        .expect("a fresh port opens");
    assert_eq!(
        reopened.number(),
        0,
        "the reaped port's number was released back to the id space"
    );
    driven
        .registry
        .close(&mut driven.poller, reopened)
        .expect("the fresh port closes");
}

/// The reactor never blocks past its deadline while a row is being driven: a coarse guard that the
/// scripted rows need no `poll(2)` at all (the fixture arms no descriptor), so
/// [`ROUND_TIMEOUT`] is only there to turn a hang into a failure rather than a stuck suite.
#[test]
fn a_scripted_row_completes_without_waiting_on_the_reactor() {
    let Some(so) = shape_fixture_so() else {
        return;
    };
    let started = std::time::Instant::now();
    let mut driven = drive(&so, 4, None);
    assert_eq!(driven.reads, vec![read(b"hdr", Some(b"body"))]);
    assert!(
        started.elapsed() < ROUND_TIMEOUT,
        "a scripted row is driven by port_command alone, it never waits on the reactor"
    );
    driven
        .registry
        .close(&mut driven.poller, driven.id)
        .expect("the port closes");
}
