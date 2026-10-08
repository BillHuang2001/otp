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

#![doc = "ETS tables: `set` over CHAMP or chained-bucket hash indexes, `ordered_set` over a B+-tree."]

//! ETS tables and their interior data structures.
//!
//! `beam_ets` owns the **observable semantics of `ets:*`** (Erlang Term Storage)
//! over flat, table-owned term storage — the A.1 copy boundary. It implements
//! Part I §A.1 (term/key/value semantics) and §A.8 (`ets:info/1` introspection),
//! plus the Stage-3 row of Part II §H: `set`, `ordered_set`, `bag` and
//! `duplicate_bag` over hash-indexed and ordered interiors.
//!
//! The interiors are: a **CHAMP** open-addressing index for `set`
//! ([`ChampInterior`], the default — a compressed bitmap HAMT over the key hash),
//! a **chained-bucket** hash index ([`HashInterior`]) for `bag`/`duplicate_bag`
//! (and, optionally, `set` via [`SetInterior::Chained`]), and a **K-ary B+-tree**
//! ([`OrderedSetInterior`]) for `ordered_set`.
//!
//! Per workspace `CONTEXT.md` §2 the interior shapes (bucket layout, hash-map
//! thresholds, tree fan-out, load factors) are *not* externally pinned; only the
//! **semantics** are contract — which object wins on a duplicate key, key
//! equality/ordering, `size`, and what `lookup`/`select`/`tab2list` return. Term
//! **ordering** and **hashing** are owned by [`beam_term`] (`compare::cmp`/`eq`,
//! `hashing::term_hash`) and MUST NOT be re-implemented here.
//!
//! Legacy ground truth (read-only): `../../erts/emulator/beam/erl_db.c` (API),
//! `erl_db_hash.c` (`set`/`bag`), `erl_db_tree.c` (`ordered_set`) and
//! `erl_db_util.c` (key/term handling, the match compiler). Mirror the
//! **semantics**, not the layouts.
//!
//! # Scope (M1)
//!
//! All module bodies are implemented: [`Table`] wire the metadata, the
//! table-owned [`TermStore`] and one interior behind the private `Interior`
//! trait. The authoritative API surface is `CONTEXT.md` at the crate root, which
//! also documents the honest M1 gaps (a `select`/deep-copy subset, no concurrency
//! or owner enforcement) under *Known Issues*.

pub mod champ_interior;
pub mod error;
pub mod hash_interior;
pub mod info;
pub mod key;
pub mod kind;
pub mod ordered_interior;
pub mod select;
pub mod store;
pub mod table;

#[cfg(test)]
mod interior_bench;
#[cfg(test)]
mod interior_parity;

// Flat re-exports of every public item in this crate. (`crate::error` exposes
// only `pub(crate)` helpers, so it is intentionally not re-exported here.)
pub use crate::champ_interior::*;
pub use crate::hash_interior::*;
pub use crate::info::*;
pub use crate::key::*;
pub use crate::kind::*;
pub use crate::ordered_interior::*;
pub use crate::select::*;
pub use crate::store::*;
pub use crate::table::*;

/// The conventional glob-import surface for downstream crates.
pub mod prelude {
    pub use crate::champ_interior::*;
    pub use crate::hash_interior::*;
    pub use crate::info::*;
    pub use crate::key::*;
    pub use crate::kind::*;
    pub use crate::ordered_interior::*;
    pub use crate::select::*;
    pub use crate::store::*;
    pub use crate::table::*;
}
