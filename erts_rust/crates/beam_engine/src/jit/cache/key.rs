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

//! # The baseline-JIT cache key
//!
//! The **sound identity** the cross-run [`CodeCache`](super::CodeCache) keys its compiled
//! blocks on — its *key machinery*, split out of `cache/mod.rs` when that file outgrew the
//! ~1000-line budget: [`CacheKey`] (which module, **and** which atom namespace its code was
//! lowered against), its [`ModuleKey`] half and the derivation helpers
//! [`digest_of`]/[`module_name`]/[`module_address`], plus the fallback content hash
//! [`module_fingerprint`].
//!
//! # Identity: the load-time MD5 digest (O(1)) **plus the atom-namespace witness**
//!
//! A cache key must never confuse two different modules. A raw pointer to the `Module`
//! is **not** a valid key — a freed `Module`'s address can be reused by a differently
//! loaded module, and the cache would then hand back the *wrong* compiled code (a silent
//! miscompilation). The key is therefore **two-part**
//!
//! ```text
//! struct CacheKey {
//!     module: ModuleKey,    // the module's own identity
//!     atoms: AtomSnapshot,  // the atom namespace the artifact was compiled against
//! }
//!
//! enum ModuleKey {
//!     Digest([u8; 16]),                            // a real `.beam`: read in O(1)
//!     Content { name: String, fingerprint: u64 },  // a synthetic module: content hash
//! }
//! ```
//!
//! `atoms` is [`Module::atom_table`]'s [`AtomSnapshot`](beam_term::AtomSnapshot) — an
//! O(1), allocation-free witness of the table's *identity*, its append generation and its
//! length. It is load-bearing because the native baseline tier **bakes atom indices into
//! the emitted code**: an IR `Operand::Atom(a)` is lowered to the `Eterm` atom immediate
//! `make_atom(a)` (see `src/jit/baseline/emit/operands.rs`), so a block that compares an
//! atom clause head (`is_eq_exact`/`select_val`) holds the *index* the module's own atom
//! table assigned. The module's MD5 says nothing about that index: the very same `.beam`
//! loaded against a different namespace (a second runtime in the same thread with its
//! own atom table — `beam_rt`'s code server loads every `.beam` against its shared table)
//! assigns every name a different index, so reusing the first runtime's artifact would
//! compare against the wrong atom — a silently wrong result, not a crash. Witnessing the
//! namespace makes that a **miss**.
//!
//! ## The trade, and why it is the right one
//!
//! Adding the witness costs at worst a **recompile**: two loads of one `.beam` — or even
//! a `Module` clone, since `AtomTable::clone` deliberately takes a *fresh* identity — no
//! longer share an entry, because each carries its own atom table instance. A stale entry
//! (an older generation of the same table) is likewise a recompile. What it can never
//! cause is a wrong answer: equal snapshots imply the same index → name mapping over
//! every index the artifact can embed, and reading one is O(1) (three scalars), so the
//! hit path stays hash-free and allocation-free for a real `.beam`.
//!
//! ## The module half
//!
//! * `Digest` is the module's **load-time MD5** — [`Module::checksum`], the same digest
//!   the C reader stores in `beam->checksum` and `beam_lib:md5/1` reports, computed
//!   once by `beam_loader::checksum::module_md5` at the single `Module` construction
//!   site in `beam_loader::lower`. Reading it is a 16-byte `Copy`: **no hash, no
//!   allocation, no module-name lookup**, which is what makes a slice entry/resume an
//!   O(1) key probe.
//! * `Content` is the fallback for a module whose checksum is all-zero — the
//!   `Module::default()`/hand-built/synthetic sentinel. There `name` is the module name
//!   **as a string** (`Module::atom_name(Module::name)`), not the per-module atom index,
//!   and `fingerprint` is [`module_fingerprint`], a deterministic hash of the module's
//!   **decoded IR content**.
//!
//! Two modules with the same name but different content have different digests (or
//! different fingerprints on the fallback) and **cannot collide**; two loads of the very
//! same `.beam` collide on the module half alone and are separated by the namespace half
//! (see the trade above).
//!
//! ## Why the digest implies artifact-equivalence
//!
//! The MD5 digests, in a fixed order, the **raw payloads** of the `Atom` (obsolete) /
//! `AtU8` / `Code` (the *full* payload, header included) / `StrT` / `ImpT` / `ExpT` /
//! `FunT` / `LitT` / `Meta` / `Recs` / `DbgB` chunks, and deliberately EXCLUDES the
//! `Line` / `LocT` / `Attr` / `CInf` / `Type` / `Docs` / `Dbgi` chunks
//! (`beam_loader::checksum`, the `only_the_significant_chunks_are_digested` test).
//! Everything a [`CompiledModule`](crate::jit::baseline::CompiledModule) is derived from
//! — the module name (wire index 1 of `AtU8`), the export and import tables, `functions`
//! (the `Code` chunk), `funs`, `records`, `code_version` and the literals — lies inside
//! that subset, and no code under `src/jit/` reads an excluded field: `module.strings`,
//! `module.line` and `module.types` are read from the **live** module by the interpreter
//! families and `machine/catch.rs` at run time, never by the compiled artifact
//! (`strings`/`StrT` is digested anyway). So equal digests imply artifact-equivalence
//! **for every operand whose meaning is namespace-independent**, and the digest is
//! strictly stronger than the 64-bit content hash it replaces on this path. The one
//! digested field whose meaning *is* namespace-dependent — the `AtU8` atom table — is
//! exactly what the `atoms` half of the [`CacheKey`] witnesses. An all-zero digest is not
//! a real `.beam`: it is the `Module::default()` sentinel, which is exactly what selects
//! the fallback.
//!
//! Neither variant ever weakens identity against the pre-existing behaviour: two
//! modules the content hash separated are still separated by the digest, and the two
//! variants can never key one module to the other (the enum discriminants differ). Two
//! entries for one artifact (a same-content module in a second namespace, a
//! digest-keyed and a synthetic module with the same logical content) are at worst a
//! wasted duplicate compile — never a wrong result.
//!
//! `beam_loader::Module` does **not** retain the original `.beam` bytes, so the
//! fallback fingerprint is computed from the decoded IR. Most IR types derive `Debug`
//! but not `Hash`, so the structural parts are rendered through a [`std::fmt::Write`]
//! adapter that streams the (deterministic, hash-map-free) `Debug` output straight into
//! a hasher — no intermediate `String`, and no per-instruction allocation.

use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use beam_loader::Module;
use beam_term::AtomSnapshot;

// Test-only count of `module_fingerprint` invocations **on this thread**.
//
// Pins the O(1)-hit contract: a cached lookup must not re-hash the IR. Compiled only
// for the crate's own unit tests, so it costs nothing in a production build.
#[cfg(test)]
thread_local! {
    static FINGERPRINT_CALLS: std::cell::Cell<u64> = std::cell::Cell::new(0);
}

/// Test-only: how many times [`module_fingerprint`] has run on this thread.
#[cfg(test)]
pub(crate) fn fingerprint_calls() -> u64 {
    FINGERPRINT_CALLS.with(|calls| calls.get())
}

/// A `fmt::Write` sink that feeds every written byte into a [`Hasher`].
///
/// Used by [`module_fingerprint`] to stream the deterministic `Debug` rendering of the
/// IR's structural tables directly into the hasher, avoiding a `String` allocation per
/// instruction. Its `write_str` cannot fail (it never returns `fmt::Error`).
struct HashWriter<'a, H: Hasher>(&'a mut H);

impl<H: Hasher> std::fmt::Write for HashWriter<'_, H> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.write(s.as_bytes());
        Ok(())
    }
}

/// A deterministic 64-bit fingerprint of a loaded module's **decoded IR content**.
///
/// The value depends only on the module's content — never on where it happens to live in
/// memory — so two independently loaded copies of the same `.beam` fingerprint equally,
/// and two modules that differ in name, atom content, imports/exports, function bodies,
/// literals or the raw attribute/compile-info/string blobs fingerprint differently. It is
/// the **fallback** half of the module's identity: a real `.beam` keys on its load-time
/// MD5 digest ([`ModuleKey::Digest`]) and only a module with an all-zero checksum (the
/// [`Module::default()`] sentinel of a hand-built/synthetic module) reaches this hash —
/// which is what keeps a synthetic module's identity exactly as it was before the digest
/// key existed (see [`ModuleKey`]).
///
/// The IR's structural tables (`export`/`import`/`fun`/`record`/`type`/`line` and the
/// `Function` bodies) derive `Debug` but not `Hash`, and none of them contains a hash
/// map, so their `Debug` rendering is deterministic and is streamed into the hasher
/// through [`HashWriter`]. `LiteralArea`'s own `Debug` is content-blind (it prints only
/// counts), so the decoded literal words are hashed directly.
///
/// Public because it is the documented fallback identity of a module — the
/// [`CodeCache`](super::CodeCache)'s own key derivation is the only composer of it.
pub fn module_fingerprint(module: &Module) -> u64 {
    #[cfg(test)]
    FINGERPRINT_CALLS.with(|calls| calls.set(calls.get() + 1));
    let mut hasher = DefaultHasher::new();

    // The module name as a string (not the per-module atom index).
    module.atom_name(module.name).hash(&mut hasher);

    // The atom table in wire order, by name: two modules whose atom *indices* happen to
    // coincide but whose names differ must not fingerprint equally.
    for atom in &module.atoms {
        module.atom_name(*atom).hash(&mut hasher);
    }

    // The structural IR. Every one of these types derives `Debug` and none holds a hash
    // map, so the rendering is deterministic; `Module::functions` carries the entire
    // instruction stream (opcode/operand/label structure) of the module.
    {
        let mut writer = HashWriter(&mut hasher);
        // `write!` into a `fmt::Write` cannot fail here; the result is discarded.
        let _ = write!(
            writer,
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            module.exports,
            module.imports,
            module.functions,
            module.funs,
            module.records,
            module.code_version,
            module.types,
            module.line,
        );
    }

    // The raw byte blobs.
    module.strings.hash(&mut hasher);
    module.attributes.hash(&mut hasher);
    module.compile_info.hash(&mut hasher);

    // The decoded literal words (`LiteralArea`'s `Debug` is content-blind).
    for term in module.literals.terms() {
        term.hash(&mut hasher);
    }

    hasher.finish()
}

/// The **sound identity** of a loaded module: its load-time digest, or — for a synthetic
/// module with an all-zero checksum — its name plus a content fingerprint.
///
/// This is only the *module* half of the [`CacheKey`]: it says which container the
/// artifact was derived from, not which atom namespace its baked atom indices refer to.
///
/// Not a pointer: the digest is the container's own MD5 (see the module docs) and the
/// fallback is derived from the IR content. Derives `Hash`/`Eq` so it can key the
/// [`CodeCache`](super::CodeCache)'s map; the two variants hash their discriminant too, so
/// a digest-keyed module and a content-keyed one can never share an entry.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum ModuleKey {
    /// A real `.beam`: the 16-byte MD5 computed once at load time
    /// ([`Module::checksum`]), read here as a plain `Copy` — O(1), no hash, no
    /// allocation. Everything a
    /// [`CompiledModule`](crate::jit::baseline::CompiledModule) is derived from is
    /// digested, so equal digests imply artifact-equivalence over everything except the
    /// indices the artifact bakes — which is what the [`CacheKey`]'s namespace half
    /// covers.
    Digest([u8; 16]),

    /// A synthetic/hand-built module ([`Module::default`] carries an all-zero checksum,
    /// which no real `.beam` has): its name as a string plus the content fingerprint of
    /// its decoded IR ([`module_fingerprint`]).
    Content {
        /// The module name as a string.
        name: String,
        /// The IR content fingerprint.
        fingerprint: u64,
    },
}

impl ModuleKey {
    /// The module half of `module`'s key: its load-time digest when it has one, else its
    /// content hash.
    pub(super) fn of(module: &Module) -> ModuleKey {
        match digest_of(module) {
            Some(digest) => ModuleKey::Digest(digest),
            // No digest (a synthetic module): fall back to the honest content hash,
            // which is what distinguishes two same-named synthetic modules.
            None => ModuleKey::Content {
                name: module_name(module),
                fingerprint: module_fingerprint(module),
            },
        }
    }

    /// The module half of `module`'s key whose **content fingerprint is already known**.
    ///
    /// The digest wins when the module has one (the passed `fingerprint` is then ignored
    /// — it is the artifact's own one-time cold-staging hash, redundant with the O(1)
    /// digest); a synthetic module keeps the caller's fingerprint, which is the one the
    /// artifact recorded ([`CompiledModule::fingerprint`]) at its creation. Used when
    /// the fingerprint comes from that compiled artifact.
    pub(super) fn with_fingerprint(module: &Module, fingerprint: u64) -> ModuleKey {
        match digest_of(module) {
            Some(digest) => ModuleKey::Digest(digest),
            None => ModuleKey::Content {
                name: module_name(module),
                fingerprint,
            },
        }
    }
}

/// The **complete cache key**: which module, **and** which atom namespace its compiled
/// code was lowered against.
///
/// The namespace half is load-bearing, not belt-and-braces: the native baseline tier
/// bakes atom *indices* into the emitted code (an IR `Operand::Atom(a)` becomes the
/// `Eterm` immediate `make_atom(a)`), and a module's MD5 says nothing about the indices
/// its atom table assigned. The very same `.beam` loaded against a second namespace —
/// another runtime in this thread with its own [`AtomTable`](beam_term::AtomTable), which
/// is exactly how `beam_rt`'s code server loads every module — numbers every name
/// differently, so the first runtime's artifact would compare a clause head against the
/// *wrong* atom. Witnessing the namespace turns that into a miss (see the module docs for
/// the trade).
///
/// Both halves come from [`CacheKey::of`]/[`CacheKey::with_fingerprint`] — the **single**
/// derivation every entry in [`CodeCache::entries`](super::CodeCache::entries) and every
/// [`MemoEntry`](super::MemoEntry) goes through, so the two maps can never disagree about
/// a module's identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct CacheKey {
    /// The module's own identity (the digest, or the synthetic content hash).
    pub(super) module: ModuleKey,
    /// The atom-namespace witness the artifact was compiled against.
    ///
    /// [`Module::atom_table`]'s [`AtomSnapshot`](beam_term::AtomSnapshot) — three scalars
    /// (the table's identity, its append generation, its length), read in O(1) with no
    /// allocation. `AtomTable::clone` deliberately takes a **fresh** identity, so two
    /// loads of one `.beam` never share a snapshot; equal snapshots therefore imply the
    /// same index → name mapping over every index the artifact can embed.
    pub(super) atoms: AtomSnapshot,
}

impl CacheKey {
    /// The key of `module`, deriving the content fingerprint when it has no digest.
    ///
    /// The **only** way a key is derived from a bare [`Module`] (the `contains`/
    /// `get_or_compile`/`compiled_handle` paths).
    pub(super) fn of(module: &Module) -> CacheKey {
        CacheKey {
            module: ModuleKey::of(module),
            atoms: module.atom_table.snapshot(),
        }
    }

    /// The key of `module` whose **content fingerprint is already known**
    /// ([`CompiledModule::fingerprint`]).
    ///
    /// The single derivation the artifact-holding paths use
    /// ([`CodeCache::prime`](super::CodeCache)/[`CodeCache::remember`](super::CodeCache)):
    /// the stored fingerprint is reused instead of re-hashing the IR, so a synthetic
    /// module is keyed without a second hash while a real `.beam` still keys on its O(1)
    /// digest.
    pub(super) fn with_fingerprint(module: &Module, fingerprint: u64) -> CacheKey {
        CacheKey {
            module: ModuleKey::with_fingerprint(module, fingerprint),
            atoms: module.atom_table.snapshot(),
        }
    }
}

/// The module's load-time MD5 digest, if it has one.
///
/// `None` exactly when the checksum is all-zero — the `Module::default()` sentinel of a
/// hand-built/synthetic module (see [`ModuleKey`]). Reading the digest is a 16-byte
/// `Copy`, which is the whole point: a slice entry/resume never hashes the decoded IR.
fn digest_of(module: &Module) -> Option<[u8; 16]> {
    let checksum = module.checksum();
    (checksum != [0u8; 16]).then_some(checksum)
}

/// The module's name as a string (the name half of the [`ModuleKey::Content`] fallback).
///
/// An unnamed module (no interned name) keys on the empty string; the fingerprint still
/// distinguishes it from any named module.
fn module_name(module: &Module) -> String {
    module.atom_name(module.name).unwrap_or_default().to_owned()
}

/// The address of `module`, the [`CodeCache`](super::CodeCache)'s resolution-memo key.
///
/// [`Arc::as_ptr`](std::sync::Arc::as_ptr) and a plain `&Module` yield the same address
/// for the same live module (an `Arc`'s payload *is* the `Module`), so a memo entry
/// recorded from an `Arc<Module>` is found when the driver later hands the entry module
/// as a `&Module`.
pub(super) fn module_address(module: &Module) -> usize {
    module as *const Module as usize
}
