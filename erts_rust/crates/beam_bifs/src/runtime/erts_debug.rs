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

//! The `erts_debug`/`erts_trace_cleaner`/`erts_literal_area_collector` BIFs (the
//! internal debugging tools).
//!
//! Every key below is a declared `bif.tab` entry. Most of them are **host-independent
//! constants, pure term operations or self-contained decode/table work**, and those are
//! implemented here **for real** (see `REAL_BIFS`): `dist_ext_to_term/2` (the
//! distribution decode, over the shared `beam_codec` decoder with an `AtomCache`),
//! `instructions/0` (this tree's generated `beam_makeops` roster),
//! `unaligned_bitstring/2` (a bitstring-to-bitstring transformation), `map_info/1` (a
//! walk of a HAMT map's node interiors), the `lcnt`/`interpreter_size`/`disassemble`
//! constants, the `get_internal_state` / `set_internal_state` gate and its three
//! debugging **loop limits** (`binary_loop_limit`/`re_loop_limit`/
//! `unicode_loop_limit` — see [`LoopLimit`]), `size_shared/1` and
//! `copy_shared/2` (term walks), and the two `notsup` protocol stubs of the
//! trace/literal-area collectors.
//!
//! The rest require the *running* runtime (VM introspection of dirty schedulers and
//! breakpoints). Each of those is registered through the single
//! [`RuntimeHooks`](crate::context::RuntimeHooks) seam, so with no hook installed a call
//! answers the typed `unsupported_bif` capability gap — never `undef`, never a
//! fabricated value.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use beam_common::{BeamError, ExitReason, Result};
use beam_term::prelude::signed_value;
use beam_term::size::{size_object_with, DefaultSizingPolicy, SizingPolicy};
use beam_term::{atom_val, Eterm, NIL};

use crate::access;
use crate::arith;
use crate::context::BifContext;
use crate::error::{badarg, raise_error, structured_error, system_limit, undefined_bif};
use crate::error::unsupported_bif;
use crate::registry::BifRegistry;

/// The `(module, function, arity)` keys this module implements **for real** (rather
/// than forwarding to the [`RuntimeHooks`](crate::context::RuntimeHooks) seam).
///
/// Sorted by `(module, function, arity)`. `src/hook_sets.rs`'s `RUNTIME_HOOKS_BIFS`
/// roster is asserted **equal** to the seam roster, so a key listed here must not be
/// there; [`register`] asserts every one of them is actually present.
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[
    ("erts_debug", "copy_shared", 2),
    ("erts_debug", "disassemble", 1),
    ("erts_debug", "dist_ext_to_term", 2),
    ("erts_debug", "get_internal_state", 1),
    ("erts_debug", "instructions", 0),
    ("erts_debug", "interpreter_size", 0),
    ("erts_debug", "lcnt_clear", 0),
    ("erts_debug", "lcnt_collect", 0),
    ("erts_debug", "lcnt_control", 1),
    ("erts_debug", "lcnt_control", 2),
    ("erts_debug", "map_info", 1),
    ("erts_debug", "set_internal_state", 2),
    ("erts_debug", "size_shared", 1),
    ("erts_debug", "unaligned_bitstring", 2),
    ("erts_literal_area_collector", "send_copy_request", 3),
    ("erts_trace_cleaner", "send_trace_clean_signal", 1),
];

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    crate::runtime::runtime_bif!(reg, "erts_debug", breakpoint, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", dirty, 3);
    crate::runtime::runtime_bif!(reg, "erts_debug", dirty_cpu, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", dirty_io, 2);

    // The host-independent keys, implemented below. `dist_ext_to_term/2` is the
    // distribution decode over the shared `beam_codec` decoder and
    // `instructions/0` renders this tree's generated `beam_makeops` roster;
    // `map_info/1` summarises a HAMT map's node interior; `unaligned_bitstring/2`
    // is a bitstring-to-bitstring transformation; the
    // `lcnt`/`interpreter_size`/`disassemble` keys are the constants a build
    // without those facilities answers; the internal-state pair is the
    // `available_internal_state` gate; `size_shared`/`copy_shared` are term walks;
    // and the two collector keys are `notsup` stubs (their machinery does not
    // exist in this runtime at all — see the module docs on each).
    reg.register("erts_debug", "copy_shared", 2, copy_shared_2);
    reg.register("erts_debug", "disassemble", 1, disassemble_1);
    reg.register("erts_debug", "dist_ext_to_term", 2, dist_ext_to_term_2);
    reg.register("erts_debug", "get_internal_state", 1, get_internal_state_1);
    reg.register("erts_debug", "instructions", 0, instructions_0);
    reg.register("erts_debug", "interpreter_size", 0, interpreter_size_0);
    reg.register("erts_debug", "lcnt_clear", 0, lcnt_clear_0);
    reg.register("erts_debug", "lcnt_collect", 0, lcnt_collect_0);
    reg.register("erts_debug", "lcnt_control", 1, lcnt_control_1);
    reg.register("erts_debug", "lcnt_control", 2, lcnt_control_2);
    reg.register("erts_debug", "map_info", 1, map_info_1);
    reg.register(
        "erts_debug",
        "set_internal_state",
        2,
        set_internal_state_2,
    );
    reg.register("erts_debug", "size_shared", 1, size_shared_1);
    reg.register("erts_debug", "unaligned_bitstring", 2, unaligned_bitstring);
    reg.register(
        "erts_literal_area_collector",
        "send_copy_request",
        3,
        send_copy_request_3,
    );
    reg.register(
        "erts_trace_cleaner",
        "send_trace_clean_signal",
        1,
        send_trace_clean_signal_1,
    );
    // `REAL_BIFS` is the declared roster; keep it honest against what was registered.
    debug_assert!(
        REAL_BIFS
            .iter()
            .all(|(module, function, arity)| reg.contains(module, function, *arity)),
        "REAL_BIFS lists a key that this module does not register"
    );
}

/// `erts_debug:interpreter_size/0` — the size of the BEAM interpreter's instruction
/// dispatch table in words.
///
/// Ground truth: `../../erts/emulator/beam/beam_debug.c:517-532`
/// (`erts_debug_interpreter_size_0`). A `BEAMASM` build (which is every default
/// OTP) has no interpreter to measure and answers `make_small(0)`; the
/// `#ifndef BEAMASM` arm measures `high - low` over the opcode addresses. This
/// runtime keeps no such table, so the `BEAMASM` answer is the faithful one.
/// Oracle-verified: the integer `0` (not the atom `false`).
///
/// # Errors
///
/// Propagates nothing: `0` is always a small integer.
fn interpreter_size_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    ctx.int(0)
}

/// `erts_debug:disassemble/1` — disassemble the code at an address or `MFA`.
///
/// Ground truth: `../../erts/emulator/beam/beam_debug.c:396-515`
/// (`erts_debug_disassemble_1`). Its `BEAMASM` arm is a bare `BIF_RET(am_false)`
/// *before* the argument is looked at, so every argument — a loaded module name, an
/// unloaded atom, an integer, a list — answers `false`. Oracle-verified for
/// `{lists,map,2}`, `lists`, `no_such_module_xyz`, `42` and `[a,b]`.
///
/// # Errors
///
/// [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) when the atom
/// `false` is neither interned nor internable — unreachable in a real runtime.
fn disassemble_1(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(false)
}

/// `erts_debug:lcnt_clear/0` — clear the lock-counter samples.
///
/// Ground truth: `../../erts/emulator/beam/erl_bif_info.c:6311-6320`. Without
/// `ERTS_ENABLE_LOCK_COUNT` (every default build) it is a bare `BIF_RET(am_error)`;
/// with it, `am_ok`. Oracle-verified on OTP 29.1.1: the atom `error`.
///
/// # Errors
///
/// [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) when the atom
/// `error` is neither interned nor internable.
fn lcnt_clear_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    ctx.atom("error")
}

/// `erts_debug:lcnt_collect/0` — collect the lock-counter samples.
///
/// Ground truth: `../../erts/emulator/beam/erl_bif_info.c:6322-6355`: without
/// `ERTS_ENABLE_LOCK_COUNT` a bare `BIF_RET(am_error)`. Oracle-verified: the atom
/// `error` — not a `{ok, {locks, []}}` shape.
///
/// # Errors
///
/// [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) when the atom
/// `error` is neither interned nor internable.
fn lcnt_collect_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    ctx.atom("error")
}

/// `erts_debug:lcnt_control/1` — read the lock-counter category mask.
///
/// Ground truth: `../../erts/emulator/beam/erl_bif_info.c:6357-6382`. The whole body
/// sits under `#ifdef ERTS_ENABLE_LOCK_COUNT`; without it the function falls through
/// to the trailing `BIF_ERROR(BIF_P, BADARG)` for **every** argument. Oracle-verified
/// for `clear`, `{clear,[]}`, `[a]` and `42`.
///
/// # Errors
///
/// Always [`BeamError::BadArg`](beam_common::BeamError::BadArg).
fn lcnt_control_1(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(badarg())
}

/// `erts_debug:lcnt_control/2` — set the lock-counter category mask / `copy_save`.
///
/// Ground truth: `../../erts/emulator/beam/erl_bif_info.c:6384-6418`: without
/// `ERTS_ENABLE_LOCK_COUNT` the trailing `BIF_ERROR(BIF_P, BADARG)` is the only
/// reachable outcome. Oracle-verified for `clear`, `a`, and `42,43`.
///
/// # Errors
///
/// Always [`BeamError::BadArg`](beam_common::BeamError::BadArg).
fn lcnt_control_2(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(badarg())
}

/// The `available_internal_state` gate (`erl_bif_info.c:4341`,
/// `erts_bif_info_init` at `:6459` initialises it to `0`).
///
/// A fresh VM has it **off**, and off means `erts_debug:get_internal_state/1` and
/// `erts_debug:set_internal_state/2` raise `EXC_UNDEF` — the bare atom `undef`, which
/// is why the oracle's answer to a gate-off call is `{error,undef}` and not a
/// `badarg` for a bogus argument. The one gate-*free* arm is
/// `set_internal_state(available_internal_state, Bool)`, which flips the gate and
/// returns its previous value.
///
/// The gate is process-global (as in ERTS) and lives here as a single atomic; it is
/// per-VM state, so it is deliberately not per-`BifContext`.
static AVAILABLE_INTERNAL_STATE: AtomicBool = AtomicBool::new(false);

/// `erts_debug:get_internal_state/1` — the emulator's internal state.
///
/// Ground truth: `../../erts/emulator/beam/erl_bif_info.c:4346-4349`: the *first*
/// thing the BIF does is
///
/// ```text
/// if (!erts_atomic_read_nob(&available_internal_state)) BIF_ERROR(BIF_P, EXC_UNDEF);
/// ```
///
/// so with the gate off (a fresh VM) **every** argument — `memory`,
/// `available_internal_state`, or garbage — raises the bare atom `undef`.
/// Oracle-verified.
///
/// With the gate **on** the oracle serves real introspection (`memory` answers the
/// allocator totals, a bogus argument answers `badarg`), and none of those values can
/// be computed by this runtime (the same `erlang:memory/0` shape is a `MemoryHook`
/// seam, and the rest is scheduler/process-table internals) — so a gate-on call
/// answers the typed [`unsupported_bif`] capability gap: a real function the host has
/// not provided, never a fabricated value and never `undef`.
///
/// # Errors
///
/// `error:undef` while the gate is off (see above), the typed `unsupported_bif` raise
/// while it is on.
fn get_internal_state_1(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    if AVAILABLE_INTERNAL_STATE.load(Ordering::SeqCst) {
        Err(unsupported_bif())
    } else {
        Err(undefined_bif())
    }
}

/// `erts_debug:set_internal_state/2` — set the emulator's internal state.
///
/// Ground truth: `../../erts/emulator/beam/erl_bif_info.c:5040-5071`. The
/// **gate-free** arm comes first and is the only one reachable while the gate is off:
///
/// ```text
/// if (ERTS_IS_ATOM_STR("available_internal_state", BIF_ARG_1)
///     && (BIF_ARG_2 == am_true || BIF_ARG_2 == am_false)) {
///     ... erts_atomic_xchg_nob(&available_internal_state, on) ...
///     BIF_RET(prev_on ? am_true : am_false);
/// }
/// if (!erts_atomic_read_nob(&available_internal_state)) BIF_ERROR(BIF_P, EXC_UNDEF);
/// ```
///
/// So `set_internal_state(available_internal_state, true|false)` returns the
/// **previous** boolean and needs no gate, while anything else — including
/// `set_internal_state(available_internal_state, 42)`, whose second argument fails the
/// `am_true`/`am_false` test and therefore falls through to the gate check — raises
/// the bare atom `undef` while the gate is off. Oracle-verified: `false` on the first
/// enable, `true` on the second, `true` on the disable, and `{error,undef}` for the
/// non-boolean.
///
/// With the gate on, the three debugging **loop limits** (`binary_loop_limit`,
/// `re_loop_limit`, `unicode_loop_limit`) are served by [`set_loop_limit`]: each is a
/// bare `xchg` over one emulator atomic (`erl_bif_info.c:5204-5236`), so its answer
/// needs no VM introspection and this runtime reproduces it exactly. Every other arm
/// (`reds_left`, `next_pid`, `block`, …) is an emulator internal this runtime cannot
/// serve, so it answers the typed [`unsupported_bif`] gap.
///
/// # Errors
///
/// `error:undef` while the gate is off and the first argument is not the
/// `available_internal_state` pair; `error:badarg` for a loop-limit arm whose second
/// argument is neither the atom `default` nor a non-negative integer fitting an `Uint`
/// (the C falls out of the `else if` chain to the function's final `badarg`); the typed
/// `unsupported_bif` raise for every other arm while the gate is on.
fn set_internal_state_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if atom_is(ctx, args[0], "available_internal_state") {
        if let Some(on) = boolean_of(ctx, args[1]) {
            let previous = AVAILABLE_INTERNAL_STATE.swap(on, Ordering::SeqCst);
            return ctx.boolean(previous);
        }
    }
    if !AVAILABLE_INTERNAL_STATE.load(Ordering::SeqCst) {
        return Err(undefined_bif());
    }
    if let Some(answer) = set_loop_limit(ctx, args[0], args[1]) {
        return answer;
    }
    Err(unsupported_bif())
}

/// The three debugging **loop limits** `set_internal_state/2` can tune, as the
/// process-global emulator atomics ERTS keeps for them.
///
/// Ground truth: the three arms at `../../erts/emulator/beam/erl_bif_info.c:5204-5236`
/// over `erts_binary_set_loop_limit` (`erl_bif_binary.c:111-121`),
/// `erts_re_set_loop_limit` (`erl_bif_re.c:229-237`) and `erts_unicode_set_loop_limit`
/// (`erl_unicode.c:155-163`), every one of which is the same exchange:
///
/// ```text
/// Sint save = (Sint) max_loop_limit;
/// if (limit <= 0) max_loop_limit = <that module's default>;
/// else            max_loop_limit = (Uint) limit;
/// return save;                                  /* the PREVIOUS value */
/// ```
///
/// The BIF arm maps the atom `default` to the C's `-1`, converts anything else with
/// `term_to_Uint` (`../erts/emulator/beam/big.c:3505-3538`) and lets a failed
/// conversion fall out of the `else if` chain to the function's final
/// `BIF_ERROR(BIF_P, BADARG)` — so `badarg` covers a negative integer, a float, an atom
/// and a bignum too wide for one `Uint`, with no `system_limit` in between (oracle:
/// `1 bsl 64` ⇒ `{error,badarg}`).
///
/// The initial values are the C's initialisers — `CONTEXT_REDS * LOOP_FACTOR` =
/// `4000 * 16 = 64_000` for `re` (`erl_bif_re.c:42`), `4000 * 10 = 40_000` for
/// `unicode` (`erl_unicode.c:48`) and **`0`** ("disabled", `erl_bif_binary.c:102`) for
/// `binary` — stored as the `Uint` bit pattern of the `Sint` limit.
///
/// Process-global like [`AVAILABLE_INTERNAL_STATE`] (ERTS keeps one `max_loop_limit`
/// per module), never per-`BifContext`.
static BINARY_LOOP_LIMIT: AtomicU64 = AtomicU64::new(0);
static RE_LOOP_LIMIT: AtomicU64 = AtomicU64::new(64_000);
static UNICODE_LOOP_LIMIT: AtomicU64 = AtomicU64::new(40_000);

/// One of the three loop limits: which atomic it names and what its default is.
///
/// # What a limit does — and does not — change
///
/// A limit is only the number of loop iterations a **trapping** operation
/// (`binary`'s search family, `re`'s NFA run, `unicode`'s conversion walkers) may run
/// before it yields back to the scheduler — `get_reds`/`allowed_iterations` in those
/// same modules. It is a **scheduling-granularity knob, never a semantic one**: no
/// result, exception class or term depends on it, so `1`, `0` and `default` are
/// indistinguishable to a program apart from *when* it is pre-empted (and this
/// runtime's `binary`/`re`/`unicode` BIFs do not trap at all, so the stored values are
/// inert here). Serving the arms is therefore faithful and side-effect-free.
#[derive(Clone, Copy)]
enum LoopLimit {
    Binary,
    Re,
    Unicode,
}

impl LoopLimit {
    /// The limit `name` selects, or `None` when the atom is not one of the three (the
    /// caller then falls through to the typed gap — the C's `else if` chain).
    fn of(ctx: &BifContext<'_>, name: Eterm) -> Option<LoopLimit> {
        if atom_is(ctx, name, "binary_loop_limit") {
            Some(LoopLimit::Binary)
        } else if atom_is(ctx, name, "re_loop_limit") {
            Some(LoopLimit::Re)
        } else if atom_is(ctx, name, "unicode_loop_limit") {
            Some(LoopLimit::Unicode)
        } else {
            None
        }
    }

    /// The module's default (`max_loop_limit`'s initialiser, restored by `default` and
    /// by any non-positive limit).
    fn default_bits(self) -> u64 {
        match self {
            LoopLimit::Binary => 0,
            LoopLimit::Re => 64_000,
            LoopLimit::Unicode => 40_000,
        }
    }

    /// The module-global slot this limit lives in.
    fn slot(self) -> &'static AtomicU64 {
        match self {
            LoopLimit::Binary => &BINARY_LOOP_LIMIT,
            LoopLimit::Re => &RE_LOOP_LIMIT,
            LoopLimit::Unicode => &UNICODE_LOOP_LIMIT,
        }
    }

    /// `erts_<module>_set_loop_limit(requested)`: store the new limit and answer the
    /// **previous** one (read before the store, as the C's `Sint save` is). A
    /// non-positive request — the atom `default` (the C's `-1`), the integer `0`, or a
    /// `Uint` whose `Sint` reinterpretation is negative — restores the default.
    fn exchange(self, requested: i64) -> u64 {
        let new = if requested <= 0 {
            self.default_bits()
        } else {
            requested as u64
        };
        self.slot().swap(new, Ordering::SeqCst)
    }
}

/// The C's `BIF_RET(make_small(max_loops))` as `signed_val` reads it back.
///
/// `make_small(x)` is `(x << 4) + TAG_IMMED1_SMALL` and `signed_val` is an arithmetic
/// `>> 4` over the 60-bit immediate, so a stored limit in `2^59 .. 2^63` — reachable
/// only through a **bignum** second argument — answers **sign-wrapped**, exactly as the
/// oracle answers it: after `set_internal_state(binary_loop_limit, 1 bsl 59)` the next
/// call answers `-576460752303423488` (`-2^59`) and after `1 bsl 62 + 7` it answers
/// `7`. The wrap is the C's own term construction, not a substitution: the stored limit
/// itself is faithful and no limit is ever *used* at these magnitudes.
fn make_small_answer(bits: u64) -> i64 {
    // `TAG_IMMED1_SMALL` (`erl_term.h`); irrelevant to the answer below (`bits << 4`
    // has a zero low nibble) but kept so the round trip reads like the C.
    const TAG_IMMED1_SMALL: u64 = 0xF;
    ((bits << 4).wrapping_add(TAG_IMMED1_SMALL) as i64) >> 4
}

/// The C's `term_to_Uint` (`../erts/emulator/beam/big.c:3505-3538`) for the limit arms:
/// the `Uint` a non-negative integer term denotes, or `None` for any other term.
///
/// A small integer is its value when non-negative. A **bignum** is accepted iff it is
/// non-negative and its magnitude fits one `ErtsDigit` — the C's
/// `xl * D_EXP > sizeof(Uint) * 8` reject, `D_EXP == 64` here, so exactly one limb; the
/// magnitude is normalised first (leading zero limbs dropped), as
/// [`crate::runtime::atomics`]'s `term_to_uword` does for the same reason. A negative
/// integer, a float, an atom, a list, … are all rejected.
///
/// The C stores `BADARG`/`SYSTEM_LIMIT` in `*up` on failure but the limit arms ignore
/// which — every failure falls to the function's final `badarg` — so one `None` is the
/// faithful answer for all of them.
fn loop_limit_argument(term: Eterm) -> Option<u64> {
    if access::is_small(term) {
        return u64::try_from(signed_value(term)).ok();
    }
    if access::is_big(term) {
        let (negative, limbs) = beam_codec::term::big_parts(term).ok()?;
        if negative {
            return None;
        }
        return match limbs.iter().rposition(|limb| *limb != 0) {
            // An all-zero magnitude is the integer 0.
            None => Some(0),
            // One `ErtsDigit` — the widest bignum `term_to_Uint` accepts.
            Some(0) => Some(limbs[0]),
            Some(_) => None,
        };
    }
    None
}

/// The `set_internal_state/2` loop-limit arm: `Some(answer)` when `name` is one of the
/// three limits, `None` when it is not (the caller then answers the typed gap).
///
/// See [`LoopLimit`] for the C it reproduces, the defaults and why no observable
/// depends on the stored value.
///
/// # Errors
///
/// `error:badarg` when the second argument is neither the atom `default` nor a
/// non-negative integer fitting an `Uint`; a heap failure materialising the answer
/// (unreachable: the answer is always a small integer).
fn set_loop_limit(
    ctx: &mut BifContext<'_>,
    name: Eterm,
    value: Eterm,
) -> Option<Result<Eterm>> {
    let limit = LoopLimit::of(ctx, name)?;
    // `default` is the C's `erts_*_set_loop_limit(-1)`; anything else must pass
    // `term_to_Uint`, whose `Uint` result is reinterpreted as the C's `Sint limit`
    // (so a value ≥ 2^63 is negative and restores the default).
    let requested = if atom_is(ctx, value, "default") {
        -1
    } else {
        match loop_limit_argument(value) {
            Some(bits) => bits as i64,
            None => return Some(Err(badarg())),
        }
    };
    let previous = limit.exchange(requested);
    Some(arith::int_from_i64(ctx, make_small_answer(previous)))
}

/// `erts_debug:size_shared/1` — the words of `Term`'s graph, each distinct object
/// counted once.
///
/// Ground truth: `size_shared(obj)` in `../../erts/emulator/beam/copy.c:315-345`
/// whose own comment says "This currently returns the same as `erts_debug:size/1`.
/// It is argued whether the size of subterms in constant pools should be counted or
/// not." The algorithm is `size_object`'s per-shape word count (the same
/// [`beam_term::layout::object_words`] table this runtime already uses for
/// `erts_debug:flat_size/1` and the copy engines), walked with a **visited set**:
/// an object reached a second time contributes `0` and is not descended into.
///
/// `beam_term` exposes exactly that as a [`SizingPolicy`], so the dedup is the
/// policy's [`is_shared`](SizingPolicy::is_shared) and the walk stays the canonical
/// one ([`size_object_with`]).
///
/// # Hash-map interior nodes: priced twice, like the oracle
///
/// The C's `size_shared` is *not* exactly `size/1` for a map above
/// `MAP_SMALL_MAP_LIMIT`: its `MAP_SUBTAG` arm adds `1 + n + header_arity(hdr)` for a
/// HAMT node and the node is charged again when it is reached a second time through
/// the queue, so the answer is `size/1` plus that map's own node words.
/// Oracle-verified (OTP 29.1.1 / erts-17.1) on `maps:from_list([{I,I} || I <-
/// lists:seq(1,N)])`:
///
/// ```text
/// N  size_shared  size/flat_size
/// 33         180            123
/// 34         184            126
/// 40         216            148
/// 100        528            364
/// ```
///
/// and the same `+node words` delta holds with tuple- or list-valued maps (33 keys
/// with `{I,I}` values: 279 vs 222). [`SharedOnce`] applies exactly that doubling, so
/// this BIF reproduces the oracle's number for every map shape.
///
/// # The one unrepresentable half: literal areas
///
/// The C also skips any subterm that lives in a **literal area**
/// (`erts_is_literal`, `COUNT_OFF_HEAP == 0`, `copy.c:330-333`), which is why a
/// *constant* such as `[a,b]` compiled into a module answers `0` there while
/// `erts_debug:size/1` answers `4`. This runtime has no literal areas at all (every
/// term a BIF sees is built on a process heap, see `crate::bits`), so no subterm is
/// ever skipped for that reason and a term the oracle keeps in a literal area is
/// counted here exactly as the oracle counts the same term built at run time. Every
/// **run-time-built** term — the whole graph a program can actually construct through
/// BIFs — agrees, which is what the module tests and
/// `tests/debugger_constants_oracle.rs` pin against the oracle.
///
/// # Errors
///
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) if the count does
/// not fit a small integer (unreachable: a heap that large cannot exist), or a heap
/// failure from materialising the integer.
fn size_shared_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let words = i64::try_from(size_object_with(args[0], &SharedOnce::new()))
        .map_err(|_| system_limit())?;
    arith::int_from_i64(ctx, words)
}

/// The `size_shared/1` pricing policy: an object is priced (and descended into) the
/// **first** time it is reached and contributes nothing afterwards.
///
/// The identity is the term word itself — for every non-immediate the frozen `Eterm`
/// layout carries the object's heap address (`beam_term` §2), so two references to
/// one object are the same word. `is_shared` is only ever called by the sizing walk
/// for a non-immediate ([`beam_term::size`]'s `needs_visit`), so an immediate can
/// never enter the set.
///
/// It also reproduces the one per-shape quirk `size_shared` differs from
/// [`size_object`](beam_term::size::size_object) in: a **hash-map interior node** is
/// priced **twice** (see [`size_shared_1`]'s "hash-map nodes" note).
struct SharedOnce {
    seen: RefCell<HashSet<Eterm>>,
}

impl SharedOnce {
    fn new() -> Self {
        SharedOnce {
            seen: RefCell::new(HashSet::new()),
        }
    }
}

impl SizingPolicy for SharedOnce {
    fn is_shared(&self, t: Eterm) -> bool {
        // `true` = already counted through another reference: price 0, do not descend.
        !self.seen.borrow_mut().insert(t)
    }

    fn object_words(&self, header: Eterm, payload: *const Eterm) -> Option<usize> {
        // The canonical per-shape pricing, reached through the trait's **safe** default
        // method (`SizingPolicy::object_words`), so this module never touches a raw
        // pointer itself.
        let words = DefaultSizingPolicy.object_words(header, payload)?;
        // The C's `size_shared` charges a HAMT node's own words twice (`copy.c:459-472`
        // plus the node reached again through the queue), which is why the oracle's
        // `size_shared` of a >32-key map is `size/1` plus that map's node words
        // (oracle-verified: 33 keys ⇒ 180 vs 123, 40 ⇒ 216 vs 148, 100 ⇒ 528 vs 364;
        // same delta with tuple- or list-valued maps). `size_object_x` prices a node
        // once (`copy.c:192-214`), so the doubling happens here rather than in the
        // shared walk. `access::is_hashmap_node_header` is a pure header-word test.
        Some(if access::is_hashmap_node_header(header) {
            words * 2
        } else {
            words
        })
    }
}
/// `erts_debug:copy_shared/2` — a sharing-preserving copy of `Term`.
///
/// Ground truth: `../../erts/emulator/beam/beam_debug.c:168-190`
/// (`erts_debug_copy_shared_2`). The **second** argument is inspected first and must
/// be exactly `true` (`copy_literals = 1`) or `false` (`0`); anything else is
/// `badarg` *before* `Term` is looked at — oracle-verified with `atom`, `1` and `[]`
/// as the second argument of `copy_shared(foo, _)`, all `{error,badarg}`.
///
/// # What the copy is here
///
/// `copy_literals` selects whether subterms living in a module's **literal area** are
/// copied onto the process heap or left shared: `erts_debug:same(copy_shared(Lit,
/// false), Lit)` is `true` on the oracle while `copy_shared(Lit, true)` returns a
/// fresh graph. This runtime has no literal areas (see
/// [`size_shared_1`]'s note), so every term, `Term` included, already lives on a
/// process heap and is already "the copy": the BIF returns `Term` itself. Structurally
/// it is `=:=` to the oracle's result for every argument (oracle-verified for a
/// literal list, a heap tuple, a 1000-byte refc binary, a 40-key map and an external
/// fun), and its sharing structure is preserved — only `erts_debug:same/2` against the
/// original cannot see the fresh address the oracle's `copy_literals = true` copy
/// gets, which is precisely the literal-area model this runtime does not have.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a second argument that is
/// not the atom `true` or `false`.
fn copy_shared_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    // The C switches on `BIF_ARG_2` first, so an invalid flag wins over any `Term`.
    if boolean_of(ctx, args[1]).is_none() {
        return Err(badarg());
    }
    Ok(args[0])
}

/// `erts_trace_cleaner:send_trace_clean_signal/1` — the trace-session cleanup RPC.
///
/// Ground truth: `../../erts/emulator/beam/erl_bif_trace.c:649-657`: the BIF is
/// restricted to the emulator's own `erts_trace_cleaner` process and raises
/// `EXC_NOTSUP` otherwise — before the target argument is inspected. This runtime has
/// no such process (nor a trace-session cleaner), so `notsup` is the faithful answer
/// for every argument. Oracle-verified for `1`, `[]`, `self()` and `a`.
///
/// # Errors
///
/// Always `error:notsup`.
fn send_trace_clean_signal_1(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(notsup())
}

/// `erts_literal_area_collector:send_copy_request/3` — the copy-literals request.
///
/// Ground truth: `../../erts/emulator/beam/beam_bif_load.c:1749-1759`: restricted to
/// the emulator's own `erts_literal_area_collector` process, `EXC_NOTSUP` otherwise
/// and before any argument check. This runtime has no literal areas, so no such
/// collector process exists and `notsup` is the faithful answer for every argument.
/// Oracle-verified for `(1,2,3)`, `([],[],[])` and `(a,b,c)`.
///
/// # Errors
///
/// Always `error:notsup`.
fn send_copy_request_3(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(notsup())
}

/// The `EXC_NOTSUP` raise (`erts/emulator/beam/erl_term.h`'s `am_notsup`): an
/// `error`-class raise whose reason is the bare atom `notsup`.
fn notsup() -> BeamError {
    raise_error(ExitReason::atom("notsup"))
}

/// Whether `term` is the atom `name`.
fn atom_is(ctx: &BifContext<'_>, term: Eterm, name: &str) -> bool {
    access::is_atom(term) && ctx.atoms().name(atom_val(term)) == Some(name)
}

/// The boolean `term` denotes, or `None` when it is neither the atom `true` nor the
/// atom `false` (the C's `BIF_ARG_2 == am_true || BIF_ARG_2 == am_false` test).
fn boolean_of(ctx: &BifContext<'_>, term: Eterm) -> Option<bool> {
    if atom_is(ctx, term, "true") {
        Some(true)
    } else if atom_is(ctx, term, "false") {
        Some(false)
    } else {
        None
    }
}

/// `erts_debug:unaligned_bitstring(Bitstring, Offset)`.
///
/// Ground truth: `../../erts/emulator/beam/beam_debug.c:100-167`
/// (`erts_debug_unaligned_bitstring_2`).
///
/// `Bitstring` must be a bitstring and `Offset` a **small** integer with
/// `0 < Offset < 8`; anything else is `badarg` — the C's `is_not_bitstring` /
/// `is_not_small` / `!(0 < offset && offset < 8)` guards, checked in that order.
/// Oracle-verified: `unaligned_bitstring(<<1,2,3>>, 0)` / `(_, 8)` / `(_, 9)` /
/// `(_, -1)` / `(_, 1.0)` / `(_, foo)` / `(_, 1 bsl 60)` ⇒ `badarg`, and
/// `unaligned_bitstring(foo, 1)` / `([1,2], 1)` ⇒ `badarg`.
///
/// The result carries **exactly the source's bits**: the C copies `source_size` bits
/// from the source into a fresh buffer at bit `offset` and then takes a sub-bit view
/// **starting at that same `offset`** (`erl_sub_bits_init(sb, …, base, offset,
/// source_size)`), so the `offset` leading bits it left uninitialised are never
/// observable — directly, or through the sub-bit view's own size. Hence
/// `unaligned_bitstring(<<1,2,3>>, 1) =:= <<1,2,3>>` and
/// `unaligned_bitstring(<<1:3>>, 1) =:= <<1:3>>` (both oracle-verified); an empty
/// source is **not** an error — `unaligned_bitstring(<<>>, 1)` ⇒ `<<>>`, because
/// `<<>>` *is* a bitstring (`is_not_bitstring` is false for it).
///
/// # Representation
///
/// What the C returns is a `refc` bitstring: a heap `ErlSubBits` whose bit pointer
/// sits `offset` bits **below** the byte start of a freshly minted off-heap refc
/// binary — i.e. a deliberately **unaligned** internal representation, which is the
/// whole point of the BIF (it exists to exercise consumers of an unaligned bit
/// pointer). When a host installs the [`crate::OffHeapBinaries`] seam the same shape
/// is built here ([`access::make_refc_window`]): the fresh payload is
/// `MAX(source_size + offset, ERL_ONHEAP_BITS_LIMIT + 1)` bits wide (`beam_debug.c`'s
/// `target_size`), the source's bits sit at bit `offset`, and the `ErlSubBits` window
/// starts at that same `offset` — so `binary:referenced_byte_size/1` reports the C's
/// oversized payload (65 for `unaligned_bitstring(<<1,2,3>>, 1)`, 257 for the
/// 256-byte source below), and a `binary:part/2,3` above 512 bits windows that same
/// payload. With **no** seam installed the on-heap `ErlHeapBits` shape is kept: the
/// **bit pattern and shape agree** (same bit size, same bits, the result `=:=` the
/// source) while only the interior representation (and hence
/// `referenced_byte_size/1`) differs.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-bitstring source
/// or a non-small / out-of-range offset; otherwise propagates the heap-profile or
/// host-seam failure when the result bitstring does not fit the destination heap.
fn unaligned_bitstring(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let source = args[0];
    if !access::is_bitstring(source) {
        return Err(badarg());
    }
    let offset = args[1];
    if !access::is_small(offset) {
        return Err(badarg());
    }
    // C reads the raw word (`unsigned_val`), so a negative small int becomes a huge
    // unsigned one; comparing the signed value against the same open interval is
    // equivalent for every small int in it (`0 < offset < 8`).
    let offset = signed_value(offset);
    if !(0 < offset && offset < 8) {
        return Err(badarg());
    }
    // The result's bits are the source's bits, at the source's bit size. Reading them
    // bit by bit (`bitstring_bits`) accepts a non-byte-aligned source and never
    // follows a `BinRef`/match-context indirection.
    let bit_len = access::bitstring_bit_size(source)?;
    let bytes = access::bitstring_bits(source)?;
    if !ctx.has_off_heap() {
        return access::make_heap_bitstring(ctx.heap(), &bytes, bit_len);
    }
    let offset_bits = offset as usize;
    // `beam_debug.c:140-145`: the fresh payload is
    // `MAX(source_size + offset, ERL_ONHEAP_BITS_LIMIT + 1)` **bits** — always above
    // the on-heap limit, so `erts_new_bitstring` places it off-heap — and the source's
    // bits are copied into it at bit `offset`. The window over it starts at that same
    // `offset`, so the leading bits are deterministic (zero) and never observed; the
    // window's `orig_size` is the payload's byte width, which is what a later
    // `binary:part` + `binary:referenced_byte_size/1` report.
    let target_bits = bit_len
        .checked_add(offset_bits)
        .map(|n| n.max(beam_term::refc::ERL_ONHEAP_BITS_LIMIT + 1))
        .ok_or_else(system_limit)?;
    let mut payload = vec![0u8; beam_term::bitstring::bit_count_to_bytes(target_bits)];
    copy_bits(&mut payload, offset_bits, &bytes, bit_len);
    access::make_refc_window(ctx, &payload, offset_bits, bit_len)
}

/// Copies `size` bits from `src` (MSB-first, starting at bit 0) into `dst` at bit
/// `dst_bit` — the C's `copy_binary_to_buffer(dst, dst_bit, src, 0, size)`.
///
/// Bits are MSB-first within each byte (the frozen `beam_term` convention), so source
/// bit `i` is bit `7 - (i % 8)` of `src[i / 8]` and lands in destination bit
/// `dst_bit + i`. Destination bits outside the copied range are left as the caller
/// initialised them; the unaligned-bitstring producer zeroes the buffer, so the
/// `dst_bit` leading bits stay zero and are never observed by the window that starts
/// there.
fn copy_bits(dst: &mut [u8], dst_bit: usize, src: &[u8], size: usize) {
    debug_assert!(dst.len() >= (dst_bit + size).div_ceil(8));
    debug_assert!(src.len() >= size.div_ceil(8));
    for i in 0..size {
        if (src[i / 8] >> (7 - (i % 8))) & 1 != 0 {
            let j = dst_bit + i;
            dst[j / 8] |= 1 << (7 - (j % 8));
        }
    }
}

/// `erts_debug:map_info/1` — `hashmap_info/2`'s traversal statistics
/// (`erl_map.c:3464-3559`).
///
/// Three cases, decided by the argument exactly as the C does:
///
/// * a **hash map** (a HAMT head node) is walked node by node and summarised as
///   `[{depth, MaxDepth}, {arrays, N, []}, {collisions, N, {16 bins}},
///   {bitmaps, N, {16 bins}}, {leafs, N, {18 bins}}]`;
/// * a **flat map** (and *only* a flat map) ⇒ `error:badarg`
///   (`BIF_ERROR(BIF_P, BADARG)`, `erl_map.c:3304-3305`);
/// * anything else — including a HAMT *inner* node, which `is_hashmap` rejects but
///   `is_map` accepts ⇒ `error:{badmap, Term}`.
///
/// The bins are indexed exactly as the C's: `arrays` counts head nodes with all 16
/// slots present; `collisions` counts collision nodes by arity, every arity above 16
/// folded onto the last bin; `bitmaps` counts bitmap nodes by `popcount(bitmap)`, bin
/// `i` holding nodes with `i + 1` slots; `leafs` counts the leaf cons cells by the
/// node level they hang at, where the head node's own level is `0` (so a leaf
/// directly below the head lands in bin `1`). `depth` is the deepest level any *node*
/// was reached at — `1` for a head whose children are all leaves.
///
/// Oracle-pinned (OTP 29.1.1 / erts-17.1; see the module tests):
/// `map_info(maps:from_list([{N, N} || N <- lists:seq(1, 40)]))` ⇒
/// `[{depth,4}, {arrays,0,[]}, {collisions,0,{0,…}}, {bitmaps,14,{1,5,3,2,1,1,0,…,1,0,0}},
/// {leafs,40,{0,4,32,2,2,0,…}}]`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a flat map, and the
/// `{badmap, Term}` structured raise for a non-map — both the C's `BIF_ERROR`s.
fn map_info_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    // The C's test order and classification: `is_hashmap` first (a HAMT head), then
    // `is_flatmap` (which is what raises the plain `badarg`), else `badmap`.
    if !access::is_hashmap(term) {
        if access::is_flatmap(term) {
            return Err(badarg());
        }
        return Err(structured_error(ctx, "badmap", &[term])?);
    }
    let info = walk_hashmap(term)?;

    let leafs = int_bins(ctx, &info.leafs)?;
    let bitmaps = int_bins(ctx, &info.bitmaps)?;
    let collisions = int_bins(ctx, &info.collisions)?;

    // The C builds the result back to front, so the list reads
    // `[depth, arrays, collisions, bitmaps, leafs]`.
    let leafs_atom = ctx.intern_atom("leafs")?;
    let value = arith::int_from_i64(ctx, info.nleaf as i64)?;
    let entry = ctx.tuple(&[leafs_atom, value, leafs])?;
    let mut result = ctx.cons(entry, NIL)?;

    let bitmaps_atom = ctx.intern_atom("bitmaps")?;
    let value = arith::int_from_i64(ctx, info.nbitmap as i64)?;
    let entry = ctx.tuple(&[bitmaps_atom, value, bitmaps])?;
    result = ctx.cons(entry, result)?;

    let collisions_atom = ctx.intern_atom("collisions")?;
    let value = arith::int_from_i64(ctx, info.ncollision as i64)?;
    let entry = ctx.tuple(&[collisions_atom, value, collisions])?;
    result = ctx.cons(entry, result)?;

    let arrays_atom = ctx.intern_atom("arrays")?;
    let value = arith::int_from_i64(ctx, info.narray as i64)?;
    let entry = ctx.tuple(&[arrays_atom, value, NIL])?;
    result = ctx.cons(entry, result)?;

    let depth_atom = ctx.intern_atom("depth")?;
    let value = arith::int_from_i64(ctx, info.depth as i64)?;
    let entry = ctx.tuple(&[depth_atom, value])?;
    result = ctx.cons(entry, result)?;
    Ok(result)
}

/// Builds the tuple of the small/bignum integers `values` — the C's
/// `hashmap_bld_tuple_uint` (`erl_map.c:3560-3572`), which materialises each count
/// through `erts_bld_uint` (a small int when it fits, else a bignum).
fn int_bins(ctx: &mut BifContext<'_>, values: &[u64]) -> Result<Eterm> {
    let mut terms = Vec::with_capacity(values.len());
    for &value in values {
        terms.push(arith::int_from_i64(ctx, value as i64)?);
    }
    ctx.tuple(&terms)
}

/// The `hashmap_info/2` traversal accumulators (`erl_map.c:3475-3483`): the four
/// per-class node counts and their bin histograms.
#[derive(Default)]
struct HashMapInfo {
    /// `leaf_usage` — `HAMT_MAX_LEVEL + 2` bins, one per level a leaf hangs at.
    leafs: [u64; beam_term::HAMT_MAX_LEVEL + 2],
    /// `bitmap_usage` — 16 bins, `bitmaps[i]` = bitmap nodes with `i + 1` slots.
    bitmaps: [u64; beam_term::HAMT_MAX_LEVEL],
    /// `collision_usage` — 16 bins, `collisions[i]` = collision nodes of arity
    /// `i + 1`, every larger arity folded onto the last bin.
    collisions: [u64; beam_term::HAMT_MAX_LEVEL],
    /// `nleaf` — the number of leaf cons cells reached.
    nleaf: u64,
    /// `nbitmap` — the number of bitmap nodes (head or inner).
    nbitmap: u64,
    /// `narray` — the number of head nodes with all 16 slots present.
    narray: u64,
    /// `ncollision` — the number of collision nodes.
    ncollision: u64,
    /// `max_depth` — the deepest level a node was reached at.
    depth: u64,
}

/// Walks the HAMT rooted at `map` (a hash-map head node) and accumulates the
/// `hashmap_info/2` statistics (`erl_map.c:3486-3536`).
///
/// The C's explicit `ESTACK` of `(node, level)` pairs is reproduced as a `Vec`; its
/// `clvl` is the level a node sits at, the head at `0`, and every node's children are
/// visited one level deeper.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a slot that is neither a
/// leaf nor a well-formed node, and for a level outside the frozen
/// `HAMT_MAX_LEVEL + 2` leaf bins (a map this runtime cannot produce).
fn walk_hashmap(map: Eterm) -> Result<HashMapInfo> {
    let mut info = HashMapInfo::default();
    let mut stack: Vec<(usize, Eterm)> = vec![(0, map)];
    while let Some((level, node)) = stack.pop() {
        // `TAG_PRIMARY_LIST`: a leaf is a two-word `[Key, Value]` cons cell.
        if access::is_list(node) {
            info.nleaf += 1;
            let bin = info.leafs.get_mut(level).ok_or(BeamError::BadArg)?;
            *bin += 1;
            continue;
        }
        let view = access::hamt_node(node)?;
        let slots = view.slots.len();
        match view.kind {
            access::HamtNode::HeadArray => info.narray += 1,
            access::HamtNode::HeadBitmap | access::HamtNode::InnerBitmap => {
                info.nbitmap += 1;
                let bin = info.bitmaps.get_mut(slots - 1).ok_or(BeamError::BadArg)?;
                *bin += 1;
            }
            access::HamtNode::Collision => {
                info.ncollision += 1;
                let bin = slots.min(beam_term::HAMT_MAX_LEVEL) - 1;
                let bin = info.collisions.get_mut(bin).ok_or(BeamError::BadArg)?;
                *bin += 1;
            }
        }
        let child_level = level + 1;
        info.depth = info.depth.max(child_level as u64);
        for slot in view.slots {
            stack.push((child_level, slot));
        }
    }
    Ok(info)
}

/// `ERTS_MAX_INTERNAL_ATOM_CACHE_ENTRIES` (`../../erts/emulator/beam/external.h:120`):
/// the atom-cache slot count `dist_ext_to_term/2` fills from its first argument.
///
/// It is 255 — one fewer than the 256 a one-byte index can address — so the C's
/// `if (arity > ERTS_MAX_INTERNAL_ATOM_CACHE_ENTRIES) goto badarg`
/// (`external.c:1362-1363`) refuses a 256-element tuple. Every index the BIF
/// accepts is therefore `< 255` and its `u8` slot cast is exact.
const MAX_INTERNAL_ATOM_CACHE_ENTRIES: usize = 255;

/// `erts_debug:instructions/0` — the instruction-dispatch table's names.
///
/// Ground truth: `../../erts/emulator/beam/beam_debug.c:376-393`
/// (`erts_debug_instructions_0`), which builds
/// `[opc[i].name || i <- 0..num_instructions-1]`: **every** row of the
/// `beam_makeops`-generated `opc[]` table, in dispatch order, each name a charlist
/// of its bytes.
///
/// Those names carry `beam_makeops`' operand-type suffixes
/// (`allocate_heap_tIt`, `append_cons_Is`, …), so they are **not** `genop.tab`'s
/// opcode names and cannot be derived from the loader's roster. The table is
/// therefore generated data: [`super::instructions::INSTRUCTIONS`], emitted by the
/// committed `gen_instructions.escript` from this tree's own generator (see that
/// file's header for the why and the how).
///
/// Oracle-verified: the OTP 29.1.x oracles answer **245** entries with the same first
/// four `["aligned_label_Lt", "allocate_tt", "allocate_heap_tIt", "append_cons_Is"]` and
/// the same last `"wait_unlocked_f"`. This tree is OTP 30 and its `ops.tab` carries the
/// native-record matching feature, so its table holds **248**; the difference is a
/// feature/fix backport split, not a divergence, and is calibrated per oracle release by
/// the `COHORTS` table in `the_generated_roster_matches_a_live_oracle_modulo_the_release_skew`.
///
/// # Errors
///
/// Propagates a heap-allocation failure.
fn instructions_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let mut names = Vec::with_capacity(super::instructions::INSTRUCTIONS.len());
    for name in super::instructions::INSTRUCTIONS {
        names.push(ctx.charlist(name)?);
    }
    ctx.list(&names)
}

/// `erts_debug:dist_ext_to_term/2` — decode an external-term image the way the
/// **distribution** receiver does, with a caller-supplied atom cache.
///
/// Ground truth: `../../erts/emulator/beam/external.c:1340-1405`
/// (`erts_debug_dist_ext_to_term_2`) → `erts_decode_dist_ext` (`:1273-1311`).
/// The argument order of the C's guards is reproduced exactly, because every
/// failure is `badarg` and only the *first* failing guard is observable:
///
/// 1. `is_not_tuple(arg1)` ⇒ `badarg` (`:1358-1359`);
/// 2. `arity > ERTS_MAX_INTERNAL_ATOM_CACHE_ENTRIES` (255) ⇒ `badarg`
///    (`:1362-1363`);
/// 3. a non-atom element ⇒ `badarg` (`:1366-1370`) — the tuple fills atom-cache
///    slots `0..arity-1` in order;
/// 4. `is_not_bitstring(arg2)` or a non-byte-aligned or **empty** bitstring ⇒
///    `badarg` (`:1372-1380`);
/// 5. a first byte that is not `VERSION_MAGIC` (131) ⇒ `badarg` (`:1290-1295`);
/// 6. a truncated or unrecognized image ⇒ `badarg` (`:1296-1298`).
///
/// Trailing bytes after the term are ignored (the C decodes one term and returns
/// it), and an `ATOM_CACHE_REF` (`82`) whose slot the caller did not fill is the
/// C's `attab` bound failure ⇒ `badarg`. The term is materialized on the context
/// heap by the shared `beam_codec` decoder — the same one `binary_to_term` uses,
/// installed with `beam_codec::AtomCache` — never a second decoder.
///
/// Oracle-verified (OTP 29.1.1 / erts-17.1); the matrix is pinned by
/// `dist_ext_to_term_matches_the_oracle_matrix` below.
///
/// # Errors
///
/// Always [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a rejected
/// argument or image; otherwise propagates a heap-allocation failure.
fn dist_ext_to_term_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let slots = args[0];
    if !access::is_tuple(slots) {
        return Err(badarg());
    }
    let arity = access::tuple_size(slots);
    if arity > MAX_INTERNAL_ATOM_CACHE_ENTRIES {
        return Err(badarg());
    }
    let mut cache = beam_codec::AtomCache::new();
    for index in 0..arity {
        let atom = access::tuple_element(slots, index);
        if !access::is_atom(atom) {
            return Err(badarg());
        }
        // `index < arity <= 255`, so the one-byte on-wire slot index is exact.
        cache.set(index as u8, atom_val(atom));
    }
    // A non-bitstring and a non-byte-aligned bitstring are both `badarg` here;
    // the empty image is the C's `size == 0` guard.
    let data = crate::bits::bytes(args[1])?;
    if data.is_empty() {
        return Err(badarg());
    }
    crate::binconv::decode_dist_ext_image(ctx, &data, &cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::{BeamError, ExitReason};
    use beam_process::ProcessId;
    use beam_term::prelude::make_small;
    use beam_term::{make_atom, AtomTable, NIL};

    /// The whole oracle probe this module's expectations were taken from
    /// (OTP 29.1.1 / erts-17.1, `erl` on `PATH`):
    ///
    /// ```text
    /// erl -noshell -eval '
    ///   R = (try EXPR catch C:E -> {C,E} end), io:format("~p~n",[R]), halt(0).'
    /// ```
    ///
    /// with `EXPR` and the pinned output:
    ///
    /// ```text
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,1)               => <<1,2,3>>
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,2)               => <<1,2,3>>
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,7)               => <<1,2,3>>
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,0)               => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,8)               => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,9)               => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,-1)              => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,1.0)             => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,foo)             => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,1 bsl 60)        => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,<<>>)            => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,<<"x">>)         => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<>>,1)                    => <<>>
    /// erts_debug:unaligned_bitstring(<<>>,7)                    => <<>>
    /// erts_debug:unaligned_bitstring(foo,1)                     => {error,badarg}
    /// erts_debug:unaligned_bitstring([1,2],1)                   => {error,badarg}
    /// erts_debug:unaligned_bitstring(1,1)                       => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1:3>>,1)                 => <<1:3>>
    /// erts_debug:unaligned_bitstring(<<17:5>>,3)                => <<17:5>>
    /// erts_debug:unaligned_bitstring(<<255:12>>,4)              => <<15,15:4>>
    /// bit_size(erts_debug:unaligned_bitstring(<<1,2,3>>,1))     => 24
    /// bit_size(erts_debug:unaligned_bitstring(<<1:3>>,1))       => 3
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,1) =:= <<1,2,3>> => true
    /// erts_debug:unaligned_bitstring(<<1:3>>,1) =:= <<1:3>>     => true
    /// binary:referenced_byte_size(…(<<1,2,3>>,1))               => 65
    /// erts_debug:size_shared(<<1,2,3>>)                         => 0
    /// ```
    ///
    /// Note `<<>>` is **not** `badarg` (it *is* a bitstring); the `badarg` sources are
    /// `foo`, `[1,2]` and `1`.

    /// Calls `erts_debug:unaligned_bitstring(Source, Offset)` on its own heap and
    /// returns the observable result shape `(bit_size, bits)`.
    ///
    /// The result term points into the call's heap, so it is read **inside** that
    /// heap's scope and only the copied `(bit_size, bits)` escapes.
    fn probe(source: Eterm, offset: Eterm) -> Result<(usize, Vec<u8>)> {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = AtomTable::new();
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let out = reg.call("erts_debug", "unaligned_bitstring", 2, &mut ctx, &[source, offset])?;
        Ok((access::bitstring_bit_size(out)?, access::bitstring_bits(out)?))
    }

    /// Builds the on-heap bitstring `<<bytes:bits>>` for a test source.
    fn bitstring(heap: &mut Heap, bytes: &[u8], bits: usize) -> Eterm {
        access::make_heap_bitstring(heap, bytes, bits).expect("the source fits")
    }

    /// The pinned `{error, badarg}` cases.
    #[track_caller]
    fn assert_badarg(result: Result<(usize, Vec<u8>)>, label: impl std::fmt::Debug) {
        match result {
            Err(BeamError::BadArg) => {}
            other => panic!("{label:?}: expected BadArg, got {other:?}"),
        }
    }

    /// A module's real BIF surface is exactly [`REAL_BIFS`], and every one of them is
    /// actually registered by [`register`].
    #[test]
    fn real_bifs_is_the_sixteen_real_keys_and_is_registered() {
        assert_eq!(
            REAL_BIFS,
            &[
                ("erts_debug", "copy_shared", 2),
                ("erts_debug", "disassemble", 1),
                ("erts_debug", "dist_ext_to_term", 2),
                ("erts_debug", "get_internal_state", 1),
                ("erts_debug", "instructions", 0),
                ("erts_debug", "interpreter_size", 0),
                ("erts_debug", "lcnt_clear", 0),
                ("erts_debug", "lcnt_collect", 0),
                ("erts_debug", "lcnt_control", 1),
                ("erts_debug", "lcnt_control", 2),
                ("erts_debug", "map_info", 1),
                ("erts_debug", "set_internal_state", 2),
                ("erts_debug", "size_shared", 1),
                ("erts_debug", "unaligned_bitstring", 2),
                ("erts_literal_area_collector", "send_copy_request", 3),
                ("erts_trace_cleaner", "send_trace_clean_signal", 1),
            ]
        );
        let mut sorted = REAL_BIFS.to_vec();
        sorted.sort_unstable();
        assert!(sorted.iter().eq(REAL_BIFS.iter()), "REAL_BIFS is not sorted");
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        for &(module, function, arity) in REAL_BIFS {
            assert!(
                reg.contains(module, function, arity),
                "{module}:{function}/{arity} must be registered by this module"
            );
            // …and none of them reads the `RuntimeHooks` seam any more.
            assert!(
                !crate::hook_sets::needs_runtime_hooks(module, function, arity),
                "{module}:{function}/{arity} is a real BIF but still sits in the seam roster"
            );
        }
        // 16 real keys + the 4 remaining `runtime_bif!` seam keys.
        assert_eq!(reg.len(), 20, "the erts_debug/collector key count");
    }    /// Every valid call returns the source's own bits at the source's own bit size —
    /// the C's sub-bit view starts at the very `offset` the bits were copied to, so
    /// the `offset` leading bits are unobservable (oracle: `=:= the source`).
    #[test]
    fn unaligned_bitstring_reproduces_the_source_bits() {
        let mut heap = Heap::new(256).expect("test heap");
        // (source bytes, source bit size, in-range offset)
        let cases: [(Vec<u8>, usize, i64); 9] = [
            (vec![1, 2, 3], 24, 1),
            (vec![1, 2, 3], 24, 2),
            (vec![1, 2, 3], 24, 7),
            (vec![0x20], 3, 1),        // <<1:3>>
            (vec![0x88], 5, 3),        // <<17:5>>
            (vec![0x0F, 0xF0], 12, 4), // <<255:12>> (prints <<15,15:4>>)
            (vec![0xFF, 0xFF], 16, 5),
            (vec![], 0, 1), // <<>> is a bitstring, not an error
            (vec![], 0, 7),
        ];
        for (bytes, bits, offset) in cases {
            let source = bitstring(&mut heap, &bytes, bits);
            let (got_bits, got_bytes) = probe(source, make_small(offset))
                .unwrap_or_else(|e| panic!("({bytes:?}/{bits}, {offset}) errored: {e:?}"));
            assert_eq!(got_bits, bits, "bit size for {bytes:?}/{bits} at offset {offset}");
            assert_eq!(got_bytes, bytes, "bits for {bytes:?}/{bits} at offset {offset}");
        }
    }

    /// Offsets outside the open interval `(0, 8)` are `badarg` (the C's
    /// `!(0 < offset && offset < 8)` guard; `0` and `8` are both rejected).
    #[test]
    fn unaligned_bitstring_rejects_offsets_outside_one_to_seven() {
        let mut heap = Heap::new(256).expect("test heap");
        let source = bitstring(&mut heap, &[1, 2, 3], 24);
        for offset in [0i64, 8, 9, -1, 1000, -(1 << 58)] {
            assert_badarg(probe(source, make_small(offset)), offset);
        }
    }

    /// A non-small offset (a float, a bignum, an atom) is `badarg` — the C checks
    /// `is_not_small` before it looks at the value.
    #[test]
    fn unaligned_bitstring_rejects_a_non_small_offset() {
        let mut heap = Heap::new(256).expect("test heap");
        let source = bitstring(&mut heap, &[1, 2, 3], 24);
        let float = access::make_float(&mut heap, 1.0).expect("the float fits");
        // 2^60 > the 60-bit small-integer range.
        let big = beam_codec::term::make_big(&mut heap, false, &[1 << 60]).expect("the bignum fits");
        let mut atoms = AtomTable::new();
        atoms.seed(["foo"]);
        let atom = make_atom(atoms.lookup("foo").expect("seeded"));
        assert_badarg(probe(source, float), "float offset");
        assert_badarg(probe(source, big), "bignum offset");
        assert_badarg(probe(source, atom), "atom offset");
    }

    /// A non-bitstring source is `badarg` (the C's `is_not_bitstring` guard runs
    /// before the offset is looked at).
    #[test]
    fn unaligned_bitstring_rejects_a_non_bitstring_source() {
        let mut heap = Heap::new(256).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["foo"]);
        let atom = make_atom(atoms.lookup("foo").expect("seeded"));
        let list = heap.cons(make_small(1), NIL).expect("the cons fits");
        let tuple = heap.tuple(&[make_small(1)]).expect("the tuple fits");
        for (label, source) in [
            ("atom", atom),
            ("integer", make_small(7)),
            ("nil", NIL),
            ("list", list),
            ("tuple", tuple),
        ] {
            assert_badarg(probe(source, make_small(1)), label);
        }
    }

    /// The atom table every probe in this section runs over: the constants a key answers
    /// with, the gate's verbs, and the argument atoms the probes use.
    fn probe_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "false",
            "true",
            "error",
            "undef",
            "notsup",
            "badarg",
            "unsupported_bif",
            "memory",
            "available_internal_state",
            "reds_left",
            "clear",
            "lists",
            "a",
            "b",
            "c",
            "foo",
            "atom",
            // `set_internal_state/2`'s three loop-limit arms and the conversion the
            // "no result depends on a limit" probe drives.
            "binary_loop_limit",
            "re_loop_limit",
            "unicode_loop_limit",
            "default",
            "utf8",
            "x",
            // `map_info/1`'s tags.
            "depth",
            "arrays",
            "collisions",
            "bitmaps",
            "leafs",
            "badmap",
        ]);
        atoms
    }

    /// One call of `module:function/arity` over a fresh heap and [`probe_atoms`], with
    /// the outcome rendered as a stable label so it can outlive the call's heap.
    ///
    /// The labels are the oracle shapes: a value prints `atom(false)` / `int(0)`, an
    /// ordinary argument failure `badarg`, and a raise `error:<reason atom>` — i.e. the
    /// `{Class,Reason}` the oracle's `try … catch C:R -> {C,R} end` probe prints.
    fn observe_args(
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> String {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        let mut heap = Heap::new(1 << 12).expect("test heap");
        let atoms = probe_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        match reg.call(module, function, arity, &mut ctx, args) {
            Ok(term) => {
                if access::is_atom(term) {
                    let name = atoms.name(atom_val(term)).unwrap_or("?").to_string();
                    format!("atom({name})")
                } else if access::is_small(term) {
                    format!("int({})", signed_value(term))
                } else {
                    format!("term({:#x})", term.raw())
                }
            }
            Err(BeamError::BadArg) => "badarg".to_string(),
            Err(BeamError::Raise(raise)) => match raise.reason.as_atom() {
                Some(name) => format!("error:{name}"),
                None => format!("raise({:?})", raise.reason),
            },
            Err(other) => format!("err({other:?})"),
        }
    }

    /// [`observe_args`] for an `erts_debug` key.
    fn observe(function: &str, arity: u32, args: &[Eterm]) -> String {
        observe_args("erts_debug", function, arity, args)
    }

    /// The atom `name` as a term over [`probe_atoms`].
    fn atom_of(name: &str) -> Eterm {
        make_atom(probe_atoms().lookup(name).expect("seeded"))
    }

    /// The two-element list `[X, Y]` on `heap`.
    fn list2(heap: &mut Heap, x: Eterm, y: Eterm) -> Eterm {
        let tail = heap.cons(y, NIL).expect("the tail fits");
        heap.cons(x, tail).expect("the cons fits")
    }

    /// `interpreter_size/0`, `disassemble/1` and the `lcnt` constants are the answers a
    /// build without the interpreter/lock-count facilities gives. Oracle-verified
    /// (OTP 29.1.1 / erts-17.1): `0`, `false`, `error`, and `badarg` for *every*
    /// `lcnt_control` argument.
    #[test]
    fn the_constant_bundle_matches_the_oracle() {
        assert_eq!(observe("interpreter_size", 0, &[]), "int(0)");

        let mfa = {
            let mut heap = Heap::new(64).expect("test heap");
            let atoms = probe_atoms();
            let module = make_atom(atoms.lookup("lists").expect("seeded"));
            let function = make_atom(atoms.lookup("a").expect("seeded"));
            heap.tuple(&[module, function, make_small(2)])
                .expect("the tuple fits")
        };
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = probe_atoms();
        let lists = make_atom(atoms.lookup("lists").expect("seeded"));
        let nomod = make_atom(atoms.lookup("foo").expect("seeded"));
        let a = make_atom(atoms.lookup("a").expect("seeded"));
        let b = make_atom(atoms.lookup("b").expect("seeded"));
        let list = list2(&mut heap, a, b);
        for (label, arg) in [
            ("{lists,a,2}", mfa),
            ("lists", lists),
            ("an unloaded atom", nomod),
            ("42", make_small(42)),
            ("[a,b]", list),
        ] {
            assert_eq!(
                observe("disassemble", 1, &[arg]),
                "atom(false)",
                "disassemble({label})"
            );
        }

        assert_eq!(observe("lcnt_clear", 0, &[]), "atom(error)");
        assert_eq!(observe("lcnt_collect", 0, &[]), "atom(error)");

        let tuple_clear = heap.tuple(&[atom_of("clear"), NIL]).expect("the tuple fits");
        let single = heap.cons(atom_of("a"), NIL).expect("the cons fits");
        for (label, arg) in [
            ("clear", atom_of("clear")),
            ("{clear,[]}", tuple_clear),
            ("[a]", single),
            ("42", make_small(42)),
        ] {
            assert_eq!(observe("lcnt_control", 1, &[arg]), "badarg", "lcnt_control({label})");
        }
        for (label, args) in [
            ("(clear,[])", vec![atom_of("clear"), NIL]),
            ("(a,b)", vec![atom_of("a"), atom_of("b")]),
            ("(42,43)", vec![make_small(42), make_small(43)]),
        ] {
            assert_eq!(observe("lcnt_control", 2, &args), "badarg", "lcnt_control{label}");
        }
    }

    /// `copy_shared/2` inspects its **flag** first (a non-boolean is `badarg` whatever the
    /// term is) and then returns the term — this runtime has no literal areas, so the
    /// term already *is* the copy (see the BIF's docs). Oracle: `copy_shared(foo, atom |
    /// 1 | [])` ⇒ `{error,badarg}`; `copy_shared([a,b], true | false)` ⇒ `[a,b]`;
    /// `copy_shared(a)` ⇒ `a`.
    #[test]
    fn copy_shared_checks_its_flag_first_and_returns_the_term() {
        let foo = atom_of("foo");
        for flag in [atom_of("atom"), make_small(1), NIL] {
            assert_eq!(observe("copy_shared", 2, &[foo, flag]), "badarg");
        }
        let mut heap = Heap::new(8).expect("test heap");
        let a = atom_of("a");
        let b = atom_of("b");
        let list = list2(&mut heap, a, b);
        for flag in [atom_of("true"), atom_of("false")] {
            assert_eq!(observe("copy_shared", 2, &[list, flag]), format!("term({:#x})", list.raw()));
        }
        // An immediate comes back as itself.
        assert_eq!(observe("copy_shared", 2, &[a, atom_of("true")]), "atom(a)");
    }

    /// The two collector keys are the `notsup` protocol stubs: the C restricts them to
    /// the emulator's own processes and raises `EXC_NOTSUP` before any argument check, so
    /// every argument shape answers `error:notsup`. Oracle-verified for `1`, `[]`,
    /// `self()` and `a` (cleaner) and `(1,2,3)`, `([],[],[])`, `(a,b,c)` (literal-area
    /// collector).
    #[test]
    fn the_collector_protocol_stubs_answer_error_notsup() {
        let mut heap = Heap::new(8).expect("test heap");
        let pid = ProcessId::new(1, 0).to_eterm();
        let single = heap.cons(atom_of("a"), NIL).expect("the cons fits");
        for (label, arg) in [
            ("1", make_small(1)),
            ("[]", NIL),
            ("self()", pid),
            ("a", atom_of("a")),
        ] {
            assert_eq!(
                observe_args("erts_trace_cleaner", "send_trace_clean_signal", 1, &[arg]),
                "error:notsup",
                "send_trace_clean_signal({label})"
            );
        }
        for (label, args) in [
            ("(1,2,3)", vec![make_small(1), make_small(2), make_small(3)]),
            ("([],[],[])", vec![NIL, NIL, NIL]),
            ("(a,b,c)", vec![atom_of("a"), atom_of("b"), atom_of("c")]),
            ("([a],a,a)", vec![single, atom_of("a"), atom_of("a")]),
        ] {
            assert_eq!(
                observe_args("erts_literal_area_collector", "send_copy_request", 3, &args),
                "error:notsup",
                "send_copy_request{label}"
            );
        }
    }

    /// The `available_internal_state` gate is a **process-global** static, so every test
    /// that touches it serialises on this lock and restores it through [`GateOff`] — a
    /// leaked "on" gate would silently turn the *other* tests' `undef` answers into the
    /// typed gap.
    static GATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Restores the gate to off when dropped, panicking test or not.
    struct GateOff;

    impl Drop for GateOff {
        fn drop(&mut self) {
            AVAILABLE_INTERNAL_STATE.store(false, Ordering::SeqCst);
        }
    }

    /// The gate matrix, oracle-verified (OTP 29.1.1 / erts-17.1):
    ///
    /// ```text
    /// get_internal_state(memory)                            => {error,undef}   (gate off)
    /// set_internal_state(available_internal_state,true)     => false           (first enable)
    /// set_internal_state(available_internal_state,true)     => true            (already on)
    /// set_internal_state(available_internal_state,false)    => true            (disable)
    /// set_internal_state(available_internal_state,42)       => {error,undef}
    /// get_internal_state(memory)                            => the real value on the
    ///                                                          oracle; the typed
    ///                                                          `unsupported_bif` gap
    ///                                                          here (gate on)
    /// set_internal_state(reds_left,1)                       => likewise (gate on)
    /// ```
    ///
    /// The gate-on arm is a **typed capability gap, not a fabricated value**: `memory`
    /// would need the `MemoryHook` totals and `reds_left` the scheduler internals.
    #[test]
    fn the_internal_state_gate_matches_the_oracle() {
        let _lock = GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        AVAILABLE_INTERNAL_STATE.store(false, Ordering::SeqCst);
        let _restore = GateOff;

        let memory = atom_of("memory");
        let available = atom_of("available_internal_state");
        let true_ = atom_of("true");
        let false_ = atom_of("false");

        // Gate off: every argument raises the bare atom `undef`.
        assert_eq!(observe("get_internal_state", 1, &[memory]), "error:undef");
        assert_eq!(observe("get_internal_state", 1, &[make_small(0)]), "error:undef");

        // The one gate-free arm: it returns the PREVIOUS value.
        assert_eq!(
            observe("set_internal_state", 2, &[available, true_]),
            "atom(false)"
        );
        assert!(AVAILABLE_INTERNAL_STATE.load(Ordering::SeqCst));
        assert_eq!(
            observe("set_internal_state", 2, &[available, true_]),
            "atom(true)"
        );
        assert_eq!(
            observe("set_internal_state", 2, &[available, false_]),
            "atom(true)"
        );
        assert!(!AVAILABLE_INTERNAL_STATE.load(Ordering::SeqCst));

        // A non-boolean second argument fails the `am_true`/`am_false` test and therefore
        // falls through to the gate check ⇒ `undef` while the gate is off.
        assert_eq!(
            observe("set_internal_state", 2, &[available, make_small(42)]),
            "error:undef"
        );

        // Gate on: the emulator internals this runtime cannot serve are the typed gap.
        assert_eq!(
            observe("set_internal_state", 2, &[available, true_]),
            "atom(false)"
        );
        assert!(AVAILABLE_INTERNAL_STATE.load(Ordering::SeqCst));
        assert_eq!(
            observe("get_internal_state", 1, &[memory]),
            "error:unsupported_bif"
        );
        assert_eq!(
            observe("set_internal_state", 2, &[atom_of("reds_left"), make_small(1)]),
            "error:unsupported_bif"
        );
        // …and flipping the gate back off restores `undef`.
        assert_eq!(
            observe("set_internal_state", 2, &[available, false_]),
            "atom(true)"
        );
        assert_eq!(observe("get_internal_state", 1, &[memory]), "error:undef");
    }

    /// Restores the three process-global loop limits to their defaults when dropped —
    /// for the same reason [`GateOff`] does for the gate: the limits are per-VM state
    /// shared by every test in this binary, so a leaked value would make another test's
    /// "previous value" assertion wrong.
    struct LimitsDefault;

    impl Drop for LimitsDefault {
        fn drop(&mut self) {
            for limit in [LoopLimit::Binary, LoopLimit::Re, LoopLimit::Unicode] {
                limit.exchange(-1);
            }
        }
    }

    /// The three loop-limit arms of `set_internal_state/2` are the emulator's
    /// **trapping-granularity** knobs, and each is a faithful `xchg`: it answers the
    /// PREVIOUS value, the atom `default` and the integer `0` both restore the module
    /// default (and still answer the previous value), and a negative / non-integer /
    /// too-wide second argument is `badarg`.
    ///
    /// Oracle transcript — OTP 29.1.2 / erts-17.1.1, `erl` on `PATH`, a compiled module
    /// calling `erts_debug:set_internal_state/2` inside `try … catch C:R -> {C,R} end`
    /// (a bare BIF name is not auto-imported under `-eval`, so the rows below are one
    /// program in exactly this order):
    ///
    /// ```text
    /// (unicode_loop_limit,7) gate off      => {error,undef}
    /// (binary_loop_limit,default) gate off => {error,undef}
    /// (available_internal_state,true)      => {ok,false}
    /// (unicode_loop_limit,7)               => {ok,40000}
    /// (unicode_loop_limit,default)         => {ok,7}
    /// (unicode_loop_limit,0)               => {ok,40000}
    /// (re_loop_limit,5)                    => {ok,64000}
    /// (re_loop_limit,default)              => {ok,5}
    /// (binary_loop_limit,5)                => {ok,0}
    /// (binary_loop_limit,0)                => {ok,5}
    /// (binary_loop_limit,default)          => {ok,0}
    /// (unicode_loop_limit,-1)              => {error,badarg}
    /// (unicode_loop_limit,x)               => {error,badarg}
    /// (unicode_loop_limit,1.0)             => {error,badarg}
    /// (binary_loop_limit,1 bsl 64)         => {error,badarg}    (wider than one Uint)
    /// (binary_loop_limit,1 bsl 59)         => {ok,0}
    /// (binary_loop_limit,default)          => {ok,-576460752303423488}
    /// (re_loop_limit,1 bsl 63)             => {ok,64000}         (negative as a Sint)
    /// (re_loop_limit,default)              => {ok,64000}
    /// (available_internal_state,true)      => {ok,true}          (gate-free while on)
    /// (foo,default)                        => {error,badarg}     (not a limit name, gate on)
    /// ```
    ///
    /// The `2^59` row shows the C's term construction, not a divergence: `make_small` of
    /// a limit `>= 2^59` wraps the 60-bit immediate, so the oracle answers `-2^59` (see
    /// [`make_small_answer`]). The stacktrace item of every `badarg` row is
    /// `{erts_debug,set_internal_state,[binary_loop_limit,-1],[]}` — no `error_info`
    /// extension, i.e. the C's plain `BIF_ERROR(BIF_P, BADARG)`, which is exactly what
    /// [`badarg`] raises.
    #[test]
    fn the_loop_limit_arms_exchange_and_answer_the_previous_value() {
        let _lock = GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        AVAILABLE_INTERNAL_STATE.store(false, Ordering::SeqCst);
        let _gate = GateOff;
        let _limits = LimitsDefault;

        let binary = atom_of("binary_loop_limit");
        let re = atom_of("re_loop_limit");
        let unicode = atom_of("unicode_loop_limit");
        let default = atom_of("default");
        let available = atom_of("available_internal_state");
        let true_ = atom_of("true");

        // Gate off: the arms sit behind the gate, so every one raises the bare `undef`
        // (the C checks `available_internal_state` before the `else if` chain).
        assert_eq!(
            observe("set_internal_state", 2, &[unicode, make_small(7)]),
            "error:undef"
        );
        assert_eq!(
            observe("set_internal_state", 2, &[binary, default]),
            "error:undef"
        );

        // Enable, then exchange each limit three ways.
        assert_eq!(
            observe("set_internal_state", 2, &[available, true_]),
            "atom(false)"
        );
        assert_eq!(
            observe("set_internal_state", 2, &[unicode, make_small(7)]),
            "int(40000)"
        );
        assert_eq!(
            observe("set_internal_state", 2, &[unicode, default]),
            "int(7)"
        );
        assert_eq!(
            observe("set_internal_state", 2, &[unicode, make_small(0)]),
            "int(40000)"
        );
        assert_eq!(
            observe("set_internal_state", 2, &[re, make_small(5)]),
            "int(64000)"
        );
        assert_eq!(observe("set_internal_state", 2, &[re, default]), "int(5)");
        assert_eq!(
            observe("set_internal_state", 2, &[binary, make_small(5)]),
            "int(0)"
        );
        assert_eq!(
            observe("set_internal_state", 2, &[binary, make_small(0)]),
            "int(5)"
        );
        assert_eq!(
            observe("set_internal_state", 2, &[binary, default]),
            "int(0)"
        );

        // A second argument `term_to_Uint` rejects: negative, an atom, a float, and a
        // bignum wider than one `Uint` — all `badarg`, none `system_limit` and none a
        // silent default restore.
        let mut scratch = Heap::new(256).expect("test heap");
        let two_59 = beam_codec::term::make_big(&mut scratch, false, &[1 << 59])
            .expect("the bignum fits");
        let two_63 = beam_codec::term::make_big(&mut scratch, false, &[1 << 63])
            .expect("the bignum fits");
        let two_64 = beam_codec::term::make_big(&mut scratch, false, &[0, 1])
            .expect("the bignum fits");
        let one_point_zero = access::make_float(&mut scratch, 1.0).expect("the float fits");
        for (label, value) in [
            ("-1", make_small(-1)),
            ("x", atom_of("x")),
            ("1.0", one_point_zero),
            ("1 bsl 64", two_64),
        ] {
            assert_eq!(
                observe("set_internal_state", 2, &[unicode, value]),
                "badarg",
                "set_internal_state(unicode_loop_limit, {label})"
            );
        }

        // A limit that only a bignum can express: stored faithfully, answered through the
        // C's `make_small` round trip, and `2^63` (negative as a `Sint`) restores the
        // default instead of storing.
        assert_eq!(
            observe("set_internal_state", 2, &[binary, two_59]),
            "int(0)"
        );
        assert_eq!(
            observe("set_internal_state", 2, &[binary, default]),
            format!("int({})", -(1i64 << 59))
        );
        assert_eq!(
            observe("set_internal_state", 2, &[re, two_63]),
            "int(64000)"
        );
        assert_eq!(observe("set_internal_state", 2, &[re, default]), "int(64000)");

        // The gate-free arm keeps working while the gate is on, and a name that is not a
        // loop limit still answers the typed gap rather than `badarg`.
        assert_eq!(
            observe("set_internal_state", 2, &[available, true_]),
            "atom(true)"
        );
        assert_eq!(
            observe("set_internal_state", 2, &[atom_of("foo"), default]),
            "error:unsupported_bif"
        );
    }

    /// The bytes of the binary `module:function/arity` answers, over a fresh heap — the
    /// arguments are built **on that same heap** by `build`, so a charlist or binary
    /// argument is valid at the call site (unlike [`observe_args`], whose labels cannot
    /// compare two runs' results).
    fn binary_answer(
        module: &str,
        function: &str,
        arity: u32,
        build: impl FnOnce(&mut BifContext<'_>) -> Vec<Eterm>,
    ) -> Result<Vec<u8>> {
        let reg = crate::registry::default_registry();
        let mut heap = Heap::new(1 << 12).expect("test heap");
        let atoms = probe_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let args = build(&mut ctx);
        let out = reg.call(module, function, arity, &mut ctx, &args)?;
        access::binary_bytes(out)
    }

    /// The invariant the three limits exist under: they tune **trapping granularity
    /// only**, so no result depends on them. `unicode:characters_to_binary/2` and
    /// `binary:part/3` — both served in-crate — answer the identical bytes with every
    /// limit set to `1` (which forces a yield as early as possible), to `0` (unlimited,
    /// the `binary` default) and back to `default`.
    ///
    /// Oracle-verified reference results: `unicode:characters_to_binary([97,98,99],
    /// utf8)` ⇒ `<<"abc">>` and `binary:part(<<"abcdef">>, 1, 3)` ⇒ `<<"bcd">>`, with
    /// each of the three limits set to `1`, `0` and `default` in turn.
    #[test]
    fn the_limits_change_no_result() {
        let _lock = GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _limits = LimitsDefault;

        let characters = |ctx: &mut BifContext<'_>| {
            let units: Vec<Eterm> = [b'a', b'b', b'c']
                .iter()
                .map(|unit| ctx.int(i64::from(*unit)).expect("the unit fits"))
                .collect();
            let list = ctx.list(&units).expect("the charlist fits");
            vec![list, atom_of("utf8")]
        };
        let window = |ctx: &mut BifContext<'_>| {
            let subject = access::make_heap_bitstring(ctx.heap(), b"abcdef", 48)
                .expect("the source fits");
            vec![subject, make_small(1), make_small(3)]
        };

        // `1` = yield after a single iteration, `0` and `-1` (`default`) = the module
        // default. Every setting must answer byte-identical results.
        for setting in [1i64, 0, -1] {
            for limit in [LoopLimit::Binary, LoopLimit::Re, LoopLimit::Unicode] {
                limit.exchange(setting);
            }
            assert_eq!(
                binary_answer("unicode", "characters_to_binary", 2, characters)
                    .expect("the conversion runs"),
                b"abc".to_vec(),
                "unicode:characters_to_binary/2 with every loop limit at {setting}"
            );
            assert_eq!(
                binary_answer("binary", "part", 3, window).expect("the slice runs"),
                b"bcd".to_vec(),
                "binary:part/3 with every loop limit at {setting}"
            );
        }
    }

    /// `size_shared/1` prices a run-time-built graph once per object and shares by
    /// identity — `size/1`'s number, oracle-verified (see [`size_shared_1`]):
    ///
    /// ```text
    /// size_shared([L1,L2])       => 12   (two separately built [a,b] lists)
    /// size_shared([L1,L1])       =>  8   (the same list twice: the second counts 0)
    /// size_shared(L1)            =>  4
    /// size_shared(42|foo|[])     =>  0
    /// ```
    ///
    /// (`{}` answers `0` on the oracle only because its empty tuple is a **global
    /// literal**, which this runtime does not have — see [`size_shared_1`]'s literal-area
    /// note.)
    #[test]
    fn size_shared_prices_a_run_time_built_graph_like_the_oracle() {
        let mut heap = Heap::new(1 << 12).expect("test heap");
        let a = atom_of("a");
        let b = atom_of("b");
        let l1 = list2(&mut heap, a, b);
        let l2 = list2(&mut heap, a, b);
        let distinct = list2(&mut heap, l1, l2);
        assert_eq!(observe("size_shared", 1, &[l1]), "int(4)");
        assert_eq!(observe("size_shared", 1, &[distinct]), "int(12)");
        let shared = list2(&mut heap, l1, l1);
        assert_eq!(observe("size_shared", 1, &[shared]), "int(8)");
        // An immediate owns no heap words.
        assert_eq!(observe("size_shared", 1, &[make_small(42)]), "int(0)");
        assert_eq!(observe("size_shared", 1, &[a]), "int(0)");
        assert_eq!(observe("size_shared", 1, &[NIL]), "int(0)");
    }

    /// A map above `MAP_SMALL_MAP_LIMIT` is the one shape whose `size_shared/1` is **not**
    /// its `size/1`: the oracle charges a HAMT node's own words twice. Oracle-verified
    /// (OTP 29.1.1 / erts-17.1) on `maps:from_list([{I,I} || I <- lists:seq(1,N)])`:
    ///
    /// ```text
    ///  index range  size_shared   size/flat_size
    ///  1..33               180              123
    ///  1..34               184              126
    ///  1..40               216              148
    ///  1..100              528              364
    /// ```
    ///
    /// and `size_shared([M33,M33])` is `184` (`180` + the two cons cells). The flat-size
    /// assertions keep the two walks tied to the same map layout.
    #[test]
    fn size_shared_prices_a_hash_map_node_twice_like_the_oracle() {
        let mut heap = Heap::new(1 << 18).expect("test heap");
        // Integer keys only, so the table names no key: any namespace gives the same
        // trie and the same stored order here.
        let atoms = AtomTable::new();
        for (range, flat, shared) in [
            (1..=33i64, 123i64, 180i64),
            (1..=34, 126, 184),
            (1..=40, 148, 216),
            (1..=100, 364, 528),
        ] {
            let map = int_map(&mut heap, &atoms, range.clone());
            assert_eq!(
                beam_term::size::size_object(map) as i64,
                flat,
                "{range:?}: the flat size must match the oracle's erts_debug:size/1"
            );
            assert_eq!(
                observe("size_shared", 1, &[map]),
                format!("int({shared})"),
                "{range:?}"
            );
            let pair = list2(&mut heap, map, map);
            assert_eq!(
                observe("size_shared", 1, &[pair]),
                format!("int({})", shared + 4),
                "{range:?} behind two cons cells"
            );
        }
    }

    /// `size_shared/1`'s pricing state is **call-scoped**: [`SharedOnce`] is constructed inside
    /// [`size_shared_1`] for each call, so the same term prices identically every time. A `seen`
    /// set that outlived the call (a `thread_local!`/`static`, a plausible "optimisation") would
    /// remember the first call's objects and price the repeat as `0`.
    ///
    /// The term is `[L, L, L]` over **one** shared `[1, 2]` subterm, so the repeated object is
    /// genuinely the same pointer (an identity-sharing walk prices it once per call). Every
    /// `observe` below runs over its own fresh [`BifContext`], so a third call proves the state is
    /// per-**call**, not per-context either.
    #[test]
    fn size_shared_is_call_scoped_and_answers_the_same_number_twice() {
        let mut heap = Heap::new(1 << 12).expect("test heap");
        let one = make_small(1);
        let two = make_small(2);
        let inner = list2(&mut heap, one, two); // [1, 2]
        let repeated = list2(&mut heap, inner, inner); // [ [1,2], [1,2] ]
        let triple = list2(&mut heap, inner, repeated); // [ [1,2], [1,2], [1,2] ]

        let first = observe("size_shared", 1, &[triple]);
        let second = observe("size_shared", 1, &[triple]);
        assert_ne!(
            first, "int(0)",
            "the repeated subterm's words must be counted at least once"
        );
        assert_eq!(
            first, second,
            "size_shared/1 must be call-scoped: a fresh call says the same number"
        );
        // A third call through yet another fresh context must agree again.
        let third = observe("size_shared", 1, &[triple]);
        assert_eq!(first, third);
    }

    /// The keys that stay **seam-backed** answer the typed capability gap — never a
    /// fabricated value, never `undef` — and the two keys this module implements for real
    /// are registered, listed in [`REAL_BIFS`] and gone from the seam roster.
    #[test]
    fn the_four_remaining_seam_keys_forward_to_the_hook() {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        for &(module, function, arity) in &[
            ("erts_debug", "breakpoint", 2),
            ("erts_debug", "dirty", 3),
            ("erts_debug", "dirty_cpu", 2),
            ("erts_debug", "dirty_io", 2),
        ] {
            assert!(
                reg.contains(module, function, arity),
                "{module}:{function}/{arity} must be registered"
            );
            assert!(
                crate::hook_sets::needs_runtime_hooks(module, function, arity),
                "{module}:{function}/{arity} must stay in the RuntimeHooks roster"
            );
            let args = vec![make_small(1); arity as usize];
            assert_eq!(
                observe_args(module, function, arity, &args),
                "error:unsupported_bif",
                "{module}:{function}/{arity}"
            );
        }

        // The two host-independent keys this module serves itself: registered, listed in
        // `REAL_BIFS`, and **not** in the `RuntimeHooks` roster (a real implementation
        // behind the seam bridge would be dead code — `beam_rt`'s native floor answers
        // before the registry).
        for &(function, arity) in &[("dist_ext_to_term", 2), ("instructions", 0)] {
            assert!(
                reg.contains("erts_debug", function, arity),
                "erts_debug:{function}/{arity} must be registered"
            );
            assert!(
                REAL_BIFS.contains(&("erts_debug", function, arity)),
                "erts_debug:{function}/{arity} must be listed in REAL_BIFS"
            );
            assert!(
                !crate::hook_sets::needs_runtime_hooks("erts_debug", function, arity),
                "erts_debug:{function}/{arity} is a real BIF but still sits in the seam roster"
            );
        }
    }

    // -----------------------------------------------------------------------
    // dist_ext_to_term/2
    //
    // The whole oracle probe these expectations were taken from (OTP 29.1.1 /
    // erts-17.1, `erl` on `PATH`):
    //
    // ```text
    // erl -noshell -eval '
    //   R = (try erts_debug:dist_ext_to_term(Slots, Image) catch C:E -> {C,E} end),
    //   io:format("~p~n",[R]), halt(0).'
    // ```
    //
    // ```text
    // ({},                  <<131,97,5>>)      => 5
    // ({a},                 <<131,97,5>>)      => 5
    // ({a},                 <<131,97,5,99>>)   => 5      (the trailing byte is ignored)
    // ({a},                 <<131,82,0>>)      => a      (`ATOM_CACHE_REF` slot 0)
    // ({},                  <<131,82,0>>)      => {error,badarg}
    // ({a},                 <<131,82,1>>)      => {error,badarg}
    // (256-atom tuple,      <<131,97,5>>)      => {error,badarg}
    // ({a,b},               <<131,97,5>>)      => 5
    // ({1},                 <<131,97,5>>)      => {error,badarg}
    // ({a},                 <<>>)              => {error,badarg}
    // ({a},                 foo)               => {error,badarg}
    // ({a},                 <<131>>)           => {error,badarg}
    // ({a},                 <<131,97,5:7>>)    => {error,badarg}
    // (-1,                  <<131,97,5>>)      => {error,badarg}
    // ({a},                 <<97,5>>)          => {error,badarg}
    // ({a}, term_to_binary(binary:copy(<<"abcdefghij">>,100), [compressed]))
    //                                          => {error,badarg}
    // ```
    //
    // The last row is the interesting one: a **genuine** `[131,80,…]` image is rejected
    // as well, because the C's distribution decode (`external.c:1273-1311`) never
    // accepts the `COMPRESSED` wrapper — it is a `term_to_binary`-only form.

    /// The atom-cache tuple of one `dist_ext_to_term/2` probe row.
    #[derive(Clone, Copy)]
    enum Slots {
        /// `{a, b, …}` — one atom per cache slot, in slot order.
        Atoms(&'static [&'static str]),
        /// `{1}` — a non-atom element, rejected before the image is looked at.
        NonAtom,
        /// `-1` — not a tuple at all.
        NotTuple,
        /// `list_to_tuple(lists:duplicate(256, a))` — past the 255-slot bound.
        TooManyAtoms,
    }

    /// The image argument of one `dist_ext_to_term/2` probe row.
    #[derive(Clone, Copy)]
    enum Image {
        /// The byte-aligned bitstring `<<bytes>>`.
        Bytes(&'static [u8]),
        /// `<<bytes:bits>>` — a bitstring that is **not** byte aligned.
        Bits(&'static [u8], usize),
        /// A genuine `term_to_binary(…, [compressed])` image: `times` repetitions of
        /// `repeat`, encoded through the shared `beam_codec` encoder at level 6.
        Compressed {
            repeat: &'static [u8],
            times: usize,
        },
        /// The atom `foo`: not a bitstring.
        NotBitstring,
    }

    /// One row of the `dist_ext_to_term/2` matrix of the module doc's probe.
    struct DistRow {
        /// The oracle-transcript label this row is reported under (no spaces: it is a
        /// field of the live oracle's one-line-per-row output).
        label: &'static str,
        slots: Slots,
        image: Image,
        /// The oracle's value for a successful decode; `None` where it raises `badarg`.
        value: Option<&'static str>,
    }

    /// The matrix, in the order of the module doc's transcript.
    const DIST_ROWS: &[DistRow] = &[
        DistRow {
            label: "empty_cache_small_int",
            slots: Slots::Atoms(&[]),
            image: Image::Bytes(&[131, 97, 5]),
            value: Some("5"),
        },
        DistRow {
            label: "slot_zero_unused",
            slots: Slots::Atoms(&["a"]),
            image: Image::Bytes(&[131, 97, 5]),
            value: Some("5"),
        },
        DistRow {
            label: "trailing_byte_ignored",
            slots: Slots::Atoms(&["a"]),
            image: Image::Bytes(&[131, 97, 5, 99]),
            value: Some("5"),
        },
        DistRow {
            label: "atom_cache_ref_slot_zero",
            slots: Slots::Atoms(&["a"]),
            image: Image::Bytes(&[131, 82, 0]),
            value: Some("a"),
        },
        DistRow {
            label: "atom_cache_ref_unset_slot",
            slots: Slots::Atoms(&[]),
            image: Image::Bytes(&[131, 82, 0]),
            value: None,
        },
        DistRow {
            label: "atom_cache_ref_unset_slot_one",
            slots: Slots::Atoms(&["a"]),
            image: Image::Bytes(&[131, 82, 1]),
            value: None,
        },
        DistRow {
            label: "cache_tuple_past_255_slots",
            slots: Slots::TooManyAtoms,
            image: Image::Bytes(&[131, 97, 5]),
            value: None,
        },
        DistRow {
            label: "two_cache_slots",
            slots: Slots::Atoms(&["a", "b"]),
            image: Image::Bytes(&[131, 97, 5]),
            value: Some("5"),
        },
        DistRow {
            label: "non_atom_cache_element",
            slots: Slots::NonAtom,
            image: Image::Bytes(&[131, 97, 5]),
            value: None,
        },
        DistRow {
            label: "empty_image",
            slots: Slots::Atoms(&["a"]),
            image: Image::Bytes(&[]),
            value: None,
        },
        DistRow {
            label: "image_not_a_bitstring",
            slots: Slots::Atoms(&["a"]),
            image: Image::NotBitstring,
            value: None,
        },
        DistRow {
            label: "version_byte_only",
            slots: Slots::Atoms(&["a"]),
            image: Image::Bytes(&[131]),
            value: None,
        },
        DistRow {
            label: "image_not_byte_aligned",
            slots: Slots::Atoms(&["a"]),
            image: Image::Bits(&[131, 97, 5], 23),
            value: None,
        },
        DistRow {
            label: "cache_not_a_tuple",
            slots: Slots::NotTuple,
            image: Image::Bytes(&[131, 97, 5]),
            value: None,
        },
        DistRow {
            label: "missing_version_magic",
            slots: Slots::Atoms(&["a"]),
            image: Image::Bytes(&[97, 5]),
            value: None,
        },
        DistRow {
            label: "compressed_wrapper",
            slots: Slots::Atoms(&["a"]),
            image: Image::Compressed {
                repeat: b"abcdefghij",
                times: 100,
            },
            value: None,
        },
    ];

    /// The two arguments of one probe row, materialized on a fresh heap.
    ///
    /// The atom terms come from a fresh [`probe_atoms`] table — the same deterministic
    /// seed [`observe_args`] builds its context over, so the cache slots name the same
    /// atoms the context will. The returned heap keeps the argument terms alive and must
    /// therefore outlive the call.
    fn dist_args(row: &DistRow) -> (Heap, Vec<Eterm>) {
        let mut heap = Heap::new(1 << 13).expect("test heap");
        let atoms = probe_atoms();
        let atom = |name: &str| make_atom(atoms.lookup(name).expect("a seeded atom"));
        let slots = match row.slots {
            Slots::Atoms(names) => {
                let elems: Vec<Eterm> = names.iter().map(|name| atom(name)).collect();
                heap.tuple(&elems).expect("the cache tuple fits")
            }
            Slots::NonAtom => heap.tuple(&[make_small(1)]).expect("the tuple fits"),
            Slots::NotTuple => make_small(-1),
            Slots::TooManyAtoms => {
                let elems = vec![atom("a"); 256];
                heap.tuple(&elems).expect("the 256-element tuple fits")
            }
        };
        let image = match row.image {
            Image::Bytes(bytes) => {
                access::make_heap_bitstring(&mut heap, bytes, bytes.len() * 8)
                    .expect("the image fits")
            }
            Image::Bits(bytes, bits) => {
                access::make_heap_bitstring(&mut heap, bytes, bits).expect("the image fits")
            }
            Image::NotBitstring => atom("foo"),
            Image::Compressed { repeat, times } => {
                let payload = repeat.repeat(times);
                let term = access::make_heap_bitstring(&mut heap, &payload, payload.len() * 8)
                    .expect("the payload fits");
                let image = beam_codec::encode_term_compressed(term, &atoms, 6)
                    .expect("the compressed image encodes");
                assert_eq!(
                    (image[0], image[1]),
                    (beam_codec::VERSION_MAGIC, beam_codec::COMPRESSED_EXT),
                    "{}: the row must carry a genuine COMPRESSED image",
                    row.label
                );
                access::make_heap_bitstring(&mut heap, &image, image.len() * 8)
                    .expect("the image fits")
            }
        };
        (heap, vec![slots, image])
    }

    /// [`observe_args`]'s label for one probe row.
    fn dist_observe(row: &DistRow) -> String {
        let (_heap, args) = dist_args(row);
        observe_args("erts_debug", "dist_ext_to_term", 2, &args)
    }

    /// The label [`observe_args`] must answer for one probe row: an integer value renders
    /// `int(N)`, an atom value `atom(Name)`, a rejected call `badarg`.
    fn dist_expected(row: &DistRow) -> String {
        match row.value {
            None => "badarg".to_string(),
            Some(value) => match value.parse::<i64>() {
                Ok(number) => format!("int({number})"),
                Err(_) => format!("atom({value})"),
            },
        }
    }

    /// The `dist_ext_to_term/2` matrix of the oracle transcript in the module doc:
    /// every row's outcome is pinned, and every failure is the plain `badarg` the C's
    /// guard order produces.
    #[test]
    fn dist_ext_to_term_matches_the_oracle_matrix() {
        for row in DIST_ROWS {
            assert_eq!(dist_observe(row), dist_expected(row), "{}", row.label);
        }
    }

    /// The Erlang source of a row's cache argument.
    fn erl_slots(row: &DistRow) -> String {
        match row.slots {
            Slots::Atoms(names) => format!("{{{}}}", names.join(",")),
            Slots::NonAtom => "{1}".to_string(),
            Slots::NotTuple => "-1".to_string(),
            Slots::TooManyAtoms => "list_to_tuple(lists:duplicate(256,a))".to_string(),
        }
    }

    /// The Erlang source of an image **term**: `foo` for the not-a-bitstring row, else a
    /// bitstring literal spelling exactly the bytes (and the trailing partial byte) that
    /// were handed to this crate's BIF.
    fn erl_image(term: Eterm) -> String {
        if access::is_atom(term) {
            return "foo".to_string();
        }
        let bits = access::bitstring_bit_size(term).expect("a bitstring");
        let bytes = access::bitstring_bits(term).expect("a bitstring");
        let whole = bits / 8;
        let partial = bits % 8;
        let mut parts: Vec<String> = bytes[..whole].iter().map(u8::to_string).collect();
        if partial != 0 {
            parts.push(format!("{}:{partial}", bytes[whole]));
        }
        format!("<<{}>>", parts.join(","))
    }

    /// The same matrix, cross-checked against a **live** `erl` in one startup: one line
    /// per row, `<label> <ok Value | badarg>`, compared with this crate's outcome and
    /// with the pinned value of every successful row.
    ///
    /// Skips **loudly** when no `erl` is on `PATH`; the pinned golden above is what CI
    /// without an oracle still checks.
    #[test]
    fn the_dist_ext_matrix_agrees_with_a_live_oracle() {
        if crate::runtime::ddll::test_support::which_erl().is_none() {
            eprintln!(
                "SKIP: no `erl` on PATH — the dist_ext_to_term matrix was not \
                 cross-checked against a live oracle"
            );
            return;
        }
        // One `-eval` statement, one startup, one line per row.
        let mut script = String::from(
            "P = fun(L, F) -> R = (try F() of V -> lists:flatten(io_lib:format(\"ok ~p\", [V])) \
             catch error:badarg -> \"badarg\"; C:Rsn -> lists:flatten(io_lib:format(\
             \"other ~p:~p\", [C, Rsn])) end), io:format(\"~s ~s~n\", [L, R]) end, ",
        );
        let mut labels = Vec::new();
        for row in DIST_ROWS {
            let (_heap, args) = dist_args(row);
            script.push_str(&format!(
                "P(\"{}\", fun() -> erts_debug:dist_ext_to_term({}, {}) end), ",
                row.label,
                erl_slots(row),
                erl_image(args[1])
            ));
            labels.push(row.label);
        }
        script.push_str("halt(0).");

        let output = std::process::Command::new("erl")
            .args(["-noshell", "-eval", &script])
            .output();
        let stdout = match output {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
            Ok(out) => {
                eprintln!(
                    "SKIP: the live oracle failed ({}); stderr: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
                return;
            }
            Err(err) => {
                eprintln!("SKIP: could not run `erl` ({err})");
                return;
            }
        };

        let lines: Vec<&str> = stdout.lines().filter(|line| !line.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            labels.len(),
            "the oracle printed {} lines for {} rows: {stdout}",
            lines.len(),
            labels.len()
        );
        for (row, line) in DIST_ROWS.iter().zip(&lines) {
            let (label, answer) = line
                .split_once(' ')
                .unwrap_or_else(|| panic!("{line:?} is not `<label> <answer>`"));
            assert_eq!(label, row.label, "the oracle's row order");
            let expected = match row.value {
                None => "badarg".to_string(),
                Some(value) => format!("ok {value}"),
            };
            assert_eq!(*answer, expected, "{}", row.label);
            assert_eq!(dist_observe(row), dist_expected(row), "{}", row.label);
        }
    }

    // -----------------------------------------------------------------------
    // instructions/0
    //
    // `erts_debug:instructions()` is the `beam_makeops`-generated `opc[]` name table,
    // in dispatch order, each name a charlist of its bytes.
    // This tree holds **248** rows, generated from the pinned release's own
    // `erts/emulator/beam/jit/x86/ops.tab`: re-running the committed
    // `gen_instructions.escript` against that tree reproduces the committed table
    // byte for byte, so the roster is right for OTP 30.0-rc0 / erts-17.1.
    // An oracle of an *older* release holds a different roster, and the difference is a
    // feature/fix backport split, never a divergence — so it is calibrated per oracle
    // release in the [`COHORTS`] table below rather than smoothed over.
    // Only the native-record family has ever moved
    // (`erts/emulator/beam/jit/x86/ops.tab:1327-1371`):
    // `6fbf8a305e` ("Add instructions for matching/updating of native records") added
    // `i_get_local_record_elements q xy I *` and `i_update_local_native_record a q s d t I *`,
    // and moved `i_get_record_elements`' failed-typed source operand `f` → `j`;
    // `e5a280abff` (issue #11691, "Handle native record operations with non-zero fail
    // label") prefixed the fail label `j` to `i_create_native_record`/`i_update_native_record`,
    // i.e. `c` → `j c` and `s` → `j s`.
    // This tree has both commits; the OTP 29 maint patch that hosts this sandbox's oracle
    // backported the fix only, and OTP 29.1.1 has neither.

    /// `use` of the generated table, so the assertions below read as prose.
    use crate::runtime::instructions::INSTRUCTIONS;

    /// One live-oracle cohort: an identifiable OTP/erts release pair, plus exactly how
    /// this tree's generated roster differs from that release's `opc[]`.
    ///
    /// A cohort is selected by the **exact** pair
    /// `(erlang:system_info(otp_release), erlang:system_info(version))`.
    /// "No skew" is a claim about one specific release, never a default: an oracle that
    /// matches no cohort fails the roster test loudly.
    struct OracleCohort {
        /// `erlang:system_info(otp_release)`, e.g. `"29"`.
        otp_release: &'static str,
        /// `erlang:system_info(version)`, e.g. `"17.1.1"` — the `erts-` prefix is not part
        /// of the value.
        erts_version: &'static str,
        /// This tree's rows that the cohort's oracle does **not** have: opcodes the newer
        /// release added and no maint branch backported.
        dropped: &'static [&'static str],
        /// `(this tree's spelling, the cohort oracle's spelling)` for a row present on
        /// both, where the older release spells the same opcode differently.
        renamed: &'static [(&'static str, &'static str)],
    }

    impl OracleCohort {
        /// The cohort's identity, for a failure message.
        fn id(&self) -> String {
            format!("OTP {}/erts-{}", self.otp_release, self.erts_version)
        }
    }

    /// Every oracle release this roster is calibrated against, one entry per
    /// `(otp_release, erts version)` pair that has actually served as an oracle.
    ///
    /// Adding a cohort means reading that release's own
    /// `erts/emulator/beam/jit/x86/ops.tab` and diffing its `beam_makeops` output against
    /// [`INSTRUCTIONS`]; nothing here may be inferred from a version string.
    static COHORTS: &[OracleCohort] = &[
        OracleCohort {
            // OTP 29.1.1 — the original calibration oracle: it has neither the
            // native-record matching/updating feature `6fbf8a305e` nor its fail-label fix
            // `e5a280abff`.
            otp_release: "29",
            erts_version: "17.1",
            dropped: &[
                "i_get_local_record_elements_qxI",
                "i_get_local_record_elements_qyI",
                "i_update_local_native_record_aqsdtI",
            ],
            renamed: &[
                ("i_create_native_record_jcdtI", "i_create_native_record_cdtI"),
                ("i_get_record_elements_jxI", "i_get_record_elements_fxI"),
                ("i_get_record_elements_jyI", "i_get_record_elements_fyI"),
                ("i_update_native_record_jsdtI", "i_update_native_record_sdtI"),
            ],
        },
        OracleCohort {
            // OTP 29.1.2 — the 29 maint patch: it backported the fix `e5a280abff`, not the
            // feature `6fbf8a305e`, so both `i_*_native_record` spellings already agree
            // and only the two `i_get_record_elements` rows still hunt for their source
            // operand with `f`.
            otp_release: "29",
            erts_version: "17.1.1",
            dropped: &[
                "i_get_local_record_elements_qxI",
                "i_get_local_record_elements_qyI",
                "i_update_local_native_record_aqsdtI",
            ],
            renamed: &[
                ("i_get_record_elements_jxI", "i_get_record_elements_fxI"),
                ("i_get_record_elements_jyI", "i_get_record_elements_fyI"),
            ],
        },
        OracleCohort {
            // OTP 30.0-rc0 / erts-17.1 — the pinned release itself: the comparison is
            // exact, so any entry appearing here would be a divergence rather than a skew.
            otp_release: "30",
            erts_version: "17.1",
            dropped: &[],
            renamed: &[],
        },
    ];

    /// The [`COHORTS`] entry for the oracle that printed `otp_release`/`erts_version`.
    ///
    /// An uncalibrated oracle **fails loudly**, naming both observed values: comparing an
    /// unknown release with "no skew" would turn every difference into a claim we never
    /// checked.
    fn cohort_for_oracle(otp_release: &str, erts_version: &str) -> &'static OracleCohort {
        COHORTS
            .iter()
            .find(|cohort| {
                cohort.otp_release == otp_release && cohort.erts_version == erts_version
            })
            .unwrap_or_else(|| {
                panic!(
                    "unrecognised oracle cohort: erlang:system_info(otp_release) = \
                     {otp_release:?}, erlang:system_info(version) = {erts_version:?}.\n\
                     Add an `OracleCohort` to COHORTS calibrated from that release's own \
                     erts/emulator/beam/jit/x86/ops.tab (the native-record family is at \
                     lines 1327-1371 here) and diffed against INSTRUCTIONS — never widen \
                     the comparison instead."
                )
            })
    }

    /// [`INSTRUCTIONS`] is this tree's dispatch roster: 248 unique, non-empty ASCII names,
    /// the same first and last rows the oracle reports (the ends are release-stable).
    #[test]
    fn the_instruction_roster_is_the_generated_table() {
        assert_eq!(INSTRUCTIONS.len(), 248, "the generated opc[] row count");
        let unique: std::collections::BTreeSet<&str> = INSTRUCTIONS.iter().copied().collect();
        assert_eq!(unique.len(), INSTRUCTIONS.len(), "a duplicated row");
        for (index, name) in INSTRUCTIONS.iter().enumerate() {
            assert!(!name.is_empty(), "row {index} is empty");
            assert!(name.is_ascii(), "row {index} ({name}) is not ASCII");
        }
        assert_eq!(
            INSTRUCTIONS[..4],
            [
                "aligned_label_Lt",
                "allocate_tt",
                "allocate_heap_tIt",
                "append_cons_Is"
            ],
            "the first four rows"
        );
        assert_eq!(
            INSTRUCTIONS[INSTRUCTIONS.len() - 4..],
            [
                "wait_locked_f",
                "wait_timeout_locked_sf",
                "wait_timeout_unlocked_sf",
                "wait_unlocked_f"
            ],
            "the last four rows"
        );
    }

    /// The bytes of the charlist `term` (each element a small integer byte).
    fn charlist_bytes(term: Eterm) -> Vec<u8> {
        list_of(term)
            .into_iter()
            .map(|element| {
                assert!(access::is_small(element), "a charlist element must be a small");
                signed_value(element) as u8
            })
            .collect()
    }

    /// `instructions/0` renders **every** row of the generated roster as its own charlist,
    /// in dispatch order — never a truncated, reordered or fabricated list.
    #[test]
    fn instructions_0_renders_every_row_as_a_charlist() {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let atoms = probe_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let out = reg
            .call("erts_debug", "instructions", 0, &mut ctx, &[])
            .expect("instructions/0 runs");

        let rows = list_of(out);
        assert_eq!(rows.len(), INSTRUCTIONS.len(), "one charlist per row");
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(
                charlist_bytes(*row),
                INSTRUCTIONS[index].as_bytes(),
                "row {index}"
            );
        }
        // The two ends, spelled out as bytes (the ends are release-stable).
        assert_eq!(charlist_bytes(rows[0]), b"aligned_label_Lt");
        assert_eq!(
            charlist_bytes(rows[INSTRUCTIONS.len() - 1]),
            b"wait_unlocked_f"
        );
    }

    /// Every [`COHORTS`] entry is a real, non-overlapping claim about [`INSTRUCTIONS`]:
    /// each dropped row and each in-tree spelling is really in the generated roster, no
    /// oracle spelling is, no row is claimed twice inside one cohort, and the pinned
    /// release's own cohort carries zero skew.
    ///
    /// **Why this exists:** the roster test below trusts the cohort table, so the table
    /// must be checkable with no `erl` in sight.
    /// The entries are a *backport split* — the opcodes and operand letters the
    /// `6fbf8a305e` feature and the `e5a280abff` fix moved (`ops.tab:1327-1371`), present
    /// here because this tree is OTP 30 and absent from the OTP 29 oracles — so a stale or
    /// invented entry has to fail here instead of letting a divergence hide behind it.
    #[test]
    fn the_version_skew_exception_set_is_exactly_the_documented_one() {
        let roster: std::collections::BTreeSet<&str> = INSTRUCTIONS.iter().copied().collect();
        let mut identities = std::collections::BTreeSet::new();
        for cohort in COHORTS {
            // Within one cohort a row is claimed at most once: a dropped row cannot also be
            // renamed, and no rename source may appear twice.
            let mut claimed: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
            let identity = (cohort.otp_release, cohort.erts_version);
            assert!(
                identities.insert(identity),
                "{} is listed twice",
                cohort.id()
            );
            for name in cohort.dropped {
                assert!(
                    roster.contains(name),
                    "{}: dropped row {name} is not in the generated roster",
                    cohort.id()
                );
                assert!(
                    claimed.insert(name),
                    "{}: {name} is claimed twice by the cohort's tables",
                    cohort.id()
                );
            }
            for (ours, oracle) in cohort.renamed {
                assert!(
                    roster.contains(ours),
                    "{}: in-tree spelling {ours} is not in the generated roster",
                    cohort.id()
                );
                assert!(
                    !roster.contains(oracle),
                    "{}: {oracle} is an oracle spelling and must not be in this tree's roster",
                    cohort.id()
                );
                assert_ne!(ours, oracle, "{}: {ours} is renamed to itself", cohort.id());
                assert!(
                    claimed.insert(ours),
                    "{}: {ours} is claimed twice by the cohort's tables",
                    cohort.id()
                );
            }
            // The mapped roster keeps every row it does not drop, and no rename collides
            // with another row (a collision would silently shrink the comparison).
            let mapped = our_roster_for_the_oracle(cohort);
            assert_eq!(
                mapped.len(),
                INSTRUCTIONS.len() - cohort.dropped.len(),
                "{}: the mapped roster keeps every row it does not drop",
                cohort.id()
            );
            let unique: std::collections::BTreeSet<&str> =
                mapped.iter().map(|(_, oracle)| oracle.as_str()).collect();
            assert_eq!(
                unique.len(),
                mapped.len(),
                "{}: a rename collides with another row",
                cohort.id()
            );
        }
        // The documented sizes: a fourth dropped row or a fifth rename is a new claim about
        // a release, not a tweak, so it has to be written down here too.
        let otp_29_1_1 = cohort_for_oracle("29", "17.1");
        assert_eq!(otp_29_1_1.dropped.len(), 3, "OTP 29.1.1: the three feature rows");
        assert_eq!(otp_29_1_1.renamed.len(), 4, "OTP 29.1.1: the four renames");
        let otp_29_1_2 = cohort_for_oracle("29", "17.1.1");
        assert_eq!(otp_29_1_2.dropped.len(), 3, "OTP 29.1.2: the same three feature rows");
        assert_eq!(otp_29_1_2.renamed.len(), 2, "OTP 29.1.2: the two get_record_elements renames");
        assert_eq!(COHORTS.len(), 3, "the calibrated oracle cohort count");
        // The pinned release is compared exactly: a skew entry for it would be a divergence
        // in disguise.
        let pinned = cohort_for_oracle("30", "17.1");
        assert!(
            pinned.dropped.is_empty(),
            "OTP 30.0-rc0/erts-17.1 is this tree's own release: no dropped rows"
        );
        assert!(
            pinned.renamed.is_empty(),
            "OTP 30.0-rc0/erts-17.1 is this tree's own release: no renames"
        );
    }

    /// The generated roster equals a **live** oracle's roster element for element, in
    /// order, once that oracle's own [`COHORTS`] entry is applied.
    ///
    /// The oracle identifies itself by the exact `(otp_release, erts version)` pair it
    /// prints, so the same test is meaningful against any calibrated release and an
    /// uncalibrated one fails with both observed values named.
    /// **Every** diverging row lands in one report — index, this tree's spelling, the
    /// oracle's, and a stale-rename hint — so a real divergence cannot hide behind the
    /// skew set.
    /// Skips **loudly** when no `erl` is on `PATH`.
    #[test]
    fn the_generated_roster_matches_a_live_oracle_modulo_the_release_skew() {
        if crate::runtime::ddll::test_support::which_erl().is_none() {
            eprintln!(
                "SKIP: no `erl` on PATH — the instructions/0 roster was not cross-checked \
                 against a live oracle"
            );
            return;
        }
        let mut erl = std::process::Command::new("erl");
        erl.args([
            "-noshell",
            "-eval",
            "L = erts_debug:instructions(), \
             io:format(\"otp ~s~nerts ~s~n~p~n\", [erlang:system_info(otp_release), \
                       erlang:system_info(version), length(L)]), \
             lists:foreach(fun(S) -> io:format(\"~s~n\", [S]) end, L), halt(0).",
        ]);
        let stdout = match erl.output() {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
            Ok(out) => {
                eprintln!(
                    "SKIP: the live oracle failed ({}); stderr: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
                return;
            }
            Err(err) => {
                eprintln!("SKIP: could not run `erl` ({err})");
                return;
            }
        };
        let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
        let otp_release = lines
            .next()
            .and_then(|line| line.trim().strip_prefix("otp "))
            .expect("the oracle printed `otp <otp_release>` first")
            .to_string();
        let erts_version = lines
            .next()
            .and_then(|line| line.trim().strip_prefix("erts "))
            .expect("the oracle printed `erts <version>` second")
            .to_string();
        let count: usize = lines
            .next()
            .expect("the oracle printed the roster length")
            .trim()
            .parse()
            .expect("the oracle's length line is a number");
        let oracle: Vec<&str> = lines.map(str::trim).collect();
        assert_eq!(oracle.len(), count, "the oracle's own count line");

        let cohort = cohort_for_oracle(&otp_release, &erts_version);
        let ours = our_roster_for_the_oracle(cohort);

        let mut report: Vec<String> = Vec::new();
        if ours.len() != oracle.len() {
            report.push(format!(
                "length: this tree yields {} rows for {}, the oracle prints {}",
                ours.len(),
                cohort.id(),
                oracle.len()
            ));
        }
        for (index, ((tree_name, ours), theirs)) in ours.iter().zip(&oracle).enumerate() {
            if ours == theirs {
                continue;
            }
            let mut row = format!(
                "row {index}: this tree says {ours:?} (unmapped {tree_name:?}), \
                 the oracle says {theirs:?}"
            );
            // The oracle agreeing with the *unmapped* spelling is exactly what a rename
            // entry that no longer applies looks like on this release.
            if theirs == tree_name && cohort.renamed.iter().any(|(ours, _)| ours == tree_name) {
                row.push_str(&format!(
                    " — the {} rename entry for {tree_name:?} is stale for this oracle",
                    cohort.id()
                ));
            }
            report.push(row);
        }
        assert!(
            report.is_empty(),
            "the instructions/0 roster disagrees with the {} oracle in {} row(s):\n{}",
            cohort.id(),
            report.len(),
            report.join("\n")
        );
    }

    /// [`INSTRUCTIONS`] with `cohort`'s skew applied: `(this tree's spelling, the spelling
    /// that cohort's oracle prints)`, in dispatch order — the roster that oracle must agree
    /// with.
    fn our_roster_for_the_oracle(cohort: &OracleCohort) -> Vec<(&'static str, String)> {
        INSTRUCTIONS
            .iter()
            .copied()
            .filter(|name| !cohort.dropped.contains(name))
            .map(|name| {
                let renamed = cohort
                    .renamed
                    .iter()
                    .find(|(ours, _)| *ours == name)
                    .map(|(_, oracle)| *oracle);
                (name, renamed.unwrap_or(name).to_string())
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // map_info/1
    //
    // The whole oracle probe these expectations were taken from (OTP 29.1.1 /
    // erts-17.1, `erl` on `PATH`):
    //
    // ```text
    // erl -noshell -eval '
    //   R = (try EXPR catch C:E -> {C,E} end), io:format("~p~n",[R]), halt(0).'
    // ```
    //
    // ```text
    // erts_debug:map_info(#{a=>1,b=>2})                       => {error,badarg}
    // erts_debug:map_info(#{})                                => {error,badarg}
    // erts_debug:map_info(maps:from_list([{I,I} || I <- lists:seq(1,32)]))
    //                                                         => {error,badarg}
    // erts_debug:map_info(1)                                  => {error,{badmap,1}}
    // erts_debug:map_info("abc")                              => {error,{badmap,"abc"}}
    // erts_debug:map_info(maps:from_list([{I,I} || I <- lists:seq(1,33)]))
    //   => [{depth,4},
    //       {arrays,0,[]},
    //       {collisions,0,{0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0}},
    //       {bitmaps,12,{1,5,1,3,1,0,0,0,0,0,0,0,1,0,0,0}},
    //       {leafs,33,{0,4,27,0,2,0,0,0,0,0,0,0,0,0,0,0,0,0}}]
    // erts_debug:map_info(maps:from_list([{I,I} || I <- lists:seq(1,40)]))
    //   => [{depth,4},
    //       {arrays,0,[]},
    //       {collisions,0,{0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0}},
    //       {bitmaps,14,{1,5,3,2,1,1,0,0,0,0,0,0,0,1,0,0}},
    //       {leafs,40,{0,4,32,2,2,0,0,0,0,0,0,0,0,0,0,0,0,0}}]
    // erts_debug:map_info(maps:put(100, M40, maps:from_list([{I,I} || I <- lists:seq(50,90)])))
    //   => [{depth,3},
    //       {arrays,1,[]},
    //       {collisions,0,{0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0}},
    //       {bitmaps,16,{0,9,4,3,0,0,0,0,0,0,0,0,0,0,0,0}},
    //       {leafs,42,{0,5,27,10,0,0,0,0,0,0,0,0,0,0,0,0,0,0}}]
    // ```
    //
    // The maps are built from **integer** keys on purpose: an integer key's
    // `erts_map_hash` is a pure function of the term (no runtime atom index), so the
    // HAMT the oracle builds for `maps:from_list/1` has the same trie shape as the one
    // `beam_term::map::write_map` builds here — which is what makes the summary
    // comparable at all. An atom-keyed map's trie shape is driven by the runtime atom
    // index and legitimately differs (see `beam_term::ihash`).

    /// Builds `maps:from_list([{K, K} || K <- keys])` through this crate's map
    /// producer ([`access::make_map`] → `beam_term::map::write_map`), over the atom
    /// table the caller's BIF context will see — the same live namespace the
    /// path-copying writers get.
    fn int_map(heap: &mut Heap, atoms: &AtomTable, keys: impl IntoIterator<Item = i64>) -> Eterm {
        let pairs: Vec<(Eterm, Eterm)> = keys
            .into_iter()
            .map(|key| (make_small(key), make_small(key)))
            .collect();
        access::make_map(heap, atoms, &pairs).expect("the map fits")
    }

    /// Calls `map_info/1` on `map` (which must already live on `heap`).
    fn probe_info(heap: &mut Heap, atoms: &AtomTable, map: Eterm) -> Result<Eterm> {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        let mut ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0));
        reg.call("erts_debug", "map_info", 1, &mut ctx, &[map])
    }

    /// The elements of the proper list `term`.
    fn list_of(term: Eterm) -> Vec<Eterm> {
        let mut out = Vec::new();
        let mut cur = term;
        while access::is_list(cur) {
            out.push(access::car(cur));
            cur = access::cdr(cur);
        }
        assert!(access::is_nil(cur), "the result must be a proper list");
        out
    }

    /// The small integers of the tuple `term`.
    fn ints_of(term: Eterm) -> Vec<i64> {
        (0..access::tuple_size(term))
            .map(|i| signed_value(access::tuple_element(term, i)))
            .collect()
    }

    /// Reads the `[{depth, D}, {arrays, N, []}, {collisions, N, Bins},
    /// {bitmaps, N, Bins}, {leafs, N, Bins}]` summary into Rust values, checking every
    /// tag atom and the arrays entry's `[]` tail against `atoms`.
    #[allow(clippy::type_complexity)]
    fn read_info(
        info: Eterm,
        atoms: &AtomTable,
    ) -> (
        i64,
        i64,
        (i64, Vec<i64>),
        (i64, Vec<i64>),
        (i64, Vec<i64>),
    ) {
        let entries = list_of(info);
        assert_eq!(entries.len(), 5, "the five summary entries");
        let tag = |entry: Eterm| {
            let atom = access::tuple_element(entry, 0);
            atoms
                .name(beam_term::atom_val(atom))
                .expect("a seeded tag atom")
                .to_string()
        };
        for (index, name) in ["depth", "arrays", "collisions", "bitmaps", "leafs"]
            .into_iter()
            .enumerate()
        {
            assert_eq!(tag(entries[index]), name, "entry {index}");
        }
        assert!(
            access::is_nil(access::tuple_element(entries[1], 2)),
            "{{arrays, N, []}}"
        );
        let count = |entry: Eterm| signed_value(access::tuple_element(entry, 1));
        let bins = |entry: Eterm| ints_of(access::tuple_element(entry, 2));
        (
            count(entries[0]),
            count(entries[1]),
            (count(entries[2]), bins(entries[2])),
            (count(entries[3]), bins(entries[3])),
            (count(entries[4]), bins(entries[4])),
        )
    }

    /// A flat map (any size at or below `MAP_SMALL_MAP_LIMIT`) is `error:badarg`; any
    /// non-map is the structured `error:{badmap, Term}` — the C's test order
    /// (`erl_map.c:3300-3308`).
    #[test]
    fn map_info_rejects_flat_maps_and_non_maps() {
        let mut atoms = AtomTable::new();
        atoms.seed(["badmap", "depth", "arrays", "collisions", "bitmaps", "leafs"]);
        let mut heap = Heap::new(1 << 12).expect("test heap");
        let empty = access::make_map(&mut heap, &atoms, &[]).expect("the empty map fits");
        let flat = int_map(&mut heap, &atoms, 1..=32);
        let tuple = heap.tuple(&[make_small(1)]).expect("the tuple fits");
        for (label, term) in [("empty map", empty), ("32-key flat map", flat)] {
            match probe_info(&mut heap, &atoms, term) {
                Err(BeamError::BadArg) => {}
                other => panic!("{label}: expected BadArg, got {other:?}"),
            }
        }
        for (label, term) in [
            ("integer", make_small(1)),
            ("nil", NIL),
            ("tuple", tuple),
        ] {
            match probe_info(&mut heap, &atoms, term) {
                Err(BeamError::Raise(raise)) => {
                    assert_eq!(raise.reason, ExitReason::Structured, "{label}");
                    // The live reason term is `{badmap, Term}`.
                    let reason = raise
                        .live_reason()
                        .and_then(|live| live.downcast_ref::<Eterm>())
                        .copied()
                        .unwrap_or_else(|| panic!("{label}: no live reason term"));
                    assert!(access::is_tuple(reason), "{label}: a 2-tuple");
                    assert_eq!(access::tuple_size(reason), 2, "{label}");
                    assert_eq!(
                        atoms.name(beam_term::atom_val(access::tuple_element(reason, 0))),
                        Some("badmap"),
                        "{label}"
                    );
                    assert_eq!(access::tuple_element(reason, 1), term, "{label}");
                }
                other => panic!("{label}: expected a raised {{badmap, Term}}, got {other:?}"),
            }
        }
    }

    /// The traversal statistics for the HAMT maps the oracle pins.
    #[test]
    fn map_info_summarises_a_hamt_like_the_oracle() {
        let mut atoms = AtomTable::new();
        atoms.seed(["badmap", "depth", "arrays", "collisions", "bitmaps", "leafs"]);
        let mut heap = Heap::new(1 << 16).expect("test heap");

        // 33 keys: a bitmap head with 13 slots, 4 of them leaves.
        let m33 = int_map(&mut heap, &atoms, 1..=33);
        let info = probe_info(&mut heap, &atoms, m33).expect("map_info runs");
        assert_eq!(
            read_info(info, &atoms),
            (
                4,
                0,
                (0, vec![0; 16]),
                (12, vec![1, 5, 1, 3, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0]),
                (
                    33,
                    vec![0, 4, 27, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
                ),
            ),
            "the 33-key map"
        );

        // 40 keys: a deeper bitmap head (14 nodes, one of them with 14 slots).
        let m40 = int_map(&mut heap, &atoms, 1..=40);
        let info = probe_info(&mut heap, &atoms, m40).expect("map_info runs");
        assert_eq!(
            read_info(info, &atoms),
            (
                4,
                0,
                (0, vec![0; 16]),
                (14, vec![1, 5, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0]),
                (
                    40,
                    vec![0, 4, 32, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
                ),
            ),
            "the 40-key map"
        );
    }

    /// The head node is a 16-slot **array** node once every nibble is present, and the
    /// walk counts it as `arrays`, not as a bitmap node (`erl_map.c:3509-3512`).
    #[test]
    fn map_info_counts_a_full_head_as_an_array_node() {
        let mut atoms = AtomTable::new();
        atoms.seed(["badmap", "depth", "arrays", "collisions", "bitmaps", "leafs"]);
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let inner = int_map(&mut heap, &atoms, 1..=40);
        let mut pairs: Vec<(Eterm, Eterm)> = (50..=90)
            .map(|key| (make_small(key), make_small(key)))
            .collect();
        pairs.push((make_small(100), inner));
        let map = access::make_map(&mut heap, &atoms, &pairs).expect("the map fits");

        let info = probe_info(&mut heap, &atoms, map).expect("map_info runs");
        assert_eq!(
            read_info(info, &atoms),
            (
                3,
                1,
                (0, vec![0; 16]),
                (16, vec![0, 9, 4, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                (
                    42,
                    vec![0, 5, 27, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
                ),
            ),
            "the nested 42-key map"
        );
    }

    /// A HAMT *inner* node is not `is_hashmap`, so it is `{badmap, Node}` — not a
    /// summary (the C's `is_hashmap`/`is_flatmap`/`BADMAP` test order).
    #[test]
    fn map_info_rejects_a_hamt_inner_node_as_badmap() {
        let mut atoms = AtomTable::new();
        atoms.seed(["badmap", "depth", "arrays", "collisions", "bitmaps", "leafs"]);
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let map = int_map(&mut heap, &atoms, 1..=33);
        let node = {
            let view = access::hamt_node(map).expect("a head node");
            // A sub-node slot: a boxed HAMT node.
            let sub = view
                .slots
                .iter()
                .copied()
                .find(|slot| access::hamt_node_kind(*slot) == Some(access::HamtNode::InnerBitmap))
                .expect("the 33-key head has sub-nodes");
            assert!(access::is_map(sub), "an inner node is still is_map");
            assert!(!access::is_hashmap(sub), "but not is_hashmap");
            sub
        };
        match probe_info(&mut heap, &atoms, node) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.reason, ExitReason::Structured);
                let reason = raise
                    .live_reason()
                    .and_then(|live| live.downcast_ref::<Eterm>())
                    .copied()
                    .expect("a live reason term");
                assert_eq!(access::tuple_element(reason, 1), node);
            }
            other => panic!("expected a raised {{badmap, Node}}, got {other:?}"),
        }
    }
}
