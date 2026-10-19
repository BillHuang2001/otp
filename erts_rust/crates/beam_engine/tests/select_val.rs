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

//! **`select_val`, end to end: both tiers against the live `erl`, on a real
//! `erlc`-compiled module.**
//!
//! `Instr::SelectVal` is the one instruction whose per-alternative decision was
//! rewritten into the cost-ordered **pair ladder** of `src/interpreter/select.rs`
//! (unboxed immediate word -> word identity -> either-word-immediate non-match ->
//! the general `beam_term::cmp`), and the ladder's `constant_word` is the single
//! definition both native tiers' `select_val` plans share. This suite is that
//! change's **integration** gate; the ladder's own unit tests
//! (`src/interpreter/select.rs`) pin the adversarial battery (float vs int, two
//! equal bignums, tag collisions, `[]` vs `'[]'`) on hand-built operands, and
//! `src/bench/mod.rs::every_bstr_select_val_pair_is_decided_without_a_general_compare`
//! is the deterministic per-pair tally gate.
//!
//! # What it pins, in three independent claims
//!
//! 1. **The oracle.** Every case's result is the value the system `erl` prints for
//!    the same call -- one batched `erl -noshell` invocation for the whole matrix
//!    (fixtures directory appended with `-pz`, never `-pa`: it holds stand-in
//!    modules that would shadow the real stdlib once prepended). The expected
//!    strings below are also pinned literally, so the suite still asserts the right
//!    answers when no `erl` can be spawned at all (it then says so loudly).
//! 2. **Tier equivalence.** The interpreter and the native baseline tier run the
//!    same loaded module through [`Machine::run_with_tier`] and must produce the
//!    **same normalized value** (a small int's value, an atom's name) -- the native
//!    tier inlines a `select_val` into its own word-compare block, so the two
//!    routes to the same alternative are structurally different code.
//! 3. **The ladder materializes nothing.** A `select_val` over immediate operands
//!    (small ints, atoms, `[]`) builds **no** term: the interpreter answers every
//!    pair with a raw-word compare, so the run makes **zero** `alloc_words` calls
//!    through the host's allocation door. That count is the externally observable
//!    consequence of the ladder -- before it, every alternative of `ints/1` and
//!    `atoms/1` was materialized through `read_operand` (a small-int `Operand`
//!    went through `terms::make_integer_unrooted`), so the same run allocated once
//!    per scanned pair.
//!
//! # Fixture
//!
//! `tests/fixtures/selval.beam`, compiled from `tests/fixtures/selval.erl` by the
//! system `erlc`. Three functions, each one `case` over a fresh set of alternatives:
//! `ints/1` (six small-int clauses), `atoms/1` (four atom clauses), `mixed/1`
//! (atoms, small ints -- negative included -- and `[]` in **one** switch, so the
//! candidate set mixes encodings a word compare must keep apart). Every function
//! ends in a catch-all clause, so a scrutinee that matches nothing exercises the
//! `fail` branch (and the mixed-encoding mismatches exercise rungs 1-3 for real:
//! a small-int candidate against an atom scrutinee, `[]` against a non-nil list,
//! a float or bignum scrutinee against a small-int candidate).

use std::process::Command;

use beam_common::Result;
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext, Tier};
use beam_loader::{load_file, Module};
use beam_term::{atom_val, is_atom, is_small, make_atom, make_small, signed_value, Eterm, NIL};

/// The committed oracle fixture (produced by `erlc selval.erl`).
const SELVAL_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/selval.beam");

// ---------------------------------------------------------------------------
// The host: a `MinimalContext` that counts the engine's allocations
// ---------------------------------------------------------------------------

/// The engine's heap door, counted.
///
/// `selval` dispatches no BIF at all (its bodies are a `select_val`, a `move` and a
/// `return`), so the only host traffic a run can produce is term construction --
/// which is exactly what claim 3 measures. Both rooted and unrooted allocations
/// land here: `ExecutionContext::alloc_words_rooted`'s default body forwards to
/// `alloc_words`, and this wrapper does not override it.
struct CountingHost {
    /// The arena every term of a run lives on (kept alive with the term).
    arena: MinimalContext,
    /// `alloc_words` calls made since the last [`CountingHost::reset_allocations`].
    allocations: usize,
}

impl CountingHost {
    fn new() -> CountingHost {
        CountingHost {
            arena: MinimalContext::new(),
            allocations: 0,
        }
    }

    fn reset_allocations(&mut self) {
        self.allocations = 0;
    }
}

impl ExecutionContext for CountingHost {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        // `selval` reaches no BIF; the unfiltered delegation keeps a surprise loud (the
        // arena hook answers the typed `unsupported`, which the run surfaces) instead of
        // failing silently with a fabricated value.
        self.arena.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.allocations += 1;
        self.arena.alloc_words(n)
    }
}

// ---------------------------------------------------------------------------
// Arguments and the normalized result value
// ---------------------------------------------------------------------------

/// One case's single argument, built on the run's own arena.
#[derive(Clone, Copy, Debug)]
enum Arg {
    /// A small integer.
    Int(i64),
    /// A small integer outside the 60-bit immediate range, i.e. a **bignum** term
    /// (the one `Operand::Integer` shape the ladder must *not* treat as an
    /// immediate, so it falls to the general comparison).
    BigInt(i64),
    /// An atom, by name (must be interned in the fixture's table).
    Atom(&'static str),
    /// The empty list `[]`.
    Nil,
    /// A proper, non-empty list of small integers.
    List(&'static [i64]),
    /// A float -- `=:=` to no small-int alternative, however close the value is.
    Float(f64),
}

/// A case's result, normalized to something comparable across tiers and to the
/// oracle's own text: an atom's **name** or a small integer's **value**.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Value {
    Atom(String),
    Small(i64),
}

impl Value {
    /// The result's `~p` text, so a case's expectation reads exactly as the
    /// oracle's own line does (`three`, `1`, …).
    fn render(&self) -> String {
        match self {
            Value::Atom(name) => name.clone(),
            Value::Small(value) => value.to_string(),
        }
    }
}

/// One row of the matrix: an export, its argument, and the value the fixture's
/// `case` selects for it.
struct Case {
    function: &'static str,
    arg: Arg,
    expected: &'static str,
}

/// The fixture's exports, in the order the matrix drives them.
const CASES: &[Case] = &[
    // `ints/1` -- six small-int alternatives, all six hits plus the misses.
    Case { function: "ints", arg: Arg::Int(0), expected: "zero" },
    Case { function: "ints", arg: Arg::Int(1), expected: "one" },
    Case { function: "ints", arg: Arg::Int(2), expected: "two" },
    Case { function: "ints", arg: Arg::Int(3), expected: "three" },
    Case { function: "ints", arg: Arg::Int(4), expected: "four" },
    Case { function: "ints", arg: Arg::Int(5), expected: "five" },
    // the catch-all: a small int past the last alternative, a negative value, a float
    // (`=:=`, so `1.0` is NOT `1`) and a bignum (materialized, and not `=:=` to any).
    Case { function: "ints", arg: Arg::Int(6), expected: "many" },
    Case { function: "ints", arg: Arg::Int(-1), expected: "many" },
    Case { function: "ints", arg: Arg::Float(1.0), expected: "many" },
    Case { function: "ints", arg: Arg::BigInt(i64::MAX), expected: "many" },
    // `atoms/1` -- four atom alternatives.
    Case { function: "atoms", arg: Arg::Atom("alpha"), expected: "1" },
    Case { function: "atoms", arg: Arg::Atom("beta"), expected: "2" },
    Case { function: "atoms", arg: Arg::Atom("gamma"), expected: "3" },
    Case { function: "atoms", arg: Arg::Atom("delta"), expected: "4" },
    // the catch-all: an atom the switch does not name (the fixture's own `other`), a
    // small int and `[]` (`[]` is NOT the atom `'[]'`).
    Case { function: "atoms", arg: Arg::Atom("other"), expected: "other" },
    Case { function: "atoms", arg: Arg::Int(1), expected: "other" },
    Case { function: "atoms", arg: Arg::Nil, expected: "other" },
    // `mixed/1` -- atoms, small ints (negative included) and `[]` in one switch.
    Case { function: "mixed", arg: Arg::Atom("alpha"), expected: "a" },
    Case { function: "mixed", arg: Arg::Atom("beta"), expected: "b" },
    Case { function: "mixed", arg: Arg::Int(0), expected: "zero" },
    Case { function: "mixed", arg: Arg::Int(1), expected: "one" },
    Case { function: "mixed", arg: Arg::Int(-1), expected: "minus_one" },
    Case { function: "mixed", arg: Arg::Nil, expected: "empty" },
    // the catch-all: an atom outside the switch, a non-nil list (`[]`'s `NIL` word is
    // not a cons word), a float and a small int the switch does not name.
    Case { function: "mixed", arg: Arg::Atom("gamma"), expected: "other" },
    Case { function: "mixed", arg: Arg::List(&[1, 2]), expected: "other" },
    Case { function: "mixed", arg: Arg::Float(1.0), expected: "other" },
    Case { function: "mixed", arg: Arg::Int(42), expected: "other" },
];

/// The functions the zero-allocation claim is asserted for: their switches are made
/// of **immediate** alternatives only (`ints/1` small ints, `atoms/1` atoms), so the
/// ladder decides every pair by raw-word compare and no candidate term can exist.
const IMMEDIATE_ONLY_FUNCTIONS: [&str; 2] = ["ints", "atoms"];

// ---------------------------------------------------------------------------
// The two-tier driver
// ---------------------------------------------------------------------------

/// One tier's run of one case.
struct TierRun {
    /// The normalized result value (kept after the arena is gone).
    value: Value,
    /// `alloc_words` calls the run made through the host's heap door.
    allocations: usize,
}

/// Loads the committed fixture.
fn module() -> Module {
    load_file(SELVAL_BEAM).unwrap_or_else(|error| panic!("load {SELVAL_BEAM}: {error:?}"))
}

/// Builds one case's argument on `host`'s arena.
fn build_arg(module: &Module, host: &mut CountingHost, arg: Arg) -> Eterm {
    match arg {
        Arg::Int(value) => make_small(value),
        // An out-of-range `Integer` operand is a bignum: `make_integer` builds the boxed
        // term through the arena, which is the shape the ladder's `constant_word`
        // deliberately declines.
        Arg::BigInt(value) => terms::make_integer(host, value).expect("a bignum argument"),
        Arg::Atom(name) => {
            let atom = module
                .atom_table
                .lookup(name)
                .unwrap_or_else(|| panic!("`selval` does not intern the atom `{name}`"));
            make_atom(atom)
        }
        Arg::Nil => NIL,
        Arg::List(items) => {
            let elems: Vec<Eterm> = items.iter().copied().map(make_small).collect();
            terms::make_list(host, &elems).expect("a proper list argument")
        }
        Arg::Float(value) => terms::make_float(host, value).expect("a float argument"),
    }
}

/// Runs `case.function/1` on `tier` over a fresh host, returning the normalized
/// result and that run's allocation count.
///
/// The allocation count is read **after** the run and covers the run only: the
/// argument is built first, and the count reset immediately before the tier entry.
fn run_tier(module: &Module, case: &Case, tier: Tier) -> TierRun {
    let mut host = CountingHost::new();
    let argument = build_arg(module, &mut host, case.arg);
    let function = module
        .atom_table
        .lookup(case.function)
        .unwrap_or_else(|| panic!("`selval` does not intern `{}`", case.function));
    assert!(
        module.export(function, 1).is_some(),
        "{}: `selval:{} /1` is not exported",
        tier.name(),
        case.function
    );

    host.reset_allocations();
    let mut machine = Machine::new();
    let result = machine.run_with_tier(
        module,
        function,
        1,
        &[argument],
        &mut host,
        tier,
    );
    let allocations = host.allocations;
    let term = result.unwrap_or_else(|error| {
        panic!(
            "{}: `selval:{}({:?})` failed: {error:?}",
            tier.name(),
            case.function,
            case.arg
        )
    });
    let value = normalize(module, term);
    // `host` (and with it the arena the result term lives on) is dropped here, so the
    // normalized value above must own everything it reports — which it does.
    TierRun { value, allocations }
}

/// Normalizes a result term to an atom's name or a small integer's value; any other
/// kind is a loud failure (every case in `CASES` returns one of those two).
fn normalize(module: &Module, term: Eterm) -> Value {
    if is_small(term) {
        return Value::Small(signed_value(term));
    }
    if is_atom(term) {
        let name = module
            .atom_name(atom_val(term))
            .unwrap_or_else(|| panic!("the module cannot name the result atom"));
        return Value::Atom(name.to_string());
    }
    panic!("`selval` returned a term that is neither a small int nor an atom: {term:?}");
}

// ---------------------------------------------------------------------------
// The live oracle
// ---------------------------------------------------------------------------

/// The `erl` expression that prints every case's result, one `~p` per line.
///
/// One `erl` boot for the whole matrix: the same calls, in the same order as
/// [`CASES`], so the comparison is line-for-line.
fn oracle_expr() -> String {
    let calls: Vec<String> = CASES
        .iter()
        .map(|case| {
            let arg = match case.arg {
                Arg::Int(value) => value.to_string(),
                Arg::BigInt(value) => value.to_string(),
                Arg::Atom(name) => name.to_string(),
                Arg::Nil => "[]".to_string(),
                Arg::List(items) => {
                    let items: Vec<String> = items.iter().map(i64::to_string).collect();
                    format!("[{}]", items.join(","))
                }
                Arg::Float(value) => format!("{value:?}"),
            };
            format!("fun() -> selval:{}({arg}) end", case.function)
        })
        .collect();
    format!(
        "lists:foreach(fun(F) -> io:format(\"~p~n\", [F()]) end, [{}]), halt(0).",
        calls.join(",")
    )
}

/// The system `erl`'s answers for the whole matrix, one line per case.
///
/// `None` means no runnable `erl` at all (the caller asserts the pinned values and
/// skips loudly). A spawned `erl` that exits non-zero is a **harness** failure, not
/// "no oracle", so it panics with the captured stderr -- a broken oracle can never
/// masquerade as a silent skip.
fn oracle() -> Option<Vec<String>> {
    let dir = std::path::Path::new(SELVAL_BEAM)
        .parent()
        .expect("the fixture path has a directory");
    let expr = oracle_expr();
    let output = Command::new("erl")
        .arg("-noshell")
        .arg("-pz")
        .arg(dir)
        .arg("-eval")
        .arg(&expr)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {} for the oracle matrix\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<String> = text.lines().map(|line| line.trim().to_string()).collect();
    assert_eq!(
        lines.len(),
        CASES.len(),
        "the oracle printed {} line(s) for {} case(s):\n{text}",
        lines.len(),
        CASES.len()
    );
    Some(lines)
}

// ---------------------------------------------------------------------------
// (1) the deliverable: the matrix, both tiers, against the oracle
// ---------------------------------------------------------------------------

#[test]
fn every_case_agrees_across_tiers_and_with_the_live_oracle() {
    let module = module();
    let oracle = oracle();
    if oracle.is_none() {
        eprintln!(
            "SKIP: no system `erl` on PATH -- the live oracle cross-check is skipped; the two \
             tiers and the pinned values are still asserted"
        );
    }

    for (index, case) in CASES.iter().enumerate() {
        let label = format!("selval:{}({:?})", case.function, case.arg);
        let interp = run_tier(&module, case, Tier::Interpreter);
        let jit = run_tier(&module, case, Tier::BaselineJit);

        // The pinned expectation -- the value the real `erl` selects for this case.
        assert_eq!(
            interp.value.render(),
            case.expected,
            "{label}: the interpreter selected the wrong alternative"
        );

        // Tier equivalence: the native tier's inlined word-compare switch and the
        // interpreter's ladder must select the same alternative, with the same encoding.
        assert_eq!(
            jit.value, interp.value,
            "TIER DIVERGENCE for {label}: interpreter {:?} vs baseline-jit {:?}",
            interp.value, jit.value
        );

        // ... and the live oracle, when there is one.
        if let Some(lines) = &oracle {
            assert_eq!(
                interp.value.render(),
                lines[index],
                "{label}: the pinned value disagrees with the live oracle"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (2) the ladder's externally observable claim: no candidate is materialized
// ---------------------------------------------------------------------------

#[test]
fn an_immediate_only_switch_allocates_nothing_on_either_tier() {
    let module = module();
    for function in IMMEDIATE_ONLY_FUNCTIONS {
        for case in CASES.iter().filter(|case| case.function == function) {
            for tier in [Tier::Interpreter, Tier::BaselineJit] {
                let run = run_tier(&module, case, tier);
                assert_eq!(
                    run.allocations, 0,
                    "{}: `selval:{}({:?})` made {} heap allocation(s) -- a switch over \
                     immediate alternatives must decide every pair by raw-word compare, \
                     building no candidate term",
                    tier.name(),
                    case.function,
                    case.arg,
                    run.allocations
                );
            }
        }
    }
}

/// The other direction, so the zero above is a *fact about the switch* and not a
/// dead counter: a scrutinee the ladder cannot decide by word alone (`1.0` is a boxed
/// float against small-int alternatives, `[1,2]` a cons) still reaches the general
/// comparison, which is exactly the path that may allocate nothing but does enter
/// `beam_term::cmp`; and the arguments themselves do allocate, so the harness can see
/// the door working at all.
#[test]
fn the_allocation_counter_sees_a_real_allocation() {
    let module = module();
    let mut host = CountingHost::new();
    host.reset_allocations();
    let built = terms::make_list(&mut host, &[make_small(1), make_small(2)]);
    assert!(built.is_ok(), "the list argument builds");
    assert!(
        host.allocations > 0,
        "the counting host must observe the engine's own allocation door"
    );

    // And a case whose *argument* is boxed still runs with no allocation of its own:
    // the run only reads the term.
    let case = CASES
        .iter()
        .find(|case| case.function == "mixed" && matches!(case.arg, Arg::List(_)))
        .expect("the `mixed/1` list case");
    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let run = run_tier(&module, case, tier);
        assert_eq!(run.value, Value::Atom("other".to_string()), "{tier:?}");
        assert_eq!(
            run.allocations, 0,
            "{tier:?}: a run that only compares an already-built scrutinee allocates nothing"
        );
    }
}
