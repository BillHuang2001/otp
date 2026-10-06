//! Micro-benchmark: the interpreter against the native JIT tiers.
//!
//! The Milestone-1 JIT tiers are **mechanisms**, not optimisers (Part II §F):
//! correctness first, performance work follows. This file is the honest
//! measurement of where they stand today — it prints `ns/op` (and `ops/s`) per
//! workload and tier, and asserts **only correctness** (every tier's result equals
//! the interpreter's). It never asserts that a JIT is faster: that is
//! environment-dependent and would be flaky; the printed numbers are the evidence.
//!
//! Two workloads, chosen to hit the two shapes the baseline tier treats
//! differently:
//!
//! * **`fib(20)`** — call-heavy recursion over the real `.beam` fixture
//!   (`beam_loader/tests/fixtures/slice.beam`, reused by relative path so there is
//!   a single source of truth for the image). Thousands of `call`/`gc_bif`/
//!   `return`, i.e. the driver loop's interpreter-step path dominates.
//! * **`kernel/2`** — a hand-built synthetic module (the loader is not involved;
//!   see `tests/opcodes.rs` for the same scaffolding pattern) whose body is a
//!   straight-line chain of eight `erlang:'+'/2` `gc_bif`s accumulating on
//!   x-registers, ending in `return`. This is exactly the shape the baseline
//!   tier's inlined guarded small-int fast path (`try_inline_gc_bif`) lowers to
//!   native machine code with no interpreter round-trip.
//!
//! # Method
//!
//! For each `(workload, tier)`:
//!
//! 1. one warm-up run (not timed),
//! 2. `iterations` timed [`run_export`] calls on a reused [`Machine`] (both entries
//!    `reset()` the machine at the start of a run, so this is exactly the cost of
//!    the call, with no per-iteration `Machine` construction noise),
//! 3. the elapsed wall-clock time from [`Instant`], divided by `iterations`.
//!
//! The interpreter is timed through [`Machine::run_export`]; a native tier builds
//! its backend once, calls [`JitBackend::compile`] once up front (which also
//! validates that the module compiles), and then times [`JitBackend::run_export`]
//! calls.
//!
//! **Honest caveat, printed with the numbers:** neither native backend has a
//! cross-run code cache yet, so `run_export` recompiles the module on *every* call
//! (see `beam_engine/CONTEXT.md`, Known Issues). Each native `ns/op` below
//! therefore includes a full compile; a steady-state compile latency is measured
//! and printed alongside so the reader can attribute the cost.
//!
//! Run with `--nocapture` to see the table.
//!
//! [`run_export`]: JitBackend::run_export

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_engine::jit::baseline::BaselineJit;
use beam_engine::{available_tiers, JitBackend, Machine, MinimalContext, Tier};
use beam_loader::{
    load_file, Export, Function, Import, ImportRef, Instr, Label, Module, Operand, NO_FAIL,
};
use beam_term::{is_small, make_small, signed_value, Atom, Eterm};

/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// The slice BIF hook: `erlang:'+'/2` and `erlang:'-'/2` over small integers.
///
/// The same implementation serves both workloads (the synthetic kernel imports
/// `erlang:'+'/2`).
fn arith_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
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

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

/// Which workload is being timed (selects the iteration count).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `slice:fib/1` from the committed oracle fixture.
    Fib,
    /// The hand-built straight-line arithmetic kernel.
    Kernel,
}

impl Kind {
    /// The `(workload, tier)` iteration count.
    ///
    /// `fib(20)` is already a ~20 000-call workload, so a few dozen runs are
    /// measurable; the kernel is ~10 instructions, so it needs many thousands. The
    /// Cranelift tier pays a whole Cranelift JIT-module build *per call* (see the
    /// module docs), which costs orders of magnitude more than one kernel run, so
    /// it gets far fewer iterations to keep the whole file inside its ~2 s budget.
    fn iterations(self, tier: Tier) -> u32 {
        match (self, tier) {
            (Kind::Fib, Tier::Cranelift) => 5,
            (Kind::Fib, _) => 30,
            (Kind::Kernel, Tier::Cranelift) => 100,
            (Kind::Kernel, _) => 20_000,
        }
    }
}

/// A benchmark workload: a module, an exported `function/arity` and its arguments.
struct Workload {
    /// The human-readable label printed with the numbers.
    label: &'static str,
    /// The workload shape (selects the iteration count).
    kind: Kind,
    /// The module holding the export.
    module: Module,
    /// The exported function's name atom (in `module`'s atom table).
    function: Atom,
    /// The exported arity.
    arity: u32,
    /// The call arguments.
    args: Vec<Eterm>,
}

/// `slice:fib/1` over the committed oracle fixture.
fn fib_workload() -> Workload {
    let module = load_file(SLICE_BEAM).expect("load slice.beam");
    let function = atom(&module, "fib");
    Workload {
        label: "slice:fib/1 (20)",
        kind: Kind::Fib,
        module,
        function,
        arity: 1,
        args: vec![make_small(20)],
    }
}

/// The synthetic straight-line kernel: `kernel/2` is eight chained `erlang:'+'/2`
/// `gc_bif`s accumulating on `x(0)`/`x(1)`, then `return`.
///
/// The body deliberately stays inside the baseline tier's inlined fast path: the
/// only BIF is small-int `'+'`, both operands and the destination are (cacheable)
/// x-registers, so every instruction of the block is lowered to machine code and
/// the driver loop never takes an interpreter step.
fn kernel_workload() -> Workload {
    let mut module = Module::default();
    module.name = module.atom_table.intern("jit_bench_kernel");

    let plus_module = module.atom_table.intern("erlang");
    let plus_function = module.atom_table.intern("+");
    module.imports.push(Import {
        module: plus_module,
        function: plus_function,
        arity: 2,
    });
    let plus = ImportRef(0);

    let kernel = module.atom_table.intern("kernel");
    let entry = Label(1);
    module.exports.push(Export {
        function: kernel,
        arity: 2,
        label: entry,
    });

    let mut code = vec![Instr::Label(entry)];
    for step in 0..8 {
        // `x0 = x0 + x1` then `x1 = x1 + x0`, alternating: a dependency chain that
        // keeps both registers live across every operation.
        let (args, dst) = if step % 2 == 0 {
            (vec![Operand::X(0), Operand::X(1)], Operand::X(0))
        } else {
            (vec![Operand::X(1), Operand::X(0)], Operand::X(1))
        };
        code.push(Instr::GcBif {
            arity: 2,
            bif: plus,
            fail: NO_FAIL,
            live: 2,
            args,
            dst,
        });
    }
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
        name: kernel,
        arity: 2,
        code,
        blocks: Vec::new(),
        labels,
    });

    Workload {
        label: "kernel/2 (x0<->x1 chain)",
        kind: Kind::Kernel,
        module,
        function: kernel,
        arity: 2,
        args: vec![make_small(1), make_small(1)],
    }
}

// ---------------------------------------------------------------------------
// The native backends
// ---------------------------------------------------------------------------

/// The native [`JitBackend`] behind a [`Tier`], untied from its associated
/// `Compiled` type so the benchmark loop can hold one backend per tier.
enum NativeBackend {
    /// The native baseline tier (always in the roster).
    Baseline(BaselineJit),
    /// The optional Cranelift hot tier (feature-gated).
    #[cfg(feature = "jit-cranelift")]
    Cranelift(beam_engine::jit::cranelift::CraneliftJit),
}

impl NativeBackend {
    /// Build the backend for `tier`. Only called for a tier
    /// [`tier::is_available`](Tier::is_available) reports, so the interpreter —
    /// which has nothing to compile — never reaches this function.
    fn new(tier: Tier) -> NativeBackend {
        match tier {
            Tier::BaselineJit => NativeBackend::Baseline(BaselineJit::new()),
            #[cfg(feature = "jit-cranelift")]
            Tier::Cranelift => NativeBackend::Cranelift(
                beam_engine::jit::cranelift::CraneliftJit::new(),
            ),
            other => panic!("no native backend for tier {}", other.name()),
        }
    }

    /// Compile `module` once (the artifact is dropped: the point of the call here
    /// is validation plus a one-time compile measurement).
    fn compile(&mut self, module: &Module) -> Result<(), BeamError> {
        match self {
            NativeBackend::Baseline(backend) => backend.compile(module).map(|_| ()),
            #[cfg(feature = "jit-cranelift")]
            NativeBackend::Cranelift(backend) => backend.compile(module).map(|_| ()),
        }
    }

    /// Run the exported `function/arity`.
    fn run_export(
        &mut self,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut MinimalContext,
    ) -> Result<Eterm, BeamError> {
        match self {
            NativeBackend::Baseline(backend) => {
                backend.run_export(machine, module, function, arity, args, ctx)
            }
            #[cfg(feature = "jit-cranelift")]
            NativeBackend::Cranelift(backend) => {
                backend.run_export(machine, module, function, arity, args, ctx)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The timing harness
// ---------------------------------------------------------------------------

/// One `(workload, tier)` measurement.
struct Measurement {
    /// The timed tier.
    tier: Tier,
    /// The number of timed runs.
    iterations: u32,
    /// The total wall-clock time of the timed runs.
    elapsed: Duration,
    /// The one-time [`JitBackend::compile`] latency: `None` for the interpreter,
    /// `Some(steady-state compile)` for a native tier (measured after the warm-up,
    /// so it is what the timed loop pays per run rather than the inflated first
    /// compile).
    compile: Option<Duration>,
    /// The last run's result.
    result: Eterm,
}

/// Warm up once, then time `iterations` runs of `workload` on `tier`.
fn measure(tier: Tier, workload: &Workload) -> Measurement {
    let iterations = workload.kind.iterations(tier);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));

    // Both entries `reset()` the machine at the start of a run, so one machine can
    // be reused across the whole loop (the timed cost is the call, not the setup).
    match tier {
        Tier::Interpreter => {
            machine
                .run_export(
                    &workload.module,
                    workload.function,
                    workload.arity,
                    &workload.args,
                    &mut ctx,
                )
                .expect("the workload must run on the interpreter");
            let start = Instant::now();
            let mut result = make_small(0);
            for _ in 0..iterations {
                result = machine
                    .run_export(
                        &workload.module,
                        workload.function,
                        workload.arity,
                        &workload.args,
                        &mut ctx,
                    )
                    .expect("the workload must run on the interpreter");
            }
            Measurement {
                tier,
                iterations,
                elapsed: start.elapsed(),
                compile: None,
                result,
            }
        }
        native => {
            let mut backend = NativeBackend::new(native);
            backend
                .compile(&workload.module)
                .expect("the workload must compile on the native tier");

            backend
                .run_export(
                    &mut machine,
                    &workload.module,
                    workload.function,
                    workload.arity,
                    &workload.args,
                    &mut ctx,
                )
                .expect("the workload must run on the native tier");

            // Informational: a *steady-state* compile. (The first compile in a
            // process pays one-off mapping costs — an `mmap`/`mprotect` of a fresh
            // code buffer — so reporting that one would overstate the per-run cost
            // the timed loop actually pays.)
            let compile_start = Instant::now();
            backend
                .compile(&workload.module)
                .expect("the workload must compile on the native tier");
            let compile = compile_start.elapsed();

            let start = Instant::now();
            let mut result = make_small(0);
            for _ in 0..iterations {
                result = backend
                    .run_export(
                        &mut machine,
                        &workload.module,
                        workload.function,
                        workload.arity,
                        &workload.args,
                        &mut ctx,
                    )
                    .expect("the workload must run on the native tier");
            }
            Measurement {
                tier,
                iterations,
                elapsed: start.elapsed(),
                compile: Some(compile),
                result,
            }
        }
    }
}

/// Print one measurement line, plus the native tiers' compile cost.
fn report(measurement: &Measurement) {
    let per_op = measurement.elapsed.as_secs_f64() / f64::from(measurement.iterations);
    let per_op_ns = per_op * 1e9;
    println!(
        "jit_bench | tier={:<12} iterations={:<6} total={:>9.3} ms | {:>14.1} ns/op | {:>14.1} ops/s",
        measurement.tier.name(),
        measurement.iterations,
        measurement.elapsed.as_secs_f64() * 1e3,
        per_op_ns,
        if per_op > 0.0 { 1.0 / per_op } else { f64::INFINITY },
    );
    if let Some(compile) = measurement.compile {
        println!(
            "jit_bench |   {} compiles the module on every run_export (no cross-run code cache), \
             so each ns/op above includes a full compile; one steady-state compile = {:.3} us",
            measurement.tier.name(),
            compile.as_secs_f64() * 1e6,
        );
    }
}

/// The tiers to benchmark: the interpreter plus every native tier this build has.
fn benchmarked_tiers() -> Vec<Tier> {
    let mut tiers = vec![Tier::Interpreter];
    for tier in Tier::ALL {
        if tier == Tier::Interpreter || !tier.is_available() {
            continue;
        }
        // The Cranelift backend only exists with the feature; without it the tier is
        // not in the roster anyway, so this only keeps `NativeBackend::new` total.
        #[cfg(not(feature = "jit-cranelift"))]
        if tier == Tier::Cranelift {
            continue;
        }
        tiers.push(tier);
    }
    tiers
}

/// Time `workload` on every available tier, assert tier agreement, print the table.
fn bench_workload(workload: &Workload) {
    let tiers = benchmarked_tiers();
    println!(
        "jit_bench | workload = {} | tiers = {:?}",
        workload.label,
        tiers.iter().map(Tier::name).collect::<Vec<_>>()
    );

    let mut reference: Option<Measurement> = None;
    for tier in tiers {
        assert!(
            available_tiers().contains(&tier.name()),
            "benchmarked tier {} must be in the build roster",
            tier.name()
        );
        let measurement = measure(tier, workload);
        report(&measurement);
        match &reference {
            None => reference = Some(measurement),
            Some(expected) => assert_eq!(
                measurement.result,
                expected.result,
                "tier {} disagrees with {} on {}",
                tier.name(),
                expected.tier.name(),
                workload.label,
            ),
        }
    }
    println!();
}

#[test]
fn interpreter_and_native_tiers_agree_and_report_timings() {
    // One test (not two) so the two workload timings never run concurrently and
    // contaminate each other; both workloads are still measured and asserted.
    let fib = fib_workload();
    let kernel = kernel_workload();
    for workload in [&fib, &kernel] {
        bench_workload(workload);
    }
}
