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

//! Immediates: small integers, atoms, `NIL`, catch terms and internal pid/port ids.
//!
//! Ground truth: `erts/emulator/beam/erl_term.h` (the "immediate object access
//! methods", "fixnum (small) access methods", "NIL access methods", "atom access
//! methods", `make_catch`, and the pid/port layouts). All values are bit-exact.

use crate::atom::Atom;
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::tag::{
    TAG_IMMED1_MASK, TAG_IMMED1_PID, TAG_IMMED1_PORT, TAG_IMMED1_SIZE, TAG_IMMED1_SMALL,
    TAG_IMMED2_ATOM, TAG_IMMED2_CATCH, TAG_IMMED2_MASK, TAG_IMMED2_NIL, TAG_IMMED2_SIZE,
    TAG_PRIMARY_IMMED1, TAG_PRIMARY_MASK,
};

// ---------------------------------------------------------------------------
// Small integers
// ---------------------------------------------------------------------------

/// `SMALL_BITS` on 64-bit (`64 - _TAG_IMMED1_SIZE`).
pub const SMALL_BITS: u32 = 64 - TAG_IMMED1_SIZE;
/// `MAX_SMALL` = `(1 << (SMALL_BITS - 1)) - 1`.
pub const MAX_SMALL: i64 = (1i64 << (SMALL_BITS - 1)) - 1;
/// `MIN_SMALL` = `-(1 << (SMALL_BITS - 1))`.
pub const MIN_SMALL: i64 = -(1i64 << (SMALL_BITS - 1));

/// `is_immed(x)`.
#[inline]
pub const fn is_immediate(x: Eterm) -> bool {
    (x.raw() & TAG_PRIMARY_MASK) == TAG_PRIMARY_IMMED1
}

/// `is_not_immed(x)`.
#[inline]
pub const fn is_not_immediate(x: Eterm) -> bool {
    !is_immediate(x)
}

/// `is_small(x)`.
#[inline]
pub const fn is_small(x: Eterm) -> bool {
    (x.raw() & TAG_IMMED1_MASK) == TAG_IMMED1_SMALL
}

/// `is_not_small(x)`.
#[inline]
pub const fn is_not_small(x: Eterm) -> bool {
    !is_small(x)
}

/// `make_small(x)` — encode a 60-bit small integer.
///
/// Unchecked (mirrors the C macro; this is an engine hot path): the caller must
/// pass a value in `MIN_SMALL..=MAX_SMALL`, which is only `debug_assert`ed.
#[inline]
pub const fn make_small(value: i64) -> Eterm {
    debug_assert!(value >= MIN_SMALL && value <= MAX_SMALL, "small int out of range");
    Eterm::from_raw(((value as usize) << TAG_IMMED1_SIZE) + TAG_IMMED1_SMALL)
}

/// Range-checked [`make_small`].
#[inline]
pub fn try_make_small(value: i64) -> TermResult<Eterm> {
    if value < MIN_SMALL || value > MAX_SMALL {
        Err(TermError::SmallIntOutOfRange)
    } else {
        Ok(make_small(value))
    }
}

/// `unsigned_val(x)` — the raw 60-bit payload of a small integer.
#[inline]
pub const fn unsigned_value(x: Eterm) -> u64 {
    (x.raw() >> TAG_IMMED1_SIZE) as u64
}

/// `signed_val(x)` — the sign-extended value of a small integer.
#[inline]
pub const fn signed_value(x: Eterm) -> i64 {
    (x.raw() as i64) >> TAG_IMMED1_SIZE
}

/// `is_byte(x)` — true for a small integer in `0..=255`.
#[inline]
pub const fn is_byte(x: Eterm) -> bool {
    let mask = (!0usize << (TAG_IMMED1_SIZE + 8)).wrapping_add(TAG_IMMED1_MASK);
    (x.raw() & mask) == TAG_IMMED1_SMALL
}

// ---------------------------------------------------------------------------
// NIL
// ---------------------------------------------------------------------------

/// `NIL` — the empty list `[]`, an `IMMED2` immediate.
pub const NIL: Eterm = Eterm::from_raw(TAG_IMMED2_NIL);

/// `is_nil(x)`.
#[inline]
pub const fn is_nil(x: Eterm) -> bool {
    x.raw() == TAG_IMMED2_NIL
}

/// `is_not_nil(x)`.
#[inline]
pub const fn is_not_nil(x: Eterm) -> bool {
    !is_nil(x)
}

// ---------------------------------------------------------------------------
// Atoms
// ---------------------------------------------------------------------------

/// `make_atom(x)` — encode an atom index as an immediate.
#[inline]
pub const fn make_atom(atom: Atom) -> Eterm {
    Eterm::from_raw(((atom.raw() as usize) << TAG_IMMED2_SIZE) + TAG_IMMED2_ATOM)
}

/// `is_atom(x)`.
#[inline]
pub const fn is_atom(x: Eterm) -> bool {
    (x.raw() & TAG_IMMED2_MASK) == TAG_IMMED2_ATOM
}

/// `is_not_atom(x)`.
#[inline]
pub const fn is_not_atom(x: Eterm) -> bool {
    !is_atom(x)
}

/// `atom_val(x)` — the atom index encoded in an atom immediate.
#[inline]
pub const fn atom_val(x: Eterm) -> Atom {
    Atom::from_raw((x.raw() >> TAG_IMMED2_SIZE) as u32)
}

// ---------------------------------------------------------------------------
// Catch
// ---------------------------------------------------------------------------

/// `make_catch(x)`.
#[inline]
pub const fn make_catch(x: u64) -> Eterm {
    Eterm::from_raw(((x as usize) << TAG_IMMED2_SIZE) | TAG_IMMED2_CATCH)
}

/// `is_catch(x)`.
#[inline]
pub const fn is_catch(x: Eterm) -> bool {
    (x.raw() & TAG_IMMED2_MASK) == TAG_IMMED2_CATCH
}

/// `catch_val(x)`.
#[inline]
pub const fn catch_val(x: Eterm) -> u64 {
    (x.raw() >> TAG_IMMED2_SIZE) as u64
}

// ---------------------------------------------------------------------------
// Internal pid / port ids
// ---------------------------------------------------------------------------
//
// The C layout splits the 60-bit immediate payload into a process-table number
// and a serial (erl_term.h: `_PID_NUM_SIZE` = 28, `_PID_DATA_SIZE` = 60). Ports
// use the whole 60-bit payload as their number.

/// `_PID_NUM_SIZE`.
pub const PID_NUM_SIZE: u32 = 28;
/// `_PID_DATA_SIZE` = `64 - _TAG_IMMED1_SIZE`.
pub const PID_DATA_SIZE: u32 = 64 - TAG_IMMED1_SIZE;
/// `_PID_SER_SIZE` = `_PID_DATA_SIZE - _PID_NUM_SIZE`.
pub const PID_SER_SIZE: u32 = PID_DATA_SIZE - PID_NUM_SIZE;
/// `_PORT_DATA_SIZE` = `64 - _TAG_IMMED1_SIZE`.
pub const PORT_DATA_SIZE: u32 = 64 - TAG_IMMED1_SIZE;

/// `is_internal_pid(x)`.
#[inline]
pub const fn is_internal_pid(x: Eterm) -> bool {
    (x.raw() & TAG_IMMED1_MASK) == TAG_IMMED1_PID
}

/// `is_not_internal_pid(x)`.
#[inline]
pub const fn is_not_internal_pid(x: Eterm) -> bool {
    !is_internal_pid(x)
}

/// `make_pid_data(Ser, Num)` — combine a process number and serial into raw id data.
#[inline]
pub const fn make_pid_data(ser: u64, num: u64) -> u64 {
    (ser << PID_NUM_SIZE) | num
}

/// Tag raw pid data (from [`make_pid_data`]) as an internal pid term.
///
/// This is the id-tagging half of C's `make_internal_pid` (the process-table
/// lookup half lives in `beam_process`).
#[inline]
pub const fn make_internal_pid(data: u64) -> Eterm {
    Eterm::from_raw(((data as usize) << TAG_IMMED1_SIZE) | TAG_IMMED1_PID)
}

/// The raw 60-bit payload of an internal pid term (`internal_pid_data`).
#[inline]
pub const fn internal_pid_data(x: Eterm) -> u64 {
    ((x.raw() >> TAG_IMMED1_SIZE) as u64) & ((1u64 << PID_DATA_SIZE) - 1)
}

/// `pid_number(x)` — the process-table number of an internal pid term.
#[inline]
pub const fn pid_number(x: Eterm) -> u64 {
    internal_pid_data(x) & ((1u64 << PID_NUM_SIZE) - 1)
}

/// `pid_serial(x)` — the serial of an internal pid term.
#[inline]
pub const fn pid_serial(x: Eterm) -> u64 {
    internal_pid_data(x) >> PID_NUM_SIZE
}

/// `is_internal_port(x)`.
#[inline]
pub const fn is_internal_port(x: Eterm) -> bool {
    (x.raw() & TAG_IMMED1_MASK) == TAG_IMMED1_PORT
}

/// `is_not_internal_port(x)`.
#[inline]
pub const fn is_not_internal_port(x: Eterm) -> bool {
    !is_internal_port(x)
}

/// `make_port_data(Num)` — the raw id data of a port.
#[inline]
pub const fn make_port_data(num: u64) -> u64 {
    num & ((1u64 << PORT_DATA_SIZE) - 1)
}

/// Tag raw port data (from [`make_port_data`]) as an internal port term.
#[inline]
pub const fn make_internal_port(data: u64) -> Eterm {
    Eterm::from_raw(((data as usize) << TAG_IMMED1_SIZE) | TAG_IMMED1_PORT)
}

/// The raw 60-bit payload of an internal port term (`internal_port_data`).
#[inline]
pub const fn internal_port_data(x: Eterm) -> u64 {
    ((x.raw() >> TAG_IMMED1_SIZE) as u64) & ((1u64 << PORT_DATA_SIZE) - 1)
}

/// `port_number(x)` — the number of an internal port term.
#[inline]
pub const fn port_number(x: Eterm) -> u64 {
    internal_port_data(x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::{TAG_IMMED1_PID, TAG_IMMED2_ATOM, TAG_IMMED2_NIL};

    #[test]
    fn small_int_layout_is_pinned() {
        assert_eq!(make_small(0).raw(), 0xF);
        assert_eq!(make_small(1).raw(), 0x1F);
        assert_eq!(make_small(-1).raw(), ((-1isize as usize) << 4) | 0xF);
        assert_eq!(MAX_SMALL, (1i64 << 59) - 1);
        assert_eq!(MIN_SMALL, -(1i64 << 59));
        assert_eq!(SMALL_BITS, 60);
    }

    #[test]
    fn small_int_round_trip_boundaries() {
        let mut values = vec![0i64, 1, -1, 2, -2, 255, 256, -256, MAX_SMALL, MIN_SMALL];
        values.push(MAX_SMALL - 1);
        values.push(MIN_SMALL + 1);
        for v in values {
            let t = make_small(v);
            assert!(is_small(t), "not small: {v}");
            assert!(is_immediate(t));
            assert_eq!(signed_value(t), v);
            // `unsigned_val` is the raw 60-bit payload (no sign extension).
            assert_eq!(unsigned_value(t), (v as u64) & ((1u64 << SMALL_BITS) - 1));
        }
    }

    #[test]
    fn try_make_small_errors_out_of_range() {
        assert_eq!(try_make_small(MAX_SMALL).unwrap(), make_small(MAX_SMALL));
        assert_eq!(try_make_small(MIN_SMALL).unwrap(), make_small(MIN_SMALL));
        assert_eq!(
            try_make_small(MAX_SMALL + 1),
            Err(TermError::SmallIntOutOfRange)
        );
        assert_eq!(
            try_make_small(MIN_SMALL - 1),
            Err(TermError::SmallIntOutOfRange)
        );
    }

    #[test]
    fn is_byte_range() {
        for v in 0..=255i64 {
            assert!(is_byte(make_small(v)), "byte {v}");
        }
        assert!(!is_byte(make_small(256)));
        assert!(!is_byte(make_small(-1)));
        assert!(!is_byte(NIL));
    }

    #[test]
    fn nil_is_a_distinct_immediate() {
        assert_eq!(NIL.raw(), 0x3B);
        assert_eq!(NIL.raw(), TAG_IMMED2_NIL);
        assert!(is_nil(NIL));
        assert!(is_not_nil(make_small(0)));
        assert!(is_immediate(NIL));
        assert!(!is_small(NIL));
        assert!(!is_atom(NIL));
    }

    #[test]
    fn atom_immediate_layout() {
        assert_eq!(make_atom(Atom::from_raw(0)).raw(), 0xB);
        assert_eq!(make_atom(Atom::from_raw(0)).raw(), TAG_IMMED2_ATOM);
        assert_eq!(make_atom(Atom::from_raw(1)).raw(), 0x4B);
        assert!(is_atom(make_atom(Atom::from_raw(1234))));
        assert_eq!(atom_val(make_atom(Atom::from_raw(1234))), Atom::from_raw(1234));
        assert!(!is_atom(NIL));
        assert!(!is_atom(make_small(0)));
    }

    #[test]
    fn catch_layout() {
        assert_eq!(make_catch(0).raw(), TAG_IMMED2_CATCH);
        assert!(is_catch(make_catch(42)));
        assert_eq!(catch_val(make_catch(42)), 42);
        assert!(!is_catch(NIL));
    }

    #[test]
    fn pid_layout_round_trip() {
        let t = make_internal_pid(make_pid_data(7, 1234));
        assert!(is_internal_pid(t));
        assert_eq!(pid_number(t), 1234);
        assert_eq!(pid_serial(t), 7);
        assert_eq!(t.raw() & TAG_IMMED1_MASK, TAG_IMMED1_PID);
        // max fields
        let max = make_internal_pid(make_pid_data(
            (1 << PID_SER_SIZE) - 1,
            (1 << PID_NUM_SIZE) - 1,
        ));
        assert_eq!(pid_number(max), (1 << PID_NUM_SIZE) - 1);
        assert_eq!(pid_serial(max), (1 << PID_SER_SIZE) - 1);
        assert!(!is_internal_port(t));
    }

    #[test]
    fn port_layout_round_trip() {
        let t = make_internal_port(make_port_data(0xDEAD_BEEF));
        assert!(is_internal_port(t));
        assert_eq!(port_number(t), 0xDEAD_BEEF);
        assert!(!is_internal_pid(t));
        // port numbers use the whole 60-bit payload
        let big = (1u64 << 59) - 1;
        assert_eq!(port_number(make_internal_port(make_port_data(big))), big);
    }
}
