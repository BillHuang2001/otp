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

//! Table-independent match-spec runner: `ets:match_spec_run/2`.
//!
//! The oracle's `ets:match_spec_run/2` takes a spec **already compiled** by
//! `ets:match_spec_compile/1` (passing a raw clause list is `badarg`, verified
//! against OTP 29 / erts-17.1). This crate has no separate compiled-spec *term*,
//! so [`match_spec_run`] accepts the **raw** spec term and compiles it internally
//! with [`super::compile`]; the `beam_bifs` bridge maps `ets:match_spec_compile/1`
//! onto the same compiled [`super::MatchSpec`].
//!
//! It runs the spec against each element of a list **in order**, keeping the body
//! value of every element whose first matching clause succeeds and dropping the
//! rest — the list analogue of `ets:select/2` (order preserved, unlike a table's
//! unspecified iteration order).
//!
//! `list` must be a proper list; anything else (an improper list, a non-list) is
//! `badarg`, exactly as the oracle raises. The head matches the **element itself**
//! (any term, not only a tuple); a repeated `'$N'` in the head enforces equality;
//! `'$_'` in a body is the whole element and `'$N'`/`'$$'` render the bindings; a
//! non-matching element (no clause matches, or every match's guard fails) is
//! dropped. Every behaviour was probed against the system `erl` (OTP 29 /
//! erts-17.1); see `tests/match_spec_run.rs`.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use crate::error::badarg;

/// `ets:match_spec_run(List, CompiledSpec)` — run a raw match-spec `spec` against
/// each element of the proper list `list`, in order, returning the body value of
/// every matching element (non-matching elements are dropped).
///
/// The oracle's `ets:match_spec_run/2` requires a spec already compiled by
/// `ets:match_spec_compile/1` (a raw clause list there is `badarg`, verified); this
/// crate has no compiled-spec *term*, so `spec` is the **raw** clause list and is
/// compiled here with [`super::compile`]. The `beam_bifs` bridge maps
/// `ets:match_spec_compile/1` onto the same compiled [`super::MatchSpec`].
///
/// Fresh tuples/lists built by a body are allocated on `heap`; a result that is
/// `'$_'`, a variable or a constant **aliases** the element or the caller's spec
/// storage (it is not copied). A body whose value is an internal boolean is
/// rendered as the atom `true`/`false`, which must be interned in `atoms`.
pub fn match_spec_run(
    list: Eterm,
    spec: Eterm,
    atoms: &AtomTable,
    heap: &mut Heap,
) -> Result<Vec<Eterm>> {
    let compiled = super::compile(spec, atoms)?;
    // A non-proper/non-list first argument is `badarg` (the oracle raises it too).
    let elements = super::list_elements(list).ok_or_else(badarg)?;
    let mut results = Vec::new();
    for element in elements {
        // The value of the first clause whose head matches and whose guards hold;
        // a clause whose guard raises is skipped, and a raising body propagates.
        if let Some(value) = super::exec::run_clauses(&compiled, element, atoms, heap)? {
            results.push(value.into_term(atoms)?);
        }
    }
    Ok(results)
}
