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

//! The **busy-port** end-to-end test (contract A.4): `erlang:port_command/2,3` against a **real,
//! busy C driver** whose entry does (and does not) advertise `ERL_DRV_FLAG_SOFT_BUSY`.
//!
//! # What this pins, and why it is not already covered
//!
//! `RuntimeContext::port_command` reads three facts under one borrow of the port table — the port's
//! liveness, `PortRegistry::is_busy` and `PortRegistry::soft_busy` — and answers from them
//! (`src/native/port.rs`). The five colocated tests in `src/native/port.rs` pin the *decision table*
//! against a Rust recorder port; what they cannot pin is that a **real** driver's `set_busy_port`
//! and its `ErlDrvEntry::driver_flags` actually reach those two predicates across the A.4 boundary
//! (`DriverPort::set_busy_port` → `PortMeta::driver_busy`; `DriverPortAdapter::load`'s
//! `with_soft_busy(library.entry().driver_flags & ERL_DRV_FLAG_SOFT_BUSY != 0)`).
//!
//! This suite joins the two halves exactly as `tests/driver.rs` does for the rest of the A.4
//! surface: the committed C fixture `tests/fixtures/busy_driver.c` is compiled by the host `cc`
//! against the host `erl_driver.h` into **two** shared objects (one per `driver_flags` variant),
//! and both this runtime's binary (`CARGO_BIN_EXE_beam_rt`) and the **system `erl`** open a
//! `{spawn_driver, …}` port of each and run the same six-case matrix through it.
//!
//! # The behaviour table (oracle-verified, erts-17.1 / OTP 29.1.1)
//!
//! | port state | options | driver entry | `beam_rt` | oracle |
//! |---|---|---|---|---|
//! | idle | none / `[]` / `[nosuspend]` | either | `true` | `true` |
//! | idle | `[force]` | without `ERL_DRV_FLAG_SOFT_BUSY` | `error:notsup` | `notsup` |
//! | idle | `[force]` | with `ERL_DRV_FLAG_SOFT_BUSY` | `true` | `true` |
//! | busy | `[nosuspend]` | either | `false` (nothing written) | `false` |
//! | busy | `[force]` | without `ERL_DRV_FLAG_SOFT_BUSY` | `error:notsup` | `notsup` |
//! | busy | `[force]` | with `ERL_DRV_FLAG_SOFT_BUSY` | `true` (delivered) | `true` |
//! | busy | none | either | typed capability gap, exit 3 | caller suspends, then `true` |
//!
//! The first six arms are asserted **identically** on both runtimes. The last is the runtime's one
//! documented divergence: the C's `erts_suspend` + `ERTS_BIF_YIELD3` (`erl_bif_port.c:211-231`) has
//! no counterpart in this runtime's BIF seam (whose only shapes are a value and an error, and whose
//! sole suspend is the `receive` family's `Step::Wait`), so a bare `port_command` on a busy port
//! raises [`beam_rt`'s `unsupported` gap](beam_rt::error) — never a fabricated `true`. The oracle's
//! own answer for that case is asserted separately, from a helper that clears the busy bit while the
//! caller is suspended (`{ok,cleared,echoed}`), so **both** halves of the divergence are evidenced.
//!
//! # The `--export-dynamic` link seam
//!
//! The fixture leaves its `driver_*` imports undefined, so the *loading* process must publish them.
//! Both probes open their port through a real process, and the runtime binary carries
//! `-Wl,--export-dynamic` (see `tests/driver.rs` and `build.rs`), so the seam is the binary's — this
//! test binary only needs the `black_box` anchor so the codegen unit is linked.
//!
//! # Skip policy
//!
//! Three host facilities are needed — the C compiler, the host `erl_driver.h` and `erlc` — plus the
//! **oracle** `erl` for the cross-check. A missing one prints a **loud** banner and returns; it is
//! never a silent pass.
#![deny(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

#[allow(dead_code)]
mod support;

/// The committed C source of the busy driver fixture, compiled twice (see its own header).
const BUSY_DRIVER_C: &str = include_str!("fixtures/busy_driver.c");

/// The Erlang half of the test, compiled at test time by `erlc` (`{dir}` → the fixture directory).
///
/// The fixture is never committed as a `.beam`: it interpolates the compiled driver's path, which
/// only exists at test time.
///
/// `open/2`'s guarded `erl_ddll:load_driver/2` is the **runtime selector**: the oracle must load the
/// object and then open the port by driver **name**, while this runtime has no such BIF and opens a
/// driver by the object's **path** (`beam_rt::port::driver_lookup`) — on this runtime the call
/// raises `undef`/`unsupported`, and that answer picks the path form. A driver may only be opened
/// under the name its entry declares, so every other line below is shared verbatim.
const BUSY_PROBE_ERL: &str = r#"%% Generated by `beam_rt/tests/port_busy.rs` (never committed).
-module(busy_probe).
-export([plain/1, soft/1, busy_plain/1, busy_plain_resumed/1]).
-nowarn_deprecated_catch.

plain(Dir) -> cases(Dir, "busy").
soft(Dir)  -> cases(Dir, "busysoft").

open(Dir, Name) ->
    case catch erl_ddll:load_driver(Dir, Name) of
        ok -> open_port({spawn_driver, Name}, [binary]);
        _  -> open_port({spawn_driver, Dir ++ "/" ++ Name}, [binary])
    end.

startup(Port) ->
    receive {Port, {data, _}} -> ok after 5000 -> ok end.

set_state(Port, idle) -> [] = port_control(Port, $N, <<>>);
set_state(Port, busy) -> [] = port_control(Port, $B, <<>>).

command(Port, none)      -> port_command(Port, <<"x">>);
command(Port, empty)     -> port_command(Port, <<"x">>, []);
command(Port, nosuspend) -> port_command(Port, <<"x">>, [nosuspend]);
command(Port, force)     -> port_command(Port, <<"x">>, [force]).

normalize(true)  -> ok;
normalize(false) -> dropped;
normalize(Other) -> {other, Other}.

echo(Port) ->
    receive
        {Port, {data, <<"x">>}} -> echoed;
        {Port, {data, Other}}   -> {other, Other}
    after 500 -> no_data
    end.

cases(Dir, Name) ->
    Port = open(Dir, Name),
    startup(Port),
    [{Tag, case_one(Port, State, Opts)}
     || {Tag, State, Opts} <-
            [{idle_none, idle, none},
             {idle_empty, idle, empty},
             {idle_nosuspend, idle, nosuspend},
             {idle_force, idle, force},
             {busy_nosuspend, busy, nosuspend},
             {busy_force, busy, force}]].

case_one(Port, State, Opts) ->
    set_state(Port, State),
    Result = try command(Port, Opts) of Value -> Value catch Class:Reason -> {Class, Reason} end,
    {normalize(Result), echo(Port)}.

%% A busy port with NO options: the C suspends the caller until the port is idle and then answers
%% `true`. The oracle runs this (never unbounded — see `busy_plain_resumed/1`); this runtime raises
%% its typed capability gap instead.
busy_plain(Dir) ->
    Port = open(Dir, "busy"),
    startup(Port),
    port_command(Port, <<"x">>).

%% The same call with a helper that clears the busy bit while the caller is suspended, so the oracle
%% finishes: `port_command` returns `true`, the helper reports `cleared`, and the driver's echo of
%% the command arrives as `{Port, {data, <<"x">>}}`.
busy_plain_resumed(Dir) ->
    Port = open(Dir, "busy"),
    startup(Port),
    Self = self(),
    spawn(fun() -> receive after 200 -> ok end,
                  _ = port_control(Port, $N, <<>>),
                  Self ! cleared end),
    Sent = port_command(Port, <<"x">>),
    Cleared = receive cleared -> cleared after 5000 -> no_clear end,
    {normalize(Sent), Cleared, echo(Port)}.
"#;

/// The oracle's answer for the `driver_flags = 0` variant (`driver_name "busy"`), measured on
/// erts-17.1 / OTP 29.1.1 with this very fixture. `{other, {error, notsup}}` is the probe's
/// `catch`-normalized `error:notsup`.
const EXPECTED_PLAIN: &str = "[{idle_none,{ok,echoed}},{idle_empty,{ok,echoed}},\
{idle_nosuspend,{ok,echoed}},{idle_force,{{other,{error,notsup}},no_data}},\
{busy_nosuspend,{dropped,no_data}},{busy_force,{{other,{error,notsup}},no_data}}]";

/// The oracle's answer for the `driver_flags = ERL_DRV_FLAG_SOFT_BUSY` variant (`driver_name
/// "busysoft"`): the same matrix, except that `[force]` is accepted — on an idle **and** on a busy
/// port.
const EXPECTED_SOFT: &str = "[{idle_none,{ok,echoed}},{idle_empty,{ok,echoed}},\
{idle_nosuspend,{ok,echoed}},{idle_force,{ok,echoed}},{busy_nosuspend,{dropped,no_data}},\
{busy_force,{ok,echoed}}]";

/// The oracle's answer for a plain `port_command` on a busy port whose helper clears it: the C
/// suspends the caller, resumes it `true`, and the command really reaches the driver (the echo).
const EXPECTED_RESUMED: &str = "{ok,cleared,echoed}";

/// The `beam_rt` reason for the one arm with no truthful answer (`src/native/port.rs`).
const BUSY_UNSUPPORTED: &str =
    "beam_rt: unsupported: port_command on a busy port: suspending the caller until the port is idle is not implemented";

/// Print a loud skip banner (never a silent pass).
fn skip(reason: &str) {
    println!("================================================================");
    println!("SKIP: {reason}");
    println!("================================================================");
}

// -------------------------------------------------------------------------------------------
// Locating the host `erl_driver.h`
// -------------------------------------------------------------------------------------------

/// The header the include-directory search keys on.
const HEADER: &str = "erl_driver.h";

/// Locate the host `erl_driver.h` include directory, robustly; `None` if unavailable.
///
/// Resolution order (the first directory that actually contains a readable `erl_driver.h` wins):
/// ask the host `erl` for `code:lib_dir(erts, include)`; then the `erts-*` sibling of the
/// **canonicalized** `erl` on `PATH`; then `code:root_dir()`'s own `erts-*`/`lib/erts-*` children
/// (the Nix layout, where `code:lib_dir(erts, include)` names a directory that does not exist);
/// then the standard installation roots.
fn erl_include_dir() -> Option<PathBuf> {
    from_erl_eval()
        .or_else(from_path_erl)
        .or_else(from_root_dir_erl)
        .or_else(|| standard_roots().into_iter().find_map(|root| search_root(&root)))
}

/// True when `dir` holds a readable `erl_driver.h`.
fn has_header(dir: &Path) -> bool {
    dir.join(HEADER).is_file()
}

/// Ask the host `erl` for `code:lib_dir(erts, include)`.
fn from_erl_eval() -> Option<PathBuf> {
    let dir = ask_erl("io:format(\"~s\",[code:lib_dir(erts,include)])")?;
    has_header(&dir).then_some(dir)
}

/// Ask the host `erl` for `code:root_dir()` and probe its own `erts-*` / `lib/erts-*` children.
///
/// This is the fallback that makes the probe immune to installations (the Nix store's among them)
/// where `code:lib_dir(erts, include)` answers a path that does not exist.
fn from_root_dir_erl() -> Option<PathBuf> {
    let root = ask_erl("io:format(\"~s\",[code:root_dir()])")?;
    for base in [root.clone(), root.join("lib")] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("erts-") {
                continue;
            }
            let include = entry.path().join("include");
            if has_header(&include) {
                return Some(include);
            }
        }
    }
    None
}

/// Run the host `erl` with `-eval expr -s init stop` and return its trimmed stdout, or `None`.
///
/// **Bounded**: the probe is executed through `support::run_bounded`, whose poll/kill loop is what
/// keeps a `-noshell` node from hanging the suite (see `tests/support/mod.rs`).
fn ask_erl(expr: &str) -> Option<PathBuf> {
    let erl = which_erl()?;
    let output = support::run_bounded(&erl, &["-noshell", "-eval", expr, "-s", "init", "stop"])?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// Canonicalize the `erl` on `PATH` and take the sibling `include` of its `erts-*` directory.
fn from_path_erl() -> Option<PathBuf> {
    let erl = which_erl()?.canonicalize().ok()?;
    let bin = erl.parent()?;
    let erts = bin.parent()?;
    if bin.file_name()? != "bin" || !erts.file_name()?.to_string_lossy().starts_with("erts-") {
        return None;
    }
    let include = erts.join("include");
    has_header(&include).then_some(include)
}

/// The first `erl` executable on `PATH`.
fn which_erl() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("erl"))
        .find(|candidate| candidate.is_file())
}

/// The standard installation roots to probe.
fn standard_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
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
    roots
}

/// Search one installation root for an include directory holding `erl_driver.h`.
fn search_root(root: &Path) -> Option<PathBuf> {
    let direct = root.join("include");
    if has_header(&direct) {
        return Some(direct);
    }
    let lib_erlang = root.join("lib").join("erlang");
    for base in [root.to_path_buf(), lib_erlang.clone()] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("erts-") {
                continue;
            }
            let include = entry.path().join("include");
            if has_header(&include) {
                return Some(include);
            }
        }
    }
    let include = lib_erlang.join("include");
    has_header(&include).then_some(include)
}

// -------------------------------------------------------------------------------------------
// The compiled fixture
// -------------------------------------------------------------------------------------------

/// The C compiler to use: `$CC` when set, else `cc`.
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

/// The scratch directory holding both compiled variants and the compiled Erlang probe.
struct Fixture {
    /// The scratch directory (its `.c`, its two `.so`s and `busy_probe.beam`).
    dir: PathBuf,
}

impl Fixture {
    /// Removes the scratch directory (`cc`/`erlc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }

    /// The driver object's **stem** path (no suffix) for `name` — what an `erl_ddll`-less runtime
    /// opens the port with.
    fn stem(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }
}

/// Compiles the committed C fixture into one shared object per `driver_flags` variant, or `None`
/// when a host facility is missing (**loudly**).
///
/// One source yields both: `-DBUSY_DRIVER_SOFT=1` selects `driver_name "busysoft"` +
/// `ERL_DRV_FLAG_SOFT_BUSY`, the committed default `driver_name "busy"` + `driver_flags = 0`.
fn build_fixture(label: &str) -> Option<Fixture> {
    let Some(include) = erl_include_dir() else {
        skip(&format!(
            "port_busy/{label}: no erl_driver.h include directory found"
        ));
        return None;
    };
    if !cc_available() {
        skip(&format!(
            "port_busy/{label}: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    let dir = std::env::temp_dir().join(format!("beam_rt_port_busy_{label}_{}", std::process::id()));
    let source = dir.join("busy_driver.c");
    if let Err(error) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&source, BUSY_DRIVER_C))
    {
        skip(&format!(
            "port_busy/{label}: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    for (stem, defines) in [("busy", &[][..]), ("busysoft", &["-DBUSY_DRIVER_SOFT=1"][..])] {
        let mut command = Command::new(cc());
        command.arg("-shared").arg("-fPIC");
        command.arg(format!("-I{}", include.display()));
        command.args(defines);
        command.arg("-o").arg(dir.join(format!("{stem}.so"))).arg(&source);
        match command.output() {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                skip(&format!(
                    "port_busy/{label}: cc failed for {stem} ({}):\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ));
                return None;
            }
            Err(error) => {
                skip(&format!(
                    "port_busy/{label}: could not run the C compiler: {error}"
                ));
                return None;
            }
        }
    }
    Some(Fixture { dir })
}

/// Compiles the Erlang probe into `<dir>/busy_probe.beam` with the system `erlc`, or `None` when
/// the compiler is missing (**loudly**).
fn compile_probe(dir: &Path) -> Option<PathBuf> {
    let source_path = dir.join("busy_probe.erl");
    if let Err(error) = std::fs::write(&source_path, BUSY_PROBE_ERL) {
        skip(&format!(
            "port_busy/probe: cannot write {}: {error}",
            source_path.display()
        ));
        return None;
    }
    let output = match Command::new("erlc").arg("-o").arg(dir).arg(&source_path).output() {
        Ok(output) => output,
        Err(error) => {
            skip(&format!("port_busy/probe: cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        skip(&format!(
            "port_busy/probe: `erlc` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    let beam = dir.join("busy_probe.beam");
    beam.is_file().then_some(beam)
}

// -------------------------------------------------------------------------------------------
// The two probes
// -------------------------------------------------------------------------------------------

/// This runtime's answer: `run` loads the module and drives the full scheduler, printing the
/// returned term with `~w~n` (the oracle's own rendering).
///
/// `export` is `plain/1` or `soft/1` — one export per variant, so the driver name never has to
/// travel through the CLI's argument grammar.
fn run_beam_rt(beam: &Path, dir: &Path, export: &str) -> Option<std::process::Output> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_beam_rt"));
    support::run_bounded(
        &binary,
        &[
            "-noshell",
            "-pa",
            &dir.display().to_string(),
            "run",
            &beam.display().to_string(),
            &format!("busy_probe:{export}"),
            &dir.display().to_string(),
        ],
    )
}

/// The oracle's answer for the same probe (`io:format("~w~n", [busy_probe:<call>])`), bounded.
fn run_oracle(oracle: &Path, dir: &Path, call: &str) -> Option<String> {
    let expr = format!("io:format(\"~w~n\",[{call}]), halt(0).");
    support::oracle_eval(oracle, Some(dir), &expr)
}

// -------------------------------------------------------------------------------------------
// The tests
// -------------------------------------------------------------------------------------------

/// The six-arm `port_command` matrix reaches **both** `driver_flags` variants of a real driver and
/// answers exactly as the oracle does.
///
/// For each variant the oracle's own line is asserted against the recorded constant **and** this
/// runtime's line is asserted against the oracle's, so a drift on either side is named: an oracle
/// upgrade that changes the table fails the first assertion, and a runtime regression fails the
/// second.
#[test]
fn the_busy_port_port_command_matrix_agrees_with_the_oracle() {
    // The A.4 link seam: the `.so` leaves its `driver_*` imports undefined, so the loading process
    // must publish them (the binary carries `--export-dynamic`; this anchors the codegen unit).
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());

    let Some(fixture) = build_fixture("matrix") else {
        return;
    };
    let Some(beam) = compile_probe(&fixture.dir) else {
        fixture.cleanup();
        return;
    };
    let Some(oracle) = support::require_oracle("port_busy/matrix") else {
        fixture.cleanup();
        return;
    };

    for (export, expected) in [("plain", EXPECTED_PLAIN), ("soft", EXPECTED_SOFT)] {
        let Some(oracle_line) = run_oracle(&oracle, &fixture.dir, &format!("busy_probe:{export}(\"{}\")", fixture.dir.display()))
        else {
            skip(&format!(
                "port_busy/matrix: the oracle could not answer busy_probe:{export}/1"
            ));
            fixture.cleanup();
            return;
        };
        assert_eq!(
            oracle_line, expected,
            "the recorded oracle table for `{export}` no longer matches the live oracle"
        );

        let Some(output) = run_beam_rt(&beam, &fixture.dir, &format!("{export}/1")) else {
            panic!("beam_rt run busy_probe:{export}/1 did not terminate");
        };
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_eq!(
            output.status.code(),
            Some(0),
            "run busy_probe:{export}/1 must succeed; stderr={stderr}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            oracle_line,
            "the `{export}` matrix must answer exactly as the oracle; stderr={stderr}"
        );
    }

    fixture.cleanup();
}

/// A plain `port_command` on a **busy** port is this runtime's documented capability gap — exit 3
/// and the `unsupported` banner — where the oracle **suspends** the caller until the port is idle
/// and then answers `true`.
///
/// The oracle side is deliberately *not* run here: with nothing to clear the busy bit that call
/// blocks for ever (the C's `erts_suspend`), so running it would hang the probe. The oracle's own
/// half of the divergence is asserted by
/// [`a_busy_ports_plain_command_suspends_the_oracle_caller_until_it_is_cleared`], which clears the
/// bit from a helper.
#[test]
fn a_busy_ports_plain_command_is_the_documented_capability_gap() {
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());

    let Some(fixture) = build_fixture("gap") else {
        return;
    };
    let Some(beam) = compile_probe(&fixture.dir) else {
        fixture.cleanup();
        return;
    };

    // `--test` is the test binary; the CLI contract is what is under test here, so drive the
    // binary directly with the same `run` action the other suites use.
    let Some(output) = run_beam_rt(&beam, &fixture.dir, "busy_plain/1") else {
        panic!("beam_rt run busy_probe:busy_plain/1 did not terminate");
    };
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(3),
        "a busy port with no options is a capability gap (exit 3), never a completion; \
         stderr={stderr}"
    );
    assert_eq!(
        stderr.trim(),
        BUSY_UNSUPPORTED,
        "the gap must carry the runtime's own reason"
    );

    fixture.cleanup();
}

/// The oracle's half of the same case: a plain `port_command` on a busy port really **suspends**
/// the caller, and the driver's echo arrives once the busy bit is cleared.
///
/// A helper process clears the bit (`port_control(Port, $N, <<>>)`) 200 ms in, while the caller is
/// inside `port_command`. The oracle then answers `true`, the helper's `cleared` arrives, and the
/// command really reached the driver (`{ok,cleared,echoed}`) — the C's
/// `erts_suspend`/`erts_port_resume_procs` round trip, which is exactly what this runtime's BIF
/// seam cannot express.
#[test]
fn a_busy_ports_plain_command_suspends_the_oracle_caller_until_it_is_cleared() {
    let Some(fixture) = build_fixture("resumed") else {
        return;
    };
    let Some(_beam) = compile_probe(&fixture.dir) else {
        fixture.cleanup();
        return;
    };
    let Some(oracle) = support::require_oracle("port_busy/resumed") else {
        fixture.cleanup();
        return;
    };

    let call = format!("busy_probe:busy_plain_resumed(\"{}\")", fixture.dir.display());
    let Some(line) = run_oracle(&oracle, &fixture.dir, &call) else {
        skip("port_busy/resumed: the oracle could not answer busy_probe:busy_plain_resumed/1");
        fixture.cleanup();
        return;
    };
    assert_eq!(
        line, EXPECTED_RESUMED,
        "the oracle suspends the caller, resumes it `true` and delivers the command"
    );

    fixture.cleanup();
}
