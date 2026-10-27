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

//! **A propagating exception keeps its ORIGINAL stacktrace.**
//!
//! When a clause of a `try … catch` does not match, the runtime re-raises the
//! exception **unchanged** — including the trace it captured at its original raise
//! site. Real ERTS implements that by copying the exception's saved trace
//! (`c_p->ftrace`) verbatim: `i_raise`
//! (`../../erts/emulator/beam/emu/instrs.tab:1236`) moves `x(1)`/`x(2)` into
//! `fvalue`/`ftrace`, and `erts_sanitize_freason` drops `EXF_SAVETRACE`, so no
//! `save_stacktrace` ever runs at the re-raise site. Only a **new** raise (a `bif`,
//! `call_ext`, `func_info`, …) captures a fresh trace.
//!
//! The engine has no saved trace *object*; it keeps the raw trace as
//! `Machine::raise_snapshot`, a `(context, func, pc)` chain re-derived from the
//! **live** frame chain at the raise site ([`Machine::throw_with_trace`]). Because
//! the first handler has already unwound the raising frame by the time it re-raises,
//! re-deriving there would drop exactly that frame — the defect these tests pin. So
//! the propagation path ([`Machine::throw_propagating`], reached only from
//! [`Instr::Raise`]) preserves the snapshot instead. `Instr::Raise` is the whole
//! propagation surface: the compiler lowers both the implicit re-raise finishing a
//! `try … catch` and `erlang:raise/3` in the caught-clause position to `{bif,raise,
//! {f,0},[Trace,Value],{x,0}}` (`beam_ssa_codegen.erl:2471`, the
//! `primop(raise, …) -> primop_succeeded(resume, …)` fold of
//! `beam_core_to_ssa.erl:491`).
//!
//! # The measured oracle rule (OTP 29.1.2 / erts-17.1.1)
//!
//! | shape | oracle |
//! |---|---|
//! | non-matching clause, implicit re-raise | the raising frame is **kept** |
//! | `erlang:raise(C,R,S)` in the caught clause | the original chain is **kept** |
//! | matching clause raising a **new** error | a **brand-new** trace |
//! | `throw` across the same path | the throw site's own frame, then its callers |
//! | `catch Expr` rather than `try` | unchanged |
//! | three levels deep | preserved verbatim, never grows |
//! | `erlang:raise(error,R,[])` | the trace is **replaced** by `[]` |
//! | propagated `undef` / `function_clause` | the synthesized head item is kept |
//!
//! Every line of that table is pinned below on a real `erlc`-compiled fixture and
//! cross-checked against the running system (skip loudly without `erlc`/`erl`), on
//! **both** the interpreter and the native baseline tier — the trace is observable
//! state, so a tier divergence would be a silent bug.
//!
//! The trailing `erl_eval`/`init` callers the oracle appends live outside this
//! engine's run, so each comparison is the leading-prefix one the sibling
//! [`super::oracle`] harness defines.

use std::path::{Path, PathBuf};

use beam_common::{BeamError, ExitReason, Raise, StackFrame};
use beam_loader::Module;
use beam_term::atom_val;

use crate::jit::Tier;

use super::oracle::{
    assert_matches_the_oracle, atom, compile_module, engine_trace, load_module, oracle_trace,
    run_fixture, OracleTrace,
};
use super::*;

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// The fixture module, compiled by the system `erlc`.
///
/// Each `through_*` entry wraps one raise in a `try … catch` whose clause does
/// **not** match (so the runtime re-raises it unchanged) inside a second `try`
/// whose clause returns the cooked `{Reason, Stacktrace}` pair — the shape the
/// defect lost the raising frame from. The remaining entries isolate the rules the
/// table above states: a matching clause raising a *new* error, `throw`, `catch`,
/// three nesting levels, an explicit empty trace, and the two raises whose head item
/// is synthesized at the raise site (`undef` from an unresolved `call_ext`,
/// `function_clause` from a failed clause head).
///
/// The fixture is source-of-truth for the pinned `{line,N}` values: both sides of
/// every comparison compile *this* text, so the numbers cannot drift apart.
const FIXTURE_SRC: &str = r#"-module(prop).

-export([through/0,
         through_raise3/0,
         through_throw/0,
         handler_raises_new/0,
         through_catch/0,
         three_deep/0,
         raise3_empty_trace/0,
         through_undef/0,
         through_function_clause/0,
         uncaught/0]).

boom() ->
    erlang:error(boom).

mid() ->
    try
        boom()
    catch
        error:undef ->
            nope
    end.

through() ->
    try
        mid()
    catch
        error:R:S ->
            {error, R, S}
    end.

mid_raise3() ->
    try
        boom()
    catch
        error:R:S ->
            erlang:raise(error, R, S)
    end.

through_raise3() ->
    try
        mid_raise3()
    catch
        error:R:S ->
            {error, R, S}
    end.

mid_throw() ->
    try
        throw(bad_throw)
    catch
        error:undef ->
            nope
    end.

through_throw() ->
    try
        mid_throw()
    catch
        throw:R:S ->
            {throw, R, S}
    end.

mid_new() ->
    try
        boom()
    catch
        error:_ ->
            erlang:error(other)
    end.

handler_raises_new() ->
    try
        mid_new()
    catch
        error:R:S ->
            {error, R, S}
    end.

through_catch() ->
    catch mid().

deep1() ->
    erlang:error(deep).

deep2() ->
    try
        deep1()
    catch
        error:undef ->
            x
    end.

deep3() ->
    try
        deep2()
    catch
        error:undef ->
            y
    end.

three_deep() ->
    try
        deep3()
    catch
        error:R:S ->
            {error, R, S}
    end.

mid_empty() ->
    try
        boom()
    catch
        error:R:_ ->
            erlang:raise(error, R, [])
    end.

raise3_empty_trace() ->
    try
        mid_empty()
    catch
        error:R:S ->
            {error, R, S}
    end.

undef_mid() ->
    try
        nonexistent_mod:foo(1)
    catch
        error:other ->
            nope
    end.

through_undef() ->
    try
        undef_mid()
    catch
        error:R:S ->
            {error, R, S}
    end.

failing_clause(1) ->
    ok.

clause_mid() ->
    try
        failing_clause(2)
    catch
        error:other ->
            nope
    end.

through_function_clause() ->
    try
        clause_mid()
    catch
        error:R:S ->
            {error, R, S}
    end.

uncaught() ->
    try
        mid()
    catch
        error:undef ->
            nope
    end.
"#;

/// The atoms the runtime holds globally but this module's own atom chunk does not
/// carry: `function_clause` is the reason `Instr::FuncInfo` raises, and `EXIT` is the
/// tag `catch_end` builds, so seeding both makes `through_function_clause/0` and
/// `through_catch/0` land (the sibling [`super::oracle`] harness's rule).
const EXTRA_SEED: &[&str] = &["function_clause", "EXIT"];

/// Compile [`FIXTURE_SRC`] into a per-`tag` temp dir; `None` when no runnable
/// `erlc` is on `PATH` (the caller skips loudly).
fn compile_fixture(tag: &str) -> Option<PathBuf> {
    compile_module(tag, "prop", FIXTURE_SRC)
}

/// Load `prop.beam` and intern the globally held atoms.
fn load_fixture(dir: &Path) -> Module {
    load_module(dir, "prop", EXTRA_SEED)
}

/// A `MinimalContext` whose BIF hook answers the fixture's two raise sites and
/// rejects everything else with the engine's typed `unsupported` — so a stray call
/// fails loudly instead of making a wrong test pass.
///
/// `erlang:error/1` is the reason **atom** the fixture raises (`boom`, `other`,
/// `deep`); the hook resolves it against the atoms it captured from the module so
/// the engine sees the same `ExitReason` the real BIF produces.
fn propagation_host(module: &Module) -> MinimalContext {
    let boom = atom(module, "boom");
    let other = atom(module, "other");
    let deep = atom(module, "deep");
    MinimalContext::with_bif_hook(Box::new(move |module, function, arity, args| {
        if let ("erlang", "error", 1) = (module, function, arity) {
            let reason = atom_val(args[0]);
            let name = if reason == boom {
                "boom"
            } else if reason == other {
                "other"
            } else if reason == deep {
                "deep"
            } else {
                return Err(crate::error::unsupported("erlang:error/1 with an unexpected reason"));
            };
            return Err(BeamError::Raise(Raise::error(ExitReason::atom(name))));
        }
        if let ("nonexistent_mod", "foo", 1) = (module, function, arity) {
            return Err(BeamError::Raise(Raise::error(ExitReason::atom("undef"))));
        }
        // `throw(X)` reaches the runtime as a BIF call carrying the `throw` class.
        if let ("erlang", "throw", 1) = (module, function, arity) {
            return Err(BeamError::Raise(Raise::throw(ExitReason::atom("bad_throw"))));
        }
        Err(crate::error::unsupported(&format!(
            "bif {module}:{function}/{arity}"
        )))
    }))
}

// ---------------------------------------------------------------------------
// Driving one fixture entry on both tiers, against the oracle
// ---------------------------------------------------------------------------

/// Run `prop:name/0` — a `{Class,Reason,Stacktrace}`-returning entry — on the
/// interpreter **and** the native baseline tier, assert the two agree, and assert
/// the result is the oracle's `{Class,Reason,S}` with the engine's cooked list as a
/// leading prefix. `None` when the toolchain is absent (already reported loudly).
fn check_entry(dir: &Path, module: &Module, name: &str) -> Option<(String, Vec<String>)> {
    let oracle = oracle_trace(dir, &format!("prop:{name}()"));
    let engine = {
        let mut ctx = propagation_host(module);
        let (class, reason, cooked) = run_fixture(
            module,
            &mut ctx,
            atom(module, name),
            0,
            &[],
            Tier::Interpreter,
        );
        engine_trace(class, reason, cooked, module)
    };
    let baseline = {
        let mut ctx = propagation_host(module);
        let (class, reason, cooked) = run_fixture(
            module,
            &mut ctx,
            atom(module, name),
            0,
            &[],
            Tier::BaselineJit,
        );
        engine_trace(class, reason, cooked, module)
    };
    assert_eq!(
        engine, baseline,
        "prop:{name}/0 cooks a different stacktrace on the interpreter and the baseline tier"
    );
    let Some(oracle) = oracle else {
        eprintln!("SKIP: no system `erl` on PATH -- prop:{name}/0 is not oracle-checked");
        return Some(engine);
    };
    assert_matches_the_oracle(&format!("prop:{name}/0"), &engine.0, &engine.1, &oracle);
    Some(engine)
}

/// The cooked items the engine produced for `prop:name/0`, panicking if the
/// toolchain is absent (the tests that use it must observe a result).
fn items_of(dir: &Path, module: &Module, name: &str) -> Vec<String> {
    check_entry(dir, module, name)
        .unwrap_or_else(|| panic!("the system `erlc`/`erl` are required for the {name} case"))
        .1
}

// ---------------------------------------------------------------------------
// The `catch Expr` shape, whose answer is `{'EXIT',{Reason,Frames}}`
// ---------------------------------------------------------------------------

/// Ask the system `erl` for `catch call`'s `{'EXIT',{Reason,S}}` and render it like
/// [`oracle_trace`] does, with `'EXIT'` standing in for the class field.
fn oracle_catch_trace(dir: &Path, call: &str) -> Option<OracleTrace> {
    let probe = format!(
        "{{'EXIT',{{R,S}}}} = (catch {call}), \
         io:format(\"~p|~p~n\",['EXIT',R]), \
         lists:foreach(fun(I) -> \
             io:format(\"~p|~p|~p|~p~n\", \
                 [element(1,I),element(2,I),element(3,I),element(4,I)]) end, S), \
         halt(0)."
    );
    let output = std::process::Command::new("erl")
        .args(["-noshell", "-pz"])
        .arg(dir)
        .args(["-eval", &probe])
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {} for `catch {call}`\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut lines = text.lines().map(str::trim_end).filter(|line| !line.is_empty());
    let head = lines.next().expect("the oracle probe prints an EXIT|reason line").to_string();
    Some(OracleTrace {
        head,
        items: lines.map(str::to_string).collect(),
    })
}

/// `prop:through_catch/0` returns `{'EXIT',{Reason,S}}` (a `catch` expression, not a
/// `try`), so it needs its own driver: unpack the pair, render it the way
/// [`oracle_catch_trace`] does, and assert both tiers agree and match the oracle.
#[test]
fn a_catch_expression_propagates_the_original_trace() {
    let Some(dir) = compile_fixture("catch") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the `catch` propagation test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let oracle = oracle_catch_trace(&dir, "prop:through_catch()");

    let engine = |tier: Tier| {
        let mut ctx = propagation_host(&module);
        let mut machine = Machine::new();
        let result = machine
            .run_with_tier(&module, atom(&module, "through_catch"), 0, &[], &mut ctx, tier)
            .unwrap_or_else(|error| panic!("running prop:through_catch/0 on {tier:?}: {error:?}"));
        assert_eq!(
            crate::terms::tuple_arity(result).expect("`catch` builds a 2-tuple"),
            2,
            "`catch mid()` answers `{{'EXIT',{{Reason,S}}}}`"
        );
        let tag = crate::terms::tuple_element(result, 0).expect("the `'EXIT'` tag");
        assert_eq!(
            super::oracle::render_atom(tag, &module),
            "EXIT",
            "a `catch` of an error is tagged `'EXIT'`"
        );
        let pair = crate::terms::tuple_element(result, 1).expect("the `{{Reason,S}}` pair");
        let reason = crate::terms::tuple_element(pair, 0).expect("the reason");
        let cooked = crate::terms::tuple_element(pair, 1).expect("the cooked stacktrace");
        engine_trace(make_atom(atom(&module, "EXIT")), reason, cooked, &module)
    };
    let interp = engine(Tier::Interpreter);
    let baseline = engine(Tier::BaselineJit);
    assert_eq!(
        interp, baseline,
        "prop:through_catch/0 cooks a different stacktrace on the two tiers"
    );
    // `engine_trace` renders the class field as the bare atom `EXIT` (an atom never
    // needs quoting in these fixtures); the oracle probe prints it as `'EXIT'`.
    let head = interp.0.replacen("EXIT|", "'EXIT'|", 1);
    let Some(oracle) = oracle else {
        eprintln!("SKIP: no system `erl` on PATH -- the `catch` propagation test is not oracle-checked");
        return;
    };
    assert_matches_the_oracle("prop:through_catch/0", &head, &interp.1, &oracle);
}

// ---------------------------------------------------------------------------
// The rule: a propagation keeps the original trace
// ---------------------------------------------------------------------------

/// The headline case: `boom/0` raises, `mid/0`'s `catch error:undef` clause does not
/// match (so the runtime re-raises the exception unchanged), and `through/0`'s
/// handler cooks it. **The `{prop,boom,0,…}` frame survives**, at the line the
/// oracle reports for the raise site — before the fix the cooked chain started at
/// `{prop,mid,0,…}`, because `mid/0` had already unwound `boom/0`'s frame.
#[test]
fn passing_through_a_non_matching_clause_keeps_the_raising_frame() {
    let Some(dir) = compile_fixture("through") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the propagation test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let items = items_of(&dir, &module, "through");
    assert_eq!(
        items[0],
        r#"prop|boom|0|[{file,"prop.erl"},{line,15}]"#,
        "the raising frame is the head of the cooked trace, at the oracle's line"
    );
    assert_eq!(
        items[1],
        r#"prop|mid|0|[{file,"prop.erl"},{line,19}]"#,
        "the pass-through function's frame is next, at its call site's line"
    );
    assert_eq!(
        items[2],
        r#"prop|through|0|[{file,"prop.erl"},{line,27}]"#,
        "then the handler's own frame"
    );
    assert_eq!(items.len(), 3, "no frame is invented and none is lost: {items:#?}");
}

/// The uncaught half of the same path: the propagated exception escapes
/// `uncaught/0`, and [`Machine::with_uncaught_frames`] must render the **preserved**
/// chain (the raising frame first) rather than re-deriving it from the live frames
/// — of which only the two callers are left by then.
///
/// Cross-checked against the oracle's `catch` answer for the same entry, whose
/// frames are the same three; the engine's symbolic list is rendered from its
/// [`StackFrame`] metadata, the oracle's from its cooked terms.
#[test]
fn an_uncaught_propagation_renders_the_original_trace() {
    let Some(dir) = compile_fixture("uncaught") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the uncaught propagation test is skipped");
        return;
    };
    let module = load_fixture(&dir);

    let engine = |tier: Tier| -> Vec<String> {
        let mut ctx = propagation_host(&module);
        let mut machine = Machine::new();
        let error = machine
            .run_with_tier(&module, atom(&module, "uncaught"), 0, &[], &mut ctx, tier)
            .expect_err("`uncaught/0` lets the exception escape");
        let BeamError::Raise(raise) = error else {
            panic!("an escaping exception is a `Raise`, got {error:?}");
        };
        raise.stacktrace.frames.iter().map(render_frame).collect()
    };
    let interp = engine(Tier::Interpreter);
    let baseline = engine(Tier::BaselineJit);
    assert_eq!(
        interp, baseline,
        "the uncaught trace differs between the interpreter and the baseline tier"
    );
    assert_eq!(
        interp,
        vec![
            r#"prop|boom|0|[{file,"prop.erl"},{line,15}]"#.to_string(),
            r#"prop|mid|0|[{file,"prop.erl"},{line,19}]"#.to_string(),
            r#"prop|uncaught|0|[{file,"prop.erl"},{line,164}]"#.to_string(),
        ],
        "the uncaught trace must keep the raising frame"
    );

    if let Some(oracle) = oracle_catch_trace(&dir, "prop:uncaught()") {
        assert_matches_the_oracle("prop:uncaught/0 (uncaught)", &oracle.head, &interp, &oracle);
    } else {
        eprintln!("SKIP: no system `erl` on PATH -- the uncaught trace is not oracle-checked");
    }
}

/// A re-raise through `erlang:raise/3` in the caught-clause position keeps the
/// **original** chain too: the compiler folds it into `Instr::RawRaise`, which hands
/// the exception's own trace back — so no re-raise-site frame is prepended and none
/// of the original frames is lost.
///
/// (The unfoldable shape — `erlang:raise(C,R,S)` in a helper, i.e. a real
/// `call_ext erlang:raise/3` with the cooked trace — was measured against the oracle
/// on both runtimes and agrees as well; it needs the full `beam_bifs` BIF, so it is
/// covered by `tests/raise_with_trace.rs` rather than by this `MinimalContext`
/// fixture.)
#[test]
fn an_erlang_raise_re_raise_keeps_the_original_chain() {
    let Some(dir) = compile_fixture("raise3") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the re-raise test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let items = items_of(&dir, &module, "through_raise3");
    assert_eq!(
        items[0],
        r#"prop|boom|0|[{file,"prop.erl"},{line,15}]"#,
        "the original raising frame is kept across `erlang:raise/3`"
    );
    assert_eq!(
        items[1],
        r#"prop|mid_raise3|0|[{file,"prop.erl"},{line,35}]"#,
        "the re-raising handler's frame is next, once — no re-raise-site duplicate"
    );
    assert_eq!(items.len(), 3, "the chain is neither grown nor shrunk: {items:#?}");
}

/// A **matching** clause that raises a *new* error starts a **new** trace: only the
/// handler's own frame and its callers, never the original raise site.
#[test]
fn a_handler_that_raises_a_new_error_starts_a_new_trace() {
    let Some(dir) = compile_fixture("new") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the new-raise test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let items = items_of(&dir, &module, "handler_raises_new");
    assert_eq!(
        items[0],
        r#"prop|mid_new|0|[{file,"prop.erl"},{line,70}]"#,
        "the new raise's own site is the head of the new trace"
    );
    assert!(
        !items.iter().any(|item| item.contains("|boom|0|")),
        "the abandoned raise's frame must NOT appear in the new trace: {items:#?}"
    );
    assert_eq!(items.len(), 2, "the new trace is the handler's chain: {items:#?}");
}

/// `throw` across the same pass-through path: the throw site has no separate callee
/// frame (nothing was called), and the propagation preserves the two frames it
/// captured.
#[test]
fn a_throw_across_the_same_path_keeps_its_frames() {
    let Some(dir) = compile_fixture("throw") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the throw test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let items = items_of(&dir, &module, "through_throw");
    assert_eq!(
        items[0],
        r#"prop|mid_throw|0|[{file,"prop.erl"},{line,51}]"#,
        "the throw site's frame is kept across the propagation"
    );
    assert_eq!(items.len(), 2, "the chain is exactly the two frames: {items:#?}");
}

/// Three nested pass-throughs preserve the chain **verbatim**: it never grows (no
/// frame per pass-through) and never loses the raising frame.
#[test]
fn three_levels_deep_keep_the_whole_chain() {
    let Some(dir) = compile_fixture("deep") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the deep propagation test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let items = items_of(&dir, &module, "three_deep");
    assert_eq!(
        items,
        vec![
            r#"prop|deep1|0|[{file,"prop.erl"},{line,85}]"#.to_string(),
            r#"prop|deep2|0|[{file,"prop.erl"},{line,89}]"#.to_string(),
            r#"prop|deep3|0|[{file,"prop.erl"},{line,97}]"#.to_string(),
            r#"prop|three_deep|0|[{file,"prop.erl"},{line,105}]"#.to_string(),
        ],
        "two pass-throughs must not grow or shrink the chain"
    );
}

/// A propagated `undef` keeps the **synthesized callee head item** the raise site
/// built ([`super::callee`]'s `{Module,Function,ArgList,Loc}`), which is derived from
/// the failing instruction — so it must survive a propagation that drops the frame
/// it was synthesized for.
#[test]
fn a_propagated_undef_keeps_its_callee_head_item() {
    let Some(dir) = compile_fixture("undef") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the propagated-undef test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let items = items_of(&dir, &module, "through_undef");
    assert_eq!(
        items[0],
        "nonexistent_mod|foo|[1]|[]",
        "the synthesized callee head item survives the propagation"
    );
    assert_eq!(
        items[1],
        r#"prop|undef_mid|0|[{file,"prop.erl"},{line,129}]"#,
        "the failing call's own frame is next"
    );
}

/// A propagated `function_clause` keeps the failed clause's **argument list**
/// (`{prop,failing_clause,[2],…}`), which rides the same one-shot latch as the
/// `undef` head item and belongs to the head frame only.
#[test]
fn a_propagated_function_clause_keeps_its_argument_list() {
    let Some(dir) = compile_fixture("clause") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the propagated-function_clause test is skipped");
        return;
    };
    let module = load_fixture(&dir);
    let items = items_of(&dir, &module, "through_function_clause");
    assert_eq!(
        items[0],
        r#"prop|failing_clause|[2]|[{file,"prop.erl"},{line,143}]"#,
        "the failed clause's arguments and location survive the propagation"
    );
}

/// Render one symbolic [`StackFrame`] the way the oracle probe's `~p` prints its
/// four fields (the engine's own cooked list uses the same text).
fn render_frame(frame: &StackFrame) -> String {
    let location = match &frame.location {
        Some(source) => format!(r#"[{{file,"{}"}},{{line,{}}}]"#, source.file, source.line),
        None => "[]".to_string(),
    };
    format!("{}|{}|{}|{}", frame.module, frame.function, frame.arity, location)
}
