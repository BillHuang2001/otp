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

//! Contract **A.7** — the `math` module (the real `bif.tab` BIFs over `libm`).
//!
//! A program's numeric output depends on these to the **last bit**, so the group pins the
//! *bit-exact* cases (the values of `math:atanh/1` and friends must equal the oracle's to the
//! last decimal digit) alongside the failure surface: a domain error or an overflow
//! is `error:badarith` (never `badarg`, never a silently wrong float), while a *non-numeric*
//! argument is `error:badarg` — the crate's documented split.
//!
//! `~w` of a float is the shortest representation that round-trips, so rendering a float is
//! already a bit-exact statement; the probes also compare against literals with `=:=`, which is
//! unambiguous about the *value* (independent of the notation the printer chooses).
//!
//! `math:erf/1` and `math:erfc/1` are deliberately **not** pinned here: the implementation
//! notes only claim agreement to ≤1 / ≤8 ULP (safe-Rust series / continued-fraction
//! approximations), and an oracle probe already disagrees at 1 ULP, so a bit-exact golden
//! would be a false mismatch.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.7 `math` cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "math/atanh_bit_exact",
            description: "math:atanh/1 is bit-exact with the oracle across the domain",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `math:atanh/1` is a double-double transcription that is bit-exact with the
                // oracle. The rendered prologue pins the values across the domain (0.5, 0.1,
                // -0.5 and the ±domain-edge arguments), and the `=:=`-against-literal forms
                // pin them to the last bit (a 1-ULP difference would flip a `true` to `false`).
                // The tiny/awkward arguments use `=:=` rather than a rendering so the probe
                // does not depend on the printer's exponent notation (`~w` of `1.0e-8`).
                expr: "{[math:atanh(0.5), math:atanh(0.1), math:atanh(-0.5), \
                        math:atanh(0.9999999), math:atanh(-0.9999999)], \
                       math:atanh(1.0e-8) =:= 1.0e-8, \
                       math:atanh(0.5) =:= 0.5493061443340549, \
                       math:atanh(0.9999999) =:= 8.40562139102231, \
                       math:atanh(-0.0) =:= -0.0}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {[0.5493061443340549,0.10033534773107558,-0.5493061443340549,\
                     8.40562139102231,-8.40562139102231],true,true,true,true}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "math/atanh_domain_badarith",
            description: "math:atanh/1 at and beyond the domain edge is error:badarith",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Each call is caught on its own and only the *reason* is returned (a bare
                // `catch` would embed a volatile stack trace).
                expr: "{try math:atanh(1.0) catch error:Reason1 -> Reason1 end, \
                       try math:atanh(2.0) catch error:Reason2 -> Reason2 end, \
                       try math:atanh(-1.0) catch error:Reason3 -> Reason3 end}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {badarith,badarith,badarith}"),
                exit_code: 0,
            },
        },
        Case {
            id: "math/domain_and_overflow_badarith",
            description: "math domain errors and overflow are error:badarith, never badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{try math:sqrt(-1.0) catch error:Reason1 -> Reason1 end, \
                       try math:log(0) catch error:Reason2 -> Reason2 end, \
                       try math:log(-1.0) catch error:Reason3 -> Reason3 end, \
                       try math:acosh(0.5) catch error:Reason4 -> Reason4 end, \
                       try math:fmod(5.0,0.0) catch error:Reason5 -> Reason5 end, \
                       try math:exp(1000.0) catch error:Reason6 -> Reason6 end}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {badarith,badarith,badarith,badarith,badarith,badarith}"),
                exit_code: 0,
            },
        },
        Case {
            id: "math/value_round_trip",
            description: "the other 21 math BIFs are bit-exact; a non-numeric argument is badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Every one of the 24 `math` BIFs except `atanh/1` (pinned above) and
                // `erf/1`/`erfc/1` (≤1/≤8 ULP, see the module doc), plus the documented
                // `badarg`-vs-`badarith` split: a *non-numeric* argument is `badarg`, only a
                // post-call domain error/overflow is `badarith`.
                expr: "{math:cos(1.0), math:cosh(1.0), math:sin(1.0), math:sinh(1.0), \
                       math:tan(1.0), math:tanh(1.0), math:acos(0.5), math:acosh(2.0), \
                       math:asin(0.5), math:asinh(1.0), math:atan(1.0), math:exp(1.0), \
                       math:log(2.0), math:log2(8.0), math:log10(100.0), math:sqrt(2.0), \
                       math:floor(1.7), math:ceil(1.2), math:atan2(1.0, 2.0), \
                       math:pow(2.0, 10.0), math:fmod(5.5, 2.0), \
                       try math:sqrt(a) catch error:Reason -> Reason end}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {0.5403023058681398,1.5430806348152437,0.8414709848078965,\
                     1.1752011936438014,1.5574077246549023,0.7615941559557649,\
                     1.0471975511965979,1.3169578969248168,0.5235987755982989,\
                     0.881373587019543,0.7853981633974483,2.718281828459045,\
                     0.6931471805599453,3.0,2.0,1.4142135623730951,1.0,2.0,\
                     0.4636476090008061,1024.0,1.5,badarg}",
                ),
                exit_code: 0,
            },
        },
    ]
}
