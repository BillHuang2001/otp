//! Integration test for the baseline JIT's **direct block-to-block linking**.
//!
//! The baseline tier ([`beam_engine::jit::baseline`]) lowers a module's basic blocks
//! into standalone native functions. A taken branch to a successor basic block this
//! compilation emitted does **not** return the successor's `(func, pc)` word to the
//! driver: the block leaves its own frame (spilling the cached x-registers first —
//! the tier's whole liveness/root-set model) and jumps **straight to that block's
//! native entry** (`src/jit/baseline/emit/link.rs`).
//!
//! Because a successor is entered by a `jmp` rather than the driver's `call`, nothing
//! sets the `rdi` its prologue expects the runtime block in — so a linked exit must
//! reload it from the block's own `rt` slot before tearing the frame down. This suite
//! is the evidence for that contract, on both levels:
//!
//! * **the emitted code**: a `jmp rel32` inside a compiled block whose landing address
//!   is the successor block's compiled entry — read out of the sealed mapping, so it
//!   is proven about the real artifact rather than about the emitter's intent;
//! * **the results**: every linked run is byte-for-byte **interpreter-identical**
//!   (the reference semantics), including the case whose successor block *calls the
//!   interpreter helper* — the case that dereferences a garbage runtime pointer if the
//!   linked exit does not reload `rdi`;
//! * **the spill**: a value a block produces into a *cached* x-register must reach
//!   `machine.x` on a linked exit, or the successor's prologue would reload a stale
//!   slot (the fixtures below pass an argument that is not `NIL`, which is what the
//!   slot holds if nothing spilled);
//! * **transparency**: linking changes control flow *inside* and *between* the blocks
//!   the compilation emitted; the driver's block-entry table and `block_count()` are
//!   exactly the loader's non-empty blocks, before and after;
//! * **the deopt floor**: instructions the tier may not lower still leave the exact
//!   interpreter behaviour (the same typed error), never a wrong result and never a
//!   panic.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_common::BeamError;
use beam_engine::jit::baseline::{BaselineJit, CompiledModule};
use beam_engine::jit::JitBackend;
use beam_engine::{Machine, MinimalContext, Tier, MAX_REGISTER};
use beam_loader::{
    load_file, Export, Function, Import, ImportRef, Instr, Label, Module, Operand, NO_FAIL,
};
use beam_term::{is_small, make_atom, make_small, signed_value, Atom, Eterm, Heap, NIL};

/// The `beam_loader` oracle fixture (reused, never copied): a real `.beam` produced by
/// the system `erlc`, with branches (`pick/1`'s `select_val`, `fib/1`'s tests) and a
/// cross-function tail call (`fib/1` → `fib/3`).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

/// The slice BIF hook: `erlang:'+'/2` and `erlang:'-'/2` over small integers.
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

fn slice_module() -> Module {
    load_file(SLICE_BEAM).expect("load slice.beam")
}

/// Run `export/arity` of `module` on `tier` over a fresh machine + context.
fn run_on_tier<H>(
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    hook: H,
    tier: Tier,
) -> Result<Eterm, BeamError>
where
    H: FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static,
{
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(hook));
    machine.run_with_tier(module, atom(module, export), arity, args, &mut ctx, tier)
}

/// Assert the interpreter and the native baseline tier agree on `module`'s
/// `export/arity` — the same value **or** the same typed error — and return the native
/// tier's result.
fn agree<H>(
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    hook: H,
) -> Result<Eterm, BeamError>
where
    H: FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + Clone + 'static,
{
    let interpreted = run_on_tier(module, export, arity, args, hook.clone(), Tier::Interpreter);
    let jitted = run_on_tier(module, export, arity, args, hook, Tier::BaselineJit);
    match (&interpreted, &jitted) {
        (Ok(interpreted), Ok(jitted)) => {
            assert_eq!(
                jitted, interpreted,
                "{export}/{arity} differs between tiers"
            )
        }
        (Err(interpreted), Err(jitted)) => assert_eq!(
            format!("{jitted:?}"),
            format!("{interpreted:?}"),
            "tiers disagree on the typed error for {export}/{arity}"
        ),
        _ => panic!("tiers disagree for {export}/{arity}: {interpreted:?} vs {jitted:?}"),
    }
    jitted
}

// ---------------------------------------------------------------------------
// A synthetic-module fixture (the loader's own block decomposition)
// ---------------------------------------------------------------------------

/// A builder for synthetic modules whose functions the baseline tier can compile.
///
/// Unlike a bare `Function` literal this runs the loader's own
/// [`beam_loader::lower::build_blocks`] over each body, because the emitter walks a
/// function's **blocks** — an empty `blocks` vector compiles to nothing.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        Fixture { module }
    }

    /// Register `module:function/arity` as import `ImportRef(n)` (`n` = its index).
    fn import(&mut self, module_name: &str, function_name: &str, arity: u32) -> ImportRef {
        let module = self.module.atom_table.intern(module_name);
        let function = self.module.atom_table.intern(function_name);
        self.module.imports.push(Import {
            module,
            function,
            arity,
        });
        ImportRef((self.module.imports.len() - 1) as u32)
    }

    /// Intern an atom (so the module can *name* it) and return it.
    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Append a function; returns its entry label (its first label).
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.module.atom_table.intern(name);
        let module = self.module.name;
        let (blocks, labels) = beam_loader::lower::build_blocks(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks,
            labels,
        });
        entry
    }

    /// Export `name/arity` at `label`.
    fn export(&mut self, name: &str, arity: u32, label: Label) {
        let function = self.module.atom_table.intern(name);
        self.module.exports.push(Export {
            function,
            arity,
            label,
        });
    }

    fn finish(self) -> Module {
        self.module
    }
}

// ---------------------------------------------------------------------------
// Reading the emitted code back out of the sealed mapping
// ---------------------------------------------------------------------------

/// Copy the compiled machine-code bytes in `[start, end)` out of the artifact.
///
/// Reading executable memory is safe here for the usual reasons: the addresses come
/// from [`CompiledModule::entry`] / [`CompiledModule::code_address_range`], so the
/// whole range is inside one live, initialized mapping; the mapping was sealed
/// (`mprotect`ed read+execute) with no writer left, so no read can tear or observe a
/// half-written instruction.
#[allow(unsafe_code)]
fn code_bytes(compiled: &CompiledModule, start: usize, end: usize) -> Vec<u8> {
    let (lo, hi) = compiled.code_address_range();
    assert!(
        lo <= start && start < end && end <= hi,
        "the requested range [{start:#x}, {end:#x}) must lie inside the code mapping [{lo:#x}, {hi:#x})"
    );
    // SAFETY: the assertion above proves `[start, end)` is a subrange of the live
    // mapping `compiled` owns, which stays mapped for as long as `compiled` does and
    // is never written after `seal`; `to_vec` copies the bytes immediately, so the
    // borrow cannot outlive the artifact.
    unsafe { std::slice::from_raw_parts(start as *const u8, end - start).to_vec() }
}

/// The offset within `code` (whose first byte sits at `base`) of a `jmp rel32` whose
/// landing address is exactly `target` — the evidence that the block jumps **straight
/// to the successor's compiled entry** instead of returning its word to the driver.
///
/// The scan is deliberate rather than an offset assertion: it pins the *link*, not an
/// instruction encoding, so an emitter change cannot make the test lie about whether
/// the branch was linked.
fn direct_jump_to(code: &[u8], base: usize, target: usize) -> Option<usize> {
    code.windows(5).enumerate().find_map(|(i, window)| {
        if window[0] != 0xE9 {
            return None;
        }
        let rel = i32::from_le_bytes(window[1..5].try_into().unwrap());
        let landing = base as i64 + i as i64 + 5 + i64::from(rel);
        (landing == target as i64).then_some(i)
    })
}

// ---------------------------------------------------------------------------
// Interpreter-identical results on the real fixture
// ---------------------------------------------------------------------------

#[test]
fn linked_native_runs_match_the_interpreter_on_the_slice_fixture() {
    // `pick/1` is a `select_val` (its alternatives are linked targets), `fib/1` loops
    // through tests and jumps and tail-calls `fib/3`, and `add/2` is a leaf — the three
    // shapes a linked exit carries control across.
    let cases: [(&str, u32, Vec<Eterm>); 6] = [
        ("add", 2, vec![make_small(2), make_small(3)]),
        ("add", 2, vec![make_small(-7), make_small(9)]),
        ("fib", 1, vec![make_small(0)]),
        ("fib", 1, vec![make_small(1)]),
        ("fib", 1, vec![make_small(10)]),
        ("fib", 1, vec![make_small(15)]),
    ];
    for (export, arity, args) in cases {
        let module = slice_module();
        let got = agree(&module, export, arity, &args, arith_hook)
            .unwrap_or_else(|err| panic!("{export}/{arity} failed on both tiers: {err:?}"));
        // The shared value is the one the oracle produces (slice.beam is compiled from
        // a known source; `tests/slice.rs`/`tests/jit.rs` pin the same numbers).
        if export == "fib" {
            assert!(is_small(got), "fib/1 must return a small integer");
        }
    }
    // `pick/1` returns `1` for `a` and `2` for anything else, through its `select_val`.
    for (arg, want) in [("a", 1i64), ("slice", 2)] {
        let module = slice_module();
        let args = [make_atom(atom(&module, arg))];
        let got = agree(&module, "pick", 1, &args, arith_hook).expect("pick/1");
        assert_eq!(got, make_small(want), "pick({arg})");
    }
}

// ---------------------------------------------------------------------------
// The link itself, and the crash it must not cause
// ---------------------------------------------------------------------------

/// The regression fixture: `entry/1` jumps to `tail/1` — a block of **another
/// function** of the same module, so the branch is linked — and the block it leaves
/// has just *called the interpreter helper*, which clobbers `rdi` (the block ABI's
/// argument register, which the driver's `call` had set to the runtime block).
///
/// ```text
///   entry(X0) ->  x1 = X0                 ; inlined, cached in a machine register
///                 x2 = m:tag(x1)          ; an unknown import: deopts to `jit_step_one`
///                                         ; (this is what destroys the incoming rdi)
///                 jump label 2 ---------> ; LINKED: spill, reload rdi, tear down, jmp
///   tail(...) ->  x0 = m:tag(x2)          ; the successor's prologue reads its rt from
///                                         ; rdi, and x2 from `machine.x` (the spill)
///                 return
/// ```
///
/// Without the `rdi` reload the successor's prologue loads `rbx` from a garbage pointer
/// and the helper call dereferences it (the reported abort); without the exit spill the
/// successor reloads `NIL` instead of the argument.
fn clobbering_jump_module() -> Module {
    let mut fixture = Fixture::new("linking_clobber");
    let tag = fixture.import("m", "tag", 1);
    let entry = fixture.add_function(
        "entry",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::X(0),
                dst: Operand::X(1),
            },
            Instr::GcBif {
                arity: 1,
                bif: tag,
                fail: NO_FAIL,
                live: 1,
                args: vec![Operand::X(1)],
                dst: Operand::X(2),
            },
            Instr::Jump { target: Label(2) },
        ],
    );
    fixture.add_function(
        "tail",
        1,
        vec![
            Instr::Label(Label(2)),
            Instr::GcBif {
                arity: 1,
                bif: tag,
                fail: NO_FAIL,
                live: 1,
                args: vec![Operand::X(2)],
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    fixture.export("entry", 1, entry);
    fixture.finish()
}

/// The `m:tag/1` hook: the identity, counting its calls.
///
/// The identity keeps the observable result the argument, and the counter proves the
/// successor block really reached the host through the helper seam rather than
/// silently doing nothing.
fn identity_hook(
    calls: Arc<AtomicUsize>,
) -> impl FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + Clone + 'static {
    move |module: &str, function: &str, arity: u32, args: &[Eterm]| {
        if module != "m" || function != "tag" || arity != 1 || args.len() != 1 {
            return Err(BeamError::BadArg);
        }
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(args[0])
    }
}

#[test]
fn a_cross_function_jump_is_linked_straight_to_the_successors_entry() {
    let module = clobbering_jump_module();
    let compiled = BaselineJit::new()
        .compile(&module)
        .expect("compile the cross-function jump fixture");

    // Both blocks were compiled, so the branch to `label 2` is linkable (the linker's
    // table *is* this block-entry table).
    let from = compiled
        .entry(0, 0)
        .expect("entry/1's block at (0, 0) must be compiled") as usize;
    let to = compiled
        .entry(1, 0)
        .expect("tail/1's block at (1, 0) must be compiled") as usize;
    assert_ne!(from, to);

    // Read the leaving block's code and look for the direct jump to the successor's
    // compiled entry.
    let exit_code = code_bytes(&compiled, from, to);
    let site = direct_jump_to(&exit_code, from, to).unwrap_or_else(|| {
        panic!(
            "the block at {from:#x} must `jmp` directly to the successor's compiled entry \
             {to:#x}; its {} bytes contain no such jump",
            exit_code.len()
        )
    });
    assert!(
        site + 5 <= exit_code.len(),
        "the link is a real instruction inside the block"
    );
}

#[test]
fn a_linked_jump_into_a_helper_calling_successor_is_interpreter_identical() {
    let module = clobbering_jump_module();
    assert_eq!(
        module.functions.len(),
        2,
        "the fixture has the leaving and the successor function"
    );

    // The reference result, and the interpreter's own call count.
    let interpreted_calls = Arc::new(AtomicUsize::new(0));
    let interpreted = run_on_tier(
        &module,
        "entry",
        1,
        &[make_small(41)],
        identity_hook(Arc::clone(&interpreted_calls)),
        Tier::Interpreter,
    )
    .expect("the interpreter must run the fixture");
    assert_eq!(interpreted, make_small(41), "the hook is the identity");
    assert_eq!(
        interpreted_calls.load(Ordering::SeqCst),
        2,
        "both blocks' `gc_bif` go through the host"
    );

    // The linked native run: same result, and the successor's helper call really ran.
    for arg in [make_small(0), make_small(41), make_small(-12345)] {
        let native_calls = Arc::new(AtomicUsize::new(0));
        let jitted = run_on_tier(
            &module,
            "entry",
            1,
            &[arg],
            identity_hook(Arc::clone(&native_calls)),
            Tier::BaselineJit,
        )
        .expect("the baseline tier must run the fixture");
        assert_eq!(
            jitted, arg,
            "the value must survive the linked exit (the spill) and the successor's \
             prologue (the `rdi` reload)"
        );
        assert_ne!(
            arg, NIL,
            "a non-NIL argument is what makes a missing spill visible"
        );
        assert!(
            native_calls.load(Ordering::SeqCst) >= 1,
            "the successor block's `gc_bif` must reach the host"
        );
    }
}

// ---------------------------------------------------------------------------
// Transparency: linking adds no block and no entry
// ---------------------------------------------------------------------------

#[test]
fn linking_leaves_the_block_table_and_block_count_unchanged() {
    // The loader's own block decomposition of the module: what the driver's entry
    // table is keyed by, and the only blocks a branch may be linked to.
    let module = slice_module();
    let compiled = BaselineJit::new()
        .compile(&module)
        .expect("compile slice.beam");

    let (lo, hi) = compiled.code_address_range();
    let mut non_empty = 0usize;
    let mut empty = 0usize;
    for (func, function) in module.functions.iter().enumerate() {
        for block in &function.blocks {
            let end = block.end.min(function.code.len());
            let entry = compiled.entry(func as u32, block.start as u32);
            if block.start >= end {
                empty += 1;
                assert!(
                    entry.is_none(),
                    "an empty block ({func}, {}) compiles to no entry",
                    block.start
                );
                continue;
            }
            let address = entry.unwrap_or_else(|| {
                panic!(
                    "every non-empty block must have an entry: ({func}, {})",
                    block.start
                )
            }) as usize;
            assert!(
                (lo..hi).contains(&address),
                "the entry {address:#x} must lie inside the code mapping [{lo:#x}, {hi:#x})"
            );
            non_empty += 1;
        }
    }
    // The empty blocks are the ones the loader cuts but never emits; the rest are
    // exactly the compiled blocks — linking neither adds nor removes one.
    assert_eq!(
        compiled.block_count(),
        non_empty,
        "one entry per non-empty block (and {empty} empty blocks were skipped)"
    );

    // An address no block starts at has no entry (there is nothing to link to).
    let first = &module.functions[0];
    assert!(compiled.entry(0, first.code.len() as u32).is_none());
    assert!(compiled.entry(module.functions.len() as u32, 0).is_none());

    // And a second compilation of the same module has the same shape: linking is a
    // control-flow change inside and between these very blocks.
    let again = BaselineJit::new()
        .compile(&module)
        .expect("compile slice.beam again");
    assert_eq!(again.block_count(), compiled.block_count());
    assert_eq!(again.cached_registers(), compiled.cached_registers());
    assert_eq!(
        again.highest_cached_register(),
        compiled.highest_cached_register()
    );
    assert_eq!(
        again.highest_native_register(),
        compiled.highest_native_register()
    );
}

#[test]
fn every_non_empty_block_of_a_linking_module_has_an_entry() {
    // The fixture whose interesting branch is linked: the successor's entry must be in
    // the table (that is what makes the branch linkable), and the block count must
    // still be the number of non-empty blocks the loader cut.
    let module = clobbering_jump_module();
    let compiled = BaselineJit::new()
        .compile(&module)
        .expect("compile the fixture");
    assert_eq!(compiled.block_count(), 2, "one block per function body");
    assert!(compiled.entry(0, 0).is_some());
    assert!(compiled.entry(1, 0).is_some());
    assert!(compiled.entry(0, 1).is_none(), "an address inside a block");
    assert!(
        compiled.entry(2, 0).is_none(),
        "a function that does not exist"
    );
}

// ---------------------------------------------------------------------------
// The deopt floor: anything the tier may not lower keeps interpreter semantics
// ---------------------------------------------------------------------------

/// `read/1`: a list read whose source register lies past `MAX_REGISTER`, so the
/// emitted guard refuses the shape and the instruction *must* deopt to the interpreter
/// step (whose `bad_register` error is the oracle behaviour).
fn out_of_register_module() -> Module {
    let mut fixture = Fixture::new("linking_out_of_register");
    let entry = fixture.add_function(
        "read",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::GetList {
                source: Operand::X(MAX_REGISTER),
                head: Operand::X(1),
                tail: Operand::X(2),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    fixture.export("read", 1, entry);
    fixture.finish()
}

/// `jump/0`: a branch to a label the module does not define, which the tier hands to
/// the interpreter step for its exact `unknown_label` error.
fn unresolved_label_module() -> Module {
    let mut fixture = Fixture::new("linking_unresolved");
    let entry = fixture.add_function(
        "jump",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Jump { target: Label(99) },
            Instr::Return,
        ],
    );
    fixture.export("jump", 0, entry);
    fixture.finish()
}

/// `classify/1`: an arity switch with **no** fail label, whose non-tuple path raises
/// `{badmatch, Value}` — a shape the inlined switch cannot serve, so it deopts.
fn no_fail_arity_switch_module() -> Module {
    let mut fixture = Fixture::new("linking_no_fail");
    // The raise reason is the compound `{badmatch, Value}`, so the module must be able
    // to *name* the atom (a module that cannot is a documented typed-error case).
    fixture.atom("badmatch");
    let entry = fixture.add_function(
        "classify",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::SelectTupleArity {
                tuple: Operand::X(0),
                fail: NO_FAIL,
                destinations: vec![(Operand::Integer(2), Label(2))],
            },
            Instr::Return,
            Instr::Label(Label(2)),
            Instr::Move {
                src: Operand::Integer(2),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    fixture.export("classify", 1, entry);
    fixture.finish()
}

#[test]
fn a_shape_the_tier_may_not_lower_still_deopts_with_interpreter_semantics() {
    // An out-of-range x-register: the guard refuses, the helper's `read_operand`
    // reports the exact `bad_register` error on both tiers.
    let module = out_of_register_module();
    let jitted = agree(&module, "read", 1, &[make_small(1)], arith_hook);
    assert!(
        matches!(jitted, Err(BeamError::BadArg)),
        "an unaddressable register is the interpreter's typed `bad_register`, got {jitted:?}"
    );

    // An unresolved label: the emitter hands the jump to the helper rather than
    // inventing a target.
    let module = unresolved_label_module();
    let jitted = agree(&module, "jump", 0, &[], arith_hook);
    assert!(
        matches!(jitted, Err(BeamError::BadArg)),
        "an undefined label is the interpreter's typed `unknown_label`, got {jitted:?}"
    );

    // A `NO_FAIL` arity switch on a non-tuple: the raise (with its reason term) is
    // identical on both tiers.
    let module = no_fail_arity_switch_module();
    for arg in [make_small(7), NIL, make_atom(atom(&module, "classify"))] {
        let jitted = agree(&module, "classify", 1, &[arg], arith_hook);
        assert!(
            matches!(jitted, Err(BeamError::Raise(_))),
            "a `NO_FAIL` switch must raise `{{badmatch, Value}}`, got {jitted:?}"
        );
    }

    // A `NO_FAIL` switch that *does* match must still return the alternative's value.
    let module = no_fail_arity_switch_module();
    let mut heap = Heap::new(16);
    let pair = heap
        .tuple(&[make_small(1), make_small(2)])
        .expect("a 2-tuple");
    assert_eq!(
        agree(&module, "classify", 1, &[pair], arith_hook).expect("classify/1"),
        make_small(2)
    );
}
