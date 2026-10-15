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

//! Unit tests for the baseline JIT's `test` subset: the boxed-term header matrix run
//! under both tiers (every newly inlined kind, on the values that make it true **and**
//! on the values that make it false), the pinned soundness of the fixtures against
//! `beam_term`'s own predicates, and `is_binary`'s size-read guard.
//!
//! A child module of `test`, so `super` is the `test` module and `use super::*` names
//! its items and imports exactly as it did when this was an inline `mod tests`.

use super::*;

use beam_loader::{lower, Export, Function};
use beam_term::{map, make_small, Atom, AtomTable, Eterm};

use crate::context::{ExecutionContext, MinimalContext};
use crate::jit::Tier;
use crate::machine::Machine;

/// Write `words` into `ctx`'s arena and return the **boxed term word** pointing at
/// them.
///
/// The word is `pointer | TAG_PRIMARY_BOXED`, i.e. `(raw & 3) == 2` — the exact
/// predicate [`super::header::load_boxed_header`] emits (`beam_term::boxed_val` masks
/// `TAG_PTR_MASK`, and `is_boxed_term` is `is_boxed && is_term_word`).
fn boxed(ctx: &mut MinimalContext, words: &[Eterm]) -> Eterm {
    let hp = ctx.alloc_words(words.len()).expect("arena words");
    // SAFETY: `alloc_words` hands out `words.len()` writable words that stay valid
    // for as long as `ctx` lives (a pointer-stable reserved-heap arena grown in
    // place) and every word is written before the term escapes.
    unsafe {
        for (i, word) in words.iter().enumerate() {
            *hp.add(i) = *word;
        }
    }
    Eterm::from_raw(hp as usize | 2)
}

/// A raw header word (`make_header(sz, subtag)`), built by hand so the test pins the
/// exact bit pattern the emitter's masks assume.
fn header(sz: usize, subtag: usize) -> Eterm {
    Eterm::from_raw((sz << 6) | subtag)
}

/// The terms the `test` fixtures below are driven with. Every boxed entry is a real
/// imaged word, so a predicate dereferences only words this test owns.
struct Zoo {
    terms: Vec<(&'static str, Eterm)>,
}

impl Zoo {
    fn get(&self, name: &str) -> Eterm {
        self.terms
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, term)| *term)
            .unwrap_or_else(|| panic!("the zoo has no `{name}`"))
    }
}

/// `foo` is interned by [`probe_module`] in every module, so its atom word is the
/// same in all of them.
fn zoo(ctx: &mut MinimalContext, foo: Eterm, table: &AtomTable) -> Zoo {
    let small = make_small(7);
    let atom = foo;
    let tuple1 = boxed(ctx, &[header(1, 0x00), small]);
    let tuple2 = boxed(ctx, &[header(2, 0x00), small, NIL]);
    let tuple2_tagged = boxed(ctx, &[header(2, 0x00), atom, small]);
    let tuple1_tagged = boxed(ctx, &[header(1, 0x00), atom]);
    // A real flat map (the `map` module's own writer), so `is_map` sees the exact
    // layout `beam_term` produces.
    let pairs = [(make_small(1), make_small(2))];
    let map_term = {
        let hp = ctx
            .alloc_words(map::map_words_capacity(pairs.len()))
            .expect("map words");
        // SAFETY: `map_words_capacity(1)` words, `write_map` fills them and returns
        // the boxed term; the words stay valid for `ctx`'s lifetime.
        unsafe { map::write_map(hp, &pairs, table) }.expect("flat map")
    };
    // A boxed float: `[HEADER_FLONUM, f64 bits]`.
    let float = boxed(
        ctx,
        &[Eterm::from_raw(0x58), Eterm::from_raw(1.5f64.to_bits() as usize)],
    );
    // Heap bits: `[thing_word, size, data...]` — `bitstring_size` reads `p[1]`.
    let bitstr8 = boxed(ctx, &[header(2, 0x20), Eterm::from_raw(8)]);
    let bitstr3 = boxed(ctx, &[header(2, 0x20), Eterm::from_raw(3)]);
    // Sub bits: `[thing_word, base_flags, start, end, orig]` — size is `end - start`.
    let subbits16 = boxed(
        ctx,
        &[
            Eterm::from_raw(0x124),
            Eterm::from_raw(0),
            Eterm::from_raw(0),
            Eterm::from_raw(16),
            Eterm::from_raw(0),
        ],
    );
    let subbits3 = boxed(
        ctx,
        &[
            Eterm::from_raw(0x124),
            Eterm::from_raw(0),
            Eterm::from_raw(0),
            Eterm::from_raw(3),
            Eterm::from_raw(0),
        ],
    );
    // A local fun header with arity 2 (`FUN_SUBTAG | arity << 8`); only `p[0]` is
    // ever read by `is_function`/`is_function2`.
    let fun2 = boxed(ctx, &[Eterm::from_raw(0x14 | (2usize << 8)), make_small(0)]);
    // The internal pid/port immediates: `(data << 4) | TAG_IMMED1_{PID,PORT}`.
    let int_pid = Eterm::from_raw((5usize << 4) | 0x3);
    let int_port = Eterm::from_raw((5usize << 4) | 0x7);
    // External pid/port/ref and internal ref: header subtag only (`0x30`/`0x34`/
    // `0x38`/`0x10`); no predicate reads past `p[0]`.
    let ext_pid = boxed(ctx, &[header(1, 0x30), small]);
    let ext_port = boxed(ctx, &[header(1, 0x34), small]);
    let ext_ref = boxed(ctx, &[header(1, 0x38), small]);
    let int_ref = boxed(ctx, &[header(1, 0x10), small]);

    Zoo {
        terms: vec![
            ("small", small),
            ("atom", atom),
            ("nil", NIL),
            ("tuple1", tuple1),
            ("tuple2", tuple2),
            ("tuple2_tagged", tuple2_tagged),
            ("tuple1_tagged", tuple1_tagged),
            ("map", map_term),
            ("float", float),
            ("bitstr8", bitstr8),
            ("bitstr3", bitstr3),
            ("subbits16", subbits16),
            ("subbits3", subbits3),
            ("fun2", fun2),
            ("int_pid", int_pid),
            ("int_port", int_port),
            ("ext_pid", ext_pid),
            ("ext_port", ext_port),
            ("ext_ref", ext_ref),
            ("int_ref", int_ref),
        ],
    }
}

/// Build a one-export module whose `probe/1` is
/// `test -> x(0) = 1 ; fail -> x(0) = 0`, with `x(0)` the value under test.
///
/// `foo` is interned right after the module name in every module built here, so
/// [`zoo`]'s atom word is valid in all of them.
fn probe_module(name: &str, kind: TestKind, extra: Vec<Operand>) -> (Module, Atom, Atom) {
    let mut module = Module::default();
    module.name = module.atom_table.intern(name);
    let foo = module.atom_table.intern("foo");
    module.atom_table.intern("true");
    module.atom_table.intern("false");
    let export = module.atom_table.intern("probe");
    let entry = Label(1);
    let fail = Label(2);
    let mut args = vec![Operand::X(0)];
    args.extend(extra);
    let code = vec![
        Instr::Label(entry),
        Instr::Test { kind, fail, args },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(fail),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let (blocks, labels) = lower::build_blocks(&code);
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name: export,
        arity: 1,
        code,
        blocks,
        labels,
    });
    module.exports.push(Export {
        function: export,
        arity: 1,
        label: entry,
    });
    (module, export, foo)
}

/// Run `probe/1` under both tiers with `value` in `x(0)` and return
/// `(interpreter, baseline)`.
fn both_tiers(module: &Module, export: Atom, value: Eterm) -> (Eterm, Eterm) {
    let args = [value];
    let mut interpreter_ctx = MinimalContext::new();
    let interpreter = Machine::new()
        .run_with_tier(
            module,
            export,
            1,
            &args,
            &mut interpreter_ctx,
            Tier::Interpreter,
        )
        .expect("the interpreter tier runs the fixture");
    let mut baseline_ctx = MinimalContext::new();
    let baseline = Machine::new()
        .run_with_tier(
            module,
            export,
            1,
            &args,
            &mut baseline_ctx,
            Tier::BaselineJit,
        )
        .expect("the baseline tier runs the fixture");
    (interpreter, baseline)
}

/// One table row: a `test` kind, its extra operands, the value it is driven with,
/// and the answer the compiler must produce.
struct Case {
    kind: TestKind,
    extra: Vec<Operand>,
    value: &'static str,
    expect: bool,
}

/// The boxed-term header-test matrix: every newly inlined kind, on the values that
/// make it true **and** on the values that make it false (including the non-boxed
/// immediates, whose fail path must not dereference anything).
fn header_cases() -> Vec<Case> {
    let case = |kind, extra: Vec<Operand>, value: &'static str, expect| Case {
        kind,
        extra,
        value,
        expect,
    };
    let arity = |n: i64| vec![Operand::Integer(n)];
    vec![
        case(TestKind::IsInteger, vec![], "small", true),
        case(TestKind::IsInteger, vec![], "atom", false),
        case(TestKind::IsInteger, vec![], "nil", false),
        case(TestKind::IsInteger, vec![], "tuple2", false),
        case(TestKind::IsNumber, vec![], "small", true),
        case(TestKind::IsNumber, vec![], "float", true),
        case(TestKind::IsNumber, vec![], "atom", false),
        case(TestKind::IsNumber, vec![], "tuple2", false),
        case(TestKind::IsFloat, vec![], "float", true),
        case(TestKind::IsFloat, vec![], "small", false),
        case(TestKind::IsFloat, vec![], "tuple2", false),
        case(TestKind::IsTuple, vec![], "tuple2", true),
        case(TestKind::IsTuple, vec![], "tuple1", true),
        case(TestKind::IsTuple, vec![], "map", false),
        case(TestKind::IsTuple, vec![], "float", false),
        case(TestKind::IsTuple, vec![], "atom", false),
        case(TestKind::IsTuple, vec![], "nil", false),
        case(TestKind::IsMap, vec![], "map", true),
        case(TestKind::IsMap, vec![], "tuple2", false),
        case(TestKind::IsMap, vec![], "atom", false),
        case(TestKind::IsBitstr, vec![], "bitstr8", true),
        case(TestKind::IsBitstr, vec![], "bitstr3", true),
        case(TestKind::IsBitstr, vec![], "subbits3", true),
        case(TestKind::IsBitstr, vec![], "atom", false),
        case(TestKind::IsBitstr, vec![], "tuple2", false),
        case(TestKind::IsBinary, vec![], "bitstr8", true),
        case(TestKind::IsBinary, vec![], "subbits16", true),
        case(TestKind::IsBinary, vec![], "bitstr3", false),
        case(TestKind::IsBinary, vec![], "subbits3", false),
        case(TestKind::IsBinary, vec![], "atom", false),
        case(TestKind::IsBinary, vec![], "small", false),
        case(TestKind::IsFunction, vec![], "fun2", true),
        case(TestKind::IsFunction, vec![], "atom", false),
        case(TestKind::IsFunction, vec![], "tuple2", false),
        case(TestKind::IsFunction2, arity(2), "fun2", true),
        case(TestKind::IsFunction2, arity(3), "fun2", false),
        case(TestKind::IsFunction2, arity(2), "atom", false),
        case(TestKind::IsPid, vec![], "int_pid", true),
        case(TestKind::IsPid, vec![], "ext_pid", true),
        case(TestKind::IsPid, vec![], "int_port", false),
        case(TestKind::IsPid, vec![], "small", false),
        case(TestKind::IsPid, vec![], "ext_port", false),
        case(TestKind::IsPort, vec![], "int_port", true),
        case(TestKind::IsPort, vec![], "ext_port", true),
        case(TestKind::IsPort, vec![], "int_pid", false),
        case(TestKind::IsPort, vec![], "atom", false),
        case(TestKind::IsReference, vec![], "int_ref", true),
        case(TestKind::IsReference, vec![], "ext_ref", true),
        case(TestKind::IsReference, vec![], "ext_pid", false),
        case(TestKind::IsReference, vec![], "nil", false),
        case(TestKind::TestArity, arity(2), "tuple2", true),
        case(TestKind::TestArity, arity(1), "tuple1", true),
        case(TestKind::TestArity, arity(3), "tuple2", false),
        case(TestKind::TestArity, arity(1), "tuple2", false),
        case(TestKind::TestArity, arity(2), "atom", false),
        case(TestKind::TestArity, arity(1), "small", false),
        case(TestKind::IsTaggedTuple, arity(1), "tuple1_tagged", true),
        case(TestKind::IsTaggedTuple, arity(2), "tuple2_tagged", true),
        case(TestKind::IsTaggedTuple, arity(2), "tuple2", false),
        case(TestKind::IsTaggedTuple, arity(2), "tuple1_tagged", false),
        case(TestKind::IsTaggedTuple, arity(1), "atom", false),
    ]
}

/// Every newly inlined header kind agrees with the interpreter tier, on the true
/// **and** the false values — the widening's correctness gate.
#[test]
fn header_tests_agree_with_the_interpreter_tier() {
    let mut ctx = MinimalContext::new();
    let table_owner = Module::default();
    // The atom word is the same in every `probe_module`, so any one of them can
    // provide it.
    let (_, _, foo) = probe_module("probe_atom", TestKind::IsNil, vec![]);
    let zoo = zoo(&mut ctx, make_atom(foo), &table_owner.atom_table);

    for case in header_cases() {
        let mut extra = case.extra.clone();
        if case.kind == TestKind::IsTaggedTuple {
            // The tag is the module's `foo` atom, matching the zoo's tagged tuples.
            extra.push(Operand::Atom(foo));
        }
        let (module, export, _) = probe_module("probe_header", case.kind, extra);
        let value = zoo.get(case.value);
        let (interpreter, baseline) = both_tiers(&module, export, value);
        let expected = make_small(i64::from(case.expect));
        assert_eq!(
            interpreter, expected,
            "{:?} on `{}` must be {} in the interpreter tier",
            case.kind, case.value, case.expect
        );
        assert_eq!(
            baseline, expected,
            "{:?} on `{}` must be {} in the baseline tier",
            case.kind, case.value, case.expect
        );
    }
}


/// `is_binary`'s heap-bits branch reads the size word at `p[1]` **only** after the
/// bitstring-header test, and a header that is not a bitstring header (`2`, say)
/// is rejected without reading it: the emitter never touches `p[1]` of a
/// non-bitstring. This pins that a fabricated short thing cannot be misread.
#[test]
fn is_binary_rejects_a_non_bitstring_header_without_reading_the_size() {
    let mut ctx = MinimalContext::new();
    // `[arityval(1), small]` — a tuple-like header, so the bitstring test fails
    // *and* the term is shorter than a heap-bits thing.
    let not_bits = boxed(&mut ctx, &[header(1, 0x00), make_small(3)]);
    let (module, export, _) = probe_module("probe_binary_guard", TestKind::IsBinary, vec![]);
    let (interpreter, baseline) = both_tiers(&module, export, not_bits);
    assert_eq!(interpreter, make_small(0));
    assert_eq!(baseline, make_small(0));
}

/// The `Zoo` fixtures are sound against `beam_term`'s **own** predicates — the
/// ground truth the emitter's masks mirror. A failure here means the fixture (not
/// the emitter) is wrong.
#[test]
fn zoo_fixtures_match_the_beam_term_predicates() {
    let mut ctx = MinimalContext::new();
    let (_, _, foo) = probe_module("probe_atom_check", TestKind::IsNil, vec![]);
    let table_owner = Module::default();
    let zoo = zoo(&mut ctx, make_atom(foo), &table_owner.atom_table);

    assert!(crate::predicates::is_integer(zoo.get("small")));
    assert!(!crate::predicates::is_integer(zoo.get("atom")));
    assert!(crate::predicates::is_float(zoo.get("float")));
    assert!(crate::predicates::is_number(zoo.get("float")));
    assert!(crate::predicates::is_tuple(zoo.get("tuple2")));
    assert!(!crate::predicates::is_tuple(zoo.get("map")));
    assert!(crate::predicates::is_map(zoo.get("map")));
    assert!(crate::predicates::is_bitstring(zoo.get("bitstr8")));
    assert!(crate::predicates::is_bitstring(zoo.get("subbits16")));
    assert!(crate::predicates::is_binary(zoo.get("bitstr8")));
    assert!(!crate::predicates::is_binary(zoo.get("bitstr3")));
    assert!(crate::predicates::is_binary(zoo.get("subbits16")));
    assert!(!crate::predicates::is_binary(zoo.get("subbits3")));
    assert!(crate::terms::is_fun(zoo.get("fun2")));
    assert_eq!(crate::terms::fun_arity(zoo.get("fun2")).unwrap(), 2);
    assert!(crate::predicates::is_external_pid(zoo.get("ext_pid")));
    assert!(crate::predicates::is_external_port(zoo.get("ext_port")));
    assert!(crate::predicates::is_internal_ref(zoo.get("int_ref")));
    assert!(crate::predicates::is_external_ref(zoo.get("ext_ref")));
    assert!(crate::predicates::is_tuple_arity(zoo.get("tuple2"), 2));
}
