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
//! A driver that fails its **own** port from inside its `output` callback — the differential
//! evidence for the `driver_failure*`/`driver_exit` family (contract A.4).
//!
//! `src/port/failure.rs` tears a failed port down: it records the failure's reason, removes the
//! port's metadata, and publishes the `'EXIT'`/`'DOWN'` signals it owes (`POrEventSignals`).
//! What makes the family subtle is the **read**: a driver writes with `driver_output*` *before*
//! it fails, so the owner is owed `{Port, {data, Data}}` **ahead of** the exit signal.
//! `beam_io` preserves those bytes for the port it reaps
//! (`PortRegistry::take_reaped_notifications` re-prepends them), and
//! `PortRegistry::fold_notifications` renders them from the recipient facts
//! `PortRegistry::fail_port` snapshotted before the metadata went — so the surviving read
//! reaches its real owner (and only its real owner: an unresolvable port drops the read).
//!
//! # The fixture
//!
//! `tests/fixtures/port_failure_driver.c` is a real C driver compiled by `cc` at test time; one
//! command byte selects one row:
//!
//! ```none
//! 1  driver_output(port,"hi",2) ; driver_failure(port,9)   %% data, then exit 9
//! 2  driver_failure_eof(port)                              %% exit normal
//! 3  driver_failure_atom(port,"boom")                      %% exit boom
//! 4  driver_failure_posix(port,EACCES)                     %% exit eacces
//! 5  driver_failure(port,7)                                %% exit 7, a bare integer
//! 6  driver_failure(port,0)                                %% exit normal
//! 7  driver_exit(port,0)                                   %% no exit signal at all
//! 8  driver_exit(port,EACCES)                              %% no exit signal, just 'DOWN'
//! ```
//!
//! `tests/fixtures/port_failure.erl` (compiled to the committed `.beam`) is the consumer: it
//! opens the port, sends the command, drains what the owner receives (**normalized** — a
//! `#Port<…>`/`#Ref<…>` cannot be equal across two OS processes) and then asks the dead port
//! for `port_info/1` and a second `port_close/1`.
//!
//! # Measured oracle rows (OTP 29.1.1 / erts-17.1)
//!
//! ```none
//! trapping   1  {[{data,<<"hi">>},{exit,true,9}],{undefined,{error,badarg}}}
//!            2  {[{exit,true,normal}],…}      3  {[{exit,true,boom}],…}
//!            4  {[{exit,true,eacces}],…}      5  {[{exit,true,7}],…}
//!            6  {[{exit,true,normal}],…}      7  {[],…}      8  {[],…}
//! +monitor   1  {[{data,<<"hi">>},{exit,true,9},{down,true,true,9}],…,true}
//!            7  {[{down,true,true,normal}],…,true}   8  {[{down,true,true,eacces}],…,true}
//! non-trap   2  [opened_port,alive,{helper_down,true,normal}]   (survives)
//!            3  [opened_port,{helper_down,true,boom}]           (terminated)
//!            7  [opened_port,alive,{helper_down,true,normal}]
//! ```
//!
//! # The two routes
//!
//! Both delivery routes are pinned because they are separate code paths over one policy: the
//! **`-eval` route** returns the messages to the evaluator's own `receive`
//! (`RuntimeContext::poll_port_messages`), and the **scheduler `run` route** puts them in the
//! recipient's mailbox and wakes a receiver parked in a compiled `receive`
//! (`Runtime::deliver_port_messages` → [`Runtime::deliver_port_signals`]).
//!
//! [`Runtime::deliver_port_signals`]: beam_rt::Runtime
//!
//! # Skip policy
//!
//! No `erl` on `$PATH`, no `cc`, or no `erl_driver.h` is a **loud** skip per test, never a
//! silent pass.
#![deny(unsafe_code)]
mod support;
use std::path::{Path, PathBuf};
use std::process::Command;
/// The runtime binary under test (cargo builds it for this suite).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");
/// The committed probe image (`tests/fixtures/port_failure.beam`).
const PROBE_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/port_failure.beam");
/// The driver's C source (compiled at test time; the `.so` is never committed).
const DRIVER_C: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/port_failure_driver.c"
);
/// The driver's registered name — the one `erl_ddll:try_load/3` records.
const DRIVER_NAME: &str = "port_failure_driver";
// ---------------------------------------------------------------------------------------
// Toolchain discovery (filesystem only — a probe must never run a tool to find it)
// ---------------------------------------------------------------------------------------
/// The C compiler: `$CC` when set, else `cc`.
fn cc() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}
/// Whether a C compiler is available.
fn cc_available() -> bool {
    Command::new(cc())
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}
/// Whether `dir` holds `erl_driver.h`.
fn has_header(dir: &Path) -> bool {
    dir.join("erl_driver.h").is_file()
}
/// The first `erts-*/include` under `base` that holds `erl_driver.h`.
fn erts_include_under(base: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(base).ok()?;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("erts-") {
            continue;
        }
        let include = entry.path().join("include");
        if has_header(&include) {
            return Some(include);
        }
    }
    None
}
/// The directory holding `erl_driver.h`, from the `erl` on `$PATH` (its sibling `erts-*`
/// directory), `$ERL_TOP`, or a standard installation root.
fn erl_include_dir() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(erl) = support::find_on_path("erl") {
        if let Ok(real) = erl.canonicalize() {
            // `<root>/erts-<vsn>/bin/erl` ⇒ the `erts-<vsn>` directory.
            if let Some(erts) = real.parent().and_then(Path::parent) {
                roots.push(erts.to_path_buf());
            }
        }
    }
    if let Some(top) = std::env::var_os("ERL_TOP") {
        roots.push(PathBuf::from(top));
    }
    for base in [
        "/usr/lib/erlang",
        "/usr/local/lib/erlang",
        "/usr/lib64/erlang",
        "/opt/erlang",
    ] {
        roots.push(PathBuf::from(base));
    }
    for root in roots {
        for direct in [
            root.join("include"),
            root.join("lib").join("erlang").join("include"),
        ] {
            if has_header(&direct) {
                return Some(direct);
            }
        }
        for base in [root.clone(), root.join("lib").join("erlang")] {
            if let Some(include) = erts_include_under(&base) {
                return Some(include);
            }
        }
    }
    None
}
// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------
/// A scratch directory holding the probe `.beam` and the compiled driver `.so`.
struct Fixture {
    /// The one directory both sides get as `-pa` / as the driver's load path.
    dir: PathBuf,
}
impl Fixture {
    /// The directory as a `&str` (every path this suite builds is UTF-8).
    fn dir(&self) -> &str {
        self.dir.to_str().expect("a UTF-8 scratch path")
    }
}
/// Compiles the driver and stages the probe, or `None` after a **loud** skip.
fn fixture(label: &str) -> Option<Fixture> {
    let Some(include) = erl_include_dir() else {
        support::skip_loudly(
            "port_failure",
            &format!("port_failure/{label}: no erl_driver.h include directory found"),
        );
        return None;
    };
    if !cc_available() {
        support::skip_loudly(
            "port_failure",
            &format!("port_failure/{label}: no C compiler ({} / $CC)", cc()),
        );
        return None;
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_port_failure_{label}_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly(
            "port_failure",
            &format!("port_failure/{label}: cannot create {}: {error}", dir.display()),
        );
        return None;
    }
    if let Err(error) = std::fs::copy(PROBE_BEAM, dir.join("port_failure.beam")) {
        support::skip_loudly(
            "port_failure",
            &format!("port_failure/{label}: cannot stage the probe: {error}"),
        );
        return None;
    }
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join(format!("{DRIVER_NAME}.so")))
        .arg(DRIVER_C)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            support::skip_loudly(
                "port_failure",
                &format!(
                    "port_failure/{label}: cc failed ({}):\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ),
            );
            return None;
        }
        Err(error) => {
            support::skip_loudly(
                "port_failure",
                &format!("port_failure/{label}: could not run the C compiler: {error}"),
            );
            return None;
        }
    }
    Some(Fixture { dir })
}
// ---------------------------------------------------------------------------------------
// The `-eval` route
// ---------------------------------------------------------------------------------------
/// Runs `expr` under the oracle and under [`BINARY`] (both with `fixture`'s dir as `-pa`) and
/// returns the two trimmed stdouts, or `None` after a **loud** skip when the *oracle* cannot
/// answer.
fn side_by_side(fixture: &Fixture, oracle: &Path, expr: &str) -> Option<(String, String)> {
    let Some(expected) = support::oracle_eval(oracle, Some(&fixture.dir), expr) else {
        support::skip_loudly(
            "port_failure",
            "the oracle could not answer the port_failure probe",
        );
        return None;
    };
    let output = support::run_bounded(
        Path::new(BINARY),
        &["-noshell", "-pa", fixture.dir(), "-eval", expr],
    )
    .expect("beam_rt answers the probe within the bound");
    assert!(
        output.status.success(),
        "beam_rt must exit 0 for this probe, got {:?}\n--- stderr ---\n{}\n--- expression ---\n{expr}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let ours = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some((expected, ours))
}
/// Asserts that `port_failure:<function>(Dir, DriverName, Op)` prints exactly the oracle's
/// bytes on this runtime.
fn agrees(fixture: &Fixture, oracle: &Path, function: &str, op: u8) {
    let expr = format!(
        "io:format(\"~w~n\",[port_failure:{function}(\"{dir}\",{DRIVER_NAME},{op})]),\nhalt(0).\n",
        dir = fixture.dir()
    );
    let Some((expected, ours)) = side_by_side(fixture, oracle, &expr) else {
        return;
    };
    assert_eq!(
        ours, expected,
        "port_failure:{function}/{op} must print the oracle's answer"
    );
}
/// Asserts `function` for each `op` in `ops`, one probe pair per row.
fn agrees_on_rows(fixture: &Fixture, oracle: &Path, function: &str, ops: &[u8]) {
    for op in ops {
        agrees(fixture, oracle, function, *op);
    }
}
// ---------------------------------------------------------------------------------------
// The scheduler `run` route
// ---------------------------------------------------------------------------------------
/// Runs `port_failure:<function>/3` through the **scheduler `run` route** (a real scheduled
/// process whose compiled `receive` drains its own mailbox) and compares it with the oracle's
/// answer for the same function.
fn agrees_on_the_run_route(fixture: &Fixture, oracle: &Path, function: &str, op: u8) {
    let expr = format!(
        "io:format(\"~w~n\",[port_failure:{function}(\"{dir}\",{DRIVER_NAME},{op})]),\nhalt(0).\n",
        dir = fixture.dir()
    );
    let Some(expected) = support::oracle_eval(oracle, Some(&fixture.dir), &expr) else {
        support::skip_loudly(
            "port_failure",
            &format!("the oracle could not answer the run-route probe for {function}/{op}"),
        );
        return;
    };
    let spec = format!("port_failure:{function}/3");
    // The directory reaches the fixture as an Erlang **string** (a double-quoted `run`
    // argument), the driver name as a bare atom and the row as an integer.
    let quoted_dir = format!("\"{}\"", fixture.dir());
    let op_arg = op.to_string();
    let output = support::run_bounded(
        Path::new(BINARY),
        &["run", PROBE_BEAM, &spec, &quoted_dir, DRIVER_NAME, &op_arg],
    )
    .expect("beam_rt answers the run-route probe within the bound");
    assert!(
        output.status.success(),
        "beam_rt run must exit 0 for {spec}, got {:?}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let ours = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(
        ours, expected,
        "the run route must print what the oracle prints for {spec} (op {op})"
    );
}
// ---------------------------------------------------------------------------------------
// Row 1 — the ordering residual: the read arrives BEFORE the exit signal
// ---------------------------------------------------------------------------------------
/// The row this suite exists for: a driver that writes a read and **then** fails its port
/// leaves the owner with `{Port, {data, <<"hi">>}}` **first** and `{'EXIT', Port, 9}` second.
///
/// The port's metadata is already gone when the round's notifications are folded (a failed
/// port is reaped, not closed), so the read's recipient comes from the snapshot
/// `PortRegistry::fail_port` took — a live port's own facts, never a guess.
#[test]
fn a_read_written_before_the_failure_is_delivered_first() {
    let Some(oracle) = support::require_oracle("port_failure") else {
        return;
    };
    let Some(fixture) = fixture("data_then_exit") else {
        return;
    };
    agrees(&fixture, &oracle, "trapping", 1);
}
/// The same row through the **scheduler** route: the data message is delivered into the
/// recipient's mailbox ahead of the exit signal, so a receiver parked in a compiled `receive`
/// sees the two in the oracle's order.
#[test]
fn the_run_route_delivers_the_read_before_the_exit() {
    let Some(oracle) = support::require_oracle("port_failure") else {
        return;
    };
    let Some(fixture) = fixture("run_data_then_exit") else {
        return;
    };
    agrees_on_the_run_route(&fixture, &oracle, "trapping", 1);
}
/// A `list`-stream port's surviving read renders as a **charlist** — the snapshot carries the
/// port's own `binary`/`list` option, so a failed port's bytes render exactly as a live one's
/// would.
#[test]
fn a_list_stream_port_renders_the_surviving_read_as_a_charlist() {
    let Some(oracle) = support::require_oracle("port_failure") else {
        return;
    };
    let Some(fixture) = fixture("list_mode") else {
        return;
    };
    agrees(&fixture, &oracle, "trapping_list_monitored", 1);
}
// ---------------------------------------------------------------------------------------
// Rows 2–6 — a trapping owner gets the failure's reason
// ---------------------------------------------------------------------------------------
/// A **trapping** owner receives `{'EXIT', Port, Reason}` for every failure reason — and a
/// numeric reason (`driver_failure(port, 7)`) is the bare **integer** `7`, never an atom.
#[test]
fn a_trapping_owner_gets_the_exit_with_the_failures_reason() {
    let Some(oracle) = support::require_oracle("port_failure") else {
        return;
    };
    let Some(fixture) = fixture("trapping_reasons") else {
        return;
    };
    // 2 eof ⇒ normal, 3 atom ⇒ boom, 4 posix ⇒ eacces, 5 code 7 ⇒ 7, 6 code 0 ⇒ normal.
    agrees_on_rows(&fixture, &oracle, "trapping", &[2, 3, 4, 5, 6]);
}
// ---------------------------------------------------------------------------------------
// Rows 7–8 — `driver_exit` never signals the owner
// ---------------------------------------------------------------------------------------
/// `driver_exit/2` is the `signals_owner == false` arm: the port goes away without telling its
/// owner, so a trapping (and **only** linked) owner receives **nothing** — no `'EXIT'` for
/// either reason, `0` or `EACCES`.
#[test]
fn driver_exit_tells_the_linked_owner_nothing() {
    let Some(oracle) = support::require_oracle("port_failure") else {
        return;
    };
    let Some(fixture) = fixture("driver_exit") else {
        return;
    };
    agrees_on_rows(&fixture, &oracle, "trapping", &[7, 8]);
}
// ---------------------------------------------------------------------------------------
// The monitor — told in both arms
// ---------------------------------------------------------------------------------------
/// A **monitor** is told in *both* arms: `{'DOWN', Ref, port, Port, Reason}` for a failure that
/// signals the owner (row 1's `9`) and for a `driver_exit` that does not (rows 7/8's `normal`
/// and `eacces`), always after the `'EXIT'` when both are owed.
#[test]
fn a_monitor_gets_the_down_in_both_signal_arms() {
    let Some(oracle) = support::require_oracle("port_failure") else {
        return;
    };
    let Some(fixture) = fixture("monitor") else {
        return;
    };
    agrees_on_rows(&fixture, &oracle, "trapping_monitored", &[1, 7, 8]);
}
// ---------------------------------------------------------------------------------------
// A non-trapping owner is terminated by a non-`normal` reason (and survives `normal`)
// ---------------------------------------------------------------------------------------
/// A **non-trapping** owner is killed by the port's exit signal for a non-`normal` reason —
/// the helper's own `'DOWN'` carries that reason (`boom`, `eacces`, and the integer `9`) — so
/// the port failed the process exactly as a linked peer's death would.
#[test]
fn a_non_trapping_owner_is_terminated_by_a_non_normal_reason() {
    let Some(oracle) = support::require_oracle("port_failure") else {
        return;
    };
    let Some(fixture) = fixture("notrap_dies") else {
        return;
    };
    agrees_on_rows(&fixture, &oracle, "non_trapping", &[3, 4, 5]);
}
/// A `normal` reason — `driver_failure_eof`, `driver_failure(port, 0)` and `driver_exit(port,
/// 0)` — never terminates a non-trapping owner: the helper sends `alive` and dies `normal`.
#[test]
fn a_non_trapping_owner_survives_a_normal_reason() {
    let Some(oracle) = support::require_oracle("port_failure") else {
        return;
    };
    let Some(fixture) = fixture("notrap_survives") else {
        return;
    };
    agrees_on_rows(&fixture, &oracle, "non_trapping", &[2, 6, 7]);
}
/// The owner's own exit signal reaches a **trapping** owner of a `normal`-reason failure too
/// (rows 2/6 above cover it with a monitor); this row pins the `driver_exit(EACCES)` case,
/// whose reason must never arrive as an `'EXIT'` at all.
#[test]
fn the_owner_that_is_only_linked_never_hears_a_driver_exit() {
    let Some(oracle) = support::require_oracle("port_failure") else {
        return;
    };
    let Some(fixture) = fixture("exit_only") else {
        return;
    };
    agrees(&fixture, &oracle, "trapping", 8);
}
