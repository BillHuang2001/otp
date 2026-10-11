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

//! **In-place term updates**: the mutating writers behind the destructive opcodes.
//!
//! Term construction lives in [`super::construct`] (a builder reserves a fresh block
//! and writes the whole node) and the multi-node shapes in [`super::sink`]; this
//! module is the third discipline: a writer that overwrites a word of an **existing**
//! term object, leaving its header and every other word untouched.
//!
//! A destructive opcode is only ever emitted by the compiler once it has proven the
//! term **uniquely owned** — that proof is what makes the write sound, and it is the
//! same assumption the C makes (`emu/instrs.tab:845-851`, `set_tuple_element`, which
//! writes through `tuple_val($Tuple)` under a plain `ASSERT(is_tuple($Tuple))`). This
//! module therefore keeps the reachable-in-Rust safety net the C does not need: the
//! target must be the right **shape** (a tuple, with the index inside its arity) and
//! must not be **shared** storage — a term in the module's immutable literal area
//! ([`beam_term::is_literal`]) is refused rather than corrupted. Every refusal is the
//! typed [`beam_common::BeamError`] the caller documents; never a wrong term, never a
//! write past the object, never UB.

use beam_common::{BeamError, Result};
use beam_term::Eterm;

use super::read::tuple_arity;

/// Overwrite `tuple`'s element word `index` **in place** with `value`.
///
/// This is the runtime half of the C `set_tuple_element s S P`
/// (`../../erts/emulator/beam/emu/instrs.tab:845-851`):
/// `*((Eterm *) ((unsigned char *) tuple_val($Tuple) + $Offset)) = $Element`. The C
/// loader pre-scales the file's `{u,Off}` operand to a byte offset
/// (`(Off + 1) * sizeof(Eterm)`, `emu/emu_load.c:1092-1094`), so the word written is
/// exactly the one `get_tuple_element` with the same index would read — element `i`
/// lives at `tuple_val(tuple)[1 + i]` (`beam_term::tuple`).
///
/// Returns [`BeamError::BadArg`] for a non-tuple or an index at/past the tuple's
/// arity, and the typed [`crate::error::unsupported`] for a term in a shared literal
/// area (whose storage outlives — and is shared by — every process, so writing it
/// would corrupt a module's constants). Both are defensive: the compiler emits this
/// opcode only for a uniquely-owned tuple of a known arity.
pub(crate) fn set_tuple_element(tuple: Eterm, index: usize, value: Eterm) -> Result<()> {
    // SAFETY: `tuple_arity` establishes `tuple` is a live tuple (it checks the boxed
    // tag before dereferencing the header word) and returns its arity.
    let arity = tuple_arity(tuple)?;
    if index >= arity {
        return Err(BeamError::BadArg);
    }
    // `is_literal` reads the literal-area bit of the word, never a pointer.
    if beam_term::is_literal(tuple) {
        return Err(crate::error::unsupported(
            "set_tuple_element: the tuple is in an immutable literal area",
        ));
    }
    // SAFETY: `tuple_arity` established that `tuple` is a live tuple of `arity`
    // elements and the two checks above established `index < arity` and that the
    // object is **not** in a shared literal area, so word `1 + index` is a writable
    // data word of an object the running code uniquely owns (the compiler's
    // uniqueness proof, which the C relies on identically). `1 + index <= arity`
    // words are inside the tuple's allocation, so the store is in bounds.
    unsafe { *beam_term::tuple_val(tuple).add(1 + index) = value };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MinimalContext;
    use crate::terms;
    use beam_term::{make_small, tuple0};

    /// Run `f` over one 3-tuple `{1, 2, 3}` living in a caller-owned arena.
    ///
    /// The context is passed in (rather than created inside) so it outlives `f`: the
    /// tuple's words live in its arena, which is freed when it is dropped.
    fn with_tuple<R>(f: impl FnOnce(&mut MinimalContext, Eterm) -> R) -> R {
        let mut ctx = MinimalContext::new();
        let tuple = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)])
            .expect("a 3-tuple");
        f(&mut ctx, tuple)
    }

    #[test]
    fn writes_the_indexed_word_and_leaves_the_rest_untouched() {
        with_tuple(|_ctx, tuple| {
            set_tuple_element(tuple, 1, make_small(20)).expect("write element 1");
            assert_eq!(terms::tuple_arity(tuple).unwrap(), 3);
            assert_eq!(terms::tuple_element(tuple, 0).unwrap(), make_small(1));
            assert_eq!(terms::tuple_element(tuple, 1).unwrap(), make_small(20));
            assert_eq!(terms::tuple_element(tuple, 2).unwrap(), make_small(3));
        });
    }

    #[test]
    fn index_zero_is_the_first_element_word() {
        // The raw index is the C loader's pre-scaling base: element `i` is word
        // `1 + i` past the header, so index 0 is the first **data** word — never the
        // header word that precedes it.
        with_tuple(|_ctx, tuple| {
            set_tuple_element(tuple, 0, make_small(10)).expect("write element 0");
            assert_eq!(terms::tuple_element(tuple, 0).unwrap(), make_small(10));
            // SAFETY: `tuple` is a live 3-tuple, so word 0 is its arityval header.
            let header = unsafe { *beam_term::tuple_val(tuple) };
            assert!(
                beam_term::is_arity_value(header),
                "the header word must survive the write"
            );
            assert_eq!(beam_term::arityval(header), 3);
        });
    }

    #[test]
    fn a_non_tuple_target_is_a_typed_error() {
        // `NIL`, a small integer and the shared zero-arity tuple (the default
        // `Module::default()`/`MinimalContext` empty tuple, which holds no element
        // word at all).
        for target in [beam_term::NIL, make_small(7), tuple0()] {
            assert!(
                matches!(
                    set_tuple_element(target, 0, make_small(1)),
                    Err(BeamError::BadArg)
                ),
                "{target:?} must not be writable"
            );
        }
    }

    #[test]
    fn an_out_of_range_index_is_a_typed_error() {
        with_tuple(|_ctx, tuple| {
            for index in [3usize, 4, usize::MAX] {
                assert!(
                    matches!(
                        set_tuple_element(tuple, index, make_small(9)),
                        Err(BeamError::BadArg)
                    ),
                    "index {index} is past the arity"
                );
            }
            // Nothing was written.
            assert_eq!(terms::tuple_element(tuple, 2).unwrap(), make_small(3));
        });
    }

    #[test]
    fn a_literal_area_target_is_refused() {
        with_tuple(|_ctx, tuple| {
            // The literal-area marker is the bit `beam_term::literal` sets on every
            // boxed word of a module's decoded constants.
            let literal = beam_term::literal::make_literal(tuple);
            assert!(beam_term::is_literal(literal));
            let error = set_tuple_element(literal, 0, make_small(99))
                .expect_err("a literal-area tuple is not writable");
            assert!(
                matches!(error, BeamError::Raise(_)),
                "expected the typed unsupported raise, got {error:?}"
            );
            // The tuple is unchanged.
            assert_eq!(terms::tuple_element(tuple, 0).unwrap(), make_small(1));
        });
    }
}
