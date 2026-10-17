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

//! `bs_create_bin` **segment size / unit / failure-reason** helpers.
//!
//! The size-validation loop shared by the `integer`/`binary`/`float` segment arms
//! (`emu/bs_instrs.tab:420-445`): [`SegSize`] is the outcome of reading a dynamic
//! size operand, [`seg_nbits`]/[`float_nbits`] are the `$BS_SAFE_MUL` + signed-fit
//! checks, and [`SegReason`] plus [`fail_seg`]/[`seg_fail_step`]/[`raise_seg_fail`]
//! model the `$BS_FAIL_INFO` reason discrimination (the bare `badarg` vs
//! `system_limit` atom, branch vs raise). The writer arms live in [`super::segments`];
//! the segment loop and dispatch live in [`super`].

use crate::interpreter::LabelMap;

use beam_common::{BeamError, ExceptionClass, Result};
use beam_loader::{Label, Module, NO_FAIL, Operand};
use beam_term::{make_atom, Eterm};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::read_operand;
use crate::predicates;
use crate::Step;

use super::super::goto_label;

/// The reason a `bs_create_bin` segment failure (`$BS_FAIL_INFO`) raises — exactly
/// the atom C leaves in `c_p->freason`.
///
/// Every `i_bs_create_bin` call site reports the bare atom `badarg`
/// (`emu/bs_instrs.tab:347`, `BADARG`), **except** the size validation, which
/// reports `system_limit` via `BS_FAIL_INFO_SYSTEM_LIMIT` (`:355`): a `size*unit`
/// multiply overflow (`:424`), a product whose signed value does not fit a word
/// (`:427`), and a **positive** bignum size (`:438`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SegReason {
    /// The bare atom `badarg`.
    BadArg,
    /// The bare atom `system_limit`.
    SystemLimit,
}

impl SegReason {
    /// The BEAM atom name C leaves in `c_p->freason`.
    pub(super) fn atom_name(self) -> &'static str {
        match self {
            SegReason::BadArg => "badarg",
            SegReason::SystemLimit => "system_limit",
        }
    }

    /// The typed [`BeamError`] a raise falls back to when it cannot land — no live
    /// catch frame, or a reason atom the running module cannot name.
    pub(super) fn typed(self) -> BeamError {
        match self {
            SegReason::BadArg => BeamError::BadArg,
            SegReason::SystemLimit => BeamError::SystemLimit,
        }
    }
}

/// One bitstring segment's `size*unit` bit count, or the reason C reports.
///
/// The C size loop (`emu/bs_instrs.tab:424-428`) multiplies the (already validated,
/// non-negative) size by the unit under `$BS_SAFE_MUL` and then rejects a product
/// whose *signed* value does not fit in a word (`size >> (sizeof(Uint) * 8 - 1)`).
/// Both are `$BS_FAIL_INFO_SYSTEM_LIMIT`.
pub(super) fn seg_nbits(size: usize, unit: usize) -> std::result::Result<usize, SegReason> {
    match size.checked_mul(unit) {
        // `(nbits as u64) >> 63 != 0` is C's `size >> (sizeof(Uint) * 8 - 1) != 0`
        // on a 64-bit word: the signed size does not fit.
        Some(nbits) if (nbits as u64) >> 63 == 0 => Ok(nbits),
        _ => Err(SegReason::SystemLimit),
    }
}

/// The bit width a `float` segment builds, or the reason its failure reports.
///
/// C's `i_bs_create_bin` **first pass** (`emu/bs_instrs.tab:415-428`) shares the
/// `$BS_LOAD_UNIT`/`$BS_LOAD_SIZE`/`$BS_SAFE_MUL` arm with `BSC_INTEGER`/`BSC_BINARY`,
/// so a `float` segment's width is `size * unit` where a multiply overflow — or a
/// product whose *signed* value does not fit a word — is `$BS_FAIL_INFO_SYSTEM_LIMIT`
/// (see [`seg_nbits`]). `erts_bs_put_float` (`erl_bits.c:1341`) then accepts **only**
/// 16/32/64 bits and reports `c_p->fvalue = am_invalid` for every other width
/// (`:1449`), which the construct loop raises as the bare `badarg` (`:661-667`).
///
/// That width rejection includes `size == 0` — which `erlc` *does* emit, with only a
/// "bad float bit size" warning, for `<<F:0/float>>`:
///
/// ```text
/// $ erl -noshell -eval 'io:format("~p~n",[try <<1.0:0/float>> catch C:E -> {C,E} end]),halt().'
/// {error,badarg}
/// ```
///
/// so the reason ordering is `system_limit` (from [`seg_nbits`]) *before* the width
/// test, exactly as C's first pass runs before the construct phase. `Size == 0` never
/// reaches the width test with an overflowed product: `0 * unit` cannot overflow.
pub(super) fn float_nbits(size: usize, unit: usize) -> std::result::Result<usize, SegReason> {
    match seg_nbits(size, unit)? {
        nbits if matches!(nbits, 16 | 32 | 64) => Ok(nbits),
        _ => Err(SegReason::BadArg),
    }
}

/// A `bs_create_bin` **segment failure** (a completed `Err(step)`).
///
/// C's `$BS_FAIL_INFO($Fail, Reason, ...)` (`emu/bs_instrs.tab:347`) ends in
/// `$FAIL_HEAD_OR_BODY($Fail)` (`emu/macros.tab:266`): a live fail label is a branch
/// (`$FAIL`), and `{f,0}` ([`NO_FAIL`]) takes the `goto find_func_info` arm — a real
/// raise whose reason is the atom `BS_FAIL_INFO` left in `c_p->freason`, the bare
/// `badarg` for every call site in `i_bs_create_bin` except the size validation
/// (which reports `system_limit`; see [`SegReason`]).
///
/// This is why `bs_create_bin` must **not** reuse the matching side's
/// [`super::fail_step`], whose `NO_FAIL` arm raises `{badmatch, Subject}`: the
/// matching opcodes branch on `$FAIL` only and never go through `BS_FAIL_INFO`.
#[allow(clippy::type_complexity)]
pub(super) fn fail_seg(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    reason: SegReason,
) -> Result<std::result::Result<super::super::Built, Step>> {
    Ok(Err(seg_fail_step(machine, module, labels, fail, reason)?))
}

/// [`fail_seg`]'s `Step` half: branch, or raise the segment failure's reason atom.
fn seg_fail_step(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    reason: SegReason,
) -> Result<Step> {
    if fail == NO_FAIL {
        raise_seg_fail(machine, module, reason)
    } else {
        goto_label(labels, fail)
    }
}

/// Raise class `error` with the bare reason atom (C's `BADARG(Fail)` /
/// `SYSTEM_LIMIT(Fail)` at `emu/macros.tab:280` followed by `find_func_info`),
/// mirroring `families/funs.rs::raise_badarg`: the reason atom comes from the
/// *running module's* atom table (the engine never invents an atom it cannot name),
/// and the `error` class atom is installed when the module has it. With no live catch
/// frame — or a reason atom the module cannot name — the raise cannot land and
/// propagates as the typed [`SegReason::typed`] error (`BadArg` / `SystemLimit`,
/// whose `as_atom()` is exactly the atom the oracle raises).
///
/// C also records the `{Segment, Op, ErrorType, Value}` extended error info
/// (`erts_prepare_bs_construct_fail_info`, `emu/beam_emu.c:725`); that only
/// decorates the *stacktrace* and is not modelled at M1.
pub(super) fn raise_seg_fail(machine: &mut Machine, module: &Module, reason: SegReason) -> Result<Step> {
    if machine.catch_frame_depth() == 0 {
        return Err(reason.typed());
    }
    let atom = module
        .atom_table
        .lookup(reason.atom_name())
        .ok_or_else(|| reason.typed())?;
    if let Some(class_atom) = module.atom_table.lookup(ExceptionClass::Error.as_str()) {
        machine.set_class_atom(ExceptionClass::Error, make_atom(class_atom));
    }
    machine.throw_error(make_atom(atom))
}

/// The outcome of reading a *dynamic* segment size operand, mirroring the C size
/// loop's validation (`emu/bs_instrs.tab:420-445`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SegSize {
    /// A non-negative small integer (the C `signed_size >= 0` arm, `:423`).
    Small(usize),
    /// A segment failure carrying the reason atom C leaves in `c_p->freason`.
    Fail(SegReason),
}

/// Whether an integer term is negative (`big_sign` for a bignum).
///
/// [`beam_term::number_as_f64`] preserves a bignum's sign for every magnitude (the
/// nearest double when representable, else `±f64::INFINITY`), so the sign test is
/// exact; a small integer never reaches here (it takes the `is_small` arm).
fn integer_is_negative(value: Eterm) -> bool {
    beam_term::number_as_f64(value) < 0.0
}

/// Read a *dynamic* segment size operand, mirroring the C size loop's validation
/// (`emu/bs_instrs.tab:420-445`).
///
/// `SegSize::Small(n)` is a non-negative small integer; `SegSize::Fail(reason)` is
/// the `$BS_FAIL_INFO` reason the C reports — `badarg` for a negative size (`:431`),
/// a **negative** bignum (`:440`) or a non-integer (`:444`), and `system_limit` for
/// a **positive** bignum (`:438`).
pub(super) fn segment_size(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    operand: &Operand,
) -> Result<SegSize> {
    let value = read_operand(machine, ctx, operand)?;
    if beam_term::is_small(value) {
        let n = beam_term::signed_value(value);
        return Ok(if n >= 0 {
            SegSize::Small(n as usize)
        } else {
            SegSize::Fail(SegReason::BadArg)
        });
    }
    if predicates::is_integer(value) {
        // An integer too large to be a small: a positive bignum is `system_limit`
        // (`emu/bs_instrs.tab:438`), a negative one `badarg` (`:440`).
        return Ok(SegSize::Fail(if integer_is_negative(value) {
            SegReason::BadArg
        } else {
            SegReason::SystemLimit
        }));
    }
    // Not an integer (`:444`).
    Ok(SegSize::Fail(SegReason::BadArg))
}
