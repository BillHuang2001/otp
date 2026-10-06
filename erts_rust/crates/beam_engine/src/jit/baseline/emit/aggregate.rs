//! The baseline JIT's guarded cons-cell readers: `get_list`/`get_hd`/`get_tl`
//! behind a guard that can only ever *reject* (a non-cons source falls back to
//! `jit_step_one`, which raises the interpreter's exact `{badmatch, Source}`).

use std::mem::size_of;

use beam_common::Result;
use beam_loader::Operand;
use beam_term::{Eterm, CAR_OFFSET, CDR_OFFSET, TAG_PRIMARY_LIST, TAG_PRIMARY_MASK, TAG_PTR_MASK};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};
use crate::machine::MAX_REGISTER;

use super::operands::{x_offset, x_register};
use super::{emit_step_and_continue, touch, Allocation, MOVE_SCRATCH, MOVE_SCRATCH_2};

// ---------------------------------------------------------------------------
// The cons-cell readers
// ---------------------------------------------------------------------------
//
// `get_list`/`get_hd`/`get_tl` split a cons cell (`emu/instrs.tab` `get_list`
// `:389`, `get_hd` `:398`, `get_tl` `:403`). The interpreter
// (`crate::families::lists`) reads the source, gates on `beam_term::is_list`
// (via `terms::cons_head`/`cons_tail`) and, on success, writes the CAR — then
// the CDR — to the destination operands; a non-cons source *raises*
// `{badmatch, Source}` through `terms::badmatch_reason`.
//
// The subset below is lowered natively and mirrors that branch for branch:
//
// * the source and **every** destination must be an x-register inside the
//   engine's [`MAX_REGISTER`] bound — a `Y` operand is not something the tier
//   can address, and a register index past the bound would read out of
//   `machine.x` (the interpreter step reports the faithful `bad_register`);
// * the guard is `is_list` itself, the C bit trick
//   `(word & (TAG_PRIMARY_MASK - TAG_PRIMARY_LIST)) == 0`: the emitted `and`
//   leaves `0` for a cons cell (primary tag `LIST`) and the mask itself for
//   everything else — `NIL`, a small int, an atom, a boxed term (`[]`, `1`,
//   `foo`, a tuple, …) — so the comparison against the mask is exactly
//   "not a cons cell" and branches to the helper, whose interpreter step raises
//   the exact `{badmatch, Source}`;
// * on success the cell's address is `list_val(word)` (`word & !TAG_PTR_MASK`),
//   the CAR is the word at offset 0 and the CDR the word at the cons cell's
//   second word (`beam_term::{CAR_OFFSET, CDR_OFFSET}`).
//
// The destinations are written **CAR first, then CDR** — the interpreter's write
// order — so a list read whose destinations alias each other (or the source)
// ends with the CDR value, exactly as the interpreter's does.

/// `TAG_PRIMARY_MASK - TAG_PRIMARY_LIST` as an `i64` — the mask of the
/// `is_not_list` bit trick (`beam_term::list::is_not_list`), which is `0x2`.
const NOT_LIST_MASK: i64 = (TAG_PRIMARY_MASK - TAG_PRIMARY_LIST) as i64;

/// `!TAG_PTR_MASK` as an `i64` — the mask `list_val` clears the pointer tag with.
const POINTER_MASK: i64 = !(TAG_PTR_MASK as i64);

/// The byte offset of the CAR (head) word inside a cons cell.
const CAR_DISP: i32 = (size_of::<Eterm>() * CAR_OFFSET) as i32;

/// The byte offset of the CDR (tail) word inside a cons cell.
const CDR_DISP: i32 = (size_of::<Eterm>() * CDR_OFFSET) as i32;

/// Which half of a cons cell a destination receives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Half {
    /// The CAR (head) word — `get_list`'s head destination, `get_hd`.
    Head,
    /// The CDR (tail) word — `get_list`'s tail destination, `get_tl`.
    Tail,
}

/// The x-register operands of a list read the tier lowers natively: the source cons
/// cell and the destinations it writes, in emission order.
#[derive(Clone)]
pub(super) struct ListShape {
    /// The `source` operand's x-register.
    source: u32,
    /// The `(half, destination)` writes, in emission order: the CAR before the CDR
    /// wherever both are written (`get_list`), so aliasing destinations end with the
    /// CDR value exactly as the interpreter's write order does.
    destinations: Vec<(Half, u32)>,
}

impl ListShape {
    /// The x-register indices the shape's native lowering touches, in emission
    /// order (the source first, then each destination).
    pub(super) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(self.destinations.len() + 1);
        registers.push(self.source);
        registers.extend(self.destinations.iter().map(|(_, x)| *x));
        registers
    }
}

/// The inlinable shape of a `get_list Source Head Tail`, or `None` when the whole
/// instruction must stay on the helper.
pub(super) fn get_list_shape(
    source: &Operand,
    head: &Operand,
    tail: &Operand,
) -> Option<ListShape> {
    shape(source, &[(Half::Head, head), (Half::Tail, tail)])
}

/// The inlinable shape of a `get_hd Source Head`, or `None` when the whole
/// instruction must stay on the helper.
pub(super) fn get_hd_shape(source: &Operand, head: &Operand) -> Option<ListShape> {
    shape(source, &[(Half::Head, head)])
}

/// The inlinable shape of a `get_tl Source Tail`, or `None` when the whole
/// instruction must stay on the helper.
pub(super) fn get_tl_shape(source: &Operand, tail: &Operand) -> Option<ListShape> {
    shape(source, &[(Half::Tail, tail)])
}

/// The inlinable shape of a list read that writes `writes` (in that order), or
/// `None` for a shape above the subset.
///
/// The source and every destination must be an x-register inside
/// [`MAX_REGISTER`] — see the section comment above.
fn shape(source: &Operand, writes: &[(Half, &Operand)]) -> Option<ListShape> {
    let source = in_bound_x(source)?;
    let destinations = writes
        .iter()
        .map(|(half, operand)| in_bound_x(operand).map(|x| (*half, x)))
        .collect::<Option<Vec<_>>>()?;
    Some(ListShape {
        source,
        destinations,
    })
}

/// The x-register index `operand` names, if it lies inside [`MAX_REGISTER`].
///
/// An out-of-bound index makes the instruction deopt, so the helper reports the
/// interpreter's exact `bad_register` instead of the emitted code reading past
/// the end of `machine.x`.
fn in_bound_x(operand: &Operand) -> Option<u32> {
    x_register(operand).filter(|x| *x < MAX_REGISTER)
}

/// Emit `get_list Source Head Tail` natively, or report that the block must
/// helper it.
///
/// Returns `true` when the whole guarded sequence was emitted, leaving the CAR in
/// `head` and the CDR in `tail`. Returns `false` **without having emitted a byte**
/// when the operands are not three in-bound x-registers ([`get_list_shape`]) — the
/// caller then emits exactly today's `jit_step_one` step.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_get_list(
    buffer: &mut CodeBuffer,
    source: &Operand,
    head: &Operand,
    tail: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = get_list_shape(source, head, tail) else {
        return Ok(false);
    };
    emit_list_read(buffer, &shape, allocation, done, func, pc, highest_touched)?;
    Ok(true)
}

/// Emit `get_hd Source Head` natively, or report that the block must helper it
/// (see [`try_inline_get_list`] for the return convention).
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_get_hd(
    buffer: &mut CodeBuffer,
    source: &Operand,
    head: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = get_hd_shape(source, head) else {
        return Ok(false);
    };
    emit_list_read(buffer, &shape, allocation, done, func, pc, highest_touched)?;
    Ok(true)
}

/// Emit `get_tl Source Tail` natively, or report that the block must helper it
/// (see [`try_inline_get_list`] for the return convention).
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_get_tl(
    buffer: &mut CodeBuffer,
    source: &Operand,
    tail: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = get_tl_shape(source, tail) else {
        return Ok(false);
    };
    emit_list_read(buffer, &shape, allocation, done, func, pc, highest_touched)?;
    Ok(true)
}

/// Lower an inlinable list read (see the section comment above for why it is
/// exact). The emitted shape:
///
/// ```text
///   mov  r11, <source>      ; the source word: a cached register, or `[rbx + x*8]`
///   mov  r10, NOT_LIST_MASK ; the `is_not_list` bit
///   and  r11, r10
///   cmp  r11, r10           ; the masked word is 0 (a cons cell) or the mask itself
///   je   fallback           ; not a cons cell: the helper raises `{badmatch,Source}`
///   mov  r11, <source>      ; reload (the mask consumed the word)
///   mov  r10, POINTER_MASK
///   and  r11, r10           ; r11 = the cons cell's address, `list_val(word)`
///   mov  r10, [r11 + 0]     ; the CAR           — for a `get_list`, first ...
///   mov  <head>, r10
///   mov  r10, [r11 + 8]     ; the CDR           — ... then the tail
///   mov  <tail>, r10
///   jmp  fast_done
/// fallback:
///   <the `emit_step_and_continue` sequence: spend one `jit_step_one` on the
///    instruction — which either advances and lets the block carry on at
///    `pc + 1`, or exits to the driver with the helper's word>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// A `get_hd` writes only the CAR and a `get_tl` only the CDR (the half each
/// destination receives is [`ListShape::destinations`]' `Half`). Every destination
/// is written through its own representation — a cached register, or its
/// `machine.x` slot (the very slot the exit spill would write) — and the source is
/// read the same way, so the tier's liveness model is unchanged. The fast path
/// never spills.
fn emit_list_read(
    buffer: &mut CodeBuffer,
    shape: &ListShape,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // The guard: `is_not_list(source)` — a non-cons source (`[]`, a small int, an
    // atom, a tuple) branches to the helper, which raises `{badmatch, Source}`.
    load_x(
        buffer,
        shape.source,
        allocation,
        MOVE_SCRATCH,
        highest_touched,
    )?;
    buffer.mov_ri(MOVE_SCRATCH_2, NOT_LIST_MASK)?;
    buffer.and_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.cmp_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::E, fallback)?;

    // The cell's address: reload the source word (the mask above consumed it) and
    // clear the pointer tag — `list_val(word)`.
    load_x(
        buffer,
        shape.source,
        allocation,
        MOVE_SCRATCH,
        highest_touched,
    )?;
    buffer.mov_ri(MOVE_SCRATCH_2, POINTER_MASK)?;
    buffer.and_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;

    // The destinations, in emission order — CAR first, then CDR — the interpreter's
    // write order, so aliasing destinations (`head == tail`, or a destination that
    // is the source) end with the CDR value.
    for (half, destination) in &shape.destinations {
        let disp = match half {
            Half::Head => CAR_DISP,
            Half::Tail => CDR_DISP,
        };
        buffer.mov_rm(MOVE_SCRATCH_2, MOVE_SCRATCH, disp)?;
        store_x(
            buffer,
            *destination,
            MOVE_SCRATCH_2,
            allocation,
            highest_touched,
        )?;
    }
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(())
}

/// Load x-register `x` into scratch register `reg` — from its cache register, or
/// straight from its `machine.x` slot, recording the direct access.
fn load_x(
    buffer: &mut CodeBuffer,
    x: u32,
    allocation: &Allocation,
    reg: Reg,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match allocation.slot(x) {
        Some(src) => buffer.mov_rr(reg, src),
        None => {
            buffer.mov_rm(reg, Reg::Rbx, x_offset(x))?;
            touch(highest_touched, x);
            Ok(())
        }
    }
}

/// Store scratch register `src` into x-register `x` — into its cache register, or
/// straight into its `machine.x` slot (the slot the exit spill writes), recording
/// the direct access.
fn store_x(
    buffer: &mut CodeBuffer,
    x: u32,
    src: Reg,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match allocation.slot(x) {
        Some(dst) => buffer.mov_rr(dst, src),
        None => {
            buffer.mov_mr(Reg::Rbx, x_offset(x), src)?;
            touch(highest_touched, x);
            Ok(())
        }
    }
}
