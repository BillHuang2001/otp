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

//! **The GC root-set hand-off for `alloc_words`-driven allocation.**
//!
//! `ensure_heap` (`test_heap`/`allocate_heap`) is the engine's admission point: it
//! hands the live root set over as mutable slices, so a host may collect and move
//! terms in place. Term **construction**, however, goes through
//! [`ExecutionContext::alloc_words`], which has no admission point and no root
//! access — so a host whose heap must be collected could never collect while the
//! engine built a term.
//!
//! This suite pins the additive fix, with **no `beam_gc` dependency**:
//!
//! * [`Machine::with_roots`]/[`Machine::alloc_rooted`] hand the engine's
//!   **whole** live root set over as a [`RootSet`] — the X-register file, the whole
//!   Erlang stack and the whole FR file (the FR file holds `Eterm` **term slots**,
//!   so a float register can point into the heap and must be a root). The three
//!   files are borrowed together because three separate `&mut self` accessors
//!   cannot coexist.
//! * [`ExecutionContext::alloc_words_rooted`] is the defaulted counterpart of
//!   `alloc_words`; its default discards the roots and forwards to `alloc_words`,
//!   so an existing host compiles and behaves **byte-identically**.
//! * a recording context (a mock collector) proves the hook **fires** for an
//!   `alloc_words`-only loop and that the roots it sees are the machine's live
//!   X registers, Y slots and FR slots — and that the slices are **mutable**, so an
//!   in-place relocation writes back into the machine.
//!
//! # Running it
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --test gc_roots
//! ```

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_engine::context::RootSet;
use beam_engine::machine::MAX_FLOAT_REGISTER;
use beam_engine::{ExecutionContext, Machine, MinimalContext, RunOutcome, MAX_REGISTER};
use beam_loader::{Export, Function, HeapNeed, Instr, Label, Module, Operand};
use beam_term::{make_small, Atom, Eterm};

// ---------------------------------------------------------------------------
// A synthetic module that suspends with a live frame
// ---------------------------------------------------------------------------

/// `gcroots_wait/0` = `allocate 2; Y0 = 0x1234; wait resume; resume: return`.
///
/// Running it through [`Machine::run_export_outcome`] leaves the machine
/// **suspended** (`RunOutcome::Wait`) with a live frame: two Y slots, `Y(0)` holding
/// a known term. That is the realistic "machine with live roots" state for the
/// hand-off — the stack cannot be seeded from outside the crate (`push_frame`/`set_y`
/// are `pub(crate)`).
fn waiting_module() -> (Module, Atom) {
    let mut module = Module::default();
    let function = module.atom_table.intern("gcroots_wait");
    module.name = function;
    let entry = Label(1);
    let resume = Label(2);
    let code = vec![
        Instr::Label(entry),
        Instr::Allocate {
            stack_need: HeapNeed::Words(2),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(0x1234),
            dst: Operand::Y(0),
        },
        Instr::Wait { target: resume },
        Instr::Label(resume),
        Instr::Return,
    ];
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
        arity: 0,
        code,
        blocks: Vec::new(),
        labels,
    });
    module.exports.push(Export {
        function,
        arity: 0,
        label: entry,
    });
    (module, function)
}

/// The Y slot `waiting_module` writes (`Y(0)`), for the assertions below.
const Y0_VALUE: i64 = 0x1234;

/// `gcroots_testheap/0` = `allocate 1; Y0 = 0x1234; wait resume; resume: test_heap 8
/// 3; return`.
///
/// The `test_heap` **after** the `wait` is what reaches [`Machine::ensure_heap`] — the
/// engine's primary admission point (`test_heap`/`allocate_heap`) — when the machine
/// is resumed. Suspending first is what gives the machine a real live root set (a
/// live Y slot; the stack cannot be seeded from outside the crate), so the recording
/// context observes genuine X/Y/FR roots at that door too, instead of a hand-forged
/// state.
fn waiting_test_heap_module() -> (Module, Atom) {
    let mut module = Module::default();
    let function = module.atom_table.intern("gcroots_testheap");
    module.name = function;
    let entry = Label(1);
    let resume = Label(2);
    let code = vec![
        Instr::Label(entry),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(0x1234),
            dst: Operand::Y(0),
        },
        Instr::Wait { target: resume },
        Instr::Label(resume),
        Instr::TestHeap {
            heap_need: HeapNeed::Words(8),
            live: 3,
        },
        Instr::Return,
    ];
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
        arity: 0,
        code,
        blocks: Vec::new(),
        labels,
    });
    module.exports.push(Export {
        function,
        arity: 0,
        label: entry,
    });
    (module, function)
}

// ---------------------------------------------------------------------------
// The mock collector: records every rooted allocation and its root set
// ---------------------------------------------------------------------------

/// What the mock collector observed, one entry per hook invocation.
#[derive(Default)]
struct Seen {
    calls: usize,
    x: Vec<Vec<Eterm>>,
    stack: Vec<Vec<Eterm>>,
    fr: Vec<Vec<Eterm>>,
}

/// A synthetic [`Machine`]-owned heap behind
/// [`Machine::alloc_rooted`](beam_engine::Machine::alloc_rooted).
///
/// [`RootRecorder::alloc_words_rooted`] records the root set it is handed and then
/// **writes through** it (simulating a relocating collector: the next value of
/// `x(1)` is decided by the heap owner, and the write must land in the machine's
/// own register file) before handing back uninitialized storage.
struct RootRecorder {
    seen: Rc<RefCell<Seen>>,
    /// The value the "collector" relocates the term in `x(1)` to on every call.
    relocated: Eterm,
    /// The blocks this mock heap has handed out (kept alive & immovable).
    blocks: Vec<Box<[Eterm]>>,
}

impl RootRecorder {
    fn new(relocated: Eterm) -> (RootRecorder, Rc<RefCell<Seen>>) {
        let seen = Rc::new(RefCell::new(Seen::default()));
        (
            RootRecorder {
                seen: Rc::clone(&seen),
                relocated,
                blocks: Vec::new(),
            },
            seen,
        )
    }
}

impl ExecutionContext for RootRecorder {
    fn call_bif(
        &mut self,
        _module: &str,
        _function: &str,
        _arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        Err(BeamError::BadArg)
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        Ok(())
    }

    fn alloc_words_rooted(&mut self, n: usize, roots: RootSet<'_>) -> Result<*mut Eterm> {
        {
            let mut seen = self.seen.borrow_mut();
            seen.calls += 1;
            seen.x.push(roots.x.to_vec());
            seen.stack.push(roots.stack.to_vec());
            seen.fr.push(roots.fr.to_vec());
        }
        // A real collector moves terms and writes them back through `roots`. Here
        // the "move" relocates `x(1)` to a value that only the heap owner knows, so
        // the test can prove the write reached the machine's register file.
        if let Some(slot) = roots.x.get_mut(1) {
            *slot = self.relocated;
        }
        let mut block = vec![Eterm::default(); n].into_boxed_slice();
        let ptr = block.as_mut_ptr();
        self.blocks.push(block);
        Ok(ptr)
    }
}

// ---------------------------------------------------------------------------
// The mock collector for `ensure_heap` — the primary admission point
// ---------------------------------------------------------------------------

/// What [`HeapRecorder`] observed at the `ensure_heap` door.
#[derive(Default)]
struct HeapSeen {
    /// How many times the **full-root** door (`ensure_heap_full`) was used.
    full_calls: usize,
    /// How many times the narrow, FR-less `ensure_heap` was used (expected: 0).
    narrow_calls: usize,
    x: Vec<Vec<Eterm>>,
    stack: Vec<Vec<Eterm>>,
    fr: Vec<Vec<Eterm>>,
}

/// A recording context that pins the **whole root set** at the engine's primary
/// admission point ([`Machine::ensure_heap`], reached by `test_heap`/`allocate_heap`).
///
/// It overrides the full-root door
/// [`ExecutionContext::ensure_heap_full`](beam_engine::ExecutionContext::ensure_heap_full)
/// — the one the machine now calls — and counts the narrow `ensure_heap` separately,
/// so a passing test proves the FR file travels with it (the FR file would be lost on
/// the narrow door). A write through `roots.fr` simulates a relocating collector, and
/// it must land back in `Machine::fr`.
struct HeapRecorder {
    seen: Rc<RefCell<HeapSeen>>,
    /// The value the "collector" relocates the term in `fr(7)` to.
    relocated_fr: Eterm,
}

impl HeapRecorder {
    fn new(relocated_fr: Eterm) -> (HeapRecorder, Rc<RefCell<HeapSeen>>) {
        let seen = Rc::new(RefCell::new(HeapSeen::default()));
        (
            HeapRecorder {
                seen: Rc::clone(&seen),
                relocated_fr,
            },
            seen,
        )
    }
}

impl ExecutionContext for HeapRecorder {
    fn call_bif(
        &mut self,
        _module: &str,
        _function: &str,
        _arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        Err(BeamError::BadArg)
    }

    /// The narrow door (no FR file): the machine must not use it any more, so this is
    /// counted but never expected.
    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        self.seen.borrow_mut().narrow_calls += 1;
        Ok(())
    }

    fn ensure_heap_full(&mut self, _heap_need: u32, roots: RootSet<'_>) -> Result<()> {
        {
            let mut seen = self.seen.borrow_mut();
            seen.full_calls += 1;
            seen.x.push(roots.x.to_vec());
            seen.stack.push(roots.stack.to_vec());
            seen.fr.push(roots.fr.to_vec());
        }
        // A real collector moves terms and writes them back through `roots`; here the
        // "move" relocates `fr(7)` to a value only the heap owner knows, so the test
        // can prove the write reached the machine's FR file.
        if let Some(slot) = roots.fr.get_mut(7) {
            *slot = self.relocated_fr;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The proof
// ---------------------------------------------------------------------------

#[test]
fn an_alloc_words_loop_reaches_the_hook_with_x_stack_and_fr_roots() {
    let (module, function) = waiting_module();
    let relocated = make_small(0x0BAD_C0DE);
    let (mut ctx, seen) = RootRecorder::new(relocated);
    let mut machine = Machine::new();

    // Suspend the module: the machine now has a live frame whose Y slots are roots.
    let outcome = machine
        .run_export_outcome(&module, function, 0, &[], &mut ctx)
        .expect("the synthetic module suspends rather than fails");
    assert!(
        matches!(outcome, RunOutcome::Wait { .. }),
        "`wait` must report a suspend, got {outcome:?}"
    );
    assert_eq!(
        machine.stack_depth(),
        2,
        "the suspended frame's two Y slots are live roots"
    );
    assert_eq!(
        seen.borrow().calls,
        0,
        "the run itself allocates through `ensure_heap`/`alloc_words`, never through the rooted hook"
    );

    // Live terms in the two register files.
    let x_mark = make_small(0xABCD);
    machine.x_mut()[3] = x_mark;
    let fr_mark = make_small(0x5678);
    machine.fr_mut()[7] = fr_mark;
    let y0 = machine.stack_mut()[0];
    assert_eq!(y0, make_small(Y0_VALUE));

    // The allocation-heavy `alloc_words`-only loop: every iteration is one
    // rooted allocation with the machine's whole root set in hand.
    const CALLS: usize = 16;
    for _ in 0..CALLS {
        let ptr = machine
            .alloc_rooted(&mut ctx, 4, RootSet::ALL_LIVE)
            .expect("the mock heap serves the request");
        assert!(!ptr.is_null());
    }

    let seen = seen.borrow();
    assert_eq!(
        seen.calls, CALLS,
        "the rooted hook must fire once per allocation, not zero times (the \
         `collections: 0` gap)"
    );
    for i in 0..CALLS {
        assert_eq!(
            seen.x[i].len(),
            MAX_REGISTER as usize,
            "the whole X-register file is handed over"
        );
        assert_eq!(
            seen.fr[i].len(),
            MAX_FLOAT_REGISTER as usize,
            "the whole FR file is handed over (a float register holds a term slot)"
        );
        assert_eq!(seen.x[i][3], x_mark, "the live X register is a root");
        assert_eq!(seen.fr[i][7], fr_mark, "the live FR slot is a root");
        assert_eq!(
            seen.stack[i],
            vec![y0, Eterm::default()],
            "the whole Erlang stack (every Y slot) is handed over"
        );
    }
    drop(seen);

    // Mutability: the collector's in-place relocation landed in the machine.
    assert_eq!(
        machine.x(1),
        Some(relocated),
        "a term moved through the root set must be visible in the machine"
    );
    // ...and the root that was never relocated is untouched.
    assert_eq!(machine.x(3), Some(x_mark));
}

#[test]
fn the_ensure_heap_admission_point_hands_over_the_fr_file() {
    // The gap this closes: `test_heap`/`allocate_heap` route through
    // `Machine::ensure_heap`, which today handed over only `x`/`stack`. A module with
    // any float-family instruction could therefore never be collected over correctly.
    // The machine must now reach the full-root door, and the FR file must be in it.
    let (module, function) = waiting_test_heap_module();
    let relocated_fr = make_small(0x0F0F);
    let (mut ctx, seen) = HeapRecorder::new(relocated_fr);
    let mut machine = Machine::new();

    // Suspend first: the machine now really has a live frame (a root), and the X/FR
    // files can be seeded afterwards (a run resets them; the stack cannot be seeded
    // from outside the crate).
    let outcome = machine
        .run_export_outcome(&module, function, 0, &[], &mut ctx)
        .expect("the synthetic module suspends rather than fails");
    let (func, pc) = match outcome {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("expected a wait, got {other:?}"),
    };
    assert_eq!(
        machine.stack_depth(),
        1,
        "the suspended frame's Y slot is a live root"
    );
    assert_eq!(
        seen.borrow().full_calls,
        0,
        "the suspend happens at `wait`, before the `test_heap` admission point"
    );

    // Live terms in the three files, exactly as a collector would have to see them.
    let x_mark = make_small(0xABCD);
    machine.x_mut()[3] = x_mark;
    let fr_mark = make_small(0x5678);
    machine.fr_mut()[7] = fr_mark;
    let y0 = machine.stack_mut()[0];
    assert_eq!(y0, make_small(Y0_VALUE));

    // Resume into `test_heap` — the engine's primary admission point.
    let resumed = machine
        .resume_outcome(&module, func, pc, &mut ctx)
        .expect("resume reaches the test_heap");
    assert!(
        matches!(resumed, RunOutcome::Returned(_)),
        "the module returns after the test_heap, got {resumed:?}"
    );

    let seen = seen.borrow();
    assert_eq!(
        seen.full_calls, 1,
        "the full-root door (`ensure_heap_full`) is the one the machine calls"
    );
    assert_eq!(
        seen.narrow_calls, 0,
        "the narrow, FR-less door is no longer used (it would drop the FR root)"
    );
    assert_eq!(
        seen.x[0].len(),
        MAX_REGISTER as usize,
        "the whole X-register file is handed over"
    );
    assert_eq!(
        seen.fr[0].len(),
        MAX_FLOAT_REGISTER as usize,
        "the whole FR file is handed over — the gap this suite closes"
    );
    assert_eq!(seen.x[0][3], x_mark, "the live X register is a root");
    assert_eq!(seen.fr[0][7], fr_mark, "the live FR slot is a root");
    assert_eq!(
        seen.stack[0],
        vec![y0],
        "the whole Erlang stack (every Y slot) is handed over"
    );
    drop(seen);

    // Mutability: the collector's relocation through the FR root landed in the machine.
    assert_eq!(
        machine.get_fr(7),
        Some(relocated_fr),
        "a term moved through the FR root must be visible in the machine"
    );
    // The untouched X root is preserved.
    assert_eq!(machine.x(3), Some(x_mark));
}

#[test]
fn with_roots_hands_over_the_machines_own_files() {
    let mut machine = Machine::new();
    let x_ptr = machine.x_mut().as_ptr();
    let fr_ptr = machine.fr_mut().as_ptr();
    let stack_ptr = machine.stack_mut().as_ptr();
    machine.with_roots(RootSet::ALL_LIVE, |roots| {
        assert_eq!(roots.x.as_ptr(), x_ptr, "the machine's own X-register file");
        assert_eq!(roots.fr.as_ptr(), fr_ptr, "the machine's own FR file");
        assert_eq!(roots.stack.as_ptr(), stack_ptr, "the machine's own stack");
        assert_eq!(roots.x.len(), MAX_REGISTER as usize);
        assert_eq!(roots.fr.len(), MAX_FLOAT_REGISTER as usize);
        assert_eq!(roots.live_len(), MAX_REGISTER as usize);
    });
}

#[test]
fn the_default_rooted_allocation_forwards_to_alloc_words() {
    // A host that implements only `alloc_words` gets the rooted entry for free: the
    // trait default discards the roots and forwards, so its behaviour is unchanged.
    let mut ctx = MinimalContext::new();
    let mut x = [Eterm::nil(); 4];
    let mut stack: [Eterm; 0] = [];
    let mut fr = [Eterm::nil(); 2];
    let roots = RootSet::new(&mut x, &mut stack, &mut fr, RootSet::ALL_LIVE);
    let ptr = ctx
        .alloc_words_rooted(8, roots)
        .expect("the default forwards to the arena-backed `alloc_words`");
    assert!(!ptr.is_null());
}

/// A host written against the **original** seam: it implements only the two
/// required methods plus `alloc_words` — it has never heard of the root set.
struct OldStyleHost {
    allocs: usize,
}

impl ExecutionContext for OldStyleHost {
    fn call_bif(
        &mut self,
        _module: &str,
        _function: &str,
        _arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        Err(BeamError::BadArg)
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        Ok(())
    }

    fn alloc_words(&mut self, _n: usize) -> Result<*mut Eterm> {
        self.allocs += 1;
        // A well-aligned, non-null (dangling) pointer: this host only counts calls.
        Ok(core::ptr::NonNull::<Eterm>::dangling().as_ptr())
    }
}

#[test]
fn an_existing_host_needs_no_change_and_its_alloc_words_is_used() {
    let mut host = OldStyleHost { allocs: 0 };
    let mut x = [Eterm::nil(); 2];
    let mut stack: [Eterm; 0] = [];
    let mut fr: [Eterm; 0] = [];
    let roots = RootSet::new(&mut x, &mut stack, &mut fr, 1);
    let ptr = host
        .alloc_words_rooted(4, roots)
        .expect("the default routes to the pre-existing `alloc_words`");
    assert!(!ptr.is_null());
    assert_eq!(host.allocs, 1, "the old implementation is what ran");
}

/// A host with no heap at all: the defaulted method must keep the unrooted
/// default's typed `unsupported`, never invent storage.
struct BareHost;

impl ExecutionContext for BareHost {
    fn call_bif(
        &mut self,
        _module: &str,
        _function: &str,
        _arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        Err(BeamError::BadArg)
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        Ok(())
    }
}

#[test]
fn the_default_rooted_allocation_keeps_the_unrooted_default_error() {
    let mut host = BareHost;
    let mut x = [Eterm::nil(); 1];
    let mut stack: [Eterm; 0] = [];
    let mut fr: [Eterm; 0] = [];
    let roots = RootSet::new(&mut x, &mut stack, &mut fr, 1);
    assert!(host.alloc_words_rooted(4, roots).is_err());
}

/// `Machine::alloc_rooted` against that host: the roots are handed over, the default
/// forwards to a host with no heap, and the typed error propagates — never a panic.
#[test]
fn alloc_rooted_propagates_the_hosts_allocation_error() {
    let mut machine = Machine::new();
    let err = machine
        .alloc_rooted(&mut BareHost, 8, RootSet::ALL_LIVE)
        .expect_err("a host with no heap cannot serve words");
    assert!(matches!(err, BeamError::Raise(_)), "typed error, got {err:?}");
}

#[test]
fn root_set_live_len_is_clamped_and_all_live_means_every_register() {
    let mut x = [Eterm::nil(); 4];
    let mut stack: [Eterm; 0] = [];
    let mut fr: [Eterm; 0] = [];
    let mut roots = RootSet::new(&mut x, &mut stack, &mut fr, 2);
    assert_eq!(roots.live_len(), 2);
    assert_eq!(roots.live_x().len(), 2);
    assert_eq!(roots.live_x_mut().len(), 2);
    // A hint larger than the file is clamped, never an out-of-bounds slice.
    roots.live = RootSet::ALL_LIVE;
    assert_eq!(RootSet::ALL_LIVE, u32::MAX);
    assert_eq!(roots.live_len(), 4);
    assert_eq!(roots.live_x_mut().len(), 4);
}
