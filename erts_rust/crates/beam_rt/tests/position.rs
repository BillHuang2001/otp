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

//! Execution-position introspection at the **scheduler** boundary (contract A.8).
//!
//! `erlang:process_info/2`'s four execution-position items — `current_function`,
//! `initial_call`, `current_location` and `current_stacktrace` — describe *where a
//! process is*. They are read from a process's saved instruction pointer, which means
//! they can only be answered for a process this runtime has an engine for, and only from
//! a second process: a scheduler pass takes the *running* process's machine out of the
//! runner state, so a process asking about **itself** is the honest capability gap.
//! [`beam_engine`]'s `Machine::current_*` accessors and `beam_rt`'s `src/native/memory`
//! item classification have their own unit tests; this test pins the whole path end to end:
//! a real `.beam` is spawned as a process, it parks in a `receive`, a **second** process
//! probes it, and the returned term is compared line for line against the live system `erl`
//! oracle.
//!
//! Oracle ground truth (system `erl`, OTP 29 / `erts-17.1`; `recv.erl` compiled with the
//! scratch directory as cwd and `recv.erl` as the argument, so the recorded source name is
//! exactly `"recv.erl"` and `wait/0`'s clause head is on line 12):
//!
//! ```none
//! $ erl -noshell -pa <scratch> -eval '
//!     P = spawn(recv, wait, []),
//!     timer:sleep(50),
//!     io:format("~p~n", [process_info(P, [current_function, initial_call,
//!         current_location, current_stacktrace])]),
//!     halt(0).'
//! [{current_function,{recv,wait,0}},
//!  {initial_call,{recv,wait,0}},
//!  {current_location,{recv,wait,0,[{file,"recv.erl"},{line,12}]}},
//!  {current_stacktrace,[{recv,wait,0,[{file,"recv.erl"},{line,12}]}]}]
//! ```
//!
//! The fixture (`tests/fixtures/recv.erl`) is compiled **at test time** with the system
//! `erlc`, so nothing path-dependent is committed; a missing `erl`/`erlc` is a **loud**
//! skip, never a silent pass. The scratch directory never contains a `lists.beam`, so it
//! is safe to hand to the oracle as a single `-pa` (unlike this crate's `tests/fixtures/`).
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_engine::terms;
use beam_rt::Runtime;
use beam_term::{signed_value, Eterm};

/// The committed fixture sources (compiled at test time — never committed as `.beam`s).
const RECV_ERL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/recv.erl");
const POS_SELF_ERL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/pos_self.erl"
);

/// Every fixture source a case may need: its name in the scratch directory and its path
/// under `tests/fixtures/`.
const FIXTURE_SOURCES: [(&str, &str); 2] =
    [("recv.erl", RECV_ERL), ("pos_self.erl", POS_SELF_ERL)];

/// The four items, in the order `probe_b/1` requests them.
const ITEMS: [&str; 4] = [
    "current_function",
    "initial_call",
    "current_location",
    "current_stacktrace",
];

// ---------------------------------------------------------------------------------------
// Toolchain discovery + the loud skip
// ---------------------------------------------------------------------------------------

/// Whether `p` is an existing executable file.
fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// The first `name` on `$PATH` that is an executable file.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// The oracle, `$ERTS_ORACLE` first (the tools' convention), then `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        return is_executable(&program).then_some(program);
    }
    find_on_path("erl")
}

/// The compiler: a sibling of the oracle first (the same installation), then `$PATH`.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(oracle) = resolve_oracle() {
        if let Some(dir) = oracle.parent() {
            let candidate = dir.join("erlc");
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    find_on_path("erlc")
}

/// Prints the loud skip banner — a missing toolchain is never a silent pass.
fn skip_loudly(why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt process_info position SKIPPED ===============");
    eprintln!("{why}: the execution-position end-to-end proof cannot run.");
    eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
    eprintln!("======================================================================");
    eprintln!();
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------

/// A compiled fixture in its own scratch directory (removed on drop).
struct Fixture {
    /// The scratch directory holding the compiled `*.erl`/`*.beam` sources.
    dir: PathBuf,
    /// The compiled `recv.beam`.
    beam: PathBuf,
}

impl Fixture {
    /// `module`'s compiled `.beam` in the scratch directory.
    fn beam_of(&self, module: &str) -> PathBuf {
        self.dir.join(format!("{module}.beam"))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Best effort, but never silent: a leftover scratch dir must not fail the test,
        // yet a cleanup failure is reported rather than swallowed.
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Compiles the committed fixture sources into a scratch directory, or `None` when the
/// system toolchain is unavailable (the caller then returns early — **loudly**).
///
/// Each source is copied to `<scratch>/<name>` and every one is compiled in a single
/// `erlc` run **with the scratch directory as cwd** and the bare file names as arguments,
/// so the file name recorded in each `.beam`'s `Line` chunk is exactly `<name>` — which is
/// what the oracle reports in `current_location`/`current_stacktrace` (an absolute or
/// repo-relative argument would record *that* string instead and the two would disagree).
fn build_fixture(label: &str) -> Option<(Fixture, PathBuf, PathBuf)> {
    let Some(oracle) = resolve_oracle() else {
        skip_loudly(&format!("position/{label}: no `erl` on $PATH"));
        return None;
    };
    let Some(erlc) = resolve_erlc() else {
        skip_loudly(&format!("position/{label}: no `erlc` available"));
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("beam_rt_position_{label}_{}_{n}", std::process::id()));
    let mut names = Vec::new();
    for (name, path) in FIXTURE_SOURCES {
        let source = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                skip_loudly(&format!("position/{label}: cannot read {path}: {error}"));
                return None;
            }
        };
        if let Err(error) =
            std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join(name), &source))
        {
            skip_loudly(&format!(
                "position/{label}: cannot prepare {}/{}: {error}",
                dir.display(),
                name
            ));
            return None;
        }
        names.push(name);
    }
    // cwd = the scratch dir, bare file names: the recorded source names are the bare names.
    let mut command = Command::new(&erlc);
    command.arg("-o").arg(&dir).args(&names);
    let output = match command
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            skip_loudly(&format!("position/{label}: cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        skip_loudly(&format!(
            "position/{label}: `erlc` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    let beam = dir.join("recv.beam");
    if !beam.is_file() {
        skip_loudly(&format!("position/{label}: `erlc` produced no {}", beam.display()));
        return None;
    }
    Some((Fixture { dir, beam }, oracle, erlc))
}

// ---------------------------------------------------------------------------------------
// The oracle probe
// ---------------------------------------------------------------------------------------

/// The four **canonical scalar lines** the oracle prints for a parked `recv:wait/0`, one per
/// item, so the live `erl` answer and this runtime's answer can be compared directly.
const ORACLE_PROBE: &str = "\
P = spawn(recv, wait, []),
timer:sleep(50),
{current_function,{M1,F1,A1}} = process_info(P, current_function),
{initial_call,{M2,F2,A2}} = process_info(P, initial_call),
{current_location,{M3,F3,A3,Loc}} = process_info(P, current_location),
{current_stacktrace,ST} = process_info(P, current_stacktrace),
[{file,File},{line,Line}] = Loc,
[{M4,F4,A4,Loc4}|_] = ST,
[{file,File4},{line,Line4}] = Loc4,
io:format(\"~s ~s ~w~n\", [M1,F1,A1]),
io:format(\"~s ~s ~w~n\", [M2,F2,A2]),
io:format(\"~s ~s ~w ~s ~w~n\", [M3,F3,A3,File,Line]),
io:format(\"~w ~s ~s ~w ~s ~w~n\", [length(ST),M4,F4,A4,File4,Line4]),
halt(0).";

/// The same four **canonical scalar lines** for `recv:main/0`'s **self** query.
///
/// `recv:main/0` (`probe_b(self())`) is run inside a *scheduled* process — the
/// [`pos_self`] fixture's `go/1`, whose entry MFA both runtimes report as
/// `initial_call` — and its answer is reported back through ordinary message passing
/// (a spawned export process's return value is unobservable). The parent receives the
/// already-ordered four-item list and prints the same four uniform scalar lines
/// [`ORACLE_PROBE`] does, so the two probes are compared byte for byte.
const ORACLE_SELF_PROBE: &str = "\
Self = self(),
_ = spawn(pos_self, go, [Self]),
R = receive {result, X} -> X end,
[{current_function,{M1,F1,A1}},
 {initial_call,{M2,F2,A2}},
 {current_location,{M3,F3,A3,Loc}},
 {current_stacktrace,ST}] = R,
[{file,File},{line,Line}] = Loc,
[{M4,F4,A4,Loc4}|_] = ST,
[{file,File4},{line,Line4}] = Loc4,
io:format(\"~s ~s ~w~n\", [M1,F1,A1]),
io:format(\"~s ~s ~w~n\", [M2,F2,A2]),
io:format(\"~s ~s ~w ~s ~w~n\", [M3,F3,A3,File,Line]),
io:format(\"~w ~s ~s ~w ~s ~w~n\", [length(ST),M4,F4,A4,File4,Line4]),
halt(0).";

/// Runs `probe` under the oracle with `code_dir` on the code path and returns its native
/// `io:format` stdout, one non-empty trimmed line at a time.
fn oracle_lines(oracle: &Path, code_dir: &Path, probe: &str) -> Vec<String> {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-pa")
        .arg(code_dir)
        .arg("-eval")
        .arg(probe)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("failed to spawn the oracle '{}': {error}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle exited with {:?}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------------------
// Term readers (the oracle's shapes, built by `beam_engine::machine::location`)
// ---------------------------------------------------------------------------------------

/// The atom name of `term`.
fn atom_name(runtime: &Runtime, term: Eterm) -> String {
    runtime
        .atom_name_of(term)
        .unwrap_or_else(|| panic!("{term:?} is not an atom"))
        .to_string()
}

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

/// The `(Module, Function, Arity)` of an `{M,F,A}` or `{M,F,A,Location}` tuple.
fn mfa(runtime: &Runtime, term: Eterm) -> (String, String, i64) {
    let arity = terms::tuple_arity(term).expect("an MFA tuple");
    assert!(
        arity == 3 || arity == 4,
        "an MFA is a 3- or 4-tuple, got arity {arity}: {term:?}"
    );
    (
        atom_name(runtime, terms::tuple_element(term, 0).expect("M")),
        atom_name(runtime, terms::tuple_element(term, 1).expect("F")),
        signed_value(terms::tuple_element(term, 2).expect("A")),
    )
}

/// The `(File, Line)` of a `[{file,File},{line,Line}]` location list.
fn file_and_line(runtime: &Runtime, location: Eterm) -> (String, i64) {
    let pairs = terms::list_elements(location).expect("a location list");
    assert_eq!(pairs.len(), 2, "a location is [{{file,..}},{{line,..}}]");
    let file_pair = pairs[0];
    assert_eq!(
        atom_name(runtime, terms::tuple_element(file_pair, 0).expect("the key")),
        "file"
    );
    let line_pair = pairs[1];
    assert_eq!(
        atom_name(runtime, terms::tuple_element(line_pair, 0).expect("the key")),
        "line"
    );
    (
        charlist(terms::tuple_element(file_pair, 1).expect("the file name")),
        signed_value(terms::tuple_element(line_pair, 1).expect("the line")),
    )
}

/// The value of an item in the `process_info/2` answer list.
fn item_value(runtime: &Runtime, list: Eterm, name: &str) -> Eterm {
    for entry in terms::list_elements(list).expect("the item list") {
        assert_eq!(
            terms::tuple_arity(entry).expect("an {Item,Value} tuple"),
            2,
            "each item is a 2-tuple"
        );
        let key = terms::tuple_element(entry, 0).expect("the item");
        if atom_name(runtime, key) == name {
            return terms::tuple_element(entry, 1).expect("the value");
        }
    }
    panic!("the process_info answer has no {name} item");
}

/// This runtime's four canonical lines for the probe `result` — byte-identical in shape to
/// what [`ORACLE_PROBE`] prints for the same parked process.
fn canonical(runtime: &Runtime, result: Eterm) -> Vec<String> {
    let entries = terms::list_elements(result).expect("the probe list");
    let names: Vec<String> = entries
        .iter()
        .map(|entry| atom_name(runtime, terms::tuple_element(*entry, 0).expect("the item")))
        .collect();
    assert_eq!(names, ITEMS, "the four items, in the requested order");

    let current = item_value(runtime, result, "current_function");
    let initial = item_value(runtime, result, "initial_call");
    let location = item_value(runtime, result, "current_location");
    let stacktrace = item_value(runtime, result, "current_stacktrace");

    let (m1, f1, a1) = mfa(runtime, current);
    let (m2, f2, a2) = mfa(runtime, initial);
    let (m3, f3, a3) = mfa(runtime, location);
    let (file, line) = file_and_line(
        runtime,
        terms::tuple_element(location, 3).expect("the location list"),
    );

    let frames = terms::list_elements(stacktrace).expect("a stacktrace list");
    assert!(!frames.is_empty(), "a parked process has at least one frame");
    let head = frames[0];
    let (m4, f4, a4) = mfa(runtime, head);
    let (file4, line4) = file_and_line(
        runtime,
        terms::tuple_element(head, 3).expect("the head's location list"),
    );

    vec![
        format!("{m1} {f1} {a1}"),
        format!("{m2} {f2} {a2}"),
        format!("{m3} {f3} {a3} {file} {line}"),
        format!("{} {m4} {f4} {a4} {file4} {line4}", frames.len()),
    ]
}

// ---------------------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------------------

/// A process parked in `recv:wait/0`'s `receive` reports the oracle's exact
/// execution-position items when probed from **another** process, and the value agrees with
/// the live system `erl` oracle line for line.
///
/// The parked process's `current_function`/`initial_call` are `{recv,wait,0}`, its
/// `current_location`/`current_stacktrace` head is
/// `{recv,wait,0,[{file,"recv.erl"},{line,12}]}` (the clause-head line 12 the fixture pins),
/// and the stacktrace is the single live frame.
#[test]
fn a_parked_process_reports_the_oracles_execution_position() {
    let Some((fixture, oracle, _erlc)) = build_fixture("parked") else {
        return;
    };

    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(&fixture.beam).expect("recv.beam loads");

    // Spawn the receiver and let it park: it blocks on an empty mailbox, so the pass ends
    // with the run queue empty and nothing left that could wake it (bounded, never hangs).
    let parked = rt
        .spawn_process("recv", "wait", vec![])
        .expect("spawn recv:wait/0");
    rt.run_scheduler().expect("the pass parks the receiver");
    let waiting = rt.processes().get(parked).expect("the receiver is alive");
    assert_eq!(
        waiting.state(),
        beam_process::ProcessState::Waiting,
        "the receiver parked in its receive"
    );
    assert!(rt.process_result(parked).is_none(), "it has not returned");

    // Probe the parked process from a *second* process (a process's own engine is taken out
    // of the runner state while it runs, so this is the only way it can be answered).
    let probe = rt
        .spawn_process("recv", "probe_b", vec![parked.to_eterm()])
        .expect("spawn recv:probe_b/1");
    rt.run_scheduler().expect("the probe runs to its return");
    let result = rt.process_result(probe).expect("the probe returned");

    // The live oracle answer for the same fixture, line for line.
    let expected = oracle_lines(&oracle, &fixture.dir, ORACLE_PROBE);
    assert_eq!(
        canonical(&rt, result),
        expected,
        "the runtime's execution-position items must match the live oracle"
    );
}

/// The **executing process's own** execution-position items are answered for real when the
/// query comes from **compiled code** — the in-machine BIF door
/// ([`crate::context::position`]) — and agree with the live system `erl` oracle line for
/// line.
///
/// `recv:main/0` is `probe_b(self())`: it asks `erlang:process_info/1,2` for
/// `current_function`/`initial_call`/`current_location`/`current_stacktrace` **of the
/// process running it**. The scheduler takes that process's engine out of the runner state
/// for the whole slice, so before the door existed the answer was the uncatchable typed gap
/// `unsupported: process_info: current_stacktrace` — `gen_server`'s `?STACKTRACE()`. Here
/// the query runs inside the *scheduled* fixture process `pos_self:go/1` (whose entry MFA is
/// the process's `initial_call`, in both runtimes) and its answer is reported back through
/// message passing, because a spawned export process's return value is unobservable.
///
/// The answer is `{recv,probe_b,1}` for `current_function`, `{pos_self,go,1}` for
/// `initial_call`, `{recv,probe_b,1,[{file,"recv.erl"},{line,18}]}` for `current_location`
/// and `[{recv,probe_b,1,[{file,"recv.erl"},{line,18}]},
/// {pos_self,go,1,[{file,"pos_self.erl"},{line,3}]}]` for `current_stacktrace` (the
/// `recv:main/0` frame is elided by its tail call into `probe_b/1`, exactly as the oracle
/// elides it).
#[test]
fn a_self_query_in_compiled_code_answers_the_oracles_execution_position() {
    let Some((fixture, oracle, _erlc)) = build_fixture("self") else {
        return;
    };

    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(&fixture.beam).expect("recv.beam loads");
    let pos_self = fixture.beam_of("pos_self");
    rt.load_module(&pos_self).expect("pos_self.beam loads");

    // The runner is the *scheduled* fixture process that performs the self query; the sink
    // is its parent, which receives the report and returns it so the test can read it back.
    let sink = rt
        .spawn_process("pos_self", "sink", vec![])
        .expect("spawn pos_self:sink/0");
    let runner = rt
        .spawn_process("pos_self", "go", vec![sink.to_eterm()])
        .expect("spawn pos_self:go/1");
    let result = run_until_returned(&mut rt, sink, runner);

    // The live oracle answer for the same fixture, line for line.
    let expected = oracle_lines(&oracle, &fixture.dir, ORACLE_SELF_PROBE);
    assert_eq!(
        canonical(&rt, result),
        expected,
        "the runtime's own execution-position items must match the live oracle"
    );
}

/// Drives the scheduler in bounded passes until `sink` returns, and answers its result.
///
/// The runner parks in no `receive` (it sends immediately), but the sink parks on an empty
/// mailbox first, so the runner's report wakes it through `beam_process`'s resume-on-send —
/// which may take a second pass. The bound makes a delivery that never arrives a loud
/// failure rather than a hang, and `runner`'s exit reason is reported when the runner died
/// before sending (the self query raising the typed capability gap is exactly what a
/// regression to the pre-door path looks like).
fn run_until_returned(
    rt: &mut Runtime,
    sink: beam_process::ProcessId,
    runner: beam_process::ProcessId,
) -> Eterm {
    for _ in 0..64 {
        rt.run_scheduler().expect("the scheduler pass");
        if let Some(result) = rt.process_result(sink) {
            return result;
        }
        if let Some(reason) = rt.process_exit(runner) {
            panic!(
                "the self-query runner exited before reporting: {reason:?} \
                 (the execution-position items must be answerable for the executing process)"
            );
        }
    }
    panic!(
        "the receiver never returned: its report was not delivered within 64 passes; \
         state = {:?}",
        rt.processes().get(sink).map(|process| process.state())
    );
}
