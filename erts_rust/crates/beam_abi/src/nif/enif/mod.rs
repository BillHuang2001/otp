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

//! The `enif_*` host functions — the Rust side of the NIF C ABI (workspace contract **A.3**).
//!
//! Every function here is a real `#[no_mangle] pub unsafe extern "C" fn` whose signature matches the
//! corresponding field of the frozen [`super::api::ErlNifApi`] table **exactly**. Each one drives the
//! per-call [`NifEnv`]/[`super::host::NifHost`] seam in [`super::host`]: term construction goes
//! through `beam_term` writers over the host's [`beam_alloc::Heap`], atoms through
//! `NifHost::intern_atom`.
//!
//! [`enif_api_table`] exposes the `#[used] static ENIF_API` — the runtime's `enif_*` surface as a
//! populated table. The `#[used]` static is what keeps the `#[no_mangle]` symbols in the linkage of a
//! binary that pulls this crate in (a loaded NIF `.so` resolves them through `--export-dynamic`).
//!
//! # One file per `enif_*` responsibility
//!
//! This module root owns only the shared helpers, the submodule wiring and the assembled table. The
//! implementations live one file per responsibility, and each of those files also owns a
//! `pub(super) const fn populate` that fills in **its own** slots — so a later workstream extends a
//! family by editing that family's file, never this one. `ENIF_API` chains the families
//! innermost→outermost:
//!
//! | file | `enif_*` family |
//! |---|---|
//! | [`mem`] | allocation, the binary family and the ETF pair |
//! | [`get`] | the `enif_get_*` accessors |
//! | [`make`] | the `enif_make_*` constructors (maps excepted) |
//! | [`map`] | the map accessors and constructors |
//! | [`pred`] | `enif_is_*`, `enif_compare`, `enif_is_identical`, `enif_hash`, `enif_term_size`, `enif_term_type` |
//! | [`proc`] | pid/port/self accessors and the atom-cache queries |
//! | [`exception`] | `enif_make_badarg`, `enif_raise_exception`, `enif_has_pending_exception`, `enif_clear_env` |
//! | [`threads`] | mutex/cond/rwlock/tsd/thread (a thin facade over [`crate::sync`]) |
//! | [`ioq`] | the I/O queue and I/O vector family |
//! | [`mapiter`] | the `enif_map_iterator_*` family |
//! | [`time`] | time, env, system info, unique integers, timeslice, `dl*` |
//! | [`resources`] | the resource-type family |
//! | [`select`] | `enif_select` / `enif_select_x` |
//! | [`process`] | monitors, send, port command, whereis, `enif_schedule_nif` |
//! | [`envs`] | `enif_alloc_env` / `enif_free_env` / `enif_make_copy` / `enif_priv_data` |
//! | [`variadic`] | the seven C-variadic slots (C shim behind `cfg(beam_abi_variadic_shim)`) |
//!
//! # The slots still `None`
//!
//! Only the seven **C-variadic** slots (`enif_make_tuple`, `enif_make_list`, `enif_set_option`,
//! `enif_fprintf`, `enif_snprintf`, `enif_vfprintf`, `enif_vsnprintf`) can be `None`: `...` in a `fn`
//! *definition* is nightly-only (rust-lang/rust#44930), so [`variadic`] defines them in the C shim
//! `csrc/nif_variadic.c`, which handles the `va_list` and forwards straight back into this crate's
//! `#[no_mangle]` helpers. `build.rs` sets the cfg only when it could compile the shim; **without a C
//! compiler those seven slots stay `None`**.
//!
//! `enif_alloc_env` — the last non-variadic slot to be filled — is populated by [`envs`]. It takes no
//! argument, so it reaches the host of the NIF call the calling thread is inside through the
//! thread-local stack `super::host::NifEnv` publishes (and its `Drop` withdraws), and answers `NULL`
//! — never a wrong term — when no env is live or the host has no `alloc_env` support.
//!
//! # Honest limitations (never a wrong result)
//!
//! * **Integers** are handled for small ints and for bignums up to one 64-bit limb; anything wider
//!   fails the accessor (returns `0`) instead of truncating.
//! * **Atom comparison / hashing** ([`enif_compare`], [`enif_is_identical`], [`enif_hash`]) has no
//!   env parameter: identical terms are handled by a raw-word fast path, and distinct atoms fall
//!   back to `beam_term`'s index fallback. [`enif_compare`] additionally orders a **top-level** atom
//!   pair by name, resolved through the current NIF host (`super::host::with_current_host`); an atom
//!   pair *nested* inside a compound still uses the index fallback (see [`pred`]).
//! * **Map operations** use an empty `AtomTable` for the same reason; they are exact whenever the
//!   key terms are not atoms (the overwhelmingly common NIF case, where the key is a term the NIF
//!   was handed).
//! * **No scheduler threads.** [`enif_thread_type`] reports only [`crate::sync`]'s answer —
//!   `ERL_NIF_THR_UNDEFINED` for a foreign thread, `ERL_NIF_THR_OTHER` for a created one — and an
//!   unnamed handle's `*_name` accessor returns `NULL` where the C returns the static string
//!   `"unknown"`.
//! * [`enif_make_binary`] builds an **on-heap** binary (off-heap refcounted binaries are a
//!   `beam_alloc` concern not yet exposed here); [`enif_inspect_binary`] refuses a *bit*-offset base
//!   instead of fabricating an unaligned pointer.

use libc::c_char;

use beam_term::atom::AtomTable;
use beam_term::prelude::{self, Eterm, NIL};

use crate::nif::api::{ErlNifApi, ERL_NIF_API_FIELD_COUNT};
use crate::nif::host::NifEnv;
use crate::nif::term::{eterm_from_nif, nif_from_eterm, ERL_NIF_TERM};

// The table this module populates must keep its frozen shape.
const _: () = assert!(ERL_NIF_API_FIELD_COUNT == 176);

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Recover the live [`NifEnv`] behind an opaque pointer (`None` when null).
///
/// # Safety
///
/// `env` must be null, or a pointer previously produced by `NifEnv::as_ptr` for an env that is still
/// alive (with its host still alive).
#[inline]
pub(crate) unsafe fn env_mut<'a>(env: *mut crate::nif::types::ErlNifEnv) -> Option<&'a mut NifEnv> {
    // SAFETY: delegated to `NifEnv::from_opaque`, which checks for null and documents the same
    // contract (pointer from `as_ptr` of a live env, whose host outlives the borrow).
    unsafe { NifEnv::from_opaque(env) }
}

/// A borrowed `&str` view over a caller-provided C string (`None` if the pointer is null).
///
/// # Safety
///
/// `ptr` must be null or point at a NUL-terminated byte sequence valid for the call.
#[inline]
pub(crate) unsafe fn cstr<'a>(ptr: *const c_char) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a NUL-terminated string; `CStr::from_ptr` only reads up to it.
    Some(unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes())
}

/// A borrowed view over `len` bytes at `ptr` (`None` if null and `len > 0`).
///
/// # Safety
///
/// `ptr` must point at `len` readable bytes (a null `ptr` is accepted only for `len == 0`).
#[inline]
pub(crate) unsafe fn cbuf<'a>(ptr: *const c_char, len: usize) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return if len == 0 { Some(&[]) } else { None };
    }
    // SAFETY: the caller guarantees `len` readable bytes at `ptr`.
    Some(unsafe { core::slice::from_raw_parts(ptr.cast::<u8>(), len) })
}

/// NIF term → internal term.
#[inline]
pub(crate) fn t(term: ERL_NIF_TERM) -> Eterm {
    eterm_from_nif(term)
}

/// Internal term → NIF term.
#[inline]
pub(crate) fn n(term: Eterm) -> ERL_NIF_TERM {
    nif_from_eterm(term)
}

/// A single-value term-out write: `true` when `ptr` was non-null and received `value`.
///
/// # Safety
///
/// `ptr` must be null or valid for one write of `T`.
#[inline]
pub(crate) unsafe fn write_out<T>(ptr: *mut T, value: T) -> bool {
    if ptr.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees `ptr` is valid for one write; null was handled above.
    unsafe { ptr.write(value) };
    true
}

/// Allocate `words` uninitialized heap words from the env's host heap (`None` on exhaustion).
///
/// # Safety
///
/// The returned pointer is only valid while the env (and hence the host heap) is alive.
#[inline]
pub(crate) unsafe fn alloc(env: &mut NifEnv, words: usize) -> Option<*mut Eterm> {
    // SAFETY: a plain bump allocation of `words` words; exhaustion is reported as `None`.
    unsafe { env.heap().alloc_words(words) }.ok()
}

/// The `badarg` atom for `env`, or `NIL` when the host cannot intern atoms.
pub(crate) fn badarg_atom(env: &mut NifEnv) -> Eterm {
    env.host().intern_atom("badarg").unwrap_or(NIL)
}

/// `badarg` as a NIF term without raising (used where no env is available).
pub(crate) const fn bare_badarg() -> ERL_NIF_TERM {
    ERL_NIF_TERM::from_raw(NIL.raw())
}

/// The `undefined` atom term used for an "undefined" `ErlNifPid` — `NIL` (`[]`), so an undefined pid
/// is a value that can never be a live internal pid term.
pub(crate) const UNDEFINED_PID: ERL_NIF_TERM = ERL_NIF_TERM::from_raw(NIL.raw());

/// Whether `t` is a pid (internal or external).
#[inline]
pub(crate) fn is_pid(t: Eterm) -> bool {
    if !is_term_word(t) {
        return false;
    }
    prelude::is_internal_pid(t)
        // SAFETY: `is_external_pid` only dereferences the header of a boxed term.
        || unsafe { prelude::is_external_pid(t) }
}

/// Whether `t` is a port (internal or external).
#[inline]
pub(crate) fn is_port(t: Eterm) -> bool {
    if !is_term_word(t) {
        return false;
    }
    prelude::is_internal_port(t)
        // SAFETY: as `is_pid`.
        || unsafe { prelude::is_external_port(t) }
}

/// Whether `t` is a reference (internal or external).
#[inline]
pub(crate) fn is_ref(t: Eterm) -> bool {
    if !is_term_word(t) {
        return false;
    }
    // SAFETY: the predicates only dereference the header of a boxed term.
    unsafe { prelude::is_internal_ref(t) || prelude::is_external_ref(t) }
}

/// A fresh (empty) atom table. Used by the map/compare API, which has no env (see the module docs).
pub(crate) fn no_atoms() -> AtomTable {
    AtomTable::new()
}

/// Whether `term` is a *term word at all* — i.e. it is not the invalid `HEADER` pattern.
///
/// A term carries one of the primary tags `LIST`, `BOXED` or `IMMED1`; the `HEADER` pattern
/// (primary tag `0` — the raw word `0`, `THE_NON_VALUE`, or any bare header word) is **not** a term.
/// The C's `is_boxed` bit trick classifies it as *boxed* (`(x & (MASK-BOXED)) == 0` holds for `0`),
/// so `is_binary`/`is_tuple`/`size_object`/… would dereference a null pointer on it — in the C that
/// is a `SIGSEGV`. Every `enif_*` entry point below therefore rejects such a word up front and
/// returns the C-safe default (`0`, `NULL`, `None`), so an errored or hostile NIF cannot take the
/// runtime down. This is strictly *safer* than the C and never changes the answer for a valid term.
#[inline]
pub(crate) fn is_term_word(term: Eterm) -> bool {
    (term.raw() & prelude::TAG_PRIMARY_MASK) != prelude::TAG_PRIMARY_HEADER
}
// ---------------------------------------------------------------------------------------------
// Integer conversion (C `term_to_Sint` / `term_to_Uint`, big.c)
// ---------------------------------------------------------------------------------------------

/// `term_to_Sint` — the term's value as an `i64`, or `None` when it is not an integer that fits.
pub(crate) fn term_to_sint64(t: Eterm) -> Option<i64> {
    if !is_term_word(t) {
        return None;
    }
    if prelude::is_small(t) {
        return Some(prelude::signed_value(t));
    }
    // SAFETY: `is_big` only dereferences the header of a boxed term.
    if !unsafe { prelude::is_big(t) } {
        return None;
    }
    // SAFETY: `t` is a live bignum (just tested), so `big_parts` is in contract.
    let (negative, digits) = unsafe { prelude::big_parts(t) };
    // `xl * D_EXP > 64` rejects anything wider than one 64-bit limb (big.c:3666).
    if digits.len() != 1 {
        return None;
    }
    let magnitude = digits[0];
    if negative {
        // `-2^63` is representable; a larger magnitude is not.
        if magnitude > (1u64 << 63) {
            return None;
        }
        Some((magnitude as i64).wrapping_neg())
    } else {
        if magnitude > i64::MAX as u64 {
            return None;
        }
        Some(magnitude as i64)
    }
}

/// `term_to_Uint` — the term's value as a `u64`, or `None` (negative, or wider than 64 bits).
pub(crate) fn term_to_uint64(t: Eterm) -> Option<u64> {
    if !is_term_word(t) {
        return None;
    }
    if prelude::is_small(t) {
        let v = prelude::signed_value(t);
        return if v < 0 { None } else { Some(v as u64) };
    }
    // SAFETY: `is_big` only dereferences the header of a boxed term.
    if !unsafe { prelude::is_big(t) } {
        return None;
    }
    // SAFETY: `t` is a live bignum (just tested).
    let (negative, digits) = unsafe { prelude::big_parts(t) };
    if negative || digits.len() != 1 {
        return None;
    }
    Some(digits[0])
}

// ---------------------------------------------------------------------------------------------
// Submodules — one file per `enif_*` responsibility
// ---------------------------------------------------------------------------------------------

pub mod envs;
pub mod exception;
pub mod get;
pub mod ioq;
pub mod make;
pub mod map;
pub mod mapiter;
pub mod mem;
pub mod pred;
pub mod proc;
pub mod process;
pub mod resources;
pub mod select;
pub mod threads;
pub mod time;
pub mod variadic;

pub use self::{exception::*, get::*, make::*, map::*, mem::*, pred::*, proc::*};

// The pre-created stub families export no public item yet (their only item is the `pub(super)
// const fn populate`), so these globs import nothing today. The allow keeps the wiring in place for
// the workstreams that fill them in: a `pub unsafe extern "C" fn enif_*` added to one of those files
// is then reachable as `nif::enif::enif_*` without touching this file.
#[allow(unused_imports)]
pub use self::{
    envs::*, ioq::*, mapiter::*, process::*, resources::*, select::*, threads::*, time::*,
    variadic::*,
};

// ---------------------------------------------------------------------------------------------
// The `#[used]` table
// ---------------------------------------------------------------------------------------------

/// The runtime's populated `enif_*` table.
///
/// `#[used]` keeps this static (and hence the `#[no_mangle]` functions it names) in the linkage, so a
/// NIF `.so` loaded into the process can resolve them. Each slot is filled in by the `populate` of
/// the file that owns it; slots left `None` are documented in the module header.
///
/// The chain is hand-laid-out (one family per line, outermost first) so that the composition order
/// stays obvious; `#[rustfmt::skip]` keeps it that way.
#[used]
#[rustfmt::skip]
static ENIF_API: ErlNifApi = variadic::populate(
    envs::populate(
        process::populate(
            select::populate(
                resources::populate(
                    time::populate(
                        mapiter::populate(
                            ioq::populate(
                                threads::populate(
                                    exception::populate(
                                        proc::populate(
                                            pred::populate(
                                                map::populate(
                                                    make::populate(
                                                        get::populate(
                                                            mem::populate(
                                                                ErlNifApi::empty(),
                                                            ),
                                                        ),
                                                    ),
                                                ),
                                            ),
                                        ),
                                    ),
                                ),
                            ),
                        ),
                    ),
                ),
            ),
        ),
    ),
);

/// The populated `enif_*` table (see [`ENIF_API`]).
pub fn enif_api_table() -> &'static ErlNifApi {
    &ENIF_API
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
