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

//! **Allocation proof: the per-BIF interpreter path allocates nothing.**
//!
//! `tests/engine_bench.rs` measures the *time* of an interpreted `gc_bif`; this
//! suite pins the complementary, load-independent property its `CONTEXT.md`
//! claims — that a `gc_bif`/`bif`/`bif0`/`call_ext*` instruction marshals its
//! arguments and dispatches the BIF **without touching the heap allocator**, at
//! *every* arity.
//!
//! # Method — a counting global allocator, gated to the measured region
//!
//! This integration test is its own binary, so it installs its own
//! `#[global_allocator]`: a thin wrapper around [`System`] that increments two
//! counters (`alloc`/`realloc` calls and bytes) **while a flag is set**. The flag
//! is raised only around the `Machine::run_export` call under test, and cleared
//! immediately after, so the counters observe exactly one run. Only this binary is
//! rebuilt when the allocator changes — the library under test is the same artifact
//! every other suite links.
//!
//! A run is *not* allocation-free as a whole: `run_export` rebuilds the module-wide
//! label index (`interpreter::build_label_map`, one `Vec`) and its import table
//! (one `Vec` + one `Box<str>` per name) **once per run**. Those are per-run, not
//! per-instruction, costs. So the assertions below never demand a zero total; they
//! demand the only thing that matters here — that the count does not grow with the
//! number of BIF instructions, nor with the arity:
//!
//! * a body of `N` chained `gc_bif` instructions and a body of `2 * N` of them
//!   (same module shape: one import, one entry label) must allocate **exactly** the
//!   same number of times — if a BIF call allocated, the second body would show
//!   `N` extra allocations, not zero;
//! * a `call_ext` of arity 8 (which marshals through the interpreter's fixed stack
//!   array) and one of arity 9 (which marshals through the machine's reusable
//!   scratch buffer) must allocate **exactly** the same number of times — if the
//!   wide path minted a `Vec` per call, the arity-9 module would show one extra
//!   allocation per run.
//!
//! Both bodies are also *checked for the value they must produce*, so a path that
//! silently did nothing cannot pass by not running.
//!
//! # Running it
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --test bif_alloc -- --nocapture
//! ```
//!
//! The counters are process-global, so the whole proof lives in **one** `#[test]`
//! (libtest would otherwise run several tests concurrently in the same process and
//! contaminate the counts).

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use beam_common::{BeamError, Result};
use beam_engine::{Machine, MinimalContext};
use beam_loader::{Export, Function, Import, ImportRef, Instr, Label, Module, Operand, NO_FAIL};
use beam_term::{is_small, make_small, signed_value, Atom, Eterm};

// ---------------------------------------------------------------------------
// The counting allocator
// ---------------------------------------------------------------------------

/// The `#[global_allocator]` wrapper: `System` plus two counters, counted only
/// while [`COUNTING`] is set.
struct Counting;

/// Whether the counters below are currently armed.
static COUNTING: AtomicBool = AtomicBool::new(false);
/// `alloc`/`alloc_zeroed`/`realloc` calls observed while armed.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Bytes requested by those calls (a `realloc`'s new size).
static BYTES: AtomicUsize = AtomicUsize::new(0);

/// Record one allocation of `bytes` bytes. Called from the allocator hooks below;
/// deliberately allocation-free itself (atomics only).
#[inline]
fn note(bytes: usize) {
    if COUNTING.load(Ordering::Relaxed) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(bytes, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to the `System` allocator with the same
// `Layout`, so the (ptr, layout) pairing the callers released is preserved
// exactly; the counters are plain atomics and allocate nothing.
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
fn measure<T>(f: impl FnOnce() -> T) -> (usize, usize, T) {
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
// Synthetic modules and the BIF hook
// ---------------------------------------------------------------------------

/// `erlang:bench_sum/N` (the sole import of every module below): the small-int sum
/// of its arguments.
///
/// It allocates nothing — the shape every measured BIF must have for the counts to
/// mean anything.
fn sum_hook(module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
    if module != "erlang" || function != "bench_sum" || arity as usize != args.len() {
        return Err(BeamError::BadArg);
    }
    let mut total: i64 = 0;
    for arg in args {
        if !is_small(*arg) {
            return Err(BeamError::BadArith);
        }
        total += signed_value(*arg);
    }
    Ok(make_small(total))
}

/// Intern `erlang:bench_sum/arity` into `module` and return the [`ImportRef`] a
/// `gc_bif`/`call_ext` needs.
fn intern_sum_import(module: &mut Module, arity: u32) -> ImportRef {
    let erlang = module.atom_table.intern("erlang");
    let function = module.atom_table.intern("bench_sum");
    module.imports.push(Import {
        module: erlang,
        function,
        arity,
    });
    ImportRef((module.imports.len() - 1) as u32)
}

/// Wrap `body` in a one-function module exported as `name/arity`:
/// `[Label(entry)] ++ body ++ [Return]`, exactly the shape
/// `tests/engine_bench.rs` builds (kept local: `tests/support` is shared and must
/// not grow a synthetic-module builder for one file).
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

/// `ops` chained `gc_bif`s computing `x0 = x0 + 1` (`x0` starts at `1`, so the
/// result is `1 + ops`).
///
/// One import, one entry label and no other instructions: two modules built with
/// different `ops` have exactly the same *shape*, so their per-run costs (the label
/// index and the import table `run_export` rebuilds) are identical and any
/// allocation difference between them is attributable to the `gc_bif` path alone.
fn gcbif_module(ops: usize) -> (Module, Atom) {
    let mut module = Module::default();
    let import = intern_sum_import(&mut module, 2);
    let body = vec![
        Instr::GcBif {
            arity: 2,
            bif: import,
            fail: NO_FAIL,
            live: 2,
            args: vec![Operand::X(0), Operand::Integer(1)],
            dst: Operand::X(0),
        };
        ops
    ];
    finish_module(module, "alloc_gcbif", 2, body)
}

/// A single `call_ext` of `arity` register arguments (a `call_ext`, never
/// `call_ext_last`, so the module keeps one entry label and one import at every
/// arity).
fn call_ext_module(arity: u32) -> (Module, Atom) {
    let mut module = Module::default();
    let import = intern_sum_import(&mut module, arity);
    let body = vec![Instr::CallExt {
        target: import,
        arity,
    }];
    finish_module(module, "alloc_call_ext", arity, body)
}

// ---------------------------------------------------------------------------
// The proof
// ---------------------------------------------------------------------------

/// The instructions per body in the first comparison (`2 * CHAIN_OPS` for the
/// second). Large enough that one allocation per BIF would be unmissable.
const CHAIN_OPS: usize = 512;

/// The `call_ext` arities of the second comparison: 8 marshals through the
/// interpreter's fixed stack array, 9 through the machine's scratch buffer.
const NARROW_ARITY: u32 = 8;
const WIDE_ARITY: u32 = NARROW_ARITY + 1;

#[test]
fn the_per_bif_path_allocates_nothing() {
    let (narrow_module, narrow_function) = gcbif_module(CHAIN_OPS);
    let (long_module, long_function) = gcbif_module(2 * CHAIN_OPS);
    let (narrow_ext, narrow_ext_function) = call_ext_module(NARROW_ARITY);
    let (wide_ext, wide_ext_function) = call_ext_module(WIDE_ARITY);

    let mut ctx = MinimalContext::with_bif_hook(Box::new(sum_hook));
    let mut machine = Machine::new();

    // Warm every module up once, outside the measured regions: the first run pays
    // the one-time growth of the machine's own vectors (the frame/stack vectors and
    // the argument scratch buffer), which is capacity, not per-call traffic.
    let warm = make_small(1);
    let warm_args = [warm; WIDE_ARITY as usize];
    for (module, function, arity) in [
        (&narrow_module, narrow_function, 2u32),
        (&long_module, long_function, 2),
        (&narrow_ext, narrow_ext_function, NARROW_ARITY),
        (&wide_ext, wide_ext_function, WIDE_ARITY),
    ] {
        machine
            .run_export(module, function, arity, &warm_args[..arity as usize], &mut ctx)
            .expect("the warm-up run must succeed");
    }

    // --- 1. the count does not grow with the number of `gc_bif`s -------------
    let args = [make_small(1), make_small(0)];
    let (nil_allocs, nil_bytes, nil_result) = measure(|| {
        machine.run_export(&narrow_module, narrow_function, 2, &args, &mut ctx)
    });
    let (long_allocs, long_bytes, long_result) = measure(|| {
        machine.run_export(&long_module, long_function, 2, &args, &mut ctx)
    });
    assert_eq!(
        nil_result.expect("the short gc_bif chain returns"),
        make_small(1 + CHAIN_OPS as i64),
        "every gc_bif must have added one"
    );
    assert_eq!(
        long_result.expect("the long gc_bif chain returns"),
        make_small(1 + 2 * CHAIN_OPS as i64),
        "every gc_bif must have added one"
    );
    println!(
        "bif_alloc | gc_bif '+'/2: {CHAIN_OPS} instructions -> {nil_allocs} allocations \
         ({nil_bytes} bytes)/run; {} instructions -> {long_allocs} allocations ({long_bytes} \
         bytes)/run (per-run only: the label index + the import table)",
        2 * CHAIN_OPS
    );
    assert_eq!(
        long_allocs, nil_allocs,
        "a {CHAIN_OPS}-instruction `gc_bif` body allocated {nil_allocs} times but the same body \
         with {} instructions allocated {long_allocs} times — the per-BIF path allocates",
        2 * CHAIN_OPS
    );
    assert_eq!(
        long_bytes, nil_bytes,
        "the `gc_bif` allocation *bytes* must not grow with the instruction count either"
    );

    // --- 2. neither does it grow with the arity (the wide scratch path) ------
    let narrow_args = [warm; NARROW_ARITY as usize];
    let wide_args = [warm; WIDE_ARITY as usize];
    let (narrow_allocs, narrow_bytes, narrow_result) = measure(|| {
        machine.run_export(
            &narrow_ext,
            narrow_ext_function,
            NARROW_ARITY,
            &narrow_args,
            &mut ctx,
        )
    });
    let (wide_allocs, wide_bytes, wide_result) = measure(|| {
        machine.run_export(
            &wide_ext,
            wide_ext_function,
            WIDE_ARITY,
            &wide_args,
            &mut ctx,
        )
    });
    assert_eq!(
        narrow_result.expect("the arity-8 call_ext returns"),
        make_small(NARROW_ARITY as i64),
        "the hook sums its {NARROW_ARITY} arguments"
    );
    assert_eq!(
        wide_result.expect("the arity-9 call_ext returns"),
        make_small(WIDE_ARITY as i64),
        "the hook sums its {WIDE_ARITY} arguments"
    );
    println!(
        "bif_alloc | call_ext arity {NARROW_ARITY} (stack array) -> {narrow_allocs} allocations \
         ({narrow_bytes} bytes)/run; arity {WIDE_ARITY} (reused scratch buffer) -> {wide_allocs} \
         allocations ({wide_bytes} bytes)/run"
    );
    assert_eq!(
        wide_allocs, narrow_allocs,
        "the arity-{WIDE_ARITY} `call_ext` allocated {wide_allocs} times against the arity-\
         {NARROW_ARITY} call's {narrow_allocs}: the wide path allocates per call"
    );
    assert_eq!(
        wide_bytes, narrow_bytes,
        "the wide `call_ext` must not allocate more bytes than the narrow one"
    );

    // --- 3. the absolute per-run floor, so a gross regression is obvious -----
    // The only allocations a run makes are the per-run label index (`Vec` of one
    // entry) and the per-run import table (`Vec` of one entry + two `Box<str>`), so
    // a run of any of these modules is a handful of allocations — never one per
    // instruction.
    assert!(
        nil_allocs <= 16,
        "a whole run must be a handful of per-run allocations, not {nil_allocs}"
    );
}
