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

//! The monotonic-time family of [`super::system_info_value`]: the `start_time` and
//! `end_time` keys.
//!
//! Both keys are **compile-time constants of the ERTS monotonic-time interface**, not
//! measured instants and not a time server: every erts-17.1 build answers them
//! identically on every run, so a drop-in replacement must define exactly the same
//! values. The C:
//!
//! ```c
//! erts_get_monotonic_start_time(c_p) { return make_time_val(c_p, ERTS_MONOTONIC_TIME_START_EXTERNAL); }
//! erts_get_monotonic_end_time(c_p)   { return make_time_val(c_p, ERTS_MONOTONIC_TIME_END_EXTERNAL);   }
//! ```
//!
//! (`erl_time_sup.c:2148-2158`), dispatched from `am_start_time`/`am_end_time`
//! (`erl_bif_info.c:2857-2859`). `make_time_val` (`erl_time_sup.c:2133-2146`) returns a
//! small-int immediate when `IS_SSMALL(val)` and a one-digit bignum otherwise
//! (`erts_sint64_to_big`, `big.c:2332-2366`).
//!
//! # The interface, and the internal ↔ external mapping
//!
//! ERTS measures monotonic time **internally** in `ERTS_MONOTONIC_TIME_UNIT` ticks whose
//! origin is `ERTS_MONOTONIC_BEGIN` (`= UNIT`, so the internal origin is one unit, never
//! zero), and exposes it **externally** shifted by a fixed offset
//! `ERTS_MONOTONIC_OFFSET_NATIVE = ERTS_MONOTONIC_TIME_START_EXTERNAL -
//! ERTS_MONOTONIC_BEGIN` (`erl_time.h:331-332`): `external = internal + OFFSET_NATIVE`.
//! `start_time`/`end_time` are the two ends of that external range, chosen so the
//! external values stay representable and so the internal origin is never reached:
//!
//! | constant | derivation | where |
//! |---|---|---|
//! | `ERTS_MONOTONIC_TIME_UNIT` | `1_000_000_000` (nanoseconds) | the 64-bit Unix build (`erl_unix_sys.h:193-198`) |
//! | `ERTS_MONOTONIC_TIME_MIN` / `_MAX` | `(ErtsMonotonicTime)(1ULL << 63)` / `~MIN` (`i64::MIN`/`i64::MAX`) | `erl_unix_sys.h:172-173` |
//! | `ERTS_MONOTONIC_BEGIN` | `UNIT` | `erl_time.h:235-236` |
//! | `ERTS_MONOTONIC_END` | `(TIME_MAX / UNIT) * UNIT` | `erl_time.h:237-239` |
//! | `ERTS_MONOTONIC_TIME_START_EXTERNAL` | `(MIN_SMALL / UNIT) * UNIT` | `erl_time.h:312-323` |
//! | `ERTS_MONOTONIC_TIME_END_EXTERNAL` | `START + (END - BEGIN)` *(the `START < 0` branch)* | `erl_time.h:403-407` |
//!
//! with `MIN_SMALL = -(1 << 59)` (`erl_term.h:269-270`). The division truncates toward
//! zero, so `START_EXTERNAL` is `MIN_SMALL`'s own unit multiple — the C's "NOTE!
//! `ERTS_MONOTONIC_TIME_START_EXTERNAL` *need* to be a multiple of
//! `ERTS_MONOTONIC_TIME_UNIT`" (`erl_time.h:286-288`), asserted below at compile time.
//!
//! # The values on this build
//!
//! * `start_time` = `-576460752000000000` — inside the 60-bit small range, so a small.
//! * `end_time`   = `8646911283000000000` — **outside** it (`> 2^59 - 1`), so the oracle
//!   materializes a bignum; this runtime's [`super::InfoValue::to_eterm`] does the same.
//!
//! Oracle evidence (OTP 29.1.1 / erts-17.1), two identical runs of
//! `erl -noshell -eval 'io:format("~p ~p~n",[erlang:system_info(start_time),erlang:system_info(end_time)]),halt().'`:
//! `-576460752000000000 8646911283000000000` both times (and
//! `abs(erlang:system_info(end_time)) < 1152921504606846976` is `false`, confirming the
//! bignum of the second value).

use beam_common::Result;

use super::InfoValue;
use crate::runtime::Runtime;

/// `ERTS_MONOTONIC_TIME_UNIT` (`erl_time.h:282-283`): the compile-time monotonic time
/// unit of the 64-bit Unix build, one **nanosecond** per tick (`erl_unix_sys.h:195`).
const MONOTONIC_TIME_UNIT: i64 = 1_000_000_000;

/// `MIN_SMALL` (`erl_term.h:270`, `erl_term.h:44-45` in `beam_term::immediate`): the most
/// negative 60-bit small integer, `-(1 << 59)`.
const MIN_SMALL: i64 = -(1i64 << 59);

/// `ERTS_MONOTONIC_TIME_MIN` (`erl_unix_sys.h:172`): the internal monotonic time range's
/// low end, `ErtsMonotonicTime` being a signed 64-bit integer.
const MONOTONIC_TIME_MIN: i64 = i64::MIN;

/// `ERTS_MONOTONIC_TIME_MAX` (`erl_unix_sys.h:173`).
const MONOTONIC_TIME_MAX: i64 = i64::MAX;

/// `ERTS_MONOTONIC_BEGIN` (`erl_time.h:235-236`).
const MONOTONIC_BEGIN: i64 = MONOTONIC_TIME_UNIT;

/// `ERTS_MONOTONIC_END` (`erl_time.h:237-239`).
const MONOTONIC_END: i64 = (MONOTONIC_TIME_MAX / MONOTONIC_TIME_UNIT) * MONOTONIC_TIME_UNIT;

/// `ERTS_MONOTONIC_TIME_START_EXTERNAL` (`erl_time.h:312-323`) — what
/// `erlang:system_info(start_time)` answers.
pub(crate) const START_TIME: i64 = (MIN_SMALL / MONOTONIC_TIME_UNIT) * MONOTONIC_TIME_UNIT;

/// `ERTS_MONOTONIC_TIME_END_EXTERNAL` (`erl_time.h:403-407`) — what
/// `erlang:system_info(end_time)` answers. `START_TIME` is negative on this build, so the
/// `START + (END - BEGIN)` branch of the C macro applies.
pub(crate) const END_TIME: i64 = START_TIME + (MONOTONIC_END - MONOTONIC_BEGIN);

// The C's own invariants, checked when this module compiles:
// `ASSERT(ERTS_MONOTONIC_TIME_MIN < ERTS_MONOTONIC_TIME_MAX)` (`erl_time_sup.c:1065`) and
// `ERTS_MONOTONIC_TIME_START_EXTERNAL *need* to be a multiple of
// ERTS_MONOTONIC_TIME_UNIT` (`erl_time.h:286-288`).
const _: () = assert!(MONOTONIC_TIME_MIN < MONOTONIC_TIME_MAX);
const _: () = assert!(START_TIME % MONOTONIC_TIME_UNIT == 0);
const _: () = assert!(END_TIME % MONOTONIC_TIME_UNIT == 0);

/// The `system_info/1` value of a `time`-family key, or `None` when `key` is not ours.
///
/// Both keys are compile-time interface constants, so the runtime is ignored and no
/// `Runtime` state can change the answer.
pub(crate) fn try_value(key: &str, _rt: &Runtime) -> Option<Result<InfoValue>> {
    match key {
        "start_time" => Some(Ok(InfoValue::Int(START_TIME))),
        "end_time" => Some(Ok(InfoValue::Int(END_TIME))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_term::{is_small, signed_value, AtomTable};

    use crate::info::test_support::runtime;

    /// The oracle's answers (OTP 29.1.1 / erts-17.1), recorded from two identical runs of
    /// the probe in the module docs.
    const ORACLE_START_TIME: i64 = -576_460_752_000_000_000;
    const ORACLE_END_TIME: i64 = 8_646_911_283_000_000_000;

    #[test]
    fn the_interface_constants_are_the_recorded_oracle_values() {
        assert_eq!(START_TIME, ORACLE_START_TIME);
        assert_eq!(END_TIME, ORACLE_END_TIME);
        // Spelled out, so the constants are the C's arithmetic rather than a literal: the
        // start is `MIN_SMALL` truncated to a unit multiple, the end is that plus the whole
        // external range (`START < 0`, so the C's first branch).
        assert_eq!(
            START_TIME,
            (MIN_SMALL / MONOTONIC_TIME_UNIT) * MONOTONIC_TIME_UNIT
        );
        assert_eq!(
            END_TIME,
            START_TIME + (MONOTONIC_END - MONOTONIC_BEGIN)
        );
        // The end is the larger of the two and lies outside the small range — which is why
        // the oracle's `make_time_val` materializes it as a bignum.
        assert!(START_TIME < END_TIME);
        assert!(
            START_TIME >= beam_term::MIN_SMALL,
            "the start time is a small integer"
        );
        assert!(
            END_TIME > beam_term::MAX_SMALL,
            "the end time is outside the 60-bit small range, so a bignum"
        );
    }

    #[test]
    fn both_keys_answer_and_no_other_key_is_ours() {
        let rt = runtime();
        assert_eq!(
            try_value("start_time", &rt).unwrap().unwrap(),
            InfoValue::Int(START_TIME)
        );
        assert_eq!(
            try_value("end_time", &rt).unwrap().unwrap(),
            InfoValue::Int(END_TIME)
        );
        // The rest of the time surface is *not* ours: the time-correction service keys stay
        // with `super::unsupported`, and an unknown key is nobody's.
        assert!(try_value("version", &rt).is_none());
        assert!(try_value("tolerant_timeofday", &rt).is_none());
        assert!(try_value("nonsense", &rt).is_none());
    }

    #[test]
    fn start_time_materializes_as_a_small_and_end_time_as_a_one_digit_bignum() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = AtomTable::new();

        let start = InfoValue::Int(START_TIME)
            .to_eterm(&mut heap, &atoms)
            .expect("a small");
        assert!(is_small(start));
        assert_eq!(signed_value(start), ORACLE_START_TIME);

        let end = InfoValue::Int(END_TIME)
            .to_eterm(&mut heap, &atoms)
            .expect("a bignum");
        let (negative, digits) = beam_codec::term::big_parts(end).expect("a bignum");
        assert!(!negative, "the end time is positive");
        assert_eq!(digits.len(), 1, "a Sint64 bignum is one digit wide (`big.h:120-121`)");
        assert_eq!(digits, [ORACLE_END_TIME as u64]);
    }
}
