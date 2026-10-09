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

//! A **fun-bodied** scheduled process must be able to suspend in a `receive` and be
//! resumed by a message — the process-entry half of the A.9 boot chain.
//!
//! `erlang:spawn/1` (and the fun head of `spawn_link/1`/`spawn_opt/2`) makes a process whose
//! entry point is a **fun value**, which is exactly the shape `proc_lib:init_p/5` is spawned
//! as (`spawn_opt(fun () -> init_p(M, F, A, …) end)`). Applying that fun is not an ordinary
//! call: a `.beam` local (`make_fun3`) closure has to be entered at *its own* lambda label
//! in the module that defines it, **on the process's own machine**, for a `receive` inside
//! it — or inside a cross-module callee it calls — to suspend the process. Routing it
//! through `ExecutionContext::apply_fun` instead runs the closure on a fresh, non-blocking
//! machine, so the `receive` folds into the typed `blocked()` capability gap, which is
//! **uncatchable**: `proc_lib`'s own `try … catch … end` around `apply(M, F, A)` cannot
//! contain it and the starter dies. This suite is the regression gate for the fix.
//!
//! # Two independent proofs
//!
//! * [`a_fun_bodied_process_parks_and_answers_the_oracle`] drives the **executable**
//!   (`beam_rt run <beam> funrecv:run_cross/0`) and cross-checks the printed line against
//!   the live system `erl` oracle, for both fixture shapes;
//! * [`a_fun_entry_parks_before_any_message_exists_and_a_later_send_wakes_it`] drives the
//!   **`Runtime` API** so it can observe the process *between* the two slices: the child
//!   parks in its `receive` with an **empty mailbox** (so the answer below can only come
//!   from a real resume, never from a message that was already lying around), and a second
//!   process's send then wakes it and its result is the value the waiter returned.
//!
//! Both fixture modules are compiled **at test time** with the system `erlc` into one
//! scratch directory, which is the single `-pa` handed to this runtime and to the oracle
//! alike; a missing `erl`/`erlc` is a **loud** skip, never a silent pass. The scratch
//! directory never contains a `lists.beam`, so it is safe to hand to the oracle as a single
//! `-pa` (unlike this crate's `tests/fixtures/`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use beam_process::ProcessState;
use beam_rt::Runtime;
use beam_term::signed_value;

mod support;

/// The committed fixture sources (compiled at test time — never committed as `.beam`).
const FIXTURES: [&str; 2] = [
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/funrecv.erl"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/funrecvw.erl"),
];

/// The fixture module the process entries live in.
const MODULE: &str = "funrecv";

/// The two waiting shapes, and the line both runtimes must print for them.
///
/// `run_cross/0`'s fun body calls `funrecvw:wait_ready/1` — a **different module** — whose
/// `receive` blocks one `call_ext` away from the lambda, exactly as
/// `proc_lib:sync_start/2` sits relative to the fun `proc_lib:init_p/5` is spawned as.
/// `run_inline/0` blocks **inline in the fun body**, so the closure's own label entry must
/// run on the process's machine too.
const AGREE: [(&str, &str, &str); 2] = [
    ("a blocking callee in a foreign module", "run_cross", "42"),
    ("a receive inline in the fun body", "run_inline", "42"),
];

// ---------------------------------------------------------------------------------------
// Toolchain discovery + the loud skip
// ---------------------------------------------------------------------------------------

/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// Prints the loud skip banner — a missing compiler is never a silent pass.
fn skip_loudly(why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt fun-entry receive SKIPPED ===============");
    eprintln!("{why}: the fun-bodied-process suspension cannot be reproduced.");
    eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
    eprintln!("================================================================");
    eprintln!();
}

// ---------------------------------------------------------------------------------------
// The compiled fixtures
// ---------------------------------------------------------------------------------------

/// The compiled fixtures in one scratch directory (removed on drop).
struct Fixtures {
    /// The scratch directory; a safe single `-pa` for both runtimes (no `lists.beam`).
    dir: PathBuf,
    /// The compiled `funrecv.beam`.
    beam: PathBuf,
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Compiles both committed fixture modules into one scratch directory, or `None` with a
/// loud skip when the toolchain cannot do it.
fn build_fixtures(erlc: &Path) -> Option<Fixtures> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("beam_rt_funrecv_{}_{n}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        skip_loudly(&format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    for source_path in FIXTURES {
        let name = Path::new(source_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {source_path}"));
        let bytes = match std::fs::read(source_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                skip_loudly(&format!("cannot read {source_path}: {error}"));
                return None;
            }
        };
        if let Err(error) = std::fs::write(dir.join(name), &bytes) {
            skip_loudly(&format!("cannot write {name}: {error}"));
            return None;
        }
    }
    let names: Vec<&str> = FIXTURES
        .iter()
        .filter_map(|source| Path::new(source).file_name().and_then(|name| name.to_str()))
        .collect();
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .args(&names)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            skip_loudly(&format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        skip_loudly(&format!(
            "`erlc` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    let beam = dir.join("funrecv.beam");
    if !beam.is_file() {
        skip_loudly(&format!("`erlc` produced no {}", beam.display()));
        return None;
    }
    Some(Fixtures { dir, beam })
}

/// Compiles the fixtures or reports the loud skip; the two callers below both need them.
fn fixtures_or_skip() -> Option<Fixtures> {
    if support::require_oracle("fun-entry receive").is_none() {
        return None;
    }
    let Some(erlc) = resolve_erlc() else {
        skip_loudly("no `erlc` available (and none beside $ERTS_ORACLE)");
        return None;
    };
    build_fixtures(&erlc)
}

// ---------------------------------------------------------------------------------------
// The bounded probe
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed
/// and reaped at [`support::PROBE_TIMEOUT`] instead of hanging the suite.
fn run_bounded(program: &Path, args: &[&str]) -> Run {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot spawn `{}`: {error}", program.display()));
    let deadline = Instant::now() + support::PROBE_TIMEOUT;
    let output: Output = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child
                .wait_with_output()
                .unwrap_or_else(|error| panic!("cannot collect the probe's output: {error}")),
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "`{}` outlived the {}s probe bound",
                    program.display(),
                    support::PROBE_TIMEOUT.as_secs()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot poll `{}`: {error}", program.display()),
        }
    };
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// The live oracle's line for `MODULE:function/0`.
fn oracle_line(oracle: &Path, dir: &Path, function: &str) -> String {
    let expr = format!("io:format(\"~p~n\", [{MODULE}:{function}()]), halt(0).");
    support::oracle_eval(oracle, Some(dir), &expr)
        .unwrap_or_else(|| panic!("the live oracle could not answer {function}/0"))
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// A fun-bodied process whose callee **blocks** answers exactly what the live oracle
/// answers, through the real executable.
///
/// The child announces itself (`{ready, Pid}`) *before* it blocks, so the parent's wake-up
/// can only be consumed by a child that really reached its `receive` — the printed `42` is
/// therefore proof of a suspend **and** a resume, not of a message that was already waiting.
/// Both shapes (a foreign-module callee and an inline `receive`) must agree, and neither may
/// report the `unsupported` capability gap the unfixed code folded the suspension into.
#[test]
fn a_fun_bodied_process_parks_and_answers_the_oracle() {
    let Some(oracle) = support::require_oracle("fun-entry receive") else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        skip_loudly("no `erlc` available (and none beside $ERTS_ORACLE)");
        return;
    };
    let Some(fixtures) = build_fixtures(&erlc) else {
        return;
    };
    let beam = fixtures
        .beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", fixtures.beam.display()));
    let dir = fixtures.dir.to_str().expect("the scratch dir is not UTF-8");

    for (label, function, expected) in AGREE {
        let spec = format!("{MODULE}:{function}/0");
        let ours = run_bounded(
            Path::new(env!("CARGO_BIN_EXE_beam_rt")),
            &["-pa", dir, "run", beam, &spec],
        );
        assert_eq!(
            ours.code, 0,
            "{label}: `beam_rt run {beam} {spec}` exited {}: {}",
            ours.code, ours.err
        );
        assert!(
            !ours.err.contains("unsupported"),
            "{label}: a blocking receive in a fun-bodied process is not a capability gap: {}",
            ours.err
        );
        assert_eq!(ours.out, expected, "{label}: beam_rt's answer");
        assert_eq!(
            oracle_line(&oracle, &fixtures.dir, function),
            expected,
            "{label}: the recorded answer must be the live oracle's"
        );
    }
}

/// The API-level proof, which the executable cannot give: the fun entry **parks** with an
/// empty mailbox, and a **later** send resumes it.
///
/// `funrecv:mkchild/0` returns a genuine `.beam` **local** (`make_fun3`) fun — not the
/// `-eval` closure kind, which has no local label and keeps the synchronous route — so this
/// isolates exactly the new blocking path. Slice 1 must leave the child `Waiting` in
/// `funrecvw:wait_go/0`'s `receive` with **nothing** in its mailbox (there is no message
/// yet, so the `42` below can only come from the resume); slice 2's sender delivers
/// `{go, 42}`, which wakes it, and the waiter's own value becomes the process's result.
#[test]
fn a_fun_entry_parks_before_any_message_exists_and_a_later_send_wakes_it() {
    let Some(fixtures) = fixtures_or_skip() else {
        return;
    };
    let mut rt = Runtime::with_defaults().expect("the default runtime");

    // `mkchild/0` is run as an ordinary export (the vertical-slice entry), so the fun term
    // it returns is a real compiled-module closure living on the runtime's pointer-stable
    // heap — and `funrecv` is now cached in the code server, which the entry resolver needs
    // to find the lambda's label.
    let fun = rt
        .run_function(&fixtures.beam, "mkchild", 0, &[])
        .expect("funrecv:mkchild/0 returns the local fun");
    // The lambda's body calls `funrecvw:wait_go/0`, a *second* module: it is resolved on
    // demand from the code path, so the scratch directory must be on it.
    rt.add_code_path(&fixtures.dir);

    let child = rt
        .spawn_fun_process(fun, vec![])
        .expect("spawn the fun-bodied process");
    rt.run_scheduler().expect("the pass parks the child");

    let parked = rt.processes().get(child).expect("the child is alive");
    assert_eq!(
        parked.state(),
        ProcessState::Waiting,
        "the fun entry parked in its receive instead of dying with the typed `blocked` gap"
    );
    assert!(
        parked.mailbox().is_empty(),
        "nothing has been sent yet, so the value below can only come from a real resume"
    );
    assert!(
        rt.process_result(child).is_none(),
        "a parked process has not returned"
    );

    // A second process sends the wake-up; the child resumes in the very machine it
    // suspended in and its own `receive` binds the value it returns.
    let waker = rt
        .spawn_process(MODULE, "wake", vec![child.to_eterm()])
        .expect("spawn funrecv:wake/1");
    rt.run_scheduler()
        .expect("the rest of the pass resumes the child");

    assert_eq!(
        signed_value(rt.process_result(child).expect("the child returned the message")),
        42,
        "the resumed receive took the message the waker sent"
    );
    assert!(
        rt.process_exit(child).is_none(),
        "a normally-returning process exits with `normal`"
    );
    assert!(
        rt.processes().get(child).is_none(),
        "the child terminated and left the process table"
    );
    assert!(
        rt.process_result(waker).is_some(),
        "the waker ran to its own return"
    );
}
