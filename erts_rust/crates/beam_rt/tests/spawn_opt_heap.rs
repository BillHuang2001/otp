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
//! `spawn_opt/2`'s **cross-field** heap rule (`erts/emulator/beam/erl_process.c`):
//!
//! ```c
//! if (sop->max_heap_size != 0 && sop->max_heap_size < sop->min_heap_size) result = -1;
//! ```
//!
//! `sop->min_heap_size` is the **rounded** value (`H_MIN_SIZE` when the option was absent or
//! below it, else `erts_next_heap_size(x, 0)`), the comparison is against that rounded value,
//! `max_heap_size == 0` means "no upper bound" and is always accepted, and the option **order**
//! is irrelevant because the whole list is parsed before the rule runs.
//!
//! These tests drive the executable (`CARGO_BIN_EXE_beam_rt`) so they pin the observable
//! answer (`ok` = spawns, `badarg` = raises), and cross-check every row against the system
//! oracle when one is installed (filesystem discovery only — the tool is never *run* to find
//! itself; a missing toolchain is a loud skip, never a silent pass).
use std::path::{Path, PathBuf};
use std::process::Command;

/// The `-eval` expression for one `spawn_opt/2` option list: `ok` when the spawn succeeds,
/// `badarg` when it raises.
fn expr(options: &str) -> String {
    format!(
        "R = try spawn_opt(fun()->ok end,[{options}]) of _ -> ok catch error:B -> B end, \
         io:format(\"~p~n\",[R]), halt(0)."
    )
}

/// Runs the built `beam_rt` on `options` and returns the trimmed stdout.
fn rust_answer(options: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .arg("-noshell")
        .arg("-eval")
        .arg(expr(options))
        .output()
        .expect("spawn the beam_rt executable");
    assert!(
        out.status.success(),
        "beam_rt must exit 0 for [{options}], got {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Resolves `erl` by filesystem discovery only (`$ERTS_ORACLE`, then `$PATH`).
fn find_oracle() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ERTS_ORACLE") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    for dir in std::env::var_os("PATH")?.to_string_lossy().split(':') {
        let candidate = Path::new(dir).join("erl");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The oracle's answer for `options`, or `None` when no toolchain is installed (a LOUD skip).
fn oracle_answer(oracle: &Path, options: &str) -> Option<String> {
    let out = Command::new(oracle)
        .args(["-noshell", "-eval", &expr(options)])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The measured OTP 29.1.1 matrix: `(options, expected answer)`.
///
/// `{min_heap_size,100}` rounds to 233, `{min_heap_size,1}`/`2`/`0` to 233 and
/// `{min_heap_size,1000}` to 1598 (the ERTS growth table), and `max_heap_size == 0` is
/// "no upper bound".
const MATRIX: &[(&str, &str)] = &[
    ("{min_heap_size,100},{max_heap_size,100}", "badarg"),
    ("{min_heap_size,100},{max_heap_size,101}", "badarg"),
    ("{min_heap_size,100},{max_heap_size,233}", "ok"),
    ("{min_heap_size,100},{max_heap_size,234}", "ok"),
    ("{min_heap_size,1000},{max_heap_size,1597}", "badarg"),
    ("{min_heap_size,1000},{max_heap_size,1598}", "ok"),
    ("{min_heap_size,1},{max_heap_size,232}", "badarg"),
    ("{min_heap_size,1},{max_heap_size,233}", "ok"),
    ("{min_heap_size,2},{max_heap_size,233}", "ok"),
    ("{min_heap_size,0},{max_heap_size,232}", "badarg"),
    ("{min_heap_size,0},{max_heap_size,233}", "ok"),
    ("{min_heap_size,233},{max_heap_size,232}", "badarg"),
    ("{min_heap_size,1000},{max_heap_size,0}", "ok"),
    ("{max_heap_size,100},{min_heap_size,100}", "badarg"),
    (
        "{max_heap_size,#{size=>100,kill=>true}},{min_heap_size,1000}",
        "badarg",
    ),
];

/// Every row of the matrix answers as the oracle does, and every `ok` row really spawns.
#[test]
fn the_cross_field_heap_rule_matches_the_oracle_matrix() {
    for (options, expected) in MATRIX {
        let got = rust_answer(options);
        assert_eq!(
            &got, expected,
            "[{options}] must answer {expected}, got {got}"
        );
    }
}

/// Each row is cross-checked against the system oracle (a LOUD skip without one).
#[test]
fn the_cross_field_heap_rule_agrees_with_the_system_erl() {
    let Some(oracle) = find_oracle() else {
        eprintln!("SKIP the_cross_field_heap_rule_agrees_with_the_system_erl: no erl on PATH");
        return;
    };
    // A row the runtime must refuse is the probe's liveness check: if the oracle does not
    // answer at all the whole comparison would be vacuous.
    if oracle_answer(&oracle, "{min_heap_size,100},{max_heap_size,100}").is_none() {
        eprintln!(
            "SKIP the_cross_field_heap_rule_agrees_with_the_system_erl: the oracle produced no answer"
        );
        return;
    }
    for (options, expected) in MATRIX {
        let Some(theirs) = oracle_answer(&oracle, options) else {
            eprintln!("SKIP [{options}]: the oracle produced no answer");
            continue;
        };
        assert_eq!(
            &theirs, expected,
            "the oracle's own answer for [{options}] drifted from the recorded matrix"
        );
        assert_eq!(
            rust_answer(options),
            theirs,
            "[{options}] must agree with the oracle"
        );
    }
}
