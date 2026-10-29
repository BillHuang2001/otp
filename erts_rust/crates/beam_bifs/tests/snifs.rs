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

//! `erlang:system_info(snifs)` — the generated static-NIF roster (`src/snifs.rs`).
//!
//! Three independent pins, each of which fails on its own:
//!
//! 1. **the generated data itself** — its length, its first/last rows in the oracle's
//!    (reversed) order, and the absence of a dotted module name, which is what the C's
//!    `is_snif_term` filter (`erts/emulator/beam/erl_bif_info.c:2743`) removes and which
//!    the generator therefore *asserts* rather than filters;
//! 2. **`bif.tab` row for row** — re-parsing the real table with [`beam_bifs::bif_tab`]
//!    (the crate's own parser, the one the roster generator mirrors) and comparing every
//!    entry, order included, so a hand-edited or stale `snifs.rs` cannot pass;
//! 3. **a live oracle** — `erlang:system_info(snifs)` under the system `erl`, which must be
//!    this roster in oracle order minus the three rows this tree declares and an OTP-29
//!    oracle does not (`erlang:pid_to_binary/1`, `erlang:port_to_binary/1`,
//!    `erlang:ref_to_binary/1`).
//!
//! Every input a test needs but the sandbox may not have (`erts/emulator/beam/bif.tab`, an
//! `erl` on `PATH`) is a **loud skip**, never a silent pass — and never a fabricated
//! expectation. The skew in test 3 is the same class as the `erts_debug:instructions/0`
//! cohort skew: this tree is OTP 30 and the pinned oracle is OTP 29, so the three extra
//! rows are recorded here, not "fixed" by trimming the roster.

use beam_bifs::bif_tab::{read_real_bif_tab, BifTab};
use beam_bifs::{snifs, Snif};

/// The number of `bif.tab` declarations this tree has, and therefore the roster's length.
///
/// The live oracle answers **553** (`= 556 − 3`): see [`OTP30_ONLY_ROWS`].
const EXPECTED_SNIFS: usize = 556;

/// The three `bif.tab` rows this tree (OTP 30) declares that an OTP-29 oracle does not.
///
/// `bif.tab`'s own `# New in 30.` block (`erts/emulator/beam/bif.tab`, the last three
/// rows). They are *data about the oracle's release*, not something to remove from the
/// roster.
const OTP30_ONLY_ROWS: &[(&str, &str, u32)] = &[
    ("erlang", "pid_to_binary", 1),
    ("erlang", "port_to_binary", 1),
    ("erlang", "ref_to_binary", 1),
];

/// The roster as `(module, function, arity)` triples, in oracle order.
fn roster() -> Vec<(&'static str, &'static str, u32)> {
    snifs()
        .map(|Snif { module, function, arity }| (module, function, arity))
        .collect()
}

/// The roster in `bif.tab` **file** order (what `src/snifs.rs` stores).
fn roster_in_table_order() -> Vec<(&'static str, &'static str, u32)> {
    roster().into_iter().rev().collect()
}

// ---------------------------------------------------------------------------
// 1. The generated data
// ---------------------------------------------------------------------------

#[test]
fn the_roster_has_one_row_per_declared_bif() {
    assert_eq!(snifs().len(), EXPECTED_SNIFS);
    assert_eq!(snifs().count(), EXPECTED_SNIFS);
    // `snifs()` is the reversing accessor, so it is double-ended both ways.
    assert_eq!(snifs().rev().count(), EXPECTED_SNIFS);
    let table_order = roster_in_table_order();
    assert_eq!(table_order.len(), EXPECTED_SNIFS);
}

/// `build_snifs_term` conses the C `bif_table` forward, so the oracle's list is the
/// `bif.tab` roster reversed: it starts with the *last* declarations of the file and ends
/// with `erlang:abs/1`, the file's first row.
#[test]
fn the_roster_is_in_the_oracles_order() {
    let rows = roster();
    assert_eq!(
        &rows[..5],
        &[
            ("erlang", "ref_to_binary", 1),
            ("erlang", "port_to_binary", 1),
            ("erlang", "pid_to_binary", 1),
            ("records", "get_definition", 2),
            ("records", "update", 4),
        ],
        "the answer is `bif.tab` in REVERSE file order (the C conses forward)"
    );
    assert_eq!(
        &rows[rows.len() - 5..],
        &[
            ("erlang", "atom_to_list", 1),
            ("erlang", "adler32_combine", 3),
            ("erlang", "adler32", 2),
            ("erlang", "adler32", 1),
            ("erlang", "abs", 1),
        ]
    );
    // The table order is the exact mirror, so the accessor really is the reversal.
    let table_order = roster_in_table_order();
    assert_eq!(table_order[0], ("erlang", "abs", 1));
    assert_eq!(
        table_order[table_order.len() - 1],
        ("erlang", "ref_to_binary", 1)
    );
}

/// `is_snif_term` (`erl_bif_info.c:2743`) drops a row whose **module** atom name contains
/// a `.`. `gen_snifs.escript` asserts the absence at generation time instead of emitting a
/// filter that could never fire, so the roster must stay dotted-free.
#[test]
fn no_row_names_a_dotted_module() {
    for (module, function, arity) in roster() {
        assert!(
            !module.contains('.'),
            "`{module}:{function}/{arity}` names a dotted module, which \
             `is_snif_term` would drop — regenerate the roster via `gen_snifs.escript`"
        );
        // A name is never quoted: the generator strips the quotes of an operator.
        assert!(
            !function.starts_with('\'') && !function.ends_with('\''),
            "`{module}:{function}/{arity}` kept its quotes; only an operator's quotes are \
             stripped, and the name itself never carries them"
        );
    }
    // The quoted operators are present, spelled as atoms: this is what makes the roster a
    // faithful `{Module, Name, Arity}` list rather than a textual echo of `bif.tab`.
    for operator in [
        "=:=", "/=", "+", "-", "*", "/", "!", "++", "--", "and", "or", "xor", "<", ">", "=<",
        ">=", "==", "div", "rem", "band", "bor", "bxor", "bsl", "bsr",
    ] {
        assert!(
            roster()
                .iter()
                .any(|r| r.0 == "erlang" && r.1 == operator && r.2 == 2),
            "`erlang:{operator}/2` is missing from the roster (or kept its quotes)"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. `bif.tab` row for row
// ---------------------------------------------------------------------------

#[test]
fn the_roster_matches_the_real_bif_tab_row_for_row() {
    let Some(text) = read_real_bif_tab() else {
        eprintln!(
            "SKIP: erts/emulator/beam/bif.tab is not present in this checkout — the \
             generated roster was not cross-checked against the source table"
        );
        return;
    };
    let tab = BifTab::parse(&text).expect("the committed bif.tab parses");
    let declared: Vec<(&str, &str, u32)> = tab.entries().iter().map(|e| e.key()).collect();
    assert_eq!(
        declared.len(),
        EXPECTED_SNIFS,
        "the table declared {} rows; the roster pins {EXPECTED_SNIFS} — regenerate \
         `src/snifs.rs` with `gen_snifs.escript`",
        declared.len()
    );
    let generated = roster_in_table_order();
    assert_eq!(
        generated.len(),
        declared.len(),
        "the generated roster has {} rows, bif.tab declares {}",
        generated.len(),
        declared.len()
    );
    // Every mismatch lands in one report (index, generated, declared), so a drifted row
    // cannot hide behind the first difference.
    let diverging: Vec<String> = generated
        .iter()
        .zip(&declared)
        .enumerate()
        .filter(|(_, (generated, declared))| generated != declared)
        .map(|(index, (generated, declared))| {
            format!(
                "  [{index}] generated {}:{} / {} vs bif.tab {}:{} / {}",
                generated.0, generated.1, generated.2, declared.0, declared.1, declared.2
            )
        })
        .collect();
    assert!(
        diverging.is_empty(),
        "the generated roster is not bif.tab in file order (regenerate it):\n{}",
        diverging.join("\n")
    );
}

// ---------------------------------------------------------------------------
// 3. A live oracle
// ---------------------------------------------------------------------------

/// `erlang:system_info(snifs)` under the system `erl`, as `(module, function, arity)`
/// triples in the oracle's order — or `None` when no `erl` can be run.
fn oracle_roster() -> Option<Vec<(String, String, u32)>> {
    // One line per row: `atom_to_list/1` prints an atom's **name**, so `'+'` arrives as
    // `+` — the roster's spelling, with the quoting stripped exactly as `bif.tab`'s own
    // quotes are (and with no `~p` quoting/escaping to unpick here).
    let script = "lists:foreach(fun({M,F,A}) -> io:format(\"~s ~s ~p~n\", \
                  [atom_to_list(M),atom_to_list(F),A]) end, \
                  erlang:system_info(snifs)), halt(0).";
    let output = match std::process::Command::new("erl")
        .args(["-noshell", "-eval", script])
        .output()
    {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            eprintln!(
                "SKIP: the live oracle failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` on PATH ({err}) — the roster was not cross-checked");
            return None;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut rows = Vec::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        let (Some(module), Some(function), Some(arity), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            panic!("unparsable oracle line `{line}` (expected `<module> <function> <arity>`)");
        };
        let arity: u32 = arity
            .parse()
            .unwrap_or_else(|_| panic!("the oracle's arity `{arity}` is not a u32"));
        rows.push((module.to_string(), function.to_string(), arity));
    }
    Some(rows)
}

/// The live oracle's list is this roster, in oracle order, with the three OTP-30 rows
/// removed.
///
/// The pin is release-aware rather than a hardcoded `553`: an oracle that already declares
/// the three rows (an OTP-30 oracle) must answer the **whole** roster instead, which is
/// checked as the one alternative — and either way a divergence reports the first differing
/// index with both sides, never a bare length.
#[test]
fn the_live_oracle_matches_the_roster_modulo_the_otp30_rows() {
    let Some(oracle) = oracle_roster() else {
        return;
    };
    let ours: Vec<(&str, &str, u32)> = roster()
        .into_iter()
        .filter(|row| !OTP30_ONLY_ROWS.contains(row))
        .collect();

    if oracle.len() == EXPECTED_SNIFS {
        // An oracle of this tree's own release: nothing to remove.
        let full = roster();
        let diverging: Vec<String> = full
            .iter()
            .zip(oracle.iter())
            .enumerate()
            .filter(|(_, (ours, oracle))| !row_eq(ours, oracle))
            .map(|(index, (ours, oracle))| {
                format!("  [{index}] ours {ours:?} vs oracle {oracle:?}")
            })
            .collect();
        assert_eq!(full.len(), oracle.len(), "row count");
        assert!(
            diverging.is_empty(),
            "the roster is not the oracle's `system_info(snifs)` in order:\n{}",
            diverging.join("\n")
        );
        eprintln!(
            "note: this oracle already declares the three OTP-30 `*_to_binary/1` rows, so \
             the full {EXPECTED_SNIFS}-row roster was compared (no release skew)"
        );
        return;
    }

    assert_eq!(
        oracle.len(),
        ours.len(),
        "the oracle answered {} rows; this roster minus the three OTP-30 rows is {} — \
         a row count difference means bif.tab and the oracle's release disagree by more \
         than the documented skew",
        oracle.len(),
        ours.len()
    );
    let diverging: Vec<String> = ours
        .iter()
        .zip(oracle.iter())
        .enumerate()
        .filter(|(_, (ours, oracle))| !row_eq(ours, oracle))
        .map(|(index, (ours, oracle))| format!("  [{index}] ours {ours:?} vs oracle {oracle:?}"))
        .collect();
    assert!(
        diverging.is_empty(),
        "the roster (minus the OTP-30 rows) is not the oracle's `system_info(snifs)` in \
         order:\n{}",
        diverging.join("\n")
    );
    eprintln!(
        "note: compared {} oracle rows against this roster's {} rows ({EXPECTED_SNIFS} \
         minus the three OTP-30 `*_to_binary/1` rows) — element for element, in order",
        oracle.len(),
        ours.len()
    );
}

/// Whether a roster row equals an oracle row (the oracle's fields are owned `String`s).
fn row_eq(ours: &(&str, &str, u32), oracle: &(String, String, u32)) -> bool {
    ours.0 == oracle.0 && ours.1 == oracle.1 && ours.2 == oracle.2
}
