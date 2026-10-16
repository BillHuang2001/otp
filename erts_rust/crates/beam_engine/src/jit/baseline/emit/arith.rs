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

//! The baseline JIT's guarded small-int arithmetic fast path — the tier's one
//! bounded optimisation (the section comment below documents why it is exact).
//!
//! It lowers the small-int `erlang:` arithmetic BIFs — `'+'/2`, `'-'/2`, `'*'/2`,
//! the bitwise `band/2`/`bor/2` and the immediate-count `bsr/2` — and nothing else.

use beam_common::Result;
use beam_loader::{ImportRef, Instr, Module, Operand};
use beam_term::{MAX_SMALL, MIN_SMALL, TAG_IMMED1_MASK, TAG_IMMED1_SIZE, TAG_IMMED1_SMALL};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};
use crate::machine::MAX_REGISTER;

use super::operands::{
    constant_word, emit_guarded_y_load, emit_guarded_y_store, x_offset, x_register, y_addressable,
    y_register,
};
use super::r#move::move_shape;
use super::{emit_step_and_continue, touch, Allocation};
// ---------------------------------------------------------------------------
// The small-int arithmetic fast path
// ---------------------------------------------------------------------------
//
// The tier's **one** bounded optimisation: `gc_bif` of `erlang:'+'/2`,
// `erlang:'-'/2` and `erlang:'*'/2` over small integers, emitted natively behind
// guards that can only ever *reject* (falling back to `jit_step_one`). It cannot
// produce a value the interpreter step would not have produced:
//
// * the two operands must both be small integers — the same `is_small` the
//   `operand`/`Test` seams use (`(word & 0xF) == 0xF`, `../../beam_term/src/immediate.rs`);
// * `+`/`-` are done on the tagged words, which is exact because
//   `make_small(v) == v * 16 + 15`, so `a_word + b_word - 15 == make_small(va+vb)`
//   (`- 15` becomes `+ 15` for the subtraction form);
// * `*` cannot use that trick (the encoding is affine, not multiplicative), so it
//   **untags** both operands with an arithmetic shift right by `SMALL_SHIFT`
//   (exact for every small, negatives included: `word == v * 16 + 15` and
//   `15 < 16`, so the floor of `word / 16` is exactly `v`), multiplies the
//   untagged values, and re-tags with `p * 16 + 15`;
// * any `i64` overflow of a step, and any result outside the small range, jumps to
//   the helper, which calls the real BIF (`BeamError`/bignum/float path) — so a
//   result that is not a small integer is never fabricated here. `+`/`-` range-check
//   the *tagged* word (the encoding maps the value range onto
//   `[MIN_SMALL_WORD, MAX_SMALL_WORD]` bijectively); `*` must range-check the
//   **untagged** product, because `p * 16` silently wraps for `|p| >= 2^59` and can
//   land back inside the valid tagged-word interval (a wrong result).
//
// ## The bitwise pair and the immediate-count shift
//
// `erlang:band/2` and `erlang:bor/2` need **no** untag/retag step: the low nibble of
// both operands' word is `TAG_IMMED1_SMALL` (`0b1111`) and the tagging is uniform
// (`make_small(v) == v * 16 + 15`), so a raw tagged-word `and`/`or` is exactly
// `make_small(va & vb)` / `make_small(va | vb)` — for negative operands too, because
// `(v << 4)` is a bit-position bijection. The result is therefore always a correctly
// tagged **small**, so no range check is needed: the `and`/`or` of two words in
// `[MIN_SMALL_WORD, MAX_SMALL_WORD]` whose low nibble is `0b1111` stays in that
// window (the `and` is `<=` and the `or` is `>=` its operands, and the low nibble
// fixes the sign-extension bit).
//
// `erlang:bsr/2` is `X >> N` with an **arithmetic** (sign-extending) shift, i.e.
// floor division by `2^N`; a negative `N` is `bsl`, which can grow to a bignum. The
// fast path claims only the shape whose count is a **compile-time small-int constant
// in `0..64`**: the emitter has only the immediate `sar_ri` form (there is no
// register-count `sar r, cl`), and x86 masks a 64-bit shift count to 6 bits, so a
// register count, a negative count and a count of `64` or more all deopt to the
// interpreter, which computes the true value (a raw `sar` would diverge on the last
// two: `3 bsr 64` is `0`, not `3`, and `3 bsr -1` is `6`). A right shift only shrinks
// the magnitude, so the untagged range check never rejects here; it is kept so the
// re-tag cannot wrap for any operand.
//
// Everything else (another BIF, another module, an operand that is a static literal,
// a destination that is a literal or a float register, a register index outside the
// engine's bound) keeps today's behaviour: one `jit_step_one` call.
//
// ## Y-slot operands and a Y-slot destination
//
// A `Y` operand is a slot in the **current call frame's** Y area, which lives in
// `machine.stack` — a Rust `Vec` that a frame push may reallocate. The runtime
// therefore publishes the live frame's Y window (`JitRuntime::y_base`/`y_end`,
// refreshed by the driver before every native entry and by every frame-changing
// helper) and the emitted Y access is **guarded** against it:
//
// ```text
//   mov  reg,       [rsp + RUNTIME_SLOT_DISP]  ; rt
//   mov  scratch,   [reg + RUNTIME_YEND_DISP]  ; y_end
//   mov  reg,       [reg + RUNTIME_YBASE_DISP] ; y_base
//   lea  scratch2,  [reg + k*8]                ; the slot address
//   cmp  scratch2,  scratch
//   jae  fallback                              ; outside the frame -> interpreter
//   mov  reg,       [scratch2]                 ; (the store writes r11 instead)
// ```
//
// The guard can only ever **reject**: a slot outside the frame jumps to the same
// `jit_step_one` fallback as any other un-lowered shape, and the interpreter's own
// `y(k)` bounds check then produces exactly its error — so no out-of-frame
// dereference and no wrong value is possible. The frame-less case is covered too:
// `y_base == y_end == null` makes `lea k*8; cmp; jae` reject every slot. A Y index so
// large that `y_offset` would overflow the `disp32` is refused at compile time
// ([`super::operands::y_addressable`]).
//
// A Y slot is **not** an x-register, so neither a Y operand nor a Y destination is
// charged to `highest_touched` — that bound is the x-register file's
// (`machine.x.len() > bound`), and raising it for a stack slot would wrongly refuse
// native entry.

/// The six BIFs the fast path lowers — `erlang:'+'/2`, `erlang:'-'/2`,
/// `erlang:'*'/2`, `erlang:band/2`, `erlang:bor/2` and `erlang:bsr/2`, and nothing
/// else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SmallArith {
    /// `erlang:'+'/2`.
    Add,
    /// `erlang:'-'/2`.
    Sub,
    /// `erlang:'*'/2`.
    Mul,
    /// `erlang:band/2` — a raw tagged-word `and` (see the section comment).
    And,
    /// `erlang:bor/2` — a raw tagged-word `or`.
    Or,
    /// `erlang:bsr/2` with a **compile-time** shift count (`FastArith::shift`).
    Bsr,
}

/// `TAG_IMMED1_MASK` as an `i64` — the low-nibble mask `is_small` tests with.
pub(crate) const SMALL_MASK: i64 = TAG_IMMED1_MASK as i64;

/// `TAG_IMMED1_SMALL` as an `i64` — the immediate-1 small tag, which is also
/// `make_small`'s addend (`((v << TAG_IMMED1_SIZE) + TAG_IMMED1_SMALL)`).
pub(crate) const SMALL_TAG: i64 = TAG_IMMED1_SMALL as i64;

/// The emitted guard compares the masked word against [`SMALL_MASK`] (one
/// register holds both, per the objective's shape), so the mask and the tag must
/// be the same nibble. `beam_term` defines both as `0b1111`.
const _: () = assert!(SMALL_MASK == SMALL_TAG);

/// The shift `make_small` applies (`TAG_IMMED1_SIZE`).
pub(crate) const SMALL_SHIFT: u32 = TAG_IMMED1_SIZE;

/// The width of the word the emitted `sar`/`shl` operate on — the bound a shift
/// count must be under for x86 to use it verbatim (the hardware masks a 64-bit
/// shift count to 6 bits, so a count of `64` or more would silently become
/// `count % 64`, a wrong result). `erlang:bsr/2` is therefore lowered only for a
/// **compile-time** count in `0..SHIFT_COUNT_LIMIT` and deopts otherwise.
const SHIFT_COUNT_LIMIT: u32 = 64;

/// The raw word of the smallest small integer (`make_small(MIN_SMALL)`).
///
/// `MIN_SMALL * 16` is exactly `i64::MIN`, so the (saturating, hence exact) word
/// is `i64::MIN + 15`.
pub(crate) const MIN_SMALL_WORD: i64 = MIN_SMALL
    .saturating_mul(1i64 << SMALL_SHIFT)
    .saturating_add(SMALL_TAG);

/// The raw word of the largest small integer (`make_small(MAX_SMALL)`) — which
/// saturates to exactly `i64::MAX`.
pub(crate) const MAX_SMALL_WORD: i64 = MAX_SMALL
    .saturating_mul(1i64 << SMALL_SHIFT)
    .saturating_add(SMALL_TAG);

/// The scratch machine registers the fast path uses for the two operands, the
/// tag/range temporary, the constant, and the result.
///
/// All are caller-saved (System V AMD64), so clobbering them is free, and none
/// overlaps the block's cached x-registers (`r12`–`r15`) or its reserved `rbx`
/// (x-register base) / `rsp` (the `rt` slot). `r11` is also `emit_helper`'s
/// helper-address scratch, but the helper sequence is emitted *after* the fast
/// path has finished with it.
pub(crate) const FAST_A: Reg = Reg::Rax;
/// The second operand's scratch register.
pub(crate) const FAST_B: Reg = Reg::R8;
/// The tag/range temporary.
pub(crate) const FAST_T: Reg = Reg::R10;
/// The tag mask / arithmetic constant.
pub(crate) const FAST_C: Reg = Reg::Rcx;
/// The computed result word.
pub(crate) const FAST_R: Reg = Reg::R11;

/// Where the fast path writes its result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FastDest {
    /// A cacheable (or directly addressable) x-register.
    X(u32),
    /// A slot in the current call frame's Y area, written through the guarded Y
    /// window (see the section comment).
    Y(u32),
}

/// A `gc_bif` shape the fast path can lower.
pub(crate) struct FastArith<'a> {
    /// Which of the two BIFs is called.
    pub(crate) op: SmallArith,
    /// The first operand (an addressable `x(i)`/`y(k)` or a small-int constant).
    pub(crate) a: &'a Operand,
    /// The second operand.
    pub(crate) b: &'a Operand,
    /// Where the result is written.
    pub(crate) dst: FastDest,
    /// For [`SmallArith::Bsr`], the **compile-time** shift count — the second
    /// operand's small-int value, proven at shape time to lie in
    /// `0..SHIFT_COUNT_LIMIT`. `None` for every other operation (whose second
    /// operand is an ordinary operand).
    pub(crate) shift: Option<u32>,
}

impl FastArith<'_> {
    /// The x-register indices the shape needs cached, in load order (the
    /// x-register operands first, then the destination **when it is an
    /// x-register**) — exactly the order [`super::regalloc::allocate_registers`]
    /// caches them in.
    ///
    /// A `Y` operand and a `Y` destination contribute nothing: they are stack slots,
    /// not x-registers, so they must not enter the x-register usage counts (a
    /// spurious entry would evict a real x-register from the cache).
    pub(super) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(3);
        for operand in [self.a, self.b] {
            if let Some(x) = x_register(operand) {
                registers.push(x);
            }
        }
        if let FastDest::X(x) = self.dst {
            registers.push(x);
        }
        registers
    }

    /// The x-register index the shape's native lowering **writes** — the destination
    /// register when it is an x-register, else nothing (a `Y` destination is a stack
    /// slot, never a cached register). The two operands are only read.
    ///
    /// This is the block register cache's write set ([`Allocation::written`]).
    pub(super) fn writes(&self) -> Vec<u32> {
        match self.dst {
            FastDest::X(x) => vec![x],
            FastDest::Y(_) => Vec::new(),
        }
    }
    /// The x-register indices the shape's native lowering **reads through the block
    /// register cache** — its two x-register operands, in load order, and nothing else.
    /// A `Y` operand is a stack slot, never a cached register.
    ///
    /// The destination is a pure write (it receives the computed value), so a destination
    /// register the block also reads is listed as an operand here — which is why an
    /// in-place `x = x + 1` (destination equal to an operand) still reports its register
    /// as read. This is what the block prologue's entry-load decision needs (see
    /// [`super::super::liveness`]).
    pub(super) fn reads(&self) -> Vec<u32> {
        let mut reads = Vec::with_capacity(2);
        for operand in [self.a, self.b] {
            if let Some(x) = x_register(operand) {
                reads.push(x);
            }
        }
        reads
    }

    /// Whether the lowered sequence reads the current frame's Y window.
    ///
    /// A `Y` operand or a `Y` destination is read/written through
    /// `JitRuntime::y_base`/`y_end`, so the emitted code needs the window published.
    /// This predicate feeds the emitter's per-instruction Y-window gate
    /// ([`super::arith::uses_y`], which unions it with every other Y-reading lowering):
    /// a `gc_bif` the fast path declines contributes nothing here and deopts to the
    /// interpreter, which reads `machine.stack` itself.
    pub(crate) fn uses_y(&self) -> bool {
        matches!(self.dst, FastDest::Y(_))
            || y_register(self.a).is_some()
            || y_register(self.b).is_some()
    }
}

/// Whether lowering `instr` would read the current frame's Y window — the emitter's
/// per-instruction Y gate ([`super::Allocation::y_last`]).
///
/// True exactly for the lowerings that read the window, each through its own
/// **syntactic** shape predicate: the arithmetic fast path ([`FastArith::uses_y`], for a
/// `Y` operand or a `Y` destination `gc_bif`), a `move` with a `Y` source or destination
/// ([`move_shape`] + [`super::r#move::MoveShape::uses_y`]), a `swap` with a `Y` side
/// ([`super::r#move::swap_shape`]), the cons-cell readers and the tuple reader/writer
/// with a `Y` source/destination/target (`super::aggregate`'s `*_shape`), an
/// `init_yregs` listing an addressable `Y` register
/// ([`super::operands::locate_all`]), and a `test`/`select_val`/`select_tuple_arity`
/// whose operand is an addressable `Y` slot ([`operand_uses_y`] — the guarded frame
/// window read of [`super::test`]/[`super::select`]/[`super::aggregate`]). MUST stay in
/// exact agreement with the inliners: a
/// shape an inliner **refuses** deopts and reads nothing, so it must be false here too,
/// or a later in-block Y access would use a window published before a `machine.stack`
/// reallocation. Gate conservatively at the call site — a block whose code contains any
/// such instruction publishes the window in its prologue, and refreshes it after an
/// in-block helper call when one still follows.
///
/// A `Y` slot is never an x-register, so none of these shapes charge the live intervals
/// or `highest_native_register` (see [`super::regalloc`]).
pub(crate) fn uses_y(module: &Module, instr: &Instr) -> bool {
    match instr {
        Instr::Move { src, dst } => move_shape(src, dst).map_or(false, |shape| shape.uses_y()),
        Instr::Swap { a, b } => {
            super::r#move::swap_shape(a, b).map_or(false, |shape| shape.uses_y())
        }
        Instr::GetList { source, head, tail } => super::aggregate::get_list_shape(source, head, tail)
            .map_or(false, |shape| shape.uses_y()),
        Instr::GetHd { source, head } => {
            super::aggregate::get_hd_shape(source, head).map_or(false, |shape| shape.uses_y())
        }
        Instr::GetTl { source, tail } => {
            super::aggregate::get_tl_shape(source, tail).map_or(false, |shape| shape.uses_y())
        }
        Instr::GetTupleElement {
            source,
            element,
            dst,
        } => super::aggregate::get_tuple_element_shape(source, *element, dst)
            .map_or(false, |shape| shape.uses_y()),
        Instr::SetTupleElement {
            element,
            tuple,
            offset,
        } => super::aggregate::set_tuple_element_shape(element, tuple, *offset)
            .map_or(false, |shape| shape.uses_y()),
        Instr::InitYregs { registers } => super::operands::locate_all(registers)
            .map_or(false, |locations| locations.iter().any(|location| location.is_y())),
        // A `test`/`select_val`/`select_tuple_arity` whose operand is an addressable
        // `Y` slot reads the frame window ([`super::test`]'s / [`super::select`]'s /
        // [`super::aggregate`]'s guarded Y read). Purely syntactic, so it is an exact
        // superset of the Y sources the plans claim.
        Instr::Test { args, .. } => args.iter().any(|operand| operand_uses_y(operand)),
        Instr::SelectVal { value, .. } => operand_uses_y(value),
        Instr::SelectTupleArity { tuple, .. } => operand_uses_y(tuple),
        // The three `Bif`s the tier lowers (`erlang:hd/1`/`element/2`/`tuple_size/1`)
        // read their source and write their destination through the same guarded
        // frame window the aggregate readers use, so a `Y` operand of one makes the
        // block publish it ([`super::bif::bif_uses_y`], purely syntactic).
        Instr::Bif { .. } => super::bif::bif_uses_y(module, instr),
        _ => fast_arith(module, instr).map_or(false, |fast| fast.uses_y()),
    }
}

/// Whether an operand is an addressable `Y` slot (the syntactic Y-source test the
/// `test`/`select_val`/`select_tuple_arity` lowerings share).
fn operand_uses_y(operand: &Operand) -> bool {
    super::operands::Loc::of(operand).map_or(false, super::operands::Loc::is_y)
}

/// The fast path's shape test for `instr`, a *purely syntactic* check (it never
/// consults the register allocation, so [`super::regalloc::allocate_registers`] and [`super::emit_block`]
/// agree on which `gc_bif`s the fast path targets).
///
/// The shape is the small-int `erlang:'+'/2`/`'-'/2`/`'*'/2`/`band/2`/`bor/2`/`bsr/2`
/// of `args.len() == 2` whose every operand is an in-bound x-register, an addressable
/// Y slot or a small-int constant and whose destination is an in-bound x-register or
/// an addressable Y slot. `bsr/2` carries the extra, still purely syntactic,
/// condition that its **count** operand is a small-int constant in
/// `0..SHIFT_COUNT_LIMIT` (see the section comment): the emitted shift is the
/// immediate `sar_ri`, and a register or out-of-range count would not be exact.
pub(crate) fn fast_arith<'a>(module: &Module, instr: &'a Instr) -> Option<FastArith<'a>> {
    let Instr::GcBif {
        arity, bif, args, dst, ..
    } = instr
    else {
        return None;
    };
    let (module_name, function_name, import_arity) = import_names(module, *bif)?;
    if module_name != "erlang" || import_arity != 2 || *arity != 2 || args.len() != 2 {
        return None;
    }
    let op = match function_name {
        "+" => SmallArith::Add,
        "-" => SmallArith::Sub,
        "*" => SmallArith::Mul,
        "band" => SmallArith::And,
        "bor" => SmallArith::Or,
        "bsr" => SmallArith::Bsr,
        _ => return None,
    };
    let dst = destination(dst)?;
    if args.iter().any(|operand| !loadable(operand)) {
        return None;
    }
    // `bsr`'s count must be known at **compile** time and stay inside the width the
    // emitted shift honours: a register count has no immediate form to emit, a
    // negative count is `bsl` (which grows, possibly to a bignum) and a count of `64`
    // or more would be masked by the hardware. All of those deopt.
    let shift = match op {
        SmallArith::Bsr => {
            let count = integer_value(&args[1])?;
            Some(u32::try_from(count).ok().filter(|c| *c < SHIFT_COUNT_LIMIT)?)
        }
        _ => None,
    };
    Some(FastArith {
        op,
        a: &args[0],
        b: &args[1],
        dst,
        shift,
    })
}

/// The `i64` value of a **small-int** constant operand, or `None` for anything else.
///
/// The untag is the documented exact arithmetic shift the emission itself uses:
/// `word == v * 16 + 15` with `15 < 16`, so `word >> 4` is exactly `v`.
fn integer_value(operand: &Operand) -> Option<i64> {
    small_constant(operand).map(|word| word >> SMALL_SHIFT)
}

/// The [`FastDest`] for `operand`, or `None` when it is neither an in-bound
/// x-register nor an addressable Y slot.
fn destination(operand: &Operand) -> Option<FastDest> {
    if let Some(x) = x_register(operand) {
        return (x < MAX_REGISTER).then_some(FastDest::X(x));
    }
    if let Some(k) = y_register(operand) {
        return y_addressable(k).then_some(FastDest::Y(k));
    }
    None
}

/// Whether `operand` is something the fast path can load into a scratch
/// register: an in-bound x-register, an addressable Y slot, or a small-int
/// constant.
fn loadable(operand: &Operand) -> bool {
    if let Some(x) = x_register(operand) {
        return x < MAX_REGISTER;
    }
    if let Some(k) = y_register(operand) {
        return y_addressable(k);
    }
    small_constant(operand).is_some()
}
/// The raw `Eterm` word of a **small-int** constant operand (`integer`/
/// `character`), or `None` for anything else — including the non-small `atom`/
/// `nil` constants, which the run-time guard would reject anyway (so refusing
/// them at compile time only saves the emission).
fn small_constant(operand: &Operand) -> Option<i64> {
    constant_word(operand).filter(|word| (word & SMALL_MASK) == SMALL_TAG)
}

/// Resolve an imported `{module,function,arity}` to its names, or `None` when
/// the [`ImportRef`] or one of its atoms is missing from `module`.
///
/// This deliberately reproduces `crate::interpreter`'s (private) `import_target`:
/// the fast path must classify a BIF by exactly the strings the interpreter's
/// `GcBif` arm hands to [`ExecutionContext::call_bif`], or it would lower a call
/// the interpreter routes somewhere else.
///
/// The sibling `baseline/bif_helper.rs` value helper reuses this same resolver (hence
/// `pub(in crate::jit::baseline)`), so both fast-path routes classify a BIF identically.
pub(in crate::jit::baseline) fn import_names<'m>(
    module: &'m Module,
    target: ImportRef,
) -> Option<(&'m str, &'m str, u32)> {
    let import = module.imports.get(target.0 as usize)?;
    let module_name = module.atom_name(import.module)?;
    let function_name = module.atom_name(import.function)?;
    Some((module_name, function_name, import.arity))
}

/// Where the fast path reads one operand from.
#[derive(Clone, Copy)]
enum FastSource {
    /// A cached x-register (already loaded by the prologue).
    Cached(Reg),
    /// A non-cached x-register, read straight from its `machine.x` slot.
    Memory(u32),
    /// A Y slot, read through the guarded frame window.
    YSlot(u32),
    /// A small-int constant, loaded as a raw `Eterm` word.
    Constant(i64),
}

/// The [`FastSource`] for `operand`, or `None` when it is neither an addressable
/// x-register, an addressable Y slot nor a small constant.
///
/// A non-cached x-register is a `machine.x` read, not a deopt: the register
/// allocator's cache budget decides only *where* the value is read from,
/// never whether the instruction can be lowered. A Y slot is likewise read natively,
/// behind the run-time frame-window guard.
fn fast_source(operand: &Operand, allocation: &Allocation) -> Option<FastSource> {
    if let Some(x) = x_register(operand) {
        if x >= MAX_REGISTER {
            return None;
        }
        return Some(match allocation.slot(x) {
            Some(reg) => FastSource::Cached(reg),
            None => FastSource::Memory(x),
        });
    }
    if let Some(k) = y_register(operand) {
        return y_addressable(k).then_some(FastSource::YSlot(k));
    }
    small_constant(operand).map(FastSource::Constant)
}

/// Load `source` into scratch register `reg`, recording a `machine.x` read and
/// jumping to `fallback` when a **Y slot** lies outside the current frame.
///
/// The Y branch is the only one that can transfer control at run time: every other
/// source is a compile-time-addressable x-register slot or an immediate.
///
/// The Y-slot guard itself is the shared [`emit_guarded_y_load`]: `reg` receives the
/// slot's word and [`FAST_C`] is its window-bound scratch (caller-saved and dead at
/// operand-load time — the mask constant is loaded into it only **after** both operands
/// are loaded). See that helper for the exact emitted sequence and why the guard can
/// only ever reject.
fn load_into(
    buffer: &mut CodeBuffer,
    source: FastSource,
    reg: Reg,
    fallback: CodeLabel,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match source {
        FastSource::Cached(src) => buffer.mov_rr(reg, src),
        FastSource::Memory(x) => {
            touch(highest_touched, x);
            buffer.mov_rm(reg, Reg::Rbx, x_offset(x))
        }
        FastSource::YSlot(k) => emit_guarded_y_load(buffer, reg, FAST_C, k, fallback),
        FastSource::Constant(word) => buffer.mov_ri(reg, word),
    }
}

/// Emit the **guarded** store of the fast path's result (`FAST_R`) into Y slot `k`,
/// jumping to `fallback` when the slot lies outside the current frame's Y window.
///
/// The store is the shared [`emit_guarded_y_store`]: [`FAST_A`] holds the slot address
/// and [`FAST_C`] the window bound. Both are dead when this runs (the arithmetic is
/// complete), and the value being stored lives in [`FAST_R`] — so the three are distinct.
/// A rejection falls back to `jit_step_one`, which re-runs the **whole** instruction in
/// the interpreter — nothing has been committed to the machine before this store, so the
/// interpreter sees exactly the state it would have seen.
fn emit_y_store(buffer: &mut CodeBuffer, k: u32, fallback: CodeLabel) -> Result<()> {
    emit_guarded_y_store(buffer, FAST_R, FAST_A, FAST_C, k, fallback)
}

/// Lower a fast-path-eligible `gc_bif` natively, or report that the block must
/// helper it.
///
/// Returns `true` when the whole guarded sequence was emitted, leaving the
/// result in the destination (its cached machine register, its `machine.x` slot
/// when it is beyond the cache budget, or its Y slot behind the frame-window
/// guard) and letting the block continue with the next instruction. Returns
/// `false` **without having emitted a byte** when the shape matched
/// ([`fast_arith`]) but an operand or the destination names neither an
/// in-bound x-register nor an addressable Y slot — the caller then emits exactly
/// today's `jit_step_one` fallback.
///
/// A Y operand or a Y destination additionally carries a **run-time** rejection:
/// the emitted frame-window guard can branch to the same `fallback` when the slot
/// lies outside the current frame, so an in-frame Y `gc_bif` runs natively and a
/// malformed frame keeps the interpreter's exact error.
///
/// The emitted shape (see the section comment above for why it is exact):
///
/// ```text
///   mov  rax, a          ; the operand: a cached register, `[rbx + x*8]`, a
///                        ; movabs of a small constant, or the guarded Y read below
///   mov  r8,  b
///   mov  rcx, SMALL_MASK
///   mov  r10, rax ; and r10, rcx ; cmp r10, rcx ; jne fallback   ; a is small?
///   mov  r10, r8  ; and r10, rcx ; cmp r10, rcx ; jne fallback   ; b is small?
///   ; --- erlang:'+'/2 and erlang:'-'/2: tagged-word arithmetic ---
///   mov  r11, rax
///   add  r11, r8            ; sub for '-'
///   jo   fallback           ; not both representable as an i64 tagged word
///   mov  rcx, SMALL_TAG  ; the double-tag adjustment make_small re-adds
///   sub  r11, rcx           ; add for '-'
///   jo   fallback
///   mov  rcx, MIN_SMALL_WORD ; cmp r11, rcx ; jl fallback
///   mov  rcx, MAX_SMALL_WORD ; cmp r11, rcx ; jg fallback
///   ; --- erlang:'*'/2: untag, multiply, range-check the product, re-tag ---
///   sar  rax, SMALL_SHIFT   ; va = word >>a 4 (exact for every small)
///   sar  r8,  SMALL_SHIFT   ; vb
///   mov  r11, rax
///   imul r11, r8            ; va * vb, `jo` on signed overflow
///   jo   fallback
///   mov  rcx, MIN_SMALL ; cmp r11, rcx ; jl fallback  ; the UNTAGGED product:
///   mov  rcx, MAX_SMALL ; cmp r11, rcx ; jg fallback  ; `p * 16` wraps for large p
///   shl  r11, SMALL_SHIFT   ; make_small(p) == p * 16 + 15, safe after the check
///   mov  rcx, SMALL_TAG
///   or   r11, rcx
///   ; --- erlang:band/2 and erlang:bor/2: a raw tagged-word and/or, no re-tag ---
///   mov  r11, rax
///   and  r11, r8            ; or for 'bor' — the low nibble stays TAG_IMMED1_SMALL,
///                           ; so the tagged word is exactly make_small(va & vb)
///   ; --- erlang:bsr/2: untag, arithmetic shift by the compile-time count, re-tag ---
///   sar  rax, SMALL_SHIFT   ; va
///   sar  rax, <count>       ; va >> count, sign-extending (count in 0..64, so the
///                           ; hardware's 6-bit count masking is never reached; the
///                           ; shift is omitted when the count is 0)
///   mov  r11, rax
///   mov  rcx, MIN_SMALL ; cmp r11, rcx ; jl fallback  ; never rejects (a right shift
///   mov  rcx, MAX_SMALL ; cmp r11, rcx ; jg fallback  ; only shrinks the magnitude)
///   shl  r11, SMALL_SHIFT
///   mov  rcx, SMALL_TAG
///   or   r11, rcx
///   mov  <dst>, r11         ; cached destination: its machine register; a
///                           ; non-cached one: `mov [rbx + dst*8], r11`; a Y slot:
///                           ; the guarded Y store (see `emit_y_store`)
///   jmp  fast_done
/// fallback:
///   <the `emit_step_and_continue` sequence: spill, call `jit_step_one`, continue
///    at `pc + 1` when the helper advanced by one, else exit to the driver>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// The fast path never spills: the destination's cached register is written
/// back by the next helper call or by the block's own exit spill, and a
/// non-cached destination is written to `machine.x` — the exact slot the spill
/// would write — so the spill-across-blocks discipline is unchanged.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_gc_bif(
    buffer: &mut CodeBuffer,
    module: &Module,
    instr: &Instr,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(fast) = fast_arith(module, instr) else {
        return Ok(false);
    };
    let Some(a_source) = fast_source(fast.a, allocation) else {
        return Ok(false);
    };
    let Some(b_source) = fast_source(fast.b, allocation) else {
        return Ok(false);
    };

    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // Load both operands into scratch registers. A Y operand carries its own
    // frame-window guard, whose rejection jumps to `fallback` before anything is
    // committed.
    load_into(buffer, a_source, FAST_A, fallback, highest_touched)?;
    load_into(buffer, b_source, FAST_B, fallback, highest_touched)?;
    // Both operands must be small integers: `(word & 0xF) == 0xF`. The mask and
    // the tag are the same nibble (asserted above), so one register serves both.
    buffer.mov_ri(FAST_C, SMALL_MASK)?;
    for operand in [FAST_A, FAST_B] {
        buffer.mov_rr(FAST_T, operand)?;
        buffer.and_rr(FAST_T, FAST_C)?;
        buffer.cmp_rr(FAST_T, FAST_C)?;
        buffer.jcc(Cond::Ne, fallback)?;
    }

    // Compute the result word in `r11`.
    match fast.op {
        // `+`/`-`: tagged-word arithmetic; an `i64` overflow of either step means
        // the result is not representable as a tagged small word, so the helper
        // computes it.
        SmallArith::Add | SmallArith::Sub => {
            buffer.mov_rr(FAST_R, FAST_A)?;
            if fast.op == SmallArith::Add {
                buffer.add_rr(FAST_R, FAST_B)?;
            } else {
                buffer.sub_rr(FAST_R, FAST_B)?;
            }
            buffer.jcc(Cond::O, fallback)?;
            buffer.mov_ri(FAST_C, SMALL_TAG)?;
            if fast.op == SmallArith::Add {
                buffer.sub_rr(FAST_R, FAST_C)?;
            } else {
                buffer.add_rr(FAST_R, FAST_C)?;
            }
            buffer.jcc(Cond::O, fallback)?;

            // Range check: a tagged word is a valid small integer iff it lies in
            // `[MIN_SMALL_WORD, MAX_SMALL_WORD]` (the encoding `v -> v*16 + 15` maps
            // the value range onto that interval bijectively).
            buffer.mov_ri(FAST_C, MIN_SMALL_WORD)?;
            buffer.cmp_rr(FAST_R, FAST_C)?;
            buffer.jcc(Cond::L, fallback)?;
            buffer.mov_ri(FAST_C, MAX_SMALL_WORD)?;
            buffer.cmp_rr(FAST_R, FAST_C)?;
            buffer.jcc(Cond::G, fallback)?;
        }
        // `*`: the tagged words cannot be multiplied directly (`make_small` is
        // affine), so untag both with an **arithmetic** shift right — exact for
        // every small, negatives included, because `word == v * 16 + 15` and
        // `15 < 16` makes the floor of `word / 16` exactly `v`.
        SmallArith::Mul => {
            buffer.sar_ri(FAST_A, SMALL_SHIFT)?;
            buffer.sar_ri(FAST_B, SMALL_SHIFT)?;
            buffer.mov_rr(FAST_R, FAST_A)?;
            buffer.imul_rr(FAST_R, FAST_B)?;
            // A signed overflow means the product is not an `i64`, let alone a small.
            buffer.jcc(Cond::O, fallback)?;

            // Range check the **untagged** product. This is *not* interchangeable
            // with the tagged-word check above: `p * 16` silently wraps for
            // `|p| >= 2^59` and can land back inside `[MIN_SMALL_WORD, MAX_SMALL_WORD]`
            // (e.g. `make_small(MAX_SMALL) * 2` tags to the word `-17`, which the
            // tagged check would accept), so only the untagged value is aliasing-free.
            buffer.mov_ri(FAST_C, MIN_SMALL)?;
            buffer.cmp_rr(FAST_R, FAST_C)?;
            buffer.jcc(Cond::L, fallback)?;
            buffer.mov_ri(FAST_C, MAX_SMALL)?;
            buffer.cmp_rr(FAST_R, FAST_C)?;
            buffer.jcc(Cond::G, fallback)?;

            // Re-tag: `make_small(p) == p * 16 + 15`, with the shift now provably
            // lossless (`|p| <= 2^59`).
            buffer.shl_ri(FAST_R, SMALL_SHIFT)?;
            buffer.mov_ri(FAST_C, SMALL_TAG)?;
            buffer.or_rr(FAST_R, FAST_C)?;
        }
        // `band`/`bor`: the tagged words are `v * 16 + 15`, and `0b1111` is all-ones
        // in the four low bits, so a raw `and`/`or` of the two words is exactly
        // `make_small(va & vb)` / `make_small(va | vb)` — for negative operands too.
        // The result is always a valid tagged small, so there is no untag, no re-tag
        // and no range check.
        SmallArith::And | SmallArith::Or => {
            buffer.mov_rr(FAST_R, FAST_A)?;
            if fast.op == SmallArith::And {
                buffer.and_rr(FAST_R, FAST_B)?;
            } else {
                buffer.or_rr(FAST_R, FAST_B)?;
            }
        }
        // `bsr`: `X bsr N == X >> N` with an **arithmetic** (sign-extending) shift,
        // i.e. floor division by `2^N`. The count is a compile-time constant in
        // `0..64` (`fast_arith` refuses anything else), so the immediate form is
        // exact and the hardware's 6-bit count masking can never bite.
        SmallArith::Bsr => {
            let count = fast
                .shift
                .expect("`Bsr` is only built with a compile-time shift count");
            // Untag (exact for every small, negatives included), then shift.
            buffer.sar_ri(FAST_A, SMALL_SHIFT)?;
            if count > 0 {
                buffer.sar_ri(FAST_A, count)?;
            }
            buffer.mov_rr(FAST_R, FAST_A)?;
            // A right shift only shrinks the magnitude, so this guard never rejects;
            // it is kept so the re-tag below cannot wrap for any operand.
            buffer.mov_ri(FAST_C, MIN_SMALL)?;
            buffer.cmp_rr(FAST_R, FAST_C)?;
            buffer.jcc(Cond::L, fallback)?;
            buffer.mov_ri(FAST_C, MAX_SMALL)?;
            buffer.cmp_rr(FAST_R, FAST_C)?;
            buffer.jcc(Cond::G, fallback)?;
            buffer.shl_ri(FAST_R, SMALL_SHIFT)?;
            buffer.mov_ri(FAST_C, SMALL_TAG)?;
            buffer.or_rr(FAST_R, FAST_C)?;
        }
    }

    // Success: store the result — in the destination's cached register, straight
    // into its `machine.x` slot when the budget could not cache it, or into the
    // destination Y slot behind the same frame-window guard — and skip the fallback.
    match fast.dst {
        FastDest::X(dst_x) => match allocation.slot(dst_x) {
            Some(dst_reg) => buffer.mov_rr(dst_reg, FAST_R)?,
            None => {
                buffer.mov_mr(Reg::Rbx, x_offset(dst_x), FAST_R)?;
                touch(highest_touched, dst_x);
            }
        },
        // A Y destination is not an x-register, so it never raises the x-register
        // bound the driver's entry guard reads.
        FastDest::Y(k) => emit_y_store(buffer, k, fallback)?,
    }
    buffer.jmp(fast_done)?;
    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}
