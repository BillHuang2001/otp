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
//! The **logger handler set** at the CLI boundary (contracts A.7/A.8).
//!
//! The oracle answers `logger:get_handler_ids/0` → `[default]` with a `logger_std_h` handler
//! over `standard_io`, because `kernel`'s start-up registers it
//! (`logger:add_handlers(kernel)`, `lib/kernel/src/logger.erl:1671-1700`) and that same call
//! **removes** `logger_server:init/1`'s `simple` fallback (`logger.erl:831-850`). This runtime
//! drives the boot script itself rather than completing `application:start_boot(kernel, …)`, so
//! `src/boot/bootscript.rs::execute_reporting` performs that one registration after the last
//! directive, through the real `logger:add_handler/3` — never a synthesized answer.
//!
//! What is pinned, from the outside:
//!
//! 1. the whole handler/primary-configuration answer is **byte-identical to the live oracle**
//!    ([`the_default_handler_is_byte_identical_to_the_live_oracle`]): the id set, the module,
//!    level, `filter_default`, the `standard_io` device type, the formatter tuple, the three
//!    default filters, `get_handler_config(simple)`'s `{error,{not_found,simple}}`,
//!    `get_primary_config/0` and `is_pid(whereis(logger))`;
//! 2. **every route that boots** leaves that set — `-eval`, `-s`, `-run`, the `run` action and
//!    the explicit `-boot` probe ([`every_boot_route_leaves_the_default_handler`]);
//! 3. a *second* `logger:add_handler(default, …)` is refused by OTP with
//!    `{error,{already_exist,default}}` and the set stays `[default]` — the idempotency the
//!    post-boot step relies on ([`a_second_registration_does_not_duplicate_the_default_handler`]);
//! 4. `init:restart/0` re-boots onto exactly one `default` handler
//!    ([`init_restart_keeps_exactly_one_default_handler`]) — no duplicate, no failure;
//! 5. a `logger:error/1`-class call **is emitted by the default handler**, and the emitted block
//!    is byte-identical to the live oracle's once its wall-clock header is elided
//!    ([`a_log_event_reaches_the_default_handler`] — the flush makes the emission an observable);
//! 6. the same call **never takes the action down**, whether or not the asynchronous handler won
//!    the race against the final `halt/0` ([`a_log_event_does_not_crash_the_action`], which
//!    pins the exit code and the `survived` effect only — never the racy tail).
//!
//! # Why the report's appearance is pinned *with a flush*, and the crash check without one
//!
//! `default` is a `logger_std_h` handler behind `logger_olp`: `logger:error/1` only **queues**
//! the event, so `halt(0)` can end the node before the handler process writes anything.
//! Measured on this host: `logger:error("boom"), io:format("survived~n"), halt(0).` prints the
//! report **0/10** runs here against the oracle's **9/10** — an asynchronous handler racing
//! `halt/0`, not a divergence in the handler path. `logger_std_h:filesync/1` is the handler's
//! own flush call (a `gen_server:call` the handler answers `ok` on both runtimes), and with it
//! the identical block appears **10/10** on both sides, so [`a_log_event_reaches_the_default_handler`]
//! is what pins the block's *content* and [`a_log_event_does_not_crash_the_action`] pins only the
//! invariant the racy form can honestly support.
//!
//! # Why no map is ever rendered whole
//!
//! `~p` renders a small map in its **internal** key order, which ERTS does not keep stable
//! across VMs: the oracle alternates `#{single_line => …,legacy_header => …}` with the reverse
//! (2/40 runs measured) and does the same inside `get_primary_config/0`. Map iteration order is
//! not part of the contract here, so every map in [`HANDLER_STATE_EXPR`] is rendered as its
//! **sorted** `{Key, Value}` pairs — every key and every value still compared, in one fixed order.
//!
//! Tests that compare against the oracle skip **loudly** when no `erl` is installed
//! ([`support::require_oracle`]); the pinned literals they assert first keep each case
//! discriminating on such a host.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};

use support::{differential_eval, run_runtime, ErlangFixture};

/// The committed `bootstrap/bin/start.boot` base (no extension — `resolve_boot_file` appends
/// `.boot`), as an absolute path so the probe does not depend on the test's CWD.
fn start_boot_base() -> PathBuf {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("the repository root is three levels above the crate")
        .join("bootstrap/bin/start.boot");
    let mut base = script.to_string_lossy().into_owned();
    if let Some(stripped) = base.strip_suffix(".boot") {
        base.truncate(stripped.len());
    }
    PathBuf::from(base)
}

// ---------------------------------------------------------------------------------------
// The two maps are compared as sorted pairs, never as maps
// ---------------------------------------------------------------------------------------

/// The one `~p` render the whole handler state is compared in: the id set, then the `default`
/// handler's module/level/filter-default/device-type/formatter/filters, then
/// `get_handler_config(simple)`, `get_primary_config/0` and the registered-server check.
///
/// The tuple shape is deliberate: `get_handler_config(default)`'s own map renders its keys in a
/// different order in the two runtimes (map iteration order is not part of the contract), while
/// every **value** here is — and the two maps that do appear (`formatter`'s options and the
/// primary config) are rendered as their **sorted** `{Key, Value}` pairs, so the oracle's
/// alternation between two internal key orders cannot flip the verdict.
const HANDLER_STATE_EXPR: &str = concat!(
    "{ok, C} = logger:get_handler_config(default), ",
    "Type = maps:get(type, maps:get(config, C)), ",
    "{FMod, FCfg} = maps:get(formatter, C), ",
    "Fmt = {FMod, [{K, maps:get(K, FCfg)} || K <- lists:sort(maps:keys(FCfg))]}, ",
    "PC = logger:get_primary_config(), ",
    "Primary = [{K, maps:get(K, PC)} || K <- lists:sort(maps:keys(PC))], ",
    "io:format(\"~p~n\", [{logger:get_handler_ids(), maps:get(module, C), maps:get(level, C), ",
    "maps:get(filter_default, C), Type, Fmt, maps:get(filters, C), ",
    "logger:get_handler_config(simple), Primary, ",
    "is_pid(whereis(logger))}]), halt(0)."
);

/// The oracle's answer to [`HANDLER_STATE_EXPR`], byte for byte (verified against OTP
/// 29.1.2 / erts-17.1.1 and reproduced by this runtime; the oracle's own internal map order
/// alternates, which is exactly why the maps are printed as sorted pairs).
const HANDLER_STATE_EXPECTED: &str = "\
{[default],
 logger_std_h,all,stop,standard_io,
 {logger_formatter,[{legacy_header,true},{single_line,false}]},
 [{remote_gl,{fun logger_filters:remote_gl/2,stop}},
  {domain,{fun logger_filters:domain/2,{log,super,[otp,sasl]}}},
  {no_domain,{fun logger_filters:domain/2,{log,undefined,[]}}}],
 {error,{not_found,simple}},
 [{filter_default,log},{filters,[]},{level,notice},{metadata,#{}}],
 true}
";

/// The fixture the route test drives on `-s`/`-run`/`run`: printing the handler set is the
/// observable, and `ok` is what the `run` action renders as its result.
const LOGGER_PROBE_ERL: &str = r#"%% Generated by `beam_rt/tests/logger_default_handler.rs` (never committed).
-module(logger_probe).
-export([go/0]).
go() ->
    io:format("~p~n", [logger:get_handler_ids()]),
    ok.
"#;

// ---------------------------------------------------------------------------------------
// The report block, and why its header is elided
// ---------------------------------------------------------------------------------------

/// A default-handler report's header with the wall-clock timestamp elided: two processes cannot
/// agree on a timestamp, and nothing else in the header is machine-dependent.
const REPORT_HEADER: &str = "=ERROR REPORT==== <timestamp> ===";

/// Rewrites every report header's timestamp to [`REPORT_HEADER`], leaving every other byte —
/// the reason line included — verbatim, so a dropped or mangled report still fails any
/// comparison made through this function.
fn normalize_report_timestamp(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let header = line
                .strip_prefix("=ERROR REPORT==== ")
                .and_then(|rest| rest.strip_suffix(" ==="));
            if header.is_some() {
                REPORT_HEADER
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `logger:error/1`, then the handler's own **flush** (`logger_std_h:filesync/1`, a
/// `gen_server:call` the handler answers `ok` on both runtimes), then the observable effect.
/// The flush is what makes the emission — not merely the call — an observable.
const LOG_REPORT_FLUSH_EXPR: &str = concat!(
    "logger:error(\"boom\"), ",
    "logger_std_h:filesync(default), ",
    "io:format(\"survived~n\"), halt(0)."
);

/// This runtime's (and the oracle's) answer to [`LOG_REPORT_FLUSH_EXPR`], with the header's
/// timestamp elided: the default handler's report for `logger:error("boom")` is the legacy
/// `=ERROR REPORT==== … ===` header plus the message, and the action then survives.
const LOG_REPORT_FLUSH_EXPECTED: &str = "\
=ERROR REPORT==== <timestamp> ===
boom
survived";

/// The **racy** form: no flush, so the asynchronous handler and the final `halt/0` race — the
/// report may or may not reach stdout (measured 0/10 here, 9/10 on the oracle). Only the
/// invariant is pinned from it.
const LOG_EVENT_RACY_EXPR: &str = "logger:error(\"boom\"), io:format(\"survived~n\"), halt(0).";

// ---------------------------------------------------------------------------------------
// 1. The whole answer, against the live oracle
// ---------------------------------------------------------------------------------------
#[test]
fn the_default_handler_is_byte_identical_to_the_live_oracle() {
    let probe = differential_eval(
        "logger/default_handler",
        "the handler set + the `default` handler's config + the primary config",
        HANDLER_STATE_EXPR,
    );
    assert_eq!(
        probe.stdout_text(),
        HANDLER_STATE_EXPECTED,
        "the rendered handler state (stderr: {})",
        probe.stderr_text()
    );
}

// ---------------------------------------------------------------------------------------
// 2. Every boot route leaves the same set
// ---------------------------------------------------------------------------------------
#[test]
fn every_boot_route_leaves_the_default_handler() {
    // `-eval`: the action route.
    let eval = differential_eval(
        "logger/route_eval",
        "`-eval` asks for the handler ids",
        "io:format(\"~p~n\", [logger:get_handler_ids()]), halt(0).",
    );
    assert_eq!(eval.stdout_text(), "[default]\n", "`-eval`");

    // The explicit boot-script probe, which names the committed script itself.
    let boot_base = start_boot_base();
    let boot_base = boot_base.to_string_lossy().into_owned();
    let booted = run_runtime(&[
        "-noshell",
        "-boot",
        &boot_base,
        "-eval",
        "io:format(\"~p~n\", [logger:get_handler_ids()])",
    ])
    .expect("the `-boot` probe runs to completion");
    assert_eq!(booted.code, 0, "`-boot` exit (stderr: {})", booted.stderr);
    assert_eq!(booted.stdout, "[default]\n", "`-boot` stdout");

    // `-s`, `-run` and the `run` action, through a fixture compiled at test time.
    let Some(fixture) =
        ErlangFixture::compile("logger/routes", "logger_probe", "logger_probe", LOGGER_PROBE_ERL)
    else {
        return;
    };
    let dir = fixture.dir().to_string_lossy().into_owned();
    let via_s = run_runtime(&["-noshell", "-pa", &dir, "-s", "logger_probe", "go"])
        .expect("the `-s` probe runs to completion");
    assert_eq!(via_s.code, 0, "`-s` exit (stderr: {})", via_s.stderr);
    assert_eq!(via_s.stdout, "[default]\n", "`-s` stdout");
    let via_run = run_runtime(&["-noshell", "-pa", &dir, "-run", "logger_probe", "go"])
        .expect("the `-run` probe runs to completion");
    assert_eq!(via_run.code, 0, "`-run` exit (stderr: {})", via_run.stderr);
    assert_eq!(via_run.stdout, "[default]\n", "`-run` stdout");
    // The `run` action prints the function's result (`ok`) after the fixture's own line.
    let beam = fixture.beam().to_string_lossy().into_owned();
    let via_action = run_runtime(&["-noshell", "run", &beam, "logger_probe:go/0"])
        .expect("the `run` action runs to completion");
    assert_eq!(
        via_action.code, 0,
        "`run` exit (stderr: {})",
        via_action.stderr
    );
    assert_eq!(via_action.stdout, "[default]\nok\n", "`run` stdout");

    // The same `-s` route under the oracle, so the pinned line is the oracle's own.
    if let Some(oracle) = support::require_oracle("logger/routes_oracle") {
        let oracle_line = support::oracle_eval(
            &oracle,
            Some(Path::new(&dir)),
            "io:format(\"~p~n\", [logger:get_handler_ids()]), halt(0).",
        );
        assert_eq!(
            oracle_line.as_deref(),
            Some("[default]"),
            "the oracle's handler ids on the same code path"
        );
    }
}

// ---------------------------------------------------------------------------------------
// 3. A duplicate registration is refused, and the set does not grow
// ---------------------------------------------------------------------------------------
#[test]
fn a_second_registration_does_not_duplicate_the_default_handler() {
    let probe = differential_eval(
        "logger/duplicate_registration",
        "a second `logger:add_handler(default, …)` and the resulting id set",
        concat!(
            "R = logger:add_handler(default, logger_std_h, #{config => #{type => standard_io}}), ",
            "io:format(\"~p~n\", [{R, logger:get_handler_ids()}]), halt(0)."
        ),
    );
    assert_eq!(
        probe.stdout_text(),
        "{{error,{already_exist,default}},[default]}\n",
        "a re-registration is refused and the set stays single (stderr: {})",
        probe.stderr_text()
    );
}

// ---------------------------------------------------------------------------------------
// 4. `init:restart/0` re-boots onto exactly one handler
// ---------------------------------------------------------------------------------------
#[test]
fn init_restart_keeps_exactly_one_default_handler() {
    // No oracle comparison: the oracle's `restart/0` hands the reboot to its own `init`
    // process, which tears down the `-eval` process that asked — so the oracle cannot answer
    // the same question after the restart. This runtime's in-process reboot
    // (`Runtime::restart`) does, and the pinned pair is the claim.
    let restarted = run_runtime(&[
        "-noshell",
        "-eval",
        concat!(
            "io:format(\"~p~n\", [logger:get_handler_ids()]), ",
            "io:format(\"~p~n\", [init:restart()]), ",
            "{ok, C} = logger:get_handler_config(default), ",
            "io:format(\"~p~n\", [{logger:get_handler_ids(), maps:get(module, C)}])"
        ),
    ])
    .expect("the restart probe runs to completion");
    assert_eq!(
        restarted.code, 0,
        "restart exit (stderr: {})",
        restarted.stderr
    );
    assert_eq!(
        restarted.stdout, "[default]\nok\n{[default],logger_std_h}\n",
        "one `default` handler before and after the in-process reboot"
    );
}

// ---------------------------------------------------------------------------------------
// 5. A log event IS emitted by the default handler — flush, then compare the block
// ---------------------------------------------------------------------------------------
#[test]
fn a_log_event_reaches_the_default_handler() {
    // The flush (`LOG_REPORT_FLUSH_EXPR`) is what removes the race with `halt/0`: measured
    // 10/10 runs with it, 0/10 without it on this runtime. The comparison is byte-identical
    // apart from the header's wall-clock timestamp, which no two processes can share
    // (`normalize_report_timestamp`).
    let probe = run_runtime(&["-noshell", "-eval", LOG_REPORT_FLUSH_EXPR])
        .expect("the flushed log-event probe runs to completion");
    assert_eq!(
        probe.code, 0,
        "the flushed log-event probe exits 0 (stderr: {})",
        probe.stderr
    );
    let ours = normalize_report_timestamp(&probe.stdout);
    assert_eq!(
        ours.trim_end(),
        LOG_REPORT_FLUSH_EXPECTED,
        "the report the default handler emitted for `logger:error(\"boom\")` (stderr: {})",
        probe.stderr
    );

    let Some(oracle) = support::require_oracle("logger/log_event_report") else {
        return;
    };
    let theirs = support::oracle_eval(&oracle, None, LOG_REPORT_FLUSH_EXPR)
        .expect("the oracle answers the same flushed log-event probe");
    assert_eq!(
        normalize_report_timestamp(&theirs).trim_end(),
        ours.trim_end(),
        "the emitted block is byte-identical to the live oracle's (header timestamp elided)"
    );
}

// ---------------------------------------------------------------------------------------
// 6. The *racy* form still never crashes the action (never byte-compared)
// ---------------------------------------------------------------------------------------
#[test]
fn a_log_event_does_not_crash_the_action() {
    // No flush: the `default` handler is asynchronous (`logger_olp`), so whether the event
    // reaches a device before `halt/0` is a race — measured **0/10** here against the oracle's
    // **9/10**, which is why the racy tail can never be byte-compared (and no oracle is asked
    // here). What is pinned is the invariant that holds either way: the call returns, the
    // action exits 0, and the `survived` effect happens — preceded by nothing or by the very
    // report shape [`a_log_event_reaches_the_default_handler`] pins in full.
    let probe = run_runtime(&["-noshell", "-eval", LOG_EVENT_RACY_EXPR])
        .expect("the racy log-event probe runs to completion");
    assert_eq!(
        probe.code, 0,
        "the action survives a log event (stderr: {})",
        probe.stderr
    );
    let observed = normalize_report_timestamp(&probe.stdout);
    let with_report = format!("{REPORT_HEADER}\nboom\nsurvived\n");
    assert!(
        observed == "survived\n" || observed == with_report,
        "the racy action's stdout is either the bare effect or the report block plus it — \
         got {observed:?} (stderr: {})",
        probe.stderr
    );
}
