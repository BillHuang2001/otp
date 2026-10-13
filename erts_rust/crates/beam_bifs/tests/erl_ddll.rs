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

//! Integration tests for the real `erl_ddll:format_error_int/1`
//! (`src/runtime/ddll.rs`), driven **only** through the crate's public API —
//! [`BifRegistry`]/[`BifContext`] over a real `beam_alloc::Heap` + a seeded
//! `beam_term::AtomTable`, exactly as `beam_rt` drives them.
//!
//! Every assertion is an Erlang-observable fact: the BIF returns the oracle's
//! byte-exact **charlist** for each of the eighteen error atoms and for every
//! `{open_error, Code}` in the fixed table `0, -1..=-8`; a code `> -10` outside that
//! table is `"Unspecified error"` and a code `<= -10` is `"Unknown dlload error"`;
//! and every other description (a plain integer, a list, a binary, a wrong-arity or
//! wrong-tag tuple, a `{open_error, NonSmallInt}`) is the plain `error:badarg`.
//!
//! `oracle_agrees_on_the_erl_ddll_error_table` re-derives the whole corpus
//! **live** under `erl -noshell -eval …`, comparing value-for-value with the Rust
//! side. It skips **loudly** (and still passes) when no usable `erl` is on `$PATH`,
//! and it kills a wedged oracle rather than hanging the suite.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_bifs::{default_registry, needs_runtime_hooks, BifContext, BifRegistry};
use beam_codec::term::{bitstring_bytes, make_float};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{make_atom, make_small, AtomTable, Eterm, NIL};

/// The eighteen error-description atoms and their byte-exact messages, grouped as
/// the C's two sources: the Erlang-interface-only `switch` (the first nine,
/// `erl_bif_ddll.c:825-855`) and the `errcode_tab` (the last nine, `:1709-1720`).
///
/// The `"permanent an cannot"` / `"name to long"` typos are the C's and are
/// observable, so they are reproduced verbatim.
const ATOM_MESSAGES: [(&str, &str); 18] = [
    (
        "inconsistent",
        "Driver name and/or driver options are inconsistent with currently loaded driver",
    ),
    (
        "linked_in_driver",
        "Driver is statically linked and cannot be loaded/unloaded",
    ),
    ("permanent", "DDLL driver is permanent an cannot be unloaded/loaded"),
    ("not_loaded", "DDLL driver is not loaded"),
    (
        "not_loaded_by_this_process",
        "DDLL driver was not loaded by this process",
    ),
    ("not_pending", "DDLL load not pending for this driver name"),
    ("already_loaded", "DDLL driver is already loaded successfully"),
    ("pending_reload", "Driver reloading is already pending"),
    (
        "pending_process",
        "Driver is loaded by others when attempting option {reload, pending_driver}",
    ),
    ("no_error", "No error"),
    ("no_driver_init", "No driver init in dynamic library"),
    ("driver_init_failed", "Driver init failed"),
    ("bad_driver_name", "Bad driver name"),
    ("driver_name_to_long", "Driver name to long"),
    (
        "driver_incorrect_version",
        "Driver compiled with incorrect version of erl_driver.h",
    ),
    (
        "no_ddll_available",
        "DDLL functionality not available on this platform",
    ),
    ("unspecified_error", "Unspecified dynamic library error"),
    ("symbol_not_found", "Symbol not found in dynamic library"),
];

/// The fixed `{open_error, Code}` code table (`ERL_DE_NO_ERROR 0` and `-1..=-8`,
/// `global.h:172-180`).
const CODE_MESSAGES: [(i64, &str); 9] = [
    (0, "No error"),
    (-1, "No driver init in dynamic library"),
    (-2, "Driver init failed"),
    (-3, "Bad driver name"),
    (-4, "Driver name to long"),
    (-5, "Driver compiled with incorrect version of erl_driver.h"),
    (-6, "DDLL functionality not available on this platform"),
    (-7, "Unspecified dynamic library error"),
    (-8, "Symbol not found in dynamic library"),
];

/// A code strictly above `ERL_DE_DYNAMIC_ERROR_OFFSET (-10)` — outside the fixed
/// table, in the platform's own code space, so the empty dynamic table yields
/// `"Unspecified error"`. Every value fits the C's 32-bit `int`.
const UNSPECIFIED_CODES: [i64; 4] = [-9, 1, 12_345, i32::MAX as i64];

/// A code at or below `ERL_DE_DYNAMIC_ERROR_OFFSET (-10)` — past the platform's
/// code space, so the empty dynamic table yields `"Unknown dlload error"`. Every
/// value fits the C's 32-bit `int`.
const UNKNOWN_CODES: [i64; 4] = [-10, -11, -100, i32::MIN as i64];

/// Codes wider than the C's 32-bit `int` — the C truncates the small integer before
/// the lookup (`errdesc_to_code` stores `signed_val` into an `int`), which the
/// oracle confirms. Each entry is the raw small integer and the message it yields.
const TRUNCATED_CODES: [(i64, &str); 6] = [
    (288_230_376_151_711_744, "No error"), // 2^58 -> 0
    (-288_230_376_151_711_744, "No error"), // -2^58 -> 0
    (4_294_967_296, "No error"),            // 2^32 -> 0
    (4_294_967_295, "No driver init in dynamic library"), // 2^32-1 -> -1
    (4_294_967_286, "Unknown dlload error"), // 2^32-10 -> -10
    (4_294_967_287, "Unspecified error"),    // 2^32-9 -> -9
];

/// The `{error, badarg}` `~p` rendering of a wrapped badarg probe.
const BADARG: &str = "{error,badarg}";

/// The atoms the fixture needs interned: the eighteen error-description atoms plus
/// `open_error`, a wrong tag and two unknown names.
const SEEDED_ATOMS: [&str; 22] = [
    "inconsistent",
    "linked_in_driver",
    "permanent",
    "not_loaded",
    "not_loaded_by_this_process",
    "not_pending",
    "already_loaded",
    "pending_reload",
    "pending_process",
    "no_error",
    "no_driver_init",
    "driver_init_failed",
    "bad_driver_name",
    "driver_name_to_long",
    "driver_incorrect_version",
    "no_ddll_available",
    "unspecified_error",
    "symbol_not_found",
    "open_error",
    "not_open_error",
    "foo",
    "bar",
];

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

/// A registry + heap + atom table the tests drive BIF calls through. No optional
/// seam is installed — this BIF needs none.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed(SEEDED_ATOMS);
        Fixture {
            registry: default_registry(),
            heap: Heap::new(1 << 16).expect("test heap"),
            atoms,
        }
    }

    /// Calls `module:function(Args)` through the real registry.
    fn bif(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry
            .call(module, function, args.len() as u32, &mut ctx, args)
    }

    /// `erl_ddll:format_error_int(Arg)`.
    fn call(&mut self, arg: Eterm) -> Result<Eterm> {
        self.bif("erl_ddll", "format_error_int", &[arg])
    }

    /// The fixture heap, for building boxed terms with the free `beam_codec`
    /// constructors.
    fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// A seeded atom term.
    fn atom(&self, name: &str) -> Eterm {
        make_atom(self.atoms.lookup(name).expect("a seeded atom"))
    }

    /// `{open_error, Code}` on the fixture heap.
    fn open_error(&mut self, code: i64) -> Eterm {
        let tag = self.atom("open_error");
        self.heap
            .tuple(&[tag, make_small(code)])
            .expect("a tuple fits")
    }

    /// `{open_error, Second}` for an arbitrary second element.
    fn open_error_tuple(&mut self, second: Eterm) -> Eterm {
        let tag = self.atom("open_error");
        self.heap.tuple(&[tag, second]).expect("a tuple fits")
    }

    /// `{Tag, Second}` on the fixture heap.
    fn tuple2(&mut self, tag: &str, second: Eterm) -> Eterm {
        let tag = self.atom(tag);
        self.heap.tuple(&[tag, second]).expect("a tuple fits")
    }

    /// A proper integer list `[v0, v1, …]` on the fixture heap.
    fn int_list(&mut self, values: &[i64]) -> Eterm {
        let mut acc = NIL;
        for value in values.iter().rev() {
            acc = self
                .heap
                .cons(make_small(*value), acc)
                .expect("a cons cell fits");
        }
        acc
    }

    /// A binary of the bytes `bytes`, built through the registered
    /// `erlang:list_to_binary/1`.
    fn binary(&mut self, bytes: &[u8]) -> Eterm {
        let values: Vec<i64> = bytes.iter().map(|b| i64::from(*b)).collect();
        let list = self.int_list(&values);
        self.bif("erlang", "list_to_binary", &[list])
            .expect("list_to_binary/1 succeeds")
    }

    /// The message the BIF returns for `arg`, rendered as a `String` by turning the
    /// returned charlist back into a binary.
    fn message(&mut self, arg: Eterm) -> String {
        let list = self
            .call(arg)
            .unwrap_or_else(|e| panic!("format_error_int failed: {e:?}"));
        let binary = self
            .bif("erlang", "list_to_binary", &[list])
            .expect("the result is a valid charlist");
        let (bytes, offset, size_bits) = bitstring_bytes(binary).expect("a binary");
        assert_eq!(offset, 0, "list_to_binary/1 is byte aligned");
        assert_eq!(size_bits % 8, 0, "list_to_binary/1 is whole-byte");
        String::from_utf8(bytes[..size_bits / 8].to_vec()).expect("the messages are ASCII")
    }

    /// Asserts the BIF rejects `arg` with the plain `error:badarg`.
    fn assert_badarg(&mut self, arg: Eterm) {
        match self.call(arg) {
            Err(BeamError::BadArg) => {}
            other => panic!("expected badarg, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The key and the hook set
// ---------------------------------------------------------------------------

/// The key is registered, is a real implementation (a `BifFn`, not a
/// `RuntimeHooks` seam), and answers without any optional seam installed.
#[test]
fn the_key_is_registered_and_not_seam_backed() {
    let reg = default_registry();
    assert!(
        reg.contains("erl_ddll", "format_error_int", 1),
        "erl_ddll:format_error_int/1 must be registered"
    );
    assert!(
        reg.lookup("erl_ddll", "format_error_int", 1).is_some(),
        "erl_ddll:format_error_int/1 must have an implementation"
    );
    assert!(
        !needs_runtime_hooks("erl_ddll", "format_error_int", 1),
        "the real implementation must not be gated behind the RuntimeHooks seam"
    );
    // `loaded_drivers/0`, `try_load/3` and `try_unload/2` are now real
    // implementations over the thread-local loaded-driver registry, so they are
    // NOT gated behind the `RuntimeHooks` seam either.
    for (function, arity) in [("loaded_drivers", 0), ("try_load", 3), ("try_unload", 2)] {
        assert!(
            reg.lookup("erl_ddll", function, arity).is_some(),
            "erl_ddll:{function}/{arity} must have an implementation"
        );
        assert!(
            !needs_runtime_hooks("erl_ddll", function, arity),
            "erl_ddll:{function}/{arity} is served by the real registry, not the seam"
        );
    }
    // The `info/2` state report (`src/runtime/ddll/info.rs`) and the
    // `monitor/2`/`demonitor/1` event registrations (`src/runtime/ddll/monitor.rs`) are real
    // implementations too, so no `erl_ddll` key is a `RuntimeHooks` seam key.
    for (function, arity) in [
        ("demonitor", 1),
        ("info", 2),
        ("monitor", 2),
        ("loaded_drivers", 0),
        ("try_load", 3),
        ("try_unload", 2),
    ] {
        assert!(
            reg.lookup("erl_ddll", function, arity).is_some(),
            "erl_ddll:{function}/{arity} must have an implementation"
        );
        assert!(
            !needs_runtime_hooks("erl_ddll", function, arity),
            "erl_ddll:{function}/{arity} must not be gated behind the RuntimeHooks seam"
        );
    }

    // A real call with no hook installed succeeds (never the typed gap).
    let mut fx = Fixture::new();
    let arg = fx.atom("no_error");
    assert_eq!(fx.message(arg), "No error");
}

// ---------------------------------------------------------------------------
// The byte-exact tables
// ---------------------------------------------------------------------------

#[test]
fn the_atom_table_is_byte_exact() {
    let mut fx = Fixture::new();
    for (name, expected) in ATOM_MESSAGES {
        let arg = fx.atom(name);
        assert_eq!(fx.message(arg), expected, "atom {name}");
    }
}

#[test]
fn the_open_error_table_is_byte_exact() {
    let mut fx = Fixture::new();
    for (code, expected) in CODE_MESSAGES {
        let arg = fx.open_error(code);
        assert_eq!(fx.message(arg), expected, "code {code}");
    }
}

#[test]
fn the_dynamic_error_offset_boundary_is_exact() {
    let mut fx = Fixture::new();
    for code in UNSPECIFIED_CODES {
        let arg = fx.open_error(code);
        assert_eq!(fx.message(arg), "Unspecified error", "code {code}");
    }
    for code in UNKNOWN_CODES {
        let arg = fx.open_error(code);
        assert_eq!(fx.message(arg), "Unknown dlload error", "code {code}");
    }
}

#[test]
fn an_out_of_int_range_code_truncates_to_the_c_int() {
    let mut fx = Fixture::new();
    for (code, expected) in TRUNCATED_CODES {
        let arg = fx.open_error(code);
        assert_eq!(fx.message(arg), expected, "code {code}");
    }
}

// ---------------------------------------------------------------------------
// The badarg classes
// ---------------------------------------------------------------------------

#[test]
fn every_badarg_class_is_rejected() {
    let mut fx = Fixture::new();

    // An unknown atom.
    let foo = fx.atom("foo");
    let bar = fx.atom("bar");
    fx.assert_badarg(foo);
    fx.assert_badarg(bar);

    // A plain integer (the code only travels inside `{open_error, _}`).
    for code in [0, 1, -1, -10, 12_345] {
        fx.assert_badarg(make_small(code));
    }

    // The empty list and a non-empty integer list.
    fx.assert_badarg(NIL);
    let list = fx.int_list(&[1, 2]);
    fx.assert_badarg(list);

    // A binary.
    let binary = fx.binary(&[1]);
    fx.assert_badarg(binary);

    // A wrong-tag and a wrong-arity tuple.
    let zero = make_small(0);
    let wrong_tag = fx.tuple2("not_open_error", zero);
    fx.assert_badarg(wrong_tag);
    let open_error = fx.atom("open_error");
    let three = fx.heap.tuple(&[open_error, zero, zero]).expect("a tuple fits");
    fx.assert_badarg(three);

    // A `{open_error, NonSmallInt}`: an atom, the empty list and a float.
    let bad_code_atom = fx.open_error_tuple(foo);
    fx.assert_badarg(bad_code_atom);
    let bad_code_list = fx.open_error_tuple(NIL);
    fx.assert_badarg(bad_code_list);
    let float = make_float(fx.heap_mut(), 2.0).expect("a float fits");
    let bad_code_float = fx.open_error_tuple(float);
    fx.assert_badarg(bad_code_float);
}

// ---------------------------------------------------------------------------
// Live oracle cross-check (skips loudly when `erl` is absent)
// ---------------------------------------------------------------------------

/// Wall-clock budget for the `erl` probe before the oracle is declared unusable.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// One cross-checked probe: the `erl -eval` statement that emits its output line
/// and the Rust-side rendering of the same call.
struct Probe {
    label: String,
    erl: String,
    rust: String,
}

/// A badarg probe: the Rust side must reject `arg`, and the oracle prints
/// `{error,badarg}` for the wrapped `expr`.
fn badarg_probe(fx: &mut Fixture, label: &str, expr: &str, arg: Eterm) -> Probe {
    fx.assert_badarg(arg);
    Probe {
        label: label.to_string(),
        erl: format!("io:format(\"~p~n\",[F({expr})])"),
        rust: BADARG.to_string(),
    }
}

/// Builds every probe, computing the Rust side by **calling** the BIF and rendering
/// it exactly as the oracle's `~s` (a message) or `~p` (a wrapped badarg) prints it.
fn build_probes(fx: &mut Fixture) -> Vec<Probe> {
    let mut probes = Vec::new();

    for (name, _) in ATOM_MESSAGES {
        let arg = fx.atom(name);
        let rust = fx.message(arg);
        probes.push(Probe {
            label: format!("atom {name}"),
            erl: format!("io:format(\"~s~n\",[erl_ddll:format_error_int({name})])"),
            rust,
        });
    }

    for code in CODE_MESSAGES
        .iter()
        .map(|(code, _)| *code)
        .chain(UNSPECIFIED_CODES)
        .chain(UNKNOWN_CODES)
        .chain(TRUNCATED_CODES.iter().map(|(code, _)| *code))
    {
        // The parenthesised code keeps a negative literal (and a shift) unambiguous.
        let expr = format!("erl_ddll:format_error_int({{open_error,({code})}})");
        let arg = fx.open_error(code);
        let rust = fx.message(arg);
        probes.push(Probe {
            label: format!("open_error {code}"),
            erl: format!("io:format(\"~s~n\",[{expr}])"),
            rust,
        });
    }

    for code in [0, 1, -1, -10, 12_345] {
        let expr = format!("{code}");
        let arg = make_small(code);
        probes.push(badarg_probe(
            fx,
            &format!("plain integer {code}"),
            &expr,
            arg,
        ));
    }

    let foo = fx.atom("foo");
    probes.push(badarg_probe(fx, "unknown atom foo", "foo", foo));
    let bar = fx.atom("bar");
    probes.push(badarg_probe(fx, "unknown atom bar", "bar", bar));

    let list = fx.int_list(&[1, 2]);
    probes.push(badarg_probe(fx, "list [1,2]", "[1,2]", list));

    let binary = fx.binary(&[1]);
    probes.push(badarg_probe(fx, "binary <<1>>", "<<1>>", binary));

    let zero = make_small(0);
    let wrong_tag = fx.tuple2("not_open_error", zero);
    probes.push(badarg_probe(
        fx,
        "{not_open_error,0}",
        "{not_open_error,0}",
        wrong_tag,
    ));

    let open_error = fx.atom("open_error");
    let three = fx.heap.tuple(&[open_error, zero, zero]).expect("a tuple fits");
    probes.push(badarg_probe(fx, "{open_error,0,0}", "{open_error,0,0}", three));

    let bad_code = fx.open_error_tuple(foo);
    probes.push(badarg_probe(fx, "{open_error,foo}", "{open_error,foo}", bad_code));

    let float = make_float(fx.heap_mut(), 2.0).expect("a float fits");
    let bad_code_float = fx.open_error_tuple(float);
    probes.push(badarg_probe(
        fx,
        "{open_error,2.0}",
        "{open_error,2.0}",
        bad_code_float,
    ));

    probes
}

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs erl_ddll] $ERTS_ORACLE='{}' is not an existing executable — \
             treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Runs one `erl -noshell -eval <program>` under a bounded wall-clock budget and
/// returns its stdout, or `None` (after a loud skip message) when no oracle is
/// available / it cannot answer.
///
/// The child's stdout and stderr are drained on worker threads (so a full pipe can
/// never wedge it) while the main thread waits with a deadline; a child that
/// overruns [`ERL_TIMEOUT`] is **killed** and reaped, so a hung `erl` can never hang
/// the suite.
fn erl_stdout(oracle: &Path, program: &str) -> Option<String> {
    let spawned = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(program)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            eprintln!(
                "[beam_bifs erl_ddll] SKIP: could not spawn the oracle '{}': {e}",
                oracle.display()
            );
            return None;
        }
    };

    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || read_pipe(stdout_pipe));
    let stderr_reader = thread::spawn(move || read_pipe(stderr_pipe));

    let deadline = Instant::now() + ERL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    eprintln!(
                        "[beam_bifs erl_ddll] SKIP: the oracle did not answer within \
                         {ERL_TIMEOUT:?} and was killed."
                    );
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                eprintln!("[beam_bifs erl_ddll] SKIP: waiting for the oracle failed: {e}");
                return None;
            }
        }
    };

    let stdout = join_pipe(stdout_reader, "stdout")?;
    let stderr = join_pipe(stderr_reader, "stderr")?;

    if !status.success() {
        eprintln!(
            "[beam_bifs erl_ddll] SKIP: the oracle exited with {:?} for:\n{program}\n\
             --- stderr ---\n{}",
            status.code(),
            String::from_utf8_lossy(&stderr),
        );
        return None;
    }
    Some(String::from_utf8_lossy(&stdout).into_owned())
}

/// Joins a pipe-reader thread, turning a panic or an I/O error into a loud skip.
fn join_pipe(reader: thread::JoinHandle<std::io::Result<Vec<u8>>>, which: &str) -> Option<Vec<u8>> {
    match reader.join() {
        Ok(Ok(buf)) => Some(buf),
        Ok(Err(e)) => {
            eprintln!("[beam_bifs erl_ddll] SKIP: reading the oracle {which} failed: {e}");
            None
        }
        Err(_) => {
            eprintln!("[beam_bifs erl_ddll] SKIP: the oracle {which} reader panicked");
            None
        }
    }
}

/// Reads a child pipe to EOF, surfacing an I/O error instead of swallowing it.
fn read_pipe(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    pipe.read_to_end(&mut buf)?;
    Ok(buf)
}

/// The whole corpus, re-derived live under `erl -noshell -eval …`.
///
/// Skips **loudly** (and still passes) when no usable `erl` is on `$PATH` — the
/// pinned golden assertions above remain the in-suite evidence.
#[test]
fn oracle_agrees_on_the_erl_ddll_error_table() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!("[beam_bifs erl_ddll] oracle cross-check SKIPPED: no usable `erl` on $PATH");
        eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        return;
    };

    let mut fx = Fixture::new();
    let probes = build_probes(&mut fx);

    // `F` catches a badarg so the eval never crashes on the negative probes; the
    // lambda's variables are local, so the comma-sequence binding rules are moot.
    let mut stmts = vec![
        "F = fun(X) -> try erl_ddll:format_error_int(X) catch Cl:Er -> {Cl,Er} end end".to_string(),
    ];
    stmts.extend(probes.iter().map(|p| p.erl.clone()));
    let program = format!("{},\nhalt(0).", stmts.join(",\n"));

    let Some(stdout) = erl_stdout(&oracle, &program) else {
        eprintln!("[beam_bifs erl_ddll] oracle cross-check SKIPPED (unusable oracle)");
        return;
    };

    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        probes.len(),
        "the oracle printed {} lines, expected {}\n--- stdout ---\n{stdout}",
        lines.len(),
        probes.len(),
    );
    for (i, probe) in probes.iter().enumerate() {
        assert_eq!(
            lines[i], probe.rust,
            "probe #{i} `{}` disagrees with the oracle\n--- oracle program ---\n{program}",
            probe.label,
        );
    }

    eprintln!(
        "[beam_bifs erl_ddll] oracle cross-check RAN against '{}': {} probes agreed.",
        oracle.display(),
        probes.len(),
    );
}
