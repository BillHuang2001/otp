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

//! The float family's copy/convert instructions: `fmove` and `fconv`.
//!
//! Ground truth: `fload`/`fstore` (`emu/ops.tab:1337-1338`), `i_fconv`
//! (`emu/float_instrs.tab:38-52`) and `big_to_double`
//! (`../../erts/emulator/beam/big.c:2412`).
//!
//! `fmove` ([`fmove`]) copies a float between an FR slot and a value register /
//! literal. In the term-slot FR model (see the parent module docs) both directions
//! are a single copy.
//!
//! `fconv` ([`fconv`]) converts a small integer (C's `SMALL_TO_DOUBLE`), a float
//! (C's `LOAD_DOUBLE`, the value unchanged) or a bignum (the faithful
//! `big_to_double` through [`big_to_f64`]) into an FR slot. It carries **no** fail
//! label, so every failure — a non-numeric source, and a bignum that is not a
//! finite double — is the `$BADARITH0` raise via [`super::raise_badarith`].

use beam_common::Result;
use beam_loader::{Module, Operand};
use beam_term::number::big_to_f64;
use beam_term::{is_small, signed_value, Eterm};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::write_operand;
use crate::predicates;
use crate::terms::make_float_rooted;
use crate::Step;

use super::{raise_badarith, read_value};

/// The outcome of a `fconv` source conversion.
enum Conv {
    /// The boxed float term to store in the destination register.
    Value(Eterm),
    /// C's `i_fconv` conversion failure, which `$BADARITH0` turns into the
    /// `badarith` raise.
    BadArith,
}

/// `fmove Src Dst` — FR ⇄ value register (and a float *literal* source).
/// Both directions are a single copy in the term-slot FR model.
pub(super) fn fmove(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    src: &Operand,
    dst: &Operand,
) -> Result<Option<Step>> {
    let value = read_value(machine, module, ctx, src)?;
    write_operand(machine, dst, value)?;
    Ok(Some(Step::Next))
}

/// `fconv Src Dst` — small int / float / bignum → float register. No fail label.
pub(super) fn fconv(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    src: &Operand,
    dst: &Operand,
) -> Result<Option<Step>> {
    let source = read_value(machine, module, ctx, src)?;
    let value = match conv_to_float(machine, ctx, source)? {
        Conv::Value(value) => value,
        // A non-numeric source, and a bignum that is not a finite double, are
        // both the `$BADARITH0` raise; `fconv` has no fail label, so it goes
        // through the catch machinery.
        Conv::BadArith => return raise_badarith(machine, module).map(Some),
    };
    write_operand(machine, dst, value)?;
    Ok(Some(Step::Next))
}

/// `fconv`: a small integer, a float or a bignum → the boxed float for `Dst`.
///
/// `fconv` carries no fail label, so every failure is a raise
/// (`emu/float_instrs.tab:38-52` calls `$BADARITH0`). A small int is the C
/// `SMALL_TO_DOUBLE` path; a float is `LOAD_DOUBLE` (the value is unchanged); a
/// bignum is `big_to_double` (`../../erts/emulator/beam/big.c:2412`) via
/// [`beam_term::number::big_to_f64`], whose `None` is exactly C's `< 0` failure — a
/// value that is not a finite double. A non-number is the raise too.
fn conv_to_float(machine: &mut Machine, ctx: &mut dyn ExecutionContext, source: Eterm) -> Result<Conv> {
    if is_small(source) {
        return Ok(Conv::Value(make_float_rooted(machine, ctx, signed_value(source) as f64)?));
    }
    if predicates::is_float(source) {
        // Float → float is the C `LOAD_DOUBLE` path: the value is unchanged.
        return Ok(Conv::Value(source));
    }
    if predicates::is_number(source) {
        // A bignum: `big_to_double` is a faithful round-to-nearest (ties to even)
        // conversion; `None` is the C's `< 0` (the magnitude is too large for a
        // finite double), which `i_fconv` turns into `badarith`.
        return Ok(match big_to_f64(source) {
            Some(value) => Conv::Value(make_float_rooted(machine, ctx, value)?),
            None => Conv::BadArith,
        });
    }
    Ok(Conv::BadArith)
}
