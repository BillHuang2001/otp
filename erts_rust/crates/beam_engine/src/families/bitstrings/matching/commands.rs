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

//! The flattened `bs_match` command interpreter.
//!
//! `bs_match`'s `commands` operand is the raw flattened list the loader preserved;
//! decoding the records is this module's job. A local `pos` cursor walks the subject
//! bits, and every command that advances it mirrors the advance into the context's own
//! position field **before** binding its destination ([`commit`]) — the C order
//! (`i_bs_read_bits`/`i_bs_get_fixed_integer`/`i_bs_get_fixed_binary`/`i_bs_skip` all do
//! `sb->start += …` before `$Dst = result`), which has to hold even when the destination
//! *is* the register the context lives in (`get_tail` binds the tail into the context's
//! own register). `ensure_at_least`/`ensure_exactly` only test; the reads bind a value
//! into a register.
use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Label, Module, Operand};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::Step;

use super::super::{
    decode_flags, extract_bits, fail_step, is_all, raw, read_size, read_uint_be, small_value,
};
use super::match_ctx::{read_ctx, reread_ctx};
use super::reads::read_integer_field_tracked;
// ---------------------------------------------------------------------------
// The `bs_match` command interpreter
// ---------------------------------------------------------------------------
/// Execute a `bs_match` command list against the context in `ctx_op`.
///
/// Two routes, one semantics. [`run_commands_fast`] runs the handful of command
/// shapes the standard compiler actually emits for fixed-size matching off a single
/// up-front recognition of the **whole** operand list; anything it does not fully
/// recognise — a register or literal-operand size, a negative or oversized literal, a
/// truncated list, another command mix — is handed to [`run_commands_general`], which
/// is the reference interpreter and owns every diagnostic. A false negative on the
/// fast route therefore costs a little speed and never changes a result.
pub(crate) fn run_commands(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    ctx_op: &Operand,
    commands: &[Operand],
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    match run_commands_fast(machine, module, labels, fail, ctx_op, commands, ctx)? {
        Fast::Step(step) => Ok(step),
        Fast::NotMine => {
            run_commands_general(machine, module, labels, fail, ctx_op, commands, ctx)
        }
    }
}

/// What [`run_commands_fast`] decided.
enum Fast {
    /// The operand list is not one of the fused shapes: the general loop owns it.
    NotMine,
    /// The instruction completed (or raised) on the fused route.
    Step(Option<Step>),
}

/// The leading bounds/alignment test of a fused shape.
enum Guard {
    /// `ensure_at_least Size Unit` ([`ensure_at_least`]).
    AtLeast(usize, usize),
    /// `ensure_exactly Stride` — `bs_test_tail_imm2`.
    Exactly(usize),
}

/// True when `atom` names `name`.
///
/// The fused recogniser compares names **directly**: `&str` equality compiles to a
/// length check plus an inline `memcmp`, whereas the general loop's classifier pays a
/// length-discriminated `match` (an indirect jump per command) because it must accept
/// seven names. Recognising a whole shape up front therefore costs two compares and
/// replaces two such dispatches.
#[inline]
fn atom_is(module: &Module, atom: beam_term::Atom, name: &str) -> bool {
    module.atom_name(atom) == Some(name)
}

/// True when a **literal** integer operand is one [`read_size`] would accept.
///
/// The fused shapes require literal sizes (what the compiler emits: `<<V:8>>` *is*
/// `Operand::Integer(8)`). A literal that `read_size` would reject — a negative value,
/// or a magnitude only a bignum holds — declines the shape instead of raising here, so
/// the general loop reports that error with its own wording and ordering.
#[inline]
fn sizeable(n: i64) -> bool {
    n >= 0 && beam_term::try_make_small(n).is_ok()
}

/// Run a fused, literal-operand command shape.
///
/// `guard` is the shape's leading test, `skip` an advance between the guard and the
/// reads (`0` = no `skip` command), and `reads` the `(flags, size, unit, dst)` integer
/// reads in order. Every step mirrors the general loop command for command: the guard's
/// failure is the instruction's fail path, `size * unit` overflow and a short subject
/// are fails (never errors), the advance is committed before the next read, each read
/// commits **before** binding its destination (the C advances `sb->start` first), and a
/// read that allocated re-derives the context capture before the next read uses it.
#[inline]
#[allow(clippy::too_many_arguments)]
fn fused_run(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    ctx_op: &Operand,
    guard: Guard,
    skip: usize,
    reads: &[(&Operand, usize, usize, &Operand)],
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let (mut term, orig, start, mut end) = read_ctx(machine, ctx, ctx_op, "bs_match")?;
    let (mut bytes, mut off) = raw::subject_bytes(orig)?;
    let mut pos = start;
    match guard {
        Guard::AtLeast(size, unit) => {
            if !ensure_at_least(end.saturating_sub(pos), size, unit) {
                return fail_step(machine, module, labels, fail, ctx_op, ctx);
            }
        }
        Guard::Exactly(stride) => {
            if end.saturating_sub(pos) != stride {
                return fail_step(machine, module, labels, fail, ctx_op, ctx);
            }
        }
    }
    if skip != 0 {
        let Some(next) = pos.checked_add(skip) else {
            return fail_step(machine, module, labels, fail, ctx_op, ctx);
        };
        if next > end {
            return fail_step(machine, module, labels, fail, ctx_op, ctx);
        }
        pos = next;
        commit(machine, ctx, ctx_op, term, false, pos)?;
    }
    // A read that built a bignum reserved through the rooted seam, so the capture may
    // have been relocated: the next read re-derives it first, exactly as the general
    // loop does at the top of each iteration (`stale`).
    let mut stale = false;
    for &(flags_op, size, unit, dst) in reads {
        if stale {
            let (fresh, fresh_orig, _, fresh_end) = read_ctx(machine, ctx, ctx_op, "bs_match")?;
            let (fresh_bytes, fresh_off) = raw::subject_bytes(fresh_orig)?;
            term = fresh;
            end = fresh_end;
            bytes = fresh_bytes;
            off = fresh_off;
            stale = false;
        }
        let flags = decode_flags(module, flags_op)?;
        let Some(nbits) = size.checked_mul(unit) else {
            return fail_step(machine, module, labels, fail, ctx_op, ctx);
        };
        if nbits > end.saturating_sub(pos) {
            return fail_step(machine, module, labels, fail, ctx_op, ctx);
        }
        let (value, allocated) =
            read_integer_field_tracked(machine, ctx, bytes, pos + off, nbits, flags)?;
        pos += nbits;
        commit(machine, ctx, ctx_op, term, allocated, pos)?;
        write_operand(machine, dst, value)?;
        stale = allocated;
    }
    Ok(Some(Step::Next))
}

/// Recognise and run the fused command shapes the standard compiler emits.
///
/// Every shape below is matched on the **whole** operand list, so the recognition is
/// one `commands.len()` switch plus a short pattern test — no per-command
/// `cmd_at` bounds check, no `read_size` operand dispatch (the sizes are destructured
/// straight out of the literal operands) and no re-derivation of the context capture.
/// The shapes are the ones a `beam_disasm` census of the installed OTP finds:
///
/// * `[ensure_at_least N U, integer L F S U2 Dst]` — a byte/binary comprehension loop
///   (`cowboy_bstr:'-to_lower/1-lbc$^0/2-0-'/2`, `emqx_base62:decode/2`, …);
/// * `[ensure_exactly N]` — a tail test (`emqx_base62:encode/2`, `capitalize_token/3`);
/// * `[ensure_exactly N, integer …]`, `[ensure_exactly N, skip K, integer …]` — the
///   fixed-width group reads of the EMQX base62 codec;
/// * `[ensure_at_least N U, integer …, integer …]` — `emqx_base62:encode/2`'s
///   three-byte group.
fn run_commands_fast(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    ctx_op: &Operand,
    commands: &[Operand],
    ctx: &mut dyn ExecutionContext,
) -> Result<Fast> {
    let step = match commands {
        // `ensure_exactly Stride` on its own.
        [Operand::Atom(c0), Operand::Integer(stride)]
            if sizeable(*stride) && atom_is(module, *c0, "ensure_exactly") =>
        {
            fused_run(
                machine,
                module,
                labels,
                fail,
                ctx_op,
                Guard::Exactly(*stride as usize),
                0,
                &[],
                ctx,
            )?
        }
        // `ensure_exactly Stride` then one `integer` read.
        [Operand::Atom(c0), Operand::Integer(stride), Operand::Atom(c1), _live, flags,
         Operand::Integer(size), Operand::Integer(unit), dst]
            if sizeable(*stride) && sizeable(*size) && sizeable(*unit)
                && atom_is(module, *c0, "ensure_exactly")
                && atom_is(module, *c1, "integer") =>
        {
            fused_run(
                machine,
                module,
                labels,
                fail,
                ctx_op,
                Guard::Exactly(*stride as usize),
                0,
                &[(flags, *size as usize, *unit as usize, dst)],
                ctx,
            )?
        }
        // `ensure_at_least Size Unit` then one `integer` read — the hot comprehension
        // loop shape.
        [Operand::Atom(c0), Operand::Integer(size0), Operand::Integer(unit0),
         Operand::Atom(c1), _live, flags, Operand::Integer(size), Operand::Integer(unit),
         dst]
            if sizeable(*size0) && sizeable(*unit0) && sizeable(*size) && sizeable(*unit)
                && atom_is(module, *c0, "ensure_at_least")
                && atom_is(module, *c1, "integer") =>
        {
            fused_run(
                machine,
                module,
                labels,
                fail,
                ctx_op,
                Guard::AtLeast(*size0 as usize, *unit0 as usize),
                0,
                &[(flags, *size as usize, *unit as usize, dst)],
                ctx,
            )?
        }
        // `ensure_exactly Stride`, `skip K`, then one `integer` read.
        [Operand::Atom(c0), Operand::Integer(stride), Operand::Atom(c1),
         Operand::Integer(skip), Operand::Atom(c2), _live, flags, Operand::Integer(size),
         Operand::Integer(unit), dst]
            if sizeable(*stride) && sizeable(*skip) && sizeable(*size) && sizeable(*unit)
                && atom_is(module, *c0, "ensure_exactly")
                && atom_is(module, *c1, "skip")
                && atom_is(module, *c2, "integer") =>
        {
            fused_run(
                machine,
                module,
                labels,
                fail,
                ctx_op,
                Guard::Exactly(*stride as usize),
                *skip as usize,
                &[(flags, *size as usize, *unit as usize, dst)],
                ctx,
            )?
        }
        // `ensure_at_least Size Unit` then two `integer` reads — the three-byte group
        // of `emqx_base62:encode/2`.
        [Operand::Atom(c0), Operand::Integer(size0), Operand::Integer(unit0),
         Operand::Atom(c1), _live1, flags1, Operand::Integer(size1), Operand::Integer(unit1),
         dst1, Operand::Atom(c2), _live2, flags2, Operand::Integer(size2),
         Operand::Integer(unit2), dst2]
            if sizeable(*size0) && sizeable(*unit0) && sizeable(*size1) && sizeable(*unit1)
                && sizeable(*size2) && sizeable(*unit2)
                && atom_is(module, *c0, "ensure_at_least")
                && atom_is(module, *c1, "integer")
                && atom_is(module, *c2, "integer") =>
        {
            fused_run(
                machine,
                module,
                labels,
                fail,
                ctx_op,
                Guard::AtLeast(*size0 as usize, *unit0 as usize),
                0,
                &[
                    (flags1, *size1 as usize, *unit1 as usize, dst1),
                    (flags2, *size2 as usize, *unit2 as usize, dst2),
                ],
                ctx,
            )?
        }
        _ => return Ok(Fast::NotMine),
    };
    Ok(Fast::Step(step))
}

/// The `bs_match` command interpreter: the reference route for every shape
/// [`run_commands_fast`] did not fully recognise.
///
/// Kept out of line (`#[inline(never)]`) so the fused recogniser stays small and
/// resident: a binary comprehension re-enters the family once per byte with a
/// two-command list, and a compiled match loop never reaches this function at all.
#[inline(never)]
fn run_commands_general(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    ctx_op: &Operand,
    commands: &[Operand],
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    // `pos` is a *local* cursor over the subject bits. Every command that advances it
    // mirrors the advance into the context's own position field straight away
    // ([`commit`]) — never in one batch after the loop: a command binds its destination
    // *after* the C has advanced `sb->start`, and that destination may be the very
    // operand the context lives in (`get_tail` in `bits/1`), so a trailing write-back
    // would have to re-read a register a command had already overwritten.
    //
    // The context term, its inner subject and the subject's byte view are captured
    // **once** here and re-derived only after a command that could have allocated
    // through the rooted seam (`stale`): `ensure_at_least`/`ensure_exactly`/`skip`/`=:=`
    // allocate nothing at all, and an `integer` command allocates only when its field
    // does not fit a small integer ([`read_integer_field_tracked`]'s flag), so in the
    // hot shape (`ensure_at_least N 1` then `integer … 8 1 Dst`) the whole command list
    // runs off one capture — no per-iteration operand read, no per-iteration
    // `subject_bytes`, and only one `commit` re-read at most. The default is the
    // re-read: a false `stale` is UB (a relocated block), a false `true` is one wasted
    // read.
    let (mut term, orig, start, mut end) = read_ctx(machine, ctx, ctx_op, "bs_match")?;
    let (mut bytes, mut off) = raw::subject_bytes(orig)?;
    let mut pos = start;
    // `true` when the previous command may have relocated the context block, so the
    // capture above is stale and must be re-derived before it is used again.
    let mut stale = false;
    let mut i = 0usize;
    while i < commands.len() {
        if stale {
            let (fresh, fresh_orig, _, fresh_end) = read_ctx(machine, ctx, ctx_op, "bs_match")?;
            let (fresh_bytes, fresh_off) = raw::subject_bytes(fresh_orig)?;
            term = fresh;
            end = fresh_end;
            bytes = fresh_bytes;
            off = fresh_off;
            stale = false;
        }
        let Operand::Atom(command) = &commands[i] else {
            return Err(error::unsupported(
                "bs_match: command list does not start with an atom",
            ));
        };
        let name = module
            .atom_name(*command)
            .ok_or_else(|| error::unsupported("bs_match: unknown command atom"))?;
        match name {
            // `ensure_at_least Size Unit` (`generators.tab:1164`).
            "ensure_at_least" => {
                let size = read_size(machine, ctx, cmd_at(commands, i + 1)?)?;
                let unit = read_size(machine, ctx, cmd_at(commands, i + 2)?)?;
                if !ensure_at_least(end.saturating_sub(pos), size, unit) {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
                i += 3;
            }
            // `ensure_exactly Stride` — `bs_test_tail_imm2` (`generators.tab:1210`).
            "ensure_exactly" => {
                let stride = read_size(machine, ctx, cmd_at(commands, i + 1)?)?;
                if end.saturating_sub(pos) != stride {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
                i += 2;
            }
            // `skip Stride` — advance without binding (`i_bs_skip`, which does
            // `sb->start += size`).
            "skip" => {
                match skip_cmd(machine, ctx, ctx_op, commands, i, term, &mut pos, end)? {
                    CmdFlow::Fail => return fail_step(machine, module, labels, fail, ctx_op, ctx),
                    CmdFlow::Done | CmdFlow::Relocated => {}
                }
                i += 2;
            }
            // `integer Live Flags Size Unit Dst` (`i_bs_get_fixed_integer`).
            "integer" => {
                let flags = decode_flags(module, cmd_at(commands, i + 2)?)?;
                let size = read_size(machine, ctx, cmd_at(commands, i + 3)?)?;
                let unit = read_size(machine, ctx, cmd_at(commands, i + 4)?)?;
                let dst = cmd_at(commands, i + 5)?;
                let Some(nbits) = size.checked_mul(unit) else {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                };
                if nbits > end.saturating_sub(pos) {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
                let (value, allocated) =
                    read_integer_field_tracked(machine, ctx, bytes, pos + off, nbits, flags)?;
                pos += nbits;
                // The C advances `sb->start` (inside `erts_bs_get_integer_2`) *before*
                // it binds `$Dst`, so the advance is mirrored first: `Dst` may be the
                // context's own operand.
                commit(machine, ctx, ctx_op, term, allocated, pos)?;
                write_operand(machine, dst, value)?;
                stale = allocated;
                i += 6;
            }
            // `binary Live Flags Size Unit Dst` (`i_bs_get_fixed_binary`).
            "binary" => {
                match binary_cmd(machine, module, ctx, commands, i, &mut pos, end, bytes, off, term, ctx_op)? {
                    CmdFlow::Fail => return fail_step(machine, module, labels, fail, ctx_op, ctx),
                    CmdFlow::Done => {}
                    // `raw::make_binary` reserves on **every** build.
                    CmdFlow::Relocated => stale = true,
                }
                i += 6;
            }
            // `get_tail Live Unit Dst` — bind the whole remaining bits
            // (`i_bs_get_tail`); the position is left unchanged (it is the last
            // command, `bs_translate_fixup`).
            "get_tail" => {
                match get_tail_cmd(machine, ctx, commands, i, pos, end, bytes, off)? {
                    CmdFlow::Fail => return fail_step(machine, module, labels, fail, ctx_op, ctx),
                    CmdFlow::Done => {}
                    CmdFlow::Relocated => stale = true,
                }
                i += 4;
            }
            // `'=:=' Nil Bits Value` (`i_bs_eq`, `bs_instrs.tab:1366`): the next
            // `Bits` bits must equal the literal unsigned `Value` exactly (no sign
            // extension). `Value` is emitted as a plain integer up to 64 bits.
            //
            // The atom *name* is `=:=` (the quotes `beam_disasm` prints around
            // `'=:='` are just the non-alphanumeric-atom notation and are not part
            // of the name).
            "=:=" => {
                match eq_cmd(machine, ctx, commands, i, &mut pos, end, bytes, off, term, ctx_op)? {
                    CmdFlow::Fail => return fail_step(machine, module, labels, fail, ctx_op, ctx),
                    CmdFlow::Done | CmdFlow::Relocated => {}
                }
                i += 4;
            }
            other => {
                return Err(error::unsupported(&format!("bs_match: command {other}")));
            }
        }
    }
    Ok(Some(Step::Next))
}

/// The outcome of one **outlined** `bs_match` command body.
///
/// The bodies that build a term ([`binary_cmd`], [`get_tail_cmd`]) are kept out of
/// [`run_commands`] as `#[inline(never)]` functions: they pull in
/// `raw::make_binary`/`extract_bits`, whose inlined form is several hundred bytes
/// each, and a compiled binary-comprehension loop re-enters `run_commands` once per
/// iteration with a two-command list (`ensure_at_least`, `integer`). Outlining the
/// cold bodies keeps that hot function small enough to stay resident instead of
/// faulting cold instruction lines in on every call; semantics are unchanged
/// because each body is still handed the same operands in the same order.
enum CmdFlow {
    /// The command completed without allocating through the rooted seam, so the
    /// caller's `(term, bytes, off, end)` capture is still live.
    Done,
    /// The command completed through an allocating build, so the context block may
    /// have been relocated: the caller must re-derive its capture before the next
    /// command reuses it.
    Relocated,
    /// The command's guard failed: the caller takes the instruction's fail path
    /// ([`fail_step`]).
    Fail,
}

/// `skip Stride` — advance without binding (`i_bs_skip`, `sb->start += size`).
#[inline(never)]
fn skip_cmd(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    ctx_op: &Operand,
    commands: &[Operand],
    i: usize,
    term: Eterm,
    pos: &mut usize,
    end: usize,
) -> Result<CmdFlow> {
    let stride = read_size(machine, ctx, cmd_at(commands, i + 1)?)?;
    let Some(next) = (*pos).checked_add(stride) else {
        return Ok(CmdFlow::Fail);
    };
    if next > end {
        return Ok(CmdFlow::Fail);
    }
    *pos = next;
    commit(machine, ctx, ctx_op, term, false, *pos)?;
    Ok(CmdFlow::Done)
}

/// `binary Live Flags Size Unit Dst` (`i_bs_get_fixed_binary`).
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn binary_cmd(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    commands: &[Operand],
    i: usize,
    pos: &mut usize,
    end: usize,
    bytes: &[u8],
    off: usize,
    term: Eterm,
    ctx_op: &Operand,
) -> Result<CmdFlow> {
    let size_op = cmd_at(commands, i + 3)?;
    let unit = read_size(machine, ctx, cmd_at(commands, i + 4)?)?;
    let dst = cmd_at(commands, i + 5)?;
    let nbits = if is_all(module, size_op) {
        end.saturating_sub(*pos)
    } else {
        let size = read_size(machine, ctx, size_op)?;
        match size.checked_mul(unit) {
            Some(nbits) => nbits,
            None => return Ok(CmdFlow::Fail),
        }
    };
    if nbits > end.saturating_sub(*pos) {
        return Ok(CmdFlow::Fail);
    }
    let out = extract_bits(bytes, *pos + off, nbits)?;
    // `i_bs_get_fixed_binary` builds the window through `erts_build_sub_bitstring`
    // for an arbitrary bit count (no 64-byte limit), so the result goes through
    // `raw::make_binary`, which picks the frozen on-heap/off-heap representation by
    // size.
    let value = raw::make_binary(machine, ctx, &out, nbits)?;
    *pos += nbits;
    // `i_bs_get_fixed_binary` does `sb->start += size;` *before* `$Dst = result;` —
    // mirror the advance first (`Dst` may be the context's own operand).
    // `raw::make_binary` reserves on **every** build, so the commit always re-reads.
    commit(machine, ctx, ctx_op, term, true, *pos)?;
    write_operand(machine, dst, value)?;
    Ok(CmdFlow::Relocated)
}

/// `get_tail Live Unit Dst` — bind the whole remaining bits (`i_bs_get_tail`).
///
/// The position is left unchanged (it is the last command, `bs_translate_fixup`).
#[inline(never)]
fn get_tail_cmd(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    commands: &[Operand],
    i: usize,
    pos: usize,
    end: usize,
    bytes: &[u8],
    off: usize,
) -> Result<CmdFlow> {
    let dst = cmd_at(commands, i + 3)?;
    let nbits = end.saturating_sub(pos);
    let out = extract_bits(bytes, pos + off, nbits)?;
    // `i_bs_get_tail` builds an arbitrary bit count (no 64-byte limit), so the
    // result goes through `raw::make_binary` (on-heap at/below
    // `ERL_ONHEAP_BINARY_LIMIT`, off-heap refc above).
    let value = raw::make_binary(machine, ctx, &out, nbits)?;
    write_operand(machine, dst, value)?;
    Ok(CmdFlow::Relocated)
}

/// `'=:=' Nil Bits Value` (`i_bs_eq`, `bs_instrs.tab:1366`).
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn eq_cmd(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    commands: &[Operand],
    i: usize,
    pos: &mut usize,
    end: usize,
    bytes: &[u8],
    off: usize,
    term: Eterm,
    ctx_op: &Operand,
) -> Result<CmdFlow> {
    let bits = read_size(machine, ctx, cmd_at(commands, i + 2)?)?;
    let want_term = read_operand(machine, ctx, cmd_at(commands, i + 3)?)?;
    let Some(want) = small_value(want_term) else {
        return Err(error::unsupported("bs_match '=:=': non-integer value"));
    };
    if bits > 64 || bits > end.saturating_sub(*pos) {
        return Ok(CmdFlow::Fail);
    }
    if read_uint_be(bytes, *pos + off, bits)? != want as u64 {
        return Ok(CmdFlow::Fail);
    }
    *pos += bits;
    // The C consumes `Bits` through the `i_bs_read_bits` that feeds `i_bs_eq`,
    // which advances `sb->start` by `Bits`.
    commit(machine, ctx, ctx_op, term, false, *pos)?;
    Ok(CmdFlow::Done)
}

/// Mirror the local cursor `pos` into the match context's own position field.
///
/// `live` is the context term this iteration captured. `stale` says the command that
/// just ran may have allocated through the rooted seam ([`read_integer_field_tracked`]'s
/// bignum arm -> [`raw::make_big`], [`raw::make_binary`]), whose moving collector may
/// have relocated the context block, so `live` is a stale pointer and the operand must
/// be re-read **here**, right before the write (the frozen reserve, then read, then
/// write ordering). When it cannot have allocated, `live` is reused — no operand
/// re-read, no [`raw::ctx_parts`]. The operand itself is part of the machine's root set
/// and is rewritten in place by the collector, so re-evaluating it yields the live term.
///
/// Every caller commits **before** binding a command's destination: the C advances
/// `sb->start` first (`i_bs_get_fixed_integer`/`i_bs_get_fixed_binary`/`i_bs_skip`/
/// `i_bs_read_bits`) and `bs_match`'s `get_tail` binds the tail into the context's own
/// register, so a destination binding can make the context operand unreadable.
fn commit(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    ctx_op: &Operand,
    live: Eterm,
    stale: bool,
    pos: usize,
) -> Result<()> {
    let term = if stale {
        reread_ctx(machine, ctx, ctx_op, "bs_match")?
    } else {
        live
    };
    raw::set_ctx_start(term, pos);
    Ok(())
}
/// The `i`-th command operand, or a typed error when the list is truncated.
pub(crate) fn cmd_at(commands: &[Operand], index: usize) -> Result<&Operand> {
    commands
        .get(index)
        .ok_or_else(|| error::unsupported("bs_match: truncated command list"))
}
/// The `ensure_at_least` test (`generators.tab:1164-1206`):
/// * `Size != 0`, `Unit == 1` ⇒ `i_bs_ensure_bits` (`remaining >= Size`);
/// * `Size != 0`, `Unit != 1` ⇒ `i_bs_ensure_bits_unit` (`remaining >= Size` **and**
///   the surplus is a whole number of units);
/// * `Size == 0`, `Unit == 8` ⇒ `i_bs_test_unit8` (byte alignment only);
/// * `Size == 0`, `Unit != 1` ⇒ `i_bs_test_unit` (alignment to `Unit`);
/// * `Size == 0`, `Unit == 1` ⇒ a redundant no-op.
pub(crate) fn ensure_at_least(remaining: usize, size: usize, unit: usize) -> bool {
    if size == 0 {
        if unit == 8 {
            remaining % 8 == 0
        } else if unit == 1 {
            true
        } else {
            remaining % unit == 0
        }
    } else if unit == 1 {
        remaining >= size
    } else {
        remaining >= size && (remaining - size) % unit == 0
    }
}

#[cfg(test)]
mod tests {
    //! Instruction-level tests for the `bs_match` command interpreter's result
    //! builders — in particular the `binary`/`get_tail` commands whose C bodies
    //! (`i_bs_get_fixed_binary`, `i_bs_get_tail`) build through
    //! `erts_build_sub_bitstring` for an **arbitrary** bit count.
    use super::*;

    use crate::context::MinimalContext;
    use crate::families::bitstrings::exec as family_exec;
    use crate::families::bitstrings::raw;
    use crate::terms::binary_bytes;
    use beam_loader::{Instr, NO_FAIL};

    /// Open a match context over the subject in `x(0)`, into `x(1)`.
    fn start_match(machine: &mut Machine, module: &Module, ctx: &mut MinimalContext) -> Step {
        family_exec(
            machine,
            module,
            &LabelMap::new(),
            &Instr::BsStartMatch3 {
                fail: NO_FAIL,
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            0,
            0,
            ctx,
        )
        .expect("bs_start_match3 reports no raise")
        .expect("the family claims bs_start_match3")
    }

    /// `binary … Size=all …` over a subject whose whole window is above
    /// `ERL_ONHEAP_BINARY_LIMIT` binds the off-heap refc form.
    ///
    /// The C `i_bs_get_fixed_binary` (`bs_instrs.tab:1447`) builds its window
    /// through `erts_build_sub_bitstring` for an arbitrary size (no 64-byte limit),
    /// so the command routes the result through `raw::make_binary`, which picks the
    /// frozen on-heap/off-heap representation by size; before that it was the typed
    /// `unsupported("bitstring: result binary > 64 bytes")`.
    #[test]
    fn bs_match_binary_above_the_heap_limit_builds_an_off_heap_refc_result() {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();

        let mut module = Module::default();
        let binary = Operand::Atom(module.atom_table.intern("binary"));
        let all = Operand::Atom(module.atom_table.intern("all"));

        let payload: Vec<u8> = (1..=70u8).collect();
        let subject = raw::make_binary(&mut machine, &mut ctx, &payload, payload.len() * 8)
            .expect("a 70-byte refc subject");
        assert!(raw::is_refc_binary(subject));
        machine.set_x(0, subject).expect("root the subject");
        assert_eq!(start_match(&mut machine, &module, &mut ctx), Step::Next);

        // `bs_match {f,0} {x,1} [binary, Live, Flags, all, Unit, {x,0}]`.
        let step = family_exec(
            &mut machine,
            &module,
            &LabelMap::new(),
            &Instr::BsMatch {
                fail: NO_FAIL,
                ctx: Operand::X(1),
                commands: vec![
                    binary,
                    Operand::Integer(2), // Live
                    Operand::Nil,        // Flags
                    all,                 // Size = all
                    Operand::Integer(1), // Unit
                    Operand::X(0),       // Dst
                ],
            },
            0,
            0,
            &mut ctx,
        )
        .expect("bs_match reports no raise")
        .expect("the family claims bs_match");
        assert_eq!(step, Step::Next);

        let result = machine.x(0).expect("the matched window");
        assert!(
            raw::is_refc_binary(result),
            "a 560-bit (70-byte) `all` window is above the 64-byte on-heap limit"
        );
        assert_eq!(
            raw::heap_bits(result).map(|(_, bits, _)| bits),
            Some(560),
            "`all` reads the whole 70-byte payload"
        );
        assert_eq!(
            binary_bytes(result).expect("a byte-aligned result"),
            payload.as_slice(),
            "the off-heap result reads back bit-exactly"
        );
    }
}