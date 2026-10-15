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

//! **A live raise latch survives a collection between the raise and the
//! cooked trace.**
//!
//! A `function_clause` raise parks the failed clause's argument list on the
//! machine ([`Machine::raise_function_clause_args`], promoted from the one-shot
//! `pending_function_clause_args` latch by `Machine::throw_with_trace`). The
//! handler's `build_stacktrace` then cooks `[{M,F,A,Location}]` from it — but the
//! list is a **machine-held heap term** that is *not* one of the frozen root-set
//! files, so a collecting host would move every term it can reach **except** that
//! one, leaving the latch pointing into the region the collector just vacated.
//!
//! [`Machine::with_roots`](beam_engine::Machine::with_roots) closes that window by
//! **parking** whichever latches are armed at the end of the Erlang stack for the
//! duration of the root hand-off and **re-seating** each field from its slot
//! afterwards: the stack is part of the frozen root set and is rooted in full (no
//! liveness map over Y slots), so a conforming host relocates the parked word and
//! the engine reads the *moved* term back.
//!
//! This suite proves that end to end, **from the public API only**, over a
//! semi-space copying mock collector ([`MovingContext`]) that relocates every term
//! it is handed and poisons the region it vacated — so a latch left behind reads
//! garbage. The fixture is a hand-built [`Module`] (no loader involved):
//!
//! ```text
//! run/0:
//!   allocate 2
//!   X2 = {7, 8, 9}
//!   X0 = X2 ; X1 = 42        % the two call arguments
//!   try Y0, L3
//!   call f/2                 % local call → f/2's func_info raises function_clause
//!   try_end Y0 ; deallocate 2 ; return
//! L3:
//!   try_case Y0 ; X0 = X2 ; build_stacktrace ; deallocate 2 ; return
//! f/2:
//!   func_info module f 2     % the failed head clause → the argument-list raise site
//!   return
//! ```
//!
//! The cooked trace's head item is `{module, f, [{7,8,9}, 42], []}` — the failed
//! call's **argument list** in the `A` slot, exactly as C's `erts_build_mfa_item`
//! renders it — and the whole graph, the argument tuple included, must live in the
//! collector's **post-move** region.
//!
//! Three more cases pin the neighbouring behaviour: the **fast path** (a run with no
//! armed latch hands over an *unmodified* root set — the stack carries the live Y
//! slots and nothing else), a **repeat** run on one machine (the stack is restored
//! after every run), and an **oracle** cross-check of the `A` slot against the real
//! ERTS (which skips loudly when no `erl`/`erlc` is on `PATH`).
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --test raise_rooting
//! ```

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_engine::context::RootSet;
use beam_engine::{terms, ExecutionContext, Machine, RunOutcome};
use beam_loader::{Export, Function, HeapNeed, Instr, Label, Module, Operand};
use beam_term::{
    boxed_val, is_boxed_term, is_list, make_atom, make_small, Atom, Eterm, NIL, TAG_PTR_MASK,
};

// ---------------------------------------------------------------------------
// The hand-built synthetic modules
// ---------------------------------------------------------------------------

/// `run/0`'s entry label (the lowest label of the function).
const L_RUN: Label = Label(1);
/// `run/0`'s `try`/`catch` handler label.
const L_HANDLER: Label = Label(3);
/// `f/2`'s `func_info` label — the raise site the caller's local `call` targets.
const L_FUNINFO: Label = Label(10);
/// The instruction after `f/2`'s `func_info` (the callable entry the loader would
/// normally mark; unused here, but it keeps the function's label table honest).
const L_F_ENTRY: Label = Label(11);

/// The argument tuple `run/0` passes as `f/2`'s first argument.
const ARG_TUPLE: [i64; 3] = [7, 8, 9];
/// The second argument (a small int, so it never moves).
const ARG_SECOND: i64 = 42;

/// The reserving instructions `run/0` performs — the counting witness below.
/// `{7,8,9}` is one `put_tuple2`, the failed-clause list is one `reserve_slack`, and
/// the cooked trace is one `reserve_slack`.
const RAISER_RESERVES: usize = 3;
/// The latch-free `plain/0` performs exactly one rooted reservation (`put_tuple2`).
const PLAIN_RESERVES: usize = 1;

/// The named module plus the atoms the assertions name (`run`, `f`, the module).
struct Fixtures {
    module: Module,
    run: Atom,
    f: Atom,
}

/// The raising module of the module docs: `run/0` calls `f/2`, whose `func_info`
/// raises `function_clause` with the real call arguments, and the handler cooks the
/// trace.
///
/// `f/2` is reached by a **local** `call` straight to its `func_info` label — the
/// engine has no first-entry/re-entry test there (`i_func_info_IaaI`), so the raise
/// happens exactly as it does on the compiler's failed-head path (which jumps to
/// `func_info`).
fn raising_module() -> Fixtures {
    let mut fixture = Fixture::new("rr");
    let module_atom = fixture.module.name;
    let f_atom = fixture.atom("f");
    // The raise site resolves both names from the running module's own atom table
    // (`function_clause_raise`): the reason atom is mandatory, the `error` class atom
    // is installed when present, so both must be interned here.
    fixture.atom("function_clause");
    fixture.atom("error");

    let run_code = vec![
        Instr::Label(L_RUN),
        Instr::Allocate {
            stack_need: HeapNeed::Words(2),
            live: 0,
        },
        Instr::PutTuple2 {
            dst: Operand::X(2),
            elements: ARG_TUPLE
                .iter()
                .map(|n| Operand::Integer(*n))
                .collect(),
        },
        Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(ARG_SECOND),
            dst: Operand::X(1),
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: L_HANDLER,
        },
        Instr::Call {
            arity: 2,
            target: L_FUNINFO,
        },
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
        Instr::Label(L_HANDLER),
        Instr::TryCase {
            frame: Operand::Y(0),
        },
        // The compiled handler shape: the raw trace lands in x(2), the handler moves
        // it to x(0) for `build_stacktrace`.
        Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        },
        Instr::BuildStacktrace,
        Instr::Deallocate(2),
        Instr::Return,
    ];
    let f_code = vec![
        Instr::Label(L_FUNINFO),
        Instr::FuncInfo {
            module: module_atom,
            function: f_atom,
            arity: 2,
        },
        Instr::Label(L_F_ENTRY),
        Instr::Return,
    ];

    let run_label = fixture.add_function("run", 0, run_code);
    fixture.add_function("f", 2, f_code);
    let run_atom = fixture.export("run", 0, run_label);
    Fixtures {
        module: fixture.finish(),
        run: run_atom,
        f: f_atom,
    }
}

/// A **latch-free** module: `plain/0` reserves a live 2-slot frame, fills both Y
/// slots, builds a tuple and returns. No raise ever arms a latch, so every root
/// hand-off must be byte-identical to the pre-parking behaviour — the stack carries
/// exactly the frame's two Y slots.
fn plain_module() -> Fixtures {
    let mut fixture = Fixture::new("plain");
    let code = vec![
        Instr::Label(L_RUN),
        Instr::Allocate {
            stack_need: HeapNeed::Words(2),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(0x1234),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Integer(0x5678),
            dst: Operand::Y(1),
        },
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![Operand::Integer(1), Operand::Integer(2)],
        },
        Instr::Deallocate(2),
        Instr::Return,
    ];
    let label = fixture.add_function("plain", 0, code);
    let plain = fixture.export("plain", 0, label);
    Fixtures {
        module: fixture.finish(),
        run: plain,
        f: plain,
    }
}

/// The two Y-slot values [`plain_module`] stores, for the fast-path assertions.
const PLAIN_Y0: i64 = 0x1234;
const PLAIN_Y1: i64 = 0x5678;

/// A builder for a synthetic module with an owned atom table (the loader is not
/// involved; the engine is driven straight off the IR).
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Append a function; returns its entry label (the lowest-numbered label).
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module = self.module.name;
        let labels = labels_of(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    /// Export `name/arity` at `label`; returns the function-name atom.
    fn export(&mut self, name: &str, arity: u32, label: Label) -> Atom {
        let function = self.atom(name);
        self.module.exports.push(Export {
            function,
            arity,
            label,
        });
        function
    }

    fn finish(self) -> Module {
        self.module
    }
}

/// Map every `Instr::Label` in `code` to its index.
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// Run an export over the **caller-owned** context (the returned term lives in that
/// context's storage, so it must outlive the result) and hand the machine back so a
/// test can inspect its state after the run.
fn run(
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    machine.run_export_outcome(module, function, arity, args, ctx)
}

/// The `x(0)` of a completed run; a suspension here is a harness error (none of
/// these modules has a `receive`).
fn returned(outcome: RunOutcome) -> Eterm {
    match outcome {
        RunOutcome::Returned(value) => value,
        other => panic!("expected a returned value, got {other:?}"),
    }
}

/// `t`'s elements, in order (a test-only convenience over
/// [`terms::tuple_arity`]/[`terms::tuple_element`]).
fn tuple_elements(t: Eterm) -> Vec<Eterm> {
    let n = terms::tuple_arity(t).expect("a tuple");
    (0..n)
        .map(|i| terms::tuple_element(t, i).expect("tuple element"))
        .collect()
}

/// The `{M,F,A,Location}` items of a cooked stacktrace, innermost first.
fn items(cooked: Eterm) -> Vec<Eterm> {
    terms::list_elements(cooked).expect("the cooked stacktrace is a list")
}

/// Assert `cooked` is the canonical two-item trace of the raising module:
/// `[{mod, f, [{7,8,9}, 42], []}, {mod, run, 0, []}]`.
///
/// The head item is the **failed function** with the failed call's argument list in
/// its `A` slot (never the declared arity `2`), and the caller keeps its own arity
/// `0` — the shape the oracle prints for the same exception (cross-checked at the
/// bottom of this file).
fn assert_canonical_trace(cooked: Eterm, fixtures: &Fixtures) {
    let items = items(cooked);
    assert_eq!(items.len(), 2, "the failed function then its caller");

    let head = items[0];
    assert_eq!(terms::tuple_arity(head).expect("a 4-tuple"), 4);
    assert_eq!(
        terms::tuple_element(head, 0).expect("M"),
        make_atom(fixtures.module.name),
        "the items name the running module"
    );
    assert_eq!(
        terms::tuple_element(head, 1).expect("F"),
        make_atom(fixtures.f),
        "the failed clause's own function"
    );
    let a = terms::tuple_element(head, 2).expect("A");
    assert!(
        is_list(a),
        "the failed clause's A slot is the argument LIST, not the declared arity"
    );
    let args = terms::list_elements(a).expect("a proper argument list");
    assert_eq!(args.len(), 2, "f/2 was called with two arguments");
    assert_eq!(args[1], make_small(ARG_SECOND));
    let tuple = args[0];
    assert_eq!(terms::tuple_arity(tuple).expect("a 3-tuple"), 3);
    assert_eq!(
        tuple_elements(tuple),
        ARG_TUPLE.iter().map(|n| make_small(*n)).collect::<Vec<_>>(),
        "the first argument is the tuple run/0 built"
    );
    assert_eq!(
        terms::tuple_element(head, 3).expect("Location"),
        NIL,
        "the synthetic module has no Line markers"
    );

    let caller = items[1];
    assert_eq!(
        terms::tuple_element(caller, 1).expect("F"),
        make_atom(fixtures.run),
        "the caller frame"
    );
    assert_eq!(
        terms::tuple_element(caller, 2).expect("A"),
        make_small(0),
        "an outer frame keeps its declared arity"
    );
    assert_eq!(terms::tuple_element(caller, 3).expect("Location"), NIL);
}

// ---------------------------------------------------------------------------
// A recording, non-collecting host (the fast-path and repeat probes)
// ---------------------------------------------------------------------------

/// What a recording host observed, one entry per rooted reservation.
#[derive(Default)]
struct Seen {
    calls: usize,
    /// The whole Erlang stack handed over, per call.
    stack: Vec<Vec<Eterm>>,
}

/// Hand out `n` uninitialized words from `blocks`: each allocation is its own
/// `Box<[Eterm]>` and the `Vec` only ever grows (it stores the box handles, never
/// the words), so a pointer handed out earlier stays valid — the stability the
/// `alloc_words` contract requires. No term is ever relocated, so a term built
/// earlier stays readable: this is the *non-move* host.
fn serve(blocks: &mut Vec<Box<[Eterm]>>, n: usize) -> *mut Eterm {
    if n == 0 {
        // A well-aligned, non-null pointer that is never dereferenced (the engine's
        // builders always ask for at least 2).
        return core::ptr::NonNull::<Eterm>::dangling().as_ptr();
    }
    let mut block = vec![Eterm::default(); n].into_boxed_slice();
    let ptr = block.as_mut_ptr();
    blocks.push(block);
    ptr
}

/// A host whose heap is a stable arena and which records the root set it is handed
/// at every rooted reservation. It never moves a term, so a test can compare the
/// cooked trace across runs.
struct StackRecorder {
    seen: Rc<RefCell<Seen>>,
    blocks: Vec<Box<[Eterm]>>,
}

impl StackRecorder {
    fn new() -> (StackRecorder, Rc<RefCell<Seen>>) {
        let seen = Rc::new(RefCell::new(Seen::default()));
        (
            StackRecorder {
                seen: Rc::clone(&seen),
                blocks: Vec::new(),
            },
            seen,
        )
    }
}

impl ExecutionContext for StackRecorder {
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
            seen.stack.push(roots.stack.to_vec());
        }
        Ok(serve(&mut self.blocks, n))
    }
}

// ---------------------------------------------------------------------------
// The moving collector: a semi-space copying mock
// ---------------------------------------------------------------------------

/// Bytes per machine word (the unit the region bookkeeping is in).
const WORD_BYTES: usize = core::mem::size_of::<Eterm>();
/// Words per semi-space: the module builds a handful of small blocks, so two small
/// regions are ample.
const REGION_WORDS: usize = 1 << 12;
/// What a collection writes over the region it vacated, so a stale (pre-move)
/// pointer reads garbage instead of the data it used to address.
const POISON: Eterm = Eterm::from_raw(0xDEAD_BEEF_DEAD_BEEF);

/// One handed-out block: an `(offset, size)` window into the active region.
#[derive(Clone, Copy)]
struct Block {
    offset: usize,
    size: usize,
}

/// The address a term word refers to, if it is a list/boxed **pointer** (`None` for
/// every immediate: a small int, an atom, `NIL`, a header word).
fn pointer_addr(w: Eterm) -> Option<usize> {
    if is_list(w) || is_boxed_term(w) {
        Some(boxed_val(w) as usize)
    } else {
        None
    }
}

/// The index of the block of `blocks` (laid out at the absolute address `base`) that
/// the term word `w` points into.
fn block_at(blocks: &[Block], base: usize, w: Eterm) -> Option<usize> {
    let addr = pointer_addr(w)?;
    blocks.iter().position(|b| {
        let start = base + b.offset * WORD_BYTES;
        addr >= start && addr < start + b.size * WORD_BYTES
    })
}

/// Rebases an old absolute address onto its post-collection address using the
/// collection's relocation table — including an address that points into the
/// *middle* of a block (a node carved inside a bigger reservation).
fn relocate(
    moved: &[(Block, Block)],
    from_base: usize,
    to_base: usize,
    addr: usize,
) -> Option<usize> {
    for (old, new) in moved {
        let start = from_base + old.offset * WORD_BYTES;
        if addr >= start && addr < start + old.size * WORD_BYTES {
            return Some(to_base + new.offset * WORD_BYTES + (addr - start));
        }
    }
    None
}

/// [`relocate`] for a whole term word, preserving its pointer tag (list/boxed and
/// the literal-area bit) so a relocated cons cell stays a cons cell.
fn relocate_term(
    moved: &[(Block, Block)],
    from_base: usize,
    to_base: usize,
    w: Eterm,
) -> Option<Eterm> {
    let addr = pointer_addr(w)?;
    let new = relocate(moved, from_base, to_base, addr)?;
    Some(Eterm::from_raw(new | (w.raw() & TAG_PTR_MASK)))
}

/// A small **semi-space copying collector**: two fixed regions, a bump cursor in the
/// active one, and a collection on every rooted reservation. A collection marks the
/// blocks the roots point at, traces them transitively (conservatively: any list/boxed
/// word whose address lands in a known block), copies the live blocks — compacted — into
/// the **other** region building the old→new relocation table, rewrites the root slices
/// *in place* and the pointers *inside* the copied blocks, and finally poisons the
/// vacated region so a stale pointer reads garbage.
///
/// It also records the whole Erlang stack of every hand-off, so a test can see the
/// parked latch word the engine appends.
///
/// [`ExecutionContext::alloc_words`] is deliberately **not** overridden (a typed
/// `unsupported`), so a builder that fell back to the unrooted seam fails loudly
/// instead of skipping the collector.
struct MovingContext {
    /// The two region buffers — never read directly (they only keep the words alive;
    /// every access goes through the raw `base` pointers).
    _regions: [Box<[Eterm]>; 2],
    /// The stable raw base of each region (the boxed buffers never move).
    base: [*mut Eterm; 2],
    /// The region currently handed out of, and the words used in it.
    current: usize,
    used: usize,
    /// The blocks handed out in `regions[current]`, oldest first.
    blocks: Vec<Block>,
    /// How many collections have run, and whether one relocated at least one block.
    collections: usize,
    moved_any: bool,
    /// The whole Erlang stack handed over at each reservation, as seen *before* the
    /// collection of that reservation.
    stack_seen: Vec<Vec<Eterm>>,
    /// The whole X-register file handed over at each reservation, before the move.
    x_seen: Vec<Vec<Eterm>>,
}

impl MovingContext {
    /// A collector with two empty `REGION_WORDS`-word semi-spaces.
    fn new() -> MovingContext {
        let a = vec![Eterm::default(); REGION_WORDS].into_boxed_slice();
        let b = vec![Eterm::default(); REGION_WORDS].into_boxed_slice();
        let base = [a.as_ptr() as *mut Eterm, b.as_ptr() as *mut Eterm];
        MovingContext {
            _regions: [a, b],
            base,
            current: 0,
            used: 0,
            blocks: Vec::new(),
            collections: 0,
            moved_any: false,
            stack_seen: Vec::new(),
            x_seen: Vec::new(),
        }
    }

    /// Hand out `n` words from the active region's bump cursor and record the block.
    fn bump(&mut self, n: usize) -> Result<*mut Eterm> {
        if self.used + n > REGION_WORDS {
            return Err(beam_engine::error::unsupported(
                "moving collector: region exhausted",
            ));
        }
        let base = self.base[self.current];
        if n > 0 {
            self.blocks.push(Block {
                offset: self.used,
                size: n,
            });
        }
        // SAFETY: `used + n <= REGION_WORDS` words, so the `n` words starting at
        // `used` are inside the region; they are uninitialised for the caller under
        // the standard `alloc_words` stability contract (valid until the next
        // collecting allocation).
        let ptr = unsafe { base.add(self.used) };
        self.used += n;
        Ok(ptr)
    }

    /// Whether the block a list/boxed `term` points into lies in the region the
    /// collector hands out of right now — i.e. the term survived the last collection
    /// (a term left behind in the poisoned region would not).
    fn points_into_current_region(&self, term: Eterm) -> bool {
        match pointer_addr(term) {
            Some(addr) => {
                let base = self.base[self.current] as usize;
                addr >= base && addr < base + REGION_WORDS * WORD_BYTES
            }
            None => false,
        }
    }

    /// Relocate every live block into the other region and rewrite the roots.
    fn collect(&mut self, roots: RootSet<'_>) {
        let from = self.current;
        let to = 1 - from;
        let from_base = self.base[from];
        let to_base = self.base[to];
        let from_addr = from_base as usize;
        let to_addr = to_base as usize;
        let from_used = self.used;

        // (1) Mark the blocks a root points at.
        let mut live = vec![false; self.blocks.len()];
        for &w in roots
            .x
            .iter()
            .chain(roots.stack.iter())
            .chain(roots.fr.iter())
        {
            if let Some(i) = block_at(&self.blocks, from_addr, w) {
                live[i] = true;
            }
        }

        // (2) Transitive closure: a pointer inside a live block keeps its target live.
        let mut changed = true;
        while changed {
            changed = false;
            for i in 0..self.blocks.len() {
                if !live[i] {
                    continue;
                }
                let b = self.blocks[i];
                for k in 0..b.size {
                    // SAFETY: block `i` owns words `[offset, offset + size)` of the live
                    // `from` region and `k < size`, so the word is in bounds.
                    let w = unsafe { *from_base.add(b.offset + k) };
                    if let Some(j) = block_at(&self.blocks, from_addr, w) {
                        if !live[j] {
                            live[j] = true;
                            changed = true;
                        }
                    }
                }
            }
        }

        // (3) Copy the live blocks into `to`, compacted, building the relocation table.
        let mut new_blocks = Vec::new();
        let mut moved: Vec<(Block, Block)> = Vec::new();
        let mut cursor = 0usize;
        for (i, b) in self.blocks.iter().enumerate() {
            if !live[i] {
                continue;
            }
            let nb = Block {
                offset: cursor,
                size: b.size,
            };
            // SAFETY: source and destination are `size` live words in two *distinct*
            // region buffers (so they cannot overlap), and the destination prefix
            // `[0, cursor)` is not yet written.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    from_base.add(b.offset),
                    to_base.add(cursor),
                    b.size,
                );
            }
            new_blocks.push(nb);
            moved.push((*b, nb));
            cursor += b.size;
        }

        // (4) Rewrite the roots in place: they still hold the *old* addresses.
        for slot in roots
            .x
            .iter_mut()
            .chain(roots.stack.iter_mut())
            .chain(roots.fr.iter_mut())
        {
            if let Some(new) = relocate_term(&moved, from_addr, to_addr, *slot) {
                *slot = new;
            }
        }

        // (5) Rebase the pointers *inside* the copied blocks (a cons cell's CDR, a
        // tuple element pointing at another built block).
        for nb in &new_blocks {
            for k in 0..nb.size {
                // SAFETY: `nb` owns words `[offset, offset + size)` of the live `to`
                // region and `k < size`, so the word is in bounds.
                let w = unsafe { *to_base.add(nb.offset + k) };
                if let Some(new) = relocate_term(&moved, from_addr, to_addr, w) {
                    // SAFETY: as above — the word is in bounds and exclusively ours.
                    unsafe { *to_base.add(nb.offset + k) = new };
                }
            }
        }

        // (6) Poison the vacated region.
        for k in 0..from_used {
            // SAFETY: `k < from_used <= REGION_WORDS`, so the word is in bounds.
            unsafe { *from_base.add(k) = POISON };
        }

        // (7) Swap in the collected region.
        self.current = to;
        self.used = cursor;
        self.blocks = new_blocks;
        self.collections += 1;
        if cursor > 0 {
            self.moved_any = true;
        }
    }
}

impl ExecutionContext for MovingContext {
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
        // Record the hand-off *before* the move: the parked latch word the engine
        // appended is still at its pre-move address here.
        self.stack_seen.push(roots.stack.to_vec());
        self.x_seen.push(roots.x.to_vec());
        // Collect on *every* rooted reservation: the most aggressive schedule, so a
        // latch left unrooted is stranded on the very first cooked-trace reservation.
        self.collect(roots);
        self.bump(n)
    }
}

// ---------------------------------------------------------------------------
// (a) The parked latch survives a collection
// ---------------------------------------------------------------------------

/// The cooked trace's `A` slot is the failed call's argument list, and every term in
/// it — the tuple included — was **relocated** by the collector while the latch rode
/// the root set.
///
/// The tuple is reachable from *no* frozen root at the cooked-trace reservation: the
/// exception landing overwrote `x(0)`/`x(1)`, so the only path to it is
/// `raise_function_clause_args` → the argument list → the tuple. If
/// [`Machine::with_roots`] did not park that latch, the collector would leave both in
/// the region it then poisons, and the cooked trace would read garbage.
#[test]
fn a_moved_failed_clause_argument_list_survives_the_cooked_trace_reservation() {
    let fixtures = raising_module();
    let mut ctx = MovingContext::new();
    let mut machine = Machine::new();

    let outcome = run(
        &mut machine,
        &fixtures.module,
        fixtures.run,
        0,
        &[],
        &mut ctx,
    )
    .expect("the raise is caught by run/0's try and the handler cooks the trace");
    let cooked = returned(outcome);

    // The parked words are gone: the stack is restored to the frame's own slots
    // (popped by the handler's `deallocate 2`).
    assert_eq!(
        machine.stack_depth(),
        0,
        "the parked latch words must be truncated after the hand-off"
    );

    assert_canonical_trace(cooked, &fixtures);

    // POST-MOVE: every node of the cooked graph lives in the region the collector
    // hands out of now, so the whole trace was built from relocated terms.
    let head = items(cooked)[0];
    let a = terms::tuple_element(head, 2).expect("A");
    let tuple = terms::list_elements(a).expect("the argument list")[0];
    for (what, term) in [
        ("the cooked trace", cooked),
        ("the head item", head),
        ("the argument list", a),
        ("the argument tuple", tuple),
    ] {
        assert!(
            ctx.points_into_current_region(term),
            "{what} must live in the collected region, not the poisoned one"
        );
    }

    // ...and the collector really relocated a live block (not a no-op semi-space).
    assert!(ctx.moved_any, "no block was ever relocated");
    assert_eq!(
        ctx.collections, RAISER_RESERVES,
        "one collection per rooted reservation"
    );

    // The decisive witness: at the cooked-trace reservation the engine parked the
    // armed latch as one extra stack word, and that word's **pre-move** address lies
    // in the region the collection then vacated — while the cooked argument tuple
    // does not. So the engine re-seated the latch from the slot the host wrote the
    // relocated word back through.
    assert_eq!(ctx.stack_seen.len(), RAISER_RESERVES);
    assert_eq!(
        ctx.stack_seen[0].len(),
        2,
        "no latch is armed yet at the tuple build: the stack is the frame's two Y slots"
    );
    assert_eq!(
        ctx.stack_seen[1].len(),
        2,
        "the argument-list reservation runs before the latch is armed"
    );
    assert_eq!(
        ctx.stack_seen[2].len(),
        3,
        "the armed latch is parked as one extra stack word for the cooked-trace hand-off"
    );
    let pre_move_latch = *ctx.stack_seen[2].last().expect("the parked latch word");
    assert!(
        !ctx.points_into_current_region(pre_move_latch),
        "the latch's pre-move address is in the region the collection vacated — a \
         cooked trace that read it would be reading poison"
    );
    assert_ne!(
        pre_move_latch, a,
        "the cooked list is a fresh node, never the stranded pre-move latch"
    );
}

/// The **fast path**: a run with no armed latch hands over an *unmodified* root set.
///
/// The contrast is inside one assertion pair — the same live-frame shape, one extra
/// stack word **iff** a latch is armed — so the parking cannot fire on a latch-free
/// run and the pre-parking behaviour is byte-identical.
#[test]
fn the_latch_free_run_hands_over_an_unmodified_root_set() {
    let plain = plain_module();
    let (mut ctx, seen) = StackRecorder::new();
    let mut machine = Machine::new();
    let outcome = run(&mut machine, &plain.module, plain.run, 0, &[], &mut ctx)
        .expect("the latch-free module runs to completion");
    let _ = returned(outcome);

    let seen = seen.borrow();
    assert_eq!(seen.calls, PLAIN_RESERVES, "one put_tuple2, one reservation");
    assert_eq!(
        seen.stack[0],
        vec![make_small(PLAIN_Y0), make_small(PLAIN_Y1)],
        "the stack is exactly the frame's two Y slots — nothing is appended"
    );
    drop(seen);
    assert_eq!(machine.stack_depth(), 0, "the frame is popped by `deallocate 2`");

    // The contrast: the SAME frame shape on a run whose raise armed a latch hands
    // over one extra word at the cooked-trace reservation and none before it.
    let raiser = raising_module();
    let (mut ctx2, seen2) = StackRecorder::new();
    let mut machine2 = Machine::new();
    run(&mut machine2, &raiser.module, raiser.run, 0, &[], &mut ctx2)
        .expect("the raising module cooks its trace");
    let seen2 = seen2.borrow();
    assert_eq!(seen2.calls, RAISER_RESERVES);
    assert_eq!(
        seen2.stack.last().expect("the cooked-trace reservation").len(),
        3,
        "an armed latch adds exactly one parked word — the signal the fast path omits"
    );
}

// ---------------------------------------------------------------------------
// (c) A repeat run re-parks and restores the stack
// ---------------------------------------------------------------------------

/// Two runs on ONE machine: each re-parks the latch, cooks the same trace and leaves
/// the stack restored. A stale parked word (or a latch that leaked across a run)
/// would show up as a changed stack depth or a different trace on the second run.
#[test]
fn a_repeat_run_re_parks_the_latch_and_restores_the_stack() {
    let fixtures = raising_module();
    let (mut ctx, seen) = StackRecorder::new();
    let mut machine = Machine::new();

    let first = returned(
        run(&mut machine, &fixtures.module, fixtures.run, 0, &[], &mut ctx)
            .expect("the first run cooks its trace"),
    );
    assert_eq!(machine.stack_depth(), 0, "the first run restored the stack");
    assert_canonical_trace(first, &fixtures);

    let second = returned(
        run(&mut machine, &fixtures.module, fixtures.run, 0, &[], &mut ctx)
            .expect("the second run on the same machine cooks its trace"),
    );
    assert_eq!(machine.stack_depth(), 0, "the second run restored the stack");
    assert_canonical_trace(second, &fixtures);

    // Both runs parked exactly one word, and only at the cooked-trace hand-off.
    let seen = seen.borrow();
    assert_eq!(seen.calls, 2 * RAISER_RESERVES, "one reservation set per run");
    for base in [0, RAISER_RESERVES] {
        assert_eq!(seen.stack[base].len(), 2);
        assert_eq!(seen.stack[base + 1].len(), 2);
        assert_eq!(seen.stack[base + 2].len(), 3, "the parked latch");
    }
}

// ---------------------------------------------------------------------------
// (b) Same results: the moving collector == a plain arena host
// ---------------------------------------------------------------------------

/// The moving collector changes nothing observable: a plain non-collecting host
/// (`MinimalContext`) cooks the byte-identical shape for the same module, so the
/// relational evidence above is about *survival*, not about the collector inventing
/// a trace.
#[test]
fn the_moving_collector_cooks_the_same_trace_as_a_plain_host() {
    let fixtures = raising_module();

    let mut plain_ctx = beam_engine::MinimalContext::new();
    let mut plain_machine = Machine::new();
    let plain = returned(
        run(
            &mut plain_machine,
            &fixtures.module,
            fixtures.run,
            0,
            &[],
            &mut plain_ctx,
        )
        .expect("the plain arena host cooks the trace"),
    );
    assert_canonical_trace(plain, &fixtures);

    let mut moving_ctx = MovingContext::new();
    let mut moving_machine = Machine::new();
    let moved = returned(
        run(
            &mut moving_machine,
            &fixtures.module,
            fixtures.run,
            0,
            &[],
            &mut moving_ctx,
        )
        .expect("the moving host cooks the trace"),
    );
    assert_canonical_trace(moved, &fixtures);
}

// ---------------------------------------------------------------------------
// Oracle cross-check (real ERTS), skipped loudly without `erlc`/`erl`
// ---------------------------------------------------------------------------

/// Compile the `.erl` fixture below with the system `erlc` into a scratch directory
/// and hand back its path, or `None` when no runnable `erlc` is on `PATH` at all.
///
/// A spawned `erlc` that ran and exited non-zero is a **harness** failure and panics
/// with its stderr, so a broken toolchain can never masquerade as a silent skip.
fn compile_oracle_fixture() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("beam_engine_raise_rooting");
    std::fs::create_dir_all(&dir).ok()?;
    let source = dir.join("rr_oracle.erl");
    std::fs::write(
        &source,
        "-module(rr_oracle).\n\
         -export([run/0]).\n\
         run() -> try f({7, 8, 9}, 42) catch error:Reason:St -> Reason, St end.\n\
         f(A, B) when A =:= B -> ok.\n",
    )
    .ok()?;
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .current_dir(&dir)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erlc` exited {}\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(dir)
}

/// Run `expr` under the system `erl` with `dir` appended to the code path and return
/// its trimmed stdout, or `None` when no runnable `erl` is on `PATH`.
///
/// The code path is appended (`-pz`), never prepended (`-pa`): a scratch directory
/// could shadow the real stdlib otherwise.
fn erl_eval(dir: &Path, expr: &str) -> Option<String> {
    let output = Command::new("erl")
        .arg("-noshell")
        .arg("-pz")
        .arg(dir)
        .arg("-eval")
        .arg(expr)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {}\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The real ERTS cooks the same `A` slot for the same exception: the failed call's
/// **argument list** `[{7,8,9},42]`, never the declared arity `2`.
///
/// The engine's own structural assertions (`assert_canonical_trace`) pin exactly that
/// list, so this case anchors them to the oracle's own rendering. It **skips loudly**
/// when `erlc`/`erl` are absent.
#[test]
fn the_oracle_cooks_the_same_argument_list_in_the_a_slot() {
    let Some(dir) = compile_oracle_fixture() else {
        eprintln!("SKIP raise_rooting: no system `erlc` on PATH -- the oracle check is skipped");
        return;
    };
    let expr = "St = rr_oracle:run(), io:format(\"~p\", [element(3, hd(St))]), halt().";
    match erl_eval(&dir, expr) {
        Some(a_slot) => assert_eq!(
            a_slot, "[{7,8,9},42]",
            "the oracle's failed-clause A slot is the argument list"
        ),
        None => eprintln!(
            "SKIP raise_rooting: no system `erl` on PATH -- the oracle check is skipped"
        ),
    }
}
