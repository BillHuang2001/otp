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

//! The `atomics` and `counters` resource objects (contract A.7).
//!
//! `erts_internal:atomics_new/2` and `erts_internal:counters_new/1` create a
//! **resource object**: a fixed array of 64-bit cells that the owner mutates
//! through an opaque handle; the `atomics:*` / `erts_internal:counters_*` BIFs are
//! the only way to touch the cells. This module implements the whole family for
//! real, registered as plain [`BifFn`](crate::registry::BifFn)s so they **override**
//! the [`RuntimeHooks`](crate::context::RuntimeHooks)-backed seam registrations of
//! `src/runtime/{erts_internal,other}.rs` (the registry keeps the last registration
//! for a key — see [`crate::registry::BifRegistry::register`]).
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_atomics.c` and
//! `../../erts/emulator/beam/erl_bif_counters.c`; the value conversions are C's
//! `term_to_UWord`/`term_to_Sint64`/`term_to_Uint64` (`big.c:3567-3720`) and the
//! rendering is C's `bld_atomic` (`erl_bif_atomics.c:134-155`).
//!
//! # Representation
//!
//! The C keeps the cells inside an off-heap "magic binary"; this runtime keeps them
//! in a module-local process-global registry — `HashMap<id, Resource>` behind a
//! `Mutex` — exactly like the compiled-pattern (`src/binary.rs`) and ETS
//! continuation/spec (`src/ets.rs`, `src/ets/reverse.rs`) registries. The handle is
//! an on-heap internal reference thing whose id is a small integer above
//! [`RESOURCE_ID_BASE`] (`1 << 52`), disjoint from every other reference id space in
//! the crate; it is built and read with the shared [`crate::access`] helpers so the
//! term layout stays owned by `beam_term`.
//!
//! An `atomics` cell is stored raw (`u64`) and its signedness is applied only on
//! read/write conversion, so one `Resource` variant serves both the signed
//! (`Opts` bit 0 set) and unsigned forms. The 64-bit arithmetic wraps
//! (`wrapping_add`) exactly like the C's `erts_atomic64_add_mb`.
//!
//! # Known Milestone-1 caveat (leak)
//!
//! Registry entries are **never reclaimed**: the C frees the magic binary when its
//! reference dies (a GC destructor), but this runtime has no off-heap finalizer for
//! a resource object yet, so a handle that goes out of scope leaks its cells. This
//! mirrors the existing compiled-pattern / ETS continuation registries (a bounded
//! M1 leak documented in the crate `CONTEXT.md`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use beam_common::Result;
use beam_term::prelude::signed_value;
use beam_term::Eterm;

use crate::access;
use crate::arith;
use crate::context::BifContext;
use crate::error::{badarg, system_limit};
use crate::registry::BifRegistry;

/// The `(module, function, arity)` keys this module registers **for real**.
///
/// Sorted by `(module, function, arity)`. A follow-up task consumes this list to
/// remove the matching `runtime_bif!` seam registrations in
/// `src/runtime/{erts_internal,other}.rs`; until then it is the single source of
/// truth for which keys the real implementations own, and [`register`] asserts every
/// one of them is actually present after registration.
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[
    ("atomics", "add", 3),
    ("atomics", "add_get", 3),
    ("atomics", "compare_exchange", 4),
    ("atomics", "exchange", 3),
    ("atomics", "get", 2),
    ("atomics", "info", 1),
    ("atomics", "put", 3),
    ("erts_internal", "atomics_new", 2),
    ("erts_internal", "counters_add", 3),
    ("erts_internal", "counters_get", 2),
    ("erts_internal", "counters_info", 1),
    ("erts_internal", "counters_new", 1),
    ("erts_internal", "counters_put", 3),
];

/// The first id handed out for a resource handle.
///
/// `1 << 52`: far above every other on-heap reference id space in this crate
/// (`src/binary.rs` compiled patterns `1 << 32`, `src/ets.rs` continuations
/// `1 << 40` and compiled specs `1 << 48`, `src/ets/reverse.rs` reverse
/// continuations `1 << 41`) so a handle can never be mistaken for one of those, and
/// well below `2^59` so it still fits the 60-bit small integer an internal
/// reference's id word carries ([`access::make_ref_thing`]).
const RESOURCE_ID_BASE: u64 = 1 << 52;

/// The next resource id (a monotonically increasing counter from
/// [`RESOURCE_ID_BASE`]).
static NEXT_RESOURCE_ID: AtomicU64 = AtomicU64::new(RESOURCE_ID_BASE);

/// A hard upper bound on a `counters` array's element count.
///
/// The C guards only arithmetic overflow (`erl_bif_counters.c:92`), so
/// `counters_new(1 bsl 33)` reaches the allocator and OOM-crashes the oracle. This
/// runtime instead refuses an element count above this bound with `system_limit`
/// (and additionally reserves fallibly, so a large-but-below-bound request the
/// allocator cannot serve is also `system_limit` rather than an abort).
const COUNTERS_MAX_SIZE: u64 = (1 << 32) - 1;

/// `ERTS_CACHE_LINE_SIZE` (`erl_bif_counters.c`).
const ERTS_CACHE_LINE_SIZE: u64 = 64;

/// `ATOMICS_PER_CACHE_LINE` — the number of 64-bit counters that fit one cache line.
const ATOMICS_PER_CACHE_LINE: u64 = ERTS_CACHE_LINE_SIZE / 8;

/// `ATOMICS_PER_COUNTER` — the C replicates each counter once per scheduler (plus a
/// slot 0) so the per-scheduler updates do not share a cache line. This runtime has
/// no per-scheduler replication; it reports the *modelled* size of the C's layout
/// (see [`counters_info_1`]).
fn atomics_per_counter() -> u64 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(1)
        + 1
}

/// A live resource object behind one handle.
enum Resource {
    /// An `atomics` array: `values.len()` raw 64-bit cells. `signed` is bit 0 of the
    /// `Opts` argument (`OPT_SIGNED`, `erl_bif_atomics.c:54`); a signed resource
    /// reads/writes each cell as `i64`, an unsigned one as `u64`.
    Atomics { signed: bool, values: Vec<u64> },
    /// A `counters` array: `values.len()` signed 64-bit cells.
    Counters { values: Vec<i64> },
}

/// The process-global resource registry (`id -> Resource`).
///
/// Entries are never reclaimed (see the module doc's M1 leak caveat).
fn registry() -> &'static Mutex<HashMap<u64, Resource>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, Resource>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Locks the registry, recovering the guard if another thread panicked while holding
/// it (a poison must not turn a resource operation into a crash).
fn lock_registry() -> std::sync::MutexGuard<'static, HashMap<u64, Resource>> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// Stores `resource` under a fresh id and returns that id.
fn new_resource(resource: Resource) -> u64 {
    let id = NEXT_RESOURCE_ID.fetch_add(1, Ordering::Relaxed);
    lock_registry().insert(id, resource);
    id
}

// ---------------------------------------------------------------------------
// Integer term conversions (C `term_to_UWord` / `term_to_Sint64` / `term_to_Uint64`)
// ---------------------------------------------------------------------------

/// The magnitude limbs of a bignum term, little-endian, with leading zero limbs
/// dropped — the value, not the storage.
///
/// A literal bignum has no leading zero limb, but a value that flows in from another
/// representation may, so they are removed here to make the `fits N bytes` checks
/// below exact.
fn bignum_magnitude(term: Eterm) -> Result<(bool, Vec<u64>)> {
    let (negative, digits) = beam_codec::term::big_parts(term)?;
    let mut magnitude = digits.to_vec();
    while magnitude.last() == Some(&0) {
        magnitude.pop();
    }
    Ok((negative, magnitude))
}

/// `term_to_UWord` for a *non-negative* integer (`erl_bif_atomics.c:64`).
///
/// A small integer is its value; a bignum is `badarg` (every BIF that uses this —
/// an arity, an index, `Opts` — bounds the value to the small range anyway, so the
/// C's `SYSTEM_LIMIT` for an out-of-`UWord` bignum is reported here as its documented
/// `badarg`), and any other term (negative, float, atom, list, ...) is `badarg`.
fn term_to_uword(term: Eterm) -> Result<u64> {
    if access::is_small(term) {
        let value = signed_value(term);
        if value >= 0 {
            return Ok(value as u64);
        }
    }
    Err(badarg())
}

/// `term_to_Sint64` (`big.c:3702`): an integer that fits `i64`.
///
/// A small integer is its value; a bignum is accepted iff its exact value fits
/// `i64` (a positive magnitude `<= i64::MAX`, a negative magnitude `<= 2^63`, so
/// `-2^63` = `i64::MIN` is accepted); anything else is `badarg`.
fn term_to_i64(term: Eterm) -> Result<i64> {
    if access::is_small(term) {
        return Ok(signed_value(term));
    }
    if access::is_big(term) {
        let (negative, magnitude) = bignum_magnitude(term)?;
        match magnitude.as_slice() {
            [] => return Ok(0),
            [limb] => {
                if negative {
                    if *limb <= 1u64 << 63 {
                        return Ok((*limb as i64).wrapping_neg());
                    }
                } else if *limb <= i64::MAX as u64 {
                    return Ok(*limb as i64);
                }
            }
            _ => {}
        }
    }
    Err(badarg())
}

/// `term_to_Uint64` (`big.c:3608` on a 64-bit word): a non-negative integer that
/// fits `u64`.
///
/// A small integer is its value when non-negative; a bignum is accepted iff it is
/// positive and its magnitude fits `u64` (so `2^63` and `u64::MAX` are accepted while
/// `2^64` and `u64::MAX + 1` are not); anything else is `badarg`.
fn term_to_u64(term: Eterm) -> Result<u64> {
    if access::is_small(term) {
        let value = signed_value(term);
        return u64::try_from(value).map_err(|_| badarg());
    }
    if access::is_big(term) {
        let (negative, magnitude) = bignum_magnitude(term)?;
        if negative {
            return Err(badarg());
        }
        return match magnitude.as_slice() {
            [] => Ok(0),
            [limb] => Ok(*limb),
            _ => Err(badarg()),
        };
    }
    Err(badarg())
}

/// `get_value` (`erl_bif_atomics.c:121`): the cell representation of a stored value
/// term — a signed resource reads it with `term_to_i64`, an unsigned one with
/// `term_to_u64`. The result is the raw two's-complement `u64` the cell holds.
fn get_value(signed: bool, term: Eterm) -> Result<u64> {
    if signed {
        Ok(term_to_i64(term)? as u64)
    } else {
        term_to_u64(term)
    }
}

/// `get_incr` (`erl_bif_atomics.c:128`, `erl_bif_counters.c:162`): an increment may
/// be any integer that fits `i64` *or* any non-negative integer that fits `u64`; the
/// value is the raw two's-complement `i64` (so a `u64` above `i64::MAX` becomes a
/// negative `i64` and the add wraps).
fn get_incr(term: Eterm) -> Result<i64> {
    term_to_i64(term).or_else(|_| term_to_u64(term).map(|u| u as i64))
}

/// The 1-based `Index` argument as a 0-based slot index, or `badarg`.
///
/// `1 <= ix <= len`; a non-integer, `0`, a negative value, a float or a value past
/// the end is `badarg` (C's `--ix < vlen`, `erl_bif_atomics.c:113-119`).
fn index_of(len: usize, term: Eterm) -> Result<usize> {
    let ix = term_to_uword(term)?;
    if ix == 0 || ix > len as u64 {
        return Err(badarg());
    }
    Ok((ix - 1) as usize)
}

/// Renders a raw cell as the oracle's `bld_atomic` (`erl_bif_atomics.c:134`): a
/// signed cell with [`crate::arith::int_from_i64`], an unsigned one as a small
/// integer when it fits the 60-bit range and an unsigned bignum otherwise.
fn render_value(ctx: &mut BifContext<'_>, signed: bool, raw: u64) -> Result<Eterm> {
    if signed {
        arith::int_from_i64(ctx, raw as i64)
    } else {
        arith::int_from_mag(ctx, false, &[raw])
    }
}

// ---------------------------------------------------------------------------
// Handle / slot access helpers
// ---------------------------------------------------------------------------

/// Runs `f` on the `atomics` resource behind `handle`, holding the registry lock.
///
/// The typed `badarg` is returned when `handle` is not a live `atomics` resource: a
/// term that is not an internal reference, a foreign/forged reference id (absent
/// from the registry — the ETS handle namespace shares this ref representation but
/// not this id space), or a `counters` handle.
fn with_atomics<R>(
    handle: Eterm,
    f: impl FnOnce(&mut Vec<u64>, bool) -> Result<R>,
) -> Result<R> {
    let id = access::ref_thing_id(handle)?;
    let mut reg = lock_registry();
    match reg.get_mut(&id) {
        Some(Resource::Atomics { signed, values }) => f(values, *signed),
        _ => Err(badarg()),
    }
}

/// [`with_atomics`] for a `counters` resource.
fn with_counters<R>(handle: Eterm, f: impl FnOnce(&mut Vec<i64>) -> Result<R>) -> Result<R> {
    let id = access::ref_thing_id(handle)?;
    let mut reg = lock_registry();
    match reg.get_mut(&id) {
        Some(Resource::Counters { values }) => f(values),
        _ => Err(badarg()),
    }
}

// ---------------------------------------------------------------------------
// `erts_internal:atomics_new/2`
// ---------------------------------------------------------------------------

/// `erts_internal:atomics_new(Arity, Opts)` (`erl_bif_atomics.c:56`).
///
/// `Arity`/`Opts` must be non-negative integers; `Arity` is at least 1. `Opts` bit 0
/// (`OPT_SIGNED`) selects signedness; every other bit is ignored (`(3,0)` and
/// `(3,2)` are unsigned, `(3,1)` and `(3,3)` signed). All cells start at `0`; the
/// handle is returned.
fn atomics_new(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let cnt = term_to_uword(args[0])?;
    if cnt == 0 {
        return Err(badarg());
    }
    let opts = term_to_uword(args[1])?;
    // The C overflow guard (`erl_bif_atomics.c:75`): `cnt * sizeof(atomic64)`.
    if cnt > u64::MAX / 8 {
        return Err(system_limit());
    }
    // A fallible reservation turns a size the allocator cannot serve into the typed
    // `system_limit` instead of an abort.
    let mut values: Vec<u64> = Vec::new();
    values
        .try_reserve_exact(cnt as usize)
        .map_err(|_| system_limit())?;
    values.resize(cnt as usize, 0);
    let id = new_resource(Resource::Atomics {
        signed: opts & 1 != 0,
        values,
    });
    access::make_ref_thing(ctx.heap(), id)
}

// ---------------------------------------------------------------------------
// `atomics:get/2`, `put/3`, `add/3`, `add_get/3`, `exchange/3`, `compare_exchange/4`
// ---------------------------------------------------------------------------

/// `atomics:get(Ref, Ix)` (`erl_bif_atomics.c:171`) — the current cell value,
/// rendered for the resource's signedness.
fn atomics_get(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (signed, raw) = with_atomics(args[0], |values, signed| {
        let ix = index_of(values.len(), args[1])?;
        Ok((signed, values[ix]))
    })?;
    render_value(ctx, signed, raw)
}

/// `atomics:put(Ref, Ix, Value)` (`erl_bif_atomics.c:157`) — stores `Value`, `ok`.
fn atomics_put(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_atomics(args[0], |values, signed| {
        let ix = index_of(values.len(), args[1])?;
        let value = get_value(signed, args[2])?;
        values[ix] = value;
        Ok(())
    })?;
    ctx.ok()
}

/// `atomics:add(Ref, Ix, Incr)` (`erl_bif_atomics.c:182`) — adds `Incr` (wrapping at
/// 64 bits), `ok`.
fn atomics_add(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_atomics(args[0], |values, _signed| {
        let ix = index_of(values.len(), args[1])?;
        let incr = get_incr(args[2])? as u64;
        values[ix] = values[ix].wrapping_add(incr);
        Ok(())
    })?;
    ctx.ok()
}

/// `atomics:add_get(Ref, Ix, Incr)` (`erl_bif_atomics.c:196`) — adds `Incr` and
/// returns the **new** value.
fn atomics_add_get(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (signed, new) = with_atomics(args[0], |values, signed| {
        let ix = index_of(values.len(), args[1])?;
        let incr = get_incr(args[2])? as u64;
        values[ix] = values[ix].wrapping_add(incr);
        Ok((signed, values[ix]))
    })?;
    render_value(ctx, signed, new)
}

/// `atomics:exchange(Ref, Ix, Desired)` (`erl_bif_atomics.c:209`) — stores `Desired`
/// and returns the **previous** value.
fn atomics_exchange(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (signed, old) = with_atomics(args[0], |values, signed| {
        let ix = index_of(values.len(), args[1])?;
        let desired = get_value(signed, args[2])?;
        let old = values[ix];
        values[ix] = desired;
        Ok((signed, old))
    })?;
    render_value(ctx, signed, old)
}

/// `atomics:compare_exchange(Ref, Ix, Expected, Desired)` (`erl_bif_atomics.c:223`) —
/// when the current cell equals `Expected` (as raw bits) it stores `Desired` and
/// returns `ok`, otherwise it returns the **actual** current value and writes
/// nothing.
fn atomics_compare_exchange(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mismatch = with_atomics(args[0], |values, signed| {
        let ix = index_of(values.len(), args[1])?;
        let expected = get_value(signed, args[2])?;
        let desired = get_value(signed, args[3])?;
        if values[ix] == expected {
            values[ix] = desired;
            Ok(None)
        } else {
            Ok(Some((signed, values[ix])))
        }
    })?;
    match mismatch {
        None => ctx.ok(),
        Some((signed, actual)) => render_value(ctx, signed, actual),
    }
}

/// `atomics:info(Ref)` (`erl_bif_atomics.c:238`) — the flat map
/// `#{max => Max, min => Min, size => N, memory => M}` in the oracle's canonical key
/// order `max, memory, min, size`.
///
/// `Max`/`Min` are the signedness bounds (`i64::MAX`/`i64::MIN`, or `u64::MAX`/`0`),
/// `size` is the cell count and `memory` is the C's `40 + 8 * size` magic-binary
/// size (`ertas_magic_ref2bin(...)->orig_size`, verified: 1→48, 3→64, 8→104,
/// 1000→8040).
fn atomics_info(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let id = access::ref_thing_id(args[0])?;
    let (signed, n) = {
        let reg = lock_registry();
        match reg.get(&id) {
            Some(Resource::Atomics { signed, values }) => (*signed, values.len() as u64),
            _ => return Err(badarg()),
        }
    };
    let memory = 40 + 8 * n;
    let (max, min) = if signed {
        (
            arith::int_from_i64(ctx, i64::MAX)?,
            arith::int_from_i64(ctx, i64::MIN)?,
        )
    } else {
        (
            arith::int_from_mag(ctx, false, &[u64::MAX])?,
            arith::int_from_mag(ctx, false, &[0])?,
        )
    };
    let size = arith::int_from_mag(ctx, false, &[n])?;
    let memory = arith::int_from_mag(ctx, false, &[memory])?;
    let key_max = ctx.intern_atom("max")?;
    let key_memory = ctx.intern_atom("memory")?;
    let key_min = ctx.intern_atom("min")?;
    let key_size = ctx.intern_atom("size")?;
    beam_codec::term::make_flatmap(
        ctx.heap(),
        &[
            (key_max, max),
            (key_memory, memory),
            (key_min, min),
            (key_size, size),
        ],
    )
}

// ---------------------------------------------------------------------------
// `erts_internal:counters_new/1` and the `counters` operations
// ---------------------------------------------------------------------------

/// `erts_internal:counters_new(N)` (`erl_bif_counters.c:77`).
///
/// `N` must be a non-negative integer and at least 1; every counter starts at `0`;
/// the handle is returned.
fn counters_new(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let n = term_to_uword(args[0])?;
    if n == 0 {
        return Err(badarg());
    }
    // See [`COUNTERS_MAX_SIZE`]: a bogus size is a typed error, never an OOM abort.
    if n > COUNTERS_MAX_SIZE {
        return Err(system_limit());
    }
    let mut values: Vec<i64> = Vec::new();
    values
        .try_reserve_exact(n as usize)
        .map_err(|_| system_limit())?;
    values.resize(n as usize, 0);
    let id = new_resource(Resource::Counters { values });
    access::make_ref_thing(ctx.heap(), id)
}

/// `erts_internal:counters_get(Ref, Ix)` (`erl_bif_counters.c:180`) — the current
/// (signed) counter value.
fn counters_get(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let value = with_counters(args[0], |values| {
        let ix = index_of(values.len(), args[1])?;
        Ok(values[ix])
    })?;
    arith::int_from_i64(ctx, value)
}

/// `erts_internal:counters_add(Ref, Ix, Incr)` (`erl_bif_counters.c:197`) — adds
/// `Incr` (wrapping at 64 bits), `ok`.
fn counters_add(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_counters(args[0], |values| {
        let ix = index_of(values.len(), args[1])?;
        let incr = get_incr(args[2])?;
        values[ix] = values[ix].wrapping_add(incr);
        Ok(())
    })?;
    ctx.ok()
}

/// `erts_internal:counters_put(Ref, Ix, Value)` (`erl_bif_counters.c:212`) — stores
/// `Value` (which must fit `i64`), `ok`.
fn counters_put(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_counters(args[0], |values| {
        let ix = index_of(values.len(), args[1])?;
        let value = term_to_i64(args[2])?;
        values[ix] = value;
        Ok(())
    })?;
    ctx.ok()
}

/// `erts_internal:counters_info(Ref)` (`erl_bif_counters.c:238`) — the flat map
/// `#{memory => M, size => N}` in the oracle's key order `memory, size`.
///
/// `size` is the counter count and `memory` is the C layout's modelled size:
/// `32 + ATOMICS_PER_COUNTER * ceil(N / ATOMICS_PER_CACHE_LINE) * 64`, where
/// `ATOMICS_PER_COUNTER` is `available_parallelism() + 1` (the C replicating one
/// counter slot per scheduler plus slot 0). This is an **ERTS-model approximation**:
/// this runtime does not reproduce the per-scheduler cache-line replication, so on a
/// host whose parallelism differs from the oracle's scheduler count the reported
/// `memory` differs (verified on a 16-scheduler oracle: 1→1120, 2→1120, 16→2208,
/// 64→8736).
fn counters_info(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let id = access::ref_thing_id(args[0])?;
    let n = {
        let reg = lock_registry();
        match reg.get(&id) {
            Some(Resource::Counters { values }) => values.len() as u64,
            _ => return Err(badarg()),
        }
    };
    let lines = (n + ATOMICS_PER_CACHE_LINE - 1) / ATOMICS_PER_CACHE_LINE;
    let memory = 32 + atomics_per_counter() * lines * ERTS_CACHE_LINE_SIZE;
    let memory = arith::int_from_mag(ctx, false, &[memory])?;
    let size = arith::int_from_mag(ctx, false, &[n])?;
    let key_memory = ctx.intern_atom("memory")?;
    let key_size = ctx.intern_atom("size")?;
    beam_codec::term::make_flatmap(ctx.heap(), &[(key_memory, memory), (key_size, size)])
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Registers the real `atomics`/`counters` implementations.
///
/// Called **last** from [`crate::runtime::register`], so each key overrides the
/// `runtime_bif!` seam registration of `src/runtime/{erts_internal,other}.rs` (the
/// registry keeps the last registration for a key).
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erts_internal", "atomics_new", 2, atomics_new);
    reg.register("atomics", "get", 2, atomics_get);
    reg.register("atomics", "put", 3, atomics_put);
    reg.register("atomics", "add", 3, atomics_add);
    reg.register("atomics", "add_get", 3, atomics_add_get);
    reg.register("atomics", "exchange", 3, atomics_exchange);
    reg.register("atomics", "compare_exchange", 4, atomics_compare_exchange);
    reg.register("atomics", "info", 1, atomics_info);
    reg.register("erts_internal", "counters_new", 1, counters_new);
    reg.register("erts_internal", "counters_get", 2, counters_get);
    reg.register("erts_internal", "counters_add", 3, counters_add);
    reg.register("erts_internal", "counters_put", 3, counters_put);
    reg.register("erts_internal", "counters_info", 1, counters_info);
    // `REAL_BIFS` is the declared roster a follow-up consumes to delete the seam
    // registrations; keep it honest by checking it against what was just registered.
    debug_assert!(
        REAL_BIFS
            .iter()
            .all(|(module, function, arity)| reg.contains(module, function, *arity)),
        "REAL_BIFS lists a key that this module does not register"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true", "false", "ok", "badarg", "max", "memory", "min", "size", "a",
        ]);
        atoms
    }

    /// Builds a context and a registry that went through the **whole** runtime
    /// registration (`crate::runtime::register`), so every test also exercises the
    /// override of the `runtime_bif!` seam keys.
    fn with_runtime<R>(f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> R {
        let mut reg = BifRegistry::new();
        crate::runtime::register(&mut reg);
        let mut heap = Heap::new(8192).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&reg, &mut ctx)
    }

    fn new_atomics(reg: &BifRegistry, ctx: &mut BifContext<'_>, arity: i64, opts: i64) -> Eterm {
        let a = ctx.int(arity).expect("small int");
        let o = ctx.int(opts).expect("small int");
        reg.call("erts_internal", "atomics_new", 2, ctx, &[a, o])
            .expect("atomics_new")
    }

    fn new_counters(reg: &BifRegistry, ctx: &mut BifContext<'_>, n: i64) -> Eterm {
        let n = ctx.int(n).expect("small int");
        reg.call("erts_internal", "counters_new", 1, ctx, &[n])
            .expect("counters_new")
    }

    fn map_value(ctx: &BifContext<'_>, map: Eterm, key: &str) -> Eterm {
        let k = ctx.atom(key).expect("key atom");
        beam_term::map::map_get(map, k, ctx.atoms())
            .expect("map_get")
            .expect("present key")
    }

    fn map_keys(ctx: &BifContext<'_>, map: Eterm) -> Vec<Eterm> {
        beam_term::map::map_pairs(map, ctx.atoms())
            .expect("map_pairs")
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    }

    fn eq(ctx: &BifContext<'_>, a: Eterm, b: Eterm) -> bool {
        beam_term::compare::eq(a, b, ctx.atoms())
    }

    /// The `memory` field of a `counters_info/1` result.
    fn counters_memory(reg: &BifRegistry, ctx: &mut BifContext<'_>, r: Eterm) -> Eterm {
        let info = reg
            .call("erts_internal", "counters_info", 1, ctx, &[r])
            .expect("counters_info");
        map_value(ctx, info, "memory")
    }

    fn assert_badarg(result: Result<Eterm>) {
        match result {
            Err(BeamError::BadArg) => {}
            other => panic!("expected badarg, got {other:?}"),
        }
    }

    #[test]
    fn real_bifs_is_the_sorted_thirteen_key_roster() {
        assert_eq!(REAL_BIFS.len(), 13);
        let mut sorted = REAL_BIFS.to_vec();
        sorted.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
        assert!(sorted.iter().eq(REAL_BIFS.iter()), "REAL_BIFS is not sorted");

        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 13, "the module registers exactly its roster");
        for (module, function, arity) in REAL_BIFS {
            assert!(reg.contains(module, function, *arity), "{module}:{function}/{arity}");
        }
    }

    #[test]
    fn resource_id_base_is_disjoint_from_the_other_ref_registries() {
        // The other on-heap-ref id spaces owned by this crate (their constants are
        // private to their modules, so the bases are mirrored here):
        //   `src/binary.rs`        compiled patterns         1 << 32
        //   `src/ets.rs`           continuations            1 << 40
        //   `src/ets/reverse.rs`   reverse continuations    1 << 41
        //   `src/ets.rs`           compiled specs           1 << 48
        const OTHER_BASES: [u64; 4] = [1 << 32, 1 << 40, 1 << 41, 1 << 48];
        assert!(RESOURCE_ID_BASE < (1 << 59), "must fit a 60-bit small integer");
        for base in OTHER_BASES {
            assert_ne!(RESOURCE_ID_BASE, base);
        }
        assert!(RESOURCE_ID_BASE > *OTHER_BASES.iter().max().unwrap());
    }

    #[test]
    fn atomics_new_overrides_the_seam_and_starts_zeroed() {
        with_runtime(|reg, ctx| {
            let r = new_atomics(reg, ctx, 3, 0);
            // If the seam registration had won, `new_atomics` above would have
            // raised the typed `unsupported_bif`; this proves the real BIF is in place.
            assert!(access::is_reference(r));
            assert!(access::ref_thing_id(r).expect("id") >= RESOURCE_ID_BASE);

            let one = ctx.int(1).unwrap();
            let got = reg.call("atomics", "get", 2, ctx, &[r, one]).expect("get");
            assert_eq!(got, ctx.int(0).unwrap());
        });
    }

    #[test]
    fn put_and_get_round_trip() {
        with_runtime(|reg, ctx| {
            let r = new_atomics(reg, ctx, 2, 1); // signed
            let one = ctx.int(1).unwrap();
            let value = ctx.int(-7).unwrap();
            let ok = reg.call("atomics", "put", 3, ctx, &[r, one, value]).unwrap();
            assert_eq!(ok, ctx.atom("ok").unwrap());
            let got = reg.call("atomics", "get", 2, ctx, &[r, one]).unwrap();
            assert_eq!(got, ctx.int(-7).unwrap());
        });
    }

    #[test]
    fn add_returns_ok_and_add_get_returns_the_new_value() {
        with_runtime(|reg, ctx| {
            let r = new_atomics(reg, ctx, 1, 1); // signed
            let one = ctx.int(1).unwrap();
            let five = ctx.int(5).unwrap();
            assert_eq!(
                reg.call("atomics", "add", 3, ctx, &[r, one, five]).unwrap(),
                ctx.atom("ok").unwrap()
            );
            assert_eq!(
                reg.call("atomics", "add_get", 3, ctx, &[r, one, five]).unwrap(),
                ctx.int(10).unwrap()
            );
            assert_eq!(
                reg.call("atomics", "get", 2, ctx, &[r, one]).unwrap(),
                ctx.int(10).unwrap()
            );
        });
    }

    #[test]
    fn exchange_returns_the_previous_value() {
        with_runtime(|reg, ctx| {
            let r = new_atomics(reg, ctx, 1, 1);
            let one = ctx.int(1).unwrap();
            let first = ctx.int(11).unwrap();
            let second = ctx.int(22).unwrap();
            reg.call("atomics", "put", 3, ctx, &[r, one, first]).unwrap();
            let old = reg.call("atomics", "exchange", 3, ctx, &[r, one, second]).unwrap();
            assert_eq!(old, ctx.int(11).unwrap());
            assert_eq!(
                reg.call("atomics", "get", 2, ctx, &[r, one]).unwrap(),
                ctx.int(22).unwrap()
            );
        });
    }

    #[test]
    fn compare_exchange_is_ok_on_match_and_reports_the_actual_value_otherwise() {
        with_runtime(|reg, ctx| {
            let r = new_atomics(reg, ctx, 1, 1);
            let one = ctx.int(1).unwrap();
            let three = ctx.int(3).unwrap();
            let four = ctx.int(4).unwrap();
            let ninety_nine = ctx.int(99).unwrap();
            reg.call("atomics", "put", 3, ctx, &[r, one, three]).unwrap();
            // Match: writes `Desired`, returns `ok`.
            let ok = reg
                .call("atomics", "compare_exchange", 4, ctx, &[r, one, three, four])
                .unwrap();
            assert_eq!(ok, ctx.atom("ok").unwrap());
            assert_eq!(
                reg.call("atomics", "get", 2, ctx, &[r, one]).unwrap(),
                ctx.int(4).unwrap()
            );
            // Mismatch: returns the actual value and writes nothing.
            let actual = reg
                .call(
                    "atomics",
                    "compare_exchange",
                    4,
                    ctx,
                    &[r, one, three, ninety_nine],
                )
                .unwrap();
            assert_eq!(actual, ctx.int(4).unwrap());
            assert_eq!(
                reg.call("atomics", "get", 2, ctx, &[r, one]).unwrap(),
                ctx.int(4).unwrap()
            );
        });
    }

    #[test]
    fn signed_add_wraps_at_64_bits() {
        with_runtime(|reg, ctx| {
            let r = new_atomics(reg, ctx, 1, 1); // signed
            let one = ctx.int(1).unwrap();
            let max = crate::arith::int_from_i64(ctx, i64::MAX).unwrap();
            reg.call("atomics", "put", 3, ctx, &[r, one, max]).unwrap();
            let wrapped = reg
                .call("atomics", "add_get", 3, ctx, &[r, one, one])
                .unwrap();
            let min = crate::arith::int_from_i64(ctx, i64::MIN).unwrap();
            // `i64::MAX + 1` wraps to `i64::MIN` (both are bignums, so compare structurally).
            assert!(eq(ctx, wrapped, min));
        });
    }

    #[test]
    fn put_and_get_accept_a_bignum_that_fits_the_cell() {
        with_runtime(|reg, ctx| {
            // Unsigned: `2^63` and `u64::MAX` are bignums (past the 60-bit small range)
            // but fit the cell, so they must be accepted, not rejected as `badarg`.
            let r = new_atomics(reg, ctx, 2, 0);
            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let high = crate::arith::int_from_mag(ctx, false, &[1u64 << 63]).unwrap();
            let max = crate::arith::int_from_mag(ctx, false, &[u64::MAX]).unwrap();
            reg.call("atomics", "put", 3, ctx, &[r, one, high]).unwrap();
            reg.call("atomics", "put", 3, ctx, &[r, two, max]).unwrap();
            let got_high = reg.call("atomics", "get", 2, ctx, &[r, one]).unwrap();
            let expect_high = crate::arith::int_from_mag(ctx, false, &[1u64 << 63]).unwrap();
            assert!(eq(ctx, got_high, expect_high));
            let got_max = reg.call("atomics", "get", 2, ctx, &[r, two]).unwrap();
            let expect_max = crate::arith::int_from_mag(ctx, false, &[u64::MAX]).unwrap();
            assert!(eq(ctx, got_max, expect_max));
        });
    }

    #[test]
    fn info_reports_the_oracle_bounds_size_and_memory() {
        with_runtime(|reg, ctx| {
            // Unsigned, 3 cells: `max = u64::MAX`, `min = 0`, `size = 3`, `memory = 64`.
            let u = new_atomics(reg, ctx, 3, 0);
            let info = reg.call("atomics", "info", 1, ctx, &[u]).unwrap();
            assert_eq!(
                map_keys(ctx, info),
                vec![
                    ctx.atom("max").unwrap(),
                    ctx.atom("memory").unwrap(),
                    ctx.atom("min").unwrap(),
                    ctx.atom("size").unwrap(),
                ]
            );
            assert_eq!(map_value(ctx, info, "size"), ctx.int(3).unwrap());
            assert_eq!(map_value(ctx, info, "memory"), ctx.int(64).unwrap());
            assert_eq!(map_value(ctx, info, "min"), ctx.int(0).unwrap());
            let expect_max = crate::arith::int_from_mag(ctx, false, &[u64::MAX]).unwrap();
            let got_max = map_value(ctx, info, "max");
            assert!(eq(ctx, got_max, expect_max));

            // Signed, 1 cell: `max = i64::MAX`, `min = i64::MIN`, `size = 1`, `memory = 48`.
            let s = new_atomics(reg, ctx, 1, 1);
            let info = reg.call("atomics", "info", 1, ctx, &[s]).unwrap();
            assert_eq!(map_value(ctx, info, "memory"), ctx.int(48).unwrap());
            assert_eq!(map_value(ctx, info, "size"), ctx.int(1).unwrap());
            let got_max = map_value(ctx, info, "max");
            let expect_max = crate::arith::int_from_i64(ctx, i64::MAX).unwrap();
            assert!(eq(ctx, got_max, expect_max));
            let got_min = map_value(ctx, info, "min");
            let expect_min = crate::arith::int_from_i64(ctx, i64::MIN).unwrap();
            assert!(eq(ctx, got_min, expect_min));

            // An 8-cell unsigned resource follows `40 + 8 * N` (104).
            let big = new_atomics(reg, ctx, 8, 2);
            let info = reg.call("atomics", "info", 1, ctx, &[big]).unwrap();
            assert_eq!(map_value(ctx, info, "memory"), ctx.int(104).unwrap());
        });
    }

    #[test]
    fn atomics_new_opts_bit0_selects_signedness() {
        with_runtime(|reg, ctx| {
            for (opts, signed) in [(0i64, false), (1, true), (2, false), (3, true)] {
                let r = new_atomics(reg, ctx, 3, opts);
                let info = reg.call("atomics", "info", 1, ctx, &[r]).unwrap();
                let got = map_value(ctx, info, "max");
                let expect = if signed {
                    crate::arith::int_from_i64(ctx, i64::MAX).unwrap()
                } else {
                    crate::arith::int_from_mag(ctx, false, &[u64::MAX]).unwrap()
                };
                assert!(eq(ctx, got, expect), "opts={opts}");
            }
        });
    }

    #[test]
    fn counters_get_add_put_and_info() {
        with_runtime(|reg, ctx| {
            let r = new_counters(reg, ctx, 4);
            assert!(access::is_reference(r));
            let two = ctx.int(2).unwrap();
            assert_eq!(
                reg.call("erts_internal", "counters_get", 2, ctx, &[r, two]).unwrap(),
                ctx.int(0).unwrap()
            );
            let add5 = ctx.int(5).unwrap();
            assert_eq!(
                reg.call("erts_internal", "counters_add", 3, ctx, &[r, two, add5])
                    .unwrap(),
                ctx.atom("ok").unwrap()
            );
            assert_eq!(
                reg.call("erts_internal", "counters_get", 2, ctx, &[r, two]).unwrap(),
                ctx.int(5).unwrap()
            );
            // A negative increment subtracts.
            let neg3 = ctx.int(-3).unwrap();
            reg.call("erts_internal", "counters_add", 3, ctx, &[r, two, neg3])
                .unwrap();
            assert_eq!(
                reg.call("erts_internal", "counters_get", 2, ctx, &[r, two]).unwrap(),
                ctx.int(2).unwrap()
            );
            // `put` replaces the value.
            let seven = ctx.int(7).unwrap();
            assert_eq!(
                reg.call("erts_internal", "counters_put", 3, ctx, &[r, two, seven])
                    .unwrap(),
                ctx.atom("ok").unwrap()
            );
            assert_eq!(
                reg.call("erts_internal", "counters_get", 2, ctx, &[r, two]).unwrap(),
                ctx.int(7).unwrap()
            );

            // `info` has the `memory`/`size` keys in that order.
            let info = reg.call("erts_internal", "counters_info", 1, ctx, &[r]).unwrap();
            assert_eq!(
                map_keys(ctx, info),
                vec![ctx.atom("memory").unwrap(), ctx.atom("size").unwrap()]
            );
            assert_eq!(map_value(ctx, info, "size"), ctx.int(4).unwrap());
            let lines = (4u64 + ATOMICS_PER_CACHE_LINE - 1) / ATOMICS_PER_CACHE_LINE;
            let expect_mem = 32 + atomics_per_counter() * lines * ERTS_CACHE_LINE_SIZE;
            assert_eq!(
                map_value(ctx, info, "memory"),
                ctx.int(expect_mem as i64).unwrap()
            );
        });
    }

    #[test]
    fn counters_info_memory_rounds_up_by_cache_line() {
        with_runtime(|reg, ctx| {
            // N=1 and N=8 (one 8-counter cache line) report the same memory; N=9 needs two.
            let one = new_counters(reg, ctx, 1);
            let eight = new_counters(reg, ctx, 8);
            let nine = new_counters(reg, ctx, 9);
            let m_one = counters_memory(reg, ctx, one);
            let m_eight = counters_memory(reg, ctx, eight);
            let m_nine = counters_memory(reg, ctx, nine);
            assert!(eq(ctx, m_one, m_eight));
            let one_line = 32 + atomics_per_counter() * ERTS_CACHE_LINE_SIZE;
            let two_lines = 32 + atomics_per_counter() * 2 * ERTS_CACHE_LINE_SIZE;
            assert_eq!(m_one, ctx.int(one_line as i64).unwrap());
            assert_eq!(m_nine, ctx.int(two_lines as i64).unwrap());
        });
    }

    #[test]
    fn badarg_classes() {
        with_runtime(|reg, ctx| {
            let zero = ctx.int(0).unwrap();
            let one = ctx.int(1).unwrap();
            let three = ctx.int(3).unwrap();
            let neg = ctx.int(-1).unwrap();
            let four = ctx.int(4).unwrap();
            let float = ctx.make_float(1.0).unwrap();
            let atom = ctx.atom("a").unwrap();

            // `atomics_new/2` argument classes.
            assert_badarg(reg.call("erts_internal", "atomics_new", 2, ctx, &[zero, zero]));
            assert_badarg(reg.call("erts_internal", "atomics_new", 2, ctx, &[neg, zero]));
            assert_badarg(reg.call("erts_internal", "atomics_new", 2, ctx, &[float, zero]));
            assert_badarg(reg.call("erts_internal", "atomics_new", 2, ctx, &[three, float]));
            assert_badarg(reg.call("erts_internal", "atomics_new", 2, ctx, &[three, neg]));
            assert_badarg(reg.call("erts_internal", "atomics_new", 2, ctx, &[three, atom]));

            // A valid unsigned atomics resource for the op/index classes.
            let u = new_atomics(reg, ctx, 3, 0);
            assert_badarg(reg.call("atomics", "get", 2, ctx, &[u, zero]));
            assert_badarg(reg.call("atomics", "get", 2, ctx, &[u, neg]));
            assert_badarg(reg.call("atomics", "get", 2, ctx, &[u, float]));
            assert_badarg(reg.call("atomics", "get", 2, ctx, &[u, four]));
            // A non-reference handle.
            assert_badarg(reg.call("atomics", "get", 2, ctx, &[three, one]));
            // A value that does not fit the cell (negative on an unsigned resource).
            assert_badarg(reg.call("atomics", "put", 3, ctx, &[u, one, neg]));
            // A float increment.
            assert_badarg(reg.call("atomics", "add", 3, ctx, &[u, one, float]));

            // Cross-kind: a `counters` handle to the `atomics` family and vice versa.
            let c = new_counters(reg, ctx, 1);
            assert_badarg(reg.call("atomics", "get", 2, ctx, &[c, one]));
            assert_badarg(reg.call("atomics", "info", 1, ctx, &[c]));
            assert_badarg(reg.call("erts_internal", "counters_get", 2, ctx, &[u, one]));
            assert_badarg(reg.call("erts_internal", "counters_info", 1, ctx, &[u]));

            // `info` on a non-reference.
            assert_badarg(reg.call("atomics", "info", 1, ctx, &[three]));
            assert_badarg(reg.call("erts_internal", "counters_info", 1, ctx, &[three]));

            // `counters_new/1` argument classes.
            assert_badarg(reg.call("erts_internal", "counters_new", 1, ctx, &[zero]));
            assert_badarg(reg.call("erts_internal", "counters_new", 1, ctx, &[neg]));
            assert_badarg(reg.call("erts_internal", "counters_new", 1, ctx, &[float]));

            // `counters_put/3` requires a value that fits `i64`: `u64::MAX` is `badarg`.
            let too_big = crate::arith::int_from_mag(ctx, false, &[u64::MAX]).unwrap();
            assert_badarg(reg.call(
                "erts_internal",
                "counters_put",
                3,
                ctx,
                &[c, one, too_big],
            ));
            // ... but `counters_add/3` accepts it as a wrapping increment.
            assert_eq!(
                reg.call("erts_internal", "counters_add", 3, ctx, &[c, one, too_big])
                    .unwrap(),
                ctx.atom("ok").unwrap()
            );
            let got = reg.call("erts_internal", "counters_get", 2, ctx, &[c, one]).unwrap();
            let minus_one = ctx.int(-1).unwrap();
            assert_eq!(got, minus_one);
        });
    }

    #[test]
    fn absurd_sizes_are_system_limit() {
        with_runtime(|reg, ctx| {
            // Above [`COUNTERS_MAX_SIZE`] the element count is rejected deterministically.
            let huge = ctx.int(1 << 33).unwrap();
            assert!(matches!(
                reg.call("erts_internal", "counters_new", 1, ctx, &[huge]),
                Err(BeamError::SystemLimit)
            ));

            // `atomics_new` has no such static bound (the C's guard is arithmetic
            // overflow only), so an element count the allocator cannot serve must be
            // reported as `system_limit` — never an abort. `1 << 50` u64 cells is
            // ~9 PiB.
            let enormous = ctx.int(1 << 50).unwrap();
            let opts = ctx.int(0).unwrap();
            assert!(matches!(
                reg.call("erts_internal", "atomics_new", 2, ctx, &[enormous, opts]),
                Err(BeamError::SystemLimit)
            ));
        });
    }

    #[test]
    fn distinct_handles_are_independent() {
        with_runtime(|reg, ctx| {
            let a = new_atomics(reg, ctx, 1, 1);
            let b = new_atomics(reg, ctx, 1, 1);
            assert_ne!(a, b);
            let one = ctx.int(1).unwrap();
            let five = ctx.int(5).unwrap();
            reg.call("atomics", "put", 3, ctx, &[a, one, five]).unwrap();
            assert_eq!(
                reg.call("atomics", "get", 2, ctx, &[a, one]).unwrap(),
                ctx.int(5).unwrap()
            );
            assert_eq!(
                reg.call("atomics", "get", 2, ctx, &[b, one]).unwrap(),
                ctx.int(0).unwrap()
            );
        });
    }
}
