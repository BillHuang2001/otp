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

//! **Micro-benchmark: the engine's OWN per-BIF-call cost (the engine -> context
//! boundary).**
//!
//! `tests/engine_bench.rs` prices one *interpreted instruction*; `tests/jit_bench.rs`
//! compares the tiers; `tests/bif_alloc.rs` proves the per-BIF path allocates
//! nothing. This file answers the question those three leave open: **how much of a
//! BIF call is the ENGINE's side (the `gc_bif`/`bif`/`bif0`/`call_ext*` instruction
//! path in this crate), as opposed to the HOST's `ExecutionContext::call_bif`
//! body?**
//!
//! # The boundary, from the code
//!
//! A `gc_bif Arity Fail Live Bif Args... Dst` / `bif...` / `bif0 Bif Dst` executes:
//!
//! 1. `interpreter/dispatch.rs` matches the arm and calls
//!    [`Machine::exec_bif`](beam_engine) with the instruction's operands;
//! 2. `interpreter/bif.rs::exec_bif` resolves the import through
//!    `interpreter/mod.rs::LabelMap::import_target` - the **per-run import cache**:
//!    one address comparison (`imports_owner`), one bounds check and three loads
//!    from the [`ImportTarget`] table (`Box<str>` module, `Box<str>` function,
//!    `u32` arity). On a miss it falls back to the uncached `import_target` (two
//!    atom-table lookups);
//! 3. it marshals the arguments: an arity `<= STACK_ARGS` (= 2) fills a fixed
//!    `[Eterm; 2]` **stack array** (no `Vec`); a wider arity reuses
//!    `Machine::args_scratch` through `dispatch_wide` (no allocation after the
//!    first such call). Each argument is one `read_operand` call;
//! 4. it calls `ctx.call_bif(..)` - a `&mut dyn ExecutionContext` **vtable**
//!    dispatch into `src/context.rs` (the boundary crossing itself);
//! 5. on `Ok`, `exec_bif` writes the result with `write_operand(dst, value)`.
//!
//! `call_ext`/`call_ext_only`/`call_ext_last` reach the same marshalling through
//! `Machine::call_import` (arguments read from `x(0..arity)`), so the per-call cost
//! is the same shape. `test_heap HeapNeed Live` reaches the *other* context seam:
//! `machine/frames.rs::Machine::ensure_heap` constructs `(&mut self.x, &mut self.stack)`
//! and calls `ctx.ensure_heap(heap_need, live, x, stack)` (again a vtable call).
//!
//! **Per call** (the engine side): the dispatch-arm match, the `import_target`
//! cache hit, `read_operand` per argument, the `&mut dyn` vtable call, the
//! `write_operand` store, and (whatever it does) the host hook body.
//! **Per run** (amortised, NOT per call): `interpreter::build_label_map` (the dense
//! label index + the import table resolution - one `Box<str>` per name). That is
//! why every workload below reports `ns/run` **and** `ns/instruction`, and why the
//! per-kind attribution is **floor-adjusted** by the empty-body setup floor.
//!
//! # Method
//!
//! A pluggable BIF hook whose body is **trivial** (`|_,_,_,_| Ok(make_small(7))`,
//! ignoring its arguments) makes the measured time engine work, not BIF work: the
//! real arithmetic body is measured separately and its delta over the trivial hook
//! is the *BIF's own* work. Same harness as `tests/engine_bench.rs`: hand-built
//! synthetic [`Module`]s driven through `Machine::run_export` with a
//! [`MinimalContext`], wall-clock timing with a warm-up + **median of [`SAMPLES`]
//! samples**, and only correctness/structural asserts (never a flaky performance
//! threshold). The numbers are printed under `--nocapture`; they are the
//! deliverable.
//!
//! | workload | body (per run) | isolates |
//! |---|---|---|
//! | `setup/empty` | `label; return` | the per-run setup floor |
//! | `dispatch/noop` | *K* shared-label no-ops | pure dispatch (the baseline) |
//! | `move/2` | *K* `move` | operand read + write |
//! | `test_heap/2` | *K* `test_heap` | one `ensure_heap` vtable call each |
//! | `bif0 trivial` | *K* `bif0`, trivial hook | the BIF path at **arity 0** |
//! | `gc_bif trivial` | *K* `gc_bif`, trivial hook | the path at **arity 2** (stack array) |
//! | `gc_bif arith` | *K* `gc_bif '+'/2`, real hook | the path at arity 2 **+ the BIF body** |
//! | `bif trivial (arity 9)` | *K* `bif` with 9 args, trivial hook | the path at **arity 9** (reused scratch) |
//!
//! The per-kind attribution subtracts the `dispatch/noop` baseline from each
//! floor-adjusted `ns/instruction`, so each figure is the **marginal** cost of that
//! instruction kind over dispatching a no-op label.
//!
//! Run it with:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --test bif_cost -- --nocapture
//! ```
//!
//! The counters of the allocation proof are process-global, so the whole file is
//! one `#[test]` (libtest would otherwise run several tests concurrently in the
//! same process and contaminate the counts and the timing).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::hint::black_box;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use beam_common::{BeamError, Result};
use beam_engine::{ExecutionContext, Machine, MinimalContext};
use beam_loader::{
    Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand, NO_FAIL,
};
use beam_term::{is_small, make_small, signed_value, Atom, Eterm};

// ---------------------------------------------------------------------------
// The counting allocator (the structural allocation proof)
// ---------------------------------------------------------------------------

/// A `#[global_allocator]` wrapper: `System` plus two counters, counted only while
/// [`COUNTING`] is set (so the timing loops pay nothing but one relaxed atomic
/// load per allocation).
struct Counting;

/// Whether the counters below are currently armed.
static COUNTING: AtomicBool = AtomicBool::new(false);
/// `alloc`/`alloc_zeroed`/`realloc` calls observed while armed.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Bytes requested by those calls.
static BYTES: AtomicUsize = AtomicUsize::new(0);

/// Record one allocation of `bytes` bytes (atomics only - allocation-free itself).
#[inline]
fn note(bytes: usize) {
    if COUNTING.load(Ordering::Relaxed) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(bytes, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to the `System` allocator with the same `Layout`,
// so the (ptr, layout) pairing the caller released is preserved exactly; the
// counters are plain atomics and allocate nothing.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Run `f` with the counters armed, returning `(allocations, bytes, f's value)`.
fn measure_allocs<T>(f: impl FnOnce() -> T) -> (usize, usize, T) {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let value = f();
    COUNTING.store(false, Ordering::Relaxed);
    (
        ALLOCATIONS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
        value,
    )
}

// ---------------------------------------------------------------------------
// Shape of the measurement
// ---------------------------------------------------------------------------

/// The number of timed samples per workload; the report quotes the **median**.
const SAMPLES: usize = 5;

/// The number of straight-line instructions in each kind-isolating body (the
/// `label; return` wrapper adds two more to the executed count).
const KIND_OPS: usize = 2048;

/// The value the **trivial** hook returns. It ignores its arguments, so the only
/// work a `gc_bif`/`bif`/`bif0` pays while this hook is installed is engine work
/// plus one constant-materialising closure body.
const TRIVIAL_VALUE: i64 = 7;

/// The whole-runtime per-registered-BIF-call wall time this file attributes against
/// (measured by the runtime crate from a real module; quoted here so the
/// conclusion can be printed without depending on that crate).
const HOST_BIF_CALL_NS: f64 = 10_500.0;

// ---------------------------------------------------------------------------
// The BIF hooks (the interpreter's `ctx.call_bif`)
// ---------------------------------------------------------------------------

/// The **real** `erlang:'+'/2` / `'-'/2` over small integers - the same body
/// `tests/engine_bench.rs` and `tests/slice.rs` use. Installed, the measured time
/// includes the **BIF body** (the string comparisons, the `is_small` guards, the
/// `signed_value` untagging and the `make_small` retag).
fn arith_hook(module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
    if module != "erlang" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if !is_small(args[0]) || !is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    let (a, b) = (signed_value(args[0]), signed_value(args[1]));
    match function {
        "+" => Ok(make_small(a + b)),
        "-" => Ok(make_small(a - b)),
        _ => Err(BeamError::BadArg),
    }
}

/// The **do-nothing** hook: it ignores `module`/`function`/`arity`/`args` and
/// returns the constant [`TRIVIAL_VALUE`]. Its body is a single `make_small` of a
/// compile-time constant, so the measured time is the engine's path (dispatch,
/// import lookup, argument marshalling, the `&mut dyn` call, the destination
/// store) plus this one call.
fn trivial_hook(_module: &str, _function: &str, _arity: u32, _args: &[Eterm]) -> Result<Eterm> {
    Ok(make_small(TRIVIAL_VALUE))
}

// ---------------------------------------------------------------------------
// Synthetic-module scaffolding (kept local so no shared test support is touched)
// ---------------------------------------------------------------------------

/// Wrap `body` in a one-function module exported as `name/arity`:
/// `[Label(entry)] ++ body ++ [Return]`, the entry label being `Label(1)`.
///
/// `Function::blocks` is left empty: the interpreter does not consult it.
fn finish_module(mut module: Module, name: &str, arity: u32, body: Vec<Instr>) -> (Module, Atom) {
    let function = module.atom_table.intern(name);
    module.name = function;
    let entry = Label(1);
    let mut code = Vec::with_capacity(body.len() + 2);
    code.push(Instr::Label(entry));
    code.extend(body);
    code.push(Instr::Return);
    let labels = collect_labels(&code);
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

/// The module-wide `Label -> code offset` map the engine rebuilds per run.
fn collect_labels(code: &[Instr]) -> BTreeMap<Label, usize> {
    code.iter()
        .enumerate()
        .filter_map(|(index, instr)| match instr {
            Instr::Label(label) => Some((*label, index)),
            _ => None,
        })
        .collect()
}

/// Intern `erlang:function/arity` into `module` and register it, returning the
/// [`ImportRef`] the body's `gc_bif`/`bif`/`bif0` needs.
fn intern_import(module: &mut Module, function: &str, arity: u32) -> ImportRef {
    let erlang = module.atom_table.intern("erlang");
    let function_atom = module.atom_table.intern(function);
    module.imports.push(Import {
        module: erlang,
        function: function_atom,
        arity,
    });
    ImportRef((module.imports.len() - 1) as u32)
}

/// `ops` chained `gc_bif`s computing `x0 = '+'/2(x0, 1)` (arity 2, the
/// stack-array marshalling path). The module has one import and one entry label.
fn gcbif_module_ops(name: &str, ops: usize) -> (Module, Atom) {
    let mut module = Module::default();
    let plus = intern_import(&mut module, "+", 2);
    let body = vec![
        Instr::GcBif {
            arity: 2,
            bif: plus,
            fail: NO_FAIL,
            live: 2,
            args: vec![Operand::X(0), Operand::Integer(1)],
            dst: Operand::X(0),
        };
        ops
    ];
    finish_module(module, name, 2, body)
}

/// The standard [`KIND_OPS`]-instruction `gc_bif` body.
fn gcbif_module(name: &str) -> (Module, Atom) {
    gcbif_module_ops(name, KIND_OPS)
}

/// `ops` chained `bif0`s - the arity-0 BIF path (an import of arity 0, no
/// arguments, no fail label).
fn bif0_module(name: &str, ops: usize) -> (Module, Atom) {
    let mut module = Module::default();
    let target = intern_import(&mut module, "self", 0);
    let body = vec![Instr::Bif0 { bif: target, dst: Operand::X(0) }; ops];
    finish_module(module, name, 2, body)
}

/// `ops` chained `bif`s of `arity` register arguments `x(0)..x(arity)`.
///
/// Arity `<= 2` marshals through the interpreter's fixed stack array; a wider
/// arity goes through the machine's reusable `args_scratch` buffer. The function
/// is exported at `arity` so `run_export` seeds that many registers.
fn wide_bif_module(name: &str, arity: u8, ops: usize) -> (Module, Atom) {
    let mut module = Module::default();
    let target = intern_import(&mut module, "wide", arity as u32);
    let args: Vec<Operand> = (0..arity as u32).map(Operand::X).collect();
    let body = vec![
        Instr::Bif {
            arity,
            bif: target,
            fail: NO_FAIL,
            args,
            dst: Operand::X(0),
        };
        ops
    ];
    finish_module(module, name, arity as u32, body)
}

// ---------------------------------------------------------------------------
// The timing harness (same shape as `tests/engine_bench.rs`)
// ---------------------------------------------------------------------------

/// One workload's measurement: the per-sample wall-clock totals and the last run's
/// result (asserted by the workload's builder).
struct Report {
    /// The printed workload name.
    label: String,
    /// The exact number of instructions one run executes.
    instructions_per_run: usize,
    /// The number of runs per sample.
    iterations: u64,
    /// The per-sample wall clock, in sample order.
    samples: Vec<Duration>,
    /// The last run's result.
    result: Eterm,
}

impl Report {
    /// Nanoseconds per **run** for one sample.
    fn ns_per_run(&self, sample: Duration) -> f64 {
        sample.as_secs_f64() * 1e9 / self.iterations as f64
    }

    /// Nanoseconds per **instruction** for one sample.
    fn ns_per_instruction(&self, sample: Duration) -> f64 {
        self.ns_per_run(sample) / self.instructions_per_run as f64
    }

    /// The fastest sample (the least-noise estimate).
    fn best(&self) -> Duration {
        *self
            .samples
            .iter()
            .min()
            .expect("every report has at least one sample")
    }

    /// The median sample.
    fn median(&self) -> Duration {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }

    /// The reported `ns/run` (median sample).
    fn median_ns_per_run(&self) -> f64 {
        self.ns_per_run(self.median())
    }

    /// The reported `ns/instruction` (median sample).
    fn median_ns_per_instruction(&self) -> f64 {
        self.ns_per_instruction(self.median())
    }
}

/// Warm up once, then time [`SAMPLES`] samples of `iterations` runs each.
fn measure(
    label: impl Into<String>,
    instructions_per_run: usize,
    iterations: u64,
    mut run: impl FnMut() -> Result<Eterm>,
) -> Report {
    assert!(
        instructions_per_run > 0 && iterations > 0,
        "a report must divide by a positive count"
    );
    let mut result = run().expect("the warm-up run must succeed");
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..iterations {
            result = run().expect("every timed run must succeed");
        }
        samples.push(start.elapsed());
    }
    Report {
        label: label.into(),
        instructions_per_run,
        iterations,
        samples,
        result,
    }
}

/// Time a raw `op` closure: warm up, then take the median of [`SAMPLES`] samples of
/// `iterations` calls; report ns per call.
///
/// Used for the two figures the workload harness cannot express - the raw
/// `&mut dyn ExecutionContext::call_bif` round-trip and the atom-table lookups.
fn time_ns_per_op(iterations: u64, mut op: impl FnMut()) -> f64 {
    assert!(iterations > 0);
    for _ in 0..(iterations / 10).max(1) {
        op();
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..iterations {
            op();
        }
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    samples[samples.len() / 2].as_secs_f64() * 1e9 / iterations as f64
}

// ---------------------------------------------------------------------------
// Execution-count probes: prove every BIF in a body actually ran
// ---------------------------------------------------------------------------

/// Run `function/arity` once with a **counting** hook and return the number of BIF
/// calls the engine made.
///
/// The counter is the structural proof that a body of *K* BIF instructions really
/// performed *K* calls (the trivial hook returns a constant, so the *result* alone
/// would not show how many ran).
fn count_bif_calls(module: &Module, function: Atom, arity: u32, args: &[Eterm]) -> u64 {
    let calls = Rc::new(Cell::new(0u64));
    let counter = Rc::clone(&calls);
    let mut ctx = MinimalContext::with_bif_hook(Box::new(move |_m, _f, _a, _args| {
        counter.set(counter.get() + 1);
        Ok(make_small(0))
    }));
    let mut machine = Machine::new();
    machine
        .run_export(module, function, arity, args, &mut ctx)
        .expect("the counting probe run must succeed");
    calls.get()
}

// ---------------------------------------------------------------------------
// The workloads
// ---------------------------------------------------------------------------

/// The **setup floor**: an empty body, so one run executes exactly two
/// instructions (`label`, `return`). Everything above it is `run_export`'s fixed
/// per-run cost (reset + the label index + the bottom frame).
fn bench_setup_empty() -> Report {
    let (module, function) = finish_module(Module::default(), "cost_empty", 2, Vec::new());
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(trivial_hook));
    let report = measure("setup/empty", 2, 20_000, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(report.result, make_small(1));
    report
}

/// The **dispatch baseline**: [`KIND_OPS`] redundant shared-label no-ops, so the
/// per-run label index stays 2 entries wide and the measurement isolates dispatch.
fn bench_dispatch_noop() -> Report {
    let body = vec![Instr::Label(Label(2)); KIND_OPS];
    let (module, function) = finish_module(Module::default(), "cost_noop", 2, body);
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(trivial_hook));
    let report = measure("dispatch/noop", KIND_OPS + 2, 800, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(report.result, make_small(1));
    report
}

/// A straight-line `move` body: the operand read + write + store cost, the
/// reference point for the [`bench_test_heap`] difference.
fn bench_move() -> Report {
    let body: Vec<Instr> = (0..KIND_OPS)
        .map(|step| {
            if step % 2 == 0 {
                Instr::Move {
                    src: Operand::X(0),
                    dst: Operand::X(1),
                }
            } else {
                Instr::Move {
                    src: Operand::X(1),
                    dst: Operand::X(0),
                }
            }
        })
        .collect();
    let (module, function) = finish_module(Module::default(), "cost_move", 2, body);
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(trivial_hook));
    let report = measure("move/2", KIND_OPS + 2, 800, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(report.result, make_small(1));
    report
}

/// A straight-line `test_heap` body: each instruction pays one **`ensure_heap`**
/// vtable call (the no-op `MinimalContext` body plus the `&mut x`/`&mut stack`
/// root-set construction), and nothing else. Priced against the `move` row and the
/// dispatch baseline.
fn bench_test_heap() -> Report {
    let body = vec![
        Instr::TestHeap {
            heap_need: HeapNeed::Words(5),
            live: 2,
        };
        KIND_OPS
    ];
    let (module, function) = finish_module(Module::default(), "cost_test_heap", 2, body);
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(trivial_hook));
    let report = measure("test_heap/2", KIND_OPS + 2, 800, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(report.result, make_small(1));
    report
}

/// The **arity-0** BIF path: [`KIND_OPS`] `bif0`s with the trivial hook. The engine
/// marshals no arguments, so this is the path's fixed overhead (dispatch + import
/// lookup + the vtable call + the destination store).
fn bench_bif0_trivial() -> Report {
    let (module, function) = bif0_module("cost_bif0", KIND_OPS);
    let args = [make_small(1), make_small(0)];
    let called = count_bif_calls(&module, function, 2, &args);
    assert_eq!(
        called, KIND_OPS as u64,
        "every one of the {KIND_OPS} `bif0`s must have performed a call"
    );
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(trivial_hook));
    let report = measure("bif0 trivial (arity 0)", KIND_OPS + 2, 400, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(report.result, make_small(TRIVIAL_VALUE));
    report
}

/// The **arity-2** BIF path with the **trivial** hook: the engine's per-call cost
/// alone (the stack-array marshalling path). This is the number the conclusion
/// calls the *engine side* of a BIF call.
fn bench_gcbif_trivial() -> Report {
    let (module, function) = gcbif_module("cost_gcbif_trivial");
    let args = [make_small(1), make_small(0)];
    let called = count_bif_calls(&module, function, 2, &args);
    assert_eq!(
        called, KIND_OPS as u64,
        "every one of the {KIND_OPS} `gc_bif`s must have performed a call"
    );
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(trivial_hook));
    let report = measure("gc_bif trivial (arity 2)", KIND_OPS + 2, 400, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(
        report.result,
        make_small(TRIVIAL_VALUE),
        "the trivial hook's constant is what the chain keeps"
    );
    report
}

/// The **arity-2** BIF path with the **real arithmetic** hook: the engine's cost
/// **plus the BIF body**. The difference from [`bench_gcbif_trivial`] is the BIF's
/// own work.
fn bench_gcbif_arith() -> Report {
    let (module, function) = gcbif_module("cost_gcbif_arith");
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let report = measure("gc_bif arith (arity 2)", KIND_OPS + 2, 400, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(
        report.result,
        make_small(1 + KIND_OPS as i64),
        "every gc_bif must have added one"
    );
    report
}

/// The **arity-9** BIF path with the trivial hook: the wide marshalling path
/// (`args_scratch` reuse, no per-call allocation). Priced against the arity-0 and
/// arity-2 rows, the slope with arity is the per-argument `read_operand` cost.
fn bench_wide_trivial() -> Report {
    const WIDE: u8 = 9;
    let (module, function) = wide_bif_module("cost_wide", WIDE, KIND_OPS);
    let args = [make_small(0); WIDE as usize];
    let called = count_bif_calls(&module, function, WIDE as u32, &args);
    assert_eq!(
        called, KIND_OPS as u64,
        "every one of the {KIND_OPS} wide `bif`s must have performed a call"
    );
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(trivial_hook));
    let report = measure("bif trivial (arity 9)", KIND_OPS + 2, 400, || {
        machine.run_export(&module, function, WIDE as u32, &args, &mut ctx)
    });
    assert_eq!(report.result, make_small(TRIVIAL_VALUE));
    report
}

// ---------------------------------------------------------------------------
// Direct micro-measurements (outside the workload harness)
// ---------------------------------------------------------------------------

/// One free-standing micro-measurement.
struct Micro {
    label: String,
    ns: f64,
}

/// The raw `&mut dyn ExecutionContext::call_bif` **round-trip**: the vtable
/// crossing plus the trivial hook body and the `&[Eterm]` argument slice - no
/// interpreter dispatch, no import lookup, no marshalling.
fn micro_dyn_call() -> Micro {
    let mut ctx = MinimalContext::with_bif_hook(Box::new(trivial_hook));
    let call: &mut dyn ExecutionContext = &mut ctx;
    let args = [make_small(0), make_small(1)];
    let ns = time_ns_per_op(200_000, || {
        let _ = black_box(call.call_bif("erlang", "+", 2, &args));
    });
    Micro {
        label: "&mut dyn ExecutionContext::call_bif round-trip (trivial hook)".to_string(),
        ns,
    }
}

/// The cost of one `Module::atom_table.lookup(name)` - half of what the *uncached*
/// import resolution pays per call (the per-run cache replaces both lookups).
fn micro_atom_lookup(module: &Module) -> Micro {
    let table = &module.atom_table;
    let ns = time_ns_per_op(500_000, || {
        let _ = black_box(table.lookup("erlang"));
    });
    Micro {
        label: "Module::atom_table.lookup(\"erlang\")".to_string(),
        ns,
    }
}

/// The cost of one `Module::atom_name(atom)` - the string side of one uncached
/// import resolution.
fn micro_atom_name(module: &Module) -> Micro {
    let atom = module
        .atom_table
        .lookup("erlang")
        .expect("`erlang` is interned by every module here");
    let ns = time_ns_per_op(500_000, || {
        let _ = black_box(module.atom_name(atom));
    });
    Micro {
        label: "Module::atom_name(atom)".to_string(),
        ns,
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Print the measurement table (workload, iterations, instructions/run, `ns/run`,
/// `ns/instruction` for the median and best samples).
fn report_table(reports: &[Report]) {
    println!(
        "bif_cost | engine `Machine::run_export` - per-BIF-call cost attribution \
         (median of {SAMPLES} samples; best sample in brackets)"
    );
    println!(
        "bif_cost | {:<26} {:>7} {:>9} {:>12} {:>12} {:>14}",
        "workload", "iters", "instr/run", "ns/run", "ns/instr", "ns/instr(best)"
    );
    for report in reports {
        println!(
            "bif_cost | {:<26} {:>7} {:>9} {:>12.1} {:>12.2} {:>14.2}",
            report.label,
            report.iterations,
            report.instructions_per_run,
            report.median_ns_per_run(),
            report.median_ns_per_instruction(),
            report.ns_per_instruction(report.best()),
        );
    }
    println!();
}

/// Print the per-kind attribution and the engine-vs-host conclusion.
///
/// Every figure is a **difference of medians**: the workload's `ns/instruction`
/// minus the `dispatch/noop` baseline, with the per-run setup floor
/// ([`bench_setup_empty`]) removed first.
fn report_breakdown(reports: &[Report], micros: &[Micro]) {
    let by_label = |needle: &str| reports.iter().find(|r| r.label.starts_with(needle));
    let floor = by_label("setup/empty")
        .expect("the setup floor is always measured")
        .median_ns_per_run();
    let adjusted = |report: &Report| -> f64 {
        (report.median_ns_per_run() - floor).max(0.0) / report.instructions_per_run as f64
    };
    let baseline = adjusted(by_label("dispatch/noop").expect("the dispatch baseline is measured"));
    let marginal = |needle: &str| -> f64 {
        adjusted(by_label(needle).unwrap_or_else(|| panic!("{needle} must be measured"))) - baseline
    };

    println!(
        "bif_cost | attribution - marginal ns per instruction over the dispatch baseline; every \
         figure is floor-adjusted by the {floor:.1} ns/run setup floor"
    );
    println!("bif_cost |   dispatch baseline (no-op label)     {baseline:>8.2} ns/instr");
    for (needle, note) in [
        ("move/2", "operand read + write + store"),
        (
            "test_heap/2",
            "one ensure_heap vtable call (no-op body + root-set slice)",
        ),
        ("bif0 trivial", "BIF path, arity 0"),
        ("gc_bif trivial", "BIF path, arity 2 (stack array) - ENGINE side"),
        ("bif trivial (arity 9)", "BIF path, arity 9 (reused scratch buffer)"),
        ("gc_bif arith", "BIF path, arity 2 + the real arithmetic body"),
    ] {
        if let Some(report) = by_label(needle) {
            let ns = adjusted(report);
            println!(
                "bif_cost |   {:<33} {:>8.2} ns/instr  (+{:>7.2} vs baseline)  {}",
                report.label,
                ns,
                ns - baseline,
                note,
            );
        }
    }

    let engine_arity0 = marginal("bif0 trivial");
    let engine_arity2 = marginal("gc_bif trivial");
    let engine_arity9 = marginal("bif trivial (arity 9)");
    let arith = marginal("gc_bif arith");
    let bif_work = arith - engine_arity2;
    let ensure_heap = marginal("test_heap/2");

    println!();
    println!(
        "bif_cost | per-call engine cost by arity (marginal over dispatch): arity 0 = \
         {engine_arity0:.2} ns, arity 2 = {engine_arity2:.2} ns, arity 9 = {engine_arity9:.2} ns \
         (slope ~ {:.3} ns per extra argument)",
        (engine_arity9 - engine_arity0) / 9.0
    );
    println!(
        "bif_cost | one `ensure_heap` vtable call (with a no-op body): {ensure_heap:.2} ns \
         (marginal over dispatch)"
    );
    println!(
        "bif_cost | BIF-body work = `gc_bif arith` - `gc_bif trivial` = {bif_work:.2} ns \
         (the real arithmetic hook's own body: name checks + guards + untag/retag)"
    );

    println!();
    println!("bif_cost | direct micro-measurements (not in the workload harness):");
    for micro in micros {
        println!("bif_cost |   {:<56} {:>8.3} ns", micro.label, micro.ns);
    }

    println!();
    println!(
        "bif_cost | CONCLUSION - engine-side per BIF call ~ {engine_arity2:.2} ns (arity 2 with a \
         do-nothing hook: dispatch + import-cache hit + argument marshalling + the `&mut dyn` \
         crossing + the destination store). Against the runtime's {HOST_BIF_CALL_NS:.0} ns/ \
         registered call this is {:.4}% engine-side, i.e. ~{:.2}% host-side \
         (~{:.0} ns/call).",
        100.0 * engine_arity2 / HOST_BIF_CALL_NS,
        100.0 * (HOST_BIF_CALL_NS - engine_arity2) / HOST_BIF_CALL_NS,
        HOST_BIF_CALL_NS - engine_arity2,
    );
    println!(
        "bif_cost |   honest limits: the engine-side figure still contains the trivial hook's own \
         closure body and the `&mut dyn` vtable crossing (the latter is measured directly above); \
         the arity-0/2/9 rows split the per-argument marshalling out, and the `arith - trivial` \
         delta is the BIF body. The per-run import-table RESOLUTION is amortised and cannot be \
         isolated per call from outside the crate; the atom-table rows above bound what the \
         uncached fallback would pay."
    );
    println!();
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[test]
fn engine_reports_per_bif_call_cost() {
    // One test (not several) so the workloads never run concurrently and
    // contaminate each other's timing, and so the process-global allocation
    // counters observe exactly the region under test.
    let reports = vec![
        bench_setup_empty(),
        bench_dispatch_noop(),
        bench_move(),
        bench_test_heap(),
        bench_bif0_trivial(),
        bench_gcbif_trivial(),
        bench_gcbif_arith(),
        bench_wide_trivial(),
    ];

    // The atom-table cost is measured on a module identical in shape to the
    // workloads (one `erlang` import), so the figure is representative.
    let (atom_module, _) = gcbif_module("cost_atom_probe");
    let micros = vec![
        micro_dyn_call(),
        micro_atom_lookup(&atom_module),
        micro_atom_name(&atom_module),
    ];

    report_table(&reports);
    report_breakdown(&reports, &micros);

    // --- structural invariants (never a performance threshold) --------------
    assert!(
        reports.len() == 8,
        "every synthetic workload must have been measured"
    );
    for report in &reports {
        let ns = report.median_ns_per_instruction();
        assert!(
            ns.is_finite() && (0.01..100_000.0).contains(&ns),
            "{} reported an implausible {ns} ns/instruction",
            report.label
        );
    }
    for micro in &micros {
        assert!(
            micro.ns.is_finite() && micro.ns > 0.0,
            "{} reported an implausible {} ns",
            micro.label,
            micro.ns
        );
    }

    // The allocation proof: the per-BIF path allocates nothing, so a body of
    // `2 * KIND_OPS` BIFs allocates exactly as many times as one of `KIND_OPS`
    // (both modules share one import and one entry label, so their per-run costs
    // are identical).
    let (short_module, short_fn) = gcbif_module_ops("cost_alloc_short", KIND_OPS);
    let (long_module, long_fn) = gcbif_module_ops("cost_alloc_long", 2 * KIND_OPS);
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(trivial_hook));
    // Warm the machine's vectors (capacity, not per-call traffic) outside the
    // measured regions.
    for (module, function) in [(&short_module, short_fn), (&long_module, long_fn)] {
        machine
            .run_export(module, function, 2, &args, &mut ctx)
            .expect("the warm-up run must succeed");
    }
    let (short_allocs, short_bytes, short_result) = measure_allocs(|| {
        machine.run_export(&short_module, short_fn, 2, &args, &mut ctx)
    });
    let (long_allocs, long_bytes, long_result) = measure_allocs(|| {
        machine.run_export(&long_module, long_fn, 2, &args, &mut ctx)
    });
    assert_eq!(
        short_result.expect("the short chain returns"),
        make_small(TRIVIAL_VALUE)
    );
    assert_eq!(
        long_result.expect("the long chain returns"),
        make_small(TRIVIAL_VALUE)
    );
    println!(
        "bif_cost | allocation proof: {KIND_OPS} gc_bifs -> {short_allocs} allocations \
         ({short_bytes} bytes)/run; {} gc_bifs -> {long_allocs} allocations ({long_bytes} \
         bytes)/run (per-run only: the label index + the import table)",
        2 * KIND_OPS
    );
    assert_eq!(
        long_allocs, short_allocs,
        "the per-BIF path allocates: a {KIND_OPS}-instruction body allocated {short_allocs} times \
         but a {} -instruction body allocated {long_allocs} times",
        2 * KIND_OPS
    );
    assert_eq!(
        long_bytes, short_bytes,
        "the per-BIF allocation bytes must not grow with the instruction count either"
    );
    assert!(
        short_allocs <= 16,
        "a whole run must be a handful of per-run allocations, not {short_allocs}"
    );
}
