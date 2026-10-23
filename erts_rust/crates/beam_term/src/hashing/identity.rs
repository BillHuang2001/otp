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

//! The one **fun-identity** seam the two compatibility-pinned hash ports need.
//!
//! `make_hash`/`make_hash2` mix four fields of a `make_fun3` (`FunKind::Local`)
//! closure (`erl_term_hashing.c:242-261` and `:1224-1246`):
//!
//! | C field | where it comes from here |
//! |---|---|
//! | `fe->index` | the term's own `index` descriptor word ([`crate::fun::fun_index`]) |
//! | `fe->old_uniq` | the term's own `uniq` descriptor word ([`crate::fun::fun_uniq`]) |
//! | `fun_num_free(funp)` | the fun header ([`crate::fun::fun_num_free`]) |
//! | `fun_tab[atom_val(fe->module)]->slot.bucket.hvalue` | **not in the term** — [`LocalFunIdentity`] |
//!
//! The C keeps an out-of-line `ErlFunEntry` behind the term's `entry` pointer, so it
//! can read `fe->module` back out; this tree inlines only `uniq`/`index` into the
//! term ([`crate::fun`]'s documented M1 substitution), so the defining module — and
//! therefore its atom-table `hvalue` — has to be supplied by whoever knows which
//! module the closure was compiled in. That is exactly what the runtime's
//! `LocalFunIdentity` provider already resolves (the same seam that makes a compiled
//! module's local fun `term_to_binary` byte-faithful), so a host can serve this
//! without inventing anything.
//!
//! The parameter is **optional and defaulted to `None`** on both ports: with no
//! provider a `Local` fun keeps answering
//! [`TermError::FunIdentityUnavailable`](crate::TermError::FunIdentityUnavailable),
//! never a guessed number. Every other term shape — including the
//! `FunKind::Imported` (`NEW_FUN_EXT`) fun, whose descriptor carries all four
//! fields — hashes exactly as it did before a provider existed.

use crate::atom::Atom;
use crate::error::TermResult;
use crate::eterm::Eterm;

/// Supplies the **defining module** of a `make_fun3` (`FunKind::Local`) closure —
/// the `ErlFunEntry.module` the C's local-fun hash arms mix in as
/// `atom_tab(atom_val(fe->module))->slot.bucket.hvalue`
/// (`erl_term_hashing.c:249` / `:1232`).
///
/// A provider is asked **once per `FunKind::Local` fun encountered**, so it must
/// answer for any local fun of the walk (a hash is structural: a closure nested
/// inside a tuple, a list or a map is reached by the same walk).
pub trait LocalFunIdentity {
    /// The module atom of the local fun `term`, or `Ok(None)` when this caller
    /// cannot attribute the term to a module.
    ///
    /// `term` is guaranteed to be a boxed `FunKind::Local` fun term that the caller
    /// is hashing, so an implementation may read its descriptor. The returned
    /// [`Atom`] must index the **same** [`AtomTable`](crate::AtomTable) the hash is
    /// given — that table is what turns the atom into the mixed `hvalue`.
    ///
    /// `Ok(None)` is the honest "I do not know" answer: the hash then reports
    /// [`TermError::FunIdentityUnavailable`](crate::TermError::FunIdentityUnavailable)
    /// rather than a value derived from a substitute.
    ///
    /// # Errors
    ///
    /// Propagates the provider's own failure.
    fn local_fun_module(&mut self, term: Eterm) -> TermResult<Option<Atom>>;
}

/// Asks `identity` (when present) for `term`'s defining module.
///
/// The `Option` is taken by `&mut` because a provider is a single borrowed value the
/// whole walk threads through; reborrowing it per fun is what lets one provider
/// answer for every local fun a structural hash meets.
pub(crate) fn local_fun_module(
    identity: &mut Option<&mut dyn LocalFunIdentity>,
    term: Eterm,
) -> TermResult<Option<Atom>> {
    match identity {
        Some(provider) => provider.local_fun_module(term),
        None => Ok(None),
    }
}

/// Reborrows `identity` for a shorter lifetime, so one provider can be handed to
/// several calls (or loop iterations) in turn.
///
/// `Option<&mut dyn _>` is invariant in its lifetime, so it cannot be copied out of
/// the `&mut` the walk owns; `&mut **provider` is the explicit shorter-lived reborrow
/// (which is also what a method call through the `&mut &mut` does implicitly).
pub(crate) fn reborrow<'p>(
    identity: &'p mut Option<&mut dyn LocalFunIdentity>,
) -> Option<&'p mut dyn LocalFunIdentity> {
    match identity {
        Some(provider) => Some(&mut **provider),
        None => None,
    }
}

/// Provider doubles the in-crate tests of the two hash ports share.
///
/// One definition, not one per test module: both ports must exercise the same three
/// answers (a module, "unknown", a hard failure), and a pair of copies would drift.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::atom::Atom;
    use crate::error::{TermError, TermResult};
    use crate::eterm::Eterm;

    use super::LocalFunIdentity;

    /// Answers a fixed defining module for every local fun — what a runtime that
    /// resolves the closure's module from its loaded `FunT` table does.
    pub(crate) struct FixedModule(pub(crate) Atom);

    impl LocalFunIdentity for FixedModule {
        fn local_fun_module(&mut self, _term: Eterm) -> TermResult<Option<Atom>> {
            Ok(Some(self.0))
        }
    }

    /// The "I cannot attribute this closure" answer, which must stay the typed
    /// refusal rather than a value mixed from a substitute.
    pub(crate) struct Unknown;

    impl LocalFunIdentity for Unknown {
        fn local_fun_module(&mut self, _term: Eterm) -> TermResult<Option<Atom>> {
            Ok(None)
        }
    }

    /// A provider that fails outright. The variant is arbitrary — the property under
    /// test is that the provider's own failure surfaces **unchanged** instead of being
    /// collapsed into [`TermError::FunIdentityUnavailable`](TermError::FunIdentityUnavailable).
    pub(crate) struct Failing;

    impl LocalFunIdentity for Failing {
        fn local_fun_module(&mut self, _term: Eterm) -> TermResult<Option<Atom>> {
            Err(TermError::HeapExhausted)
        }
    }
}
