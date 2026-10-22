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

//! Contract A.9: the **scheduled** in-process compile of a real `lib/compiler/src/*.erl`
//! module, into a **pre-populated (dirty) output directory**, with the debt-gated in-machine
//! route exercised.
//!
//! # What this suite pins
//!
//! A reported crash on exactly this shape — `beam_rt … run <probe>.beam probe:go/1
//! cerl_clauses` panicking with `class: invalid boxed header subtag 0x31` out of
//! `beam_term/src/ops.rs`, reached through `beam_term::compare::cmp` ←
//! `beam_engine::families::maps::lookup` ← `get_map_elements` — could not be reproduced
//! (≈415 invocations across launch, environment, output-directory, module, tier and
//! concurrency shapes; 0 panics). The class it was reported in is the *dangling boxed
//! argument* one, and the change that landed immediately before the report is the one that
//! makes a collection **reachable inside a fun-applying stdlib fast path**
//! ([`RuntimeContext::resolve_external_call`]'s collection-debt gate,
//! `src/context/execution.rs`): while the calling process carries the collector's debt, a
//! `lists`/`maps` fold/map key is entered **in the caller's machine** instead of through the
//! host BIF's nested, unarmed machine — and that in-machine entry is what lets the debt door
//! collapse the retired nursery chain the growth seam leaves behind.
//!
//! So this suite keeps the reported reproducer executable, and asserts three things about it:
//!
//! 1. the compile **completes** and answers the compiler's own `{ok, cerl_clauses}`;
//! 2. it answers that **again** into an output directory that already holds the previous
//!    run's artifact (the reported "dirty outdir" ingredient) — the state every iteration
//!    after the first of a reproducer loop is in;
//! 3. the run **really took the debt-gated route** (the `seam debt-gate` debug trace is
//!    present), so the pin cannot go vacuous if the route is ever changed back — it is the
//!    in-machine entry, not the host BIF, that this suite exists to keep honest.
//!
//! Assertion 3 is why the dirty run carries `BEAM_RT_BOOT_TRACE=1`: the trace is the only
//! observable of the routing decision, and a compile of `cerl_clauses.erl` takes it **15804**
//! times (measured with the `debug` profile binary on this host), with 205 copy-out nursery
//! growths in the same run and the chain collapsed back to one space at the end.
//!
//! That trace is also why this suite drives its own child runner ([`run_with_env`]) instead of
//! the shared `support::run_bounded_with_env`: it emits ~1.6 MB, and the shared runner drains
//! the pipes only after the child exits, so a probe that writes past one pipe buffer (~64 KiB)
//! deadlocks against a full pipe — which is a property of that helper's documented contract, not
//! of the runtime.
//!
//! # Why the scheduled route and not `-eval`
//!
//! [`compile_file`](crate) runs its real-module byte-identity test on the `-eval` route, which
//! has **no process table** — so `calling_process_has_collection_debt` answers `false` there
//! by construction and the gate can never fire. The scheduled `run` route is the only one with
//! a process nursery, a retired-nursery chain and a debt bit, i.e. the only route on which the
//! reported shape exists at all.
//!
//! # Missing inputs are a loud skip
//!
//! The probe needs `erlc` (to build its own driver module), the `lib/compiler/src` source tree
//! and the committed bootstrap compiler's `ebin` directories. Any of them missing — an
//! installed-only or relocated checkout, no toolchain — is a **loud** skip, never a silent
//! pass.
//!
//! # Cost
//!
//! Two probes of a 13.7 kB real module plus one `erlc` run: ≈5–7 s with a release `beam_rt`,
//! ≈85 s when the harness runs the `debug`-profile binary (`cargo test`'s default), where
//! `+opt-level=1` makes the in-process compile ~15× slower. The numbers this suite quotes are
//! *shape* facts (the route was taken, the compile completed), never a throughput claim.
#[allow(dead_code)]
mod support;

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The module the probe compiles: the real `lib/compiler/src/cerl_clauses.erl` the report named.
const MODULE: &str = "cerl_clauses";

/// A private temporary directory, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    /// Creates `$TMPDIR/beam_rt_compile_gate_<pid>_<n>_<label>`, unique per test and per run.
    fn new(label: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "beam_rt_compile_gate_{}_{unique}_{label}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("a private temporary directory");
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The repository root: `$ERL_TOP` when set, else three levels up from this crate's manifest
/// directory (`<root>/erts_rust/crates/beam_rt`) — the discovery `tests/compile_file.rs` and
/// the in-crate suites use.
fn source_root() -> PathBuf {
    match std::env::var_os("ERL_TOP") {
        Some(value) => PathBuf::from(value),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
    }
}

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// The wall-clock bound on one probe. Deliberately **not** the shared
/// [`support::PROBE_TIMEOUT`]: a real-module compile takes ≈2.5 s (release) and this suite runs
/// two of them, so the bound only has to be generous, never tight.
const GATE_TIMEOUT: Duration = Duration::from_secs(240);

/// Runs `program args` with `/dev/null` stdin, killing the child at [`GATE_TIMEOUT`].
///
/// Both pipes are drained by **their own threads** while the child runs, so a probe that writes
/// more than one pipe buffer cannot deadlock on a full pipe. That is load-bearing here: the
/// dirty run carries `BEAM_RT_BOOT_TRACE=1`, whose `seam debt-gate` trace is ~1.6 MB — far past
/// the ~64 KiB the shared [`support::run_bounded_with_env`] documents as a probe's limit, and a
/// runner that only reads the pipes after the child exits would block the child forever.
fn run_with_env(program: &Path, args: &[&str], env: &[(&str, Option<&str>)]) -> Run {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in env {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    let mut child = command.spawn().unwrap_or_else(|error| {
        panic!(
            "`{}` {args:?} (env {env:?}) could not be spawned: {error}",
            program.display()
        )
    });
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let out_reader = drain(child.stdout.take().map(|p| Box::new(p) as Box<_>));
    let err_reader = drain(child.stderr.take().map(|p| Box::new(p) as Box<_>));
    let deadline = Instant::now() + GATE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "`{}` {args:?} outlived the {}s bound",
                    program.display(),
                    GATE_TIMEOUT.as_secs()
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => panic!(
                "cannot poll `{}` {args:?}: {error}",
                program.display()
            ),
        }
    };
    let out = String::from_utf8_lossy(&out_reader.join().expect("the stdout drain thread"))
        .trim()
        .to_string();
    let err = String::from_utf8_lossy(&err_reader.join().expect("the stderr drain thread"))
        .trim()
        .to_string();
    Run {
        code: status.code().unwrap_or(-1),
        out,
        err,
    }
}

/// The **Erlang string literal** for `path` (the temporary names are ASCII, which Rust's
/// `Debug` and Erlang's string syntax quote identically).
fn literal(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("an ASCII temporary path"))
}

/// The driver module's source: it compiles `lib/<App>/src/<Mod>.erl` with
/// `[deterministic, {outdir, <out>}]` — the reported reproducer's option list — and prints the
/// compiler's own answer. The paths are baked in, so the `run` action needs no `-extra` list.
fn probe_source(root: &Path, out: &Path) -> String {
    format!(
        "-module(probe).\n\
         -export([go/1]).\n\
         go(Mod) ->\n\
         \x20   Src = \"{}\" ++ \"/lib/compiler/src/\" ++ atom_to_list(Mod) ++ \".erl\",\n\
         \x20   R = compile:file(Src, [deterministic, {{outdir, {}}}]),\n\
         \x20   io:format(\"RESULT ~p~n\", [R]),\n\
         \x20   ok.\n",
        root.display(),
        literal(out)
    )
}

/// Builds the probe module into `dir` with the system `erlc`, returning its path, or `None`
/// after a **loud** skip when the toolchain cannot be run (the pattern
/// `tests/a9_native_floor.rs` uses).
fn build_probe(banner: &str, dir: &Path, source: &str) -> Option<PathBuf> {
    let src = dir.join("probe.erl");
    fs::write(&src, source).expect("write the probe source");
    match Command::new("erlc").arg("-o").arg(dir).arg(&src).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            support::skip_loudly(
                banner,
                &format!(
                    "`erlc` exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            );
            return None;
        }
        Err(error) => {
            support::skip_loudly(banner, &format!("cannot run `erlc`: {error}"));
            return None;
        }
    }
    let beam = dir.join("probe.beam");
    beam.is_file().then_some(beam)
}

/// **The reported reproducer, kept executable.** A real `lib/compiler/src/cerl_clauses.erl` is
/// compiled by `compile:file/2` from a **scheduled** process — `beam_rt run <probe>.beam
/// probe:go/1 compiler cerl_clauses`, against the committed bootstrap compiler — twice into the
/// **same** output directory, so the second compile sees the stale artifact the report named.
///
/// Asserts, for both compiles: exit `0`, the compiler's `RESULT {ok,cerl_clauses}`, and no
/// panic marker on stderr (`invalid boxed header` / `panicked`) — the reported failure class.
/// The second compile additionally carries `BEAM_RT_BOOT_TRACE=1` and must record at least one
/// `seam debt-gate` line, so the suite fails if the debt-gated in-machine route ever stops being
/// the one this shape takes (a vacuous "no crash" would otherwise be indistinguishable from a
/// genuine pass).
///
/// A missing `erlc`, source tree or bootstrap `ebin` set is a loud skip.
#[test]
fn the_reported_dirty_outdir_scheduled_compile_is_clean_and_takes_the_debt_gate() {
    const BANNER: &str = "compile_scheduled_gate";

    let root = source_root();
    let source = root.join("lib/compiler/src").join(format!("{MODULE}.erl"));
    if !source.is_file() {
        support::skip_loudly(
            BANNER,
            &format!("no real compiler source at {}", source.display()),
        );
        return;
    }
    // The committed bootstrap OTP-30 compiler — the code path the reported runs used, and the
    // one whose `-pa` list is in-tree (so the probe does not depend on an installation).
    let mut pa: Vec<PathBuf> = Vec::new();
    for app in ["kernel", "stdlib", "compiler"] {
        let dir = root.join("bootstrap/lib").join(app).join("ebin");
        if !dir.is_dir() {
            support::skip_loudly(
                BANNER,
                &format!("no committed bootstrap code path at {}", dir.display()),
            );
            return;
        }
        pa.push(dir);
    }

    let dir = TempDir::new("cerl_clauses");
    let outdir = dir.0.join("out");
    // `compile` does not create `{outdir, D}`; the first run fills it, which is what makes the
    // second run's output directory **dirty**.
    fs::create_dir_all(&outdir).expect("the output directory");
    let probe = probe_source(&root, &outdir);
    let Some(beam) = build_probe(BANNER, &dir.0, &probe) else {
        return;
    };

    // The `-pa` list is **absolute** (a relative one silently selects the system compiler) and
    // the order is the documented bootstrap order.
    let mut args: Vec<String> = Vec::new();
    for dir in &pa {
        args.push("-pa".to_string());
        args.push(dir.display().to_string());
    }
    args.push("-noshell".to_string());
    args.push("run".to_string());
    args.push(beam.display().to_string());
    args.push("probe:go/1".to_string());
    args.push(MODULE.to_string());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    // ---- the first compile: an empty output directory ----
    let first = run_with_env(Path::new(BINARY), &args, &[]);
    assert_eq!(
        first.code, 0,
        "the scheduled in-process compile must complete: exit {} — stdout {:?} stderr {:?}",
        first.code, first.out, first.err
    );
    assert!(
        first.out.contains(&format!("RESULT {{ok,{MODULE}}}")),
        "the compiler must answer RESULT {{ok,{MODULE}}}; stdout {:?} stderr {:?}",
        first.out,
        first.err
    );
    let artifact = outdir.join(format!("{MODULE}.beam"));
    assert!(
        artifact.is_file(),
        "the compile wrote no artifact at {}",
        artifact.display()
    );
    let written = fs::read(&artifact).expect("the artifact");
    assert!(
        !written.is_empty(),
        "the artifact at {} is empty",
        artifact.display()
    );

    // ---- the second compile: the same directory, now holding the stale artifact ----
    let dirty = run_with_env(
        Path::new(BINARY),
        &args,
        &[("BEAM_RT_BOOT_TRACE", Some("1"))],
    );
    assert!(
        !dirty.err.contains("invalid boxed header") && !dirty.err.contains("panicked"),
        "a compile into a dirty output directory must not panic; stderr {:?}",
        dirty.err
    );
    assert_eq!(
        dirty.code, 0,
        "the compile into a dirty output directory must complete: exit {} — stdout {:?} \
         stderr (tail) {:?}",
        dirty.code,
        dirty.out,
        &dirty.err[dirty.err.len().saturating_sub(400)..]
    );
    assert!(
        dirty.out.contains(&format!("RESULT {{ok,{MODULE}}}")),
        "the dirty-directory compile must answer RESULT {{ok,{MODULE}}}; stdout {:?}",
        dirty.out
    );
    // The routing evidence: the debt-gated in-machine entry
    // (`src/context/execution.rs::resolve_external_call`) really ran, so this pin covers the
    // collection-reachable route rather than the host BIF's nested machine.
    let gate_entries = dirty.err.matches("seam debt-gate").count();
    assert!(
        gate_entries > 0,
        "the compile must take the debt-gated in-machine route at least once, saw {gate_entries} \
         `seam debt-gate` trace lines (is the process really carrying collection debt?)"
    );
    eprintln!(
        "compile_scheduled_gate: {gate_entries} debt-gated in-machine entries, artifact {} bytes",
        written.len()
    );
}
