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

//! The synthesized callee head item, end to end, on a **real `erlc`-compiled
//! module** and against the **system oracle**.
//!
//! [`crate::families::exceptions::callee`] synthesizes the innermost item of an
//! `undef` raise's cooked stacktrace — C's `erts_build_mfa_item` with the
//! exception's saved argument list — and
//! [`crate::families::exceptions::stacktrace::symbolic_stacktrace`] conses it in
//! front of the raising frame chain. The sibling [`super::callee`] unit tests pin
//! that from a hand-built `Module` and a `BifHook`; **this** module pins it from
//! the shapes a real compiler actually emits, loaded through
//! [`beam_loader::load_file`], and cross-checks every field against the running
//! Erlang system.
//!
//! # Oracle discipline
//!
//! Each test compiles a fixture with the system `erlc` and asks the system `erl`
//! for the **same** `{Class,Reason,S}` triple, printing it field by field with
//! `~p` so the comparison is textual and exact (never a hand-written expectation
//! that could drift). The engine's own triple is rendered the same way. Every
//! test **skips loudly** (`eprintln!`) when `erlc`/`erl` are absent, and a spawned
//! `erlc`/`erl` that exits non-zero panics with its stderr — a harness failure
//! must not masquerade as an absent toolchain. The discipline mirrors
//! [`crate::machine::deadline_tests`].
//!
//! **The engine's cooked list is a strictly shorter PREFIX of the oracle's** (the
//! oracle appends its own `init`/`erl_eval` caller frames, which live *outside*
//! this engine's run — see `super::stacktrace`), so the assertion is: the
//! engine's class/reason line equals the oracle's, the engine cooked a non-empty
//! list, and that list equals the oracle's **first** `n` items.
//!
//! # The fixture
//!
//! The module `p3` is compiled from [`FIXTURE_SRC`]; it exercises every failing
//! call site that publishes a head item — a direct `call_ext` to a missing
//! function of an existing module, both compiled `apply/3` routes (the `call_ext
//! erlang:apply/3` encoding and the BEAM `apply` instruction), a missing module —
//! plus the latch-hygiene pair.

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_common::{ExitReason, Raise};
use beam_loader::{load_file, Atom};
use beam_term::{atom_val, is_atom, is_list, is_small, signed_value, Eterm};

use crate::jit::Tier;

use super::*;

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// The fixture module, compiled by the system `erlc`.
///
/// The `later/1` / `headless/1` pair exists for the latch-hygiene test: the
/// `undef` in `latch_hygiene/1` lands in a handler that **ignores `S`** (so no
/// `build_stacktrace` ever consumes the synthesized-head latch), and the raise
/// that follows it is raised in a **different function** whose handler does use
/// `S` — so a leaked head item would show up as an extra first item there.
const FIXTURE_SRC: &str = r#"-module(p3).

-export([missing/0,
         go_apply/0,
         call_h/3,
         dyn_apply/4,
         go_missing_module/0,
         latch_hygiene/1,
         headless/1]).

%% (a) a direct `call_ext` to a MISSING function of an EXISTING module.
missing() ->
    try lists:no_such_function(1, 2) catch C:R:S -> {C,R,S} end.

%% (b) `apply/3` through the `call_ext erlang:apply/3` encoding.
go_apply() ->
    call_h(nonexistent_mod, f, [a, b, c]).

call_h(A, B, C) ->
    try apply(A, B, C) catch C2:R:S -> {C2,R,S} end.

%% (b) `apply/3` through the BEAM `apply` instruction (a dynamic
%% `M:F(A, B)`, so the compiler cannot name a literal callee).
dyn_apply(M, F, A, B) ->
    try M:F(A, B) catch C:R:S -> {C,R,S} end.

%% (c) a call to a MISSING module.
go_missing_module() ->
    try totally_missing_module:f(a, b) catch C:R:S -> {C,R,S} end.

%% (d) an `undef` raise whose handler ignores S, then a later unrelated raise
%% raised in a different function whose handler uses S.
latch_hygiene(X) ->
    _ = try lists:no_such_function(1, 2) catch _C:_R:_S -> leaked end,
    later(X).

later(X) ->
    try case X of ok -> ok end catch C:R:S -> {C,R,S} end.

%% (d) a head-less raise (no unresolvable callee) whose handler uses S.
headless(X) ->
    try case X of ok -> ok end catch C:R:S -> {C,R,S} end.
"#;

/// The runtime's **globally interned** atoms the fixture's own atom table must
/// carry: the exception-class atom a landing installs in `x(3)` (`error`), the
/// `undef` reason and the `case_clause` tag the raising opcodes expand, and the
/// `build_stacktrace` source-location keys.
///
/// A real runtime keeps these interned for *every* module whether or not the
/// module's source mentions them; the real OTP compiler does not emit them into a
/// module's atom chunk. The production counterpart of this seed is
/// `beam_rt::boot::CORE_ATOMS` (and, in the crate's integration suites,
/// `tests/support/atoms.rs`'s `SEED_ATOMS`). The engine's lookup policy is
/// deliberately unchanged: it still resolves a reason atom by name from the
/// running module's table and never substitutes a *different* atom.
const SEED_ATOMS: &[&str] = &["error", "undef", "case_clause", "file", "line"];

/// `beam_rt`'s `error::undef()` shape: class `error`, the bare reason atom
/// `undef` — what a code server answers for a call target it cannot resolve.
fn undef_error() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("undef")))
}

/// A `MinimalContext` whose BIF/apply hook answers `error:undef` for the
/// fixture's (deliberately) unresolvable call targets and rejects everything else
/// with the engine's typed `unsupported`.
///
/// The allowlist is what makes the test honest: a *stray* call the fixture is not
/// supposed to make fails loudly instead of silently answering `undef` and
/// making a wrong test pass.
fn undef_host() -> MinimalContext {
    MinimalContext::with_bif_hook(Box::new(|module, function, arity, _args| {
        if matches!(
            (module, function, arity),
            ("lists", "no_such_function", 2)
                | ("nonexistent_mod", "f", 3)
                | ("nonexistent_mod", "f", 2)
                | ("totally_missing_module", "f", 2)
        ) {
            return Err(undef_error());
        }
        Err(crate::error::unsupported(&format!(
            "bif {module}:{function}/{arity}"
        )))
    }))
}

/// Compile [`FIXTURE_SRC`] into a per-`tag` temp dir and return that dir; `None`
/// when no runnable `erlc` is on `PATH` at all (the caller skips loudly).
///
/// The `tag` keeps tests running in parallel from sharing a scratch directory.
/// `None` means `erlc` could not be **spawned**; an `erlc` that *ran* and exited
/// non-zero is a harness failure (the source is valid Erlang) and panics with its
/// stderr.
fn compile_fixture(tag: &str) -> Option<PathBuf> {
    compile_module(tag, "p3", FIXTURE_SRC)
}

/// Compile `source` into `{module_name}.erl` under a per-`tag` temp dir and return
/// that dir — the general form of [`compile_fixture`], shared with the sibling
/// oracle-backed suites under `super::`.
///
/// The `tag` keeps tests running in parallel from sharing a scratch directory.
/// `None` means `erlc` could not be **spawned** (the caller skips loudly); an
/// `erlc` that *ran* and exited non-zero is a harness failure (the source is valid
/// Erlang) and panics with its stderr.
pub(super) fn compile_module(tag: &str, module_name: &str, source: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("beam_engine_exceptions_oracle_{tag}"));
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    let file = format!("{module_name}.erl");
    std::fs::write(dir.join(&file), source)
        .unwrap_or_else(|error| panic!("write {file}: {error}"));
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg(&file)
        .current_dir(&dir)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erlc` exited {} compiling {file}\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(dir)
}

/// Load `p3.beam` from `dir` through [`beam_loader::load_file`] and intern the
/// [`SEED_ATOMS`] the runtime holds globally.
fn load_fixture(dir: &Path) -> Module {
    load_module(dir, "p3", &[])
}

/// Load `{module_name}.beam` from `dir` through [`beam_loader::load_file`] and
/// intern the [`SEED_ATOMS`] the runtime holds globally (plus `extra_seed`) — the
/// general form of [`load_fixture`], shared with the sibling oracle-backed suites
/// under `super::`.
///
/// A real runtime keeps the [`SEED_ATOMS`] interned for *every* module whether or
/// not the module's source mentions them, and the real OTP compiler does not emit
/// them into a module's atom chunk; a fixture whose code path needs a further
/// globally interned atom (`function_clause`, …) names it in `extra_seed`.
///
/// Interning only ever **appends**, so every atom index the loader assigned (and
/// every literal term built from it) stays valid.
pub(super) fn load_module(dir: &Path, module_name: &str, extra_seed: &[&str]) -> Module {
    let beam = dir.join(format!("{module_name}.beam"));
    let mut module = load_file(&beam).unwrap_or_else(|error| panic!("load {}: {error:?}", beam.display()));
    for &name in SEED_ATOMS.iter().chain(extra_seed) {
        module.atom_table.intern(name);
    }
    module
}

// ---------------------------------------------------------------------------
// The oracle probe
// ---------------------------------------------------------------------------

/// The system oracle's answer for one fixture call: the `Class|Reason` line and
/// one `M|F|Third|Location` line per cooked stacktrace item, exactly as `~p`
/// prints them.
pub(super) struct OracleTrace {
    /// `~p` of the class and of the reason, joined by `|`.
    pub(super) head: String,
    /// One line per cooked item, the raising frame first.
    pub(super) items: Vec<String>,
}

/// Ask the system `erl` for `call`'s `{Class,Reason,S}` triple, printing the
/// class and reason and then every cooked item's four fields with `~p`.
///
/// `-pz`, never `-pa`: appending keeps the real stdlib ahead of the fixture
/// module. `None` when no runnable `erl` is on `PATH`; a spawned `erl` that exits
/// non-zero is a harness failure and panics with its stderr.
pub(super) fn oracle_trace(dir: &Path, call: &str) -> Option<OracleTrace> {
    let probe = format!(
        "{{C,R,S}} = {call}, \
         io:format(\"~p|~p~n\",[C,R]), \
         lists:foreach(fun(I) -> \
             io:format(\"~p|~p|~p|~p~n\", \
                 [element(1,I),element(2,I),element(3,I),element(4,I)]) end, S), \
         halt(0)."
    );
    let output = Command::new("erl")
        .args(["-noshell", "-pz"])
        .arg(dir)
        .args(["-eval", &probe])
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {} for {call}\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut lines = text.lines().map(str::trim_end).filter(|line| !line.is_empty());
    let head = lines.next().expect("the oracle probe prints a class|reason line").to_string();
    let items = lines.map(str::to_string).collect();
    Some(OracleTrace { head, items })
}

/// Assert the engine's answer for `what` agrees with the oracle: the same
/// class/reason, a **non-empty** cooked trace, and the oracle's first items
/// (the engine cooks a strictly shorter prefix — see the module docs).
pub(super) fn assert_matches_the_oracle(what: &str, head: &str, items: &[String], oracle: &OracleTrace) {
    assert_eq!(
        head, oracle.head,
        "{what}: the engine's class/reason disagrees with the oracle"
    );
    assert!(
        !items.is_empty(),
        "{what}: the engine cooked an EMPTY stacktrace; the oracle cooked {} item(s)",
        oracle.items.len()
    );
    assert!(
        items.len() <= oracle.items.len(),
        "{what}: the engine cooked {} items but the oracle only cooked {}\n  engine: {items:#?}\n  oracle: {:#?}",
        items.len(),
        oracle.items.len(),
        oracle.items
    );
    assert_eq!(
        items,
        &oracle.items[..items.len()],
        "{what}: the engine's cooked stacktrace is not a prefix of the oracle's"
    );
}

// ---------------------------------------------------------------------------
// Rendering — the engine's terms in the oracle probe's `~p` text
// ---------------------------------------------------------------------------

/// The name of the atom `term` as the oracle prints it (an atom never needs
/// quoting in these fixtures).
pub(super) fn render_atom(term: Eterm, module: &Module) -> String {
    assert!(is_atom(term), "expected an atom, got {term:?}");
    module
        .atom_name(atom_val(term))
        .unwrap_or_else(|| panic!("the module's atom table cannot name {term:?}"))
        .to_string()
}

/// Render `term` the way the oracle probe's `~p` does, restricted to the shapes
/// these fixtures build: an atom, a small integer, a proper list of those, or a
/// tuple of those (a `{case_clause,X}`-style reason).
pub(super) fn render_flat(term: Eterm, module: &Module) -> String {
    if is_atom(term) {
        return render_atom(term, module);
    }
    if is_small(term) {
        return signed_value(term).to_string();
    }
    if is_nil(term) {
        return "[]".to_string();
    }
    if is_list(term) {
        let items = crate::terms::list_elements(term).expect("a proper list");
        let rendered: Vec<String> = items.iter().map(|t| render_flat(*t, module)).collect();
        return format!("[{}]", rendered.join(","));
    }
    let arity = crate::terms::tuple_arity(term).expect("a boxed term is a tuple here");
    let rendered: Vec<String> = (0..arity)
        .map(|index| {
            let field = crate::terms::tuple_element(term, index).expect("a tuple field");
            render_flat(field, module)
        })
        .collect();
    format!("{{{}}}", rendered.join(","))
}

/// Render a cooked item's `Location` field: the empty list, or the
/// `[{file,"Name"},{line,N}]` shape `build_stacktrace` builds.
///
/// Special-cased (never [`render_flat`]) because the file name is a **char list**
/// the oracle prints as a string.
pub(super) fn render_location(term: Eterm) -> String {
    if is_nil(term) {
        return "[]".to_string();
    }
    let pairs = crate::terms::list_elements(term).expect("a location is a list");
    assert_eq!(pairs.len(), 2, "a location is `[{{file,_}},{{line,_}}]`, got {pairs:?}");
    let name: String = crate::terms::list_elements(
        crate::terms::tuple_element(pairs[0], 1).expect("the file pair's value"),
    )
    .expect("the file name is a char list")
    .iter()
    .map(|code| char::from_u32(signed_value(*code) as u32).expect("a scalar value"))
    .collect();
    let line = crate::terms::tuple_element(pairs[1], 1).expect("the line pair's value");
    format!("[{{file,\"{name}\"}},{{line,{}}}]", signed_value(line))
}

/// Render one cooked stacktrace item the way the oracle probe prints its four
/// fields.
pub(super) fn render_item(item: Eterm, module: &Module) -> String {
    let name = render_atom(crate::terms::tuple_element(item, 0).expect("the item's module"), module);
    let function = render_atom(
        crate::terms::tuple_element(item, 1).expect("the item's function"),
        module,
    );
    let third = crate::terms::tuple_element(item, 2).expect("the item's third field");
    let location = render_location(
        crate::terms::tuple_element(item, 3).expect("the item's location"),
    );
    format!("{name}|{function}|{}|{location}", render_flat(third, module))
}

// ---------------------------------------------------------------------------
// Driving the engine
// ---------------------------------------------------------------------------

/// Run `module:name/arity(args)` on `tier`, assert it returned the compiler's
/// `{Class,Reason,Cooked}` triple, and hand the three terms back.
pub(super) fn run_fixture(
    module: &Module,
    ctx: &mut MinimalContext,
    name: Atom,
    arity: u32,
    args: &[Eterm],
    tier: Tier,
) -> (Eterm, Eterm, Eterm) {
    let mut machine = Machine::new();
    let result = machine
        .run_with_tier(module, name, arity, args, ctx, tier)
        .unwrap_or_else(|error| panic!("running {name:?}/{arity} on {tier:?}: {error:?}"));
    let third = crate::terms::tuple_arity(result)
        .unwrap_or_else(|error| panic!("the handler returns `{{C,R,S}}`, got {error:?}"));
    assert_eq!(third, 3, "the handler returns the 3-element `{{C,R,S}}` triple");
    (
        crate::terms::tuple_element(result, 0).expect("the class"),
        crate::terms::tuple_element(result, 1).expect("the reason"),
        crate::terms::tuple_element(result, 2).expect("the cooked stacktrace"),
    )
}

/// The engine's `Class|Reason` line and cooked items, rendered exactly like
/// [`oracle_trace`]'s.
pub(super) fn engine_trace(class: Eterm, reason: Eterm, cooked: Eterm, module: &Module) -> (String, Vec<String>) {
    let head = format!("{}|{}", render_flat(class, module), render_flat(reason, module));
    let items = crate::terms::list_elements(cooked)
        .expect("the cooked stacktrace is a proper list")
        .iter()
        .map(|item| render_item(*item, module))
        .collect();
    (head, items)
}

/// The atom `module` names, or a harness panic.
pub(super) fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("the fixture module does not name `{name}`"))
}

// ---------------------------------------------------------------------------
// (a) a direct `call_ext` to a missing function of an EXISTING module
// ---------------------------------------------------------------------------

/// `p3:missing/0` is `try lists:no_such_function(1, 2) catch C:R:S -> {C,R,S} end`
/// — a real `call_ext` to a missing function of an **existing** module. The
/// oracle cooks `{lists,no_such_function,[1,2],[]}` as the innermost item (the
/// callee named with its real argument list and the empty location, because the
/// callee does not exist) and the raising frame next.
#[test]
fn a_compiled_call_ext_to_a_missing_function_cooks_the_callee_head_item() {
    let Some(dir) = compile_fixture("missing") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the compiled missing-function test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let name = atom(&module, "missing");
    let mut ctx = undef_host();
    let (class, reason, cooked) = run_fixture(&module, &mut ctx, name, 0, &[], Tier::Interpreter);
    let (head, items) = engine_trace(class, reason, cooked, &module);

    let Some(oracle) = oracle_trace(&dir, "p3:missing()") else {
        eprintln!("SKIP: no system `erl` on PATH -- the compiled missing-function oracle check is skipped");
        return;
    };
    assert_matches_the_oracle("p3:missing/0", &head, &items, &oracle);

    // The synthesized head item is exactly one cons cell in front of the raising
    // frame, and it is the CALLEE named with its real argument list.
    assert_eq!(items.len(), 2, "the synthesized callee item then the raising frame");
    let frame = crate::terms::list_elements(cooked).expect("a proper list");
    let item = frame[0];
    assert_eq!(
        render_atom(crate::terms::tuple_element(item, 0).expect("the module"), &module),
        "lists",
        "the item names the import's module"
    );
    assert_eq!(
        render_atom(crate::terms::tuple_element(item, 1).expect("the function"), &module),
        "no_such_function",
        "the item names the import's function"
    );
    assert_eq!(
        crate::terms::tuple_element(item, 3).expect("the location"),
        NIL,
        "the location of a callee that does not exist is the empty list"
    );
    let args = crate::terms::list_elements(crate::terms::tuple_element(item, 2).expect("the args"))
        .expect("the item's ArgList is a proper list");
    assert_eq!(
        args,
        vec![make_small(1), make_small(2)],
        "the item carries the callee's REAL argument list, not an arity"
    );
    assert_eq!(
        render_atom(crate::terms::tuple_element(frame[1], 1).expect("the function"), &module),
        "missing",
        "the raising frame follows the synthesized item"
    );
}

// ---------------------------------------------------------------------------
// (b) the two compiled `apply/3` routes
// ---------------------------------------------------------------------------

/// The BEAM **`call_ext erlang:apply/3`** route: `p3:call_h/3` is
/// `try apply(A, B, C) catch C2:R:S -> {C2,R,S} end`, invoked from `go_apply/0`
/// (a tail call, so the raising frame is `call_h/3` itself). The oracle cooks
/// `{nonexistent_mod,f,[a,b,c],[]}` in front of `{p3,call_h,3,...}`.
#[test]
fn a_compiled_call_ext_encoded_apply_cooks_the_callee_head_item() {
    let Some(dir) = compile_fixture("apply_ext") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the compiled apply/3 test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let name = atom(&module, "go_apply");
    let mut ctx = undef_host();
    let (class, reason, cooked) = run_fixture(&module, &mut ctx, name, 0, &[], Tier::Interpreter);
    let (head, items) = engine_trace(class, reason, cooked, &module);

    let Some(oracle) = oracle_trace(&dir, "p3:go_apply()") else {
        eprintln!("SKIP: no system `erl` on PATH -- the compiled apply/3 oracle check is skipped");
        return;
    };
    assert_matches_the_oracle("p3:go_apply/0", &head, &items, &oracle);

    assert_eq!(items.len(), 2, "the synthesized callee item then the raising frame");
    let frame = crate::terms::list_elements(cooked).expect("a proper list");
    let item = frame[0];
    assert_eq!(
        render_atom(crate::terms::tuple_element(item, 0).expect("the module"), &module),
        "nonexistent_mod"
    );
    assert_eq!(
        render_atom(crate::terms::tuple_element(item, 1).expect("the function"), &module),
        "f"
    );
    let args = crate::terms::list_elements(crate::terms::tuple_element(item, 2).expect("the args"))
        .expect("a proper list");
    assert_eq!(
        args,
        vec![
            make_atom(atom(&module, "a")),
            make_atom(atom(&module, "b")),
            make_atom(atom(&module, "c")),
        ],
        "the `[a,b,c]` argument list the compiled `apply/3` walked into x(0..3)"
    );
    assert_eq!(
        crate::terms::tuple_element(item, 3).expect("the location"),
        NIL
    );
    assert_eq!(
        render_atom(crate::terms::tuple_element(frame[1], 1).expect("the function"), &module),
        "call_h",
        "the raising frame is the tail-called helper"
    );
}

/// The BEAM **`apply` instruction** route: `p3:dyn_apply/4` is
/// `try M:F(A, B) catch C:R:S -> {C,R,S} end`, a dynamic remote call the compiler
/// cannot turn into a literal `call_ext`. The oracle cooks
/// `{nonexistent_mod,f,[a,b],[]}` in front of `{p3,dyn_apply,4,...}`.
#[test]
fn a_compiled_dynamic_apply_instruction_cooks_the_callee_head_item() {
    let Some(dir) = compile_fixture("apply_insn") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the dynamic `apply` test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let name = atom(&module, "dyn_apply");
    // `dyn_apply(nonexistent_mod, f, a, b)` — the call the fixture's own
    // `go_apply/0` shape would make, driven directly so the arguments are visible.
    let args = [
        make_atom(atom(&module, "nonexistent_mod")),
        make_atom(atom(&module, "f")),
        make_atom(atom(&module, "a")),
        make_atom(atom(&module, "b")),
    ];
    let mut ctx = undef_host();
    let (class, reason, cooked) = run_fixture(&module, &mut ctx, name, 4, &args, Tier::Interpreter);
    let (head, items) = engine_trace(class, reason, cooked, &module);

    let Some(oracle) = oracle_trace(&dir, "p3:dyn_apply(nonexistent_mod, f, a, b)") else {
        eprintln!("SKIP: no system `erl` on PATH -- the dynamic `apply` oracle check is skipped");
        return;
    };
    assert_matches_the_oracle("p3:dyn_apply/4", &head, &items, &oracle);

    assert_eq!(items.len(), 2, "the synthesized callee item then the raising frame");
    let frame = crate::terms::list_elements(cooked).expect("a proper list");
    let item = frame[0];
    assert_eq!(
        render_atom(crate::terms::tuple_element(frame[1], 0).expect("the module"), &module),
        "p3"
    );
    assert_eq!(
        crate::terms::tuple_element(item, 3).expect("the location"),
        NIL
    );
    let args = crate::terms::list_elements(crate::terms::tuple_element(item, 2).expect("the args"))
        .expect("a proper list");
    assert_eq!(
        args,
        vec![make_atom(atom(&module, "a")), make_atom(atom(&module, "b"))],
        "the BEAM `apply` register shape: x(0..2) are the arguments"
    );
}

// ---------------------------------------------------------------------------
// (c) a MISSING module
// ---------------------------------------------------------------------------

/// `p3:go_missing_module/0` is
/// `try totally_missing_module:f(a, b) catch C:R:S -> {C,R,S} end`. The oracle
/// cooks the missing **module** with its real arguments.
#[test]
fn a_compiled_call_to_a_missing_module_cooks_the_callee_head_item() {
    let Some(dir) = compile_fixture("missing_module") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the missing-module test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let name = atom(&module, "go_missing_module");
    let mut ctx = undef_host();
    let (class, reason, cooked) = run_fixture(&module, &mut ctx, name, 0, &[], Tier::Interpreter);
    let (head, items) = engine_trace(class, reason, cooked, &module);

    let Some(oracle) = oracle_trace(&dir, "p3:go_missing_module()") else {
        eprintln!("SKIP: no system `erl` on PATH -- the missing-module oracle check is skipped");
        return;
    };
    assert_matches_the_oracle("p3:go_missing_module/0", &head, &items, &oracle);

    assert_eq!(items.len(), 2, "the synthesized callee item then the raising frame");
    let frame = crate::terms::list_elements(cooked).expect("a proper list");
    let item = frame[0];
    assert_eq!(
        render_atom(crate::terms::tuple_element(item, 0).expect("the module"), &module),
        "totally_missing_module",
        "the item names the missing module"
    );
    assert_eq!(
        render_atom(crate::terms::tuple_element(item, 1).expect("the function"), &module),
        "f"
    );
    let args = crate::terms::list_elements(crate::terms::tuple_element(item, 2).expect("the args"))
        .expect("a proper list");
    assert_eq!(
        args,
        vec![make_atom(atom(&module, "a")), make_atom(atom(&module, "b"))]
    );
    assert_eq!(crate::terms::tuple_element(item, 3).expect("the location"), NIL);
}

// ---------------------------------------------------------------------------
// (d) latch hygiene
// ---------------------------------------------------------------------------

/// The synthesized-head latch describes **exactly one** cooking.
///
/// `p3:latch_hygiene/1` first raises the `undef` inside a `try` whose handler
/// **ignores `S`** (so no `build_stacktrace` ever consumes the latch), then
/// tail-calls `later/1`, whose `case_clause` raise *does* use `S`. The oracle's
/// trace for the second raise starts at `{p3,later,1,…}` — if the first raise's
/// synthesized `{lists,no_such_function,[1,2],[]}` item leaked, it would appear as
/// an extra first item and the prefix comparison would fail.
#[test]
fn a_synthesized_head_item_does_not_leak_into_a_later_raise() {
    let Some(dir) = compile_fixture("latch") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the latch-hygiene test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let name = atom(&module, "latch_hygiene");
    // `nope` is the test's own scrutinee; it is not in the fixture source, so it
    // is interned here (appending never shifts an existing index).
    let mut module = module;
    let nope = make_atom(module.atom_table.intern("nope"));
    let mut ctx = undef_host();
    let (class, reason, cooked) = run_fixture(&module, &mut ctx, name, 1, &[nope], Tier::Interpreter);
    let (head, items) = engine_trace(class, reason, cooked, &module);

    let Some(oracle) = oracle_trace(&dir, "p3:latch_hygiene(nope)") else {
        eprintln!("SKIP: no system `erl` on PATH -- the latch-hygiene oracle check is skipped");
        return;
    };
    assert_matches_the_oracle("p3:latch_hygiene/1", &head, &items, &oracle);

    // The second raise is raised in `later/1` (a tail call, so the engine cooks
    // exactly that one frame) — never the leaked `lists:no_such_function/2` item.
    assert_eq!(items.len(), 1, "the second raise's own raising frame, and nothing else");
    let frame = crate::terms::list_elements(cooked).expect("a proper list");
    assert_eq!(
        render_atom(crate::terms::tuple_element(frame[0], 1).expect("the function"), &module),
        "later",
        "the trace starts at the SECOND raise's raising frame"
    );
}

/// A head-less raise must not acquire a head: `p3:headless/1` raises a
/// `case_clause` (no unresolvable callee), so its cooked trace is its own raising
/// frame alone.
#[test]
fn a_head_less_raise_cooks_only_its_raising_frame() {
    let Some(dir) = compile_fixture("headless") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the head-less-raise test is skipped");
        return;
    };
    let mut module = load_fixture(&dir);
    let name = atom(&module, "headless");
    let nope = make_atom(module.atom_table.intern("nope"));
    let mut ctx = undef_host();
    let (class, reason, cooked) = run_fixture(&module, &mut ctx, name, 1, &[nope], Tier::Interpreter);
    let (head, items) = engine_trace(class, reason, cooked, &module);

    let Some(oracle) = oracle_trace(&dir, "p3:headless(nope)") else {
        eprintln!("SKIP: no system `erl` on PATH -- the head-less-raise oracle check is skipped");
        return;
    };
    assert_matches_the_oracle("p3:headless/1", &head, &items, &oracle);

    assert_eq!(items.len(), 1, "one raising frame, no synthesized callee item");
    let frame = crate::terms::list_elements(cooked).expect("a proper list");
    assert_eq!(
        render_atom(crate::terms::tuple_element(frame[0], 1).expect("the function"), &module),
        "headless"
    );
}

// ---------------------------------------------------------------------------
// (e) the invariant the mechanism depends on
// ---------------------------------------------------------------------------

/// **Pin the compiler's `try … catch C:R:S -> S` handler shape.**
///
/// The engine's `build_stacktrace` reads the raw-trace word from its own operand
/// (`x(0)`, see `super::Instr::BuildStacktrace`) and re-reads it after its one
/// reservation through that same operand. That is only the synthesized callee head
/// item because the compiler emits, for a handler whose `S` is used:
///
/// ```text
///   try_case Frame          ; x(1)=reason, x(2)=raw trace, x(3)=class
///   move {x,2} -> {x,0}     ; the raw trace is MOVED into x(0)
///   build_stacktrace        ; ... which reads x(0)
/// ```
///
/// If a future compiler changed that (e.g. handed the trace to `build_stacktrace`
/// in `x(2)`, or dropped the move), the engine would read the wrong word and the
/// head item would silently disappear. This test fails **loudly** and prints the
/// disassembly when the shape changes — for the loaded IR *and* for `erlc`'s own
/// textual output.
#[test]
fn the_compilers_stacktrace_handler_moves_the_raw_trace_into_x0() {
    let Some(dir) = compile_fixture("handler_shape") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the handler-shape pin is skipped");
        return;
    };
    let module = load_fixture(&dir);

    // (1) The loaded IR: every `build_stacktrace` is immediately preceded by the
    //     `move x(2) -> x(0)` that publishes the raw trace in its operand.
    let mut handlers = 0;
    for function in &module.functions {
        for (pc, instr) in function.code.iter().enumerate() {
            if !matches!(instr, Instr::BuildStacktrace) {
                continue;
            }
            handlers += 1;
            let previous = function.code[..pc].last();
            assert!(
                matches!(
                    previous,
                    Some(Instr::Move {
                        src: Operand::X(2),
                        dst: Operand::X(0)
                    })
                ),
                "the compiler's `try ... catch C:R:S -> S` handler no longer moves the raw \
                 trace from x(2) into x(0) before `build_stacktrace` (function {:?}, pc {pc}: \
                 the preceding instruction is {previous:?}). The engine's \
                 `Instr::BuildStacktrace` reads the raw trace word — and the synthesized \
                 callee head item is published there — from its own operand (x(0)); a \
                 compiler shape change breaks that assumption. Full IR: {:#?}",
                function.name,
                function.code
            );
        }
    }
    assert!(
        handlers > 0,
        "the fixture must compile at least one `build_stacktrace` handler; loaded {handlers}"
    );

    // (2) The compiler's own textual disassembly, so the failure names the
    //     source-level shape rather than only the loaded IR.
    let output = Command::new("erlc")
        .args(["-S", "p3.erl"])
        .current_dir(&dir)
        .output()
        .expect("`erlc -S` (erlc was available for the compile above)");
    assert!(
        output.status.success(),
        "the system `erlc -S` exited {} on p3.erl\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let asm = std::fs::read_to_string(dir.join("p3.S")).expect("`erlc -S` wrote p3.S");
    let collapsed: String = asm.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        collapsed.contains("{move,{x,2},{x,0}}. build_stacktrace."),
        "the compiler's `build_stacktrace` handler shape changed: erlc no longer emits \
         `{{move,{{x,2}},{{x,0}}}}` immediately before `build_stacktrace`.\n  p3.S: {asm}"
    );
}

// ---------------------------------------------------------------------------
// Tier equivalence
// ---------------------------------------------------------------------------

/// The cooked stacktrace must be **identical** on the interpreter tier and the
/// native baseline JIT tier for both failing routes (a `call_ext` and an
/// `apply/3`).
///
/// The JIT's declined-import route hands the host's already-computed answer to the
/// frozen shared `Machine::call_ext_dispatch`, and the dynamic `apply`
/// instruction deopts to the interpreter's own step for the parts it does not
/// inline — so the same failure arm synthesizes the head item on both tiers. This
/// proves it rather than assuming it, and checks both tiers against the oracle.
#[test]
fn the_cooked_callee_head_item_is_identical_on_the_interpreter_and_the_baseline_tier() {
    let Some(dir) = compile_fixture("tiers") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the tier-equivalence test is skipped");
        return;
    };
    let module = load_fixture(&dir);

    // The oracle answers for both shapes (skipped loudly without `erl`).
    let oracle_ext = oracle_trace(&dir, "p3:go_apply()");
    let oracle_dyn = oracle_trace(&dir, "p3:dyn_apply(nonexistent_mod, f, a, b)");
    if oracle_ext.is_none() || oracle_dyn.is_none() {
        eprintln!("SKIP: no system `erl` on PATH -- the tier-equivalence oracle check is skipped");
        return;
    }

    // The `call_ext erlang:apply/3` route: `p3:go_apply/0`.
    let mut interpreter_ctx = undef_host();
    let ext_interp = {
        let (class, reason, cooked) =
            run_fixture(&module, &mut interpreter_ctx, atom(&module, "go_apply"), 0, &[], Tier::Interpreter);
        engine_trace(class, reason, cooked, &module)
    };
    let mut baseline_ctx = undef_host();
    let ext_baseline = {
        let (class, reason, cooked) =
            run_fixture(&module, &mut baseline_ctx, atom(&module, "go_apply"), 0, &[], Tier::BaselineJit);
        engine_trace(class, reason, cooked, &module)
    };
    assert_eq!(
        ext_interp, ext_baseline,
        "the `call_ext erlang:apply/3` failure cooks differently on the two tiers"
    );
    assert_matches_the_oracle(
        "p3:go_apply/0 (both tiers)",
        &ext_interp.0,
        &ext_interp.1,
        oracle_ext.as_ref().expect("checked above"),
    );

    // The BEAM `apply` instruction route: `p3:dyn_apply/4`.
    let dyn_args = [
        make_atom(atom(&module, "nonexistent_mod")),
        make_atom(atom(&module, "f")),
        make_atom(atom(&module, "a")),
        make_atom(atom(&module, "b")),
    ];
    let mut interpreter_ctx = undef_host();
    let dyn_interp = {
        let (class, reason, cooked) = run_fixture(
            &module,
            &mut interpreter_ctx,
            atom(&module, "dyn_apply"),
            4,
            &dyn_args,
            Tier::Interpreter,
        );
        engine_trace(class, reason, cooked, &module)
    };
    let mut baseline_ctx = undef_host();
    let dyn_baseline = {
        let (class, reason, cooked) = run_fixture(
            &module,
            &mut baseline_ctx,
            atom(&module, "dyn_apply"),
            4,
            &dyn_args,
            Tier::BaselineJit,
        );
        engine_trace(class, reason, cooked, &module)
    };
    assert_eq!(
        dyn_interp, dyn_baseline,
        "the dynamic `apply` failure cooks differently on the two tiers"
    );
    assert_matches_the_oracle(
        "p3:dyn_apply/4 (both tiers)",
        &dyn_interp.0,
        &dyn_interp.1,
        oracle_dyn.as_ref().expect("checked above"),
    );
}
