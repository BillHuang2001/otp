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

//! The term seam — `ERL_NIF_TERM` and the safe `Eterm <=> ERL_NIF_TERM` mapping.
//!
//! `erl_nif.h` defines `ERL_NIF_TERM` as `ErlNifUInt` (`typedef ErlNifUInt ERL_NIF_TERM;`), i.e.
//! `unsigned long` on LP64 — exactly one machine word. The internal BEAM term
//! [`beam_term::Eterm`] is also one machine word (`#[repr(transparent)]` over `usize`), so at
//! **Level 1 (drop-in)** the NIF term *is* the internal term: compatibility contract **A.3**
//! requires the two to be bit-identical.
//!
//! The mapping is expressed with [`Eterm::raw`]/[`Eterm::from_raw`] — both `const fn`s — rather
//! than `mem::transmute`, so the identity is total, explicit and entirely safe.

/// The internal BEAM term, re-exported for NIF code that needs to build/inspect terms.
pub use beam_term::Eterm;

/// `ERL_NIF_TERM` — a NIF term, one machine word.
///
/// ABI-identical to `beam_term::Eterm` and to the C `ERL_NIF_TERM` (`unsigned long` on LP64).
/// The field is private: construct/inspect via [`ERL_NIF_TERM::raw`] /
/// [`ERL_NIF_TERM::from_raw`], or go through the typed seam [`eterm_from_nif`] /
/// [`nif_from_eterm`].
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ERL_NIF_TERM(usize);

impl ERL_NIF_TERM {
    /// The raw machine word, tags included.
    #[inline]
    pub const fn raw(self) -> usize {
        self.0
    }

    /// Wrap a raw machine word. The caller is responsible for the term tag bits.
    #[inline]
    pub const fn from_raw(raw: usize) -> Self {
        Self(raw)
    }
}

impl Default for ERL_NIF_TERM {
    /// The empty list `[]` (`NIL`) — BEAM's "blank register" value.
    #[inline]
    fn default() -> Self {
        ERL_NIF_TERM(Eterm::nil().raw())
    }
}

/// `ERL_NIF_UINT` — `erl_nif.h` aliases `ERL_NIF_TERM` to this name for "unsigned integer" term
/// positions (map-iterator indices and sizes, `ErlNifMapIterator::{size, idx}`).
pub type ERL_NIF_UINT = ERL_NIF_TERM;

/// NIF term → internal term.
///
/// Level-1 requirement (contract A.3): the internal term **is** the NIF term, so this is the
/// identity on the raw word — no boxing, no conversion table.
#[inline]
pub const fn eterm_from_nif(t: ERL_NIF_TERM) -> Eterm {
    Eterm::from_raw(t.raw())
}

/// Internal term → NIF term (the inverse of [`eterm_from_nif`]).
#[inline]
pub const fn nif_from_eterm(t: Eterm) -> ERL_NIF_TERM {
    ERL_NIF_TERM::from_raw(t.raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_is_one_machine_word() {
        assert_eq!(
            core::mem::size_of::<ERL_NIF_TERM>(),
            core::mem::size_of::<usize>()
        );
        assert_eq!(
            core::mem::size_of::<ERL_NIF_TERM>(),
            core::mem::size_of::<Eterm>()
        );
        assert_eq!(
            core::mem::align_of::<ERL_NIF_TERM>(),
            core::mem::align_of::<Eterm>()
        );
        assert_eq!(core::mem::size_of::<ERL_NIF_UINT>(), core::mem::size_of::<usize>());
    }

    #[test]
    fn seam_round_trips_over_boundary_values() {
        let values = [
            Eterm::nil().raw(),
            Eterm::default().raw(),
            0,
            0x3B,
            usize::MAX,
            (1usize << 59) - 1, // max small integer
        ];
        for raw in values {
            let nif = ERL_NIF_TERM::from_raw(raw);
            assert_eq!(nif.raw(), raw);
            assert_eq!(nif_from_eterm(eterm_from_nif(nif)), nif);
            assert_eq!(eterm_from_nif(nif).raw(), raw);
        }
    }

    #[test]
    fn default_is_nil() {
        assert_eq!(ERL_NIF_TERM::default().raw(), Eterm::nil().raw());
        assert_eq!(nif_from_eterm(Eterm::nil()), ERL_NIF_TERM::default());
    }
}
