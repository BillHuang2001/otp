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
        Some(frame) => (
            unsafe { base.add(frame.y_base) },
            unsafe { base.add(frame.y_base + frame.y_count as usize) },
        ),
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
    let labels = LabelMap::new();
    let mut machine = Machine::new();
    machine.push_frame(y_count).expect("push_frame");
    let mut ctx = MinimalContext::new();
    let x_regs = machine.x.as_mut_ptr();
    let mut rt = JitRuntime {
        machine: &mut machine,
        x_regs,
        module,
        entry_module: module,
        labels: &labels,
        ctx: &mut ctx,
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
    assert!(rt.y_window_current, "the first publish must arm the witness");
    assert_eq!(rt.y_base, expected.0, "the derivation must name the live base");
    assert_eq!(rt.y_end, expected.1, "the derivation must name the live end");
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
    // `jit_allocate_heap`'s **error** arm needs a refusing context; the success arm is
    // pinned by `the_republishing_frame_helpers_publish_the_live_window`.
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
        assert_eq!(rt.y_base, live.0, "the republished base must be the live one");
        assert_eq!(rt.y_end, live.1, "the republished end must be the live one");
    });
    // `jit_allocate_heap`: admission then push.
    with_armed_runtime(&module, 1, |rt| {
        assert_eq!(
            super::runtime::jit_allocate_heap(rt, 0, 4, 2, 8, 0),
            super::runtime::encode_target(0, 5),
        );
        assert!(rt.y_window_current, "allocate_heap must republish the window");
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
    assert!(
        snapshot.publish_y > 0,
        "the block's prologue must publish the window"
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
        assert_same_outcome("y_read_after_skipped_refresh", interpreter, baseline, &module.atom_table);
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
    println!("--- dynamic y-witness probe: loadavg before={} ---", probe_loadavg());
    counters::reset();
    counters::set_enabled(true);
    crate::jit::bench::run_workloads_on(Tier::BaselineJit);
    counters::set_enabled(false);
    let snapshot = counters::snapshot();
    counters::reset();
    print_probe_counters("dynamic y-witness (Tier::BaselineJit, all bench workloads)", &snapshot);
    println!("--- dynamic y-witness probe: loadavg after={} ---", probe_loadavg());
}
