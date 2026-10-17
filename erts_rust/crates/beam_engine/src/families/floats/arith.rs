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

//! The float family's arithmetic: the four binary ops and `fnegate`.
//!
//! Ground truth: `i_f{add,sub,mul,div}` (`emu/float_instrs.tab:62-76`) and
//! `i_fnegate` (`:78-84`).
//!
//! `fadd`/`fsub`/`fmul`/`fdiv` share one body ([`binary`]): `Dst = op(Src1, Src2)`,
//! where a **non-float operand** or a **non-finite result** is the instruction's
//! arithmetic failure — the `fail`-label branch, or the `badarith` raise, both
//! owned by [`super::arithmetic_failure`].
//!
//! `fnegate` ([`negate`]) deliberately performs **no** finiteness check:
//! negating a finite float is always finite, and every producer of a float value
//! here rejects a non-finite result, so the only failure it can take is a
//! non-float source.

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Label, Module, Operand};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::write_operand;
use crate::terms::{float_value, make_float_rooted};
use crate::Step;

use super::{arithmetic_failure, read_value};

/// The shared body of `fadd`/`fsub`/`fmul`/`fdiv`: `dst = op(lhs, rhs)`.
///
/// A non-float operand or a non-finite result is the arithmetic failure handled by
/// [`arithmetic_failure`] (`i_f*` bodies, `emu/float_instrs.tab:54-76`).
#[allow(clippy::too_many_arguments)] // the family seam's fixed parameter list
pub(super) fn binary(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    lhs: &Operand,
    rhs: &Operand,
    dst: &Operand,
    ctx: &mut dyn ExecutionContext,
    op: fn(f64, f64) -> f64,
) -> Result<Option<Step>> {
    let a = read_value(machine, module, ctx, lhs)?;
    let b = read_value(machine, module, ctx, rhs)?;
    let (x, y) = match (float_value(a), float_value(b)) {
        (Ok(x), Ok(y)) => (x, y),
        // A non-float FR slot is the documented arithmetic failure. A structurally
        // malformed program is a different error and was already propagated by the
        // `?` above.
        _ => return arithmetic_failure(machine, module, labels, fail, ctx),
    };
    let result = op(x, y);
    if !result.is_finite() {
        return arithmetic_failure(machine, module, labels, fail, ctx);
    }
    let value = make_float_rooted(machine, ctx, result)?;
    write_operand(machine, dst, value)?;
    Ok(Some(Step::Next))
}

/// `fnegate Fail Src Dst` — `Dst = -Src`. `i_fnegate` deliberately performs
/// no finiteness check: negating a finite float is always finite (and every
/// producer of a float value here rejects a non-finite result).
pub(super) fn negate(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    src: &Operand,
    dst: &Operand,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let source = read_value(machine, module, ctx, src)?;
    let value = match float_value(source) {
        Ok(x) => x,
        Err(_) => return arithmetic_failure(machine, module, labels, fail, ctx),
    };
    let negated = make_float_rooted(machine, ctx, -value)?;
    write_operand(machine, dst, negated)?;
    Ok(Some(Step::Next))
}
