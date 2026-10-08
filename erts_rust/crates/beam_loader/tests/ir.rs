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

//! End-to-end IR integration test over the committed oracle fixture.
//!
//! Loads `tests/fixtures/slice.beam` (a real `erlc`-produced image; see
//! `tests/fixtures/README.md` for the verified wire summary) through the public
//! [`beam_loader::load_file`] entry point and asserts the *complete* lowered IR:
//! the module header, the atom/export/import tables and the exact instruction
//! sequence of every function — ground truth captured from
//! `beam_disasm:file("slice.beam")`.
use beam_loader::{
    load_file, Atom, CodeVersion, Export, Import, ImportRef, Instr, Label, Module, Operand, Reg,
    TestKind, TypeIndex, NO_FAIL,
};

/// Loads the committed oracle fixture.
fn load_fixture() -> Module {
    load_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/slice.beam"
    ))
    .expect("the oracle fixture must load")
}

/// The interned atom for `name` in `module`.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom `{name}` is interned"))
}

/// The function `name/arity` of `module`.
fn function<'a>(module: &'a Module, name: &str, arity: u32) -> &'a beam_loader::Function {
    module
        .function(atom(module, name), arity)
        .unwrap_or_else(|| panic!("function {name}/{arity} is defined"))
}

/// An X-register operand.
fn x(n: u32) -> Operand {
    Operand::X(n)
}

/// An integer operand.
fn int(n: i64) -> Operand {
    Operand::Integer(n)
}

/// The module the fixture describes.
fn slice(module: &Module) -> Atom {
    atom(module, "slice")
}

#[test]
fn loads_the_code_header_and_module_name() {
    let module = load_fixture();

    assert_eq!(
        module.code_version,
        CodeVersion {
            format_number: 0,
            max_opcode: 181,
            label_count: 15,
            function_count: 6,
        }
    );
    assert_eq!(module.atom_name(module.name), Some("slice"));
}

#[test]
fn preserves_the_wire_atom_table() {
    let module = load_fixture();

    // Wire index 0 is the NIL placeholder; the rest are the `AtU8` names in order.
    let names: Vec<&str> = module
        .atoms
        .iter()
        .map(|&a| {
            if a == Atom::MAX {
                ""
            } else {
                module.atom_name(a).expect("every wire atom is interned")
            }
        })
        .collect();
    assert_eq!(
        names,
        vec![
            "",
            "slice",
            "add",
            "erlang",
            "+",
            "fib",
            "-",
            "pick",
            "a",
            "module_info",
            "get_module_info",
        ]
    );
}

#[test]
fn preserves_the_local_function_table() {
    let module = load_fixture();

    // `LocT` is byte-for-byte an `ExpT` layout: the module's single non-exported
    // local `fib/3` at entry label 6 (the exported `fib/1` is separate, label 4).
    assert_eq!(
        module.locals,
        vec![Export {
            function: atom(&module, "fib"),
            arity: 3,
            label: Label(6),
        }]
    );
    // The local is *not* an export: `fib/3` is absent from `ExpT`.
    assert_eq!(module.export(atom(&module, "fib"), 3), None);
    assert_eq!(module.export(atom(&module, "fib"), 1), Some(Label(4)));
}

#[test]
fn preserves_the_export_and_import_tables_in_file_order() {
    let module = load_fixture();

    // `ExpT` file order is preserved (it is *not* sorted); the label is the entry
    // label of the function.
    let expected_exports = vec![
        Export {
            function: atom(&module, "module_info"),
            arity: 1,
            label: Label(14),
        },
        Export {
            function: atom(&module, "module_info"),
            arity: 0,
            label: Label(12),
        },
        Export {
            function: atom(&module, "pick"),
            arity: 1,
            label: Label(9),
        },
        Export {
            function: atom(&module, "fib"),
            arity: 1,
            label: Label(4),
        },
        Export {
            function: atom(&module, "add"),
            arity: 2,
            label: Label(2),
        },
    ];
    assert_eq!(module.exports, expected_exports);

    let expected_imports = vec![
        Import {
            module: atom(&module, "erlang"),
            function: atom(&module, "+"),
            arity: 2,
        },
        Import {
            module: atom(&module, "erlang"),
            function: atom(&module, "-"),
            arity: 2,
        },
        Import {
            module: atom(&module, "erlang"),
            function: atom(&module, "get_module_info"),
            arity: 1,
        },
        Import {
            module: atom(&module, "erlang"),
            function: atom(&module, "get_module_info"),
            arity: 2,
        },
    ];
    assert_eq!(module.imports, expected_imports);
}

#[test]
fn export_lookup_returns_the_entry_label() {
    let module = load_fixture();

    for (name, arity, label) in [
        ("add", 2, 2u32),
        ("fib", 1, 4),
        ("pick", 1, 9),
        ("module_info", 0, 12),
        ("module_info", 1, 14),
    ] {
        assert_eq!(
            module.export(atom(&module, name), arity),
            Some(Label(label)),
            "{name}/{arity}"
        );
    }

    // A function that is defined but not exported (`fib/3`) has no export label.
    assert_eq!(module.export(atom(&module, "fib"), 3), None);
}

#[test]
fn groups_functions_with_their_two_labels() {
    let module = load_fixture();

    // `func_label` is the label before `func_info`, `entry_label` the one after it.
    let expected: &[(&str, u32, u32, u32, u32)] = &[
        ("add", 2, 1, 2, 2),
        ("fib", 1, 3, 4, 1),
        ("fib", 3, 5, 6, 3),
        ("pick", 1, 8, 9, 1),
        ("module_info", 0, 11, 12, 0),
        ("module_info", 1, 13, 14, 1),
    ];
    assert_eq!(module.functions.len(), expected.len());

    for (index, &(name, arity, func_label, entry_label, info_arity)) in expected.iter().enumerate() {
        let f = &module.functions[index];
        assert_eq!(f.name, atom(&module, name), "function #{index} name");
        assert_eq!(f.arity, info_arity, "function #{index} arity");
        assert_eq!(f.module, slice(&module), "function #{index} module");
        assert_eq!(f.func_label, Label(func_label), "function #{index} func_label");
        assert_eq!(f.entry_label, Label(entry_label), "function #{index} entry_label");
        assert_eq!(
            f.arity, arity,
            "function #{index} expected arity consistency"
        );
    }
}

#[test]
fn lowers_add_2() {
    let module = load_fixture();
    let f = function(&module, "add", 2);
    assert_eq!(f.func_label, Label(1));
    assert_eq!(f.entry_label, Label(2));

    let expected = vec![
        Instr::Label(Label(1)),
        Instr::Line(1),
        Instr::FuncInfo {
            module: slice(&module),
            function: atom(&module, "add"),
            arity: 2,
        },
        Instr::Label(Label(2)),
        Instr::GcBif {
            arity: 2,
            bif: ImportRef(0),
            fail: NO_FAIL,
            live: 2,
            args: vec![x(0), x(1)],
            dst: x(0),
        },
        Instr::Return,
    ];
    assert_eq!(f.code, expected);
}

#[test]
fn lowers_fib_1() {
    let module = load_fixture();
    let f = function(&module, "fib", 1);
    assert_eq!(f.func_label, Label(3));
    assert_eq!(f.entry_label, Label(4));

    let expected = vec![
        Instr::Label(Label(3)),
        Instr::Line(2),
        Instr::FuncInfo {
            module: slice(&module),
            function: atom(&module, "fib"),
            arity: 1,
        },
        Instr::Label(Label(4)),
        Instr::Move {
            src: int(1),
            dst: x(2),
        },
        Instr::Move {
            src: int(0),
            dst: x(1),
        },
        Instr::CallOnly {
            arity: 3,
            target: Label(6),
        },
    ];
    assert_eq!(f.code, expected);
}

#[test]
fn lowers_fib_3_with_typed_registers() {
    let module = load_fixture();
    let f = function(&module, "fib", 3);
    assert_eq!(f.func_label, Label(5));
    assert_eq!(f.entry_label, Label(6));

    let expected = vec![
        Instr::Label(Label(5)),
        Instr::Line(3),
        Instr::FuncInfo {
            module: slice(&module),
            function: atom(&module, "fib"),
            arity: 3,
        },
        Instr::Label(Label(6)),
        Instr::Test {
            kind: TestKind::IsEqExact,
            fail: Label(7),
            args: vec![x(0), int(0)],
        },
        Instr::Move {
            src: x(1),
            dst: x(0),
        },
        Instr::Return,
        Instr::Label(Label(7)),
        Instr::Line(4),
        Instr::GcBif {
            arity: 2,
            bif: ImportRef(1),
            fail: NO_FAIL,
            live: 3,
            args: vec![x(0), int(1)],
            dst: x(0),
        },
        // The `tr` operands keep their `Type` chunk indices (0-based; `TypeIndex(0)`
        // is the "any type" entry).
        Instr::GcBif {
            arity: 2,
            bif: ImportRef(0),
            fail: NO_FAIL,
            live: 3,
            args: vec![
                Operand::Typed {
                    reg: Reg::X(1),
                    ty: TypeIndex(1),
                },
                Operand::Typed {
                    reg: Reg::X(2),
                    ty: TypeIndex(2),
                },
            ],
            dst: x(1),
        },
        Instr::Swap {
            a: x(1),
            b: x(2),
        },
        Instr::CallOnly {
            arity: 3,
            target: Label(6),
        },
    ];
    assert_eq!(f.code, expected);
}

#[test]
fn lowers_pick_1() {
    let module = load_fixture();
    let f = function(&module, "pick", 1);
    assert_eq!(f.func_label, Label(8));
    assert_eq!(f.entry_label, Label(9));

    let expected = vec![
        Instr::Label(Label(8)),
        Instr::Line(5),
        Instr::FuncInfo {
            module: slice(&module),
            function: atom(&module, "pick"),
            arity: 1,
        },
        Instr::Label(Label(9)),
        Instr::Test {
            kind: TestKind::IsEqExact,
            fail: Label(10),
            args: vec![x(0), Operand::Atom(atom(&module, "a"))],
        },
        Instr::Move {
            src: int(1),
            dst: x(0),
        },
        Instr::Return,
        Instr::Label(Label(10)),
        Instr::Move {
            src: int(2),
            dst: x(0),
        },
        Instr::Return,
    ];
    assert_eq!(f.code, expected);
}

#[test]
fn lowers_module_info_0() {
    let module = load_fixture();
    let f = function(&module, "module_info", 0);
    assert_eq!(f.func_label, Label(11));
    assert_eq!(f.entry_label, Label(12));

    let expected = vec![
        Instr::Label(Label(11)),
        // The `Line` chunk location is undefined (`{line,[]}` in `beam_disasm`),
        // i.e. item index 0.
        Instr::Line(0),
        Instr::FuncInfo {
            module: slice(&module),
            function: atom(&module, "module_info"),
            arity: 0,
        },
        Instr::Label(Label(12)),
        Instr::Move {
            src: Operand::Atom(slice(&module)),
            dst: x(0),
        },
        Instr::CallExtOnly {
            arity: 1,
            target: ImportRef(2),
        },
    ];
    assert_eq!(f.code, expected);
}

#[test]
fn lowers_module_info_1_and_ends_the_stream_with_int_code_end() {
    let module = load_fixture();
    let f = function(&module, "module_info", 1);
    assert_eq!(f.func_label, Label(13));
    assert_eq!(f.entry_label, Label(14));

    let expected = vec![
        Instr::Label(Label(13)),
        Instr::Line(0),
        Instr::FuncInfo {
            module: slice(&module),
            function: atom(&module, "module_info"),
            arity: 1,
        },
        Instr::Label(Label(14)),
        Instr::Move {
            src: x(0),
            dst: x(1),
        },
        Instr::Move {
            src: Operand::Atom(slice(&module)),
            dst: x(0),
        },
        Instr::CallExtOnly {
            arity: 2,
            target: ImportRef(3),
        },
        // The module-wide stream ends at `int_code_end`, which belongs to the last
        // function.
        Instr::IntCodeEnd,
    ];
    assert_eq!(f.code, expected);
}

#[test]
fn decomposes_fib_3_into_label_delimited_blocks() {
    let module = load_fixture();
    let f = function(&module, "fib", 3);

    assert_eq!(f.code.len(), 13);

    // One block per label; each runs to the next label (last to the end of the body).
    let expected_blocks = vec![
        beam_loader::Block {
            label: Label(5),
            start: 0,
            end: 3,
        },
        beam_loader::Block {
            label: Label(6),
            start: 3,
            end: 7,
        },
        beam_loader::Block {
            label: Label(7),
            start: 7,
            end: 13,
        },
    ];
    assert_eq!(f.blocks, expected_blocks);

    let labels: Vec<(Label, usize)> = f.labels.iter().map(|(l, i)| (*l, *i)).collect();
    assert_eq!(
        labels,
        vec![(Label(5), 0), (Label(6), 3), (Label(7), 7)]
    );
}

#[test]
fn every_block_starts_at_its_own_label() {
    let module = load_fixture();

    for f in &module.functions {
        for block in &f.blocks {
            assert_eq!(
                f.code[block.start],
                Instr::Label(block.label),
                "{} / {}: block must start at its label",
                module.atom_name(f.name).unwrap_or("?"),
                f.arity
            );
            assert!(block.start < block.end, "block must be non-empty");
            assert!(block.end <= f.code.len(), "block must stay inside the body");
        }
        // The label map agrees with the block decomposition.
        assert_eq!(f.labels.len(), f.blocks.len());
        for (label, index) in &f.labels {
            assert!(f.blocks.iter().any(|b| b.label == *label && b.start == *index));
        }
    }
}
