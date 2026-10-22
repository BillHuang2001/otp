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

//! **Every term `call_fun`/`apply_target` reads before a host seam is re-derived from
//! the machine afterwards.**
//!
//! [`ExecutionContext::resolve_local_fun`], [`ExecutionContext::apply_fun_outcome`],
//! [`ExecutionContext::resolve_external_call`] and the BIF door are ordinary host
//! calls (`&mut dyn ExecutionContext`). A host is free to allocate through a rooted
//! door while serving one — and a **collecting** host relocates exactly the frozen
//! [`RootSet`] (`x[..live]` + the whole Erlang stack + the FR file), which is where
//! the closure, its captures and the call's arguments live. Every `Eterm` a Rust
//! local still holds after such a call therefore names the region the collector
//! vacated.
//!
//! The two sites this suite pins:
//!
//! * `families::funs::call_fun` — the closure (`x(arity)`/the `call_fun2` operand) and
//!   the `register_args` copy were held across `resolve_local_fun`, and the closure's
//!   **free-variable block** was then dereferenced (`terms::fun_free`) at the stale
//!   address; the `Ok(None)` fallback additionally handed the stale pair to
//!   `apply_fun_outcome`.
//! * `families::funs::apply_target` — the `register_args` copy was built *before*
//!   `resolve_external_call`, so the BIF dispatch that follows a decline received an
//!   argument list naming the vacated region.
//!
//! The invariant proven here: each of those terms is read from its **re-readable
//! source** (`FunSource::read` / the machine's registers) *after* the seam, so what
//! the engine publishes into `x(arity..)` — and what the fallback is handed — is
//! always the **moved** term.
//!
//! # The harness
//!
//! The collector is a semi-space copying mock ([`MovingContext`], the same shape
//! `tests/rooted_alloc.rs` / `tests/raise_rooting.rs` use): it collects on every
//! rooted reservation, copies the live blocks into the other region building an
//! old→new relocation table, rewrites the root slices **in place**, rebases the
//! pointers inside the copied blocks and poisons the region it vacated — so a stale
//! read yields garbage rather than the data it used to address.
//!
//! One instrument is unusual and deliberate: the seam methods take **no** `RootSet`
//! (the engine hands roots only to `alloc_words_rooted`/`ensure_heap_full`), so a test
//! that must place a collection *inside* the seam window reaches the machine the run
//! is driving through the raw handle [`MovingContext::attach`] stashes, and collects
//! through [`Machine::with_roots`] — the very root set the engine hands a rooted door.
//! That is a test-only stand-in for a host that allocates in a seam; the production
//! hosts (`beam_rt`) are documented as non-collecting there.
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --test fun_seam_rooting
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_engine::context::RootSet;
use beam_engine::{error, terms, ExecutionContext, FunOutcome, Machine, RunOutcome};
use beam_loader::{Export, FunDescriptor, Function, HeapNeed, Instr, Label, Module, Operand};
use beam_term::{
    boxed_val, is_boxed_term, is_list, make_small, Atom, Eterm, TAG_PTR_MASK,
};

// ---------------------------------------------------------------------------
// A synthetic module builder (no loader involved: the engine runs the typed IR)
// ---------------------------------------------------------------------------

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

/// The `FunT` identity every closure below carries (opaque to the seam: the host
/// answers from its own registry, and the test reads it back through
/// `terms::fun_uniq`).
const LAMBDA_UNIQ: u32 = 0xCAFE;

/// A builder for one synthetic module with its own atom table.
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

    /// Register a `FunT` descriptor; its position is the fun-table index. `arity`
    /// is the lambda's **total** arity and `num_free` the captured count, so the
    /// closure's own arity is `arity - num_free`.
    fn add_fun(&mut self, name: &str, arity: u32, num_free: u32, uniq: u32) -> u32 {
        let function = self.atom(name);
        let index = self.module.funs.len() as u32;
        self.module.funs.push(FunDescriptor {
            function,
            arity,
            label: Label(index + 1),
            index,
            num_free,
            uniq,
        });
        index
    }

    /// Append a function; its entry label is the first label in `code`.
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

    fn add_export(&mut self, name: &str, arity: u32, label: Label) {
        let function = self.atom(name);
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
// The callers
// ---------------------------------------------------------------------------

/// The heap tuple every fixture builds, so a relocation is observable in the value.
const HEAP_TUPLE: [i64; 3] = [1, 2, 3];
/// The tuple a *captured* free variable holds, distinct from [`HEAP_TUPLE`] so a
/// swapped or stale capture can never pass by coincidence.
const CAPTURED_TUPLE: [i64; 3] = [7, 8, 9];

/// `m:run/0` — `call_fun 1` over a closure that captures a **heap tuple**:
///
/// ```text
/// run/0:
///     allocate 0 3
///     put_tuple2 x2 {7,8,9}       ; the captured value (a heap term)
///     make_fun3 <funT/1+1> x1 x2  ; F = fun(A) -> … end, capturing x2
///     move 10 -> x0               ; the call argument (the oracle's `A`)
///     call_fun 1                  ; F(10)
///     deallocate 0
///     return
/// ```
fn caller_with_a_captured_tuple() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let index = f.add_fun("lambda", 2, 1, LAMBDA_UNIQ);
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 3,
            },
            Instr::PutTuple2 {
                dst: Operand::X(2),
                elements: vec![
                    Operand::Integer(CAPTURED_TUPLE[0]),
                    Operand::Integer(CAPTURED_TUPLE[1]),
                    Operand::Integer(CAPTURED_TUPLE[2]),
                ],
            },
            Instr::MakeFun3 {
                index,
                dst: Operand::X(1),
                env: vec![Operand::X(2)],
            },
            Instr::Move {
                src: Operand::Integer(10),
                dst: Operand::X(0),
            },
            Instr::CallFun { arity: 1 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// `m:run/0` — `call_fun 1` over a closure with **no** captures, called with a heap
/// tuple, so the call's *argument array* (`register_args`) is the term a seam can
/// strand:
///
/// ```text
/// run/0:
///     allocate 0 2
///     put_tuple2 x0 {1,2,3}       ; the call argument (a heap term)
///     make_fun3 <funT/1+0> x1     ; F = fun(A) -> … end
///     call_fun 1                  ; F({1,2,3})
///     deallocate 0
///     return
/// ```
fn caller_calling_with_a_heap_argument() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let index = f.add_fun("lambda", 1, 0, LAMBDA_UNIQ);
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 2,
            },
            Instr::PutTuple2 {
                dst: Operand::X(0),
                elements: vec![
                    Operand::Integer(HEAP_TUPLE[0]),
                    Operand::Integer(HEAP_TUPLE[1]),
                    Operand::Integer(HEAP_TUPLE[2]),
                ],
            },
            Instr::MakeFun3 {
                index,
                dst: Operand::X(1),
                env: vec![],
            },
            Instr::CallFun { arity: 1 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// `m:run/0` — `apply m:echo({1,2,3})`, i.e. the `fixed_apply` shape:
///
/// ```text
/// run/0:
///     allocate 0 3
///     put_tuple2 x0 {1,2,3}       ; the call argument (a heap term)
///     move m    -> x1             ; the module  (`apply 1` reads x(arity))
///     move echo -> x2             ; the function (`x(arity+1)`)
///     apply 1
///     deallocate 0
///     return
/// ```
fn apply_caller() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let echo = f.atom("echo");
    let module_atom = f.module.name;
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 3,
            },
            Instr::PutTuple2 {
                dst: Operand::X(0),
                elements: vec![
                    Operand::Integer(HEAP_TUPLE[0]),
                    Operand::Integer(HEAP_TUPLE[1]),
                    Operand::Integer(HEAP_TUPLE[2]),
                ],
            },
            Instr::Move {
                src: Operand::Atom(module_atom),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Atom(echo),
                dst: Operand::X(2),
            },
            Instr::Apply { arity: 1 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// The lambda body `call_fun`'s in-machine route enters: the seam places the call
/// argument at `x(0)` and the capture at `x(1)`, so the body returns the **capture**:
///
/// ```text
/// lambda/2:
///     move x1 -> x0    ; the captured free variable
///     return
/// ```
fn lambda_module() -> (Arc<Module>, Label) {
    let mut f = Fixture::new("c");
    let lambda = f.add_function(
        "lambda",
        2,
        vec![
            Instr::Label(Label(20)),
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    f.add_export("lambda", 2, lambda);
    (Arc::new(f.finish()), lambda)
}

// ---------------------------------------------------------------------------
// The moving collector: a semi-space copying mock
// ---------------------------------------------------------------------------

/// Bytes per machine word.
const WORD_BYTES: usize = core::mem::size_of::<Eterm>();
/// Words per semi-space. The fixtures build a handful of 2-4-word blocks, so two
/// small regions are ample — and large enough that even a wild read (a poisoned
/// header's bogus `num_free`) stays inside an allocated buffer.
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
/// `w` points into.
fn block_at(blocks: &[Block], base: usize, w: Eterm) -> Option<usize> {
    let addr = pointer_addr(w)?;
    blocks.iter().position(|b| {
        let start = base + b.offset * WORD_BYTES;
        addr >= start && addr < start + b.size * WORD_BYTES
    })
}

/// Rebases an old absolute address onto its post-collection address using the
/// collection's relocation table — including an address that points into the *middle*
/// of a block (a node carved inside a bigger reservation).
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

/// [`relocate`] for a whole term word, preserving its pointer tag (list/boxed and the
/// literal-area bit) so a relocated cons cell stays a cons cell.
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

/// What the host's seams do in each test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// `resolve_local_fun` collects and answers `Some((callee, lambda))`.
    ResolveLocal,
    /// `resolve_local_fun` collects and declines; `apply_fun_outcome` answers with the
    /// closure's own `uniq` — a read of the **closure** word.
    DeclineEchoUniq,
    /// `resolve_local_fun` collects and declines; `apply_fun_outcome` answers `args[0]`
    /// — a read of the **argument array**.
    DeclineEchoArg,
    /// `resolve_external_call` collects and declines; `call_bif` answers `args[0]`.
    ExternalEchoArg,
}

/// A small **semi-space copying collector** over two fixed regions: a bump cursor in
/// the active one, and a collection on every rooted reservation (and, for this suite,
/// inside the seam the engine calls without a root set).
///
/// [`ExecutionContext::alloc_words`] is deliberately **not** overridden (the trait
/// default is a typed `unsupported`), so an arm that fell back to the unrooted seam
/// fails loudly instead of skipping the collector.
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
    /// How many collections have run, and how many relocated at least one block.
    collections: usize,
    moved_any: bool,
    /// The machine this run is driving — the test-only way to reach the live root set
    /// from inside a seam that is handed none.
    machine: Option<*mut Machine>,
    mode: Mode,
    /// The module + lambda label `Mode::ResolveLocal` answers with.
    callee: Option<(Arc<Module>, Label)>,
    /// How many seam consultations ran (so a test can assert the seam was reached).
    seam_calls: usize,
}

impl MovingContext {
    /// A collector with two empty `REGION_WORDS`-word semi-spaces.
    fn new(mode: Mode) -> MovingContext {
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
            machine: None,
            mode,
            callee: None,
            seam_calls: 0,
        }
    }

    /// The in-machine route's callee: the lambda module and its entry label.
    fn resolving(mut self, callee: Arc<Module>, lambda: Label) -> MovingContext {
        self.callee = Some((callee, lambda));
        self
    }

    /// Point the collector at the machine a run is about to drive, so a seam can
    /// collect over the **live** root set.
    ///
    /// # Safety note (test harness only)
    ///
    /// The handle aliases the `&mut Machine` the run drives. It is re-borrowed only
    /// from inside a seam the engine calls *through* that machine (so the machine is
    /// exclusively this run's for the duration of the call), and it is the only way a
    /// test can place a collection where a host implementation would place one — the
    /// seam signatures carry no `RootSet`.
    fn attach(&mut self, machine: &mut Machine) {
        self.machine = Some(machine as *mut Machine);
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

    /// Whether a list/boxed `term` points into the region the collector hands out of
    /// right now — i.e. whether the term survived the last collection (a term left
    /// behind in the poisoned region does not).
    fn points_into_current_region(&self, term: Eterm) -> bool {
        match pointer_addr(term) {
            Some(addr) => {
                let base = self.base[self.current] as usize;
                addr >= base && addr < base + REGION_WORDS * WORD_BYTES
            }
            None => false,
        }
    }

    /// Run a collection **now**, over the machine's live root set.
    ///
    /// This is the seam-window instrument: the engine hands no `RootSet` to the fun
    /// family's host seams, so the collection is driven through
    /// [`Machine::with_roots`] — the identical hand-off a rooted door performs.
    fn collect_now(&mut self) {
        let Some(handle) = self.machine else {
            return;
        };
        // SAFETY: `handle` names the machine the run is driving (see `attach`); the
        // engine is inside one of that machine's own host seams, so the machine is
        // live and this run's. The collector only reads the handed root slices for
        // marking and writes the relocated words back in place — exactly the contract
        // `alloc_words_rooted` states for a collecting host.
        let machine = unsafe { &mut *handle };
        machine.with_roots(RootSet::ALL_LIVE, |roots| self.collect(roots));
        let _ = self.bump(1);
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

        // (5) Rebase the pointers *inside* the copied blocks (a closure's captured
        // free variable pointing at another built block).
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
        module: &str,
        function: &str,
        _arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        if self.mode == Mode::ExternalEchoArg {
            // Echo the first marshalled argument back: the observable value the engine
            // must have re-derived from the machine after the seam.
            return args.first().copied().ok_or(BeamError::BadArg);
        }
        Err(error::unsupported(&format!("bif {module}:{function}")))
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
        // Collect on *every* rooted reservation: the most aggressive schedule, so the
        // fixture's own `put_tuple2`/`make_fun3` also move their terms.
        self.collect(roots);
        self.bump(n)
    }

    fn resolve_local_fun(
        &mut self,
        _fun: Eterm,
        _args: &[Eterm],
    ) -> Result<Option<(Arc<Module>, Label)>> {
        self.seam_calls += 1;
        // The seam window: a collecting host relocates every term the caller still
        // holds in a Rust local.
        self.collect_now();
        match self.mode {
            Mode::ResolveLocal => match &self.callee {
                Some((module, label)) => Ok(Some((Arc::clone(module), *label))),
                None => Ok(None),
            },
            _ => Ok(None),
        }
    }

    fn apply_fun_outcome(&mut self, fun: Eterm, args: &[Eterm]) -> Result<FunOutcome> {
        self.seam_calls += 1;
        match self.mode {
            // A read of the *closure* word the engine handed over.
            Mode::DeclineEchoUniq => Ok(FunOutcome::Return(make_small(
                terms::fun_uniq(fun)? as i64,
            ))),
            // A read of the *argument array* the engine handed over.
            Mode::DeclineEchoArg => Ok(FunOutcome::Return(
                args.first().copied().ok_or(BeamError::BadArg)?,
            )),
            _ => Err(error::unsupported("apply_fun_outcome: not this mode")),
        }
    }

    fn resolve_external_call(
        &mut self,
        _module: &str,
        _function: &str,
        _arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        self.seam_calls += 1;
        self.collect_now();
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Driving
// ---------------------------------------------------------------------------

/// Run `run/0` of `caller` on a fresh machine, with `ctx` attached to it.
fn run(
    ctx: &mut MovingContext,
    caller: &Module,
    run_atom: Atom,
) -> Result<RunOutcome> {
    let mut machine = Machine::new();
    ctx.attach(&mut machine);
    machine.run_export_outcome(caller, run_atom, 0, &[], ctx)
}

/// The `x(0)` of a completed run.
fn returned(outcome: RunOutcome) -> Eterm {
    match outcome {
        RunOutcome::Returned(value) => value,
        other => panic!("expected a returned value, got {other:?}"),
    }
}

/// Assert `term` is the 3-tuple `expected` **and** lives in the collector's current
/// (post-move) region — i.e. it is the relocated term, not a stale address.
fn assert_moved_tuple(term: Eterm, expected: [i64; 3], ctx: &MovingContext, what: &str) {
    assert!(
        ctx.points_into_current_region(term),
        "{what}: the engine published a term outside the collector's live region \
         (a stale pre-collection address), got {term:?}"
    );
    let arity = terms::tuple_arity(term)
        .unwrap_or_else(|e| panic!("{what}: the published term is not a tuple ({e:?})"));
    assert_eq!(arity, expected.len(), "{what}: tuple arity");
    for (i, want) in expected.iter().enumerate() {
        let got = terms::tuple_element(term, i).expect("tuple element");
        assert_eq!(got, make_small(*want), "{what}: element {i}");
    }
}

// ---------------------------------------------------------------------------
// (1) the in-machine route: the closure's free variables are re-read
// ---------------------------------------------------------------------------

/// The `Ok(Some((callee, label)))` arm publishes the closure's captured variables into
/// `x(arity..)` from `terms::fun_free(fun)`. The seam collects first, so the pre-seam
/// `fun` names the poisoned region: only a **re-read** of the target from its
/// re-readable source (`x(arity)`) yields the moved closure — and through it the moved
/// capture.
#[test]
fn the_free_variables_published_after_the_closure_seam_are_the_moved_terms() {
    let (caller, run_atom) = caller_with_a_captured_tuple();
    let (callee, lambda) = lambda_module();
    let mut ctx = MovingContext::new(Mode::ResolveLocal).resolving(callee, lambda);
    let outcome = run(&mut ctx, &caller, run_atom)
        .expect("the seam resolves the closure and the run completes");
    // The seam really ran (and therefore really collected in the window).
    assert_eq!(ctx.seam_calls, 1, "the closure seam must be consulted once");
    assert!(
        ctx.moved_any && ctx.collections > 0,
        "the harness must have relocated at least one block"
    );
    // The lambda body returns `x(1)` — the capture the engine published — so the run's
    // value IS the free variable.
    let value = returned(outcome);
    assert_moved_tuple(value, CAPTURED_TUPLE, &ctx, "the published free variable");
}

// ---------------------------------------------------------------------------
// (2) the declining route: both the closure word and the argument array
// ---------------------------------------------------------------------------

/// The `Ok(None)` arm hands the closure to `apply_fun_outcome`. The seam collected
/// while declining, so only a re-read of the target from `x(arity)` gives the host the
/// live closure — the pre-fix code hands over the poisoned one.
#[test]
fn the_closure_handed_to_the_apply_fun_fallback_after_the_seam_is_the_moved_one() {
    let (caller, run_atom) = caller_calling_with_a_heap_argument();
    let mut ctx = MovingContext::new(Mode::DeclineEchoUniq);
    let outcome = run(&mut ctx, &caller, run_atom).expect("the fallback answers the closure's uniq");
    assert_eq!(ctx.seam_calls, 2, "the seam and the fallback are both consulted");
    assert!(ctx.moved_any, "the harness must have relocated at least one block");
    // The host read `terms::fun_uniq` off the closure it was handed; a stale word makes
    // that read fail (the poisoned header is not a fun header) instead of answering.
    assert_eq!(
        returned(outcome),
        make_small(LAMBDA_UNIQ as i64),
        "the fallback must be handed the relocated closure"
    );
}

/// The same arm also re-collects the **arguments** from `x(0..arity)`: the pre-seam
/// `register_args` copy is dead once the seam collects.
#[test]
fn the_arguments_handed_to_the_apply_fun_fallback_after_the_seam_are_the_moved_ones() {
    let (caller, run_atom) = caller_calling_with_a_heap_argument();
    let mut ctx = MovingContext::new(Mode::DeclineEchoArg);
    let outcome = run(&mut ctx, &caller, run_atom).expect("the fallback echoes its first argument");
    assert_eq!(ctx.seam_calls, 2);
    assert!(ctx.moved_any);
    assert_moved_tuple(
        returned(outcome),
        HEAP_TUPLE,
        &ctx,
        "the argument handed to the fallback",
    );
}

// ---------------------------------------------------------------------------
// (3) `apply_target`: the argument list is built after the cross-module seam
// ---------------------------------------------------------------------------

/// `apply_target` builds the `call_bif` argument list **after**
/// `resolve_external_call` declines (the seam collects), so the BIF dispatcher is
/// handed live words.
#[test]
fn the_arguments_dispatched_by_apply_after_the_cross_module_seam_are_the_moved_ones() {
    let (caller, run_atom) = apply_caller();
    let mut ctx = MovingContext::new(Mode::ExternalEchoArg);
    let outcome = run(&mut ctx, &caller, run_atom).expect("the BIF echoes its first argument");
    assert_eq!(ctx.seam_calls, 1, "the cross-module seam is consulted once");
    assert!(ctx.moved_any, "the harness must have relocated at least one block");
    assert_moved_tuple(
        returned(outcome),
        HEAP_TUPLE,
        &ctx,
        "the argument handed to the BIF dispatch",
    );
}

// ---------------------------------------------------------------------------
// The control: without a collecting seam, nothing moves
// ---------------------------------------------------------------------------

/// The same fixture with a seam that does **not** collect: the terms stay where they
/// were built and the results are unchanged, so the three cases above differ only by
/// the collection.
#[test]
fn a_non_collecting_seam_leaves_the_values_unchanged() {
    /// A seam host that never collects: the `alloc_words_rooted` door serves storage
    /// from a stable arena and nothing is ever relocated.
    struct QuietHost {
        inner: MovingContext,
        quiet: bool,
    }
    impl ExecutionContext for QuietHost {
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
            heap_need: u32,
            live: u32,
            x_regs: &mut [Eterm],
            stack: &mut [Eterm],
        ) -> Result<()> {
            self.inner.ensure_heap(heap_need, live, x_regs, stack)
        }
        fn alloc_words_rooted(&mut self, n: usize, _roots: RootSet<'_>) -> Result<*mut Eterm> {
            // No collection: hand out storage only.
            self.quiet = true;
            self.inner.bump(n)
        }
        fn resolve_local_fun(
            &mut self,
            _fun: Eterm,
            _args: &[Eterm],
        ) -> Result<Option<(Arc<Module>, Label)>> {
            // No collection, no `machine` handle: the plain `Ok(Some(..))` route.
            match &self.inner.callee {
                Some((module, label)) => Ok(Some((Arc::clone(module), *label))),
                None => Ok(None),
            }
        }
    }

    let (caller, run_atom) = caller_with_a_captured_tuple();
    let (callee, lambda) = lambda_module();
    let mut ctx = QuietHost {
        inner: MovingContext::new(Mode::ResolveLocal).resolving(callee, lambda),
        quiet: false,
    };
    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome(&caller, run_atom, 0, &[], &mut ctx)
        .expect("the in-machine route completes");
    assert!(ctx.quiet, "the rooted seam must have served the fixture's blocks");
    assert_eq!(ctx.inner.collections, 0, "nothing may be collected");
    assert_eq!(
        terms::tuple_element(returned(outcome), 0).expect("element 0"),
        make_small(CAPTURED_TUPLE[0]),
        "a non-collecting host keeps every term where it was built"
    );
}
