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
//! Pins for the **Y-window witness** ([`JitRuntime::y_window_current`], Invariant IW) and
//! the dynamic probe that measures how often it lets the two emitted publish points skip a
//! derivation. A `#[path]` child of `super::tests`, so it shares that module's harness
//! (`with_both_tiers`, `assert_same_outcome`, `body_module`).
//!
//! Invariant IW: `y_window_current == true` **⇒** `(y_base, y_end)` are exactly the pair
//! `refresh_y_base()` would derive now. A **missed** invalidation is a miscompile, a
//! spurious one costs one derivation — so the invalidation set must be exhaustive and every
//! member is pinned here, one test per site.
//!
//! The direct-helper pins build a `Machine` with a live frame plus a hand-built
//! `JitRuntime`, arm the witness by hand (`y_window_current = true`) and assert what the
//! helper leaves behind: **cleared** for the sweeping sites, and **re-derived to the live
//! window** for the four frame helpers that republish.
use super::*;
use crate::jit::Tier;
use crate::machine::Machine;
use beam_loader::HeapNeed;

/// The live `[y_base, y_end)` window of the machine's current top frame, as raw
/// addresses — what `refresh_y_base` must derive.
fn live_window(machine: &mut Machine) -> (*mut Eterm, *mut Eterm) {
    let base = machine.stack.as_mut_ptr();
    match machine.frames.last() {
        Some(frame) => (unsafe { base.add(frame.y_base) }, unsafe {
            base.add(frame.y_base + frame.y_count as usize)
        }),
        None => (std::ptr::null_mut(), std::ptr::null_mut()),
    }
}

/// Run `f` against a `JitRuntime` whose machine has exactly one live frame with `y_count`
/// Y slots, with the witness armed (`true`) and a **deliberately wrong** published window,
/// so a republishing site is distinguishable from a skipping one.
fn with_armed_runtime<R>(
    module: &Module,
    y_count: u32,
    f: impl FnOnce(&mut JitRuntime<'_>) -> R,
) -> R {
    let mut ctx = MinimalContext::new();
    with_armed_runtime_ctx(module, y_count, &mut ctx, f)
}

/// [`with_armed_runtime`] over a caller-supplied context — the seam a helper whose **error**
/// arm is only reachable through the host needs (a declining heap-admission door, say).
fn with_armed_runtime_ctx<'c, R>(
    module: &'c Module,
    y_count: u32,
    ctx: &'c mut dyn ExecutionContext,
    f: impl FnOnce(&mut JitRuntime<'c>) -> R,
) -> R {
    let labels = LabelMap::new();
    let mut machine = Machine::new();
    machine.push_frame(y_count).expect("push_frame");
    let x_regs = machine.x.as_mut_ptr();
    let mut rt = JitRuntime {
        machine: &mut machine,
        x_regs,
        module,
        entry_module: module,
        labels: &labels,
        ctx,
        error: None,
        enter: None,
        // Provably not the live pair: the arming below asserts the flag, not the window.
        y_base: std::ptr::null_mut(),
        y_end: std::ptr::null_mut(),
        literals_base: std::ptr::null(),
        literals_end: std::ptr::null(),
        resume_entry: std::ptr::null(),
        tables: super::runtime::EntryTables::NONE,
        compiled: std::ptr::null(),
        y_window_current: true,
    };
    f(&mut rt)
}

/// An [`ExecutionContext`] whose **heap-admission** door declines, so the
/// `jit_allocate_heap` error arm is reachable: that door (`Machine::ensure_heap` →
/// `ensure_heap_full` → `ensure_heap`) is the only engine→host call the heap/frame family
/// makes, and every other member of the family fails without a host. Everything else
/// delegates to a [`MinimalContext`].
struct RefusingHeapCtx {
    inner: MinimalContext,
}

impl RefusingHeapCtx {
    fn new() -> RefusingHeapCtx {
        RefusingHeapCtx {
            inner: MinimalContext::new(),
        }
    }
}

impl ExecutionContext for RefusingHeapCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.inner.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        Err(crate::error::unsupported(
            "y-witness pin: the frame-admission door declines",
        ))
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.inner.alloc_words(n)
    }
}

/// Fresh `JitRuntime` ⇒ the first `jit_publish_y` **derives**; the second skips. This is
/// the constructor pin: every driver build and every test literal starts
/// `y_window_current: false`, so "not published" is the initial state and the first
/// publish always derives.
#[test]
fn a_fresh_runtime_derives_on_its_first_publish_and_skips_on_the_second() {
    let module = Module::default();
    let labels = LabelMap::new();
    let mut machine = Machine::new();
    machine.push_frame(2).expect("push_frame");
    let expected = live_window(&mut machine);
    let mut ctx = MinimalContext::new();
    let x_regs = machine.x.as_mut_ptr();
    let mut rt = JitRuntime {
        machine: &mut machine,
        x_regs,
        module: &module,
        entry_module: &module,
        labels: &labels,
        ctx: &mut ctx,
        error: None,
        enter: None,
        y_base: std::ptr::null_mut(),
        y_end: std::ptr::null_mut(),
        literals_base: std::ptr::null(),
        literals_end: std::ptr::null(),
        resume_entry: std::ptr::null(),
        tables: super::runtime::EntryTables::NONE,
        compiled: std::ptr::null(),
        y_window_current: false,
    };
    assert!(
        !rt.y_window_current,
        "a fresh JitRuntime must start with the witness cleared"
    );
    counters::reset();
    counters::set_enabled(true);
    super::runtime::jit_publish_y(&mut rt, 0, 2);
    assert!(
        rt.y_window_current,
        "the first publish must arm the witness"
    );
    assert_eq!(
        rt.y_base, expected.0,
        "the derivation must name the live base"
    );
    assert_eq!(
        rt.y_end, expected.1,
        "the derivation must name the live end"
    );
    // The position store is unconditional — the skip must never drop it.
    assert_eq!(
        rt_machine_code(&rt),
        Some((0, 2)),
        "jit_publish_y must store the position even when it skips the derivation"
    );
    super::runtime::jit_publish_y(&mut rt, 0, 3);
    counters::set_enabled(false);
    let snapshot = counters::snapshot();
    counters::reset();
    assert_eq!(snapshot.publish_y, 2, "both prologues are counted");
    assert_eq!(
        snapshot.publish_y_skips, 1,
        "the second prologue must take the skip path"
    );
    assert_eq!(
        rt_machine_code(&rt),
        Some((0, 3)),
        "the skipped publish still advances the position"
    );
}

/// The position store `jit_publish_y` must keep on the skip path, read back through the
/// runtime's own machine pointer (the `&mut Machine` the test handed it is borrowed for
/// the runtime's lifetime).
fn rt_machine_code(rt: &JitRuntime<'_>) -> Option<(u32, u32)> {
    // SAFETY: `rt.machine` is the live `Machine` the test lent the runtime and no other
    // reference to it is alive while this shared borrow of `rt` exists (the runtime holds
    // a raw pointer, not a `&mut`).
    let machine = unsafe { &*rt.machine };
    machine.current_code()
}

/// `jit_refresh_y` skips while the witness holds and re-derives once it is cleared.
#[test]
fn the_in_block_refresh_skips_on_a_current_witness_and_derives_once_cleared() {
    let module = Module::default();
    with_armed_runtime(&module, 2, |rt| {
        counters::reset();
        counters::set_enabled(true);
        super::runtime::jit_refresh_y(rt);
        counters::set_enabled(false);
        let snapshot = counters::snapshot();
        counters::reset();
        assert_eq!(snapshot.refresh_y, 1, "the refresh itself is counted");
        assert_eq!(
            snapshot.refresh_y_skips, 1,
            "a current witness must skip the derivation"
        );
        assert!(
            rt.y_base.is_null(),
            "the skip must not derive: the deliberately-wrong window must survive"
        );
        // Clearing the witness forces the derivation on the next refresh.
        rt.invalidate_y_window();
        super::runtime::jit_refresh_y(rt);
        assert!(rt.y_window_current, "the derivation re-arms the witness");
        assert!(!rt.y_base.is_null(), "the derivation must publish a window");
    });
}

/// One pin per **sweeping** invalidation site: each must leave the witness cleared.
///
/// The sites are driven with the cheapest arguments that reach the invalidation (a real
/// one-instruction step for `jit_step_one`, an unresolvable import for `jit_ext_call`, a
/// frame-size mismatch for `jit_deallocate`'s error arm, an over-trim for `jit_trim`'s).
#[test]
fn every_sweeping_site_clears_the_witness() {
    // `jit_step_one` — it runs an *arbitrary* interpreter instruction.
    {
        let (module, _atom) = body_module(vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::Return,
        ]);
        let labels = LabelMap::new();
        let mut machine = Machine::new();
        machine.push_frame(0).expect("push_frame");
        let mut ctx = MinimalContext::new();
        let x_regs = machine.x.as_mut_ptr();
        let mut rt = JitRuntime {
            machine: &mut machine,
            x_regs,
            module: &module,
            entry_module: &module,
            labels: &labels,
            ctx: &mut ctx,
            error: None,
            enter: None,
            y_base: std::ptr::null_mut(),
            y_end: std::ptr::null_mut(),
            literals_base: std::ptr::null(),
            literals_end: std::ptr::null(),
            resume_entry: std::ptr::null(),
            tables: super::runtime::EntryTables::NONE,
            compiled: std::ptr::null(),
            y_window_current: true,
        };
        super::runtime::jit_step_one(&mut rt, 0, 2);
        assert!(
            !rt.y_window_current,
            "jit_step_one must clear the witness before the step"
        );
    }
    // `jit_ext_call` — `EXT_CALL_LAST` pops a frame, and both fallbacks step.
    {
        let module = Module::default();
        with_armed_runtime(&module, 1, |rt| {
            super::runtime::jit_ext_call(rt, 0, 0, 0, 0, 0);
            assert!(
                !rt.y_window_current,
                "jit_ext_call must clear the witness on every outcome"
            );
        });
    }
    // `jit_tail_call` — `pop_frame_checked` (a `call_last`).
    {
        let module = Module::default();
        with_armed_runtime(&module, 1, |rt| {
            super::runtime::jit_tail_call(rt, 0, 1);
            assert!(
                !rt.y_window_current,
                "jit_tail_call must clear the witness (it pops a frame)"
            );
        });
    }
    // `jit_deallocate_return` — the pop happens before the continuation read.
    {
        let module = Module::default();
        with_armed_runtime(&module, 1, |rt| {
            super::runtime::jit_deallocate_return(rt, 0, 0, 1);
            assert!(
                !rt.y_window_current,
                "jit_deallocate_return must clear the witness before the pop"
            );
        });
    }
    // `jit_test_heap` — the one engine→host door that sees the whole `RootSet`.
    {
        let module = Module::default();
        with_armed_runtime(&module, 1, |rt| {
            super::runtime::jit_test_heap(rt, 0, 0, 1, 0);
            assert!(
                !rt.y_window_current,
                "jit_test_heap must clear the witness (the host door sees the stack)"
            );
        });
    }
    // `jit_deallocate`'s **error** arm — a frame-size mismatch.
    {
        let module = Module::default();
        with_armed_runtime(&module, 1, |rt| {
            super::runtime::jit_deallocate(rt, 0, 0, 2);
            assert!(
                !rt.y_window_current,
                "a failed pop must not leave the witness describing a pre-change window"
            );
        });
    }
    // `jit_trim`'s **error** arm — an over-trim.
    {
        let module = Module::default();
        with_armed_runtime(&module, 1, |rt| {
            super::runtime::jit_trim(rt, 0, 0, 5);
            assert!(
                !rt.y_window_current,
                "a failed trim must not leave the witness current"
            );
        });
    }
    // `jit_allocate_heap`'s **error** arm — the frame-admission door declines, so the arm
    // that must not leave the witness describing a pre-change window is the one taken.
    // (Its success arm is pinned by `the_republishing_frame_helpers_publish_the_live_window`.)
    {
        let module = Module::default();
        let mut ctx = RefusingHeapCtx::new();
        with_armed_runtime_ctx(&module, 1, &mut ctx, |rt| {
            let word = super::runtime::jit_allocate_heap(rt, 0, 0, 2, 8, 0);
            assert_eq!(
                word,
                super::runtime::ERROR,
                "a declined admission must leave the block with the error word"
            );
            assert!(
                rt.error.is_some(),
                "the declined admission must be reported through `rt.error`"
            );
            assert!(
                !rt.y_window_current,
                "a declined admission must not leave the witness describing a pre-change window"
            );
        });
    }
}

/// The four **republishing** frame helpers must leave the witness armed **and** the window
/// equal to the live one — they are the parties that invalidate it, and they re-derive it
/// inline so the emitted continue arm pays no separate `jit_refresh_y`.
#[test]
fn the_republishing_frame_helpers_publish_the_live_window() {
    let module = Module::default();
    // `jit_allocate`: push a frame (the window must move to the *new* frame).
    with_armed_runtime(&module, 1, |rt| {
        assert_eq!(
            super::runtime::jit_allocate(rt, 0, 4, 3),
            super::runtime::encode_target(0, 5),
            "a successful allocate continues at pc + 1"
        );
        assert!(rt.y_window_current, "allocate must republish the window");
        // SAFETY: `rt.machine` is the live machine the test lent the runtime.
        let machine = unsafe { &mut *rt.machine };
        let live = live_window(machine);
        assert_eq!(
            rt.y_base, live.0,
            "the republished base must be the live one"
        );
        assert_eq!(rt.y_end, live.1, "the republished end must be the live one");
    });
    // `jit_allocate_heap`: admission then push.
    with_armed_runtime(&module, 1, |rt| {
        assert_eq!(
            super::runtime::jit_allocate_heap(rt, 0, 4, 2, 8, 0),
            super::runtime::encode_target(0, 5),
        );
        assert!(
            rt.y_window_current,
            "allocate_heap must republish the window"
        );
        // SAFETY: see above.
        let machine = unsafe { &mut *rt.machine };
        let live = live_window(machine);
        assert_eq!((rt.y_base, rt.y_end), live);
    });
    // `jit_trim`: the base moves up by `n` and the survivors shift down.
    with_armed_runtime(&module, 3, |rt| {
        assert_eq!(
            super::runtime::jit_trim(rt, 0, 4, 2),
            super::runtime::encode_target(0, 5),
        );
        assert!(rt.y_window_current, "trim must republish the window");
        // SAFETY: see above.
        let machine = unsafe { &mut *rt.machine };
        let live = live_window(machine);
        assert_eq!((rt.y_base, rt.y_end), live);
    });
    // `jit_deallocate`: the pop truncates the stack, so the caller's frame is the top one.
    with_armed_runtime(&module, 1, |rt| {
        assert_eq!(
            super::runtime::jit_deallocate(rt, 0, 4, 1),
            super::runtime::encode_target(0, 5),
        );
        assert!(rt.y_window_current, "deallocate must republish the window");
        // SAFETY: see above.
        let machine = unsafe { &mut *rt.machine };
        let live = live_window(machine);
        assert_eq!((rt.y_base, rt.y_end), live);
    });
}

/// `jit_return` **must not** clear the witness: a `return` pops nothing, so the published
/// window still names the live frame. (Listed in Invariant IW's "not cleared" set; a future
/// change that started clearing here would only cost a derivation, but the *contract* is
/// that it does not, so it is pinned.)
#[test]
fn a_return_leaves_the_witness_alone() {
    let module = Module::default();
    with_armed_runtime(&module, 1, |rt| {
        let published = rt.y_base;
        super::runtime::jit_return(rt, 0, 0);
        assert!(
            rt.y_window_current,
            "Instr::Return pops nothing, so the witness must survive it"
        );
        assert_eq!(rt.y_base, published, "the window must be untouched");
    });
}

/// A **Y read after an in-block helper whose refresh took the skip path** must see the live
/// value: end-to-end, a block that builds a boxed term (a family-owned helper, frame-neutral,
/// so `jit_refresh_y` skips) and then reads a `Y` slot it wrote before the helper.
///
/// The `Y` value is a *runtime* value (`x(0)`'s argument is not needed — it is a constant
/// here), so the assertion is really "the inlined `Y` read resolved through the surviving
/// window instead of deopting", which the structural counters confirm: a deopt would put
/// a `Move` in `step_one`.
///
/// # Why this shape pins `jit_refresh_y`'s skip and **not** `jit_publish_y`'s
///
/// The shape's `allocate` is the block's **first** instruction — exactly the shape every
/// real function body has. `Allocation::prologue_publishes_y` is therefore legitimately
/// **false** for it: every `Y` access of the block lies after an in-block frame helper,
/// and that helper (`jit_allocate`) re-publishes the window itself on its in-block
/// continue arm, so the prologue's publish would be overwritten before anything could
/// read it. The prologue emits `jit_set_position` instead (a dead publish elided — the
/// round-4 gate, [round-4 gate]: see `Allocation::prologue_publishes_y` and
/// `src/jit/baseline/CONTEXT.md`).
///
/// So `publish_y == 0` here **by correct design**, and the witness path this test pins is
/// the *other* emitted publish point: the gated in-block `jit_refresh_y` after the
/// frame-neutral family helper. A `publish_y > 0` assertion would be pinning a
/// prologue-publish this shape must not have; a future reader must not "fix" the shape by
/// adding one.
///
/// [round-4 gate]: super::emit::Allocation::prologue_publishes_y
#[test]
fn a_y_read_after_a_skipped_refresh_sees_the_live_value() {
    let (module, atom) = y_shape_module();
    let (outcome, snapshot) = run_counting(&module, atom, Tier::BaselineJit);
    counters::reset();
    match &outcome {
        Ok(RunOutcome::Returned(value)) => assert_eq!(
            value.raw(),
            make_small(11).raw(),
            "the Y slot written before the family helper must be read back after it"
        ),
        other => panic!("the Y shape must return 11, got {other:?}"),
    }
    assert_eq!(
        snapshot.publish_y, 0,
        "this shape opens with `allocate`, so `Allocation::prologue_publishes_y` must be \
         false and the prologue must emit `jit_set_position` instead of `jit_publish_y` \
         (`jit_allocate` re-publishes the window itself); saw publish_y={} set_position={}",
        snapshot.publish_y, snapshot.set_position,
    );
    assert!(
        snapshot.set_position > 0,
        "the prologue still stores the position (the frozen publication set) even when the \
         Y-window publish is elided"
    );
    assert!(
        snapshot.refresh_y_skips > 0,
        "the family helper is frame-neutral, so the gated refresh must take the skip path; \
         refresh_y={} skips={}",
        snapshot.refresh_y,
        snapshot.refresh_y_skips,
    );
    assert_eq!(
        snapshot.step_one, 0,
        "every instruction of the Y shape is lowered, so no interpreter step may remain"
    );
    // And the tier agrees with the interpreter on the whole shape.
    with_both_tiers(&module, atom, |interpreter, baseline| {
        assert_same_outcome(
            "y_read_after_skipped_refresh",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

/// The shape `a_y_read_after_a_skipped_refresh_sees_the_live_value` drives: `allocate`,
/// write a `Y` slot, build a tuple (family-owned ⇒ a helper call whose gated refresh
/// skips), read the `Y` slot back into `x(0)`, deallocate, return.
fn y_shape_module() -> (Module, Atom) {
    body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(2),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(11),
            dst: Operand::Y(0),
        },
        Instr::PutTuple2 {
            dst: Operand::X(1),
            elements: vec![Operand::Integer(1), Operand::Integer(2)],
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
    ])
}

// ---------------------------------------------------------------------------
// The driver loop head: the witness never survives an iteration boundary
// ---------------------------------------------------------------------------

/// The engine resolves a raise's **reason** and **class** atoms through the running
/// module's own atom table (`machine::catch::namespace_atom`, whose fallback is the
/// module snapshot), so a hand-built fixture that raises must name them itself —
/// `build_module` interns only its module/function/import atom names.
fn intern_exception_atoms(module: &mut Module) {
    for name in ["case_clause", "error", "exit", "throw", "EXIT"] {
        module.atom_table.intern(name);
    }
}

/// **The driver-loop-head clear, end to end** — the one reachable frame change inside
/// native code that is *not* instrumented: a **family raise**.
///
/// `jit_family` runs the interpreter's own `families::dispatch` body; its raise path
/// (`Machine::unwind_to_frame`, reached from `Machine::throw_with_trace`) truncates
/// `machine.frames` **and** `machine.stack` and reports the landing as a non-`Next`
/// `Step`, so the block leaves with the witness still claiming a window over a frame that
/// no longer exists. The driver therefore clears the witness at the head of **every** loop
/// iteration (`super::drive`), which is exactly the boundary a block return is.
///
/// The shape is built so a missed clear is a **wrong value**, not just a redundant
/// derivation:
///
/// 1. block 1: `allocate 2` (frame A, 2 Y slots), `y(0) := 11`, `try` (the catch frame
///    records frame A), then `allocate 1` — a **second** frame B, and `jit_allocate`
///    re-arms the witness naming *B's* window;
/// 2. `case_end` raises `{case_clause,x(0)}`; the unwind destroys frame B and truncates
///    `machine.stack` back to frame A — the witness now names B;
/// 3. block 2 (the handler): `try_case`, then `y(0) → x(0)`, which must read the **11**
///    frame A wrote. Through the stale B window that read lands on B's old slot instead.
///
/// The surviving-window assertion (`publish_y == 1`, `publish_y_skips == 0`) is the
/// discriminating one: the handler block's prologue publish is the *only* `jit_publish_y`
/// of the run (block 1's is elided — every `Y` access there follows an in-block frame
/// helper), and a witness that survived the boundary would make it skip.
///
/// # Why the fixture carries an unreachable filler before the handler label
///
/// The clear is only observable when the raise actually **crosses an iteration boundary**,
/// i.e. when the block hands its `Step::Goto` word back to the driver. The block ABI
/// encodes `Step::Next` and `Step::Goto { pc: pc + 1 }` as the *same* word
/// (`runtime::fold_step` → `encode_target(func, pc + 1)`), so a raising instruction whose
/// catch handler is its own `pc + 1` is folded by `emit_continue_check` into an in-block
/// continue, after which the block runs off its end and `Linker::emit_exit` **links
/// straight into the handler block**, bypassing the driver iteration the clear happens in.
/// That shape is a real divergence today and is pinned (as a known gap) by
/// [`a_raise_whose_handler_is_its_own_next_instruction_reads_a_stale_y_window`]; this
/// fixture keeps an unreachable `move` between the `case_end` and the handler label so the
/// handler is *not* the raise's `pc + 1` and the raise takes the driver route this test is
/// about.
#[test]
fn the_driver_clears_the_witness_across_a_raise_iteration_boundary() {
    let (module, atom) = raise_across_frames_module();
    let (outcome, snapshot) = run_counting(&module, atom, Tier::BaselineJit);
    match &outcome {
        Ok(RunOutcome::Returned(value)) => assert_eq!(
            value.raw(),
            make_small(11).raw(),
            "the handler block must read y(0) through the LIVE window (frame A's), not \
             through the window the raise invalidated (frame B's)"
        ),
        other => panic!("the raise-across-frames shape must return 11, got {other:?}"),
    }
    assert_eq!(
        snapshot.publish_y, 1,
        "the run publishes exactly once — the handler block's prologue"
    );
    assert_eq!(
        snapshot.publish_y_skips, 0,
        "the post-raise publish must DERIVE: the witness was cleared at the iteration \
         boundary the raise crossed; a surviving witness would name the destroyed frame B \
         and skip the derivation (publish_y={} skips={})",
        snapshot.publish_y, snapshot.publish_y_skips,
    );
    assert_eq!(
        snapshot.step_one, 0,
        "every instruction of the shape is lowered; a deopt here would mask the witness pin"
    );
    // And the interpreter takes the same route to the same value.
    with_both_tiers(&module, atom, |interpreter, baseline| {
        assert_same_outcome(
            "raise_across_frames",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

/// The shape [`the_driver_clears_the_witness_across_a_raise_iteration_boundary`] drives: a
/// `try` in frame A, a second `allocate` (re-arming the witness for frame B), a family
/// raise that unwinds back to A, and a handler block that reads A's `y(0)`.
fn raise_across_frames_module() -> (Module, Atom) {
    let (mut module, atom) = raise_across_frames_module_code(&[
        // Unreachable filler: it keeps the handler label off `case_end`'s own `pc + 1`,
        // so the raise's `Step::Goto` word cannot collide with
        // `encode_target(func, pc + 1)` (see the known-gap pin below).
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(5),
        },
    ]);
    intern_exception_atoms(&mut module);
    debug_assert!(atom == export_atom(&module, "f"));
    (module, atom)
}

/// **KNOWN GAP (a real base-tier miscompile, narrow but reachable).** The block ABI
/// encodes `Step::Next` and `Step::Goto { pc: pc + 1 }` as the same word, so a family
/// raise whose catch handler is the raising instruction's own `pc + 1` is folded by
/// `emit_continue_check` into an in-block continue; the block then runs off its end and
/// `Linker::emit_exit` links **directly** into the handler block, so the driver iteration
/// that clears the Y-window witness (`super::drive`'s loop head, Invariant IW) never runs.
/// The witness therefore still names the frame the raise destroyed
/// (`Machine::unwind_to_frame` truncates `machine.frames` **and** `machine.stack`), and the
/// handler's inlined `Y` read resolves through that dead window.
///
/// The shape here is exactly [`raise_across_frames_module`]'s **without** the unreachable
/// filler, so `case_end` sits at index 6 and its handler label at index 7 = its `pc + 1`:
///
/// * interpreter: 11 (it reads `machine.stack` directly, through frame A's live window);
/// * `Tier::BaselineJit`: the raw stack word `0x3B` (59) — not even a small integer, since
///   it is read past the truncated `machine.stack`, i.e. the handler block's prologue
///   publish **skipped** the derivation (`publish_y == 1`, `publish_y_skips == 1`), and its
///   `jit_refresh_y` skipped too.
///
/// That is a **finding, not a fixture defect**: it is the very hole the driver-loop-head
/// clear exists to close, still open for the in-block route. It is left pinned (rather
/// than deleted) so the closing change has a red-to-green gate: the fix has to give a
/// family `Goto` a block-ABI word of its own (or clear the witness on that route), and
/// then this test simply loses its `#[ignore]`.
///
/// `#[ignore]`d because it currently fails; run it with `--ignored` to reproduce.
#[test]
#[ignore = "known gap: a raise whose handler is its own `pc + 1` links in-block past the \
            driver's witness clear (see the test docs)"]
fn a_raise_whose_handler_is_its_own_next_instruction_reads_a_stale_y_window() {
    let (mut module, atom) = raise_across_frames_module_code(&[]);
    intern_exception_atoms(&mut module);
    let (outcome, snapshot) = run_counting(&module, atom, Tier::BaselineJit);
    match &outcome {
        Ok(RunOutcome::Returned(value)) => assert_eq!(
            value.raw(),
            make_small(11).raw(),
            "the handler block must read frame A's y(0) through the live window even when \
             it is the raising instruction's own pc + 1 (publish_y={} publish_y_skips={})",
            snapshot.publish_y,
            snapshot.publish_y_skips,
        ),
        other => panic!("the raise-across-frames shape must return 11, got {other:?}"),
    }
    with_both_tiers(&module, atom, |interpreter, baseline| {
        assert_same_outcome(
            "raise_across_frames_at_pc_plus_one",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

/// The instruction body shared by [`raise_across_frames_module`] and the known-gap pin
/// [`a_raise_whose_handler_is_its_own_next_instruction_reads_a_stale_y_window`]:
/// `tail` is spliced in between the raising `case_end` and the handler label, so the
/// handler is (`[]`) or is not (the unreachable filler) the raise's own `pc + 1`.
fn raise_across_frames_module_code(tail: &[Instr]) -> (Module, Atom) {
    let mut code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(2),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(11),
            dst: Operand::Y(0),
        },
        // The catch frame records the function depth of frame A.
        Instr::Try {
            frame: Operand::X(1),
            fail: Label(9),
        },
        // A second frame: `jit_allocate` re-publishes the Y window for frame B, so the
        // witness is armed and describes the frame the raise is about to destroy.
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        // A family raise (`{case_clause, x(0)}`) — the unwinding site that does **not**
        // invalidate the witness itself.
        Instr::CaseEnd {
            value: Operand::X(0),
        },
    ];
    code.extend_from_slice(tail);
    // The handler: its own block.
    code.extend_from_slice(&[
        Instr::Label(Label(9)),
        Instr::TryCase {
            frame: Operand::X(1),
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
    ]);
    body_module(code)
}

/// **Family frame-neutrality, shape (i): the `exceptions` family.** A `raise` caught in a
/// live `try` of the *same* frame — `try` / `case_end` / `try_case` all run in `jit_family`
/// — leaves the frame depth and `machine.stack` untouched, and the handler's `y(0)` read
/// must still resolve through the published window (which the gated in-block refresh
/// re-publishes, or skips, after each family helper).
///
/// The tiers must agree end to end: the interpreter's own step and the native helper run
/// the identical `families::dispatch` body, so the observable is the interpreter's by
/// construction — the pin is that the *emitted* arm's window discipline does not perturb it.
#[test]
fn a_raise_caught_in_the_same_frame_leaves_the_y_window_usable() {
    let (module, atom) = same_frame_raise_module();
    let (outcome, snapshot) = run_counting(&module, atom, Tier::BaselineJit);
    counters::reset();
    match &outcome {
        Ok(RunOutcome::Returned(value)) => assert_eq!(
            value.raw(),
            make_small(11).raw(),
            "the handler must read the y(0) its own frame wrote before the try"
        ),
        other => panic!("the same-frame raise shape must return 11, got {other:?}"),
    }
    assert_eq!(
        snapshot.step_one, 0,
        "every instruction of the shape is lowered; a deopt here would mask the witness pin"
    );
    assert!(
        snapshot.refresh_y > 0,
        "the family helpers are followed by a gated in-block refresh (a `Y` access still \
         follows), so this shape must exercise `jit_refresh_y`"
    );
    with_both_tiers(&module, atom, |interpreter, baseline| {
        assert_same_outcome(
            "same_frame_raise",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

/// The shape [`a_raise_caught_in_the_same_frame_leaves_the_y_window_usable`] drives: one
/// frame, one `y(0)`, a `try` whose `case_end` raises straight into its own `try_case`.
fn same_frame_raise_module() -> (Module, Atom) {
    let (mut module, atom) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(11),
            dst: Operand::Y(0),
        },
        Instr::Try {
            frame: Operand::X(1),
            fail: Label(5),
        },
        // Caught by the `try` above, in this very frame: `unwind_to_frame` truncates
        // nothing, so the published window still names the live frame.
        Instr::CaseEnd {
            value: Operand::X(0),
        },
        Instr::Label(Label(5)),
        Instr::TryCase {
            frame: Operand::X(1),
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ]);
    intern_exception_atoms(&mut module);
    (module, atom)
}

/// **Family frame-neutrality, shape (ii): `MakeFun3` + `CallFun`.** Both are family-owned
/// (so both are an in-block helper call), and a closure apply is a *value-returning*
/// synchronous seam call: no frame is pushed, `machine.stack` does not move. A `Y` access
/// after the call must therefore still resolve through the published window, and the
/// tiers must agree.
///
/// The host serves the apply through [`MinimalContext::with_apply_hook`] (the shared
/// `with_both_tiers` harness installs no hook, so this shape supplies its own runner).
///
/// The module carries a real `FunT` entry: `make_fun3`'s `index` addresses
/// `Module::fun`, and a fun-table miss is a typed error rather than a closure.
#[test]
fn a_fun_build_and_apply_leave_the_y_window_usable() {
    let (module, atom) = fun_shape_module();
    let (outcome, snapshot) =
        run_counting_with(|| fun_shape_ctx(), &module, atom, Tier::BaselineJit);
    counters::reset();
    match &outcome {
        Ok(RunOutcome::Returned(value)) => assert_eq!(
            value.raw(),
            make_small(11).raw(),
            "the y(0) read after `make_fun3`/`call_fun` must be the value written before them"
        ),
        other => panic!("the fun shape must return 11, got {other:?}"),
    }
    assert_eq!(
        snapshot.step_one, 0,
        "every instruction of the shape is lowered; a deopt here would mask the witness pin"
    );
    assert!(
        snapshot.refresh_y > 0,
        "`call_fun` is a family helper with a `Y` access after it, so the gated in-block \
         refresh must run"
    );
    // Tier agreement, with the same apply hook on both sides.
    with_both_tiers_ctx(&module, atom, fun_shape_ctx, |interpreter, baseline| {
        assert_same_outcome(
            "fun_build_and_apply",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

/// The apply hook both tiers of [`a_fun_build_and_apply_leave_the_y_window_usable`] use:
/// answer `42` for any closure (the shape never enters the lambda body, it only has to be
/// *a* closure of the called arity).
fn fun_shape_ctx() -> MinimalContext {
    MinimalContext::with_apply_hook(Box::new(|_fun, _args| Ok(make_small(42))))
}

/// The shape [`a_fun_build_and_apply_leave_the_y_window_usable`] drives: `allocate`, write
/// `y(0)`, build a closure (`make_fun3`), apply it (`call_fun`), read `y(0)` back.
fn fun_shape_module() -> (Module, Atom) {
    let (mut module, atom) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(11),
            dst: Operand::Y(0),
        },
        Instr::MakeFun3 {
            index: 0,
            dst: Operand::X(0),
            env: Vec::new(),
        },
        Instr::CallFun { arity: 0 },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ]);
    let function = module.atom_table.intern("lambda");
    module.funs.push(beam_loader::FunDescriptor {
        function,
        arity: 0,
        label: Label(1),
        index: 0,
        num_free: 0,
        uniq: 1,
    });
    (module, atom)
}

/// [`run_counting`](super::tests::run_counting) over a caller-supplied context — the
/// counting twin of the apply-hook runner above.
fn run_counting_with(
    make_ctx: impl FnOnce() -> MinimalContext,
    module: &Module,
    name: Atom,
    tier: Tier,
) -> (Result<RunOutcome>, counters::Counters) {
    counters::reset();
    counters::set_enabled(true);
    let mut machine = Machine::new();
    let mut ctx = make_ctx();
    let outcome = machine.run_export_outcome_with_tier(module, name, 0, &[], &mut ctx, tier);
    counters::set_enabled(false);
    (outcome, counters::snapshot())
}

/// [`with_both_tiers`](super::tests::with_both_tiers) over a caller-supplied context
/// factory (one fresh context per tier, exactly as the shared harness does), so a shape
/// that needs a host hook can still be compared tier-to-tier.
fn with_both_tiers_ctx<R>(
    module: &Module,
    name: Atom,
    mut make_ctx: impl FnMut() -> MinimalContext,
    check: impl FnOnce(&Result<RunOutcome>, &Result<RunOutcome>) -> R,
) -> R {
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = make_ctx();
    let interpreter_outcome = interpreter.run_export_outcome_with_tier(
        module,
        name,
        0,
        &[],
        &mut interpreter_ctx,
        Tier::Interpreter,
    );
    let mut baseline = Machine::new();
    let mut baseline_ctx = make_ctx();
    let baseline_outcome = baseline.run_export_outcome_with_tier(
        module,
        name,
        0,
        &[],
        &mut baseline_ctx,
        Tier::BaselineJit,
    );
    check(&interpreter_outcome, &baseline_outcome)
}

// ---------------------------------------------------------------------------
// The dynamic probe (measurement only, `#[ignore]`d so it never runs in the suite)
// ---------------------------------------------------------------------------
/// **The dynamic Y-witness probe.** Runs every `crate::jit::bench` tier workload once on
/// `Tier::BaselineJit` with the structural counters recording, then prints
/// `publish_y`/`publish_y_skips`/`refresh_y`/`refresh_y_skips` and the emitting-range split
/// beside `/proc/loadavg` (the numbers are only readable next to the host load).
///
/// `#[ignore]`d: it is a measurement, never an assertion, so it must not run in the normal
/// suite. Run it with
/// `... -p beam_engine --lib -- --ignored --nocapture the_dynamic_y_witness_probe_is_printed`.
#[test]
#[ignore = "measurement probe: run explicitly with --ignored --nocapture"]
fn the_dynamic_y_witness_probe_is_printed() {
    println!(
        "--- dynamic y-witness probe: loadavg before={} ---",
        probe_loadavg()
    );
    counters::reset();
    counters::set_enabled(true);
    crate::jit::bench::run_workloads_on(Tier::BaselineJit);
    counters::set_enabled(false);
    let snapshot = counters::snapshot();
    counters::reset();
    print_probe_counters(
        "dynamic y-witness (Tier::BaselineJit, all bench workloads)",
        &snapshot,
    );
    println!(
        "--- dynamic y-witness probe: loadavg after={} ---",
        probe_loadavg()
    );
}
