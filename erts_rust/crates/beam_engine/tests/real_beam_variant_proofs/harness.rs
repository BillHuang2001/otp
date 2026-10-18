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

#![allow(dead_code)]
//! The **shared real-`.beam` variant-proof harness** — one implementation of the
//! four-step proof every row in this suite performs.
//!
//! # The proof
//!
//! 1. Drive a **real compiled module** (a committed `erlc` fixture, or a system
//!    stdlib module) through the engine's public entry
//!    [`ModuleHost::call`](crate::support::ModuleHost::call). No hand-built
//!    [`Module`](beam_loader::Module): the instruction under test comes from the
//!    OTP compiler, never from a test.
//! 2. Run it with [`beam_engine::exec_census`] enabled and assert the variant's
//!    **executed count is greater than zero** — the assertion that this variant
//!    really ran, not merely that it exists in a `.beam`. (Before the engine
//!    implemented a variant, the census count was structurally zero: the run
//!    either died on `unsupported opcode …` or deopted around it.)
//! 3. Compare the run's **observable result** with the real ERTS oracle: either
//!    [`Proof::value`] rendered by [`render`] against [`oracle_w`] (a live `erl`
//!    invocation at test time), or a committed oracle-derived value asserted with
//!    the `support::assert_*` helpers under a doc comment recording the exact `erl`
//!    command.
//! 4. Report honestly: a variant the installed compiler never emits cannot be
//!    proven here, and is listed in the crate-level `real_beam_variant_proofs.rs`
//!    module docs with the reason.
//!
//! # Why the census is the right instrument
//!
//! [`exec_census`](beam_engine::exec_census) is recorded at the single
//! per-instruction funnel inside `Machine::step_instruction_inner`, which the
//! interpreter's dispatch loop and the native tiers' deopt helper both pass
//! through. Counting happens **before** the body runs, so a raising instruction
//! (e.g. `Badmatch`, `Badrecord`, `CaseEnd`) is counted as executed and the raise
//! is still observable as the typed [`BeamError`] the run returns. A raise folded
//! into a live in-machine `try`/`catch` comes back `Ok(..)` — an executed,
//! non-error count — which is exactly how the exception-family rows read it.
//!
//! [`ModuleHost::call`](crate::support::ModuleHost::call) runs
//! `Machine::run_export`, i.e. the **interpreter** path, so every instruction of
//! the run is observable. (A natively-inlined baseline-JIT instruction bypasses
//! the funnel; that is why these proofs pin the interpreter — see the
//! `exec_census` module docs.)
//!
//! # Oracle discipline
//!
//! Nothing here is guessed. [`oracle_w`] shells out to the real `erl` on `PATH`
//! (the differential oracle, OTP 29.1.1 / `erts-17.1`) and returns the `~w`
//! rendering of an expression; [`render`] renders the engine's result in the same
//! notation, so a row's assertion is a literal string equality against the system
//! runtime's own answer. A row whose result is not renderable (a float, a
//! bignum, a fun) asserts a committed oracle-derived value instead, with the
//! `erl` command in the doc comment.
//!
//! Every test **skips loudly** when the oracle/OTP install is absent
//! ([`skip_if_unavailable`]) — a green run without `erl` proves nothing about
//! parity, so the skip is printed.

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_common::BeamError;
use beam_engine::exec_census::{self, Snapshot};
use beam_engine::terms;
use beam_engine::ExecutionContext;
use beam_term::{atom_val, is_atom, is_small, signed_value, Eterm};

use crate::support::{app_ebin, skip_reason, stdlib_ebin, ModuleHost};

/// The committed fixture directory (`tests/fixtures/`).
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Skips the calling test **loudly** when the system OTP install (the oracle and
/// the stdlib the fixtures are built against) is unavailable.
///
/// Returns `true` when the caller should return early. Mirrors
/// `tests/stdlib_lists.rs`, so the whole suite behaves like its siblings.
pub fn skip_if_unavailable() -> bool {
    match skip_reason() {
        Some(reason) => {
            eprintln!("{reason}");
            true
        }
        None => false,
    }
}

/// A [`ModuleHost`] over the system stdlib `ebin` (plus `kernel`'s, so a fixture
/// may call into `kernel`), with `fixtures` registered.
///
/// Each entry is `(module name, fixture file name)`; the file is resolved in
/// [`fixtures_dir`]. Pass `&[]` to run a **system** module (e.g. `lists`) by name
/// — the strongest form of the proof, since the module is the oracle's own
/// compiled output.
pub fn host(fixtures: &[(&str, &str)]) -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    let mut host = ModuleHost::new(ebin);
    if let Some(kernel) = app_ebin("kernel") {
        host.add_search_dir(kernel);
    }
    for (module, file) in fixtures {
        host.register(module, fixtures_dir().join(file));
    }
    host
}

/// One real-`.beam` run: its **observable result** and the execution census it
/// produced.
pub struct Proof {
    /// The run's result: `Ok(term)` for a returned value, `Err(e)` for the raise /
    /// capability gap the run surfaced.
    pub value: Result<Eterm, BeamError>,
    /// The thread's census over exactly this run.
    pub census: Snapshot,
}

impl Proof {
    /// How many times `variant` executed during this run (0 when it never ran).
    ///
    /// `variant` is the [`Instr`](beam_loader::Instr) variant name as
    /// [`exec_census::variant_name`] renders it: `GetTupleElement`, `FuncInfo`,
    /// `BsSetPosition`, … and `Test(IsEqExact)` for a `test` (keyed by its kind).
    pub fn executed(&self, variant: &str) -> u64 {
        self.census.executed.get(variant).copied().unwrap_or(0)
    }

    /// Asserts that **every** named variant executed at least once in this run.
    ///
    /// This is the core assertion of the suite: it fails when the real compiler
    /// output never reaches the variant, and it prints the whole observed census
    /// (and the typed error, if the run raised) so a failure is diagnosable
    /// without a re-run.
    #[track_caller]
    pub fn require(&self, variants: &[&str]) -> &Self {
        for variant in variants {
            assert!(
                self.executed(variant) > 0,
                "variant {variant:?} did not execute on this real `.beam` run\n{}",
                self.report()
            );
        }
        self
    }

    /// Asserts that this run executed `variant` **exactly** `times` times (used
    /// where the count itself is the observable, e.g. one `func_info` per entered
    /// clause).
    #[track_caller]
    pub fn require_exactly(&self, variant: &str, times: u64) -> &Self {
        assert_eq!(
            self.executed(variant),
            times,
            "variant {variant:?} executed {} times, expected {times}\n{}",
            self.executed(variant),
            self.report()
        );
        self
    }

    /// The run's returned term; panics with the typed error when it raised.
    #[track_caller]
    pub fn value(&self) -> Eterm {
        match &self.value {
            Ok(term) => *term,
            Err(error) => panic!("the run raised {error:?}\n{}", self.report()),
        }
    }

    /// The run's raised error; panics when the run returned normally.
    ///
    /// Borrowed rather than cloned because [`BeamError`] is not `Clone` (it can
    /// carry a live raise reason).
    #[track_caller]
    pub fn error(&self) -> &BeamError {
        match &self.value {
            Err(error) => error,
            Ok(term) => panic!("expected a raise, the run returned {term:?}\n{}", self.report()),
        }
    }

    /// The observed census as a sorted `variant=count` table (the failure text of
    /// [`require`](Self::require)/[`value`](Self::value), reused by a row that
    /// wants to record what a run did).
    pub fn report(&self) -> String {
        let mut out = format!(
            "census: total_executed={} total_errors={}\n",
            self.census.total_executed, self.census.total_errors
        );
        for (variant, count) in &self.census.executed {
            out.push_str(&format!("  {variant} = {count}\n"));
        }
        if let Err(error) = &self.value {
            out.push_str(&format!("  run returned: {error:?}\n"));
        }
        out
    }
}

/// Runs `body` with the execution census enabled, returning its value and the
/// census recorded over exactly that run.
///
/// The census is thread-local: it is reset (so the counts belong to this run
/// alone), enabled for `body`, snapshotted, and disabled again — so a panic in a
/// `require` cannot leak counting into the next test on the same thread.
///
/// This is the raw form of the proof's steps 2 and 3, for a row that must drive the
/// engine through something other than
/// [`ModuleHost::call`](crate::support::ModuleHost::call) (e.g. a context that
/// carries a mailbox, as the receive family needs); [`run`] and [`apply`] are the
/// [`ModuleHost`] wrappers.
pub fn measure<T>(body: impl FnOnce() -> T) -> (T, Snapshot) {
    exec_census::reset();
    exec_census::enable();
    let value = body();
    let census = exec_census::snapshot();
    exec_census::disable();
    (value, census)
}

/// Runs `module:function/args.len()` on a **real** compiled module with the
/// execution census enabled, returning the result and the census.
pub fn run(host: &mut ModuleHost, module: &str, function: &str, args: &[Eterm]) -> Proof {
    let (value, census) = measure(|| host.call(module, function, args));
    Proof { value, census }
}

/// Runs an **anonymous fun** on its defining module (`call_fun`/`call_fun2` reach
/// the engine's closure path through the host's `apply_fun` seam), with the same
/// census discipline as [`run`].
///
/// `fun` is a closure term produced by an earlier [`run`] on the same host (see
/// `tests/funs.rs`); `args` are the call arguments, and the closure's free
/// variables are appended by the host exactly as the C `call_fun` does.
pub fn apply(host: &mut ModuleHost, fun: Eterm, args: &[Eterm]) -> Proof {
    let (value, census) = measure(|| host.apply_fun(fun, args));
    Proof { value, census }
}

// ---------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------

/// The real system `erl`'s `~w` rendering of the Erlang expression `expr`,
/// evaluated with `tests/fixtures/` on the code path.
///
/// The fixture directory is **appended** (`-pz`), never prepended (`-pa`): several
/// committed fixtures are stand-ins for stdlib modules (`lists.beam`, `floats.beam`,
/// …), and prepending them makes the emulator die at boot with an empty
/// stdout+stderr (see `tests/CONTEXT.md`).
///
/// Spawns one `erl -noshell` per call (≈1 s), which is the price of an assertion
/// that is *checked against the live oracle* rather than baked in. A missing
/// `erl` is a harness bug (the caller must have gone through
/// [`skip_if_unavailable`]); an `erl` that **ran and exited non-zero** panics with
/// its captured output rather than being folded into "no oracle" (the discipline
/// `tests/CONTEXT.md` prescribes).
pub fn oracle_w(expr: &str) -> String {
    let code = format!("io:format(\"~w~n\", [{expr}]), halt(0).");
    let output = Command::new("erl")
        .arg("-noshell")
        .arg("-pz")
        .arg(fixtures_dir())
        .arg("-eval")
        .arg(&code)
        .output()
        .expect("spawn `erl` (the caller checked `skip_if_unavailable`)");
    assert!(
        output.status.success(),
        "`erl -eval {code}` exited with {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim_end().to_string()
}

/// The real system `erl`'s `~w` rendering for the fixture call
/// `Module:Function(Args)`, written in Erlang source syntax.
///
/// Shorthand for the common row shape: `oracle_call("sp_exc", "raise_badmatch", "[]")`.
pub fn oracle_call(module: &str, function: &str, args: &str) -> String {
    oracle_w(&format!("{module}:{function}({args})"))
}

/// Renders a returned term in the oracle's `~w` notation so a row can compare it
/// with [`oracle_w`] by string equality.
///
/// Supported (the whole value space of the rows in this suite): small integers,
/// atoms, cons cells / proper and improper lists, tuples, and byte-aligned
/// binaries. Anything else — a float, a bignum, a fun, a map — renders as
/// `<unrenderable: …>` **loudly**, so a row that needs one of those must assert a
/// committed oracle value instead of silently comparing two placeholders.
pub fn render(host: &ModuleHost, term: Eterm) -> String {
    match render_inner(host, term, 0) {
        Ok(text) => text,
        Err(reason) => format!("<unrenderable: {reason}>"),
    }
}

fn render_inner(host: &ModuleHost, term: Eterm, depth: usize) -> Result<String, String> {
    if depth > 64 {
        return Err("nesting deeper than 64".into());
    }
    if beam_term::is_nil(term) {
        return Ok("[]".into());
    }
    if is_small(term) {
        return Ok(signed_value(term).to_string());
    }
    if is_atom(term) {
        let name = host
            .atom_table()
            .name(atom_val(term))
            .ok_or_else(|| "unknown atom".to_string())?;
        return Ok(render_atom_name(&name));
    }
    // `[]` is the atom `nil` on the wire but a distinct immediate here; the atom
    // branch above covers a named atom, so test the cons/list shape next.
    if let Ok(head) = terms::cons_head(term) {
        let tail = terms::cons_tail(term).map_err(|e| format!("{e:?}"))?;
        let mut out = String::from("[");
        out.push_str(&render_inner(host, head, depth + 1)?);
        let mut rest = tail;
        loop {
            if beam_term::is_nil(rest) {
                out.push(']');
                return Ok(out);
            }
            match terms::cons_head(rest) {
                Ok(next) => {
                    out.push(',');
                    out.push_str(&render_inner(host, next, depth + 1)?);
                    rest = terms::cons_tail(rest).map_err(|e| format!("{e:?}"))?;
                }
                Err(_) => {
                    // An improper tail renders as `[a|b]`, exactly as `~w` does.
                    out.push('|');
                    out.push_str(&render_inner(host, rest, depth + 1)?);
                    out.push(']');
                    return Ok(out);
                }
            }
        }
    }
    if beam_term::is_nil(term) {
        return Ok("[]".into());
    }
    if let Ok(arity) = terms::tuple_arity(term) {
        let mut out = String::from("{");
        for index in 0..arity {
            if index > 0 {
                out.push(',');
            }
            let element = terms::tuple_element(term, index).map_err(|e| format!("{e:?}"))?;
            out.push_str(&render_inner(host, element, depth + 1)?);
        }
        out.push('}');
        return Ok(out);
    }
    if let Ok(bytes) = terms::binary_bytes(term) {
        let mut out = String::from("<<");
        for (index, byte) in bytes.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&byte.to_string());
        }
        out.push_str(">>");
        return Ok(out);
    }
    Err(format!("{term:?}"))
}

/// An atom name in `~w` notation: bare when it lexes as an unquoted atom, quoted
/// (with the embedded quote escaped) otherwise.
fn render_atom_name(name: &str) -> String {
    let plain = !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@');
    if plain {
        name.to_string()
    } else {
        format!("'{}'", name.replace('\'', "\\'"))
    }
}
