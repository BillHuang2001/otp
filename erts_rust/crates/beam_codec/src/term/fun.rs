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

//! Fun / export forwarding layer over [`beam_term::fun`].
//!
//! The frozen interior layout of every kind of fun — the 3-word `EXPORT_EXT`
//! (`[header, module, function]`) block and the 11-word `NEW_FUN_EXT` descriptor
//! (`[header, module, uniq[0..8], uniq[8..16], index, old_index, old_uniq, pid…]`)
//! plus its environment — is owned by [`beam_term::fun`] (the lowest common
//! ancestor for term operations — workspace `CONTEXT.md` §2, "No duplication
//! across crates"). Every constructor here therefore only:
//!
//! 1. checks the documented bounds/type/validity preconditions, mapping a failure
//!    to a [`beam_common::BeamError`], and
//! 2. allocates the block from [`beam_alloc::Heap`] (the `make_*` family) or
//!    forwards straight to the matching [`beam_term::fun`] accessor.
//!
//! All layout arithmetic — header construction and word offsets — lives in
//! [`beam_term::fun`].
//!
//! This is a child of the [`crate::term`] subtree, so it inherits that subtree's
//! `#[allow(unsafe_code)]` (the crate's single, auditable `unsafe` surface under
//! the crate-wide `#![deny(unsafe_code)]`): the [`beam_alloc`] allocation calls,
//! the [`beam_term`] forwarders and the minimal type/validity guards — each
//! carrying a `// SAFETY:` rationale.
//!
//! Every error stays [`beam_common::Result`] (via [`crate::error`]); an accessor
//! never panics and never exhibits undefined behaviour on a mismatched term.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::fun;
use beam_term::prelude::{boxed_val, is_any_fun, Atom, Eterm};

use crate::error;

/// The frozen fun-layout surface dependents need to inspect, size or call a fun:
/// the kind discriminator, the creator-pid struct, the arity ceiling and the word
/// sizing helper. Re-exported verbatim from the layout owner (`beam_term::fun`).
pub use beam_term::fun::{fun_kind_of_header, fun_words, FunKind, FunPid, FUN_MAX_FIELD, MAX_ARG};

/// The header word of a fun **term**, or `BeamError::BadArg` for a non-fun.
///
/// The boxed flag and the fun subtag are both tested before the header word is
/// read, so a small, `NIL` or an unrelated boxed term is reported as an error
/// rather than dereferenced.
fn fun_header_word(term: Eterm) -> Result<Eterm> {
    // SAFETY: `is_any_fun` reads only the header word of a boxed term (the caller
    // invariant for any `Eterm`); a non-boxed term is rejected without a read.
    if !unsafe { is_any_fun(term) } {
        return Err(error::bad_arg());
    }
    // SAFETY: `is_any_fun` established that `term` is a live boxed fun, so its
    // header word is a live, initialised term word.
    Ok(unsafe { *boxed_val(term) })
}

/// Materializes an `EXPORT_EXT` ([`FunKind::External`]) fun on `heap`.
///
/// The `[header, module, function]` 3-word block is written by
/// [`beam_term::fun::write_fun_external`]; `module`/`function` are the exported
/// MFA's atoms and there is no environment. `arity` is bounded by the header's
/// 8-bit field (`0..=MAX_ARG`); a larger value is a `SystemLimit` (via
/// `make_fun_header`), never a truncated header.
pub fn make_fun_external(
    heap: &mut Heap,
    arity: usize,
    module: Atom,
    function: Atom,
) -> Result<Eterm> {
    // SAFETY: `alloc_words` returned `fun_words(FunKind::External, 0)`
    // exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(fun_words(FunKind::External, 0))? };
    // SAFETY: `hp` owns exactly those words, which `write_fun_external` fully
    // initialises.
    Ok(unsafe { fun::write_fun_external(hp, arity, module, function)? })
}

/// Materializes a `NEW_FUN_EXT` ([`FunKind::Imported`]) fun on `heap`.
///
/// The 11-word descriptor (`[header, module, uniq[0..8], uniq[8..16], index,
/// old_index, old_uniq, pid…]`) plus `env.len()` words is written by
/// [`beam_term::fun::write_fun_imported`]. `uniq` is the 16-byte MD5 identity,
/// `creator` the process that built the original closure and `env` the free
/// variables — **`env[k]` is the `k`-th environment slot**, i.e. the wire order
/// (`external.c:5171-5174`).
#[allow(clippy::too_many_arguments)]
pub fn make_fun_imported(
    heap: &mut Heap,
    arity: usize,
    module: Atom,
    uniq: &[u8; 16],
    index: u32,
    old_index: u32,
    old_uniq: u32,
    creator: FunPid,
    env: &[Eterm],
) -> Result<Eterm> {
    // SAFETY: `alloc_words` returned `fun_words(FunKind::Imported, env.len())`
    // exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(fun_words(FunKind::Imported, env.len()))? };
    // SAFETY: `hp` owns exactly those words and `env` is the `num_free` slice
    // `write_fun_imported` documents.
    Ok(unsafe {
        fun::write_fun_imported(hp, arity, module, uniq, index, old_index, old_uniq, creator, env)?
    })
}

/// The [`FunKind`] of a fun term; `BeamError::BadArg` for a non-fun.
pub fn fun_kind_term(term: Eterm) -> Result<FunKind> {
    Ok(fun_kind_of_header(fun_header_word(term)?))
}

/// The arity of a fun term; `BeamError::BadArg` for a non-fun.
///
/// The term-level counterpart of [`beam_term::fun::fun_arity`], which takes a
/// *header word*; the `_term` suffix keeps the two distinct.
pub fn fun_arity_term(term: Eterm) -> Result<usize> {
    Ok(fun::fun_arity(fun_header_word(term)?))
}

/// The environment size (`num_free`) of a fun term; `BeamError::BadArg` for a
/// non-fun.
///
/// The term-level counterpart of [`beam_term::fun::fun_num_free`].
pub fn fun_num_free_term(term: Eterm) -> Result<usize> {
    Ok(fun::fun_num_free(fun_header_word(term)?))
}

/// The `module` atom of an external or imported fun term.
///
/// `BeamError::BadArg` for a non-fun or a local fun (whose inlined descriptor
/// carries no module — the C `ErlFunEntry.module` is not part of the term).
pub fn fun_module(term: Eterm) -> Result<Atom> {
    fun_header_word(term)?;
    // SAFETY: the guard established that `term` is a live boxed fun; `fun_module`
    // rejects a local fun with `TermError::BadFun` (converted to `BadArg`).
    Ok(unsafe { fun::fun_module(term)? })
}

/// The `function` (name) atom of an external fun term.
///
/// `BeamError::BadArg` for a non-fun or any other fun kind.
pub fn fun_function(term: Eterm) -> Result<Atom> {
    fun_header_word(term)?;
    // SAFETY: the guard established that `term` is a live boxed fun; `fun_function`
    // rejects everything but an external fun with `TermError::BadFun`.
    Ok(unsafe { fun::fun_function(term)? })
}

/// The 16-byte MD5 `uniq` of an imported (`NEW_FUN_EXT`) fun term.
///
/// `BeamError::BadArg` for a non-fun or any other fun kind.
pub fn fun_uniq_bytes(term: Eterm) -> Result<[u8; 16]> {
    fun_header_word(term)?;
    // SAFETY: the guard established that `term` is a live boxed fun; `fun_uniq_bytes`
    // rejects everything but an imported fun with `TermError::BadFun`.
    Ok(unsafe { fun::fun_uniq_bytes(term)? })
}

/// The lambda-table `index` of a local or imported fun term.
///
/// `BeamError::BadArg` for a non-fun or an external fun (which has no index).
pub fn fun_index(term: Eterm) -> Result<u32> {
    fun_header_word(term)?;
    // SAFETY: the guard established that `term` is a live boxed fun; `fun_index`
    // rejects an external fun with `TermError::BadFun`.
    Ok(unsafe { fun::fun_index(term)? })
}

/// The `old_index` of an imported (`NEW_FUN_EXT`) fun term.
///
/// `BeamError::BadArg` for a non-fun or any other fun kind.
pub fn fun_old_index(term: Eterm) -> Result<u32> {
    fun_header_word(term)?;
    // SAFETY: the guard established that `term` is a live boxed fun; `fun_old_index`
    // rejects everything but an imported fun with `TermError::BadFun`.
    Ok(unsafe { fun::fun_old_index(term)? })
}

/// The `old_uniq` of an imported (`NEW_FUN_EXT`) fun term.
///
/// `BeamError::BadArg` for a non-fun or any other fun kind.
pub fn fun_old_uniq(term: Eterm) -> Result<u32> {
    fun_header_word(term)?;
    // SAFETY: the guard established that `term` is a live boxed fun; `fun_old_uniq`
    // rejects everything but an imported fun with `TermError::BadFun`.
    Ok(unsafe { fun::fun_old_uniq(term)? })
}

/// The creator pid of an imported (`NEW_FUN_EXT`) fun term.
///
/// `BeamError::BadArg` for a non-fun or any other fun kind.
pub fn fun_creator_pid(term: Eterm) -> Result<FunPid> {
    fun_header_word(term)?;
    // SAFETY: the guard established that `term` is a live boxed fun; `fun_creator_pid`
    // rejects everything but an imported fun with `TermError::BadFun`.
    Ok(unsafe { fun::fun_creator_pid(term)? })
}

/// The environment (free variables) of a fun term, in `env` order — `env[k]` is
/// the `k`-th wire element.
///
/// The slice is borrowed from the term's storage (the caller must keep the owning
/// heap alive and unmoved); an external fun has no environment, so the slice is
/// empty. `BeamError::BadArg` for a non-fun.
pub fn fun_env<'a>(term: Eterm) -> Result<&'a [Eterm]> {
    fun_header_word(term)?;
    // SAFETY: the guard established that `term` is a live boxed fun; the caller keeps
    // the owning heap alive and unmoved for `'a`.
    Ok(unsafe { fun::fun_env(term)? })
}

/// Materializes a [`FunKind::Local`] (`make_fun3`) fun on `heap`.
///
/// Provided **for tests only**: the codec never *decodes* a local fun (no wire tag
/// represents one) and cannot *encode* it faithfully (see
/// [`crate::error::unsupported_local_fun`]), but the encode path's rejection of one
/// must be exercised with a real local fun term. The production builder lives in
/// `beam_engine::terms::make_fun`.
#[cfg(test)]
pub fn make_fun_local(
    heap: &mut Heap,
    arity: usize,
    uniq: u64,
    index: u32,
    env: &[Eterm],
) -> Result<Eterm> {
    // SAFETY: `alloc_words` returned `fun_words(FunKind::Local, env.len())`
    // exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(fun_words(FunKind::Local, env.len()))? };
    // SAFETY: `hp` owns exactly those words and `env` is the `num_free` slice
    // `write_fun_local` documents.
    Ok(unsafe { fun::write_fun_local(hp, arity, uniq, index, env)? })
}

#[cfg(test)]
mod tests {
    //! Raw-word unit tests for the fun blocks: the descriptor layout is asserted
    //! word-by-word against the legacy C ground truth, and every accessor is checked
    //! for its error behaviour on the wrong term type / wrong fun kind.
    //!
    //! `unsafe` below only dereferences pointers into a `Heap` this test has just
    //! allocated and written itself; each block has its own `// SAFETY:` note.

    use beam_term::prelude::{
        boxed_val, is_any_fun, is_external_fun, make_atom, make_fun_header, make_small, Atom, Eterm,
        NIL,
    };

    use super::*;
    use crate::term::tests::{assert_bad_arg, assert_system_limit, new_heap};

    /// The `u64` a little-endian 8-byte window decodes to (the packing
    /// `write_fun_imported` applies to each half of the 16-byte `uniq`).
    fn from_le(bytes: &[u8]) -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(bytes);
        u64::from_le_bytes(buf)
    }

    #[test]
    fn external_fun_block_is_header_module_function() {
        let mut heap = new_heap();
        let module = Atom::from_raw(0x11);
        let function = Atom::from_raw(0x22);
        let term = make_fun_external(&mut heap, 2, module, function).expect("3 words fit");
        assert_eq!(heap.used(), 3, "the external fun is 1 + 2 words");

        assert_eq!(fun_kind_term(term).unwrap(), FunKind::External);
        assert_eq!(fun_arity_term(term).unwrap(), 2);
        assert_eq!(fun_num_free_term(term).unwrap(), 0);
        assert_eq!(fun_module(term).unwrap(), module);
        assert_eq!(fun_function(term).unwrap(), function);
        assert_eq!(fun_env(term).unwrap(), &[] as &[Eterm]);

        // SAFETY: `term` is the 3-word external fun this test just built, so the
        // block `[header, module, function]` is live; every index is in block.
        unsafe {
            assert!(is_any_fun(term));
            let p = boxed_val(term);
            assert_eq!(*p, make_fun_header(2, 0, FunKind::External).unwrap());
            assert!(is_external_fun(*p));
            assert_eq!(*p.add(1), make_atom(module));
            assert_eq!(*p.add(2), make_atom(function));
        }

        // An arity past the 8-bit header field is a `SystemLimit`, never a
        // truncated header.
        assert_system_limit(make_fun_external(&mut heap, 256, module, function));
    }

    #[test]
    fn imported_fun_block_is_the_ten_word_descriptor_plus_env() {
        let mut heap = new_heap();
        let module = Atom::from_raw(0x33);
        let node = Atom::from_raw(0x44);
        let mut uniq = [0u8; 16];
        for (i, byte) in uniq.iter_mut().enumerate() {
            *byte = i as u8 + 1; // 1..=16
        }
        let creator = FunPid {
            node,
            number: 0xAAAA_BBBB,
            serial: 0xCCCC_DDDD,
            creation: 0x1122_3344,
        };
        let env = [make_small(7), make_small(8)];
        let term = make_fun_imported(&mut heap, 1, module, &uniq, 42, 7, 3, creator, &env)
            .expect("fits");
        assert_eq!(heap.used(), 1 + 10 + 2, "1 + 10 + num_free words");

        assert_eq!(fun_kind_term(term).unwrap(), FunKind::Imported);
        assert_eq!(fun_arity_term(term).unwrap(), 1);
        assert_eq!(fun_num_free_term(term).unwrap(), 2);
        assert_eq!(fun_module(term).unwrap(), module);
        assert_eq!(fun_uniq_bytes(term).unwrap(), uniq);
        assert_eq!(fun_index(term).unwrap(), 42);
        assert_eq!(fun_old_index(term).unwrap(), 7);
        assert_eq!(fun_old_uniq(term).unwrap(), 3);
        assert_eq!(fun_creator_pid(term).unwrap(), creator);
        assert_eq!(fun_env(term).unwrap(), &env[..]);

        // SAFETY: `term` is the imported fun this test just built; every descriptor
        // word (1..=10) and both env words are inside its block.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(*p, make_fun_header(1, 2, FunKind::Imported).unwrap());
            assert_eq!(*p.add(1), make_atom(module));
            assert_eq!((*p.add(2)).raw() as u64, from_le(&uniq[..8]));
            assert_eq!((*p.add(3)).raw() as u64, from_le(&uniq[8..]));
            assert_eq!((*p.add(4)).raw() as u32, 42);
            assert_eq!((*p.add(5)).raw() as u32, 7);
            assert_eq!((*p.add(6)).raw() as u32, 3);
            assert_eq!(*p.add(7), make_atom(node));
            assert_eq!((*p.add(8)).raw() as u32, 0xAAAA_BBBB);
            assert_eq!((*p.add(9)).raw() as u32, 0xCCCC_DDDD);
            assert_eq!((*p.add(10)).raw() as u32, 0x1122_3344);
            assert_eq!(*p.add(11), make_small(7));
            assert_eq!(*p.add(12), make_small(8));
        }

        // An imported fun without an environment is 11 words.
        let bare = make_fun_imported(&mut heap, 0, module, &[0u8; 16], 9, 9, 9, creator, &[])
            .expect("fits");
        assert_eq!(fun_num_free_term(bare).unwrap(), 0);
        assert_eq!(fun_env(bare).unwrap(), &[] as &[Eterm]);
    }

    #[test]
    fn fun_accessors_reject_non_funs_and_the_wrong_kind() {
        let mut heap = new_heap();

        // Non-funs: an immediate, `NIL`, a boxed non-fun.
        let tuple = heap.tuple(&[make_small(1)]).unwrap();
        for term in [make_small(1), NIL, tuple] {
            assert_bad_arg(fun_kind_term(term));
            assert_bad_arg(fun_arity_term(term));
            assert_bad_arg(fun_num_free_term(term));
            assert_bad_arg(fun_module(term));
            assert_bad_arg(fun_function(term));
            assert_bad_arg(fun_uniq_bytes(term));
            assert_bad_arg(fun_index(term));
            assert_bad_arg(fun_old_index(term));
            assert_bad_arg(fun_old_uniq(term));
            assert_bad_arg(fun_creator_pid(term));
            assert_bad_arg(fun_env(term));
        }

        // An external fun carries no index/uniq/old_*/creator pid.
        let external =
            make_fun_external(&mut heap, 1, Atom::from_raw(1), Atom::from_raw(2)).unwrap();
        assert_bad_arg(fun_index(external));
        assert_bad_arg(fun_uniq_bytes(external));
        assert_bad_arg(fun_old_index(external));
        assert_bad_arg(fun_old_uniq(external));
        assert_bad_arg(fun_creator_pid(external));

        // An imported fun has no function name.
        let creator = FunPid {
            node: Atom::from_raw(3),
            number: 1,
            serial: 2,
            creation: 3,
        };
        let imported =
            make_fun_imported(&mut heap, 0, Atom::from_raw(1), &[0u8; 16], 1, 2, 3, creator, &[])
                .unwrap();
        assert_bad_arg(fun_function(imported));
    }

    #[test]
    fn local_fun_has_no_module_function_or_creator() {
        let mut heap = new_heap();
        // The local `make_fun3` layout is owned by `beam_term::fun`; build it there.
        // SAFETY: the test reserved exactly `fun_words(Local, 0) == 3` words and
        // `write_fun_local` writes all of them.
        let local = unsafe {
            let hp = heap.alloc_words(fun_words(FunKind::Local, 0)).unwrap();
            fun::write_fun_local(hp, 1, 0x42, 5, &[]).unwrap()
        };
        assert_eq!(fun_kind_term(local).unwrap(), FunKind::Local);
        assert_eq!(fun_arity_term(local).unwrap(), 1);
        assert_eq!(fun_num_free_term(local).unwrap(), 0);
        assert_eq!(fun_index(local).unwrap(), 5);
        // A local fun inlines no module/uniq/creator pid.
        assert_bad_arg(fun_module(local));
        assert_bad_arg(fun_function(local));
        assert_bad_arg(fun_uniq_bytes(local));
        assert_bad_arg(fun_creator_pid(local));
    }
}
