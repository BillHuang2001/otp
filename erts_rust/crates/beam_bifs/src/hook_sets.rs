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

//! The curated "hook-needed" BIF sets: exactly which registered BIFs read the
//! optional [`MemoryHook`] and [`LocalFunIdentity`] seams.
//!
//! # What these sets are
//!
//! A host keeps its optional `beam_bifs` seams behind *gates*: for the curried
//! `(module, function, arity)` of a BIF call it decides whether the seam has to be
//! built and installed on the [`BifContext`] before it dispatches. Two of those
//! seams have a tiny, fixed consumer set, so the gate is a list membership test:
//!
//! * the [`MemoryHook`] seam — the `erlang:memory/0,1` accounting pair, whose
//!   `{Key, Bytes}` pairs only the runtime knows;
//! * the [`LocalFunIdentity`] seam — the external-term encoders
//!   (`erlang:term_to_binary/1,2`, `erlang:external_size/1,2`), which must install
//!   a local (`make_fun3`) fun's `NEW_FUN_EXT` identity on the encoder before it
//!   can write the term.
//!
//! # Why the sets live **here**, not in the host
//!
//! The seam *consumers* are BIF implementations inside this crate: `ctx.memory()`
//! is read only by `runtime::memory`, and `ctx.local_fun_identity()` only by
//! `binconv::encode_body` (reached from both the `term_to_binary` and
//! `external_size` encoders). If a host re-derived these sets
//! privately it would silently **drift** the moment this crate changed which BIFs
//! read a seam, and the drift is invisible in both directions:
//!
//! * a **too-narrow** gate leaves a BIF that *does* need the seam with the typed
//!   `unsupported_bif` / `Ok(None)` capability gap — a working BIF turned into a
//!   failure;
//! * a **too-wide** gate builds an expensive host bridge on BIFs that never touch
//!   the seam.
//!
//! Keeping the one authoritative list next to the call sites makes the dependency
//! explicit and testable: the tests below pin the exact contents and assert every
//! listed key is really registered, so renaming or removing a BIF on the registry
//! side fails loudly here rather than degrading at runtime.
//!
//! # Adoption (host crates)
//!
//! A host that currently re-derives these gates should delegate to the predicates
//! here instead of carrying a private copy:
//!
//! * `RuntimeContext::uses_memory_hook(m, f, a)` → [`needs_memory_hook`]
//! * `RuntimeContext::uses_fun_identity(m, f, a)` → [`needs_local_fun_identity`]
//!
//! # The cost asymmetries that make the gates load-bearing
//!
//! These predicates exist because *eagerly* building either bridge is expensive, so
//! it must not happen on the ordinary BIF dispatch path:
//!
//! * the **memory bridge** folds a `.beam` size per loaded module — one `stat`
//!   syscall each — so building it eagerly costs on the order of ~10 µs **per call**;
//! * the **fun-identity bridge** clones an `Arc` and allocates a `PathBuf`, so it too
//!   is a real allocation on every call if installed unconditionally.
//!
//! Both predicates are allocation-free `binary_search` membership tests over the
//! `&'static` tables, so calling them on every dispatch is free.

/// The registered BIFs that read the [`MemoryHook`] seam — the only consumers of
/// [`BifContext::memory`].
///
/// Sorted by `(module, function, arity)`; see [`needs_memory_hook`].
pub const MEMORY_HOOK_BIFS: &[(&str, &str, u32)] = &[
    ("erlang", "memory", 0),
    ("erlang", "memory", 1),
];

/// The registered BIFs that read the [`LocalFunIdentity`] seam — the only callers
/// reaching `binconv::encode_body`, which installs a local fun's `NEW_FUN_EXT`
/// identity on the external-term encoder.
///
/// `erlang:term_to_binary/1,2` reach it through `encode_image`/`encode_body`;
/// `erlang:external_size/1,2` reach it through `plain_image_of`, which shares the
/// same `encode_body`. Sorted by `(module, function, arity)`; see
/// [`needs_local_fun_identity`].
pub const LOCAL_FUN_IDENTITY_BIFS: &[(&str, &str, u32)] = &[
    ("erlang", "external_size", 1),
    ("erlang", "external_size", 2),
    ("erlang", "term_to_binary", 1),
    ("erlang", "term_to_binary", 2),
];

/// Whether `(module, function, arity)` is one of the [`MEMORY_HOOK_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_memory_hook(module: &str, function: &str, arity: u32) -> bool {
    MEMORY_HOOK_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`LOCAL_FUN_IDENTITY_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_local_fun_identity(module: &str, function: &str, arity: u32) -> bool {
    LOCAL_FUN_IDENTITY_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::registry::default_registry;

    #[test]
    fn memory_hook_bifs_are_exactly_the_memory_pair() {
        assert_eq!(
            MEMORY_HOOK_BIFS,
            &[("erlang", "memory", 0), ("erlang", "memory", 1)]
        );
    }

    #[test]
    fn local_fun_identity_bifs_are_exactly_the_encoders() {
        // `encode_body` (binconv.rs) reads `ctx.local_fun_identity()`; it is reached
        // by `term_to_binary/1,2` (via `encode_image`) and by `external_size/1,2`
        // (via `plain_image_of`). Sorted by `(module, function, arity)`.
        assert_eq!(
            LOCAL_FUN_IDENTITY_BIFS,
            &[
                ("erlang", "external_size", 1),
                ("erlang", "external_size", 2),
                ("erlang", "term_to_binary", 1),
                ("erlang", "term_to_binary", 2),
            ]
        );
    }

    /// The tables are `binary_search`-ed, so they must be strictly ascending.
    #[test]
    fn both_tables_are_sorted_and_duplicate_free() {
        for table in [MEMORY_HOOK_BIFS, LOCAL_FUN_IDENTITY_BIFS] {
            for window in table.windows(2) {
                assert!(
                    window[0] < window[1],
                    "the table must be strictly ascending, got {:?} then {:?}",
                    window[0],
                    window[1]
                );
            }
        }
    }

    /// Every listed key answers `true` through the predicate.
    #[test]
    fn predicates_accept_every_listed_key() {
        for (module, function, arity) in MEMORY_HOOK_BIFS {
            assert!(
                needs_memory_hook(module, function, *arity),
                "needs_memory_hook must accept the listed key {module}:{function}/{arity}"
            );
        }
        for (module, function, arity) in LOCAL_FUN_IDENTITY_BIFS {
            assert!(
                needs_local_fun_identity(module, function, *arity),
                "needs_local_fun_identity must accept the listed key {module}:{function}/{arity}"
            );
        }
    }

    /// Near misses (wrong arity / wrong module / a different BIF) answer `false`.
    #[test]
    fn predicates_reject_near_misses() {
        let memory_misses = [
            ("erlang", "memory", 2),          // wrong arity
            ("erlang", "system_info", 1),     // a different erlang BIF
            ("lists", "memory", 1),           // wrong module
            ("erlang", "binary_to_term", 1),  // another encoder, no memory seam
        ];
        for (module, function, arity) in memory_misses {
            assert!(
                !needs_memory_hook(module, function, arity),
                "{module}:{function}/{arity} must not need the memory hook"
            );
        }

        let fun_identity_misses = [
            ("erlang", "term_to_binary", 0),   // wrong arity
            ("erlang", "term_to_binary", 3),   // wrong arity
            ("erlang", "external_size", 0),    // wrong arity
            ("erlang", "binary_to_term", 1),   // the decode direction is not an encoder
            ("lists", "term_to_binary", 1),    // wrong module
            ("erlang", "-", 2),                // an unrelated operator BIF
        ];
        for (module, function, arity) in fun_identity_misses {
            assert!(
                !needs_local_fun_identity(module, function, arity),
                "{module}:{function}/{arity} must not need the local-fun identity"
            );
        }
    }

    /// Drift guard: every listed key is actually registered, so renaming/removing a
    /// BIF on the registry side fails here instead of degrading at runtime.
    #[test]
    fn every_listed_key_is_registered() {
        let reg = default_registry();
        for (module, function, arity) in MEMORY_HOOK_BIFS {
            assert!(
                reg.contains(module, function, *arity),
                "listed memory-hook key {module}:{function}/{arity} is not registered"
            );
        }
        for (module, function, arity) in LOCAL_FUN_IDENTITY_BIFS {
            assert!(
                reg.contains(module, function, *arity),
                "listed local-fun-identity key {module}:{function}/{arity} is not registered"
            );
        }
    }
}
