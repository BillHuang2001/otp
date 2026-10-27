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

//! `erlang:fun_info_mfa/1` — and its agreement with `erlang:fun_info/2` — for a
//! **compiled module's own** `make_fun3` local fun, pinned against the live system `erl`.
//!
//! # The defect this suite exists for
//!
//! A `make_fun3` closure's term inlines only its engine-internal descriptor
//! (`[uniq: u64][index: u32]`, `beam_term::fun`): it carries **no** module atom and no
//! lambda name, so the `{Module, Name, Arity}` triple the C answers through
//! `erts_get_fun_mfa(funp->entry.fun, erts_active_code_ix())` (`erl_bif_info.c:4029-4050`)
//! is derivable only from the *loaded* module's `FunT` table. That derivation is the
//! `beam_bifs::LocalFunIdentity` seam, and the runtime installs it through
//! `RuntimeContext::uses_fun_identity` — the dispatch gate. `fun_info_mfa/1` was missing
//! from that gate's introspection half, so the BIF ran with **no** bridge, the seam
//! answered its trait default `None`, and every call on a compiled module's local fun
//! raised the typed `unsupported_bif` gap instead of the oracle's triple. (OTP's own
//! `eunit_data.erl:542` calls exactly this BIF on every fun-spec, which is how the
//! `jsone`/`bbmustache` suites are written.)
//!
//! # What is pinned, and why an exact pin is legitimate here
//!
//! Every expected value below is the oracle's own `~w` rendering of the **same** expression
//! (`funmfa:<F>()`), measured with the system `erlc`/`erl` while this suite was written.
//! A compiled lambda's generated name (`-F/0-fun-0-`) is a deterministic function of the
//! source, so the literals cannot drift for a given fixture; the suite nevertheless asserts
//! the *oracle* side against the literal too, so a fixture edit that changes a name fails
//! loudly instead of silently re-pinning this runtime's own answer.
//!
//! The cases cover the shapes the derivation has to get right:
//!
//! * [`single`](single_case) — one anonymous fun, named after its enclosing function;
//! * [`siblings`](siblings_case) — **two** anonymous funs in the **same** function, i.e. the
//!   `-fun-0-`/`-fun-1-` numbering of one `FunT` table;
//! * [`spread`](spread_case) — two anonymous funs in **different** functions;
//! * [`named`](named_case) — the **named** lambda `fun Fact/1` (recursive, so the compiler
//!   keeps the name: `-named/0-Fact/1-0-`);
//! * [`external`](external_case) — an `EXPORT_EXT` (`fun lists:reverse/1`) fun, whose identity
//!   is inlined in the term and never consults the seam;
//! * [`name_items`](name_items_case) / [`info_agreement`](info_agreement_case) — the
//!   `fun_info/2` items (`name`/`module`/`arity`/`type`) for the same kind of fun, and their
//!   agreement **with** `fun_info_mfa/1` in one container, so the two BIFs cannot drift apart;
//! * [`mfa_item`](mfa_item_case) — `erlang:fun_info(F, mfa)` is `badarg` on the oracle (the C's
//!   `switch (what)` has no `mfa` arm), never a synonym for the triple.
//!
//! # Toolchain
//!
//! The oracle is resolved from the **filesystem** (`support::require_oracle`, never by running
//! the tool) and the compiler as `erlc` beside it or on `$PATH`. A missing toolchain is a
//! **loud skip** (`support::skip_loudly`), never a silent pass; a missing committed fixture
//! **fails**. Every probe is bounded by `support::PROBE_TIMEOUT`.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_engine::terms;
use beam_rt::error;
use beam_rt::{Runtime, RuntimeConfig};

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "fun_info_mfa";

/// The fixture module (its source lives in `tests/fixtures/`, its `.beam` is never committed).
const MODULE: &str = "funmfa";

/// The fixture's source, embedded so nothing is read relative to the test's CWD.
const FIXTURE_ERL: &str = include_str!("fixtures/funmfa.erl");

/// The defining module of the *unloaded* case's fun: only the first scratch directory
/// compiles it, so the using module's own directory — and therefore the code path — never
/// holds it.
const UNSRC_ERL: &str = "\
-module(fisrc).
-export([mk/0]).
mk() -> fun(Y) -> Y + 1 end.
";

/// The **using** module of the unloaded case: it decodes a `NEW_FUN_EXT` image embedded as a
/// byte literal (the oracle produced it from [`UNSRC_ERL`]) and reports the fun through both
/// introspection BIFs. Embedding the bytes keeps the fun's defining module off the code path,
/// which is exactly the unloaded-defining-module case.
fn unuse_src(image: &[u8]) -> String {
    let literal: Vec<String> = image.iter().map(u8::to_string).collect();
    format!(
        "-module(fidecm).\n\
         -export([mfa/0, name/0]).\n\
         mfa() ->\n    \
         erlang:fun_info_mfa(binary_to_term(<<{}>>)).\n\
         name() ->\n    \
         F = binary_to_term(<<{}>>),\n    \
         [erlang:fun_info(F, name), erlang:fun_info(F, module), erlang:fun_info_mfa(F)].\n",
        literal.join(","),
        literal.join(",")
    )
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------
/// A per-case scratch directory holding **only** the compiled fixture module (so it is also a
/// safe single `-pa` for the oracle).
struct Fixture {
    /// The scratch directory.
    dir: PathBuf,
}

impl Fixture {
    /// The compiled module's `.beam` inside the scratch directory.
    fn beam(&self, module: &str) -> PathBuf {
        self.dir.join(format!("{module}.beam"))
    }

    /// Removes the scratch directory (`erlc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }
}

/// A fresh scratch directory under `$TMPDIR` for `label`.
fn scratch(label: &str) -> Option<PathBuf> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_fun_info_mfa_{label}_{}_{n}",
        std::process::id()
    ));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly(SUITE, &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    Some(dir)
}

/// `erlc` beside the resolved oracle, else `erlc` on `$PATH` — the convention the crate's
/// other compiled-fixture suites use.
fn resolve_erlc(oracle: &Path) -> Option<PathBuf> {
    if let Some(dir) = oracle.parent() {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// Compiles `label.erl` (source `content`) into a fresh scratch directory with the system
/// `erlc`.
///
/// Returns `None` after a loud skip when the toolchain cannot do it, and **panics** when the
/// committed source is empty or `erlc` rejects it (a repository defect, not a skip).
fn compile(label: &str, module: &str, content: &str) -> Option<Fixture> {
    let oracle = support::require_oracle(SUITE)?;
    let Some(erlc) = resolve_erlc(&oracle) else {
        support::skip_loudly(SUITE, "no `erlc` beside the oracle and none on $PATH");
        return None;
    };
    let dir = scratch(label)?;
    assert!(
        !content.trim().is_empty(),
        "the `{module}` fixture source is empty"
    );
    let source = dir.join(format!("{module}.erl"));
    std::fs::write(&source, content)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", source.display()));
    let output = std::process::Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .output()
        .unwrap_or_else(|error| panic!("cannot run `{}`: {error}", erlc.display()));
    assert!(
        output.status.success(),
        "`{}` rejected the fixture {}: {}\n{}",
        erlc.display(),
        source.display(),
        String::from_utf8_lossy(&output.stderr).trim(),
        String::from_utf8_lossy(&output.stdout).trim()
    );
    let fixture = Fixture { dir };
    assert!(
        fixture.beam(module).is_file(),
        "`erlc` produced no {}; stderr:\n{}",
        fixture.beam(module).display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(fixture)
}

// ---------------------------------------------------------------------------------------
// The two sides of one comparison
// ---------------------------------------------------------------------------------------
/// The oracle's `~w` rendering of `funmfa:<function>()`, or `None` after a loud skip when the
/// probe could not answer.
fn oracle_answer(oracle: &Path, dir: &Path, function: &str) -> Option<String> {
    let expr = format!("io:format(\"~w~n\", [{MODULE}:{function}()]), halt(0).");
    match support::oracle_eval(oracle, Some(dir), &expr) {
        Some(answer) => Some(answer),
        None => {
            support::skip_loudly(SUITE, "the oracle could not answer the probe");
            None
        }
    }
}

/// This runtime's `~w` rendering of `funmfa:<function>()`.
///
/// [`Runtime::format_term_w`] is the `~w` printer — the same control the oracle probe used —
/// so the two strings are directly comparable.
fn runtime_answer(runtime: &mut Runtime, beam: &Path, function: &str) -> String {
    let term = runtime
        .run_function(beam, function, 0, &[])
        .unwrap_or_else(|e| panic!("{MODULE}:{function}/0 did not run: {e:?}"));
    runtime
        .format_term_w(term, None)
        .unwrap_or_else(|e| panic!("{MODULE}:{function}/0's result did not print: {e:?}"))
        .trim_end()
        .to_string()
}

/// A runtime over the default configuration.
fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("the default runtime")
}

// ---------------------------------------------------------------------------------------
// The `fun_info_mfa/1` cases
// ---------------------------------------------------------------------------------------
/// Every case: the `funmfa` function and the **oracle's own** `~w` answer for it, measured
/// with the system `erlc`/`erl` while this suite was written.
///
/// * `single` — one anonymous fun in a function of its own: the enclosing function's own
///   generated name;
/// * `siblings` — **two** anonymous funs in the **same** function: one `FunT` table, numbered
///   `-fun-0-` and `-fun-1-` in source order;
/// * `spread` — two anonymous funs in **different** functions, so the numbering restarts per
///   enclosing function;
/// * `named` — the **named** lambda `fun Fact(0) -> 1; Fact(N) -> N * Fact(N - 1) end`,
///   recursive so the compiler keeps the name: the generated atom embeds the fun's own name
///   and its arity (`-named/0-Fact/1-0-`);
/// * `external` — `fun lists:reverse/1`, an `EXPORT_EXT` fun whose whole identity is the
///   export it inlines (`type` is `external`, and the seam is never consulted);
/// * `name_items` — the `fun_info/2` items for one local fun: the `name` the seam supplies,
///   `type = local`, the module atom and the closure arity;
/// * `info_agreement` — the same fun's `name`/`module`/`arity`/`type` items **and** its
///   `fun_info_mfa/1` triple in one container, so the two BIFs cannot drift apart;
/// * `mfa_item` — `erlang:fun_info(F, mfa)` is `badarg` on the oracle: the C's `switch (what)`
///   (`erl_bif_info.c:3951-4014`) has no `mfa` arm, so the triple belongs to
///   `fun_info_mfa/1` alone.
const CASES: [(&str, &str); 8] = [
    ("single", "{funmfa,'-single/0-fun-0-',1}"),
    (
        "siblings",
        "[{funmfa,'-siblings/0-fun-0-',1},{funmfa,'-siblings/0-fun-1-',1}]",
    ),
    (
        "spread",
        "[{funmfa,'-first/0-fun-0-',1},{funmfa,'-second/0-fun-0-',1}]",
    ),
    ("named", "{funmfa,'-named/0-Fact/1-0-',1}"),
    ("external", "{lists,reverse,1}"),
    (
        "name_items",
        "{{name,'-name_items/0-fun-0-'},{type,local},{module,funmfa},{arity,1}}",
    ),
    (
        "info_agreement",
        "{{name,'-info_agreement/0-fun-0-'},{module,funmfa},{arity,1},{type,local},\
         {funmfa,'-info_agreement/0-fun-0-',1}}",
    ),
    ("mfa_item", "badarg"),
];

/// [`CASES`]' `single` pin (re-used by the scheduled-route test, which needs a subset).
fn single_pin() -> (&'static str, &'static str) {
    CASES[0]
}

/// [`CASES`]' `siblings` pin.
fn siblings_pin() -> (&'static str, &'static str) {
    CASES[1]
}

/// The oracle's triple for every fun shape, and this runtime's, against the same pins.
///
/// Both sides are asserted against the pinned literal, so a failure names which side moved:
/// a fixture edit that changes a generated name fails on the **oracle** side (the pin is not
/// this runtime's own answer re-recorded), and a derivation bug fails on **ours**.
#[test]
fn fun_info_mfa_answers_the_oracles_triple_for_every_fun_kind() {
    let Some(oracle) = support::require_oracle(SUITE) else {
        return;
    };
    let Some(fixture) = compile("shapes", MODULE, FIXTURE_ERL) else {
        return;
    };
    let mut runtime = runtime();
    for (function, expected) in CASES {
        let Some(oracle_answer) = oracle_answer(&oracle, &fixture.dir, function) else {
            fixture.cleanup();
            return;
        };
        assert_eq!(
            &oracle_answer, expected,
            "{MODULE}:{function}/0: the pin must be the oracle's own answer"
        );
        let ours = runtime_answer(&mut runtime, &fixture.beam(MODULE), function);
        assert_eq!(
            &ours, expected,
            "{MODULE}:{function}/0\n  ours:   {ours}\n  oracle: {oracle_answer}\n  pin:    {expected}"
        );
    }
    fixture.cleanup();
}

/// The triple, read **structurally** (not through the printer): the module and function
/// elements are atoms whose names are the pins, and element 3 is the closure arity.
///
/// This is the assertion that `{Module, Name, Arity}` is a real 3-tuple of the right atoms,
/// independent of the `~w` renderer above: a printer bug and a derivation bug could otherwise
/// cancel out.
#[test]
fn the_triple_is_a_three_tuple_of_the_pinned_atoms_and_arity() {
    // `compile` itself loud-skips without the toolchain, so a fixture here implies one.
    let Some(fixture) = compile("structural", MODULE, FIXTURE_ERL) else {
        return;
    };
    let mut runtime = runtime();
    for (function, module, name, arity) in [
        ("single", "funmfa", "-single/0-fun-0-", 1),
        ("named", "funmfa", "-named/0-Fact/1-0-", 1),
        ("external", "lists", "reverse", 1),
    ] {
        let term = runtime
            .run_function(&fixture.beam(MODULE), function, 0, &[])
            .unwrap_or_else(|e| panic!("{MODULE}:{function}/0 did not run: {e:?}"));
        let element = |index: usize| {
            terms::tuple_element(term, index).unwrap_or_else(|e| {
                panic!("{MODULE}:{function}/0 element {index} is not a tuple field: {e:?}")
            })
        };
        assert_eq!(
            runtime.atom_name_of(element(0)),
            Some(module),
            "{MODULE}:{function}/0 module element"
        );
        assert_eq!(
            runtime.atom_name_of(element(1)),
            Some(name),
            "{MODULE}:{function}/0 function element"
        );
        assert_eq!(
            beam_term::signed_value(element(2)),
            arity,
            "{MODULE}:{function}/0 arity element"
        );
        assert_eq!(
            terms::tuple_arity(term).expect("the answer is a tuple"),
            3,
            "{MODULE}:{function}/0 must be a 3-tuple"
        );
    }
    fixture.cleanup();
}

/// The **scheduled** route (`beam_rt run <file.beam> <mod>:<fun>/<arity>`) answers the same
/// triples as the single-pass one: the `LocalFunIdentity` gate lives in the shared dispatch
/// prologue (`RuntimeContext::call_registered`), so a real scheduler slice over the real
/// binary must not diverge from the in-process route the tests above drive.
#[test]
fn the_scheduled_run_route_answers_the_same_triples() {
    /// The **real** executable under test (the test target's build guarantees it is current).
    const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");
    let Some(fixture) = compile("scheduled", MODULE, FIXTURE_ERL) else {
        return;
    };
    let beam = fixture.beam(MODULE);
    let beam = beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", beam.display()));
    for (function, expected) in [single_pin(), siblings_pin()] {
        let spec = format!("{MODULE}:{function}/0");
        let output = support::run_bounded(Path::new(BINARY), &["run", beam, spec.as_str()])
            .unwrap_or_else(|| panic!("`beam_rt run {spec}` could not be spawned"));
        assert_eq!(
            output.status.code(),
            Some(0),
            "`beam_rt run {spec}` must succeed; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let printed = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            printed.trim_end(),
            expected,
            "`beam_rt run {spec}`\n  ours:   {}\n  pin:    {expected}",
            printed.trim_end()
        );
    }
    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------
// The unloaded defining module: the C's `{Module, [], Arity}`, and the `name` boundary
// ---------------------------------------------------------------------------------------
/// `erlang:fun_info_mfa/1` of an **imported** fun whose defining module is not loaded answers
/// the C's `{Module, [], Arity}` on both sides.
///
/// `fun_info_mfa_1`'s local arm (`erl_bif_info.c:4029-4040`) reports the fun's own module atom,
/// `NIL` for the function name and the closure arity when `erts_get_fun_mfa/2` cannot resolve
/// the entry — and this runtime's `beam_bifs` `Imported` arm answers the same shape from the
/// wire descriptor when the `LocalFunIdentity` seam declines (the defining module is not on the
/// code path, so no `FunT` entry can match).
///
/// The same fun's `erlang:fun_info(F, name)` stays the **documented typed gap** (the oracle
/// reports `{name,[]}`, this runtime raises `unsupported_bif` — a seam that answers "an
/// identity or the gap" has no `NIL` to report). Both sides are pinned, so the boundary is
/// recorded rather than hidden.
#[test]
fn an_unloaded_defining_module_answers_the_c_mfa_shape_and_keeps_the_documented_name_gap() {
    let Some(oracle) = support::require_oracle(SUITE) else {
        return;
    };
    let Some(source_dir) = compile("unloaded_src", "fisrc", UNSRC_ERL) else {
        return;
    };
    // The oracle encodes its own lambda…
    let Some(hex) = support::oracle_eval(
        &oracle,
        Some(&source_dir.dir),
        "io:format(\"~s\", [binary:encode_hex(term_to_binary(fisrc:mk()))]), halt(0).",
    ) else {
        source_dir.cleanup();
        support::skip_loudly(SUITE, "the oracle could not encode the fixture's fun");
        return;
    };
    let image = decode_hex(&hex);
    assert!(
        !image.is_empty(),
        "the oracle's `NEW_FUN_EXT` image must not be empty (hex {hex:?})"
    );
    // …and the using module embeds those bytes, so the fun it decodes names a module that is on
    // no code path in this test (its own directory holds only `fidecm`).
    let Some(use_dir) = compile("unloaded_use", "fidecm", &unuse_src(&image)) else {
        source_dir.cleanup();
        return;
    };

    // `fun_info_mfa/1`: the C's `{Module, [], Arity}`, on both sides.
    let expected_mfa = "{fisrc,[],1}";
    let Some(oracle_mfa) = oracle_answer(&oracle, &use_dir.dir, "mfa") else {
        source_dir.cleanup();
        use_dir.cleanup();
        return;
    };
    assert_eq!(
        oracle_mfa, expected_mfa,
        "the oracle must answer the C's unloaded shape"
    );
    let mut runtime = runtime();
    let ours = runtime_answer(&mut runtime, &use_dir.beam("fidecm"), "mfa");
    assert_eq!(ours, expected_mfa, "the unloaded `fun_info_mfa/1` shape");

    // `fun_info(F, name)`: the oracle answers `[]`; this runtime keeps the typed gap.
    let expr = "io:format(\"~w~n\", [fidecm:name()]), halt(0).";
    let Some(oracle_name) = support::oracle_eval(&oracle, Some(&use_dir.dir), expr) else {
        source_dir.cleanup();
        use_dir.cleanup();
        support::skip_loudly(SUITE, "the oracle could not answer the `name` probe");
        return;
    };
    assert!(
        oracle_name.contains("{name,[]}") && oracle_name.contains("{module,fisrc}"),
        "the oracle must report `name = []` for the unloaded defining module, got {oracle_name}"
    );
    let error = runtime
        .run_function(use_dir.beam("fidecm"), "name", 0, &[])
        .expect_err("an unloaded defining module must not fabricate a fun name");
    assert_eq!(
        error::unsupported_reason(&error).as_deref(),
        Some("bif"),
        "the typed gap must be the `unsupported_bif` raise, got {error:?}"
    );

    source_dir.cleanup();
    use_dir.cleanup();
}

/// Decodes `binary:encode_hex/1`'s lowercase hex (the oracle's own rendering).
fn decode_hex(hex: &str) -> Vec<u8> {
    let digits: Vec<u8> = hex
        .trim()
        .bytes()
        .map(|byte| match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            other => panic!("not a hex digit: {:?}", other as char),
        })
        .collect();
    assert_eq!(digits.len() % 2, 0, "hex must have an even digit count");
    digits.chunks_exact(2).map(|pair| pair[0] << 4 | pair[1]).collect()
}
