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

//! The record family's **reason-carrying** failure path.
//!
//! The native-record opcodes reach their fail label in two C shapes, and the
//! difference is observable when the fail label is [`NO_FAIL`] (`0`):
//!
//! * `$FAIL($Fail)` (`emu/macros.tab:56`) — used by the *guards*
//!   (`is_any_native_record`, `i_is_native_record`, `i_get_record_elements`,
//!   `i_is_record_accessible`/`i_is_wildcard_record_accessible`). **No** reason is
//!   set, so a branchless failure reaches the C's `find_func_info`
//!   (`emu/beam_emu.c:526-532`) with the reason still `EXC_FUNCTION_CLAUSE`: the
//!   bare atom `function_clause`. That is [`super::fail_step`].
//! * `$FAIL_HEAD_OR_BODY($Fail)` (`emu/macros.tab:266-278`) — taken by
//!   `i_update_native_record`, `i_get_local_record_field` and
//!   `i_get_record_field` **after** their helper set `p->fvalue`/`p->freason`
//!   (`erl_record.c:746-756`, `:790-821`, `:832-879`). On `Fail == 0` the C still
//!   reaches `handle_error` with that reason, and `expand_error_value`
//!   (`beam_common.c:745-768`) wraps it with the class-`error` atom
//!   `error_atom[r]`: `{badfield, Value}` / `{badrecord, Value}`. That is
//!   [`fail_step_with_reason`].
//!
//! Both atoms (`badfield`/`badrecord` and the `error` class) are **global**
//! `error_atom[…]` cells in C (`beam_common.c:754-768`), so they exist in every
//! universe; here they are resolved through the engine's live atom namespace with
//! the module's load-time snapshot as fallback ([`crate::machine::namespace_atom`]).
//! A name **neither** source can spell is the honest typed
//! [`crate::error::unsupported`] — never a substituted, wrong atom.
//!
//! Every build follows the frozen **reserve → read → write** ordering: a reason
//! that carries a heap term (`{badrecord, Src}`) reserves first and re-reads its
//! operand inside the reader closure, and a reason built from immediates alone
//! (`{Tag, {{Module, Name}, Field}}`) is one rooted reservation
//! ([`raw::write_field_failure_reason`]).

use beam_common::{ExceptionClass, Result};
use beam_loader::{Label, Module, Operand, NO_FAIL};
use beam_term::{make_atom, Atom, Eterm};

use super::{fail_step, raw};
use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::interpreter::LabelMap;
use crate::machine::{namespace_atom, Machine};
use crate::operand::{operand_slack_words, read_operand_slack};
use crate::terms;
use crate::Step;

/// The atom `badfield` (`am_badfield`, `EXC_BADFIELD`, `error.h:172` /
/// `error_atom[21]`, `atom.names:127`).
pub(super) const BADFIELD: &str = "badfield";

/// The atom `badrecord` (`am_badrecord`, `EXC_BADRECORD`, `error.h:171` /
/// `error_atom[20]`).
pub(super) const BADRECORD: &str = "badrecord";

/// The record failures whose C helper sets an exception **reason** before taking
/// `$FAIL_HEAD_OR_BODY($Fail)`.
pub(super) enum RecordReason<'a> {
    /// `p->fvalue = src; p->freason = EXC_BADRECORD` (`erl_record.c:790-792`,
    /// `:832-834`) ⇒ the class-`error` reason **`{badrecord, Src}`**, where `Src`
    /// is the instruction's **source** operand — the offending term itself, whether
    /// it is a non-record or a record of the wrong identity (and, for the create
    /// form, the `Id` operand, `erl_record.c:407-409`).
    BadRecord {
        /// The source (or create `Id`) operand; it is re-read **after** the
        /// reason's reservation, because a moving collector may relocate it.
        src: &'a Operand,
    },
    /// `p->fvalue = TUPLE2(TUPLE2(defp->module, defp->name), field);
    /// p->freason = EXC_BADFIELD` (`erl_record.c:746-756`, `:814-821`, `:872-879`;
    /// the create form's `:339-351`) ⇒ the class-`error` reason
    /// **`{Tag, {{Module, Name}, Field}}`**.
    ///
    /// `tag` is the reason's own tag name — `badfield` for every opcode here, and
    /// `badfield`/`novalue` for the create form's two delayed-field errors
    /// (`EXC_BADFIELD`/`EXC_NOVALUE`, `error_atom[21]`/`[22]`).
    BadField {
        /// The reason tag's atom **name** (`"badfield"`/`"novalue"`).
        tag: &'a str,
        /// The definition's `module` (`defp->module`).
        owner: Atom,
        /// The definition's `name` (`defp->name`).
        name: Atom,
        /// The offending field's key (`new_p[0]`, or the requested field name).
        field: Atom,
    },
}

/// The `$FAIL_HEAD_OR_BODY($Fail)` decision, carrying the C's exception reason.
///
/// A live `fail` label is the C's branch — the reason the helper set is discarded
/// and the step is exactly [`fail_step`]'s. [`NO_FAIL`] is the C's
/// `goto find_func_info` **with** `p->fvalue`/`p->freason` still set, so the raise
/// is class `error` with the reason `expand_error_value` builds.
pub(super) fn fail_step_with_reason(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    instruction: &str,
    reason: RecordReason<'_>,
    ctx: &mut dyn ExecutionContext,
) -> Result<Step> {
    if fail != NO_FAIL {
        return fail_step(machine, module, labels, fail, instruction, ctx);
    }
    // The reason and class atoms are resolved **first**: `atoms` borrows `ctx`
    // immutably, so it must not straddle the reason's `&mut ctx` reservation.
    match reason {
        RecordReason::BadRecord { src } => {
            let tag = reason_atom(ctx, module, BADRECORD, instruction)?;
            // Reserve the 3-word tuple first, then re-read the source operand: a
            // moving collector may run inside the reserve, so `Src` is taken from the
            // machine's post-collection state.
            let extra = operand_slack_words(src);
            let reason =
                terms::make_tuple_rooted(machine, ctx, RootSet::ALL_LIVE, 2, extra, |m, slack| {
                    Ok(vec![make_atom(tag), read_operand_slack(m, src, slack)?])
                })?;
            raise_error(machine, module, ctx, reason)
        }
        RecordReason::BadField {
            tag,
            owner,
            name,
            field,
        } => {
            let tag = reason_atom(ctx, module, tag, instruction)?;
            let reason = raw::write_field_failure_reason(machine, ctx, owner, name, field, tag)?;
            raise_error(machine, module, ctx, reason)
        }
    }
}

/// Raise `reason` as class `error` — the C's `handle_error` after
/// `expand_error_value` (`beam_common.c:745-768`).
///
/// The class atom is installed on the machine first (so a caught landing writes the
/// `error` atom into `x(3)`); a namespace that cannot name it leaves it unresolved,
/// which [`Machine::throw_error`] reports as a typed error once a raise actually
/// lands. An **uncaught** raise needs no class atom at all.
fn raise_error(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    reason: Eterm,
) -> Result<Step> {
    if let Some(class_atom) = namespace_atom(ctx, module, ExceptionClass::Error.as_str()) {
        machine.set_class_atom(ExceptionClass::Error, make_atom(class_atom));
    }
    machine.throw_error(reason)
}

/// Resolve a reason tag **name** through the live atom namespace
/// ([`crate::machine::namespace_atom`]), or the typed [`crate::error::unsupported`]
/// when neither the namespace nor the running module's snapshot can spell it.
fn reason_atom(
    ctx: &dyn ExecutionContext,
    module: &Module,
    name: &str,
    instruction: &str,
) -> Result<Atom> {
    namespace_atom(ctx, module, name).ok_or_else(|| {
        error::unsupported(&format!("{instruction}: no '{name}' atom in the module"))
    })
}
