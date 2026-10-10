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
//! | `application:get_all_key/1` | the loaded-application gate, the controller's 13-key order, and the `env` divergence |
//! | `application:get_key/1,2` | `undefined` for `/1`; the controller key set + documented defaults for `/2` |
//! | `application:get_application/0,1` | the module → loaded-application map, the `/0` `undefined`, and the non-atom guard |
//! | `init:get_argument/1` | the four automatic flags (`root`, `home`, `progname`, `bindir`) and an absent flag |
//! | `init:script_id/0` | the **loaded** boot script's real header, and the typed gap on the default boot path |
//! | `code:lib_dir/2` | the app dir joined with any subdir, `{error,bad_name}`, `function_clause` |
//! | `filelib:is_dir/1, is_file/1, is_regular/1, wildcard/1` | the absent-path answers, and the precise typed gap on success |
//!
//! `erlang:module_loaded/1`'s boot-set divergence is **not** re-tested here — it is pinned by the
//! sibling [`module_loaded.rs`](module_loaded) suite; this suite only adds the complementary
//! observation that a *boot-script* invocation really loads the `primLoad` batch
//! ([`script_id_is_the_loaded_boot_scripts_real_header`]).
//!
//! Every rendering compared here comes from the **same printer** on both sides: this runtime's
//! `~p` drives the registered `io:format/2`, so [`agrees_with_the_oracle`] is a byte comparison of
//! the two answers, never a hardcoded expectation.
//!
//! # Where the values diverge **by construction** (never a bug)
//!
//! Each is marked `// documented divergence:` where it is asserted, and every one is an answer
//! that reads *this* runtime's own state truthfully:
//!
//! * **`application:get_all_key/1` / `get_key/2`'s `env`.** The oracle's `kernel` environment
//!   carries two **runtime-injected** entries (`connect_all`, `logger`) that the resource file
//!   never declares, in the application controller's own storage order. This runtime has no live
//!   `application_controller`, so it reports the `.app` resource's `env` **verbatim** — which the
//!   test proves against the oracle's own `file:consult` of that resource (`stdlib`'s empty
//!   environment agrees outright, and the resource file's list is *not* what the oracle's loaded
//!   controller reports).
//! * **`application:get_application/1` on a pid.** The oracle has an `is_pid/1` clause answering
//!   `undefined`; this runtime runs no application-owned process, so a pid names no application
//!   here either, but it reports the non-atom guard's `function_clause` shape.
//! * **`init:script_id/0` on the default boot path.** No script is loaded, so there is no id to
//!   report — the typed capability gap, never a fabricated `{"Erlang/OTP", …}`.
//! * **`init:script_id/0`'s value under a script.** The committed `bootstrap/bin/start.boot` is
//!   OTP 30's, so its real header is `{"Erlang/OTP","30"}`; the OTP-29 oracle's own `start.boot`
//!   carries `"29"`. The release differs, not the mechanism — and the test **derives** the
//!   expected header from the script through the oracle, so a release bump cannot make it lie.
//! * **`filelib` on an existing path.** The `#file_info{}` `access` slot is `access(2)`-derived in
//!   the oracle and unreachable from safe Rust, so the success path is a precise typed gap.
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
// 1. application:get_all_key/1
// ---------------------------------------------------------------------------------------

/// `application:get_all_key/1`'s key order is the application controller's own
/// (`application_controller:get_pid_all_key/1`), not the resource file's declaration order.
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
            unwrapped(&agrees_with_the_oracle(&call)),
            unwrapped(APPLICATIONS_CONTROLLER_KEYS),
            "`application:get_all_key({app})`'s key order"
        );
        let call = format!("length(element(2, application:get_all_key({app})))");
        assert_eq!(agrees_with_the_oracle(&call), "13", "`{app}` reports 13 keys");
    }

    // The three keys the A.9 surface names, on the loaded pair.
    for (app, key) in [("kernel", "modules"), ("kernel", "env"), ("kernel", "mod")] {
        let call = format!("lists:keymember({key}, 1, element(2, application:get_all_key({app})))");
        assert_eq!(agrees_with_the_oracle(&call), "true", "`{app}` has `{key}`");
    }

    // The per-key values are assembled by the very helper `application:get_key/2` uses
    // (`loaded_app_value`), so the value cross-check in
    // `get_key_2_resolves_the_controller_key_set_and_its_defaults` covers this list too.
    assert_eq!(
        agrees_with_the_oracle("lists:keyfind(mod, 1, element(2, application:get_all_key(kernel)))"),
        "{mod,{kernel,[]}}"
    );
}

#[test]
fn get_all_key_is_undefined_for_an_unloaded_or_non_atom_application() {
    // An application outside the loaded set — installed (`erts`, `compiler`), unknown, or not an
    // atom at all — is the oracle's atom `undefined`.
    for argument in ["erts", "compiler", "sasl", "nosuchapp_xyz", "42", "[kernel]"] {
        let call = format!("application:get_all_key({argument})");
        assert_eq!(
            agrees_with_the_oracle(&call),
            "undefined",
            "`application:get_all_key({argument})`"
        );
    }
}

/// The `env` key is the one **documented divergence** in the all-key list, and this test proves it
/// is *not* a parse or ordering bug: the runtime's list is byte-identical to the resource file's
/// own `env` declaration (as read by the oracle's `file:consult/1`), while the oracle's *loaded*
/// list additionally carries the `application_controller`'s runtime injections.
#[test]
fn the_env_key_is_the_resource_declaration_while_the_oracle_injects_two_entries() {
    // `stdlib` declares no environment at all, so both runtimes agree outright.
    assert_eq!(agrees_with_the_oracle("application:get_key(stdlib, env)"), "{ok,[]}");

    // `kernel`'s resource declares seven entries; the oracle's loaded environment carries nine.
    let declared = oracle_program(
        "{ok,[{application,kernel,Props}]} = \
           file:consult(filename:join([code:lib_dir(kernel),\"ebin\",\"kernel.app\"])),\
         {env,E} = lists:keyfind(env,1,Props),\
         io:format(\"~p\",[{ok,E}])",
    );
    let Some(declared) = declared else {
        eprintln!("warning: the oracle could not read kernel.app; the env comparison is skipped");
        return;
    };
    // `// documented divergence:` the runtime answers the resource file's `env` verbatim — which
    // is exactly what the oracle's own `file:consult` of that file holds. The oracle's *loaded*
    // environment differs because its `application_controller` stores two injected entries (and
    // keeps its own order); reproducing those needs a live `application_controller`, which this
    // runtime does not run.
    assert_eq!(
        renders("application:get_key(kernel, env)"),
        declared,
        "the runtime's env must be the resource file's declaration"
    );

    // The two injected entries are what the oracle's controller adds: the resource file never
    // declares them, and the oracle serves them from its live environment.
    assert_eq!(
        renders("application:get_env(kernel, connect_all)"),
        "undefined",
        "kernel.app declares no `connect_all`"
    );
    assert_eq!(
        oracle_renders("application:get_env(kernel, connect_all)").as_deref(),
        Some("{ok,true}"),
        "the oracle's application_controller injects `connect_all` at kernel start"
    );
    assert_eq!(renders("application:get_env(kernel, logger)"), "undefined");
    assert_ne!(
        oracle_renders("application:get_env(kernel, logger)").as_deref(),
        Some("undefined"),
        "the oracle's application_controller injects `logger` at kernel start"
    );
    // …and the same injection is why the oracle's *loaded* env list is longer than the file's 7.
    if let Some(len) = oracle_renders("length(element(2, application:get_key(kernel, env)))") {
        assert!(
            len.parse::<usize>().expect("a length") > 7,
            "the oracle's loaded kernel env ({len}) is longer than the file's 7 entries"
        );
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
        assert_eq!(agrees_with_the_oracle(&call), "undefined", "`{call}`");
    }
}

#[test]
fn get_key_2_resolves_the_controller_key_set_and_its_defaults() {
    // Every key of the controller's set: the resource file's declaration, or the controller's own
    // default (`make_appl_i`, `application_controller.erl:1572-1598`). `agrees_with_the_oracle`
    // asserts the two runtimes' renderings are byte-identical.
    for key in [
        "description",
        "id",
        "vsn",
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
        let _ = agrees_with_the_oracle(&format!("application:get_key(kernel, {key})"));
    }
    for key in ["vsn", "mod", "applications"] {
        let _ = agrees_with_the_oracle(&format!("application:get_key(stdlib, {key})"));
    }

    // The three documented shapes, pinned literally so the test still asserts without an oracle.
    assert_eq!(renders("application:get_key(kernel, vsn)"), "{ok,\"11.0.4\"}");
    assert_eq!(renders("application:get_key(stdlib, mod)"), "{ok,[]}");

    // A key outside the controller's set — a file-only property such as `runtime_dependencies` —
    // and any unknown key is `undefined`, as is a non-atom application, an unknown one, or a
    // non-atom key.
    for call in [
        "application:get_key(kernel, runtime_dependencies)",
        "application:get_key(kernel, nope_key)",
        "application:get_key(bogusapp_xyz, vsn)",
        "application:get_key(42, vsn)",
        "application:get_key(kernel, 42)",
    ] {
        assert_eq!(agrees_with_the_oracle(call), "undefined", "`{call}`");
    }
}

// ---------------------------------------------------------------------------------------
// 3. application:get_application/0,1
// ---------------------------------------------------------------------------------------

#[test]
fn get_application_1_maps_a_module_to_its_loaded_application() {
    assert_eq!(agrees_with_the_oracle("application:get_application(kernel)"), "{ok,kernel}");
    assert_eq!(agrees_with_the_oracle("application:get_application(lists)"), "{ok,stdlib}");
    assert_eq!(agrees_with_the_oracle("application:get_application(digraph)"), "{ok,stdlib}");
    assert_eq!(
        agrees_with_the_oracle("application:get_application(nosuchmodule_xyz)"),
        "undefined"
    );
    // `/0` is the calling process's own application: `undefined` in both runtimes.
    assert_eq!(agrees_with_the_oracle("application:get_application()"), "undefined");
}

#[test]
fn get_application_1_reports_the_non_atom_guard_as_function_clause() {
    // The oracle's guard is `is_atom(Module); is_pid(Module)` (`application.erl:1051`); a non-atom
    // that is not a pid leaves the function with no clause at all.
    for call in ["application:get_application([lists])", "application:get_application(42)"] {
        assert_eq!(
            raise_of(call).as_atom().expect("the bare function_clause atom"),
            "function_clause",
            "`{call}`"
        );
    }
    // The oracle answers the same class for the same argument.
    if let Some(shape) = oracle_program(
        "try application:get_application([lists]) \
         catch Class:Reason -> io:format(\"~p:~p\",[Class,Reason]) end",
    ) {
        assert_eq!(shape, "error:function_clause", "the oracle's own raise shape");
    }

    // `// documented divergence:` the oracle's extra `is_pid/1` clause answers `undefined` for a
    // pid. This runtime runs no application-owned process, so a pid names no application here
    // either — but it is reported with the non-atom guard's `function_clause` shape rather than
    // walked as a pid.
    assert_eq!(
        raise_of("application:get_application(self())")
            .as_atom()
            .expect("the bare function_clause atom"),
        "function_clause"
    );
    if let Some(answer) = oracle_renders("application:get_application(self())") {
        assert!(
            answer == "undefined" || answer.starts_with("{ok,"),
            "the oracle does not raise for a pid: {answer}"
        );
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

/// The exact typed gap `init:script_id/0` reports on the default (no-script) boot path.
const SCRIPT_ID_GAP: &str = "init:script_id/0 (no boot script has been loaded)";

/// The real header of the committed `bootstrap/bin/start.boot` (this checkout is OTP 30), as the
/// `~p` rendering of the `{"Erlang/OTP", Vsn}` 2-tuple.
const START_BOOT_HEADER: &str = "{\"Erlang/OTP\",\"30\"}";

#[test]
fn script_id_is_a_typed_gap_on_the_default_boot_path() {
    // `-eval` builds the runtime through `Runtime::boot` only: no boot script is read, so there is
    // no id to report. The gap is a capability gap — exit 3 and one stable stderr line — never a
    // fabricated `{"Erlang/OTP", …}`.
    let cli = beam_rt_eval("init:script_id().");
    assert_eq!(cli.code, 3, "a capability gap exits 3 (stderr {:?})", cli.err);
    assert_eq!(cli.err, format!("beam_rt: unsupported: {SCRIPT_ID_GAP}"));
    assert!(cli.out.is_empty(), "stdout {:?}", cli.out);

    // The same answer in-process.
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
    let Some(mut runtime) = booted_runtime() else {
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

/// The exact reason the native `file_server_2` service reports when a `stat(2)` **succeeded** and
/// the reply would need the `#file_info{}` `access` slot (`src/native/file_server/file_info.rs`).
const FILE_INFO_GAP: &str = "file_server: the #file_info{} `access` slot needs access(2), which \
                             no safe std API exposes";

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

/// The **success** path stays an honest capability gap: the `#file_info{}` `access` slot is
/// `access(2)`-derived in the oracle (`unix_prim_file.c:739-769`) and no safe `std` API exposes
/// `access(2)`/`geteuid(2)`, so the runtime must not invent the field — handing a fabricated
/// record to `filelib:is_dir/1` would turn the oracle's truthful `true` into a divergence.
#[test]
fn filelib_existence_probes_on_an_existing_path_are_the_access_capability_gap() {
    let dir = TempDir::new("existing");
    std::fs::write(dir.0.join("sample.txt"), b"x").expect("a temporary file");
    for call in [
        format!("filelib:is_dir({})", dir.literal(".")),
        format!("filelib:is_file({})", dir.literal("sample.txt")),
        format!("filelib:is_regular({})", dir.literal("sample.txt")),
        format!("filelib:wildcard({})", dir.literal("sample.txt")),
    ] {
        let error = raise_of(&call);
        assert_eq!(
            beam_rt::error::unsupported_reason(&error).as_deref(),
            Some(FILE_INFO_GAP),
            "`{call}` must stay the precise typed `access` gap, got {error:?}"
        );
    }
    // The oracle answers a real boolean here — exactly the answer a fabricated `access` would risk
    // turning into a divergence.
    if let Some(answer) = oracle_renders(&format!("filelib:is_file({})", dir.literal("sample.txt")))
    {
        assert_eq!(answer, "true", "the oracle sees the existing file");
    }
}
