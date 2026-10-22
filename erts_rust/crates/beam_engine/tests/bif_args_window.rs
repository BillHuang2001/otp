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

//! **The BIF argument window is collection-free** — the invariant that makes the
//! dispatcher's argument slice sound even though it is in no root set.
//!
//! # The window
//!
//! A BIF dispatch hands the host a `&[Eterm]` of arguments that is **not** part of the
//! machine's frozen [`RootSet`](beam_engine::context::RootSet). The common arities use a
//! fixed stack array (`EMPTY_ARGS`, [`STACK_ARGS`] = 2 words) and a wider one
//! (`arity >= 3`) the machine's reusable `args_scratch`, which
//! `Machine::dispatch_wide` `mem::take`s **out of the machine** for the whole dispatch
//! and gives back empty. Either way the words are outside every root set while the host
//! runs, so a collection in that window would strand them.
//!
//! Nothing collects there, and the reason is structural rather than incidental:
//! `Machine::dispatch_bif_call` takes **`&self`**, so the host never gets the machine
//! mutably — while both rooted doors (`alloc_words_rooted`, `ensure_heap_full`) take
//! `&mut self` (the *context*) plus a root set whose slices come **from this machine**.
//! The engine therefore makes **zero** rooted-allocation / heap-admission calls for the
//! duration, which is exactly what this suite pins.
//!
//! # What a future change must not break
//!
//! * `dispatch_bif_call` stays `&self`, and no route inside the window regains a `&mut
//!   Machine` (a host callback that took one would re-open the hole);
//! * the marshalling helpers (`exec_bif`, `call_bif_named`, `dispatch_wide`) call no
//!   rooted door;
//! * `ExecutionContext::alloc_words` — the only heap door a BIF can legally reach, and
//!   the one the marshalling uses for a non-small integer operand — stays
//!   non-collecting.
//!
//! If any of those falls, the argument words must be rooted (kept in the machine and
//! enumerated, or parked at the stack tail) — **not** this comment relaxed.
//!
//! # The probe
//!
//! [`WindowProbe`] is a real `ExecutionContext` that delegates to a [`MinimalContext`]
//! and counts every crossing of the three heap-admission doors. A negative assertion
//! only means something if the probe can fire at all, so the suite also drives a
//! `test_heap` fixture — an ordinary `ensure_heap_full` admission point — and requires
//! the counters to move there.

use std::collections::BTreeMap;

use beam_common::Result;
use beam_engine::context::RootSet;
use beam_engine::{BifOutcome, ExecutionContext, Machine, MinimalContext};
use beam_loader::{Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module};
use beam_term::{make_small, signed_value, Atom, Eterm};

/// The import every fixture dispatches: `host:pair_sum/N`, the small-int sum of its
/// arguments. It allocates nothing (the shape a measured BIF must have).
const BIF_MODULE: &str = "host";
const BIF_FUNCTION: &str = "pair_sum";

// ---------------------------------------------------------------------------
// The probe context
// ---------------------------------------------------------------------------

/// One observed BIF dispatch.
struct Dispatch {
    /// The arity the engine dispatched with.
    arity: u32,
    /// The argument slice the host was handed.
    args: Vec<Eterm>,
    /// `x(0..arity)` as the **live machine** held them at call time — recorded only on
    /// the in-machine door, which is the one that is handed the machine.
    registers: Option<Vec<Eterm>>,
}

/// A [`ExecutionContext`] that records every rooted-door crossing and every BIF
/// dispatch, and can opt into the in-machine door so the argument slice can be compared
/// with the live registers.
struct WindowProbe {
    arena: MinimalContext,
    /// Every `alloc_words_rooted` / `ensure_heap_full` / `ensure_heap` crossing, for the
    /// whole lifetime of the probe.
    rooted_doors: usize,
    /// The same, but only while a BIF dispatch was in flight (the window itself).
    during_dispatch: usize,
    /// Whether a dispatch is in flight right now.
    in_dispatch: bool,
    /// Every dispatch the engine made, in order.
    dispatches: Vec<Dispatch>,
    /// Answer [`ExecutionContext::wants_bif_in_machine`] with this.
    in_machine: bool,
}

impl WindowProbe {
    fn new(in_machine: bool) -> Self {
        Self {
            arena: MinimalContext::new(),
            rooted_doors: 0,
            during_dispatch: 0,
            in_dispatch: false,
            dispatches: Vec::new(),
            in_machine,
        }
    }

    /// Note one heap-admission / rooted-allocation crossing.
    fn note_rooted_door(&mut self) {
        self.rooted_doors += 1;
        if self.in_dispatch {
            self.during_dispatch += 1;
        }
    }

    /// The value a fixture's BIF returns: the small-int sum of its arguments.
    fn answer(&self, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        assert_eq!(
            arity as usize,
            args.len(),
            "the engine must dispatch the BIF at the arity whose arguments it marshalled"
        );
        let mut total: i64 = 0;
        for arg in args {
            total += signed_value(*arg);
        }
        Ok(make_small(total))
    }
}

impl ExecutionContext for WindowProbe {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        assert_eq!((module, function), (BIF_MODULE, BIF_FUNCTION));
        self.in_dispatch = true;
        self.dispatches.push(Dispatch {
            arity,
            args: args.to_vec(),
            registers: None,
        });
        let value = self.answer(arity, args);
        self.in_dispatch = false;
        value
    }

    fn wants_bif_in_machine(&self) -> bool {
        self.in_machine
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
        self.in_dispatch = true;
        let mut registers = Vec::with_capacity(arity as usize);
        for index in 0..arity {
            registers.push(machine.x(index).expect("the register is in range"));
        }
        self.dispatches.push(Dispatch {
            arity,
            args: args.to_vec(),
            registers: Some(registers),
        });
        let value = self.answer(arity, args);
        self.in_dispatch = false;
        value.map(BifOutcome::Return)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.note_rooted_door();
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn ensure_heap_full(&mut self, heap_need: u32, roots: RootSet<'_>) -> Result<()> {
        self.note_rooted_door();
        self.arena.ensure_heap_full(heap_need, roots)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }

    fn alloc_words_rooted(&mut self, n: usize, roots: RootSet<'_>) -> Result<*mut Eterm> {
        self.note_rooted_door();
        self.arena.alloc_words_rooted(n, roots)
    }
}

// ---------------------------------------------------------------------------
// The fixtures
// ---------------------------------------------------------------------------

/// Intern `host:pair_sum/arity` into `module` and return the [`ImportRef`] a `call_ext`
/// needs.
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

/// `bifwindow_call/arity` = `label 1 | call_ext arity host:pair_sum/arity | return`.
///
/// A plain `call_ext` (never `call_ext_last`), so the module keeps exactly one import,
/// one entry label and one BIF dispatch at every arity. `arity <= STACK_ARGS` marshals
/// through the fixed stack array; a larger one through the machine's reusable
/// `args_scratch`.
fn call_ext_module(arity: u32) -> (Module, Atom) {
    let mut module = Module::default();
    let import = intern_import(&mut module, arity);
    let body = vec![Instr::CallExt {
        target: import,
        arity,
    }];
    finish_module(module, "bifwindow_call", arity, body)
}

/// `bifwindow_heap/0` = `label 1 | test_heap 8 0 | return` — the **positive control**:
/// an ordinary `ensure_heap_full` admission point, so the probe's counters are proven to
/// move on a fixture that does cross a rooted door.
fn test_heap_module() -> (Module, Atom) {
    let module = Module::default();
    let body = vec![Instr::TestHeap {
        heap_need: HeapNeed::Words(8),
        live: 0,
    }];
    finish_module(module, "bifwindow_heap", 0, body)
}

/// The narrow and wide arities either side of `STACK_ARGS = 2`.
const NARROW_ARITY: u32 = 2;
const WIDE_ARITY: u32 = 5;

/// `arity` small-int arguments, `1..=arity`, so the expected sum is
/// `arity * (arity + 1) / 2` and every argument is distinguishable by value.
fn args_for(arity: u32) -> Vec<Eterm> {
    (1..=arity).map(|n| make_small(i64::from(n))).collect()
}

// ---------------------------------------------------------------------------
// (a) no rooted door is crossed while the argument slice is out of the root set
// ---------------------------------------------------------------------------

#[test]
fn a_wide_dispatch_crosses_no_rooted_door_while_the_scratch_is_out() {
    for arity in [NARROW_ARITY, WIDE_ARITY] {
        let (module, function) = call_ext_module(arity);
        let args = args_for(arity);
        let mut ctx = WindowProbe::new(false);
        let mut machine = Machine::new();
        let value = machine
            .run_export(&module, function, arity, &args, &mut ctx)
            .expect("the fixture's BIF returns");
        assert_eq!(
            value,
            make_small(i64::from(arity * (arity + 1) / 2)),
            "the BIF must really have run and summed its arguments"
        );
        assert_eq!(ctx.dispatches.len(), 1, "exactly one BIF dispatch");
        assert_eq!(
            ctx.dispatches[0].arity, arity,
            "arity {arity}: the dispatch must use the import's declared arity"
        );
        assert_eq!(
            ctx.dispatches[0].args, args,
            "arity {arity}: the host must be handed `x(0..arity)` — the run's own \
             argument registers — verbatim"
        );
        assert_eq!(
            ctx.rooted_doors, 0,
            "arity {arity}: the engine crossed a rooted-allocation/heap-admission door \
             during a run whose only heap-relevant step is a BIF dispatch. The argument \
             slice (`EMPTY_ARGS` for arity <= 2, `Machine::args_scratch` beyond) is in no \
             root set, so a collection there strands every argument: if this fires, the \
             argument words must be rooted (kept in the machine and enumerated, or parked \
             at the stack tail) before the change lands"
        );
        assert_eq!(
            ctx.during_dispatch, 0,
            "arity {arity}: a rooted door was crossed from inside a BIF dispatch"
        );
    }
}

// ---------------------------------------------------------------------------
// (b) the in-machine door sees the same words the host is handed
// ---------------------------------------------------------------------------

#[test]
fn both_arities_hand_the_host_exactly_the_live_registers() {
    for arity in [NARROW_ARITY, WIDE_ARITY] {
        let (module, function) = call_ext_module(arity);
        let args = args_for(arity);
        let mut ctx = WindowProbe::new(true);
        let mut machine = Machine::new();
        let value = machine
            .run_export(&module, function, arity, &args, &mut ctx)
            .expect("the fixture's BIF returns");
        assert_eq!(value, make_small(i64::from(arity * (arity + 1) / 2)));

        let dispatch = ctx
            .dispatches
            .first()
            .expect("the in-machine door must be taken");
        let registers = dispatch
            .registers
            .as_ref()
            .expect("the in-machine door is handed the machine");
        assert_eq!(
            dispatch.args, args,
            "arity {arity}: the marshalled arguments must be the run's x(0..arity)"
        );
        assert_eq!(
            (dispatch.args.as_slice(), registers.as_slice()),
            (args.as_slice(), args.as_slice()),
            "arity {arity}: the argument slice the host is handed must equal the live \
             x(0..arity) registers at call time — a marshalling bug that dropped, \
             reordered or duplicated an argument would show here"
        );
        assert_eq!(
            ctx.rooted_doors, 0,
            "arity {arity}: the in-machine door is still a `&self` dispatch and must \
             cross no rooted door"
        );
    }
}

// ---------------------------------------------------------------------------
// (c) the probe is live: an ordinary admission point does move the counters
// ---------------------------------------------------------------------------

#[test]
fn the_probe_counts_a_real_heap_admission_point() {
    let (module, function) = test_heap_module();
    let mut ctx = WindowProbe::new(false);
    let mut machine = Machine::new();
    machine
        .run_export(&module, function, 0, &[], &mut ctx)
        .expect("the test_heap fixture returns");
    assert!(
        ctx.rooted_doors >= 1,
        "`test_heap` reaches `ensure_heap_full`, so the probe's counters MUST move — \
         without this the zero assertions above would prove nothing"
    );
    assert!(
        ctx.dispatches.is_empty(),
        "the control fixture dispatches no BIF"
    );
}