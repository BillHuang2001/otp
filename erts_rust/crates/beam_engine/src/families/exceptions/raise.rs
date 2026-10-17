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

//! Raise helpers of the exception family: the reason-term constructors, the
//! class-atom installation and the class classification [`super::exec`] and its
//! opcode bodies share.
//!
//! Split out of [`super`] (the family's `exec` dispatch) by responsibility; the C
//! ground truth and every edge case are documented on each item below and in
//! [`super`]'s module docs.

use beam_common::{ExceptionClass, Result};
use beam_loader::{Module, Operand};
use beam_term::{make_atom, Eterm};

use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::machine::{namespace_atom, Machine};
use crate::operand::{operand_slack_words, read_operand_slack};
use crate::terms;
use crate::Step;

// ---------------------------------------------------------------------------
// Reason construction + class installation
// ---------------------------------------------------------------------------

/// Raise a `{Tag,Value}` `error` exception (the `badmatch`/`case_end`/
/// `try_case_end`/`badrecord` expansion).
///
/// The tag atom is resolved through [`namespace_atom`] — the host's live atom
/// namespace ([`ExecutionContext::atoms`]) first, the running module's load-time
/// snapshot second — so a host that owns ONE shared namespace can name a tag the
/// module's snapshot lacks. A name **neither** source knows is a typed
/// `unsupported` (never a wrong atom). The `error` class atom is installed the same
/// way (see [`install_class_atom`]); an uncaught raise therefore still propagates as
/// a proper `Err(BeamError::Raise(..))`.
pub(super) fn raise_error_tuple(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    tag: &str,
    value: &Operand,
) -> Result<Option<Step>> {
    let tag_atom = namespace_atom(ctx, module, tag)
        .ok_or_else(|| error::unsupported(&format!("exceptions: no '{tag}' atom in the module")))?;
    // Reserve the reason block first, then re-read the value operand from the
    // post-collection machine state (a moving collector may run in the reserve).
    let extra = operand_slack_words(value);
    let reason = terms::make_tuple_rooted(machine, ctx, RootSet::ALL_LIVE, 2, extra, |m, s| {
        Ok(vec![make_atom(tag_atom), read_operand_slack(m, value, s)?])
    })?;
    let _ = install_class_atom(machine, module, ctx, ExceptionClass::Error);
    machine.throw_error(reason).map(Some)
}

/// Raise a bare-atom `error` exception (`if_end` ⇒ the atom `if_clause`).
pub(super) fn raise_error_atom(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    tag: &str,
) -> Result<Option<Step>> {
    let atom = namespace_atom(ctx, module, tag)
        .ok_or_else(|| error::unsupported(&format!("exceptions: no '{tag}' atom in the module")))?;
    let reason = make_atom(atom);
    let _ = install_class_atom(machine, module, ctx, ExceptionClass::Error);
    machine.throw_error(reason).map(Some)
}

/// Install `class`'s atom from the module atom table into the machine.
///
/// Returns the atom term on success; when the atom is absent from the module
/// atom table the machine keeps its previous (or unset) value and a typed
/// `unsupported` is returned. Callers that must not spend the error (a raise
/// landing decides lazily) ignore it; a `throw` with no installed atom then fails
/// with the same typed error at landing time, rather than substituting a
/// different, wrong atom.
pub(super) fn install_class_atom(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    class: ExceptionClass,
) -> Result<Eterm> {
    match namespace_atom(ctx, module, class.as_str()) {
        Some(atom) => {
            let term = make_atom(atom);
            machine.set_class_atom(class, term);
            Ok(term)
        }
        None => Err(error::unsupported(&format!(
            "exceptions: no '{}' atom for the exception class in the module",
            class.as_str()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Class classification (`raw_raise`'s class protocol)
// ---------------------------------------------------------------------------

/// The BEAM exception `class` the atom `class_term` names, if any.
///
/// C compares against its global `am_error`/`am_exit`/`am_throw`
/// (`../../erts/emulator/beam/beam_common.c:1161-1176`); the engine's atoms are
/// module-local, so the names are resolved through the running module's atom
/// table. Any other term — including a non-atom — is *not* a class, which is
/// exactly what makes `raw_raise` take its `badarg` fall-through.
pub(super) fn classify_class(module: &Module, class_term: Eterm) -> Option<ExceptionClass> {
    [
        ExceptionClass::Error,
        ExceptionClass::Exit,
        ExceptionClass::Throw,
    ]
    .into_iter()
    .find(|class| module.atom_table.lookup(class.as_str()).map(make_atom) == Some(class_term))
}
