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

#![doc = "Frozen 64-bit tagged Erlang term representation: immediates, boxed terms, atoms and literal areas."]

//! The frozen term representation owned by this crate (workspace `CONTEXT.md` §2).
//!
//! Implements Part I §A.1–A.2 (language semantics and the `.beam` data model) and
//! the term-layout discussion of Part II §B: one machine word `Eterm` with a 2-bit
//! primary tag (`HEADER`/`LIST`/`BOXED`/`IMMED1`), a 3-bit pointer tag on 64-bit
//! (literal-area bit), 60-bit small integers, 6-bit-shifted atom immediates and
//! header-less 2-word cons cells tagged `TAG_PRIMARY_LIST`. Every other crate
//! depends downward on it for term operations.
//!
//! Ground truth for every constant and bit pattern is
//! `erts/emulator/beam/erl_term.h` (plus `erl_map.h`, `erl_bits.h`, `erl_fun.h`
//! for thing layouts and `erts/emulator/utils/make_tables` for atom ordering).
//!
//! Downstream code can `use beam_term::prelude::*;`.

pub mod atom;
pub mod bitstring;
pub mod boxed;
pub mod compare;
pub mod error;
pub mod eterm;
pub mod fun;
pub mod hashing;
pub mod heap;
pub mod ihash;
pub mod immediate;
pub mod list;
pub mod literal;
pub mod map;
pub mod number;
pub mod ops;
pub mod record;
pub mod refc;
pub mod size;
pub mod tag;
pub mod thing;
pub mod tuple;

/// Flat re-exports of every public item in this crate.
pub use crate::atom::*;
pub use crate::bitstring::*;
pub use crate::boxed::*;
pub use crate::compare::*;
pub use crate::error::*;
pub use crate::eterm::*;
pub use crate::fun::*;
pub use crate::hashing::*;
pub use crate::heap::*;
pub use crate::ihash::*;
pub use crate::immediate::*;
pub use crate::list::*;
pub use crate::literal::*;
pub use crate::map::*;
pub use crate::number::*;
pub use crate::ops::*;
pub use crate::record::*;
pub use crate::refc::*;
pub use crate::size::*;
pub use crate::tag::*;
pub use crate::thing::*;
pub use crate::tuple::*;

/// The conventional glob-import surface for downstream crates.
pub mod prelude {
    pub use crate::atom::*;
    pub use crate::bitstring::*;
    pub use crate::boxed::*;
    pub use crate::compare::*;
    pub use crate::error::*;
    pub use crate::eterm::*;
    pub use crate::fun::*;
    pub use crate::hashing::*;
    pub use crate::heap::*;
    pub use crate::ihash::*;
    pub use crate::immediate::*;
    pub use crate::list::*;
    pub use crate::literal::*;
    pub use crate::map::*;
    pub use crate::number::*;
    pub use crate::ops::*;
    pub use crate::record::*;
    pub use crate::refc::*;
    pub use crate::size::*;
    pub use crate::tag::*;
    pub use crate::thing::*;
    pub use crate::tuple::*;
}

#[cfg(test)]
mod tests {
    //! Smoke tests that the flat re-exports and the prelude resolve.

    #[test]
    fn flat_reexports_are_reachable() {
        use super::{is_boxed, is_list, is_small, make_small, Eterm, NIL};
        assert!(is_small(make_small(1)));
        assert!(!is_boxed(NIL));
        assert!(!is_list(NIL));
        assert_eq!(Eterm::default(), NIL);
    }

    #[test]
    fn prelude_resolves() {
        use crate::prelude::*;
        let t = make_small(42);
        assert_eq!(signed_value(t), 42);
        assert!(is_immediate(t));
        assert_eq!(AtomTable::new().len(), 0);
        let mut heap = Heap::new(4);
        assert!(heap.cons(t, NIL).is_ok());
    }
}
