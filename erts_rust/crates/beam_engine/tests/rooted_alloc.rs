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

//! **The term-construction instruction arms route their storage through the ROOTED
//! allocation seam.**
//!
//! [`ExecutionContext::alloc_words_rooted`] is the engine's admission point for
//! *term construction*: it hands the host the machine's whole live [`RootSet`] (the
//! X-register file, the whole Erlang stack, the whole FR file), so a host with a
//! **moving** collector may relocate every live term — and rewrite the root slices in
//! place — *before* it hands back the storage. The arms that build terms (`put_tuple2`
//! → `make_tuple_rooted`, `put_list` → `make_cons_rooted`, the map/bitstring/record
//! families) reach it through [`Machine::alloc_rooted`] and follow the frozen
//! **reserve → read → write** ordering: reserve the whole block first (exactly one
//! rooted allocation), then read every operand from the machine's *post-collection*
//! state, then write the header/element words.
//!
//! This suite proves that end to end, **from the public API only**: a hand-built
//! [`Module`] (no loader involved) is run through [`Machine::run_export_outcome`] over
//! three custom contexts.
//!
//! 1. [`ArenaContext`] counts every rooted reservation and serves storage from a stable
//!    context-owned arena, leaving `alloc_words` at its **trait default** (a typed
//!    `unsupported`) — a passing run *proves* the unrooted seam was never reached.
//! 2. [`UnrootedFallbackContext`] is the mirror image: its `alloc_words` would happily
//!    serve words, its rooted seam fails, and every construction arm must fail with
//!    *that* typed error — a fallback would have silently succeeded.
//! 3. [`MovingContext`] is a semi-space copying collector: each rooted reservation
//!    relocates every block reachable from the roots, rewrites the root slices in
//!    place, rebases the pointers *inside* the copied blocks (so a term built out of
//!    other built terms survives) and poisons the vacated region. A builder that read
//!    its operands *before* the reserve would write poisoned, pre-move addresses.
//!
//! Every failure is a typed [`BeamError`] — no test here panics on engine data.
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --test rooted_alloc
//! ```

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_engine::context::RootSet;
use beam_engine::machine::MAX_FLOAT_REGISTER;
use beam_engine::{error, terms, ExecutionContext, Machine, RunOutcome, MAX_REGISTER};
use beam_loader::{Export, Function, Instr, Label, Module, Operand};
use beam_term::{
    boxed_val, is_boxed_term, is_list, make_atom, make_small, Atom, Eterm, NIL, TAG_PTR_MASK,
};

// ---------------------------------------------------------------------------
// The synthetic module
// ---------------------------------------------------------------------------

/// Entry label of `ralloc_tuple/1` — one `put_tuple2`.
const L_TUPLE: Label = Label(1);
/// Entry label of `ralloc_list/2` — one `put_list`.
const L_LIST: Label = Label(10);
/// Entry label of `ralloc_build/3` — four construction instructions whose *later*
/// builds consume the *earlier* builds' results.
const L_BUILD: Label = Label(20);

/// Rooted reservations each export must perform. Every builder reserves its whole
/// block with exactly **one** rooted allocation (`make_tuple_rooted`/
/// `make_cons_rooted` → `Machine::alloc_rooted` → `alloc_words_rooted`), so the count
/// is one per term-construction instruction — nothing more, nothing less.
const TUPLE_RESERVES: usize = 1;
const LIST_RESERVES: usize = 1;
/// `ralloc_build/3` = `put_tuple2`; `put_list`; `put_tuple2`; `put_list`.
const BUILD_RESERVES: usize = 4;

/// The module under test, its three exports and the `ok` atom they build.
struct Fixtures {
    module: Module,
    tuple_fn: Atom,
    list_fn: Atom,
    build_fn: Atom,
    ok: Atom,
}

/// ```text
/// ralloc_tuple(A)      -> {A, ok, 7}          % put_tuple2          (1 reserve)
/// ralloc_list(H, T)    -> [H | T]             % put_list            (1 reserve)
/// ralloc_build(A,B,C)  -> [[{A, B, ok}, [C]]] % 4 instructions      (4 reserves)
/// ```
///
/// `ralloc_build/3` is the interesting shape: each later reserve is handed a live heap
/// term built by an earlier instruction (`{A, B, ok}` in `X(3)`, a cons cell in `X(4)`,
/// the pair in `X(5)`), so the last collection must keep those nodes alive *through*
/// the pair that points at them (transitive marking + internal-pointer rebasing).
fn fixtures() -> Fixtures {
    let mut fixture = Fixture::new();
    let ok = fixture.atom("ok");

    let tuple_code = vec![
        Instr::Label(L_TUPLE),
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![Operand::X(0), Operand::Atom(ok), Operand::Integer(7)],
        },
        Instr::Return,
    ];
    let list_code = vec![
        Instr::Label(L_LIST),
        Instr::PutList {
            head: Operand::X(0),
            tail: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let build_code = vec![
        Instr::Label(L_BUILD),
        Instr::PutTuple2 {
            dst: Operand::X(3),
            elements: vec![Operand::X(0), Operand::X(1), Operand::Atom(ok)],
        },
        Instr::PutList {
            head: Operand::X(2),
            tail: Operand::Nil,
            dst: Operand::X(4),
        },
        Instr::PutTuple2 {
            dst: Operand::X(5),
            elements: vec![Operand::X(3), Operand::X(4)],
        },
        Instr::PutList {
            head: Operand::X(5),
            tail: Operand::Nil,
            dst: Operand::X(0),
        },
        Instr::Return,
    ];

    let tuple_label = fixture.add_function("ralloc_tuple", 1, tuple_code);
    let list_label = fixture.add_function("ralloc_list", 2, list_code);
    let build_label = fixture.add_function("ralloc_build", 3, build_code);
    let tuple_fn = fixture.export("ralloc_tuple", 1, tuple_label);
    let list_fn = fixture.export("ralloc_list", 2, list_label);
    let build_fn = fixture.export("ralloc_build", 3, build_label);
    Fixtures {
        module: fixture.finish(),
        tuple_fn,
        list_fn,
        build_fn,
        ok,
    }
}

/// A builder for a synthetic module with an owned atom table (the loader is not
/// involved; the engine is driven straight off the IR).
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new() -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("ralloc");
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

/// Run an export on a fresh [`Machine`] over the **caller-owned** context (the
/// returned term lives in that context's storage, so it must outlive the result).
fn run_export(
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    let mut machine = Machine::new();
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

// ---------------------------------------------------------------------------
// (1) The counting context: the heap is reachable ONLY through the rooted seam
// ---------------------------------------------------------------------------

/// Hand out `n` uninitialized words from `blocks`: each allocation is its own
/// `Box<[Eterm]>` and the `Vec` only ever grows (it stores the box handles, never the
/// words), so a pointer handed out earlier stays valid — the stability the
/// `alloc_words` contract requires.
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

/// What [`ArenaContext`] observed, one entry per rooted reservation.
#[derive(Default)]
struct RootedCalls {
    calls: usize,
    /// The whole X-register file / Erlang stack / FR file handed over, per call.
    x: Vec<Vec<Eterm>>,
    stack: Vec<Vec<Eterm>>,
    fr: Vec<Vec<Eterm>>,
}

/// A counting, arena-backed context whose **only** heap door is the rooted seam:
/// `alloc_words` stays the trait default (a typed `unsupported`), so an arm that fell
/// back to the unrooted seam fails the run loudly instead of allocating outside the
/// collector's view.
struct ArenaContext {
    seen: Rc<RefCell<RootedCalls>>,
    blocks: Vec<Box<[Eterm]>>,
}

impl ArenaContext {
    fn new() -> (ArenaContext, Rc<RefCell<RootedCalls>>) {
        let seen = Rc::new(RefCell::new(RootedCalls::default()));
        let ctx = ArenaContext {
            seen: Rc::clone(&seen),
            blocks: Vec::new(),
        };
        (ctx, seen)
    }
}

impl ExecutionContext for ArenaContext {
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
        Ok(serve(&mut self.blocks, n))
    }
}

// ---------------------------------------------------------------------------
// (2) The mirrored control: a live unrooted seam, a broken rooted one
// ---------------------------------------------------------------------------

/// What the failing rooted seam reports. The full reason atom is
/// `unsupported_opcode: <this>`; the test derives the atom from the constructor itself
/// ([`rooted_seam_reason`]) so the two cannot drift.
const ROOTED_SEAM_WHAT: &str = "rooted-seam-disabled (test control)";

/// A context whose `alloc_words` **would** serve words (so a fallback would succeed
/// silently) but whose rooted seam always fails — the root-cause control: an arm that
/// went through `alloc_words` would return a perfectly good term, so a run failing with
/// [`ROOTED_SEAM_WHAT`] proves the arm used the rooted seam and nothing else.
struct UnrootedFallbackContext {
    blocks: Vec<Box<[Eterm]>>,
}

impl UnrootedFallbackContext {
    fn new() -> UnrootedFallbackContext {
        UnrootedFallbackContext { blocks: Vec::new() }
    }
}

impl ExecutionContext for UnrootedFallbackContext {
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

    /// A **working** unrooted seam — the trap.
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        Ok(serve(&mut self.blocks, n))
    }

    /// The rooted seam is broken, so an arm that uses it cannot build anything.
    fn alloc_words_rooted(&mut self, _n: usize, _roots: RootSet<'_>) -> Result<*mut Eterm> {
        Err(error::unsupported(ROOTED_SEAM_WHAT))
    }
}

/// The reason atom `error::unsupported(ROOTED_SEAM_WHAT)` carries, taken from the
/// constructor itself rather than re-spelled here.
fn rooted_seam_reason() -> Option<String> {
    error::unsupported(ROOTED_SEAM_WHAT)
        .into_raise()
        .and_then(|raise| raise.reason.as_atom().map(str::to_owned))
}

// ---------------------------------------------------------------------------
// (3) The moving collector: a semi-space copying mock
// ---------------------------------------------------------------------------

/// Bytes per machine word (the unit the region bookkeeping is in).
const WORD_BYTES: usize = core::mem::size_of::<Eterm>();
/// Words per semi-space: the module builds a handful of 2-4-word blocks, so two small
/// regions are ample.
const REGION_WORDS: usize = 1 << 12;
/// What a collection writes over the region it vacated, so a stale (pre-move) pointer
/// reads garbage instead of the data it used to address.
const POISON: Eterm = Eterm::from_raw(0xDEAD_BEEF_DEAD_BEEF);

/// One handed-out block: an `(offset, size)` window into the active region.
#[derive(Clone, Copy)]
struct Block {
    offset: usize,
    size: usize,
}

/// The address a term word refers to, if it is a list/boxed **pointer** (`None` for
/// every immediate: a small int, an atom, `NIL`, a header word). The predicates are
/// `beam_term`'s own tag tests; the caller must still confirm the address lies inside a
/// known region (see [`block_at`]), which keeps a word that merely *looks* like a
/// pointer from being treated as one.
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
/// *in place* and the pointers *inside* the copied blocks (a whole block moves as a unit,
/// so its internal absolute addresses must be rebased too), and finally poisons the
/// vacated region so a stale pointer reads garbage.
///
/// [`ExecutionContext::alloc_words`] is deliberately **not** overridden (a typed
/// `unsupported`), so a builder that fell back to the unrooted seam fails loudly instead
/// of skipping the collector.
struct MovingContext {
    /// The two region buffers — never read directly (they only keep the words alive;
    /// every access goes through the raw `base` pointers).
    _regions: [Box<[Eterm]>; 2],
    /// The stable raw base of each region (the boxed buffers never move).
    base: [*mut Eterm; 2],
    /// The region currently handed out of, and the words used in it.
    current: usize,
    used: usize,
    /// The blocks handed out in `regions[current]`, oldest first (contiguous, since a
    /// collection compacts and every allocation appends).
    blocks: Vec<Block>,
    /// How many collections have run, and whether one relocated at least one block.
    collections: usize,
    moved_any: bool,
    /// `x(3)` — the live tuple operand — as seen at the *start* of each collection
    /// (i.e. from a real machine register, before the move).
    x3_seen: Vec<Eterm>,
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
            x3_seen: Vec::new(),
        }
    }

    /// Hand out `n` words from the active region's bump cursor and record the block.
    fn bump(&mut self, n: usize) -> Result<*mut Eterm> {
        if self.used + n > REGION_WORDS {
            return Err(error::unsupported("moving collector: region exhausted"));
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
        // Record a real machine register *before* the move, so the test can show the
        // live operand was relocated (not merely that the terms happen to read right).
        self.x3_seen.push(roots.x.get(3).copied().unwrap_or(NIL));
        // Collect on *every* rooted reservation: the most aggressive schedule, so a
        // reserve-before-read bug is exercised on the very first build.
        self.collect(roots);
        self.bump(n)
    }
}

// ---------------------------------------------------------------------------
// The built graph, shared by the two `ralloc_build/3` tests
// ---------------------------------------------------------------------------

/// The live blocks a `ralloc_build/3` run leaves behind: `{A, B, ok}` (the first
/// instruction's result), `[C]` (the second's) and the pair of the two (the third's).
struct Built {
    inner_tuple: Eterm,
    inner_list: Eterm,
    pair: Eterm,
}

/// `t`'s elements, in order (a test-only convenience over
/// [`terms::tuple_arity`]/[`terms::tuple_element`]).
fn tuple_elements(t: Eterm) -> Vec<Eterm> {
    let n = terms::tuple_arity(t).expect("a tuple");
    (0..n)
        .map(|i| terms::tuple_element(t, i).expect("tuple element"))
        .collect()
}

/// Assert that a `ralloc_build(1, 2, 3)` run produced the whole graph
/// `[[{1, 2, ok}, [3]]]`, and return its blocks. Every node is checked, not just the
/// outermost list: each is a term an earlier instruction built and a later instruction
/// consumed, so a single stale address (a pre-move pointer, say) surfaces here.
fn assert_built(result: Eterm, ok: Atom) -> Built {
    let pair = terms::cons_head(result).expect("the built pair");
    let inner_tuple = terms::tuple_element(pair, 0).expect("elem 0");
    let inner_list = terms::tuple_element(pair, 1).expect("elem 1");
    assert_eq!(
        terms::list_elements(result).expect("outer list"),
        vec![pair],
        "the run returns a one-element list wrapping the pair"
    );
    assert_eq!(terms::tuple_arity(pair).expect("a 2-tuple"), 2);
    assert_eq!(
        tuple_elements(inner_tuple),
        vec![make_small(1), make_small(2), make_atom(ok)]
    );
    assert_eq!(
        terms::list_elements(inner_list).expect("a list"),
        vec![make_small(3)]
    );
    Built {
        inner_tuple,
        inner_list,
        pair,
    }
}

// ---------------------------------------------------------------------------
// (1) The counting proofs
// ---------------------------------------------------------------------------

#[test]
fn the_tuple_arm_reserves_once_through_the_rooted_seam() {
    let f = fixtures();
    let (mut ctx, seen) = ArenaContext::new();

    // The negative control, stated positively: `alloc_words` is the trait default, so
    // it is a typed error — any fallback would fail the run below.
    assert!(
        ctx.alloc_words(4).is_err(),
        "`alloc_words` must stay the trait default (a typed error), not a second heap"
    );

    let result = returned(
        run_export(&f.module, f.tuple_fn, 1, &[make_small(42)], &mut ctx)
            .expect("put_tuple2 must build through the rooted seam, never through `alloc_words`"),
    );

    // {42, ok, 7} — the term is correct for this run's arguments.
    assert_eq!(
        tuple_elements(result),
        vec![make_small(42), make_atom(f.ok), make_small(7)]
    );

    let seen = seen.borrow();
    // One `put_tuple2` = exactly one rooted reservation. The whole X-register file and
    // the whole FR file (a float register holds a term slot) are handed over, and the
    // argument the build reads is a live root at the reserve.
    assert_eq!(seen.calls, TUPLE_RESERVES);
    assert_eq!(seen.x[0].len(), MAX_REGISTER as usize);
    assert_eq!(seen.fr[0].len(), MAX_FLOAT_REGISTER as usize);
    assert_eq!(seen.x[0][0], make_small(42));
}

#[test]
fn the_list_arm_reserves_once_through_the_rooted_seam() {
    let f = fixtures();
    let (mut ctx, seen) = ArenaContext::new();

    // [8 | 9]: two operands, so the builder reserves before reading both of them.
    let result = returned(
        run_export(
            &f.module,
            f.list_fn,
            2,
            &[make_small(8), make_small(9)],
            &mut ctx,
        )
        .expect("put_list must build through the rooted seam"),
    );
    assert_eq!(terms::cons_head(result).expect("head"), make_small(8));
    assert_eq!(terms::cons_tail(result).expect("tail"), make_small(9));

    let seen = seen.borrow();
    // One `put_list` = exactly one rooted reservation (one cons cell), with no frame
    // live in a leaf run and both operands live in the handed-over X file.
    assert_eq!(seen.calls, LIST_RESERVES);
    assert_eq!(seen.stack[0].len(), 0);
    assert_eq!(seen.x[0].len(), MAX_REGISTER as usize);
    assert_eq!(seen.fr[0].len(), MAX_FLOAT_REGISTER as usize);
    assert_eq!(seen.x[0][0], make_small(8));
    assert_eq!(seen.x[0][1], make_small(9));
}

#[test]
fn each_build_instruction_reserves_exactly_once_and_hands_over_the_live_operands() {
    let f = fixtures();
    let (mut ctx, seen) = ArenaContext::new();

    let result = returned(
        run_export(
            &f.module,
            f.build_fn,
            3,
            &[make_small(1), make_small(2), make_small(3)],
            &mut ctx,
        )
        .expect("the four builders must all reach the rooted seam"),
    );
    let Built {
        inner_tuple,
        inner_list,
        pair,
    } = assert_built(result, f.ok);

    let seen = seen.borrow();
    // Four term-construction instructions = exactly four rooted reservations. The root
    // sets are the machine's own, and the *terms built by earlier instructions* really
    // are among them: the `put_list` reserve is handed the tuple the previous
    // `put_tuple2` built (X(3)), the third reserve still sees the live tuple in X(3) and
    // the live cons cell in X(4), and the fourth is handed the pair built into X(5).
    assert_eq!(seen.calls, BUILD_RESERVES);
    assert_eq!(seen.x[0][3], NIL);
    assert_eq!(seen.x[1][3], inner_tuple);
    assert_eq!(seen.x[2][3], inner_tuple);
    assert_eq!(seen.x[2][4], inner_list);
    assert_eq!(seen.x[3][5], pair);
}

// ---------------------------------------------------------------------------
// (2) The mirrored control: the rooted seam is the ONLY seam the arms use
// ---------------------------------------------------------------------------

#[test]
fn every_construction_arm_fails_when_only_the_rooted_seam_is_broken() {
    let f = fixtures();
    let mut ctx = UnrootedFallbackContext::new();

    // The trap is genuine: the unrooted seam really would have served storage, so a
    // fallback would have produced a perfectly good term (and no failure at all).
    assert!(
        ctx.alloc_words(8).is_ok(),
        "the control's unrooted seam must be live, otherwise this test proves nothing"
    );

    let cases: [(&str, Atom, u32, Vec<Eterm>); 3] = [
        ("ralloc_tuple/1", f.tuple_fn, 1, vec![make_small(1)]),
        (
            "ralloc_list/2",
            f.list_fn,
            2,
            vec![make_small(1), make_small(2)],
        ),
        (
            "ralloc_build/3",
            f.build_fn,
            3,
            vec![make_small(1), make_small(2), make_small(3)],
        ),
    ];
    let expected = rooted_seam_reason();
    for (name, function, arity, args) in cases {
        let err = match run_export(&f.module, function, arity, &args, &mut ctx) {
            Err(err) => err,
            Ok(outcome) => panic!(
                "{name} succeeded ({outcome:?}): it allocated through `alloc_words` \
                 instead of the rooted seam"
            ),
        };
        let raise = match err.into_raise() {
            Some(raise) => raise,
            None => panic!("{name}: expected a raise"),
        };
        assert_eq!(
            raise.reason.as_atom().map(str::to_owned),
            expected,
            "{name} must fail with the rooted seam's own error, never silently fall \
             back to `alloc_words`"
        );
    }
}

// ---------------------------------------------------------------------------
// (3) The moving-collector proof
// ---------------------------------------------------------------------------

#[test]
fn a_moving_collector_relocates_the_live_operands_and_the_terms_stay_correct() {
    let f = fixtures();
    let mut ctx = MovingContext::new();

    let result = returned(
        run_export(
            &f.module,
            f.build_fn,
            3,
            &[make_small(1), make_small(2), make_small(3)],
            &mut ctx,
        )
        .expect("a reserve-before-read builder survives every relocation"),
    );

    // The term is correct *after* the operands were relocated underneath it: the
    // fourth instruction read X(3)/X(4)/X(5) from the post-collection machine state,
    // and the collection rebased the pair's own element pointers (transitive
    // marking: {A,B,ok} and [C] survive only *through* the pair that points at them).
    let Built {
        inner_tuple,
        inner_list,
        pair,
    } = assert_built(result, f.ok);

    // ...and every live term now sits in the region the collector hands out of, i.e.
    // it was really moved (the pre-move copies were poisoned).
    for (what, term) in [
        ("the returned list", result),
        ("the pair", pair),
        ("the inner tuple", inner_tuple),
        ("the inner list", inner_list),
    ] {
        assert!(
            ctx.points_into_current_region(term),
            "{what} must live in the collected region"
        );
    }

    // One collection per rooted reservation, and a collection really did relocate a
    // live block.
    assert_eq!(
        ctx.collections, BUILD_RESERVES,
        "the collector ran once per rooted reservation"
    );
    assert!(ctx.moved_any, "no block was ever relocated");

    // The witness that the *machine registers* were relocated: X(3) — the live tuple
    // the later builders read as an operand — is a real heap term at the second and
    // third collections, at two *different* addresses. The two semi-spaces alternate,
    // so the address the second collection saw is in the space the collector has since
    // vacated, and the address the third collection saw is in the live one.
    assert_eq!(ctx.x3_seen.len(), BUILD_RESERVES);
    assert_eq!(
        ctx.x3_seen[0], NIL,
        "nothing is built yet at the first reserve"
    );
    assert!(
        !ctx.points_into_current_region(ctx.x3_seen[1]),
        "the second reserve is handed the tuple at its first (pre-move) address"
    );
    assert_ne!(
        ctx.x3_seen[1], ctx.x3_seen[2],
        "the live tuple operand changed address: the collector really moved it"
    );
    assert!(
        ctx.points_into_current_region(ctx.x3_seen[2]),
        "the third reserve is handed the relocated tuple in the live region"
    );
}
