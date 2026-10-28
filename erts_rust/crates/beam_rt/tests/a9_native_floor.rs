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

//! The **A.9 native-floor suite** (contract A.9, build/bootstrapping): the runtime-native
//! floor entries that belong to the *preloaded-module* surface — the boot script, the code
//! server's installation, the `init` command line, the application-config view — driven both
//! through a real [`Runtime`] and through the **built `beam_rt` binary**, and cross-checked
//! against the **live system `erl` oracle**.
//!
//! > A.9's differential coverage is boot/version **observability** only. This suite pins the
//! > observable surface those floor entries expose; a missing toolchain (or an oracle probe
//! > that cannot answer) is a **loud skip** ([`support`]), never a silent pass.
//!
//! # What it pins
//!
//! | Surface | Shape |
//! |---|---|
//! | `application:*` (the **live** controller) | `set_env/4` → `get_env/2` visibility, the environment incl. kernel's injected `logger`/`connect_all`, the 13-key `get_all_key/1` order, the `get_key/2` defaults, the module → application map, the pid clause, and both application lists |
//! | `application:which_applications/0`, `loaded_applications/0` | the live `application_controller`'s two triples in its own orders (`stdlib`,`kernel` / `kernel`,`stdlib`) |
//! | `init:get_argument/1` | the four automatic flags (`root`, `home`, `progname`, `bindir`) and an absent flag |
//! | `init:script_id/0` | the **loaded** boot script's real header on the default boot path, and the typed gap for a runtime that drives no script |
//! | `code:lib_dir/2` | the app dir joined with any subdir, `{error,bad_name}`, `function_clause` |
//! | `filelib:is_dir/1, is_file/1, is_regular/1, wildcard/1` | the absent-path answers and the served success path (a real `#file_info{}`) |
//!
//! `erlang:module_loaded/1`'s boot-set divergence is **not** re-tested here — it is pinned by the
//! sibling [`module_loaded.rs`](module_loaded) suite; this suite only adds the complementary
//! observation that a *boot-script* invocation really loads the `primLoad` batch
//! ([`script_id_is_the_loaded_boot_scripts_real_header`]).
//!
//! Every rendering compared here comes from the **same printer** on both sides: this runtime's
//! `~p` drives the registered `io:format/2`, so [`agrees_with_the_oracle`] (the in-process route)
//! and [`agrees_on_the_booted_route`] (the built binary, after the boot script) are byte
//! comparisons of the two answers, never hardcoded expectations.
//!
//! # The routes these values are measured on
//!
//! Every CLI action **but `-version`** drives a boot script before its own start instruction
//! (`cli.rs::boot_script_to_drive` + `boot_then`, mirroring `init`'s `bootfile/2` + `do_boot/3`)
//! — the explicit `-boot FILE` value when one was given, else this runtime's **own** release's
//! committed `bootstrap/bin/start.boot` — so a `-eval`/`-s`/`run` node now carries the booted
//! supervision tree the oracle
//! has — a live `kernel_sup`, `application_controller`, `logger` and `file_server_2` — rather than
//! the preloaded-only floor an earlier runtime reported. The in-process
//! [`Runtime::with_defaults`] route ([`renders`]) drives **no** script (it boots the preloaded set
//! only), so an `application:*` query there has no `ac_tab` to read: every `application:*`
//! assertion below therefore uses [`renders_booted`] / [`agrees_on_the_booted_route`], the built
//! binary's booted route — the only route on which a live `application_controller` exists on
//! either side (`erl -noshell` always boots).
//!
//! # Where the values diverge **by construction** (never a bug)
//!
//! Each is marked `// documented divergence:` where it is asserted, and every one is an answer
//! that reads *this* runtime's own state truthfully:
//!
//! * **`application:*`'s loaded-release `vsn`.** Every `application:*` query is now the live
//!   `application_controller`'s — there is no `.app`-resource floor left — so the environment it
//!   reports is the running node's (nine `kernel` pairs, the injected `logger`/`connect_all`
//!   included) and a `set_env/4` is visible to the next `get_env/2`. The one value that can still
//!   differ from the oracle's is a **version**: the default route loads *this runtime's own*
//!   release's boot script, which embeds this checkout's `stdlib` version (`8.1`) where the
//!   oracle's embeds its installation's (`8.1.1`) — the same relationship `init:script_id/0`
//!   pins. `kernel`'s version happens to coincide, and every name/description/order is compared
//!   with the oracle outright.
//! * **`init:script_id/0` on the default boot path.** The default boot script really is loaded, so
//!   there *is* an id: the **loaded** script's real `{Name, Vsn}` header, never a fabricated one.
//!   The typed capability gap survives only for a runtime that drives **no** script (the plain
//!   in-process [`Runtime::with_defaults`] route).
//! * **`init:script_id/0`'s value is the *loaded script's* header.** With no `-boot`, the default
//!   route loads **this runtime's own** release's script — the committed
//!   `bootstrap/bin/start.boot`, discovered from the binary's source tree by
//!   `CodeServer::own_bootstrap_boot_script` — so the id is that file's `{Name, Vsn}` header
//!   (`{"Erlang/OTP","30"}`), matching the compile-time `erlang:system_info(otp_release)` (`"30"`).
//!   An explicit `-boot FILE` loads the named script instead. Neither test hardcodes the header:
//!   the `-boot FILE` test reads it out of the very script the runtime loads, and the default-route
//!   test compares against this checkout's committed bootstrap header plus the self-consistency
//!   predicate the differential case pins. Note that this is *our* id: the oracle's own default
//!   route loads *its* installation's script (`{"Erlang/OTP","29"}` on this host), so the two ids
//!   are **not** compared byte-for-byte — the invariant both sides satisfy is
//!   `init:script_id() =:= {"Erlang/OTP", erlang:system_info(otp_release)}`.
//! * **`filelib` on an existing path.** The `#file_info{}` `access` slot is `access(2)`-derived
//!   in the oracle and served here through `beam_io`'s `access(2)` probe, so the success path
//!   answers the oracle's own value (`filelib:is_dir/1` ⇒ `true`) rather than a typed gap.
//!
//! # Toolchain
//!
//! The probe module is compiled **at test time**; the oracle is resolved from the filesystem
//! (never run merely to probe existence) and every probe is bounded (`tests/support/mod.rs`). The
//! shared `tests/fixtures/` (which holds a shadowing `lists.beam`) is never handed to the oracle;
//! the one probe module this suite compiles lives in its own scratch directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_engine::terms;
use beam_rt::Runtime;
use beam_term::{signed_value, Eterm};

#[allow(dead_code)]
mod support;

// ---------------------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------------------

/// The suite name every loud skip names.
const BANNER: &str = "a9_native_floor";

/// The live `erl` oracle, resolved **once** (so a missing toolchain is a single loud skip rather
/// than one banner per comparison).
fn oracle() -> Option<PathBuf> {
    static ORACLE: OnceLock<Option<PathBuf>> = OnceLock::new();
    ORACLE.get_or_init(|| support::require_oracle(BANNER)).clone()
}

/// The value of `call` on `runtime`. A raise is a test failure — the callers that expect one use
/// [`raise_of`].
fn value_of_in(runtime: &mut Runtime, call: &str) -> Eterm {
    runtime
        .eval(call)
        .unwrap_or_else(|error| panic!("`{call}` raised {error:?}"))
}

/// The `~p` rendering of `value` under `runtime`, trimmed.
fn rendered(runtime: &mut Runtime, value: Eterm) -> String {
    runtime
        .format_term(value, None)
        .expect("a printable term")
        .trim_end()
        .to_string()
}

/// Renders `call` on a fresh default runtime: `~p` of its value, trimmed.
fn renders(call: &str) -> String {
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    let value = value_of_in(&mut runtime, call);
    rendered(&mut runtime, value)
}

/// The rendering with the printer's line wrapping undone: both runtimes' `~p` break a term longer
/// than the printer's line width onto continuation lines, so a comparison against a single-line
/// expectation must join them back. Only the layout is dropped — the atoms carry no spaces.
fn unwrapped(text: &str) -> String {
    text.lines().map(str::trim_start).collect::<String>()
}

/// The error `call` raises, asserting it really raises.
fn raise_of(call: &str) -> BeamError {
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    runtime
        .eval(call)
        .expect_err(&format!("`{call}` must raise"))
}

/// Asserts this runtime renders `call` **exactly** as the live oracle does, and returns the
/// rendering.
///
/// A missing oracle — or one that cannot answer — is a loud skip: the runtime's own rendering is
/// still returned, so the caller's assertions still run instead of passing silently.
fn agrees_with_the_oracle(call: &str) -> String {
    let rendered = renders(call);
    let Some(expected) = oracle_renders(call) else {
        return rendered;
    };
    assert_eq!(rendered, expected, "the oracle renders `{call}` differently");
    rendered
}

/// The oracle's own `~p` rendering of `call`, or `None` — loudly — when it cannot answer.
fn oracle_renders(call: &str) -> Option<String> {
    let oracle = oracle()?;
    let probe = format!("io:format(\"~p\", [{call}])");
    match support::oracle_eval(&oracle, None, &probe) {
        Some(answer) => Some(answer),
        None => {
            eprintln!("warning: the oracle could not answer `{call}`");
            None
        }
    }
}
// ---------------------------------------------------------------------------------------
// The booted route
// ---------------------------------------------------------------------------------------
/// The `~p` rendering of `call` on the **booted** route: the built `beam_rt` binary, which drives
/// the default boot script before its action, so the live `kernel_sup` / `application_controller`
/// the caller needs really exists.
///
/// The in-process [`Runtime::with_defaults`] route boots the preloaded set only. It has **no**
/// `application_controller` and therefore no `ac_tab`, so an `application:*` query there is an
/// honest `badarg` — which is why every `application:*` assertion below uses this route instead
/// of [`renders`].
fn renders_booted(call: &str) -> String {
    let cli = beam_rt_eval(&format!("io:format(\"~p\", [{call}])."));
    assert_eq!(
        cli.code, 0,
        "`{call}` on the booted route must answer (stderr {:?})",
        cli.err
    );
    cli.out.trim_end().to_string()
}
/// Asserts the **booted** route renders `call` exactly as the live oracle does, and returns the
/// rendering. Line wrapping is undone on both sides ([`unwrapped`]), since a long term is broken
/// across lines by whichever printer is closer to the width limit.
///
/// A missing oracle — or one that cannot answer — is a loud skip: the runtime's own rendering is
/// still returned, so the caller's assertions still run instead of passing silently.
fn agrees_on_the_booted_route(call: &str) -> String {
    let rendered = renders_booted(call);
    let Some(expected) = oracle_renders(call) else {
        return rendered;
    };
    assert_eq!(
        unwrapped(&rendered),
        unwrapped(&expected),
        "the oracle renders `{call}` differently"
    );
    rendered
}

/// Runs the arbitrary Erlang `program` (which must terminate on its own) under the oracle and
/// returns its trimmed stdout, or `None` — loudly — without a working toolchain.
fn oracle_program(program: &str) -> Option<String> {
    let oracle = oracle()?;
    match support::oracle_eval(&oracle, None, program) {
        Some(answer) => Some(answer),
        None => {
            eprintln!("warning: the oracle could not run `{program}`");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------
// The CLI probe
// ---------------------------------------------------------------------------------------

/// One finished `beam_rt` invocation: its exit code and captured output.
struct Cli {
    code: i32,
    out: String,
    err: String,
}

/// Runs the built `beam_rt` binary with `args`, bounded so a probe that blocks cannot hang the
/// suite. `ERL_CRASH_DUMP_SECONDS=0` keeps a failing boot from littering the working directory.
fn beam_rt(args: &[&str]) -> Cli {
    let mut command = Command::new(env!("CARGO_BIN_EXE_beam_rt"));
    command.env("ERL_CRASH_DUMP_SECONDS", "0");
    command.args(args);
    let output = run_bounded(command);
    Cli {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim_end().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim_end().to_string(),
    }
}

/// The same, for the `-eval` action.
fn beam_rt_eval(expr: &str) -> Cli {
    beam_rt(&["-noshell", "-eval", expr])
}

/// Spawns `command` with `/dev/null` as stdin, polling so a child that blocks is killed and
/// reaped at [`support::PROBE_TIMEOUT`] instead of hanging the caller.
fn run_bounded(mut command: Command) -> Output {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("spawning `{program}` failed: {error}"));
    let deadline = Instant::now() + support::PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .unwrap_or_else(|error| panic!("collecting `{program}` failed: {error}"))
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "`{program}` outlived the {}s probe bound",
                        support::PROBE_TIMEOUT.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("polling `{program}` failed: {error}");
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Term helpers
// ---------------------------------------------------------------------------------------

/// An Erlang char list as a Rust `String`.
fn charlist(term: Eterm) -> String {
    terms::list_elements(term)
        .expect("a char list")
        .iter()
        .map(|character| {
            let code = u32::try_from(signed_value(*character)).expect("a code point");
            char::from_u32(code).expect("a char")
        })
        .collect()
}

/// The **one** value of `init:get_argument(Flag)`'s `{ok, [[Value]]}` answer, or `None` when the
/// runtime answers the atom `error` (no value for the flag).
fn single_argument(runtime: &mut Runtime, flag: &str) -> Option<String> {
    let call = format!("init:get_argument({flag})");
    let value = value_of_in(runtime, &call);
    if runtime.atom_name_of(value) == Some("error") {
        return None;
    }
    assert_eq!(terms::tuple_arity(value).expect("a 2-tuple"), 2);
    let occurrences = terms::list_elements(terms::tuple_element(value, 1).expect("the values"))
        .expect("an occurrence list");
    assert_eq!(occurrences.len(), 1, "`{flag}` holds one occurrence");
    let values = terms::list_elements(occurrences[0]).expect("the occurrence's values");
    assert_eq!(values.len(), 1, "`{flag}` holds one value");
    Some(charlist(values[0]))
}

// ---------------------------------------------------------------------------------------
// 0. the `application` namespace is the LIVE controller's, not a floor
// ---------------------------------------------------------------------------------------
/// This checkout's `kernel` release (`lib/kernel/vsn.mk`, embedded in the committed
/// `bootstrap/bin/start.boot` the default route loads).
const KERNEL_VSN: &str = "11.0.4";
/// This checkout's `stdlib` release (`lib/stdlib/vsn.mk`, likewise embedded in *our* start
/// script). The oracle reports the version of **its** installation instead (`8.1.1` on an
/// OTP-29 host), exactly as `init:script_id/0` reports `{"Erlang/OTP","30"}` here and
/// `{"Erlang/OTP","29"}` there: each runtime answers for the release it loaded. The name, the
/// description and the ordering are compared with the oracle outright; only this literal is
/// the local release's.
const STDLIB_VSN: &str = "8.1";
/// `application:set_env/4` writes the live `application_controller`'s `ac_tab`; the very next
/// `application:get_env/2` must see it. This is the regression the withdrawn `.app`-resource
/// floor introduced, and the reason it was withdrawn: the writer was never floored (it is a
/// `gen_server:call` into the controller) while the reader was, so a value the running node
/// had just set was invisible to the read that followed.
///
/// `unset_env/2` restores the environment, so the probe leaves nothing behind.
#[test]
fn set_env_is_visible_to_get_env() {
    let expr = "ok = application:set_env(kernel, beam_rt_probe_key, {visible, 42}),\
                Answer = application:get_env(kernel, beam_rt_probe_key),\
                ok = application:unset_env(kernel, beam_rt_probe_key),\
                Gone = application:get_env(kernel, beam_rt_probe_key),\
                io:format(\"~p\", [{Answer, Gone}])";
    let cli = beam_rt_eval(expr);
    assert_eq!(cli.code, 0, "the probe exits 0 (stderr {:?})", cli.err);
    assert_eq!(cli.out.trim_end(), "{{ok,{visible,42}},undefined}");
    // … and the oracle answers the same pair, so this is the oracle's semantics, not a shape
    // this runtime happens to produce.
    let Some(expected) = oracle_program(expr) else {
        return;
    };
    assert_eq!(cli.out.trim_end(), expected, "`set_env/4` → `get_env/2` on the oracle");
}
/// Every `application:*` query is a `lib/kernel/src/application.erl` one-line delegation to
/// the **live** `application_controller`, whose `ac_tab` carries the environment the booted
/// `kernel` really injected (`logger`, `connect_all`, …). The withdrawn floor answered from the
/// parsed `.app` resource instead, so the environment it reported was the *file's* seven
/// entries and it never saw a `set_env/4`. Both halves are now the server's:
/// `get_all_env(kernel)` lists the same nine pairs as the oracle (order unpinned — it is the
/// controller's own `ets:match` order), and `get_env(kernel, logger)`/`(kernel, connect_all)`
/// answer their injected values.
#[test]
fn get_env_and_get_all_env_are_the_live_controllers_environment() {
    // `stdlib` declares no environment, so both runtimes answer `[]` outright.
    assert_eq!(agrees_on_the_booted_route("application:get_all_env(stdlib)"), "[]");
    // The injected entries: `connect_all` and `logger` are set by `kernel`'s own start, never
    // by its resource file.
    assert_eq!(
        agrees_on_the_booted_route("application:get_env(kernel, connect_all)"),
        "{ok,true}"
    );
    assert_eq!(
        agrees_on_the_booted_route("application:get_env(kernel, net_ticktime)"),
        "{ok,60}"
    );
    assert_ne!(
        agrees_on_the_booted_route("application:get_env(kernel, logger)"),
        "undefined",
        "kernel's start injects the default `logger` handler configuration"
    );
    // The whole environment: nine pairs in both runtimes (the file declares seven; the
    // controller's `kernel` start adds `logger` and `connect_all`). Compared as a sorted list
    // because the traversal order is the controller's own `ets:match` order, which no contract
    // pins — the two ETS implementations enumerate it differently.
    assert_eq!(agrees_on_the_booted_route("length(application:get_all_env(kernel))"), "9");
    let _ = agrees_on_the_booted_route("lists:sort(application:get_all_env(kernel))");
    // The three-arity form falls back to its default exactly like the oracle.
    for call in [
        "application:get_env(kernel, nope_key, def)",
        "application:get_env(kernel, net_ticktime, def)",
        "application:get_env(nosuchapp_xyz, key, def)",
        "application:get_env(kernel, 42, def)",
    ] {
        let _ = agrees_on_the_booted_route(call);
    }
    // A non-atom argument never raises (`application.erl`'s guards leave such a call to the
    // controller's plain lookups, which simply miss) — the answer is the not-found shape.
    for call in [
        "application:get_env(42)",
        "application:get_env(42, key)",
        "application:get_env(kernel, 42)",
    ] {
        assert_eq!(agrees_on_the_booted_route(call), "undefined", "`{call}`");
    }
    assert_eq!(agrees_on_the_booted_route("application:get_all_env(42)"), "[]");
}
/// `application:get_env/1` — the calling process's *own* application, resolved through its
/// group leader (`application.erl:927`) — is `undefined` in both runtimes: a `-noshell`
/// process belongs to none.
#[test]
fn get_env_1_is_undefined_in_both_runtimes() {
    for argument in ["kernel", "42"] {
        let call = format!("application:get_env({argument})");
        assert_eq!(agrees_on_the_booted_route(&call), "undefined", "`{call}`");
    }
}
// ---------------------------------------------------------------------------------------
// 1. application:get_all_key/1
// ---------------------------------------------------------------------------------------
/// `application:get_all_key/1`'s key order is the application controller's own
/// (`application_controller.erl:412-446`), not the resource file's declaration order.
const APPLICATIONS_CONTROLLER_KEYS: &str = "[description,id,vsn,modules,maxP,maxT,registered,\
                                            included_applications,optional_applications,\
                                            applications,env,mod,start_phases]";
#[test]
fn get_all_key_reports_the_loaded_applications_in_the_controller_key_order() {
    // The gate is the oracle's `ets:lookup(ac_tab, {loaded, AppName})`: only an application this
    // runtime *is* answers its keys.
    for app in ["kernel", "stdlib"] {
        let call = format!("[K || {{K,_}} <- element(2, application:get_all_key({app}))]");
        assert_eq!(
            unwrapped(&agrees_on_the_booted_route(&call)),
            unwrapped(APPLICATIONS_CONTROLLER_KEYS),
            "`application:get_all_key({app})`'s key order"
        );
        let call = format!("length(element(2, application:get_all_key({app})))");
        assert_eq!(agrees_on_the_booted_route(&call), "13", "`{app}` reports 13 keys");
    }
    // The three keys the A.9 surface names, on the loaded pair.
    for (app, key) in [("kernel", "modules"), ("kernel", "env"), ("kernel", "mod")] {
        let call = format!("lists:keymember({key}, 1, element(2, application:get_all_key({app})))");
        assert_eq!(agrees_on_the_booted_route(&call), "true", "`{app}` has `{key}`");
    }
    // The per-key values are the controller's own `get_key/2` clauses, so the value cross-check
    // in `get_key_2_resolves_the_controller_key_set_and_its_defaults` covers this list too.
    assert_eq!(
        agrees_on_the_booted_route(
            "lists:keyfind(mod, 1, element(2, application:get_all_key(kernel)))"
        ),
        "{mod,{kernel,[]}}"
    );
    // The `env` slot is the live environment, not the resource file's declaration: it holds the
    // same nine pairs `get_all_env/1` reports, the two injected ones included.
    assert_eq!(
        agrees_on_the_booted_route(
            "length(element(2, application:get_all_key(kernel))) - 0"
        ),
        "13"
    );
    assert_eq!(
        agrees_on_the_booted_route("lists:sort(element(2, application:get_key(kernel, env)))"),
        agrees_on_the_booted_route("lists:sort(application:get_all_env(kernel))"),
        "`get_all_key/1`'s `env` slot is `get_all_env/1`'s list"
    );
}
#[test]
fn get_all_key_is_undefined_for_an_unloaded_or_non_atom_application() {
    // An application outside the loaded set — installed (`erts`, `compiler`), unknown, or not an
    // atom at all — is the oracle's atom `undefined`.
    for argument in ["erts", "compiler", "sasl", "nosuchapp_xyz", "42", "[kernel]"] {
        let call = format!("application:get_all_key({argument})");
        assert_eq!(
            agrees_on_the_booted_route(&call),
            "undefined",
            "`application:get_all_key({argument})`"
        );
    }
}
/// The `vsn` a controller reports is the version of the release the application was **loaded
/// from**: for `kernel` this checkout's and the installation's agree (`11.0.4`), for `stdlib`
/// they do not — the default route loads **this runtime's own** release's boot script, which
/// embeds this checkout's `stdlib` version where the oracle's embeds its installation's. That
/// is the same relationship `init:script_id/0` pins (each runtime reports the release it
/// loaded), so the *predicate* is compared with the oracle here, not the string.
#[test]
fn the_reported_vsn_is_the_loaded_releases() {
    assert_eq!(
        agrees_on_the_booted_route("application:get_key(kernel, vsn)"),
        format!("{{ok,\"{KERNEL_VSN}\"}}")
    );
    assert_eq!(
        renders_booted("application:get_key(stdlib, vsn)"),
        format!("{{ok,\"{STDLIB_VSN}\"}}")
    );
    assert_eq!(
        renders_booted("lists:any(fun({_,_,V}) -> V =:= \"\" end, application:loaded_applications())"),
        "false",
        "every reported version is a non-empty string"
    );
    // Self-consistency, which holds on both runtimes: the `get_key/2` answer and the triple the
    // two application lists report carry the same version.
    for list in ["loaded_applications", "which_applications"] {
        let predicate = format!(
            "element(2, application:get_key(stdlib, vsn)) =:= \
             element(3, lists:keyfind(stdlib, 1, application:{list}()))"
        );
        assert_eq!(agrees_on_the_booted_route(&predicate), "true", "`{predicate}`");
    }
}
/// The two application **lists** are the live controller's: `loaded_applications/0` reads
/// `ac_tab` directly (`application_controller.erl:300-306`) and reports `kernel` first;
/// `which_applications/0` is the controller's `#state.running` list
/// (`application_controller.erl:295-296`, `:904`), which `start_application` really populates
/// on this boot path — so it reports the two **running** applications, `stdlib` first, exactly
/// as the oracle does. Both were floor answers before; `which_applications/0` was the one key
/// that could not have been un-floored had `#state.running` stayed empty.
#[test]
fn the_application_lists_are_the_live_controllers() {
    assert_eq!(
        agrees_on_the_booted_route("[A || {A,_,_} <- application:loaded_applications()]"),
        "[kernel,stdlib]"
    );
    assert_eq!(
        agrees_on_the_booted_route("[A || {A,_,_} <- application:which_applications()]"),
        "[stdlib,kernel]"
    );
    // The description is the resource file's own string in both, compared outright — a
    // fabricated triple cannot pass this.
    assert_eq!(
        agrees_on_the_booted_route("[D || {_,D,_} <- application:loaded_applications()]"),
        "[\"ERTS  CXC 138 10\",\"ERTS  CXC 138 10\"]"
    );
    assert_eq!(
        agrees_on_the_booted_route("[D || {_,D,_} <- application:which_applications()]"),
        "[\"ERTS  CXC 138 10\",\"ERTS  CXC 138 10\"]"
    );
    // Both lists are two `{atom, string, string}` triples: shape-checked so the test still
    // asserts something without an oracle.
    for list in ["loaded_applications", "which_applications"] {
        let call = format!(
            "lists:all(fun({{A,D,V}}) -> is_atom(A) andalso is_list(D) andalso is_list(V) end, \
             application:{list}()) andalso length(application:{list}()) =:= 2"
        );
        assert_eq!(renders_booted(&call), "true", "`{list}`'s triple shape");
    }
}
// ---------------------------------------------------------------------------------------
// 2. application:get_key/1 and get_key/2
// ---------------------------------------------------------------------------------------
#[test]
fn get_key_1_is_undefined_in_both_runtimes() {
    // `get_key(Key)` resolves the *calling process's* application through its group leader
    // (`application.erl:986-987`); neither runtime gives a `-noshell` process one.
    for argument in ["kernel", "stdlib", "nosuchapp_xyz", "42"] {
        let call = format!("application:get_key({argument})");
        assert_eq!(agrees_on_the_booted_route(&call), "undefined", "`{call}`");
    }
}
#[test]
fn get_key_2_resolves_the_controller_key_set_and_its_defaults() {
    // Every key of the controller's set (`application_controller.erl:380-404`): the loaded
    // app's own value. `agrees_on_the_booted_route` asserts the two runtimes' renderings are
    // byte-identical, so the controller's defaults (`{ok,infinity}` for `maxP`, `{ok,[]}` for a
    // `mod` the file does not declare) are compared, not assumed.
    for key in [
        "description",
        "id",
        "modules",
        "maxP",
        "maxT",
        "registered",
        "included_applications",
        "optional_applications",
        "applications",
        "mod",
        "start_phases",
    ] {
        let _ = agrees_on_the_booted_route(&format!("application:get_key(kernel, {key})"));
    }
    for key in ["mod", "applications"] {
        let _ = agrees_on_the_booted_route(&format!("application:get_key(stdlib, {key})"));
    }
    // Two shapes pinned literally, so the test still asserts without an oracle.
    assert_eq!(renders_booted("application:get_key(stdlib, mod)"), "{ok,[]}");
    assert_eq!(renders_booted("application:get_key(kernel, maxP)"), "{ok,infinity}");
    // A key outside the controller's set — a file-only property such as `runtime_dependencies` —
    // and any unknown key is `undefined`, as is a non-atom application, an unknown one, or a
    // non-atom key.
    for call in [
        "application:get_key(kernel, runtime_dependencies)",
        "application:get_key(kernel, nope_key)",
        "application:get_key(bogusapp_xyz, vsn)",
        "application:get_key(erts, vsn)",
        "application:get_key(42, vsn)",
        "application:get_key(kernel, 42)",
    ] {
        assert_eq!(agrees_on_the_booted_route(call), "undefined", "`{call}`");
    }
}
// ---------------------------------------------------------------------------------------
// 3. application:get_application/0,1
// ---------------------------------------------------------------------------------------
#[test]
fn get_application_1_maps_a_module_to_its_loaded_application() {
    assert_eq!(agrees_on_the_booted_route("application:get_application(kernel)"), "{ok,kernel}");
    assert_eq!(agrees_on_the_booted_route("application:get_application(lists)"), "{ok,stdlib}");
    assert_eq!(agrees_on_the_booted_route("application:get_application(digraph)"), "{ok,stdlib}");
    assert_eq!(
        agrees_on_the_booted_route("application:get_application(nosuchmodule_xyz)"),
        "undefined"
    );
    // `/0` is the calling process's own application: `undefined` in both runtimes.
    assert_eq!(agrees_on_the_booted_route("application:get_application()"), "undefined");
    // A **pid** takes the oracle's `is_pid/1` clause (`application.erl:1047-1050`): the pid's
    // group leader names no application here (a `-noshell` process belongs to none), so the
    // answer is the oracle's `undefined` rather than the non-atom guard's `function_clause` the
    // floor reported.
    assert_eq!(
        agrees_on_the_booted_route("application:get_application(self())"),
        "undefined"
    );
    assert_eq!(
        agrees_on_the_booted_route("application:get_application(whereis(init))"),
        "undefined"
    );
}
#[test]
fn get_application_1_reports_the_non_atom_guard_as_function_clause() {
    // The oracle's guards are `is_pid(Pid); is_atom(Module)` (`application.erl:1047,1051`); any
    // other argument leaves the function with no clause at all.
    for call in ["application:get_application([lists])", "application:get_application(42)"] {
        let cli = beam_rt_eval(&format!("{call}."));
        assert_eq!(cli.code, 1, "`{call}` raises (stderr {:?})", cli.err);
        assert_eq!(
            cli.err.trim_end(),
            "beam_rt: error function_clause",
            "`{call}` is the bare `function_clause` raise"
        );
    }
    // The oracle answers the same class for the same argument.
    if let Some(shape) = oracle_program(
        "try application:get_application([lists]) \
         catch Class:Reason -> io:format(\"~p:~p\",[Class,Reason]) end",
    ) {
        assert_eq!(shape, "error:function_clause", "the oracle's own raise shape");
    }
}
// ---------------------------------------------------------------------------------------
// 4. init:get_argument/1
// ---------------------------------------------------------------------------------------

#[test]
fn init_get_argument_matches_the_oracle_for_the_automatic_flags() {
    // `progname` follows the C rule (`erlexec.c:1702-1705`): `$PROGNAME` when set, else the
    // compile-time `"erl"`. `root` is the resolved installation root, `home` the `$HOME`
    // environment variable. All three are `{ok, [[Value]]}` in both runtimes.
    if std::env::var_os("PROGNAME").is_none() {
        assert_eq!(
            agrees_with_the_oracle("init:get_argument(progname)"),
            "{ok,[[\"erl\"]]}",
            "an unset PROGNAME is the C's compile-time default"
        );
    } else {
        // An ambient `PROGNAME` is exactly what the oracle reports too, so comparing the two
        // renderings still pins the C rule without hardcoding a value.
        let _ = agrees_with_the_oracle("init:get_argument(progname)");
    }
    let _ = agrees_with_the_oracle("init:get_argument(root)");
    let _ = agrees_with_the_oracle("init:get_argument(home)");

    // A flag the runtime has no value for is the atom `error`, exactly like an absent flag.
    for flag in ["nosuchflag_xyz", "bogus", "bindr"] {
        let call = format!("init:get_argument({flag})");
        assert_eq!(agrees_with_the_oracle(&call), "error", "`{call}`");
    }
}

#[test]
fn init_get_argument_bindir_is_the_installations_emulator_directory() {
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    let Some(bindir) = single_argument(&mut runtime, "bindir") else {
        eprintln!("SKIP: no `<root>/erts-*/bin` emulator directory under this installation");
        return;
    };
    // The shape is the C's (`erlexec.c:1541-1543`): an absolute `<root>/erts-<vsn>/bin` directory
    // that really holds the emulator executables.
    let path = Path::new(&bindir);
    assert!(path.is_absolute(), "bindir is absolute: {bindir}");
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("bin"),
        "bindir's last component is `bin`: {bindir}"
    );
    assert!(
        path.join("beam.smp").is_file() || path.join("erlexec").is_file(),
        "bindir holds the emulator executables: {bindir}"
    );
    let Some(erts) = path.parent().and_then(|dir| dir.file_name()).and_then(|n| n.to_str()) else {
        panic!("bindir has an `<erts-*>/bin` shape: {bindir}");
    };
    assert!(erts.starts_with("erts-"), "bindir's parent is an `erts-*` directory: {bindir}");

    // Compare the SHAPE/containment against the oracle, never the machine-dependent path: the
    // oracle reports the emulator directory of *its* installation, which is the same one this
    // runtime resolves (the `erl` on `$PATH`).
    let Some(oracle_bindir) = oracle_renders("init:get_argument(bindir)") else {
        return;
    };
    assert!(oracle_bindir.starts_with("{ok,[["), "the oracle's bindir shape: {oracle_bindir}");
    if let Some(oracle_erts) = Path::new(&oracle_bindir)
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .filter(|name| name.starts_with("erts-"))
        .next_back()
    {
        assert_eq!(
            erts, oracle_erts,
            "this runtime resolves the installation the oracle runs from"
        );
    }
}

// ---------------------------------------------------------------------------------------
// 5. init:script_id/0
// ---------------------------------------------------------------------------------------

/// The exact typed gap `init:script_id/0` reports when **no boot script has been loaded** — the
/// plain in-process [`Runtime::with_defaults`] route, which boots the preloaded set only. Every CLI
/// action but `-version` drives a script, so the gap is not reachable through the CLI any more.
const SCRIPT_ID_GAP: &str = "init:script_id/0 (no boot script has been loaded)";

/// The real header of the committed `bootstrap/bin/start.boot` (this checkout is OTP 30), as the
/// `~p` rendering of the `{"Erlang/OTP", Vsn}` 2-tuple.
const START_BOOT_HEADER: &str = "{\"Erlang/OTP\",\"30\"}";

/// The default boot path — every CLI action but `-version` — drives **this runtime's own**
/// release's boot script: the committed `bootstrap/bin/start.boot` of the source tree the binary
/// runs from (`cli.rs::boot_script_to_drive` via `CodeServer::own_bootstrap_boot_script`), so
/// `init:script_id/0` answers the **loaded** script's real `{Name, Vsn}` header — the id of the
/// very script the runtime read and executed, never a fabricated `{"Erlang/OTP", …}` and no
/// longer the pre-boot capability gap. An explicit `-boot FILE` still wins (see
/// [`script_id_is_the_loaded_boot_scripts_real_header`]).
#[test]
fn script_id_is_the_loaded_boot_scripts_header_on_the_default_boot_path() {
    // The action's own expression evaluates for its effect only (`cli.rs::drive`), so a silent
    // `-eval` exits 0 with empty stdout and stderr. A capability gap would instead be exit 3 with
    // the one-line `beam_rt: unsupported: …` diagnostic this test used to pin — the answer of a
    // runtime that drove no script.
    let cli = beam_rt_eval("init:script_id().");
    assert_eq!(
        cli.code, 0,
        "the default boot path loads a script, so `init:script_id/0` answers (stderr {:?})",
        cli.err
    );
    assert!(cli.out.is_empty(), "stdout {:?}", cli.out);
    assert!(cli.err.is_empty(), "stderr {:?}", cli.err);

    // The id itself, printed by the action's own expression: **this checkout's** committed
    // `bootstrap/bin/start.boot` header — the script of the release this runtime *is*. (Before
    // the default route preferred the own bootstrap it resolved `init`'s literal `start` against
    // the *consumed* installation, so it answered that installation's header instead.)
    let cli = beam_rt_eval("io:format(\"~p\", [init:script_id()]).");
    assert_eq!(cli.code, 0, "the observable probe exits 0 (stderr {:?})", cli.err);
    let ours = cli.out.trim_end();
    assert_eq!(
        ours, START_BOOT_HEADER,
        "the default route reports this runtime's own release's boot-script header"
    );

    // The self-consistency invariant the differential case pins: with the loaded script being
    // this runtime's **own** release's, its header agrees with the compile-time
    // `erlang:system_info(otp_release)` — so
    // `init:script_id() =:= {"Erlang/OTP", erlang:system_info(otp_release)}` is `true`, exactly
    // as on the oracle (whose own loaded script and release agree). The oracle's *id* is its
    // installation's, never ours, so this compares the **predicate**, not the two ids.
    let consistent = "init:script_id() =:= {\"Erlang/OTP\", erlang:system_info(otp_release)}";
    let cli = beam_rt_eval(&format!("io:format(\"~p\", [{consistent}])."));
    assert_eq!(cli.code, 0, "the consistency probe exits 0 (stderr {:?})", cli.err);
    assert_eq!(cli.out.trim_end(), "true", "`{consistent}` is true on this runtime");
    if let Some(expected) = oracle_renders(consistent) {
        assert_eq!(expected, "true", "the oracle's own id and release agree too");
    }

    // A runtime that drives **no** script has no id to report: the typed capability gap, never a
    // fabricated `{"Erlang/OTP", …}`. This is the plain in-process route ([`Runtime::with_defaults`]
    // boots the preloaded set only) — the gap the pre-boot runtime reported on *every* route.
    assert_eq!(
        beam_rt::error::unsupported_reason(&raise_of("init:script_id()")).as_deref(),
        Some(SCRIPT_ID_GAP)
    );
}

#[test]
fn script_id_is_the_loaded_boot_scripts_real_header() {
    let Some(probe) = ScriptIdProbe::build() else {
        return;
    };

    // A script-running invocation (`-boot FILE`, `../cli.rs::run_boot_script`) really loads the
    // script before running its directives, so `init:script_id/0` answers its real header. The
    // expected value is **derived**, not hardcoded: the oracle read the `{Name, Vsn}` header out
    // of the very script this runtime loads, so a release bump cannot make this test lie.
    let expected = format!("{}{{\"{}\",\"{}\"}}", PROBE_PREFIX, probe.header.0, probe.header.1);
    let boot = probe.dir.join("script_id");
    let cli = beam_rt(&["-noshell", "-boot", &boot.to_string_lossy()]);
    assert_eq!(cli.code, 0, "the boot probe exits 0 (stderr {:?})", cli.err);
    assert!(
        cli.out.lines().any(|line| line == expected),
        "the boot script's apply must report `{expected}`, got {:?}",
        cli.out
    );
    // `// documented divergence:` the committed script of this checkout is OTP 30's, so its header
    // is `{"Erlang/OTP","30"}` where the OTP-29 oracle's own `start.boot` carries `"29"`. The
    // *release* differs, not the mechanism — both runtimes report the real header of the script
    // they loaded.
    assert_eq!(probe.header.0, "Erlang/OTP");
    assert_eq!(probe.header.1, "30");
    assert_eq!(expected, format!("{PROBE_PREFIX}{START_BOOT_HEADER}"));

    // In-process, through the runtime's own API: the same header, and the proof that the script's
    // `primLoad` batch really loaded a module (`erlang:module_loaded/1` — the boot-set divergence
    // itself is pinned by the sibling `module_loaded.rs` suite).
    let Some(mut runtime) = boot_script_runtime(&probe.script) else {
        return;
    };
    runtime
        .boot_from_script_reporting(&probe.script)
        .expect("the probe script runs to completion");
    let value = value_of_in(&mut runtime, "init:script_id()");
    assert_eq!(rendered(&mut runtime, value), START_BOOT_HEADER);
    let loaded = value_of_in(&mut runtime, "erlang:module_loaded(lists)");
    assert_eq!(
        runtime.atom_name_of(loaded),
        Some("true"),
        "the boot script's primLoad batch really loaded `lists`"
    );
}

/// A runtime whose boot sequence really ran — `None` (loudly) without a system installation.
fn booted_runtime() -> Option<Runtime> {
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    match runtime.boot() {
        Ok(()) => Some(runtime),
        Err(error) => {
            eprintln!("SKIP a9_native_floor: boot needs the system Erlang install ({error})");
            None
        }
    }
}

/// A runtime configured **exactly as the CLI configures a `-boot FILE` invocation** for `script`,
/// or `None` — loudly — when the system Erlang install is unavailable or the invocation does not
/// parse.
///
/// The **route** is part of the measurement, not an implementation detail: on the bare
/// [`Runtime::with_defaults`] path (an empty command line, hence no `boot` init argument)
/// [`Runtime::new`](beam_rt::Runtime::new) **registers** the native `file_server_2` service under
/// that name, so a script that really starts `kernel` aborts its `kernel_sup` with
/// `{shutdown,{failed_to_start_child,file_server_2,{already_started,_}}}` — and `kernel` is
/// `permanent`, so the node tears down and the drive stops short of the script's end. A runtime
/// built through the CLI's **own parser** carries the `-boot` argument
/// ([`RuntimeConfig::boot_script`](beam_rt::RuntimeConfig::boot_script)), on which
/// [`Runtime::new`](beam_rt::Runtime::new) spawns that service **unclaimed** so the booted
/// `kernel_sup`'s own `file_server_2` child claims the name. This mirrors
/// `tests/boot.rs::boot_cli_runtime` (the same construction, the same reason), so both suites
/// measure the invocation the CLI really runs.
fn boot_script_runtime(script: &Path) -> Option<Runtime> {
    if booted_runtime().is_none() {
        return None;
    }
    let argv = vec![
        "-noshell".to_string(),
        "-boot".to_string(),
        script.to_string_lossy().into_owned(),
    ];
    let config = match beam_rt::parse(&argv) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("SKIP a9_native_floor: cannot build the `-boot` invocation's configuration ({error})");
            return None;
        }
    };
    match Runtime::new(config) {
        Ok(runtime) => Some(runtime),
        Err(error) => {
            eprintln!("SKIP a9_native_floor: no system Erlang install ({error})");
            None
        }
    }
}

/// The line [`PROBE_ERL`] prints before `init:script_id/0`'s rendering.
const PROBE_PREFIX: &str = "script_id=";

/// The fixture module a script-running invocation applies: it only *reads* the id.
const PROBE_ERL: &str = r#"%% Generated by `beam_rt/tests/a9_native_floor.rs` (never committed).
-module(a9_script_id).
-export([run/0]).
%% The observable value `init:script_id/0` reports inside a real boot-script `apply` directive.
run() -> io:format("script_id=~p~n", [init:script_id()]).
"#;

/// The committed `bootstrap/bin/start.boot` of this checkout, three ancestors above the crate.
fn start_boot() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("the repository root is three levels above the crate")
        .join("bootstrap/bin/start.boot")
}

/// A scratch directory holding the compiled probe module and its two probe `.boot` images,
/// removed when the test ends.
///
/// Each `.boot` is the **real** `bootstrap/bin/start.boot` — its directives verbatim, so the
/// script the runtime loads carries the real header — plus a code-path entry for the probe; the
/// CLI copy additionally carries the probe's `apply`.
struct ScriptIdProbe {
    dir: PathBuf,
    /// The copy the in-process drive reads (`boot_from_script_reporting`).
    script: PathBuf,
    /// The `{Name, Vsn}` header the oracle read out of the script.
    header: (String, String),
}

impl ScriptIdProbe {
    /// Compiles the probe module and writes the probe scripts, or `None` — **loudly** — when the
    /// toolchain or the committed script is unavailable.
    fn build() -> Option<ScriptIdProbe> {
        let real = start_boot();
        if !real.is_file() {
            eprintln!("SKIP a9_native_floor: {} is absent", real.display());
            return None;
        }
        oracle()?; // a missing oracle already printed the loud skip
        let dir = std::env::temp_dir().join(format!("beam_rt_a9_script_id_{}", std::process::id()));
        if let Err(error) = std::fs::create_dir_all(&dir) {
            eprintln!("SKIP a9_native_floor: cannot create {}: {error}", dir.display());
            return None;
        }
        let source = dir.join("a9_script_id.erl");
        if let Err(error) = std::fs::write(&source, PROBE_ERL) {
            eprintln!("SKIP a9_native_floor: cannot write {}: {error}", source.display());
            return None;
        }
        match Command::new("erlc")
            .arg("+deterministic")
            .arg("-o")
            .arg(&dir)
            .arg(&source)
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                eprintln!(
                    "SKIP a9_native_floor: `erlc` exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
                return None;
            }
            Err(error) => {
                eprintln!("SKIP a9_native_floor: cannot run `erlc`: {error}");
                return None;
            }
        }

        // The oracle writes both probe scripts and prints the header it read, so the expected
        // value is derived from the script rather than hardcoded. The CLI copy carries the probe's
        // `apply` (its stdout is the observable), while the in-process copy only adds the code
        // path — the in-process drive reads `init:script_id/0` through the runtime API instead, so
        // it must not print.
        let stem = dir.join("script_id");
        let second = dir.join("script_id_in_process.boot");
        let program = format!(
            "{{ok,B}} = file:read_file(\"{real}\"),\
             {{script,{{N,V}},Is}} = binary_to_term(B),\
             Cli = {{script,{{N,V}}, Is ++ [{{path,[\"{dir}\"]}}, {{apply,{{a9_script_id,run,[]}}}}]}},\
             ok = file:write_file(\"{stem}.boot\", term_to_binary(Cli)),\
             Inner = {{script,{{N,V}}, Is ++ [{{path,[\"{dir}\"]}}]}},\
             ok = file:write_file(\"{second}\", term_to_binary(Inner)),\
             io:format(\"{prefix}~s|~s\", [N,V])",
            real = real.display(),
            dir = dir.display(),
            stem = stem.display(),
            second = second.display(),
            prefix = PROBE_PREFIX,
        );
        let Some(answer) = oracle_program(&program) else {
            eprintln!("SKIP a9_native_floor: the oracle could not write the probe scripts");
            return None;
        };
        let Some(header) = answer.strip_prefix(PROBE_PREFIX) else {
            eprintln!("SKIP a9_native_floor: unexpected probe output {answer:?}");
            return None;
        };
        let Some((name, vsn)) = header.split_once('|') else {
            eprintln!("SKIP a9_native_floor: unexpected probe header {header:?}");
            return None;
        };
        Some(ScriptIdProbe {
            dir,
            script: second,
            header: (name.to_string(), vsn.to_string()),
        })
    }
}

impl Drop for ScriptIdProbe {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---------------------------------------------------------------------------------------
// 6. code:lib_dir/2
// ---------------------------------------------------------------------------------------

#[test]
fn lib_dir_2_joins_the_application_directory_without_validating_the_subdirectory() {
    // The oracle's guard is `is_atom(App), is_atom(SubDir)`; the code server then joins the two
    // unconditionally, so an unknown subdirectory is still `<appdir>/<SubDir>` — never stat'ed.
    for sub in ["ebin", "src", "bogus", "include"] {
        let _ = agrees_with_the_oracle(&format!("code:lib_dir(kernel, {sub})"));
    }
    // The join really is a join: whatever the app directory is, the answer ends with `/ebin`.
    let dir = renders("code:lib_dir(kernel, ebin)");
    assert!(dir.ends_with("/ebin\""), "`code:lib_dir(kernel, ebin)` ends in `ebin`: {dir}");
    assert!(
        renders("code:lib_dir(kernel, bogus)").contains("/bogus\""),
        "an unknown subdirectory is joined verbatim"
    );

    // An unknown application is the code server's own `{error, bad_name}`.
    assert_eq!(agrees_with_the_oracle("code:lib_dir(nosuchapp_xyz, ebin)"), "{error,bad_name}");
}

#[test]
fn lib_dir_2_reports_a_non_atom_argument_as_function_clause() {
    for call in [
        "code:lib_dir(kernel, 42)",
        "code:lib_dir(42, ebin)",
        "code:lib_dir(kernel, [x])",
    ] {
        assert_eq!(
            raise_of(call).as_atom().expect("the bare function_clause atom"),
            "function_clause",
            "`{call}`"
        );
    }
    // The oracle raises the same class for the same argument.
    if let Some(shape) = oracle_program(
        "try code:lib_dir(kernel, 42) \
         catch Class:Reason -> io:format(\"~p:~p\",[Class,Reason]) end",
    ) {
        assert_eq!(shape, "error:function_clause", "the oracle's own raise shape");
    }
}

// ---------------------------------------------------------------------------------------
// 7. filelib existence probes (the file_server_2 read_file_info seam)
// ---------------------------------------------------------------------------------------

/// A private temporary directory, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> TempDir {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "beam_rt_a9_native_floor_{}_{unique}_{label}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("a private temporary directory");
        TempDir(path)
    }

    /// The Erlang **string literal** for a name inside the directory.
    fn literal(&self, name: &str) -> String {
        format!("{:?}", self.0.join(name).to_str().expect("an ASCII temporary path"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `filelib` existence probes reach `file_server_2` through `file:read_file_info/1` /
/// `read_link_info/1` (`filelib.erl:273-297` / `:742-753`), whose `stat(2)`/`lstat(2)` **failure**
/// path the native service serves — so an absent path is decided entirely by the failed stat,
/// which is the oracle's own `false`/`[]`.
#[test]
fn filelib_existence_probes_agree_that_an_absent_path_does_not_exist() {
    let dir = TempDir::new("absent");
    let missing = dir.literal("absent");
    for (call, expected) in [
        (format!("filelib:is_dir({missing})"), "false"),
        (format!("filelib:is_file({missing})"), "false"),
        (format!("filelib:is_regular({missing})"), "false"),
        (format!("filelib:wildcard({missing})"), "[]"),
    ] {
        assert_eq!(agrees_with_the_oracle(&call), expected, "`{call}`");
    }
}

/// The **success** path is served: the native `file_server_2` service builds the whole
/// `#file_info{}` record, with the `access` slot derived from a real `access(2)` probe
/// (`beam_io`'s `fs_access`), so every `filelib` existence probe on an **existing** path answers
/// the oracle's own value instead of raising a capability gap.
#[test]
fn filelib_existence_probes_on_an_existing_path_agree_with_the_oracle() {
    let dir = TempDir::new("existing");
    std::fs::write(dir.0.join("sample.txt"), b"x").expect("a temporary file");
    let sample = dir.literal("sample.txt");
    for (call, expected) in [
        (format!("filelib:is_dir({})", dir.literal(".")), "true".to_string()),
        (format!("filelib:is_file({sample})"), "true".to_string()),
        (format!("filelib:is_regular({sample})"), "true".to_string()),
        (
            format!("filelib:wildcard({sample})"),
            format!("[{sample}]"),
        ),
    ] {
        assert_eq!(agrees_with_the_oracle(&call), expected, "`{call}`");
    }
}
/// The **audit** of the twelve remaining floor namespaces: `src/native/CONTEXT.md`'s
/// *Namespace audit* section says every one of them stays floored, and the two reasons a key can
/// stay are (a) its OTP body needs a facility this runtime does not run, or (b) it answers a
/// question whose true source is a live server this runtime keeps its own copy of. This pins the
/// part that *is* falsifiable without withdrawing anything: for the keys a body **could** plausibly
/// serve, the floor's answer is the oracle's answer, argument by argument — so "kept because it
/// agrees" is measured rather than asserted.
///
/// The withdrawn `application` namespace is the counter-example this pins against: it agreed on
/// the plain reads too and still had to go, because it disagreed on the *state* (`set_env/4`).
/// A key that merely agrees on a read is therefore kept here only where no body can disagree
/// later — every probe below is a pure query.
#[test]
fn the_kept_floor_namespaces_agree_with_the_oracle() {
    // Environment-independent answers: pinned outright (so the probe is meaningful without an
    // oracle), then required to be the oracle's rendering too.
    for (call, expected) in [
        // `code:is_loaded/1` — the `CodeServer`'s own loaded set.
        // `is_loaded/1` answers `{file, Path}` for a loaded module, `false` for one it never saw.
        ("is_tuple(code:is_loaded(lists))", "true"),
        ("code:is_loaded(no_such_xyz)", "false"),
        // `code:is_sticky/1` — the release's sticky directory rule.
        ("code:is_sticky(lists)", "true"),
        ("code:is_sticky(mnesia)", "false"),
        // `code:get_mode/0` — the floor's `-mode` init argument.
        ("code:get_mode()", "interactive"),
        // `code:load_file/1`, `ensure_loaded/1` and `get_object_code/1` — a miss is the same
        // `{error,nofile}`/`error` the oracle's own loader answers.
        ("code:load_file(no_such_xyz)", "{error,nofile}"),
        ("code:ensure_loaded(no_such_xyz)", "{error,nofile}"),
        ("code:get_object_code(no_such_xyz)", "error"),
        // The node's own link table: `net_adm:ping/1` on a name nothing is listening for is
        // `pang` (the oracle's timeout answer) and `net_kernel:connect_node/1` is `ignored`.
        ("net_adm:ping(nonode@nohost)", "pang"),
        ("net_kernel:connect_node(nonode@nohost)", "ignored"),
        // `code:lib_dir/1` and `code:priv_dir/1` — the release layout, whose version suffix the
        // installation owns, so the two are pinned by their relationship and by `is_list/1`.
        ("is_list(code:lib_dir(stdlib))", "true"),
        ("is_list(code:priv_dir(stdlib))", "true"),
        ("code:priv_dir(stdlib) =:= code:lib_dir(stdlib) ++ \"/priv\"", "true"),
        // `os:cmd/1` runs a real shell; the comparison form keeps the rendering independent of
        // the shell's own output buffering.
        ("os:cmd(\"echo hi\") =:= \"hi\\n\"", "true"),
        ("os:cmd(\"printf hi\") =:= \"hi\"", "true"),
    ] {
        assert_eq!(agrees_on_the_booted_route(call), expected, "`{call}`");
    }
    // Path-valued answers name the installation, so only the oracle can pin them — each is
    // compared outright, and independently required to be a non-empty quoted path naming what
    // was asked for, so a probe that stops answering cannot pass vacuously.
    for (call, suffix) in [
        ("code:which(lists)", "lists.beam\""),
        ("code:which(erlang)", "preloaded"),
        ("code:where_is_file(\"lists.beam\")", "lists.beam\""),
        ("code:root_dir()", "erlang\""),
        ("code:priv_dir(stdlib)", "priv\""),
        ("filename:basename(code:which(lists))", "\"lists.beam\""),
    ] {
        let rendered = agrees_on_the_booted_route(call);
        assert!(
            rendered.ends_with(suffix),
            "`{call}` renders {rendered:?}, not an answer ending {suffix:?}"
        );
    }
}
