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
//! **A.3 end-to-end, part 3 of 3: the NIF resource `stop` callback and the
//! `enif_select` STOP verdicts.**
//!
//! Loads the real C NIF `../beam_bifs/tests/fixtures/a3_resource_stop.c` (it creates two
//! resource types in its `load` callback, one with a `stop` callback and one without)
//! through `erlang:load_nif/2` from interpreted Erlang, drives the STOP probes from a
//! `.beam`, and cross-checks every verdict against the live system `erl` running the same
//! `.so` and the same `.beam`.
//!
//! | probe | the observation |
//! |---|---|
//! | `probe_stop_unknown_fd/0` | `{1,1,1,true}` — a STOP on a pipe descriptor nothing ever armed: `ERL_NIF_SELECT_STOP_CALLED` (`1`), the callback ran **once**, `is_direct_call = 1`, and `LastEvent` is a real descriptor |
//! | `probe_arm_then_stop/0` | `{0,17,1,1,2,true}` — arm READ then STOP on the same descriptor: the verdict is `17` = `STOP_CALLED \| READ_CANCELLED`, the callback ran **once synchronously** (`1` call, `is_direct_call = 1`) and it saw the right payload (`tag = 2`) |
//! | `probe_report/0` after it | `{1,1,true,2,1,2}` — the recorded statics agree with the returned tuple, and the destructor ran once for that object |
//! | `probe_double_stop/0` | `{0,17,1,2,1,3}` — arm READ, STOP, STOP: the second STOP takes the fast track again (`1`) and runs the callback a **second** time |
//! | `probe_cancel_then_stop/0` | `{0,16,1,1,1,4}` — arm READ, CANCEL, STOP: the CANCEL answers `16` (`READ_CANCELLED`), after which the STOP is a plain `1` (there is no read interest left to cancel) |
//!
//! The verdict bits (`erl_nif.h:178-183`) are `STOP_CALLED = 1<<0`,
//! `STOP_SCHEDULED = 1<<1`, `READ_CANCELLED = 1<<4`; `is_direct_call = 1` is the C's
//! synchronous scheduler-side call (`erts_resource_stop(resource, fd, 1)`,
//! `erl_check_io.c:1528-1535`), which is what proves the callback really ran inline.
//!
//! # The one documented divergence
//!
//! `enif_thread_type()` inside the `stop` callback: the oracle runs it on a scheduler thread
//! (`1`), this runtime on its own driver thread (`0`, `ERL_NIF_THR_UNDEFINED`) — the same
//! mechanism as the other two A.3 suites. Every other slot of every tuple agrees exactly.
//!
//! # Probes this suite deliberately does NOT drive
//!
//! * **`probe_stop_nostop_type/0`** — a STOP on a resource type opened **without** a `stop`
//!   callback. The C only `ASSERT`s that the callback exists (`erl_check_io.c:1279`) and
//!   `ASSERT` is compiled out of a release build, so the oracle calls a NULL function
//!   pointer: it **takes the VM down with SIGSEGV** (exit 139, measured 3/3 runs by the
//!   fixture's own suite, `beam_bifs/tests/nif_a3_host_gaps.rs`). Nothing about this
//!   runtime could be learned from it, and a test that kills the oracle is not a test. Left
//!   uncalled, deliberately.
//! * **`probe_arm_then_stop_fallback/0`, `probe_stop_fallback_wait_ms/1`,
//!   `probe_stop_sleep_ms/1`, `probe_read_on_pending_stop/0`** — the **deferred** STOP arm
//!   (`ERL_NIF_SELECT_STOP_SCHEDULED`, bit 1). This runtime answers
//!   `ERL_NIF_SELECT_STOP_CALLED` for every STOP *by construction*: the `stop` callback is
//!   run synchronously through `beam_abi`'s `nif::enif::select::service_stop_verdict` with
//!   `is_direct_call = true` (`src/nif_select.rs`), so no STOP is ever deferred to a poller
//!   thread. The oracle's own answers for these probes are **timing-dependent**
//!   (`{0,18,…}` or `{0,2,…}`, and `read_on_pending` needs fresh-VM retries to observe its
//!   `2`), so pinning them would encode a race rather than a behaviour. They are listed in
//!   [`UNDRIVEN`] with that reason instead of being weakened into an assertion that cannot
//!   hold.
#[path = "support/nif_a3.rs"]
#[allow(dead_code)]
mod nif_a3;
mod support;

use std::path::{Path, PathBuf};

use nif_a3::Expect;

/// The fixture under test — `beam_bifs`'s own C source, compiled where it lives.
fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../beam_bifs/tests/fixtures/a3_resource_stop.c")
}

/// The NIF module: the C's `ERL_NIF_INIT` name, with a stub for every row of its `funcs`
/// table — including `probe_stop_nostop_type/0`, which must *exist* for the load to pass and
/// is never called.
const NIF_ERL: &str = r#"%% Generated by `beam_rt/tests/nif_a3_resource_stop.rs` (never committed).
-module(a3_resource_stop).
-export([init/1, probe_reset/0, probe_report/0, probe_stop_unknown_fd/0,
         probe_arm_then_stop/0, probe_double_stop/0, probe_cancel_then_stop/0,
         probe_stop_sleep_ms/1, probe_arm_then_stop_fallback/0,
         probe_stop_fallback_wait_ms/1, probe_read_on_pending_stop/0,
         probe_stop_nostop_type/0]).

init(Path) -> erlang:load_nif(Path, 0).

probe_reset() -> erlang:nif_error(nif_not_loaded).
probe_report() -> erlang:nif_error(nif_not_loaded).
probe_stop_unknown_fd() -> erlang:nif_error(nif_not_loaded).
probe_arm_then_stop() -> erlang:nif_error(nif_not_loaded).
probe_double_stop() -> erlang:nif_error(nif_not_loaded).
probe_cancel_then_stop() -> erlang:nif_error(nif_not_loaded).
probe_stop_sleep_ms(_Ms) -> erlang:nif_error(nif_not_loaded).
probe_arm_then_stop_fallback() -> erlang:nif_error(nif_not_loaded).
probe_stop_fallback_wait_ms(_Ms) -> erlang:nif_error(nif_not_loaded).
probe_read_on_pending_stop() -> erlang:nif_error(nif_not_loaded).
probe_stop_nostop_type() -> erlang:nif_error(nif_not_loaded).
"#;

/// The driver. `probe_reset/0` zeroes the fixture's statics between probes so each tuple
/// belongs to exactly one probe (the fixture's statics are module-global and the probes are
/// meant to be driven one at a time).
///
/// The descriptors and the recorded `LastEvent` are host-dependent, so they are reduced to
/// `is_integer/1` before printing; the callback's thread identity is printed on its own
/// `_callback_thread` line, which is the documented divergence.
const DRIVER_ERL: &str = r#"%% Generated by `beam_rt/tests/nif_a3_resource_stop.rs` (never committed).
-module(a3_resource_stop_driver).
-export([go/1]).

go(Path) ->
    ok = a3_resource_stop:init(Path),
    %% {Code, StopCalls, LastIsDirect, LastEvent, LastThreadType}
    ok = a3_resource_stop:probe_reset(),
    {C1, N1, D1, E1, T1} = a3_resource_stop:probe_stop_unknown_fd(),
    p("unknown_fd", {C1, N1, D1, is_integer(E1)}),
    p("unknown_fd_callback_thread", thr(T1)),
    %% {ArmCode, StopCode, StopCalls, LastIsDirect, LastThreadType, LastTag, Fd}
    ok = a3_resource_stop:probe_reset(),
    {A2, S2, N2, D2, T2, G2, F2} = a3_resource_stop:probe_arm_then_stop(),
    p("arm_then_stop", {A2, S2, N2, D2, G2, is_integer(F2)}),
    p("arm_then_stop_callback_thread", thr(T2)),
    %% {StopCalls, LastIsDirect, LastEvent, LastTag, LastThreadType, DtorCalls, DtorLastTag}
    {R3, Rd3, Ev3, Tg3, T3, Dt3, Dtg3} = a3_resource_stop:probe_report(),
    p("report_after_arm", {R3, Rd3, is_integer(Ev3), Tg3, Dt3, Dtg3}),
    p("report_after_arm_callback_thread", thr(T3)),
    %% {ArmCode, Stop1, Stop2, StopCalls, LastIsDirect, LastThreadType, LastTag}
    ok = a3_resource_stop:probe_reset(),
    {B1, B2, B3, B4, B5, B6, B7} = a3_resource_stop:probe_double_stop(),
    p("double_stop", {B1, B2, B3, B4, B5, B7}),
    p("double_stop_callback_thread", thr(B6)),
    %% {ArmCode, CancelCode, StopCode, StopCalls, LastIsDirect, LastThreadType, LastTag}
    ok = a3_resource_stop:probe_reset(),
    {K1, K2, K3, K4, K5, K6, K7} = a3_resource_stop:probe_cancel_then_stop(),
    p("cancel_then_stop", {K1, K2, K3, K4, K5, K7}),
    p("cancel_then_stop_callback_thread", thr(K6)),
    ok.

thr(0) -> undefined; thr(1) -> normal; thr(2) -> dirty_cpu; thr(3) -> dirty_io;
thr(X) -> {unknown, X}.

p(Name, Value) -> io:format("~s=~w~n", [Name, Value]).
"#;

/// Probes of this fixture that this suite deliberately leaves undriven, and why.
const UNDRIVEN: &[(&str, &str)] = &[
    (
        "probe_stop_nostop_type/0",
        "the resource type has NO stop callback and the C only ASSERTs that it has one \
         (erl_check_io.c:1279), which a release build compiles out: the oracle calls a NULL \
         function pointer and dies with SIGSEGV (exit 139, 3/3 measured runs). Never driven — \
         it would kill the oracle, not measure this runtime",
    ),
    (
        "probe_arm_then_stop_fallback/0, probe_stop_fallback_wait_ms/1, probe_stop_sleep_ms/1",
        "the DEFERRED stop arm (ERL_NIF_SELECT_STOP_SCHEDULED = bit 1) is unreachable here by \
         construction: every STOP is served synchronously (is_direct_call = true) through \
         beam_abi's nif::enif::select::service_stop_verdict, so nothing is handed to a poller \
         thread (src/nif_select.rs). The oracle's own answers are timing-dependent \
         ({0,18,...} or {0,2,...} depending on whether the poller drained the stop first)",
    ),
    (
        "probe_read_on_pending_stop/0",
        "an explicit race the fixture itself documents as 'both shapes reported, never \
         asserted': when the poller has not yet drained a pending stop a later READ answers \
         STOP_SCHEDULED (2), otherwise it is a fresh arm (0). Pinning either shape would \
         encode a race",
    ),
];

/// The measured transcript (OTP 29.1.1 / erts-17.1 is the oracle).
const EXPECTED: &[Expect] = &[
    Expect {
        key: "unknown_fd",
        oracle: "{1,1,1,true}",
        diverges: None,
    },
    Expect {
        key: "unknown_fd_callback_thread",
        oracle: "normal",
        diverges: Some((
            "undefined",
            "`enif_thread_type()` inside the `stop` callback: the oracle calls it on a \
             scheduler thread (ERL_NIF_THR_NORMAL_SCHEDULER = 1); this runtime calls it on \
             its own driver thread, which is not a registered scheduler thread, so the answer \
             is ERL_NIF_THR_UNDEFINED (0) — the callback itself still runs synchronously, \
             which the `LastIsDirect = 1` slot proves",
        )),
    },
    Expect {
        key: "arm_then_stop",
        oracle: "{0,17,1,1,2,true}",
        diverges: None,
    },
    Expect {
        key: "arm_then_stop_callback_thread",
        oracle: "normal",
        diverges: Some((
            "undefined",
            "`enif_thread_type()` inside the `stop` callback — scheduler thread (1) on the \
             oracle, this runtime's driver thread (0) here",
        )),
    },
    Expect {
        key: "report_after_arm",
        oracle: "{1,1,true,2,1,2}",
        diverges: None,
    },
    Expect {
        key: "report_after_arm_callback_thread",
        oracle: "normal",
        diverges: Some((
            "undefined",
            "`enif_thread_type()` recorded by the `stop` callback — scheduler thread (1) on \
             the oracle, this runtime's driver thread (0) here",
        )),
    },
    Expect {
        key: "double_stop",
        oracle: "{0,17,1,2,1,3}",
        diverges: None,
    },
    Expect {
        key: "double_stop_callback_thread",
        oracle: "normal",
        diverges: Some((
            "undefined",
            "`enif_thread_type()` inside the `stop` callback — scheduler thread (1) on the \
             oracle, this runtime's driver thread (0) here",
        )),
    },
    Expect {
        key: "cancel_then_stop",
        oracle: "{0,16,1,1,1,4}",
        diverges: None,
    },
    Expect {
        key: "cancel_then_stop_callback_thread",
        oracle: "normal",
        diverges: Some((
            "undefined",
            "`enif_thread_type()` inside the `stop` callback — scheduler thread (1) on the \
             oracle, this runtime's driver thread (0) here",
        )),
    },
];

/// Every STOP verdict on both runtimes, observation by observation.
#[test]
fn the_resource_stop_fixture_agrees_with_the_oracle_verdict_by_verdict() {
    let Some(tc) = nif_a3::toolchain("nif_a3_resource_stop") else {
        return;
    };
    let Some(fixture) = nif_a3::build(
        &tc,
        "resource_stop",
        &fixture_source(),
        "a3_resource_stop",
        &[
            ("a3_resource_stop", NIF_ERL),
            ("a3_resource_stop_driver", DRIVER_ERL),
        ],
        "a3_resource_stop_driver",
    ) else {
        return;
    };

    let rust = fixture.rust_transcript();
    let oracle = fixture.oracle_transcript(&tc);
    nif_a3::check("nif_a3_resource_stop", EXPECTED, &rust, &oracle);
    rust.print_notes("nif_a3_resource_stop");
    for (probe, why) in UNDRIVEN {
        println!("nif_a3_resource_stop: {probe} deliberately not driven: {why}");
    }

    fixture.cleanup();
}
