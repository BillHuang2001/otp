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

//! **The `call_ext*` BIF argument slice is the LIVE register file — the GC-rooting fix.**
//!
//! The defect this suite pins: [`Machine::call_bif_named`] used to **copy** the
//! `call_ext*` argument registers into a stack array (`STACK_ARGS` words) or a detached
//! `args_scratch` Vec, and hand the host that copy. The copy is in **no root set** (the
//! dispatch door is `&self`, so the machine's collecting doors are not crossed), and a
//! nested collection the host drives itself would relocate the real `x(0..arity)`
//! registers while the BIF kept reading a stale, freed copy — the khepri `projections`
//! segfault shape (`beam_term::list::car` on a stranded cons cell).
//!
//! The fix: `call_bif_named` now hands the host the **live** `x(0..arity)` slice by
//! reference ([`Machine::x_slice`]), never a copy. That slice IS inside the machine's
//! frozen [`RootSet`](beam_engine::context::RootSet) (`x[0..used_x_extent]`), so a
//! collection relocates the very words the BIF reads.
//!
//! # Why these tests prove relocation, not just pointer equality
//!
//! `Machine::dispatch_bif_call` takes `&self`, and the in-machine door hands the host a
//! `&Machine` — there is no sound way for a `&mut self` context to drive the machine's own
//! collecting doors *during* the dispatch window (a `&mut Machine` aliasing the live
//! `&Machine` would be UB). These tests therefore prove the two links of the equivalent
//! guarantee, which is stronger because it does not rely on the dispatch internals:
//!
//! 1. **The relocation rewrites the register file in place.** A term is built on the
//!    arena, rooted in `x(0)`, and a real Tier-B [`MinimalContext::reclaim_heap`] is
//!    driven through [`Machine::for_each_root_mut`] — the same door a collecting host
//!    uses. The test observes `x(0)` change address while its shape is preserved, so the
//!    `x` register-file slots are provably the relocation target.
//! 2. **The BIF reads those same slots.** The in-machine door asserts
//!    `args.as_ptr() == machine.x_slice(arity).as_ptr()` — the slice handed to the host
//!    IS the live register file, not a marshalled buffer. A relocation therefore updates
//!    the very words the BIF reads.
//!
//! The pointer-equality assertion is the regression guard: before the fix `args` pointed
//!   at a stack-local `buf` (narrow) or a detached `args_scratch` (wide) — a different
//!   allocation — so it fails. After the fix it points at `self.x` and passes. A stale
//!   read is never fabricated: the BIF is handed the **already-relocated** term and must
//!   observe its intact structure.
//!
//! # Running it
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --test bif_arg_rooting
//! ```

use std::collections::BTreeMap;

use beam_common::Result;
use beam_engine::{BifOutcome, ExecutionContext, Machine, MinimalContext};
use beam_loader::{Export, Function, Import, ImportRef, Instr, Label, Module};
use beam_term::{
    arityval, is_nil, is_small, list_val, make_arityval, make_list, make_small, make_tuple,
    primary_tag, signed_value, tuple_val, Atom, Eterm, PrimaryTag, NIL,
};

/// The import every fixture dispatches: `host:id/arity`, the identity BIF.
const BIF_MODULE: &str = "host";
const BIF_FUNCTION: &str = "id";

// ---------------------------------------------------------------------------
// Helpers (raw-word access is confined to this test crate and every use is annotated)
// ---------------------------------------------------------------------------

/// A cons cell `[car | cdr]`, returning the LIST term.
fn cons_cell(ctx: &mut MinimalContext, car: Eterm, cdr: Eterm) -> Eterm {
    let cell = ctx
        .alloc_words(2)
        .expect("the arena allocates a cons cell");
    // SAFETY: `alloc_words(2)` handed out exactly the cell's two words.
    unsafe {
        cell.write(car);
        cell.add(1).write(cdr);
    }
    make_list(cell)
}

/// A proper list of `elements`, built last-to-first.
fn list_of(ctx: &mut MinimalContext, elements: &[Eterm]) -> Eterm {
    let mut list = NIL;
    for element in elements.iter().rev() {
        list = cons_cell(ctx, *element, list);
    }
    list
}

/// A tuple of `elements`, returning the BOXED term.
fn tuple_of(ctx: &mut MinimalContext, elements: &[Eterm]) -> Eterm {
    let hp = ctx
        .alloc_words(1 + elements.len())
        .expect("the arena allocates a tuple");
    // SAFETY: `alloc_words(1 + n)` handed out the header word plus one word per element.
    unsafe {
        hp.write(make_arityval(elements.len()));
        for (i, element) in elements.iter().enumerate() {
            hp.add(1 + i).write(*element);
        }
    }
    make_tuple(hp)
}

/// A structural description of `term` with **no addresses in it** — the tags and the small
/// integers, recursively. Two graphs are the same graph iff their descriptions match.
fn shape(term: Eterm) -> String {
    match primary_tag(term) {
        PrimaryTag::Immed1 if is_nil(term) => "[]".to_string(),
        PrimaryTag::Immed1 if is_small(term) => format!("#{}", signed_value(term)),
        PrimaryTag::Immed1 => format!("immed:{:#x}", term.raw()),
        PrimaryTag::List => {
            // SAFETY: a LIST word built by `cons_cell` addresses a live `[CAR, CDR]` cell.
            let (car, cdr) = unsafe { (*list_val(term), *list_val(term).add(1)) };
            format!("[{}|{}]", shape(car), shape(cdr))
        }
        PrimaryTag::Boxed => {
            // SAFETY: a BOXED word built by `tuple_of` addresses a live arityval header,
            // and its `arityval` elements follow it.
            let hp = tuple_val(term);
            let arity = unsafe { arityval(*hp) };
            let mut out = String::from("{");
            for i in 0..arity {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&shape(unsafe { *hp.add(1 + i) }));
            }
            out.push('}');
            out
        }
        PrimaryTag::Header => format!("header:{:#x}", term.raw()),
    }
}

// ---------------------------------------------------------------------------
// The probe context
// ---------------------------------------------------------------------------

/// A [`ExecutionContext`] that delegates its heap doors to a [`MinimalContext`] and, on
/// the in-machine BIF door, (a) asserts the handed argument slice IS the live register
/// file and (b) records what the BIF observed as `shape(args[0])`.
struct RelocatingProbe {
    arena: MinimalContext,
    /// The shape the last BIF dispatch observed for its first argument.
    observed: Option<String>,
}

impl RelocatingProbe {
    fn new() -> Self {
        Self {
            arena: MinimalContext::new(),
            observed: None,
        }
    }
}

impl ExecutionContext for RelocatingProbe {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        _arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        // Not taken in these tests (`wants_bif_in_machine` is `true`), but kept correct.
        assert_eq!((module, function), (BIF_MODULE, BIF_FUNCTION));
        self.observed = args.first().map(|arg| shape(*arg));
        Ok(args.first().copied().unwrap_or(NIL))
    }

    fn wants_bif_in_machine(&self) -> bool {
        true
    }

    fn call_bif_in_machine_outcome(
        &mut self,
        machine: &Machine,
        _entry_module: &Module,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<BifOutcome> {
        assert_eq!((module, function), (BIF_MODULE, BIF_FUNCTION));
        // THE regression guard: the `call_ext*` register path is zero-copy. The slice the
        // host is handed must be the LIVE `x(0..arity)` register file, not a marshalled
        // buffer — a copied `buf`/`args_scratch` (the pre-fix behaviour) is a different
        // allocation and would strand its argument words across a collection.
        assert_eq!(
            args.as_ptr(),
            machine
                .x_slice(arity)
                .expect("the register file is wide enough")
                .as_ptr(),
            "the in-machine door must be handed the LIVE x(0..arity) register slice, \
             not a marshalled copy"
        );
        // The BIF "reads" its first argument as a term graph and records what it observed.
        self.observed = args.first().map(|arg| shape(*arg));
        Ok(BifOutcome::Return(args.first().copied().unwrap_or(NIL)))
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
        self.arena.alloc_words(n)
    }
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// Intern `host:id/arity` into `module` and return the [`ImportRef`] a `call_ext` needs.
fn intern_import(module: &mut Module, arity: u32) -> ImportRef {
    module.imports.push(Import {
        module: module.atom_table.intern(BIF_MODULE),
        function: module.atom_table.intern(BIF_FUNCTION),
        arity,
    });
    ImportRef((module.imports.len() - 1) as u32)
}

/// Wrap `body` in a one-function module exported as `name/arity`.
fn finish_module(mut module: Module, name: &str, arity: u32, body: Vec<Instr>) -> (Module, Atom) {
    let function = module.atom_table.intern(name);
    module.name = function;
    let entry = Label(1);
    let mut code = Vec::with_capacity(body.len() + 2);
    code.push(Instr::Label(entry));
    code.extend(body);
    code.push(Instr::Return);
    let labels: BTreeMap<Label, usize> = code
        .iter()
        .enumerate()
        .filter_map(|(index, instr)| match instr {
            Instr::Label(label) => Some((*label, index)),
            _ => None,
        })
        .collect();
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name: function,
        arity,
        code,
        blocks: Vec::new(),
        labels,
    });
    module.exports.push(Export {
        function,
        arity,
        label: entry,
    });
    (module, function)
}

/// `bifroot_call/arity` = `label 1 | call_ext arity host:id/arity | return`.
fn call_ext_module(arity: u32) -> (Module, Atom) {
    let mut module = Module::default();
    let import = intern_import(&mut module, arity);
    let body = vec![Instr::CallExt {
        target: import,
        arity,
    }];
    finish_module(module, "bifroot_call", arity, body)
}

// ---------------------------------------------------------------------------
// (1) a list argument — the khepri `projections` segfault shape
// ---------------------------------------------------------------------------

#[test]
fn a_call_ext_list_argument_is_read_from_the_live_register_file_and_survives_relocation() {
    // Both sides of `STACK_ARGS = 2`: arity 1 is the narrow path, arity 3 the wide path.
    for arity in [1u32, 3] {
        let mut ctx = RelocatingProbe::new();

        // Build a cons list of small ints on the arena.
        let list = list_of(
            &mut ctx.arena,
            &[make_small(1), make_small(2), make_small(3)],
        );
        let expected = shape(list);

        // Root it in x(0) and drive a REAL Tier-B relocation through the machine's own
        // writable root door — proving the relocation rewrites the very `x` register-file
        // slots a collecting host enumerates.
        let mut machine = Machine::new();
        machine.x_mut()[0] = list;
        let report = ctx
            .arena
            .reclaim_heap(|reloc| {
                machine.for_each_root_mut(&mut |slots| reloc.forward_roots(slots))
            })
            .expect("the reclaim copies the machine's root set")
            .expect("there was work to do");
        assert_eq!(report.live_words, 6, "three cons cells, two words each");
        let moved = machine.x(0).expect("x(0) is inside the register file");
        assert_ne!(moved, list, "the relocation moved the list");
        assert_eq!(shape(moved), expected, "the list is intact after relocation");

        // Dispatch `call_ext arity host:id/arity` with the relocated list as x(0). The
        // probe's in-machine door asserts the handed slice IS the live register file and
        // records what the BIF observed.
        let (module, function) = call_ext_module(arity);
        let mut args = vec![moved];
        for n in 2..=arity {
            args.push(make_small(i64::from(n)));
        }
        let mut dispatch_machine = Machine::new();
        let value = dispatch_machine
            .run_export(&module, function, arity, &args, &mut ctx)
            .expect("the identity BIF returns");
        assert_eq!(value, moved, "arity {arity}: the identity BIF returns x(0)");
        assert_eq!(
            ctx.observed.as_deref(),
            Some(expected.as_str()),
            "arity {arity}: the BIF observed the intact, already-relocated list"
        );
    }
}

// ---------------------------------------------------------------------------
// (2) a nested boxed-tuple argument — the `binary_module_SUITE:error_info` D1c shape
// ---------------------------------------------------------------------------

#[test]
fn a_call_ext_tuple_argument_is_read_from_the_live_register_file_and_survives_relocation() {
    let mut ctx = RelocatingProbe::new();

    // The D1c shape `{ct_case_result, {ok, ok}}`, stood in by small ints so the boxed
    // structure (the part a relocation moves) is what is pinned: `{1, {2, 3}}`.
    let inner = tuple_of(&mut ctx.arena, &[make_small(2), make_small(3)]);
    let tuple = tuple_of(&mut ctx.arena, &[make_small(1), inner]);
    let expected = shape(tuple);
    assert_eq!(expected, "{#1,{#2,#3}}");

    // Root + relocate through the machine's writable root door.
    let mut machine = Machine::new();
    machine.x_mut()[0] = tuple;
    let report = ctx
        .arena
        .reclaim_heap(|reloc| {
            machine.for_each_root_mut(&mut |slots| reloc.forward_roots(slots))
        })
        .expect("the reclaim copies the machine's root set")
        .expect("there was work to do");
    assert_eq!(
        report.live_words, 6,
        "the inner tuple is 3 words and the outer tuple is 3 words"
    );
    let moved = machine.x(0).expect("x(0) is inside the register file");
    assert_ne!(moved, tuple, "the relocation moved the tuple");
    assert_eq!(shape(moved), expected, "the nested tuple is intact after relocation");

    // Dispatch `call_ext 1 host:id/1`; the BIF reads x(0) as a boxed tuple.
    let (module, function) = call_ext_module(1);
    let mut dispatch_machine = Machine::new();
    let value = dispatch_machine
        .run_export(&module, function, 1, &[moved], &mut ctx)
        .expect("the identity BIF returns");
    assert_eq!(value, moved, "the identity BIF returns x(0)");
    assert_eq!(
        ctx.observed.as_deref(),
        Some(expected.as_str()),
        "the BIF observed the intact, already-relocated nested tuple"
    );
}
