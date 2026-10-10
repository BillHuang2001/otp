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

//! The `fun`-taking `maps` BIFs' **argument guard** (`map/2`, `fold/3`,
//! `filter/2`, `foreach/2`), pinned against the OTP 29 / erts-17.1 oracle.
//!
//! `maps.erl` puts an `is_function(Fun, Arity)` guard on the *first* clause head of
//! each of these functions and falls through to a `badarg_with_info([Fun, …])`
//! clause otherwise, so a **non-fun** (or a fun of the **wrong arity**) is the plain
//! atom `error:badarg` — never the `function_clause` the `lists.erl` guards raise,
//! and never a fabricated success. Because the guard is on the first clause, it
//! fires **before** the map argument is inspected or the map is walked, so a bad fun
//! is `badarg` even for an **empty** map or a **non-map** second argument. A *valid*
//! fun paired with a non-map argument is instead the structured
//! `error:{badmap, Term}`.
//!
//! The behaviour this file pins was probed directly against the oracle; the exact
//! transcript (recording both the exception class and the reason) is:
//!
//! ```text
//! maps:map(5,#{})                                => {error,badarg}
//! maps:map(5,#{a=>1})                            => {error,badarg}
//! maps:map(fun/1,#{})                            => {error,badarg}
//! maps:map(fun/1,#{a=>1})                        => {error,badarg}
//! maps:map(fun/3,#{a=>1})                        => {error,badarg}
//! maps:map(fun/2,not_a_map)                      => {error,{badmap,not_a_map}}
//! maps:map(5,not_a_map)                          => {error,badarg}
//! maps:map(fun/2,#{a=>1})                        => #{a => 1,b => 2}
//! maps:fold(5,0,#{})                             => {error,badarg}
//! maps:fold(5,0,#{a=>1})                         => {error,badarg}
//! maps:fold(fun/2,0,#{})                         => {error,badarg}
//! maps:fold(fun/2,0,#{a=>1})                     => {error,badarg}
//! maps:fold(fun/1,0,#{a=>1})                     => {error,badarg}
//! maps:fold(fun/3,0,not_a_map)                   => {error,{badmap,not_a_map}}
//! maps:fold(5,0,not_a_map)                       => {error,badarg}
//! maps:fold(fun/3,0,#{a=>1})                     => [b,2,a,1]
//! maps:filter(5,#{})                             => {error,badarg}
//! maps:filter(5,#{a=>1})                         => {error,badarg}
//! maps:filter(fun/1,#{})                         => {error,badarg}
//! maps:filter(fun/1,#{a=>1})                     => {error,badarg}
//! maps:filter(fun/2,not_a_map)                   => {error,{badmap,not_a_map}}
//! maps:filter(5,not_a_map)                       => {error,badarg}
//! maps:filter(fun/2,#{a=>1})                     => #{b => 2}
//! maps:foreach(5,#{})                            => {error,badarg}
//! maps:foreach(5,#{a=>1})                        => {error,badarg}
//! maps:foreach(fun/1,#{})                        => {error,badarg}
//! maps:foreach(fun/1,#{a=>1})                    => {error,badarg}
//! maps:foreach(fun/2,not_a_map)                  => {error,{badmap,not_a_map}}
//! maps:foreach(5,not_a_map)                      => {error,badarg}
//! maps:foreach(fun/2,#{a=>1})                    => ok
//! ```
//!
//! Every function/argument combination in that transcript is asserted here against a
//! **literal golden**, and the full sweep (each function × {non-fun, wrong-arity fun,
//! right-arity fun} × {empty map, non-empty map, non-map}) is additionally
//! **cross-checked against the live `erl` oracle** in a single invocation — the
//! cross-check skips loudly when no `erl` is on `PATH`.
//!
//! The fun argument is validated before the map is looked at, so the mock
//! [`FunCaller`] here is only ever reached by a *valid* fun (built by
//! [`real_fun`] as a real `EXPORT_EXT` fun of the requested arity).

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, FunCaller};
use beam_codec::term::{make_fun_external, tuple_element};
use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, make_atom, make_small, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// The outcome vocabulary
// ---------------------------------------------------------------------------

/// The three observable outcomes of a guarded `maps` call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    /// The call succeeded (an empty map needs no fun call; a non-empty map is walked).
    Ok,
    /// The bare `error:badarg` the first clause's `is_function(Fun, Arity)` guard
    /// produces (`BeamError::BadArg`, whose `as_atom()` is `badarg`).
    BadArg,
    /// The structured `error:{badmap, Term}` a *valid* fun meets on a non-map
    /// second/third argument.
    BadMap,
}

/// The fun argument of a probe: either a term that is not a fun at all, or a real
/// `EXPORT_EXT` fun of the given arity.
#[derive(Clone, Copy, Debug)]
enum FunArg {
    NonFun,
    Arity(u32),
}

/// The map argument of a probe.
#[derive(Clone, Copy, Debug)]
enum MapArg {
    Empty,
    NonEmpty,
    NotAMap,
}

/// One `maps` call to exercise: the function, its BIF arity, and its two arguments.
struct Probe {
    function: &'static str,
    /// The BIF arity (`2`, or `3` for `fold/3`).
    arity: u32,
    fun: FunArg,
    map: MapArg,
}

/// The four guarded functions and the fun arity each one requires
/// (`maps.erl:887`/`:932`/`:728`/`:836`).
const FUNCTIONS: [(&str, u32); 4] = [("map", 2), ("fold", 3), ("filter", 2), ("foreach", 2)];

/// The fun arity `function` requires — the `Arity` of `is_function(Fun, Arity)`.
fn good_arity(function: &str) -> u32 {
    FUNCTIONS
        .iter()
        .find(|(name, _)| *name == function)
        .map(|(_, arity)| *arity)
        .expect("a guarded maps function")
}

/// A *wrong* fun arity for `function` — the other arity the tests exercise: `1` for
/// the arity-2 functions, `2` for `fold/3`.
fn wrong_arity(function: &str) -> u32 {
    if good_arity(function) == 3 {
        2
    } else {
        1
    }
}

impl Probe {
    fn new(function: &'static str, fun: FunArg, map: MapArg) -> Probe {
        Probe { function, arity: if function == "fold" { 3 } else { 2 }, fun, map }
    }
}

// ---------------------------------------------------------------------------
// The Rust side
// ---------------------------------------------------------------------------

/// The atoms the reasons and the probes name. `badmap`/`not_a_map` are the
/// structured-reason tag and the bad map argument; `mock`/`f_probe` name the real
/// fun [`real_fun`] builds.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a", "b", "badmap", "not_a_map", "mock", "f_probe"]);
    atoms
}

/// A real `EXPORT_EXT` fun of `arity`, so the BIF's `is_function(Fun, Arity)` guard
/// observes the true arity (a small-integer sentinel would not).
fn real_fun(ctx: &mut BifContext<'_>, arity: u32) -> Eterm {
    let module = atom_val(ctx.atom("mock").expect("seeded"));
    let function = atom_val(ctx.atom("f_probe").expect("seeded"));
    make_fun_external(ctx.heap(), arity as usize, module, function).expect("the probe fun fits")
}

/// The mock `FunCaller`: reached only by a valid fun of the right arity, it answers
/// a `fold/3` call with the accumulator (its 3rd argument) and any 2-argument call
/// with `true` (a boolean, so `filter/2` keeps the pair and `map/2` stores a value).
struct GuardMock {
    truth: Eterm,
}

impl FunCaller for GuardMock {
    fn call_fun(&mut self, _fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        Ok(match args.len() {
            3 => *args.last().expect("a 3-argument call threads the accumulator"),
            _ => self.truth,
        })
    }
}

/// Builds the argument vector the probe describes, on `ctx`'s heap.
fn build_args(reg: &BifRegistry, ctx: &mut BifContext<'_>, probe: &Probe) -> Vec<Eterm> {
    let fun = match probe.fun {
        FunArg::NonFun => make_small(5),
        FunArg::Arity(arity) => real_fun(ctx, arity),
    };
    let map = match probe.map {
        MapArg::Empty => reg.call("maps", "new", 0, ctx, &[]).expect("maps:new/0"),
        MapArg::NonEmpty => {
            let one = ctx.tuple(&[ctx.int(1).expect("fits"), ctx.int(10).expect("fits")]);
            let two = ctx.tuple(&[ctx.int(2).expect("fits"), ctx.int(20).expect("fits")]);
            let list = ctx.list(&[one.expect("fits"), two.expect("fits")]).expect("fits");
            reg.call("maps", "from_list", 1, ctx, &[list]).expect("maps:from_list/1")
        }
        MapArg::NotAMap => ctx.atom("not_a_map").expect("seeded"),
    };
    match probe.function {
        "fold" => vec![fun, make_small(0), map],
        _ => vec![fun, map],
    }
}

/// Classifies a call outcome into the oracle's observable vocabulary.
///
/// `BeamError::BadArg` is the plain `error:badarg` (its `as_atom()` is `badarg`);
/// the only structured reason these calls raise is `{badmap, _}` (`map_pairs`),
/// whose tag atom is checked explicitly so a different structured reason can never
/// pass silently.
fn classify(result: &Result<Eterm>) -> Outcome {
    match result {
        Ok(_) => Outcome::Ok,
        Err(BeamError::BadArg) => Outcome::BadArg,
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, ExceptionClass::Error, "class of {result:?}");
            if raise.reason == ExitReason::atom("badarg") {
                return Outcome::BadArg;
            }
            let live = *raise
                .live_reason()
                .expect("a structured reason carries its live term")
                .downcast_ref::<Eterm>()
                .expect("the payload is the live reason term");
            let tag = tuple_element(live, 0).expect("a tagged reason tuple");
            let badmap = make_atom(seeded_atoms().lookup("badmap").expect("seeded"));
            assert_eq!(tag, badmap, "only {{badmap,_}} is expected here: {result:?}");
            Outcome::BadMap
        }
        other => panic!("unexpected outcome {other:?}"),
    }
}

/// Runs one probe through the real registry and classifies the outcome.
fn run_probe(probe: &Probe) -> Outcome {
    let registry = default_registry();
    let atoms = seeded_atoms();
    // Leaked on purpose: the result terms point into this heap and are inspected
    // after the call returns.
    let heap: &'static mut Heap = Box::leak(Box::new(Heap::new(4096).expect("test heap")));
    let mut mock = GuardMock { truth: make_atom(atoms.lookup("true").expect("seeded")) };
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_fun_caller(&mut mock);
    let args = build_args(&registry, &mut ctx, probe);
    let result = registry.call("maps", probe.function, probe.arity, &mut ctx, &args);
    classify(&result)
}

// ---------------------------------------------------------------------------
// The live-`erl` oracle
// ---------------------------------------------------------------------------

impl MapArg {
    /// The Erlang source of the map argument.
    fn erl(self) -> &'static str {
        match self {
            MapArg::Empty => "#{}",
            MapArg::NonEmpty => "#{a => 1, b => 2}",
            MapArg::NotAMap => "not_a_map",
        }
    }
}

/// The Erlang source of the fun argument at `arity` (`None` is the non-fun `5`).
fn erl_fun(arity: Option<u32>) -> String {
    match arity {
        None => "5".to_string(),
        Some(1) => "fun(_K) -> true end".to_string(),
        Some(2) => "fun(_K,_V) -> true end".to_string(),
        Some(3) => "fun(_K,_V,Acc) -> Acc end".to_string(),
        Some(other) => unreachable!("probes only use fun arities 1..=3, got {other}"),
    }
}

/// The Erlang source of a probe, equivalent to the Rust call [`run_probe`] makes.
fn erl_expr(probe: &Probe) -> String {
    let fun = erl_fun(match probe.fun {
        FunArg::NonFun => None,
        FunArg::Arity(arity) => Some(arity),
    });
    let map = probe.map.erl();
    if probe.function == "fold" {
        format!("maps:fold({fun}, 0, {map})")
    } else {
        format!("maps:{}({fun}, {map})", probe.function)
    }
}

/// Runs **every** probe's equivalent Erlang expression under the system `erl` in one
/// invocation, normalising each result to the [`Outcome`] vocabulary (a successful
/// call is `ok_result`, and a failure is `{Class, ReasonAtomOrTag}`).
///
/// `None` (with a loud message) when the oracle is unavailable, so the cross-check
/// skips rather than fails.
fn oracle_outcomes(probes: &[Probe]) -> Option<Vec<Outcome>> {
    // `Norm` reduces "succeeded" / "raised Class:Reason" to a stable marker, so the
    // comparison never depends on rendering an arbitrary result term.
    let mut statement = String::from(
        "Norm = fun(F) -> try F() of _ -> ok_result catch C:R -> \
         case R of A when is_atom(A) -> {C,A}; \
         T when is_tuple(T), tuple_size(T) >= 1 -> {C, element(1,T)}; \
         _ -> {C, other} end end end,",
    );
    for probe in probes {
        statement.push_str(&format!(" io:format(\"~p~n\", [Norm(fun() -> {} end)]),", erl_expr(probe)));
    }
    statement.push_str(" halt().");

    let output = match Command::new("erl").args(["-noshell", "-eval", &statement]).output() {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            return None;
        }
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    assert_eq!(lines.len(), probes.len(), "the oracle printed {} lines for {} probes", lines.len(), probes.len());
    Some(
        lines
            .into_iter()
            .map(|line| match line.trim() {
                "ok_result" => Outcome::Ok,
                "{error,badarg}" => Outcome::BadArg,
                "{error,badmap}" => Outcome::BadMap,
                other => panic!("unexpected oracle output {other:?}"),
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The transcript at the top of this file, one literal golden per row.
#[test]
fn the_guard_reasons_match_the_oracle_transcript() {
    use MapArg::{Empty, NonEmpty, NotAMap};
    // (function, fun, map, golden) — every expected value is the reason the system
    // `erl` produced (the transcript above), never a re-derivation of the rule.
    let cases: [(&str, FunArg, MapArg, Outcome); 20] = [
        ("map", FunArg::NonFun, Empty, Outcome::BadArg),
        ("map", FunArg::NonFun, NonEmpty, Outcome::BadArg),
        ("map", FunArg::NonFun, NotAMap, Outcome::BadArg),
        ("map", FunArg::Arity(1), Empty, Outcome::BadArg),
        ("map", FunArg::Arity(3), NonEmpty, Outcome::BadArg),
        ("map", FunArg::Arity(2), NotAMap, Outcome::BadMap),
        ("map", FunArg::Arity(2), Empty, Outcome::Ok),
        ("map", FunArg::Arity(2), NonEmpty, Outcome::Ok),
        ("fold", FunArg::NonFun, Empty, Outcome::BadArg),
        ("fold", FunArg::NonFun, NotAMap, Outcome::BadArg),
        ("fold", FunArg::Arity(2), Empty, Outcome::BadArg),
        ("fold", FunArg::Arity(1), NonEmpty, Outcome::BadArg),
        ("fold", FunArg::Arity(3), NotAMap, Outcome::BadMap),
        ("fold", FunArg::Arity(3), Empty, Outcome::Ok),
        ("filter", FunArg::NonFun, Empty, Outcome::BadArg),
        ("filter", FunArg::Arity(1), NonEmpty, Outcome::BadArg),
        ("filter", FunArg::Arity(2), NotAMap, Outcome::BadMap),
        ("filter", FunArg::Arity(2), NonEmpty, Outcome::Ok),
        ("foreach", FunArg::NonFun, NotAMap, Outcome::BadArg),
        ("foreach", FunArg::Arity(2), NotAMap, Outcome::BadMap),
    ];
    for (function, fun, map, expected) in cases {
        let probe = Probe::new(function, fun, map);
        assert_eq!(
            run_probe(&probe),
            expected,
            "maps:{}({}) with a {:?} fun and a {:?} map",
            probe.function,
            probe.arity,
            probe.fun,
            probe.map
        );
    }
}

/// A bad fun is `badarg` **even when the map argument is also bad** — the guard runs
/// first (oracle: `maps:map(5, not_a_map)` ⇒ `error:badarg`, not `{badmap,_}`).
#[test]
fn the_fun_guard_precedes_the_map_argument() {
    for (function, _) in FUNCTIONS {
        let probe = Probe::new(function, FunArg::NonFun, MapArg::NotAMap);
        assert_eq!(run_probe(&probe), Outcome::BadArg, "maps:{function}");
    }
}

/// Every wrong arity (not only the canonical one) is the first clause's `badarg`,
/// even for the **empty** map the fun would never be called on.
#[test]
fn every_wrong_fun_arity_is_badarg_even_for_an_empty_map() {
    for (function, _) in FUNCTIONS {
        let good = good_arity(function);
        let wrongs = [1u32, 2, 3].into_iter().filter(|arity| *arity != good);
        for arity in wrongs {
            let probe = Probe::new(function, FunArg::Arity(arity), MapArg::Empty);
            assert_eq!(
                run_probe(&probe),
                Outcome::BadArg,
                "maps:{function} with an arity-{arity} fun (needs {good}) and an empty map"
            );
        }
    }
}

/// The exhaustive sweep — each function × {non-fun, wrong-arity fun, right-arity fun}
/// × {empty map, non-empty map, non-map} — against the **live oracle**.
///
/// This is the compatibility check: the expectation is the real `erl`'s own outcome,
/// so a divergence between this crate and `maps.erl` (a missing guard, a guard with
/// the wrong reason, or a fabricated success) fails here rather than only in the
/// hand-written goldens above.
#[test]
fn the_fun_guard_matches_the_live_oracle() {
    let mut probes = Vec::new();
    for (function, _) in FUNCTIONS {
        let funs = [
            FunArg::NonFun,
            FunArg::Arity(wrong_arity(function)),
            FunArg::Arity(good_arity(function)),
        ];
        for fun in funs {
            for map in [MapArg::Empty, MapArg::NonEmpty, MapArg::NotAMap] {
                probes.push(Probe::new(function, fun, map));
            }
        }
    }

    let Some(expected) = oracle_outcomes(&probes) else {
        return;
    };

    for (probe, want) in probes.iter().zip(expected) {
        assert_eq!(
            run_probe(probe),
            want,
            "maps:{} differs from the live oracle (fun {:?}, map {:?})",
            probe.function,
            probe.fun,
            probe.map
        );
    }
}
