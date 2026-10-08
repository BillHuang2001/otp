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

//! `enif_*` — the map-iterator family (workspace contract **A.3**).
//!
//! Owned slots: `enif_map_iterator_create`, `enif_map_iterator_destroy`, `enif_map_iterator_is_head`,
//! `enif_map_iterator_is_tail`, `enif_map_iterator_next`, `enif_map_iterator_prev`,
//! `enif_map_iterator_get_pair`.
//!
//! The family binds one `ErlNifMapIterator` (the frozen 56-byte struct in `crate::nif::types`) and
//! walks the map it was created on in the map's **storage order** — the C's
//! `hashmap_iterator_next` DFS slot order (`erl_map.c:2346`) / the flat map's keys-tuple order, which
//! is exactly the order `maps:to_list/1` reports. Nothing here touches the scheduler or the runtime:
//! the whole family is a pure function of the frozen struct plus `beam_term`'s map interior, so — like
//! the rest of the `beam_abi` host layer — it lives entirely behind the frozen C ABI.
//!
//! # Reproduced semantics (ground truth `erts/emulator/beam/erl_nif.c:3773-3952`)
//!
//! | function | C | behaviour reproduced |
//! |---|---|---|
//! | [`enif_map_iterator_create`] | `erl_nif.c:3773` | positions at `FIRST`/`LAST`; `0` for a non-map; the empty-flat-map wrap-around (`offset = size - 1` ⇒ `idx = 0`, the C's own "bad shape" comment) |
//! | [`enif_map_iterator_destroy`] | `erl_nif.c:3831` | releases the hash arm's cache; the flat arm owns nothing |
//! | [`enif_map_iterator_is_head`] | `erl_nif.c:3859` | flat: `size == 0 \|\| idx == 0`; hash: `idx == 0` |
//! | [`enif_map_iterator_is_tail`] | `erl_nif.c:3845` | flat: `size == 0 \|\| idx > size`; hash: `idx > size` |
//! | [`enif_map_iterator_next`] | `erl_nif.c:3874` | steps forward when `idx <= size`, returns `idx <= size` afterwards |
//! | [`enif_map_iterator_prev`] | `erl_nif.c:3900` | steps back when `idx > 0`, returns `idx > 0` afterwards |
//! | [`enif_map_iterator_get_pair`] | `erl_nif.c:3926` | the pair at `idx` **only** when `0 < idx <= size`; `0` — and no writes — at HEAD/TAIL |
//!
//! The C carries the current pair in `u` (`ks`/`vs` into the map's own arrays for a flat map, `kv`
//! into the work stack for a hash map) and derives the *binding* rule from `idx` alone:
//! `get_pair` is exactly `idx ∈ (0, size]` ⇒ the `idx`-th pair of the storage-order sequence. This
//! implementation reproduces both the state **and** that rule, field for field.
//!
//! # Documented deviations from the oracle
//!
//! * **The hash arm's cache is a pair list, not an `ErtsDynamicWStack`.** The C allocates an
//!   `ErtsDynamicWStack` in `create` (`erl_nif.c:3803`) and frees it in `destroy`
//!   (`erl_nif.c:3835`); this implementation allocates a `Box`ed vector holding the map's pairs in
//!   storage order in exactly the same slot (`u.hash.wstack`) and frees it in exactly the same way.
//!   The slot's *type* is therefore nominal: `u.hash.wstack` must not be interpreted as an
//!   `ErtsDynamicWStack`. `u.hash.kv` is kept faithful (it points at the current pair's two
//!   contiguous words, so `CAR(kv)`/`CDR(kv)` read key/value as the C does), but it points into that
//!   vector rather than into the C's work stack. The struct's fields are documented as internal and
//!   subject to change by the frozen header itself.
//! * **A hash iterator's pair list is computed once, at `create`.** The C's work stack advances a
//!   DFS cursor per `next`/`prev`; here the order is materialised once (via
//!   `beam_term::map::map_pairs_storage_order`, which reproduces the C's walk order) and the cursor
//!   is the index. Observably identical — `get_pair` answers the same sequence — and O(1) per step
//!   rather than O(subtree) per step.
//! * **`create` never fails on a well-formed map.** The C's `default: goto error` arm rejects an
//!   out-of-range `ErlNifMapIteratorEntry`; the frozen Rust type is a two-variant `#[repr(C)]` enum,
//!   so an out-of-range discriminant is already undefined behaviour at the FFI boundary and every
//!   value Rust can name is accepted.
//! * **The flat arm reads the map's own key/value arrays**, exactly as the C does
//!   (`flatmap_get_keys`/`values`), through `beam_term`'s `flatmap_parts`. No copy of a flat map is
//!   made, so a *copied* flat iterator keeps working — as it does in the C.
//! * **`destroy` is idempotent, and post-`destroy` use cannot read freed memory.** The C sets
//!   `iter->map = THE_NON_VALUE` only under `DEBUG` and leaves `u.hash.wstack` dangling, so a second
//!   `destroy` is a double free and a `get_pair` after it reads freed memory. Here `destroy` nulls
//!   the cache handle before releasing it (a second call is a no-op) and the hash arm's `get_pair`
//!   reports `0` for a null handle instead of dereferencing it. The flat arm behaves exactly as the
//!   C's does (nothing to release, and its pointers still address the map).
//! * **Refusals where the C would fault.** A null iterator, a null `key`/`value` out-parameter, or
//!   a non-map/non-term `iter->map` yields the C's failure code (`0`) instead of a `SIGSEGV`. The
//!   C's `ASSERT`s on the index invariants are not reproduced as panics.
//!
//! # `unsafe`
//!
//! The whole file is FFI: it dereferences a caller-owned iterator, interprets a term as a map, and
//! reads the `#[repr(C)]` union arm that `create` selected. Every `unsafe fn` therefore carries a
//! `# Safety` section (its caller contract) and every `unsafe` block a preceding `// SAFETY:`
//! rationale (workspace `CONTEXT.md` §3). No runtime data is `unwrap()`ed and no failure path panics.

use super::*;

use core::ptr::null_mut;
use libc::c_int;

use crate::nif::types::{
    ErlNifEnv, ErlNifMapIterator, ErlNifMapIteratorEntry, ErlNifMapIteratorFlat,
    ErlNifMapIteratorHash, ErtsDynamicWStack,
};

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_map_iterator_create = Some(enif_map_iterator_create);
    api.enif_map_iterator_destroy = Some(enif_map_iterator_destroy);
    api.enif_map_iterator_is_head = Some(enif_map_iterator_is_head);
    api.enif_map_iterator_is_tail = Some(enif_map_iterator_is_tail);
    api.enif_map_iterator_next = Some(enif_map_iterator_next);
    api.enif_map_iterator_prev = Some(enif_map_iterator_prev);
    api.enif_map_iterator_get_pair = Some(enif_map_iterator_get_pair);
    api
}

// ---------------------------------------------------------------------------------------------
// The hash arm's pair cache behind `u.hash.wstack`
// ---------------------------------------------------------------------------------------------

/// The hash arm's cached pairs, in the map's **storage order**
/// (`beam_term::map::map_pairs_storage_order`) — the same order the C's `hashmap_iterator_next`
/// walk yields (`erl_map.c:2346`), and therefore the order `maps:to_list/1` reports.
///
/// Each entry is a two-word array rather than a tuple so that `u.hash.kv` can point at it and the
/// two words are the C's `CAR(kv)`/`CDR(kv)` (`erl_nif.c:3946-3947`) — `repr(Rust)` does not
/// guarantee that a `(Eterm, Eterm)` lays its fields out in order, an array does.
type HashPairs = Vec<[Eterm; 2]>;

/// Recover the live hash-arm pair cache (`None` when the handle is null — see the module docs).
///
/// # Safety
///
/// `it` must be a live iterator whose map is a hash map, and whose `u.hash.wstack` is either null or
/// a `Box<HashPairs>` still owned by this iterator (i.e. produced by [`enif_map_iterator_create`]
/// and not yet released by [`enif_map_iterator_destroy`]).
unsafe fn hash_pairs<'a>(it: &ErlNifMapIterator) -> Option<&'a HashPairs> {
    // SAFETY: the caller guarantees the hash arm is the active one.
    let handle = unsafe { it.u.hash.wstack };
    if handle.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `handle` is a live `Box<HashPairs>` (the layout of a `Vec` is
    // irrelevant here — the pointer addresses the `Vec` *value* the `Box` owns).
    Some(unsafe { &*handle.cast::<HashPairs>() })
}

/// The `u.hash.kv` value for the pair at `idx` (1-based), or null outside `1..=size`.
fn kv_for(it: &ErlNifMapIterator, idx: usize) -> *mut ERL_NIF_TERM {
    // SAFETY: `kv_for` is only called for the hash arm (see the call sites).
    let Some(pairs) = (unsafe { hash_pairs(it) }) else {
        return null_mut();
    };
    if idx == 0 || idx > pairs.len() {
        return null_mut();
    }
    // SAFETY: `idx - 1` is a valid pair index, so the two `ERL_NIF_TERM` words starting at
    // `(idx - 1) * 2` are inside the vector's allocation.
    unsafe { pairs.as_ptr().cast::<ERL_NIF_TERM>().cast_mut().add(2 * (idx - 1)) }
}

// ---------------------------------------------------------------------------------------------
// Small accessors over the frozen struct
// ---------------------------------------------------------------------------------------------

/// The iterator's `idx` as a plain `usize` (`idx`/`size` are `ERL_NIF_UINT` = `ERL_NIF_TERM`).
#[inline]
fn idx_of(it: &ErlNifMapIterator) -> usize {
    it.idx.raw()
}

/// The iterator's `size` as a plain `usize`.
#[inline]
fn size_of(it: &ErlNifMapIterator) -> usize {
    it.size.raw()
}

/// Set the iterator's `idx`.
#[inline]
fn set_idx(it: &mut ErlNifMapIterator, idx: usize) {
    it.idx = ERL_NIF_TERM::from_raw(idx);
}

/// Recover the live iterator behind an `ErlNifMapIterator *` (`None` when null).
///
/// # Safety
///
/// `iter` must be null, or point at a live, exclusively-borrowed `ErlNifMapIterator` owned by the
/// caller for the duration of the call.
unsafe fn iter_mut<'a>(iter: *mut ErlNifMapIterator) -> Option<&'a mut ErlNifMapIterator> {
    if iter.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a live, unaliased iterator; a null pointer was handled above.
    Some(unsafe { &mut *iter })
}

/// Whether `iter`'s map is a **flat** map (the C's `is_flatmap(iter->map)`).
///
/// # Safety
///
/// `iter.map` must be a term word: the map of a created iterator, or the caller's own
/// (possibly uninitialised) struct — a non-term word is the caller's contract violation, exactly as
/// it is in the C.
unsafe fn iter_is_flat(it: &ErlNifMapIterator) -> bool {
    let m = t(it.map);
    // SAFETY: the caller guarantees `it.map` is a term word; `is_flatmap` only dereferences the
    // header of a boxed term.
    is_term_word(m) && unsafe { prelude::is_flatmap(m) }
}

// ---------------------------------------------------------------------------------------------
// enif_map_iterator_*
// ---------------------------------------------------------------------------------------------

/// `int enif_map_iterator_create(ErlNifEnv *env, ERL_NIF_TERM map, ErlNifMapIterator *iter,
/// ErlNifMapIteratorEntry entry);`
///
/// # Safety
///
/// `iter` must be null or writable for one `ErlNifMapIterator`; `map` must be a term word (anything
/// that is not a map is reported as `0`). A returned `1` makes `iter` the owner of the hash arm's
/// cache, which must be released with [`enif_map_iterator_destroy`].
#[no_mangle]
pub unsafe extern "C" fn enif_map_iterator_create(
    env: *mut ErlNifEnv,
    map: ERL_NIF_TERM,
    iter: *mut ErlNifMapIterator,
    entry: ErlNifMapIteratorEntry,
) -> c_int {
    // The env is not used: `create` allocates from the NIF allocator in the C
    // (`ERTS_ALC_T_NIF`, `erl_nif.c:3803-3804`), never from the environment's term heap.
    let _ = env;
    let Some(it) = (unsafe { iter_mut(iter) }) else {
        return 0;
    };
    let m = t(map);
    if !is_term_word(m) {
        return 0;
    }
    // SAFETY: `m` is a term word (checked); the predicates dereference a boxed term's header only.
    if unsafe { prelude::is_flatmap(m) } {
        // SAFETY: `m` is a live flat map (just tested), so its `[head, size, keys, values...]`
        // interior is live; both slices have the same length, in storage order.
        let (keys, values) = unsafe { prelude::flatmap_parts(m) };
        let size = keys.len();
        let offset = match entry {
            ErlNifMapIteratorEntry::First => 0,
            // An empty map wraps around, exactly as `size_t offset = flatmap_get_size(mp) - 1` does
            // in the C (`erl_nif.c:3784`), leaving `idx = 0` — the C's own "empty maps are ok but
            // will leave the iterator in bad shape" (`erl_nif.c:3788-3790`).
            ErlNifMapIteratorEntry::Last => size.wrapping_sub(1),
        };
        it.map = map;
        it.size = ERL_NIF_TERM::from_raw(size);
        it.idx = ERL_NIF_TERM::from_raw(offset.wrapping_add(1));
        // The flat arm holds the map's own key/value arrays, as the C does (`erl_nif.c:3793-3794`).
        // `wrapping_add`: for the empty-map wrap `offset` is `usize::MAX` on purpose; the resulting
        // pointer is never dereferenced (`get_pair` requires `idx > 0`).
        it.u = crate::nif::types::ErlNifMapIteratorUnion {
            flat: ErlNifMapIteratorFlat {
                ks: keys
                    .as_ptr()
                    .cast::<ERL_NIF_TERM>()
                    .cast_mut()
                    .wrapping_add(offset),
                vs: values
                    .as_ptr()
                    .cast::<ERL_NIF_TERM>()
                    .cast_mut()
                    .wrapping_add(offset),
            },
        };
        return 1;
    }
    // SAFETY: as above — `m` is a term word.
    if !(unsafe { prelude::is_hashmap(m) }) {
        // Not a map at all (the C's `goto error`, `erl_nif.c:3824`).
        return 0;
    }
    // A hash map (> MAP_SMALL_MAP_LIMIT keys) caches its pairs in storage order — the same order the
    // C's `hashmap_iterator_next` walk produces (`erl_map.c:2346`).
    let Ok(pairs) = prelude::map_pairs_storage_order(m) else {
        // Unreachable for a term `is_hashmap` accepted (a corrupt interior); refuse rather than
        // build an iterator whose walk would be wrong.
        return 0;
    };
    let pairs: HashPairs = pairs.into_iter().map(|(k, v)| [k, v]).collect();
    let size = pairs.len();
    let boxed = Box::into_raw(Box::new(pairs));
    it.map = map;
    it.size = ERL_NIF_TERM::from_raw(size);
    it.idx = ERL_NIF_TERM::from_raw(match entry {
        ErlNifMapIteratorEntry::First => 1,
        ErlNifMapIteratorEntry::Last => size,
    });
    it.u = crate::nif::types::ErlNifMapIteratorUnion {
        hash: ErlNifMapIteratorHash {
            wstack: boxed.cast::<ErtsDynamicWStack>(),
            // `kv` mirrors the C's cursor (`erl_nif.c:3810`/`3815`): the pair at `idx`, i.e. the
            // first for `FIRST` and the last for `LAST`.
            kv: kv_for_raw(boxed, idx_of(it)),
        },
    };
    1
}

/// The `kv` for pair `idx` (1-based) of a freshly boxed cache — the `create`-time counterpart of
/// [`kv_for`], which cannot take a `&ErlNifMapIterator` before the cache handle is stored.
fn kv_for_raw(boxed: *mut HashPairs, idx: usize) -> *mut ERL_NIF_TERM {
    // SAFETY: `boxed` is the live `Box::into_raw` handle of `create` and `idx` is 1 or `size`.
    unsafe {
        let pairs = &*boxed;
        if idx == 0 || idx > pairs.len() {
            return null_mut();
        }
        pairs.as_ptr().cast::<ERL_NIF_TERM>().cast_mut().add(2 * (idx - 1))
    }
}

/// `void enif_map_iterator_destroy(ErlNifEnv *env, ErlNifMapIterator *iter);`
///
/// # Safety
///
/// `iter` must be null, or point at a live, exclusively-borrowed iterator (a zeroed *never created*
/// one is fine, as is a second call on an already destroyed one — both are a no-op). After the call
/// its hash-arm cache (if any) is released and must not be used.
#[no_mangle]
pub unsafe extern "C" fn enif_map_iterator_destroy(env: *mut ErlNifEnv, iter: *mut ErlNifMapIterator) {
    let _ = env;
    let Some(it) = (unsafe { iter_mut(iter) }) else {
        return;
    };
    // SAFETY: `it.map` is the caller's iterator field.
    let m = t(it.map);
    // The same live-term guard the sibling paths apply (`iter_is_flat`, `create`): an all-zero
    // (never created / already released) or hand-built iterator holds a non-term `map` word, and the
    // C's `is_boxed` bit trick classifies the raw word `0` as *boxed*, so an unguarded `is_hashmap`
    // would dereference a null pointer (the `ASSERT(is_flatmap(iter->map))` at `erl_nif.c:3838` in
    // the C). A non-term map owns no cache, so there is nothing to release — this keeps `destroy`
    // idempotent and safe on a never-created iterator, as the module docs promise.
    if !is_term_word(m) {
        return;
    }
    // SAFETY: `m` is a term word (checked above) belonging to the caller's iterator.
    if !(unsafe { prelude::is_hashmap(m) }) {
        // The flat arm owns nothing — the C's `ASSERT(is_flatmap(iter->map))` (`erl_nif.c:3838`).
        return;
    }
    // SAFETY: reading the active (hash) union arm.
    let handle = unsafe { it.u.hash.wstack };
    if handle.is_null() {
        // Already released (or never created), so there is nothing to do — a documented
        // improvement over the C, where a second `destroy` is a double free.
        return;
    }
    // Null the handle *before* releasing the cache, so the call is idempotent and a later `get_pair`
    // reports failure instead of reading freed memory.
    it.u = crate::nif::types::ErlNifMapIteratorUnion {
        hash: ErlNifMapIteratorHash::default(),
    };
    // SAFETY: `handle` came from `Box::into_raw` in `create` for this iterator and has not been
    // freed (it was non-null above), so reclaiming it is exactly the inverse of that call.
    drop(unsafe { Box::from_raw(handle.cast::<HashPairs>()) });
}

/// `int enif_map_iterator_is_head(ErlNifEnv *env, ErlNifMapIterator *iter);`
///
/// # Safety
///
/// `iter` must be a live iterator previously positioned by [`enif_map_iterator_create`].
#[no_mangle]
pub unsafe extern "C" fn enif_map_iterator_is_head(
    env: *mut ErlNifEnv,
    iter: *mut ErlNifMapIterator,
) -> c_int {
    let _ = env;
    let Some(it) = (unsafe { iter_mut(iter) }) else {
        return 0;
    };
    // SAFETY: `it.map` belongs to the caller's iterator (a term word once created).
    let head = if unsafe { iter_is_flat(it) } {
        // `erl_nif.c:3865`.
        size_of(it) == 0 || idx_of(it) == 0
    } else {
        // `erl_nif.c:3869`.
        idx_of(it) == 0
    };
    head as c_int
}

/// `int enif_map_iterator_is_tail(ErlNifEnv *env, ErlNifMapIterator *iter);`
///
/// # Safety
///
/// `iter` must be a live iterator previously positioned by [`enif_map_iterator_create`].
#[no_mangle]
pub unsafe extern "C" fn enif_map_iterator_is_tail(
    env: *mut ErlNifEnv,
    iter: *mut ErlNifMapIterator,
) -> c_int {
    let _ = env;
    let Some(it) = (unsafe { iter_mut(iter) }) else {
        return 0;
    };
    // SAFETY: as `is_head`.
    let tail = if unsafe { iter_is_flat(it) } {
        // `erl_nif.c:3851`.
        size_of(it) == 0 || idx_of(it) > size_of(it)
    } else {
        // `erl_nif.c:3855`.
        idx_of(it) > size_of(it)
    };
    tail as c_int
}

/// `int enif_map_iterator_next(ErlNifEnv *env, ErlNifMapIterator *iter);`
///
/// # Safety
///
/// `iter` must be a live iterator previously positioned by [`enif_map_iterator_create`].
#[no_mangle]
pub unsafe extern "C" fn enif_map_iterator_next(
    env: *mut ErlNifEnv,
    iter: *mut ErlNifMapIterator,
) -> c_int {
    let _ = env;
    let Some(it) = (unsafe { iter_mut(iter) }) else {
        return 0;
    };
    let size = size_of(it);
    let idx = idx_of(it);
    // SAFETY: as `is_head`.
    if unsafe { iter_is_flat(it) } {
        // `erl_nif.c:3877-3883`. `wrapping_add` keeps the empty-map wrap-around defined (the C's
        // `ks++` on the wrapped pointer is undefined arithmetic, though it is never dereferenced).
        if idx <= size {
            set_idx(it, idx + 1);
            // SAFETY: writing the active (flat) union arm.
            unsafe {
                it.u.flat.ks = it.u.flat.ks.wrapping_add(1);
                it.u.flat.vs = it.u.flat.vs.wrapping_add(1);
            }
        }
        return (idx_of(it) <= size) as c_int;
    }
    // `erl_nif.c:3886-3896`: the counter steps while `idx <= size`, and the cursor follows it.
    if idx <= size {
        let next = idx + 1;
        set_idx(it, next);
        let kv = kv_for(it, next);
        // SAFETY: writing the active (hash) union arm's `kv`.
        it.u.hash.kv = kv;
    }
    (idx_of(it) <= size) as c_int
}

/// `int enif_map_iterator_prev(ErlNifEnv *env, ErlNifMapIterator *iter);`
///
/// # Safety
///
/// `iter` must be a live iterator previously positioned by [`enif_map_iterator_create`].
#[no_mangle]
pub unsafe extern "C" fn enif_map_iterator_prev(
    env: *mut ErlNifEnv,
    iter: *mut ErlNifMapIterator,
) -> c_int {
    let _ = env;
    let Some(it) = (unsafe { iter_mut(iter) }) else {
        return 0;
    };
    let idx = idx_of(it);
    // SAFETY: as `is_head`.
    if unsafe { iter_is_flat(it) } {
        // `erl_nif.c:3903-3909`.
        if idx > 0 {
            set_idx(it, idx - 1);
            // SAFETY: writing the active (flat) union arm.
            unsafe {
                it.u.flat.ks = it.u.flat.ks.wrapping_sub(1);
                it.u.flat.vs = it.u.flat.vs.wrapping_sub(1);
            }
        }
        return (idx_of(it) > 0) as c_int;
    }
    // `erl_nif.c:3914-3922`: stepping back from the tail re-runs the walk in reverse; the counter
    // (`idx - 1`) identifies the same pair either way.
    if idx > 0 {
        let prev = idx - 1;
        set_idx(it, prev);
        let kv = kv_for(it, prev);
        // SAFETY: writing the active (hash) union arm's `kv`.
        it.u.hash.kv = kv;
    }
    (idx_of(it) > 0) as c_int
}

/// `int enif_map_iterator_get_pair(ErlNifEnv *env, ErlNifMapIterator *iter, ERL_NIF_TERM *key,
/// ERL_NIF_TERM *value);`
///
/// # Safety
///
/// `iter` must be a live iterator previously positioned by [`enif_map_iterator_create`]; `key` and
/// `value` must both be writable.
#[no_mangle]
pub unsafe extern "C" fn enif_map_iterator_get_pair(
    env: *mut ErlNifEnv,
    iter: *mut ErlNifMapIterator,
    key: *mut ERL_NIF_TERM,
    value: *mut ERL_NIF_TERM,
) -> c_int {
    let _ = env;
    let Some(it) = (unsafe { iter_mut(iter) }) else {
        return 0;
    };
    // SAFETY: as `is_head`.
    let flat = unsafe { iter_is_flat(it) };
    // The C's single binding rule (`erl_nif.c:3933`/`3945`): only `0 < idx <= size` is a pair, so
    // HEAD and TAIL both fail without touching the out-parameters.
    let idx = idx_of(it);
    if idx == 0 || idx > size_of(it) {
        return 0;
    }
    if key.is_null() || value.is_null() {
        // The C would dereference them; refuse instead of faulting.
        return 0;
    }
    if flat {
        // SAFETY: reading the active (flat) union arm; `create` (or the map's own storage) set both.
        let (ks, vs) = unsafe { (it.u.flat.ks, it.u.flat.vs) };
        if ks.is_null() || vs.is_null() {
            return 0;
        }
        // SAFETY: for `0 < idx <= size` the flat arm's pointers address element `idx - 1` of the
        // map's live key/value arrays, and `key`/`value` are writable (both checked).
        unsafe {
            key.write(*ks);
            value.write(*vs);
        }
        return 1;
    }
    // SAFETY: reading the active (hash) union arm.
    let kv = unsafe { it.u.hash.kv };
    if kv.is_null() {
        // A released (or hand-built) iterator: refuse rather than read freed memory.
        return 0;
    }
    // SAFETY: `kv` addresses a live pair of contiguous words (see `kv_for`), and `key`/`value` are
    // writable (both checked) — the C's `CAR(kv)`/`CDR(kv)` (`erl_nif.c:3946-3947`).
    unsafe {
        key.write(*kv);
        value.write(*kv.add(1));
    }
    1
}

// ---------------------------------------------------------------------------------------------
// Unit tests — the index arithmetic and the arm-specific binding rule, over hand-built terms
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
#[path = "mapiter_tests.rs"]
mod tests;
