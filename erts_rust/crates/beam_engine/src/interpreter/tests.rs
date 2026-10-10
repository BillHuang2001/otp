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

use super::*;
use crate::context::MinimalContext;
use beam_common::ExceptionClass;
use beam_loader::{Function, Import, TestKind};
use beam_term::{make_atom, make_small, Atom, Eterm};
/// A one-function module whose single entry label is `entry`.
fn module_with_function(code: Vec<Instr>, imports: Vec<Import>) -> (Module, Atom) {
    let mut module = Module::default();
    module.imports = imports;
    let name = module.atom_table.intern("");
    let function = Function {
        func_label: Label(1),
        entry_label: Label(2),
        module: Atom::MAX,
        name,
        arity: 0,
        labels: code
            .iter()
            .enumerate()
            .filter_map(|(i, ins)| match ins {
                Instr::Label(l) => Some((*l, i)),
                _ => None,
            })
            .collect(),
        blocks: Vec::new(),
        code,
    };
    module.functions = vec![function];
    (module, name)
}

#[test]
fn build_label_map_collects_every_function() {
    let (module, _) =
        module_with_function(vec![Instr::Label(Label(1)), Instr::Return], Vec::new());
    let map = build_label_map(&module);
    assert_eq!(map.resolve(Label(1)).unwrap(), (0, 0));
}

/// The index is module-wide: a target living in *another* function resolves to
/// that function's `(index, code offset)`.
#[test]
fn label_map_resolves_cross_function_targets() {
    let (mut module, _) =
        module_with_function(vec![Instr::Label(Label(1)), Instr::Return], Vec::new());
    // A second function with labels `2` (offset 0) and `3` (offset 2).
    let (second, _) = module_with_function(
        vec![
            Instr::Label(Label(2)),
            Instr::Jump { target: Label(3) },
            Instr::Label(Label(3)),
            Instr::Return,
        ],
        Vec::new(),
    );
    module.functions.extend(second.functions);

    let map = build_label_map(&module);
    assert_eq!(map.resolve(Label(1)).unwrap(), (0, 0));
    assert_eq!(map.resolve(Label(2)).unwrap(), (1, 0));
    assert_eq!(map.resolve(Label(3)).unwrap(), (1, 2));
}

/// A missing label is the exact same typed error the previous `BTreeMap` map
/// produced, through every entry point (`LabelMap::resolve`, `resolve`,
/// `resolve_label`).
#[test]
fn label_map_missing_label_is_the_same_typed_error() {
    let (module, _) =
        module_with_function(vec![Instr::Label(Label(1)), Instr::Return], Vec::new());
    let map = build_label_map(&module);

    assert!(matches!(map.resolve(Label(99)), Err(BeamError::BadArg)));
    assert!(matches!(resolve(&map, Label(99)), Err(BeamError::BadArg)));
    assert!(matches!(
        resolve_label(&module, Label(99)),
        Err(BeamError::BadArg)
    ));
    // An out-of-range label beyond the dense table is a miss, not a panic.
    assert!(matches!(
        map.resolve(Label(u32::MAX)),
        Err(BeamError::BadArg)
    ));
    // `Label(0)` is the `NO_FAIL` sentinel, never a code label.
    assert!(matches!(map.resolve(NO_FAIL), Err(BeamError::BadArg)));
}

/// A sparse id space takes the sorted fallback (so one far-away id can never
/// reserve a huge dense table) and still resolves every defined label.
#[test]
fn label_map_tolerates_a_sparse_id_space() {
    // `count = 2` but `max = 4_000_000_000`: a dense table would be ~48 GB.
    let (module, _) = module_with_function(
        vec![
            Instr::Label(Label(1)),
            Instr::Jump {
                target: Label(4_000_000_000),
            },
            Instr::Label(Label(4_000_000_000)),
            Instr::Return,
        ],
        Vec::new(),
    );
    let map = build_label_map(&module);
    assert_eq!(map.resolve(Label(1)).unwrap(), (0, 0));
    assert_eq!(map.resolve(Label(4_000_000_000)).unwrap(), (0, 2));
    assert!(matches!(map.resolve(Label(2)), Err(BeamError::BadArg)));
}

/// The builder used by tests (`new`/`insert`/`from`) replaces a repeated id
/// (last wins), exactly like the `BTreeMap` it replaced.
#[test]
fn label_map_builder_replaces_a_repeated_label() {
    let mut map = LabelMap::new();
    map.insert(Label(7), (0, 1));
    map.insert(Label(7), (0, 2));
    map.insert(Label(3), (1, 5));
    assert_eq!(map.resolve(Label(7)).unwrap(), (0, 2));
    assert_eq!(map.resolve(Label(3)).unwrap(), (1, 5));

    let from = LabelMap::from([(Label(4), (0, 4u32))]);
    assert_eq!(from.resolve(Label(4)).unwrap(), (0, 4));
    assert!(matches!(
        LabelMap::default().resolve(Label(4)),
        Err(BeamError::BadArg)
    ));
}

// ---- the argument-marshalling / fail-branch contract -------------------

/// A one-function module whose body is `[Label(2), gc_bif, Label(3), return]`,
/// the `gc_bif` calling `erlang:f/<args.len()>` with the given `fail` label.
fn gc_bif_module(fail: Label, args: Vec<Operand>) -> Module {
    let arity = args.len() as u32;
    let (mut module, _) = module_with_function(
        vec![
            Instr::Label(Label(2)),
            Instr::GcBif {
                arity: arity as u8,
                bif: ImportRef(0),
                fail,
                live: arity,
                args,
                dst: Operand::X(arity),
            },
            Instr::Label(Label(3)),
            Instr::Return,
        ],
        Vec::new(),
    );
    let erlang = module.atom_table.intern("erlang");
    let name = module.atom_table.intern("f");
    module.imports.push(Import {
        module: erlang,
        function: name,
        arity,
    });
    module
}

/// A *marshalling* failure (a bad import reference, or a bad operand) must
/// propagate the typed error **even with a live fail label**: it is not a guard
/// failure, and branching to `fail` would silently continue down a wrong path.
#[test]
fn gc_bif_marshalling_errors_propagate_instead_of_branching() {
    // (a) The import reference is out of range (no imports declared) while the
    // instruction has a *live* fail label, which must not be used.
    let (module, _) = module_with_function(
        vec![
            Instr::Label(Label(2)),
            Instr::GcBif {
                arity: 1,
                bif: ImportRef(0),
                fail: Label(3),
                live: 1,
                args: vec![Operand::X(0)],
                dst: Operand::X(0),
            },
            Instr::Label(Label(3)),
            Instr::Return,
        ],
        Vec::new(),
    );
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let err = machine
        .step_instruction(&module, &labels, 0, 1, &mut ctx)
        .unwrap_err();
    match err {
        BeamError::Raise(raise) => assert_eq!(
            raise.reason.as_atom(),
            Some("unsupported_opcode: call: import out of range")
        ),
        other => panic!("expected the typed import error, got {other:?}"),
    }

    // (b) The operand is beyond the register file: a typed `BadArg`.
    let module = gc_bif_module(Label(3), vec![Operand::X(crate::machine::MAX_REGISTER)]);
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    assert!(matches!(
        machine.step_instruction(&module, &labels, 0, 1, &mut ctx),
        Err(BeamError::BadArg)
    ));
}

/// A genuine **BIF** error *is* a guard failure: it branches to the live fail
/// label, or raises when there is none. (`MinimalContext::new()` rejects every
/// BIF; `Label(3)` is code offset 2 of the only function.)
#[test]
fn gc_bif_bif_error_follows_the_fail_label_or_raises() {
    let module = gc_bif_module(Label(3), vec![Operand::X(0)]);
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    assert_eq!(
        machine
            .step_instruction(&module, &labels, 0, 1, &mut ctx)
            .unwrap(),
        Step::Goto { func: 0, pc: 2 }
    );

    let module = gc_bif_module(NO_FAIL, vec![Operand::X(0)]);
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    assert!(machine
        .step_instruction(&module, &labels, 0, 1, &mut ctx)
        .is_err());
}

/// An arity above [`STACK_ARGS`] takes the heap fallback; every operand must
/// still arrive, in order.
#[test]
fn gc_bif_beyond_stack_args_uses_the_vec_fallback() {
    const N: usize = STACK_ARGS + 1;
    let module = gc_bif_module(NO_FAIL, (0..N as u32).map(Operand::X).collect());
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    for i in 0..N as u32 {
        machine.set_x(i, make_small(100 + i as i64)).unwrap();
    }
    let mut ctx = MinimalContext::with_bif_hook(Box::new(|m, f, a, args| {
        assert_eq!((m, f, a), ("erlang", "f", N as u32));
        assert_eq!(args.len(), N);
        for (i, arg) in args.iter().enumerate() {
            assert_eq!(beam_term::signed_value(*arg), 100 + i as i64);
        }
        Ok(args[N - 1])
    }));

    assert_eq!(
        machine
            .step_instruction(&module, &labels, 0, 1, &mut ctx)
            .unwrap(),
        Step::Next
    );
    assert_eq!(machine.x(N as u32), Some(make_small(100 + N as i64 - 1)));
}

#[test]
fn resolve_label_of_a_missing_label_is_a_typed_error() {
    let (module, _) = module_with_function(vec![Instr::Return], Vec::new());
    assert!(resolve_label(&module, Label(99)).is_err());
}

#[test]
fn step_instruction_advances_exactly_one_instruction() {
    // `[Label, move x0 <- 5, return]`.
    let (module, _) = module_with_function(
        vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::Integer(5),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
        Vec::new(),
    );
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    // A bottom frame whose continuation is `Halt`, so the trailing `return`
    // finishes the run rather than looking for a caller's continuation.
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();

    // `pc = 1` is the `move`: exactly one instruction, advancing to `pc + 1`.
    let step = machine
        .step_instruction(&module, &labels, 0, 1, &mut ctx)
        .unwrap();
    assert_eq!(step, Step::Next);
    assert_eq!(machine.x(0), Some(make_small(5)));
    // The move did not run the `return`: the frame is still live.
    assert_eq!(machine.frame_depth(), 1);

    // `pc = 2` is the `return` reaching the `Halt` continuation: the run ends.
    let step = machine
        .step_instruction(&module, &labels, 0, 2, &mut ctx)
        .unwrap();
    assert_eq!(step, Step::Done);
}

#[test]
fn step_instruction_reports_a_jump_target() {
    // `[Label(1), jump Label(2), Label(2), return]`.
    let (module, _) = module_with_function(
        vec![
            Instr::Label(Label(1)),
            Instr::Jump { target: Label(2) },
            Instr::Label(Label(2)),
            Instr::Return,
        ],
        Vec::new(),
    );
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();

    // The `jump` at `pc = 1` transfers control to `Label(2)`, which is code
    // offset 2 of the same (only) function.
    let step = machine
        .step_instruction(&module, &labels, 0, 1, &mut ctx)
        .unwrap();
    assert_eq!(step, Step::Goto { func: 0, pc: 2 });
}

#[test]
fn step_instruction_out_of_range_is_a_typed_error() {
    let (module, _) = module_with_function(vec![Instr::Return], Vec::new());
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();

    // An out-of-range `pc` / function index stays an `Err`, never a panic.
    assert!(machine
        .step_instruction(&module, &labels, 0, 99, &mut ctx)
        .is_err());
    assert!(machine
        .step_instruction(&module, &labels, 99, 0, &mut ctx)
        .is_err());
}

// ---- the `NO_FAIL` failure semantics of `test`/`select_val` ------------

/// The `(class, live reason term)` of the uncaught raise one step of `machine`
/// at `pc` produced. Panics unless the step raised, and unless the raise carries
/// a live reason term, so a test can never pass by observing a different outcome
/// than the one it pins.
fn step_uncaught_raise(
    module: &Module,
    labels: &LabelMap,
    pc: u32,
    machine: &mut Machine,
) -> (ExceptionClass, Eterm) {
    let err = machine
        .step_instruction(module, labels, 0, pc, &mut MinimalContext::new())
        .expect_err("the step raises");
    match err {
        BeamError::Raise(raise) => {
            let live = raise
                .live_reason
                .as_ref()
                .expect("an uncaught reason carries its term");
            (
                raise.class,
                *live.downcast_ref::<Eterm>().expect("the reason term"),
            )
        }
        other => panic!("expected an uncaught raise, got {other:?}"),
    }
}

/// A `test` whose operand is not a tuple and whose failure operand is the
/// `f,0` sentinel raises class `error` with the **bare** atom
/// `function_clause` — the failed-clause reason `func_info` raises, with the
/// arguments in the stacktrace rather than in the reason.
#[test]
fn test_with_no_fail_raises_function_clause() {
    let (mut module, _) = module_with_function(
        vec![
            Instr::Label(Label(1)),
            Instr::Test {
                kind: TestKind::IsTuple,
                fail: NO_FAIL,
                args: vec![Operand::X(0)],
            },
            Instr::Return,
        ],
        Vec::new(),
    );
    let reason_atom = module.atom_table.intern("function_clause");
    module.atom_table.intern("error");
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    // A small integer is not a tuple, so the test fails.
    machine.set_x(0, make_small(1)).unwrap();

    let (class, reason) = step_uncaught_raise(&module, &labels, 1, &mut machine);
    assert_eq!(class, ExceptionClass::Error);
    assert_eq!(reason, make_atom(reason_atom));
    // The machine records exactly the same pending class + reason term.
    assert_eq!(machine.pending_class(), Some(ExceptionClass::Error));
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Error, make_atom(reason_atom)))
    );
}

/// The same for `select_val`: a scrutinee matching no alternative with an `f,0`
/// failure operand raises class `error` with the bare atom `function_clause`.
#[test]
fn select_val_with_no_fail_raises_function_clause() {
    let (mut module, _) = module_with_function(
        vec![
            Instr::Label(Label(1)),
            Instr::SelectVal {
                value: Operand::X(0),
                fail: NO_FAIL,
                pairs: vec![(Operand::Integer(1), Label(3))],
            },
            Instr::Label(Label(2)),
            Instr::Return,
            Instr::Label(Label(3)),
            Instr::Return,
        ],
        Vec::new(),
    );
    let reason_atom = module.atom_table.intern("function_clause");
    module.atom_table.intern("error");
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    // `2` matches neither alternative.
    machine.set_x(0, make_small(2)).unwrap();

    let (class, reason) = step_uncaught_raise(&module, &labels, 1, &mut machine);
    assert_eq!(class, ExceptionClass::Error);
    assert_eq!(reason, make_atom(reason_atom));
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Error, make_atom(reason_atom)))
    );
}

/// A module that never mentions `function_clause` cannot name the reason, so a
/// `test` `NO_FAIL` failure stays the honest typed error — never a substituted
/// atom.
#[test]
fn test_with_no_fail_on_a_module_without_function_clause_is_the_typed_gap() {
    let (module, _) = module_with_function(
        vec![
            Instr::Label(Label(1)),
            Instr::Test {
                kind: TestKind::IsTuple,
                fail: NO_FAIL,
                args: vec![Operand::X(0)],
            },
            Instr::Return,
        ],
        Vec::new(),
    );
    // The synthetic module's table holds no `function_clause` atom.
    assert!(module.atom_table.lookup("function_clause").is_none());
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    machine.set_x(0, make_small(1)).unwrap();

    let err = machine
        .step_instruction(&module, &labels, 0, 1, &mut MinimalContext::new())
        .unwrap_err();
    match err {
        BeamError::Raise(raise) => assert_eq!(
            raise.reason.as_atom(),
            Some("unsupported_opcode: test: no 'function_clause' atom in the module")
        ),
        other => panic!("expected the typed gap, got {other:?}"),
    }
}

/// The `select_val` spelling of the same typed gap.
#[test]
fn select_val_with_no_fail_on_a_module_without_function_clause_is_the_typed_gap() {
    let (module, _) = module_with_function(
        vec![
            Instr::Label(Label(1)),
            Instr::SelectVal {
                value: Operand::X(0),
                fail: NO_FAIL,
                pairs: vec![(Operand::Integer(1), Label(3))],
            },
            Instr::Label(Label(2)),
            Instr::Return,
            Instr::Label(Label(3)),
            Instr::Return,
        ],
        Vec::new(),
    );
    assert!(module.atom_table.lookup("function_clause").is_none());
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    machine.set_x(0, make_small(2)).unwrap();

    let err = machine
        .step_instruction(&module, &labels, 0, 1, &mut MinimalContext::new())
        .unwrap_err();
    match err {
        BeamError::Raise(raise) => assert_eq!(
            raise.reason.as_atom(),
            Some("unsupported_opcode: select_val: no 'function_clause' atom in the module")
        ),
        other => panic!("expected the typed gap, got {other:?}"),
    }
}

/// A live fail label is unchanged: the failed test **branches** to it, it does
/// not raise.
#[test]
fn test_with_a_live_fail_label_branches_instead_of_raising() {
    let (module, _) = module_with_function(
        vec![
            Instr::Label(Label(1)),
            Instr::Test {
                kind: TestKind::IsTuple,
                fail: Label(2),
                args: vec![Operand::X(0)],
            },
            Instr::Label(Label(2)),
            Instr::Return,
        ],
        Vec::new(),
    );
    // No `function_clause`/`error` atom is needed: a live fail label never
    // builds a reason.
    let labels = build_label_map(&module);
    let mut machine = Machine::new();
    machine.set_x(0, make_small(1)).unwrap();

    let step = machine
        .step_instruction(&module, &labels, 0, 1, &mut MinimalContext::new())
        .unwrap();
    assert_eq!(step, Step::Goto { func: 0, pc: 2 });
    assert_eq!(machine.pending_exception(), None);
}/// A module whose `n` imports are `erlang:f0/2` .. `erlang:f<n-1>/2`,
/// interned into its own atom table.
fn module_with_imports(n: u32) -> Module {
    let (mut module, _) =
        module_with_function(vec![Instr::Label(Label(1)), Instr::Return], Vec::new());
    let m = module.atom_table.intern("erlang");
    for i in 0..n {
        let f = module.atom_table.intern(&format!("f{i}"));
        module.imports.push(Import {
            module: m,
            function: f,
            arity: 2,
        });
    }
    module
}

/// The per-run cache answers exactly what the uncached resolution would, for
/// every in-range import.
#[test]
fn import_cache_matches_the_uncached_resolution() {
    let module = module_with_imports(64);
    let labels = build_label_map(&module);
    for i in 0..64u32 {
        let cached = labels.import_target(&module, ImportRef(i)).unwrap();
        let direct = import_target(&module, ImportRef(i)).unwrap();
        assert_eq!(cached, direct);
        assert_eq!(cached.0, "erlang");
        assert_eq!(cached.1, format!("f{i}").as_str());
        assert_eq!(cached.2, 2);
    }
}

/// An out-of-range `ImportRef` is the same typed error as before, cached or
/// not.
#[test]
fn import_cache_out_of_range_is_the_same_typed_error() {
    let module = module_with_imports(3);
    let labels = build_label_map(&module);
    for i in [3u32, 4, u32::MAX] {
        let cached = labels.import_target(&module, ImportRef(i)).unwrap_err();
        let direct = import_target(&module, ImportRef(i)).unwrap_err();
        assert_eq!(format!("{cached:?}"), format!("{direct:?}"));
        assert!(format!("{cached:?}").contains("call: import out of range"));
    }
}

/// An import whose module/function atom is absent from the table yields the
/// same typed error from the cache and the fallback (never a substituted
/// name).
#[test]
fn import_cache_missing_atoms_are_the_same_typed_error() {
    let (mut module, _) =
        module_with_function(vec![Instr::Label(Label(1)), Instr::Return], Vec::new());
    let good = module.atom_table.intern("erlang");
    let missing = Atom::from_raw(1_000_000);
    module.imports.push(Import {
        module: missing,
        function: good,
        arity: 1,
    });
    module.imports.push(Import {
        module: good,
        function: missing,
        arity: 1,
    });
    let labels = build_label_map(&module);
    for i in 0..2u32 {
        let cached = labels.import_target(&module, ImportRef(i)).unwrap_err();
        let direct = import_target(&module, ImportRef(i)).unwrap_err();
        assert_eq!(format!("{cached:?}"), format!("{direct:?}"));
        assert!(format!("{cached:?}").contains("call: import"));
    }
}

/// A `LabelMap` built from one module never answers for another: the owner
/// check falls back to resolving against the module actually running, so a
/// foreign name can never be substituted and a longer import table is never
/// truncated to the cached length.
#[test]
fn import_cache_never_answers_for_a_foreign_module() {
    let a = module_with_imports(2);
    let b = module_with_imports(5);
    let labels_a = build_label_map(&a);

    let cached = labels_a.import_target(&b, ImportRef(1)).unwrap();
    assert_eq!(cached, import_target(&b, ImportRef(1)).unwrap());
    assert_eq!(cached.1, "f1");
    assert_eq!(cached.2, 2);
    // `b` has five imports, so index 4 resolves through the fallback even
    // though `a`'s cache only holds two.
    assert_eq!(labels_a.import_target(&b, ImportRef(4)).unwrap().1, "f4");
    // And `a`'s own cache still answers for `a`.
    assert_eq!(labels_a.import_target(&a, ImportRef(0)).unwrap().1, "f0");
}

/// A `LabelMap` that was not built from any module (`new`/`from`) carries no
/// cache and still resolves through the fallback.
#[test]
fn import_cache_is_absent_on_a_hand_built_label_map() {
    let module = module_with_imports(2);
    let map = LabelMap::new();
    assert_eq!(
        map.import_target(&module, ImportRef(1)).unwrap(),
        import_target(&module, ImportRef(1)).unwrap()
    );
}
