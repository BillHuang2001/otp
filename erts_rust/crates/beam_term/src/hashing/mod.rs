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

//! Structural term hashing — the lowest-common-ancestor `make_hash2`.
//!
//! The legacy `make_hash2`/`make_hash2_helper` live in
//! `erts/emulator/beam/erl_term_hashing.c`. This module reproduces the
//! *properties* that matter for the rewrite — a deterministic, structural hash
//! where **equal terms hash equal** and structurally different terms (almost
//! always) hash differently — but **not** `make_hash2`'s exact bit values.
//!
//! # These hash values are internal
//!
//! The exact hash values produced here are **not part of any compatibility
//! contract**. Nothing outside the Rust runtime observes them: they are only
//! compared against other values produced by *this* function (e.g. `beam_ets`
//! bucket selection for `set`/`hash` tables and `ordered_set` map keys). The
//! only guarantee callers may rely on is the hash/equality consistency rule —
//! `beam_term::eq(a, b, atoms)` implies `term_hash(a, atoms) == term_hash(b, atoms)`.
//! A change to the mixing constants or algorithm is therefore *not* a
//! compatibility break.
//!
//! # Known Issues
//!
//! * The hash covers the structural classes the rewrite needs today
//!   (`small`/`big`/`float`, atoms by name, tuples, lists, binaries/bitstrings,
//!   `nil`, internal pids/ports/refs, funs, maps, native records).
//! * **Maps** hash over their pairs in the canonical flat-map key order
//!   ([`crate::map::map_pairs_flatmap_order`]) — see [`hash_map`].
//! * **Funs** hash over exactly the fields [`crate::compare`] compares — see
//!   [`hash_fun`] — so `eq` implies equal hashes for every fun kind.
//! * **Native records** hash over exactly the fields [`crate::compare`] compares —
//!   see [`hash_record`] — so two structurally equal records hash equal whatever
//!   their addresses. (The instance's *definition pointer* is deliberately never
//!   mixed in: it is an address.)
//! * **External pids/ports/refs** hash their *content* words (header, node atom,
//!   `creation`, number/serial/ids), which hold no pointers. The value is therefore
//!   stable across processes/images and already agrees with the structural
//!   comparison of [`crate::compare`]: two externals that compare `=:=` differ only
//!   in fields those words carry.
//! * The remaining classes without a faithful layout-aware representation —
//!   internal references and on-heap bin refs — use an **opaque object fallback**:
//!   the object's raw words are mixed in. This is deterministic for a given runtime
//!   image but, because such an object's words include a pointer (a ref's payload
//!   word, a bin ref's payload/`orig` word), the hash of such a term is not stable
//!   across processes/images. It is only ever used as an *internal* hash, never
//!   compared against the C.
//!
//! # Layout
//!
//! This structural hash (and the shared low-level primitives) live here; the two
//! *compatibility-pinned* ports — `make_hash2` and `make_hash` — live in the
//! `make_hash2` and `make_hash` submodules (see the section comment below).

mod common;
mod make_hash;
mod make_hash2;

pub use make_hash::make_hash;
pub use make_hash2::{compute_record_definition_hash, make_hash2};

use crate::atom::AtomTable;
use crate::bitstring::{bitstring_parts, float_bits};
use crate::boxed::{boxed_val, header_arity, thing_arityval};
use crate::compare::{exported_rank, record_parts};
use crate::eterm::Eterm;
use crate::fun::{fun_arity, fun_desc_words, fun_kind_of_header, fun_num_free, FunKind};
use crate::immediate::{atom_val, internal_pid_data, internal_port_data, signed_value};
use crate::list::{car, cdr, is_list};
use crate::ops::{class, TermClass};
use crate::tag::BIG_SIGN_BIT;
use crate::tuple::{tuple_element, tuple_size};

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

// Per-class discriminators, so structurally similar terms of different types
// (e.g. `{1}` vs `[1]`) hash differently.
const TAG_SMALL: u64 = 0x01;
const TAG_BIG: u64 = 0x02;
const TAG_FLOAT: u64 = 0x03;
const TAG_ATOM: u64 = 0x04;
const TAG_NIL: u64 = 0x05;
const TAG_LIST: u64 = 0x06;
const TAG_TUPLE: u64 = 0x07;
const TAG_BITS: u64 = 0x08;
const TAG_PID: u64 = 0x09;
const TAG_PORT: u64 = 0x0a;
const TAG_OPAQUE: u64 = 0x0b;
/// Marks the transition from the list cells to their (improper/`nil`) tail.
const TAG_LIST_TAIL: u64 = 0x0c;
/// Map discriminator (structural, over the canonical flat-map key order).
const TAG_MAP: u64 = 0x0d;
/// Fun discriminators — one per [`FunKind`], so the three descriptor shapes can
/// never collide.
const TAG_FUN_LOCAL: u64 = 0x0e;
const TAG_FUN_IMPORTED: u64 = 0x0f;
const TAG_FUN_EXTERNAL: u64 = 0x10;
/// Discriminator for a word that is **not a term at all** (a bare header word such
/// as the ABI's `THE_NON_VALUE`; see [`crate::tag::is_term_word`]).
const TAG_NON_VALUE: u64 = 0x11;
/// Native-record discriminator (structural, over exactly the words
/// [`crate::compare::cmp`] compares — see [`hash_record`]).
const TAG_RECORD: u64 = 0x12;

/// Mix a word into a running FNV-1a style hash.
#[inline]
fn mix64(h: u64, word: u64) -> u64 {
    (h ^ word).wrapping_mul(FNV_PRIME)
}

/// A deterministic structural hash of `a`.
///
/// Consistent with [`crate::eq`] / [`crate::cmp`] (equal terms hash equal). See
/// the module docs: the exact values are internal and not a compatibility
/// contract.
///
/// Total and panic-free: a word that is not a term at all (a bare header word, e.g.
/// the ABI's `THE_NON_VALUE`; see [`crate::tag::is_term_word`]) hashes from its raw
/// word — mixed with a per-class discriminator, like every other class — and is
/// never dereferenced. Since [`crate::eq`] calls two non-terms equal only when
/// their words are identical, the `eq ⇒ equal hash` rule still holds.
pub fn term_hash(a: Eterm, atoms: &AtomTable) -> u64 {
    hash_term(a, atoms, FNV_OFFSET)
}

/// Recursively hash a term into the running hash `h`.
fn hash_term(t: Eterm, atoms: &AtomTable, h: u64) -> u64 {
    // SAFETY: `class` is total — it dereferences only the header word of a
    // well-formed boxed term and answers `NonValue` (no dereference) for a word that
    // is not a term.
    match unsafe { class(t) } {
        TermClass::Small => mix64(mix64(h, TAG_SMALL), signed_value(t) as u64),
        TermClass::Big => unsafe { hash_big(t, mix64(h, TAG_BIG)) },
        // SAFETY: `t` is a boxed float.
        TermClass::Float => mix64(mix64(h, TAG_FLOAT), unsafe { float_bits(t) }),
        TermClass::Atom => hash_atom(t, atoms, mix64(h, TAG_ATOM)),
        TermClass::Nil => mix64(h, TAG_NIL),
        // SAFETY: `t` is a cons cell / tuple.
        TermClass::List => unsafe { hash_list(t, atoms, mix64(h, TAG_LIST)) },
        TermClass::Tuple => unsafe { hash_tuple(t, atoms, mix64(h, TAG_TUPLE)) },
        // SAFETY: `t` is a bitstring.
        TermClass::Bitstring => unsafe { hash_bits(t, mix64(h, TAG_BITS)) },
        TermClass::Pid => mix64(mix64(h, TAG_PID), internal_pid_data(t)),
        TermClass::Port => mix64(mix64(h, TAG_PORT), internal_port_data(t)),
        // SAFETY: `t` is a live map.
        TermClass::Map => hash_map(t, atoms, mix64(h, TAG_MAP)),
        // SAFETY: `t` is a live fun.
        TermClass::Fun => unsafe { hash_fun(t, atoms, h) },
        // Native records are hashed structurally, over exactly the fields
        // `crate::compare::cmp` compares (see [`hash_record`]).
        TermClass::Record => hash_record(t, atoms, mix64(h, TAG_RECORD)),
        // A word that is not a term at all: hash its raw word (never dereference it).
        TermClass::NonValue => mix64(mix64(h, TAG_NON_VALUE), t.raw() as u64),
        // Internal refs and on-heap bin refs (the opaque raw-word fallback) plus the
        // external pid/port/ref classes, whose words are content-only and therefore
        // hash consistently with `crate::compare`'s structural arms — see the module
        // `Known Issues`.
        _ => unsafe { hash_words(t, mix64(h, TAG_OPAQUE)) },
    }
}

/// Hash a fun over exactly the fields [`crate::compare::cmp`] compares, so `eq`
/// implies equal hashes for every kind (see [`crate::fun`] and
/// [`crate::compare::cmp`]):
///
/// * [`FunKind::Local`] ⇒ `(uniq, index)`, then `num_free`, then the environment.
/// * [`FunKind::Imported`] ⇒ `(module, index, old_uniq)`, then `num_free`, then
///   the environment.
/// * [`FunKind::External`] ⇒ `(module, function, arity)`.
///
/// The fields the comparison ignores are deliberately not hashed — the fun arity
/// for the two local kinds, and the imported `uniq`/`old_index`/creator pid — so
/// two funs the comparison calls `=:=` always hash equal.
///
/// # Safety
///
/// `t` must be a live fun term.
unsafe fn hash_fun(t: Eterm, atoms: &AtomTable, h: u64) -> u64 {
    let p = boxed_val(t);
    // SAFETY: `t` is a live fun, so word 0 is its (live) header word.
    let hdr = unsafe { *p };
    match fun_kind_of_header(hdr) {
        FunKind::Local => {
            let mut h = mix64(h, TAG_FUN_LOCAL);
            // SAFETY: a local fun's descriptor words 1 and 2 hold `uniq` and `index`.
            h = mix64(h, unsafe { (*p.add(1)).raw() } as u64);
            h = mix64(h, unsafe { (*p.add(2)).raw() } as u64);
            // SAFETY: `p` is the header word of the live local fun `hdr`.
            unsafe { hash_fun_env(p, hdr, atoms, h) }
        }
        FunKind::Imported => {
            let mut h = mix64(h, TAG_FUN_IMPORTED);
            // SAFETY: an imported fun's descriptor word 1 holds its module atom.
            h = hash_term(unsafe { *p.add(1) }, atoms, h);
            // SAFETY: an imported fun's descriptor words 4 and 6 hold `index` and
            // `old_uniq`.
            h = mix64(h, unsafe { (*p.add(4)).raw() } as u64);
            h = mix64(h, unsafe { (*p.add(6)).raw() } as u64);
            // SAFETY: `p` is the header word of the live imported fun `hdr`.
            unsafe { hash_fun_env(p, hdr, atoms, h) }
        }
        FunKind::External => {
            let mut h = mix64(h, TAG_FUN_EXTERNAL);
            // SAFETY: an external fun's descriptor words 1 and 2 hold its module and
            // function atoms.
            h = hash_term(unsafe { *p.add(1) }, atoms, h);
            h = hash_term(unsafe { *p.add(2) }, atoms, h);
            mix64(h, fun_arity(hdr) as u64)
        }
    }
}

/// Hash a local-kind fun's environment: `num_free`, then the `num_free`
/// elements — the same tail [`crate::compare::cmp`] walks.
///
/// # Safety
///
/// `p` must be the header word of the live local-kind fun `hdr`, whose whole
/// block (header + descriptor + `num_free` environment words) must be live.
unsafe fn hash_fun_env(p: *mut Eterm, hdr: Eterm, atoms: &AtomTable, mut h: u64) -> u64 {
    let num_free = fun_num_free(hdr);
    h = mix64(h, num_free as u64);
    let offset = 1 + fun_desc_words(fun_kind_of_header(hdr));
    for i in 0..num_free {
        // SAFETY: environment word `i` is within the fun's payload.
        let elem = unsafe { *p.add(offset + i) };
        h = hash_term(elem, atoms, h);
    }
    h
}

/// Hash a map structurally, over its pairs in the canonical flat-map key order
/// ([`crate::map::map_pairs_flatmap_order`]) — the same order [`crate::compare`]
/// uses, so `eq` implies equal hashes.
///
/// A malformed map (only reachable if the term was forged outside this crate)
/// falls back to the opaque raw-word hash rather than panicking.
fn hash_map(t: Eterm, atoms: &AtomTable, mut h: u64) -> u64 {
    let pairs = match crate::map::map_pairs_flatmap_order(t, atoms) {
        Ok(pairs) => pairs,
        Err(_) => return unsafe { hash_words(t, h) },
    };
    h = mix64(h, pairs.len() as u64);
    for (key, value) in pairs {
        h = hash_term(key, atoms, h);
        h = hash_term(value, atoms, h);
    }
    h
}

/// Hash a native record structurally, over exactly the fields
/// [`crate::compare::cmp`] compares, in the same order (see
/// [`crate::compare::cmp_records`]): the definition's `module` atom, its `name`
/// atom, `is_exported` (`false < true`), the field count, the field **keys** (in the
/// crate's sorted-key storage order) and finally the field **values** (each through
/// [`hash_term`]).
///
/// The instance's word 1 — a *boxed pointer* to the definition block — is
/// deliberately never mixed in: it is an address, so two structurally equal records
/// at different addresses would otherwise hash differently, breaking the
/// `eq ⇒ equal hash` rule.
///
/// The comparable words come from [`crate::compare::record_parts`], the same
/// validated reader [`crate::compare::cmp_records`] uses, so the hash and the
/// comparison cannot drift apart. A malformed record — only reachable if the term
/// was forged outside this crate (a definition that is not a valid definition block,
/// or one whose arity disagrees with the instance's own header) — makes that reader
/// answer `None` and falls back to the opaque raw-word hash rather than panicking or
/// dereferencing garbage.
fn hash_record(t: Eterm, atoms: &AtomTable, mut h: u64) -> u64 {
    // SAFETY: `hash_term` dispatched here on `TermClass::Record`, so `t` is a live
    // native-record instance whose definition block, key array and value words are all
    // live (and stay live while this call reads them).
    let Some(rec) = (unsafe { record_parts(t) }) else {
        // SAFETY: the dispatch guarantees `t` is a live boxed term, which is all the
        // raw-word fallback dereferences.
        return unsafe { hash_words(t, h) };
    };
    // `cmp_records`' field order, exactly.
    h = hash_term(rec.module, atoms, h);
    h = hash_term(rec.name, atoms, h);
    h = mix64(h, exported_rank(rec.is_exported) as u64);
    h = mix64(h, rec.field_count as u64);
    for &key in rec.keys {
        h = hash_term(key, atoms, h);
    }
    for &value in rec.values {
        h = hash_term(value, atoms, h);
    }
    h
}

/// Hash an atom by its name bytes (falling back to the index when the name is
/// unknown to `atoms`).
fn hash_atom(t: Eterm, atoms: &AtomTable, mut h: u64) -> u64 {
    let index = atom_val(t);
    match atoms.name(index) {
        Some(name) => {
            for &byte in name.as_bytes() {
                h = mix64(h, byte as u64);
            }
            mix64(h, name.len() as u64)
        }
        None => mix64(h, index.raw() as u64),
    }
}

/// Hash a bignum by sign and limbs.
///
/// # Safety
///
/// `t` must be a live bignum term.
unsafe fn hash_big(t: Eterm, mut h: u64) -> u64 {
    // SAFETY: `t` is a boxed bignum.
    let p = boxed_val(t);
    let hdr = unsafe { *p };
    h = mix64(h, u64::from(hdr.raw() & BIG_SIGN_BIT != 0));
    let arity = header_arity(hdr);
    h = mix64(h, arity as u64);
    for i in 0..arity {
        // SAFETY: limb `i` is within the bignum payload.
        h = mix64(h, unsafe { (*p.add(1 + i)).raw() } as u64);
    }
    h
}

/// Hash a tuple by arity and elements.
///
/// # Safety
///
/// `t` must be a live tuple term.
unsafe fn hash_tuple(t: Eterm, atoms: &AtomTable, mut h: u64) -> u64 {
    // SAFETY: `t` is a tuple.
    let arity = unsafe { tuple_size(t) };
    h = mix64(h, arity as u64);
    for i in 0..arity {
        // SAFETY: `i < arity`.
        let elem = unsafe { tuple_element(t, i) };
        h = hash_term(elem, atoms, h);
    }
    h
}

/// Hash a cons cell (and the rest of the list) cell-by-cell.
///
/// # Safety
///
/// `t` must be a live non-`NIL` cons cell.
unsafe fn hash_list(mut t: Eterm, atoms: &AtomTable, mut h: u64) -> u64 {
    let mut cells = 0u64;
    loop {
        // SAFETY: `t` is a cons cell.
        let head = unsafe { car(t) };
        h = hash_term(head, atoms, h);
        cells += 1;
        // SAFETY: `t` is a cons cell.
        let tail = unsafe { cdr(t) };
        if is_list(tail) {
            t = tail;
        } else {
            h = mix64(h, TAG_LIST_TAIL);
            h = hash_term(tail, atoms, h);
            break;
        }
    }
    mix64(h, cells)
}

/// Hash a bitstring by bit size and (byte-normalised) contents.
///
/// # Safety
///
/// `t` must be a live bitstring term.
unsafe fn hash_bits(t: Eterm, mut h: u64) -> u64 {
    // SAFETY: `t` is a bitstring.
    let (base, offset, size) = unsafe { bitstring_parts(t) };
    h = mix64(h, size as u64);
    let bytes = size.div_ceil(8);
    for j in 0..bytes {
        let mut byte = 0u8;
        for k in 0..8 {
            let bit = j * 8 + k;
            if bit < size {
                // SAFETY: bit `offset + bit` is within the bitstring.
                byte |= unsafe {
                    (*base.add((offset + bit) >> 3) >> (7 - ((offset + bit) & 7))) & 1
                } << (7 - k);
            }
        }
        h = mix64(h, byte as u64);
    }
    h
}

/// Opaque object fallback: mix the object's raw header + payload words. See the
/// module `Known Issues`.
///
/// # Safety
///
/// `t` must be a live boxed term.
unsafe fn hash_words(t: Eterm, mut h: u64) -> u64 {
    // SAFETY: `t` is boxed.
    let p = boxed_val(t);
    let hdr = unsafe { *p };
    let arity = thing_arityval(hdr);
    for i in 0..=arity {
        // SAFETY: word `i` is within the object.
        h = mix64(h, unsafe { (*p.add(i)).raw() } as u64);
    }
    h
}

// ===========================================================================
// `make_hash2` (`erlang:phash2/1,2`) and `make_hash` (`erlang:phash/2`)
//
// Bit-exact u32-wrapping ports of `erts/emulator/beam/erl_term_hashing.c`:
// `make_hash2`/`make_hash2_helper` (the non-trapping path only — there is no
// process/reds machinery here) and `make_hash`. Unlike [`term_hash`] above,
// **these values are a compatibility contract**: the system ERTS produces them,
// so a Rust term must hash identically (Level-1 drop-in).
//
// Error handling: a well-formed term always hashes. The only *reachable* typed
// failure for a well-formed term is a [`FunKind::Local`] fun (a `make_fun3`
// closure), whose inlined 2-word descriptor carries neither the module nor
// `old_uniq` that the C's `ErlFunEntry` supplies — see [`crate::fun`]. That is
// reported as the dedicated [`crate::error::TermError::FunIdentityUnavailable`]
// (the term is fine; the identity is not carried), never as a substituted value.
// A word that is **not a term at all** — a bare header word, i.e. the C ABI's
// `THE_NON_VALUE` (`0`, or a bare `FLOAT` header in a debug build) and friends;
// see [`crate::tag::is_term_word`] — is reported as
// [`crate::error::TermError::NotATerm`] rather than dereferenced (the C is
// undefined behaviour there). Every other `Err` below is for a forged/unknown
// term layout (unreachable through the safe API) and stays
// [`crate::error::TermError::BadFun`], whose meaning — "this is not a well-formed
// fun" — is exactly right there.
//
// The shared low-level primitives live in the [`common`] submodule; the two
// ports are the [`make_hash2`](self::make_hash2) and
// [`make_hash`](self::make_hash) submodules, re-exported here so
// `beam_term::hashing::{make_hash2, make_hash, compute_record_definition_hash}`
// and `lib.rs`'s `pub use crate::hashing::*;` are unchanged.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::{make_arityval, make_boxed, make_header, make_record_header, HEADER_FLONUM};
    use crate::fun::{fun_words, write_fun_external, write_fun_imported, write_fun_local, FunPid};
    use crate::heap::Heap;
    use crate::immediate::{make_atom, make_small, NIL};
    use crate::ops::TermClass;
    use crate::record::{record_block_words, write_record};
    use crate::tag::HEAP_BITS_SUBTAG;
    use crate::thing::{
        external_pid_size_words, external_port_size_words, external_ref_size_words,
        write_external_pid, write_external_port, write_external_ref,
    };
    use crate::Atom;

    /// Build a [`FunKind::Local`] fun.
    ///
    /// # Safety
    ///
    /// `heap` must own the [`fun_words`]-sized block and outlive the term.
    unsafe fn local_fun(heap: &mut Heap, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
        // SAFETY: the caller keeps the block alive; the writer fills it.
        let hp = unsafe { heap.alloc_words(fun_words(FunKind::Local, env.len())).unwrap() };
        // SAFETY: `hp` addresses exactly `fun_words(Local, env.len())` words.
        unsafe { write_fun_local(hp, 0, uniq, index, env).unwrap() }
    }

    /// Build a [`FunKind::External`] fun.
    ///
    /// # Safety
    ///
    /// As [`local_fun`].
    unsafe fn external_fun(heap: &mut Heap, module: Atom, function: Atom, arity: usize) -> Eterm {
        // SAFETY: the caller keeps the block alive.
        let hp = unsafe { heap.alloc_words(fun_words(FunKind::External, 0)).unwrap() };
        // SAFETY: `hp` addresses exactly `fun_words(External, 0)` words.
        unsafe { write_fun_external(hp, arity, module, function).unwrap() }
    }

    /// Build a [`FunKind::Imported`] fun.
    ///
    /// # Safety
    ///
    /// As [`local_fun`].
    unsafe fn imported_fun(
        heap: &mut Heap,
        module: Atom,
        index: u32,
        old_uniq: u32,
        uniq: [u8; 16],
        env: &[Eterm],
    ) -> Eterm {
        // SAFETY: the caller keeps the block alive.
        let hp = unsafe { heap.alloc_words(fun_words(FunKind::Imported, env.len())).unwrap() };
        // SAFETY: `hp` addresses exactly `fun_words(Imported, env.len())` words.
        unsafe {
            write_fun_imported(
                hp,
                0,
                module,
                &uniq,
                index,
                0,
                old_uniq,
                FunPid {
                    node: module,
                    number: 0,
                    serial: 0,
                    creation: 0,
                },
                env,
            )
            .unwrap()
        }
    }
    fn atoms() -> AtomTable {
        let mut t = AtomTable::new();
        t.seed(["a", "b"]);
        t
    }

    /// Build a 16-bit (2-byte) heap binary from a `u16`.
    unsafe fn heap_binary(heap: &mut Heap, value: u16) -> Eterm {
        let p = unsafe { heap.alloc_words(3).unwrap() };
        unsafe {
            *p = make_header(2, HEAP_BITS_SUBTAG);
            *p.add(1) = Eterm::from_raw(16);
            *p.add(2) = Eterm::from_raw(value as usize);
        }
        make_boxed(p)
    }

    fn float_term(bits: f64) -> Eterm {
        let words: Box<[Eterm; 2]> =
            Box::new([HEADER_FLONUM, Eterm::from_raw(bits.to_bits() as usize)]);
        let leaked: &'static [Eterm; 2] = Box::leak(words);
        make_boxed(leaked.as_ptr())
    }

    /// Build an external pid (`number`, `serial`, `creation` on node atom `node`).
    ///
    /// # Safety
    ///
    /// `heap` must own the [`external_pid_size_words`]-sized block and outlive the
    /// returned term.
    unsafe fn external_pid(
        heap: &mut Heap,
        node: Atom,
        number: u32,
        serial: u32,
        creation: u32,
    ) -> Eterm {
        // SAFETY: the caller keeps the block alive; the writer fills all of it.
        let hp = unsafe { heap.alloc_words(external_pid_size_words()).unwrap() };
        // SAFETY: `hp` addresses exactly `external_pid_size_words()` words.
        unsafe { write_external_pid(hp, make_atom(node), number, serial, creation) }
    }

    /// Build an external port (`number`, `creation` on node atom `node`).
    ///
    /// # Safety
    ///
    /// As [`external_pid`], with [`external_port_size_words`].
    unsafe fn external_port(heap: &mut Heap, node: Atom, number: u64, creation: u32) -> Eterm {
        // SAFETY: the caller keeps the block alive; the writer fills all of it.
        let hp = unsafe { heap.alloc_words(external_port_size_words()).unwrap() };
        // SAFETY: `hp` addresses exactly `external_port_size_words()` words.
        unsafe { write_external_port(hp, make_atom(node), number, creation) }
    }

    /// Build an external reference (`ids`, `creation` on node atom `node`).
    ///
    /// # Safety
    ///
    /// As [`external_pid`], with [`external_ref_size_words`]`(ids.len())`;
    /// `ids` must be non-empty and at most `MAX_REF_IDS` long.
    unsafe fn external_ref(heap: &mut Heap, node: Atom, ids: &[u32], creation: u32) -> Eterm {
        // SAFETY: the caller keeps the block alive; the writer fills all of it.
        let hp = unsafe { heap.alloc_words(external_ref_size_words(ids.len())).unwrap() };
        // SAFETY: `hp` addresses exactly `external_ref_size_words(ids.len())` words.
        unsafe { write_external_ref(hp, make_atom(node), ids, creation) }
    }

    /// Build a native record (`module`/`name`/`is_exported` + the `(key, value)`
    /// fields, in declaration order) on a fresh block of `heap`.
    ///
    /// # Safety
    ///
    /// `heap` must own the [`record_block_words`]-sized block and outlive the term.
    unsafe fn record(
        heap: &mut Heap,
        module: Atom,
        name: Atom,
        is_exported: bool,
        fields: &[(Eterm, Eterm)],
    ) -> Eterm {
        // SAFETY: the caller keeps the block alive; the writer fills all of it. The
        // `definition_hash` word is not part of the hash ([`hash_record`] hashes
        // exactly the fields `cmp_records` compares, and the cached hash is not one of
        // them), so the placeholder is fine here.
        let hp = unsafe { heap.alloc_words(record_block_words(fields.len())).unwrap() };
        // SAFETY: `hp` addresses exactly `record_block_words(fields.len())` words.
        unsafe { write_record(hp, module, name, is_exported, fields, make_small(0)).unwrap() }
    }
    #[test]
    fn equal_terms_hash_equal_across_heaps() {
        let a = atoms();
        let mut h1 = Heap::new(32);
        let mut h2 = Heap::new(32);
        // Two independently-built but structurally equal nested tuples.
        let inner1 = h1.tuple(&[make_small(1), make_small(2)]).unwrap();
        let outer1 = h1.tuple(&[inner1, make_atom(a.lookup("a").unwrap())]).unwrap();
        let inner2 = h2.tuple(&[make_small(1), make_small(2)]).unwrap();
        let outer2 = h2.tuple(&[inner2, make_atom(a.lookup("a").unwrap())]).unwrap();
        assert_eq!(term_hash(outer1, &a), term_hash(outer2, &a));
        // Lists too.
        let tail1 = h1.cons(make_small(2), NIL).unwrap();
        let l1 = h1.cons(make_small(1), tail1).unwrap();
        let tail2 = h2.cons(make_small(2), NIL).unwrap();
        let l2 = h2.cons(make_small(1), tail2).unwrap();
        assert_eq!(term_hash(l1, &a), term_hash(l2, &a));
    }

    #[test]
    fn different_terms_hash_differently() {
        let a = atoms();
        let mut heap = Heap::new(64);
        let tup = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let list_tail = heap.cons(make_small(2), NIL).unwrap();
        let list = heap.cons(make_small(1), list_tail).unwrap();
        let other = heap.tuple(&[make_small(1), make_small(3)]).unwrap();
        let hashes = vec![
            term_hash(make_small(1), &a),
            term_hash(make_small(2), &a),
            term_hash(NIL, &a),
            term_hash(make_atom(a.lookup("a").unwrap()), &a),
            term_hash(make_atom(a.lookup("b").unwrap()), &a),
            term_hash(tup, &a),
            term_hash(list, &a),
            term_hash(other, &a),
        ];
        let n = hashes.len();
        let mut sorted = hashes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), n, "expected distinct hashes: {hashes:?}");
    }

    #[test]
    fn atoms_hash_by_name_not_index() {
        let mut t1 = AtomTable::new();
        t1.seed(["x", "a"]);
        let mut t2 = AtomTable::new();
        t2.seed(["a"]);
        // "a" is index 1 in t1 but index 0 in t2; the hash must still match.
        let a1 = make_atom(t1.lookup("a").unwrap());
        let a2 = make_atom(t2.lookup("a").unwrap());
        assert_eq!(term_hash(a1, &t1), term_hash(a2, &t2));
    }

    #[test]
    fn numeric_hashes_are_class_sensitive() {
        let a = atoms();
        // 1 and 1.0 are `==` but not `=:=`; they must hash differently.
        assert_ne!(
            term_hash(make_small(1), &a),
            term_hash(float_term(1.0), &a)
        );
        // Same bits hash the same.
        assert_eq!(
            term_hash(float_term(2.5), &a),
            term_hash(float_term(2.5), &a)
        );
    }

    #[test]
    fn binaries_hash_by_content() {
        let a = atoms();
        let mut heap = Heap::new(32);
        // SAFETY: the heap owns the binaries.
        let (b1, b2, b3) = unsafe {
            (
                heap_binary(&mut heap, 0x0102),
                heap_binary(&mut heap, 0x0102),
                heap_binary(&mut heap, 0x0103),
            )
        };
        assert_eq!(term_hash(b1, &a), term_hash(b2, &a));
        assert_ne!(term_hash(b1, &a), term_hash(b3, &a));
    }

    #[test]
    fn funs_hash_by_their_compared_fields() {
        let a = atoms();
        let mut heap = Heap::new(256);
        let m = a.lookup("a").unwrap();
        let n = a.lookup("b").unwrap();
        // SAFETY: the heap owns every block built here.
        let (l1, l1_copy, l_uniq, l_index, l_env, e1, e1_copy, e_arity, i1, i1_copy, i_index, i_uniq_bytes) =
            unsafe {
                (
                    local_fun(&mut heap, 7, 3, &[]),
                    local_fun(&mut heap, 7, 3, &[]),
                    local_fun(&mut heap, 8, 3, &[]),
                    local_fun(&mut heap, 7, 4, &[]),
                    local_fun(&mut heap, 7, 3, &[make_small(1)]),
                    external_fun(&mut heap, m, n, 2),
                    external_fun(&mut heap, m, n, 2),
                    external_fun(&mut heap, m, n, 3),
                    imported_fun(&mut heap, m, 1, 0, [1u8; 16], &[]),
                    imported_fun(&mut heap, m, 1, 0, [1u8; 16], &[]),
                    imported_fun(&mut heap, m, 2, 0, [1u8; 16], &[]),
                    // Only the `uniq` bytes differ: the comparison ignores them, so
                    // the hash must too.
                    imported_fun(&mut heap, m, 1, 0, [2u8; 16], &[]),
                )
            };

        // Equal funs hash equal, for every kind.
        assert_eq!(term_hash(l1, &a), term_hash(l1_copy, &a));
        assert_eq!(term_hash(e1, &a), term_hash(e1_copy, &a));
        assert_eq!(term_hash(i1, &a), term_hash(i1_copy, &a));
        // ... including an imported fun that differs only in the ignored `uniq`.
        assert!(crate::eq(i1, i_uniq_bytes, &a));
        assert_eq!(term_hash(i1, &a), term_hash(i_uniq_bytes, &a));

        // Structurally different funs hash differently.
        let distinct = [
            term_hash(l1, &a),
            term_hash(l_uniq, &a),
            term_hash(l_index, &a),
            term_hash(l_env, &a),
            term_hash(e1, &a),
            term_hash(e_arity, &a),
            term_hash(i1, &a),
            term_hash(i_index, &a),
        ];
        let n = distinct.len();
        let mut sorted = distinct.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), n, "expected distinct hashes: {distinct:?}");
    }

    #[test]
    fn fun_hash_agrees_with_eq_across_every_pair() {
        let a = atoms();
        let mut heap = Heap::new(256);
        let m = a.lookup("a").unwrap();
        let n = a.lookup("b").unwrap();
        // SAFETY: the heap owns every block built here.
        let samples = unsafe {
            vec![
                local_fun(&mut heap, 1, 1, &[]),
                local_fun(&mut heap, 1, 1, &[]),
                local_fun(&mut heap, 2, 1, &[make_small(0)]),
                external_fun(&mut heap, m, n, 1),
                external_fun(&mut heap, m, n, 1),
                external_fun(&mut heap, n, m, 1),
                imported_fun(&mut heap, m, 1, 0, [0u8; 16], &[]),
                imported_fun(&mut heap, m, 1, 0, [0u8; 16], &[]),
                imported_fun(&mut heap, m, 1, 5, [0u8; 16], &[]),
            ]
        };
        for x in &samples {
            for y in &samples {
                if crate::eq(*x, *y, &a) {
                    assert_eq!(
                        term_hash(*x, &a),
                        term_hash(*y, &a),
                        "eq terms must hash equal: {x:?} {y:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn hash_agrees_with_eq_over_samples() {
        let a = atoms();
        let mut heap = Heap::new(64);
        let t = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let t2 = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        // `eq` => equal hashes.
        assert!(crate::eq(t, t2, &a));
        assert_eq!(term_hash(t, &a), term_hash(t2, &a));
    }

    /// The consistency rule for a second, field-identical copy of an external
    /// pid/port/ref and of a native record — each copy living at a **different**
    /// address (its own [`Heap`]), which is exactly what breaks a raw-word hash.
    #[test]
    fn copies_of_externals_and_records_hash_equal() {
        let mut t = AtomTable::new();
        t.seed(["m", "r", "node", "a", "b"]);
        let m = t.lookup("m").unwrap();
        let r = t.lookup("r").unwrap();
        let node = t.lookup("node").unwrap();
        let ka = make_atom(t.lookup("a").unwrap());
        let kb = make_atom(t.lookup("b").unwrap());

        let mut h1 = Heap::new(128);
        let mut h2 = Heap::new(128);
        // SAFETY: each heap owns its own blocks and outlives every term built here.
        let (pid1, pid2, port1, port2, ref1, ref2, rec1, rec2) = unsafe {
            (
                external_pid(&mut h1, node, 1, 2, 3),
                external_pid(&mut h2, node, 1, 2, 3),
                external_port(&mut h1, node, 7, 3),
                external_port(&mut h2, node, 7, 3),
                external_ref(&mut h1, node, &[1, 2, 3], 3),
                external_ref(&mut h2, node, &[1, 2, 3], 3),
                record(
                    &mut h1,
                    m,
                    r,
                    true,
                    &[(ka, make_small(1)), (kb, make_small(2))],
                ),
                record(
                    &mut h2,
                    m,
                    r,
                    true,
                    &[(ka, make_small(1)), (kb, make_small(2))],
                ),
            )
        };
        for (x, y, what) in [
            (pid1, pid2, "external pid"),
            (port1, port2, "external port"),
            (ref1, ref2, "external ref"),
            (rec1, rec2, "record"),
        ] {
            // The test is only meaningful if the two copies really are separately
            // allocated (a record's word 1 is a definition *pointer*).
            assert_ne!(
                x.raw(),
                y.raw(),
                "{what}: the two copies must live at different words"
            );
            assert!(crate::eq(x, y, &t), "{what}: the copies must compare equal");
            assert_eq!(
                term_hash(x, &t),
                term_hash(y, &t),
                "{what}: eq terms must hash equal"
            );
        }
    }

    /// Two records differing in exactly one compared field hash differently.
    #[test]
    fn distinct_records_hash_differently() {
        let mut t = AtomTable::new();
        t.seed(["m", "m2", "r", "r2", "a", "b", "c"]);
        let m = t.lookup("m").unwrap();
        let m2 = t.lookup("m2").unwrap();
        let r = t.lookup("r").unwrap();
        let r2 = t.lookup("r2").unwrap();
        let ka = make_atom(t.lookup("a").unwrap());
        let kb = make_atom(t.lookup("b").unwrap());
        let kc = make_atom(t.lookup("c").unwrap());

        let mut heap = Heap::new(512);
        // SAFETY: `heap` owns every block built here and outlives them all.
        let hashes = unsafe {
            [
                // base
                term_hash(
                    record(&mut heap, m, r, false, &[(ka, make_small(1)), (kb, make_small(2))]),
                    &t,
                ),
                // module differs
                term_hash(
                    record(&mut heap, m2, r, false, &[(ka, make_small(1)), (kb, make_small(2))]),
                    &t,
                ),
                // name differs
                term_hash(
                    record(&mut heap, m, r2, false, &[(ka, make_small(1)), (kb, make_small(2))]),
                    &t,
                ),
                // is_exported differs
                term_hash(
                    record(&mut heap, m, r, true, &[(ka, make_small(1)), (kb, make_small(2))]),
                    &t,
                ),
                // field count differs
                term_hash(record(&mut heap, m, r, false, &[(ka, make_small(1))]), &t),
                // a key differs (same field count)
                term_hash(
                    record(&mut heap, m, r, false, &[(ka, make_small(1)), (kc, make_small(2))]),
                    &t,
                ),
                // a value differs (same keys)
                term_hash(
                    record(&mut heap, m, r, false, &[(ka, make_small(1)), (kb, make_small(9))]),
                    &t,
                ),
            ]
        };
        let n = hashes.len();
        let mut sorted = hashes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), n, "expected distinct hashes: {hashes:?}");
    }

    /// A forged `RECORD_SUBTAG` term is never guessed at, never panics and always
    /// takes the deterministic raw-word fallback.
    #[test]
    fn malformed_record_takes_the_opaque_fallback() {
        let a = atoms();

        // (1) A `RECORD_SUBTAG` instance whose *definition* word is not a boxed
        // pointer: `class` still reports a record, but `record_parts` answers `None`.
        let words = [make_record_header(2), NIL, make_small(0)];
        let forged = make_boxed(words.as_ptr());
        // SAFETY: `forged` is a live 3-word box on this frame.
        assert_eq!(unsafe { crate::ops::class(forged) }, TermClass::Record);
        // SAFETY: `forged` is a live boxed term, so the fallback may read its words.
        // The fallback reuses the already-mixed hash (the `TAG_RECORD` discriminator —
        // exactly as [`hash_map`] falls back with `TAG_MAP` mixed in).
        assert_eq!(
            term_hash(forged, &a),
            unsafe { hash_words(forged, mix64(FNV_OFFSET, TAG_RECORD)) }
        );
        // ... and the fallback is deterministic (the same term, the same value).
        assert_eq!(term_hash(forged, &a), term_hash(forged, &a));

        // (2) A record whose definition arity disagrees with the instance's own field
        // count: `is_arity_value`-shaped definition, so only the lengths catch it.
        let module = make_atom(a.lookup("a").unwrap());
        let name = make_atom(a.lookup("b").unwrap());
        let def_words = [
            make_arityval(7), // a definition arity => 2 keys
            NIL,              // field_order (not read by the hash)
            make_small(0),    // cached hash
            module,
            name,
            name, // is_exported (never an atom in a real record)
            module,
            name, // key 0, key 1
        ];
        let inst_words = [
            make_record_header(2), // arity 2 => 1 field, so the lengths disagree
            make_boxed(def_words.as_ptr()),
            make_small(1),
        ];
        let forged2 = make_boxed(inst_words.as_ptr());
        // SAFETY: `forged2` and the definition block it points at are live on this frame.
        assert_eq!(unsafe { crate::ops::class(forged2) }, TermClass::Record);
        // SAFETY: `forged2` is a live boxed term.
        assert_eq!(
            term_hash(forged2, &a),
            unsafe { hash_words(forged2, mix64(FNV_OFFSET, TAG_RECORD)) }
        );
    }
}
