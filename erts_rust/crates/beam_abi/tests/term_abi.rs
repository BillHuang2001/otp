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

//! The term seam (compatibility contract A.3): `ERL_NIF_TERM` **is** the internal `Eterm`.
//!
//! Level-1 drop-in requires the NIF term and the internal BEAM term to be bit-identical: one
//! machine word, same size, same alignment. The [`eterm_from_nif`]/[`nif_from_eterm`] mapping must
//! therefore be a total identity on the raw word, checked here over the layout-critical boundary
//! values (`NIL`, small-int extremes, an atom tag, and an all-ones raw word).

use core::mem::{align_of, size_of};

use beam_abi::nif::term::{eterm_from_nif, nif_from_eterm, ERL_NIF_TERM, ERL_NIF_UINT};
use beam_term::{make_atom, make_small, Atom, Eterm, MAX_SMALL, MIN_SMALL, NIL};

#[test]
fn nif_term_is_one_machine_word() {
    assert_eq!(size_of::<ERL_NIF_TERM>(), size_of::<usize>());
    assert_eq!(size_of::<ERL_NIF_TERM>(), size_of::<Eterm>());
    assert_eq!(align_of::<ERL_NIF_TERM>(), align_of::<usize>());
    assert_eq!(align_of::<ERL_NIF_TERM>(), align_of::<Eterm>());
    // `ERL_NIF_UINT` is the header's alias of `ERL_NIF_TERM`.
    assert_eq!(size_of::<ERL_NIF_UINT>(), size_of::<usize>());
}

#[test]
fn seam_round_trips_over_boundary_terms() {
    let terms = [
        NIL,
        make_small(0),
        make_small(MAX_SMALL),
        make_small(MIN_SMALL),
        make_atom(Atom::from_raw(1234)),
        Eterm::from_raw(usize::MAX),
    ];

    for t in terms {
        let nif = nif_from_eterm(t);
        assert_eq!(nif.raw(), t.raw(), "nif_from_eterm preserves the raw word");
        assert_eq!(
            eterm_from_nif(nif),
            t,
            "eterm_from_nif(nif_from_eterm(t)) == t"
        );
        assert_eq!(
            nif_from_eterm(eterm_from_nif(nif)),
            nif,
            "nif_from_eterm(eterm_from_nif(t)) == t"
        );
    }
}

#[test]
fn default_is_nil_in_both_representations() {
    assert_eq!(ERL_NIF_TERM::default().raw(), Eterm::nil().raw());
    assert_eq!(nif_from_eterm(Eterm::nil()), ERL_NIF_TERM::default());
    assert_eq!(eterm_from_nif(ERL_NIF_TERM::default()), Eterm::nil());
}
