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

//! The `~w`/`~p` **atom-quoting rule**, cross-checked against the system `erl`.
//!
//! The printer's atom leaf is the oracle's `io_lib:write_atom_as_latin1/1` (the
//! latin1 clause of `write1/4`, which `~w`/`~W`/`~p`/`~P` all take). Its bareness
//! decision is `io_lib:quote_atom/2` + `name_char/1` (`lib/stdlib/src/io_lib.erl`)
//! — **not** the `~p` printable-latin1 predicate: a space, `16#A0`, a leading
//! digit or a leading uppercase letter are printable yet still force quotes,
//! while `héllo` is bare. The whole latin1 range is therefore ambiguous for a
//! hand-written table; this file pins it against the live oracle instead.
//!
//! The probe is
//!
//! ```text
//! erl -noshell -eval 'io:format("~w~n",[lists:flatten(io_lib:format("~w",[A]))]),halt().'
//! ```
//!
//! per corpus atom (built with `binary_to_atom(unicode:characters_to_binary(C),
//! utf8)`, so the probe is independent of this crate's atom construction), plus
//! the same corpus as containers for the `~w`/`~p` nesting paths. The crate side
//! calls the registered `io_lib:format/2` BIF and flattens through the registered
//! `lists:flatten/1`, so the comparison is on the oracle's **character codes**,
//! not on any UTF-8 text rendering.
//!
//! With no `erl` on `PATH` the cross-check skips loudly (the pinned goldens
//! below still run).

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{cons_head, cons_tail};
use beam_process::ProcessId;
use beam_term::{is_list, is_nil, signed_value, AtomTable, Eterm};

/// One atom per row, as the **code points** of its name.
///
/// The rows cover every decision the rule makes: the ASCII name characters, the
/// two non-ASCII name ranges with their two exclusions (`÷`/`×`), the
/// non-ASCII initials, the control/whitespace set, `16#7F`, the C1 range,
/// `16#A0`, a space, a leading digit/uppercase, the empty name, the quote and
/// backslash, reserved words, and code points above `16#FF`.
const CORPUS: &[&[u32]] = &[
    &[],
    &[97],
    &[97, 98, 99],
    &[120, 49, 95, 64, 122],
    &[104, 233, 108, 108, 111], // héllo — bare
    &[99, 97, 102, 233],        // café — bare
    &[224, 98, 99],             // àbc — bare
    &[223],                     // ß — bare
    &[255],                     // ÿ — bare
    &[248],                     // ø — bare
    &[246],                     // ö — bare
    &[247],                     // ÷ — quoted (excluded from ß..ÿ)
    &[97, 247],                 // a÷ — quoted
    &[97, 215, 98],             // a×b — quoted (excluded from À..Þ)
    &[97, 216, 98],             // aØb — bare
    &[72, 101, 108, 108, 111],  // Hello — quoted
    &[95, 102, 111, 111],       // _foo — quoted
    &[49, 120],                 // 1x — quoted
    &[97, 45, 98],              // a-b — quoted
    &[97, 46, 98],              // a.b — quoted
    &[32, 97],                  // " a" — quoted (printable, still quoted)
    &[97, 32, 98],              // "a b" — quoted
    &[97, 39, 98],              // a'b — quoted, `'` escaped
    &[97, 92, 98],              // a\b — quoted, `\` escaped
    &[97, 34, 98],              // a"b — quoted, `"` NOT escaped
    &[0],                       // NUL — octal escape
    &[1],
    &[8],
    &[9],
    &[10],
    &[11],
    &[12],
    &[13],
    &[14],
    &[27],
    &[31],
    &[97, 127, 98], // 16#7F — `\d`
    &[128],
    &[97, 159, 98], // 16#9F — octal
    &[160],         // 16#A0 — verbatim (printable, still quoted)
    &[97, 160, 98],
    &[97, 168, 98], // 16#A8 verbatim
    &[97, 173, 98], // soft hyphen, verbatim
    &[97, 215, 247, 255],
    // Reserved words.
    &[97, 110, 100], // and
    &[101, 110, 100],  // end
    &[102, 117, 110],  // fun
    &[102, 117, 110, 110, 121],
    &[109, 97, 121, 98, 101], // maybe
    &[101, 108, 115, 101],    // else
    &[114, 101, 99],          // rec — not reserved, bare
    &[113, 117, 101, 114, 121], // query — bare
    // Above latin1: `\x{H}`.
    &[256], // Ā
    &[97, 256],
    &[300], // Ĭ
    &[2047],
    &[128512], // 😀
    &[1114111],
];

/// The Rust name of a corpus row.
fn name_of(cps: &[u32]) -> String {
    cps.iter()
        .map(|&c| char::from_u32(c).expect("the corpus holds no surrogate"))
        .collect()
}

/// The Erlang source of the atom list `L` (one code-point list per corpus row).
fn corpus_literal() -> String {
    CORPUS
        .iter()
        .map(|cps| {
            let codes: Vec<String> = cps.iter().map(|c| c.to_string()).collect();
            format!("[{}]", codes.join(","))
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Runs one `erl -noshell -eval` statement; `None` (with a loud message) when
/// the oracle is unavailable, so the cross-check skips rather than fails.
fn oracle(statement: &str) -> Option<String> {
    match Command::new("erl")
        .args(["-noshell", "-eval", statement])
        .output()
    {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

/// Parses one `[105,233,…]` oracle line into its integer codes.
fn parse_codes(line: &str) -> Vec<i64> {
    let line = line.trim();
    let body = line
        .strip_prefix('[')
        .and_then(|l| l.strip_suffix(']'))
        .unwrap_or_else(|| panic!("the oracle line {line:?} is not an integer list"));
    if body.is_empty() {
        return Vec::new();
    }
    body.split(',')
        .map(|part| {
            part.trim()
                .parse::<i64>()
                .unwrap_or_else(|_| panic!("the oracle line {line:?} is not an integer list"))
        })
        .collect()
}

/// Flattens a rendered term through the registered `lists:flatten/1` and reads
/// the character codes back out.
fn codes(ctx: &mut BifContext<'_>, reg: &BifRegistry, rendered: Eterm) -> Vec<i64> {
    let flat = reg
        .call("lists", "flatten", 1, ctx, &[rendered])
        .expect("lists:flatten/1");
    let mut out = Vec::new();
    let mut cur = flat;
    while is_list(cur) {
        out.push(signed_value(cons_head(cur).expect("a cons")));
        cur = cons_tail(cur).expect("a cons");
    }
    assert!(is_nil(cur), "lists:flatten/1 returned an improper list");
    out
}

/// The crate's flattened `io_lib:format(Format, [Term])`.
fn rendered(ctx: &mut BifContext<'_>, reg: &BifRegistry, format: &[u8], term: Eterm) -> Vec<i64> {
    let format_term = byte_list(ctx, format);
    let args = ctx.list(&[term]).expect("a short argument list fits");
    let deep = reg
        .call("io_lib", "format", 2, ctx, &[format_term, args])
        .unwrap_or_else(|err| panic!("io_lib:format failed: {err:?}"));
    codes(ctx, reg, deep)
}

/// A flat proper list of the byte-sized integers of `bytes`.
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let elems: Vec<Eterm> = bytes
        .iter()
        .map(|&b| ctx.int(i64::from(b)).expect("a character code fits"))
        .collect();
    ctx.list(&elems).expect("a short format list fits")
}

/// The regression itself: `héllo` is **bare** (the old rule quoted it).
#[test]
fn hello_with_a_non_ascii_letter_is_bare() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let mut atoms = AtomTable::new();
    atoms.seed(["héllo", "_foo"]);
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // ORACLE: `io_lib:format("~w",['héllo'])` => [104,233,108,108,111] — BARE.
    // (The rule before this fix produced [39,104,233,108,108,111,39].)
    let hello = ctx.atom("héllo").expect("seeded");
    assert_eq!(
        rendered(&mut ctx, &reg, b"~w", hello),
        vec![104, 233, 108, 108, 111]
    );
    // ORACLE: `io_lib:format("~p",['héllo'])` => [104,233,108,108,111] as well.
    assert_eq!(
        rendered(&mut ctx, &reg, b"~p", hello),
        vec![104, 233, 108, 108, 111]
    );

    // A name that really needs quotes still gets them.
    // ORACLE: `io_lib:format("~w",['_foo'])` => "'_foo'".
    let underscore = ctx.atom("_foo").expect("seeded");
    assert_eq!(
        rendered(&mut ctx, &reg, b"~w", underscore),
        vec![39, 95, 102, 111, 111, 39]
    );
}

/// The whole corpus, per atom, against the live oracle (`~w`).
#[test]
fn atom_quoting_matches_the_live_oracle() {
    let statement = format!(
        "L=[{}], [io:format(\"~w~n\",[lists:flatten(io_lib:format(\"~w\",\
         [binary_to_atom(unicode:characters_to_binary(C),utf8)]))]) || C <- L], halt().",
        corpus_literal()
    );
    let Some(out) = oracle(&statement) else {
        return;
    };
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines.len(),
        CORPUS.len(),
        "the oracle printed {} lines for {} corpus rows",
        lines.len(),
        CORPUS.len()
    );

    let names: Vec<String> = CORPUS.iter().map(|cps| name_of(cps)).collect();
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let mut atoms = AtomTable::new();
    atoms.seed(names.iter().map(String::as_str));
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    for (i, name) in names.iter().enumerate() {
        let want = parse_codes(lines[i]);
        let term = ctx.atom(name).expect("the corpus name is seeded");
        let got = rendered(&mut ctx, &reg, b"~w", term);
        assert_eq!(
            got, want,
            "~w of the atom {name:?} (code points {:?})",
            CORPUS[i]
        );
    }
}

/// The corpus as containers, through `~w` and the `~p` pretty path.
///
/// `~w` sees the whole corpus tuple (never re-flowed); the `~p` probes use the
/// first ten rows, whose rendering stays far inside one 80-column line, so no
/// `io_lib_pretty` line breaking is involved.
#[test]
fn nested_atoms_match_the_live_oracle() {
    let statement = format!(
        "L=[{}], A=fun(C) -> binary_to_atom(unicode:characters_to_binary(C),utf8) end, \
         Big=list_to_tuple([A(C) || C <- L]), Small=list_to_tuple([A(C) || C <- lists:sublist(L,10)]), \
         SmallList=[A(C) || C <- lists:sublist(L,10)], \
         [io:format(\"~w~n\",[lists:flatten(X)]) || X <- \
           [io_lib:format(\"~w\",[Big]), io_lib:format(\"~w\",[[Big,a,nested]]), \
            io_lib:format(\"~p\",[Small]), io_lib:format(\"~p\",[SmallList]), \
            io_lib:format(\"~w\",[Small])]], halt().",
        corpus_literal()
    );
    let Some(out) = oracle(&statement) else {
        return;
    };
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 5, "the oracle printed {} lines", lines.len());

    let names: Vec<String> = CORPUS.iter().map(|cps| name_of(cps)).collect();
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let mut atoms = AtomTable::new();
    atoms.seed(names.iter().map(String::as_str));
    atoms.seed(["a", "nested"]);
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let terms: Vec<Eterm> = names
        .iter()
        .map(|name| ctx.atom(name).expect("the corpus name is seeded"))
        .collect();
    let big = ctx.tuple(&terms).expect("the corpus tuple fits");
    let small = ctx.tuple(&terms[..10]).expect("the small tuple fits");
    let small_list = ctx.list(&terms[..10]).expect("the small list fits");
    let a = ctx.atom("a").expect("seeded");
    let nested = ctx.atom("nested").expect("seeded");
    let big_list = ctx.list(&[big, a, nested]).expect("fits");

    let probes: [(&[u8], Eterm, &str); 5] = [
        (b"~w", big, "~w of the corpus tuple"),
        (b"~w", big_list, "~w of [Big,a,nested]"),
        (b"~p", small, "~p of the ten-atom tuple"),
        (b"~p", small_list, "~p of the ten-atom list"),
        (b"~w", small, "~w of the ten-atom tuple"),
    ];
    for (i, (format, term, what)) in probes.into_iter().enumerate() {
        let want = parse_codes(lines[i]);
        let got = rendered(&mut ctx, &reg, format, term);
        assert_eq!(got, want, "{what}");
    }
}
