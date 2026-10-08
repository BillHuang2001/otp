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

//! The `FLOAT_EXT` (`99`) text formatter — the `{minor_version,0}` float flavour.
//!
//! `DFLAG_NEW_FLOATS` is clear under `term_to_binary(T, [{minor_version,0}])`
//! (`parse_t2b_opts`, `external.c:1504-1507`), so the encoder writes the legacy
//! `FLOAT_EXT` (`99`) form instead of `NEW_FLOAT_EXT` (`70`). The C fills a
//! **fixed 31-byte** field (`external.c:3840-3847`):
//!
//! ```text
//! *ep++ = FLOAT_EXT;
//! i = sys_double_to_chars(f.fd, (char*) ep, (size_t)31);   /* -> "%.20e" */
//! sys_memset(ep+i, 0, 31-i);                               /* zero-fill the tail */
//! ep += 31;
//! ```
//!
//! `sys_double_to_chars(fp, buf, 31)` is `sys_double_to_chars_ext(fp, buf, 31,
//! SYS_DEFAULT_FLOAT_DECIMALS)` with `SYS_DEFAULT_FLOAT_DECIMALS == 20`
//! (`sys.h:890`, `sys_float.c:54-68`, `erl_sys_common_misc.c:142-145`), i.e. C
//! `snprintf(buf, 31, "%.20e", fp)` (after the locale `,`→`.` fixup the C applies).
//!
//! Rust's `format!("{:.20e}", f)` produces the same 21-significant-digit
//! correctly-rounded (round-half-to-even) mantissa as glibc `%.20e`, but a
//! *different exponent tail*: Rust writes `e0`, `e-5`, `e20` (no sign for a
//! positive exponent, no padding), whereas C writes `e+00`, `e-05`, `e+20` (an
//! always-present sign and **at least two** exponent digits). [`format_float_ext`]
//! normalizes Rust's tail to C's, producing byte-identical output.
//!
//! This module is safe Rust.

use beam_common::Result;

use crate::error;

/// The fixed `FLOAT_EXT` payload size in bytes: the C writes the formatted string
/// starting right after the tag and zero-fills the field to exactly 31 bytes
/// (`external.c:3843-3847`).
pub(crate) const FLOAT_EXT_LEN: usize = 31;

/// Renders `value` into the 31-byte `FLOAT_EXT` payload (string bytes then zero
/// padding), byte-exact with C `snprintf(buf, 31, "%.20e", value)` + zero-fill.
///
/// The non-finite cases match glibc's `%.20e` spelling — `"inf"`, `"-inf"` and
/// `"nan"` — rather than Rust's `inf`/`-inf`/`NaN` (note the capital `N`), so the
/// emitted field is the C ground truth's. (Such terms are rejected by the oracle's
/// own `binary_to_term`, so they are not a wire-observable round-trip case.)
///
/// If the rendered string were **longer than 31 bytes** the field cannot hold it;
/// rather than truncate (which would silently corrupt the float) the write is
/// refused with `BeamError::SystemLimit`. A finite `f64` never reaches that: the
/// longest image is a sign, a digit, a point, 20 decimals and a 5-byte exponent
/// (`"-9.99999999999999999999e+308"`, 28 bytes).
pub(crate) fn format_float_ext(value: f64) -> Result<[u8; FLOAT_EXT_LEN]> {
    let text = if value.is_nan() {
        "nan".to_owned()
    } else if value.is_infinite() {
        if value < 0.0 { "-inf" } else { "inf" }.to_owned()
    } else {
        normalize_exponent(&format!("{value:.20e}"))
    };
    if text.len() > FLOAT_EXT_LEN {
        return Err(error::system_limit());
    }
    let mut out = [0u8; FLOAT_EXT_LEN];
    out[..text.len()].copy_from_slice(text.as_bytes());
    Ok(out)
}

/// Normalizes Rust's `{:.20e}` exponent tail to C's `%e`: a sign **always** present
/// and **at least two** exponent digits (`e0` → `e+00`, `e-5` → `e-05`, `e20` →
/// `e+20`, `e-300` stays `e-300`). The mantissa (everything before `e`) is copied
/// unchanged — it is already identical to glibc's.
fn normalize_exponent(rust: &str) -> String {
    let Some((mantissa, exponent)) = rust.split_once('e') else {
        // Unreachable for a finite `f64` (`{:.20e}` always contains `e`), but a
        // robust fallback keeps the function total.
        return rust.to_owned();
    };
    let (sign, digits) = match exponent.strip_prefix('-') {
        Some(rest) => ('-', rest),
        None => ('+', exponent.strip_prefix('+').unwrap_or(exponent)),
    };
    let mut out = String::with_capacity(mantissa.len() + 3 + digits.len());
    out.push_str(mantissa);
    out.push('e');
    out.push(sign);
    if digits.len() < 2 {
        out.push('0');
    }
    out.push_str(digits);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 31-byte payload as `[string bytes][zero padding]`.
    fn payload(s: &str) -> [u8; FLOAT_EXT_LEN] {
        let mut out = [0u8; FLOAT_EXT_LEN];
        out[..s.len()].copy_from_slice(s.as_bytes());
        out
    }

    #[test]
    fn exponent_is_normalized_to_c_style() {
        assert_eq!(
            normalize_exponent("3.14000000000000012434e0"),
            "3.14000000000000012434e+00"
        );
        assert_eq!(
            normalize_exponent("1.00000000000000008180e-5"),
            "1.00000000000000008180e-05"
        );
        assert_eq!(
            normalize_exponent("1.00000000000000000000e20"),
            "1.00000000000000000000e+20"
        );
        assert_eq!(
            normalize_exponent("1.00000000000000002506e-300"),
            "1.00000000000000002506e-300"
        );
        assert_eq!(
            normalize_exponent("4.94065645841246544177e-324"),
            "4.94065645841246544177e-324"
        );
        assert_eq!(
            normalize_exponent("1.23456789123456001282e8"),
            "1.23456789123456001282e+08"
        );
        // A three-digit exponent keeps all of its digits (no truncation to two).
        assert_eq!(
            normalize_exponent("1.00000000000000000000e308"),
            "1.00000000000000000000e+308"
        );
    }

    #[test]
    fn oracle_goldens_are_reproduced_byte_for_byte() {
        // Values and images verified against the system `erl` (OTP 29.1.1 /
        // erts-17.1): `term_to_binary(V, [{minor_version,0}])`.
        let cases: &[(f64, &str)] = &[
            (0.0, "0.00000000000000000000e+00"),
            (1.0, "1.00000000000000000000e+00"),
            (-2.5, "-2.50000000000000000000e+00"),
            (3.14, "3.14000000000000012434e+00"),
            (1.0e-5, "1.00000000000000008180e-05"),
            (1.0e20, "1.00000000000000000000e+20"),
            (1.0e-300, "1.00000000000000002506e-300"),
            (123_456_789.123_456, "1.23456789123456001282e+08"),
            // The minimum positive subnormal double.
            (5.0e-324, "4.94065645841246544177e-324"),
            // A round-half-to-even tie: the exact value is 644048203731.4326171875,
            // whose 21st significant digit (7, odd) is followed by exactly `5`, so
            // the last digit rounds **up** to 8.
            (644_048_203_731.432_6, "6.44048203731432617188e+11"),
            // Negative zero keeps its sign (the C renders `-0.0`).
            (-0.0, "-0.00000000000000000000e+00"),
        ];
        for (value, expected) in cases {
            assert_eq!(
                format_float_ext(*value).expect("a finite float always formats"),
                payload(expected),
                "FLOAT_EXT payload mismatch for {value:?}"
            );
        }
    }

    #[test]
    fn non_finite_values_match_glibc_spelling() {
        // glibc `%.20e`: "inf" / "-inf" / "nan" (never Rust's "NaN").
        assert_eq!(format_float_ext(f64::INFINITY).unwrap(), payload("inf"));
        assert_eq!(
            format_float_ext(f64::NEG_INFINITY).unwrap(),
            payload("-inf")
        );
        assert_eq!(format_float_ext(f64::NAN).unwrap(), payload("nan"));
    }
}
