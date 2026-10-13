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

//! Integration tests for the **plain** (unlimited) unicode `~ts` directive of
//! `io_lib:format/2`.
//!
//! `~t` sets the directive's encoding to `unicode`, so `~s` renders through
//! `io_lib_format:cdata_to_chars/1` + `uniconv/1` instead of `iolist_to_chars/1`.
//! The defect this file pins: a **binary** argument used to be rendered as its raw
//! latin1 bytes, so `io_lib:format("~ts",[<<230,151,165>>])` yielded
//! `[[230,151,165]]` where the oracle yields the *decoded* `[[26085]]` — a silent
//! wrong result. The oracle **decodes** the binary as UTF-8
//! (`unicode:characters_to_list/1`) and falls back to `binary_to_list/1` (the raw
//! bytes) only when the image is not valid UTF-8.
//!
//! Every golden below is the literal deep list the system oracle (OTP 29.1.1 /
//! erts-17.1) produced for
//!
//! ```text
//! erl -noshell -eval 'io:format("~w~n",[io_lib:format("~ts", Args)]),halt().'
//! ```
//!
//! captured with `~w` so the *structure* — not the flattened text — is visible;
//! the result is compared with `beam_term::compare::eq`, so a differing nested
//! sub-list is a failure even when the flattened text agrees.
//!
//! The `~ts`-under-`{chars_limit, N}` (grapheme-limited) path is a *separate*
//! algorithm and is deliberately out of scope here.
//!
//! The tests drive only the public API — [`BifRegistry`]/[`BifContext`] plus the
//! declared dependency crates (`beam_alloc` for the heap, `beam_codec::term` for
//! the binary/bitstring/bignum constructors and the list readers).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{
    bitstring_bytes, cons_head, cons_tail, make_big, make_binary, make_bitstring,
};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{is_nil, is_small, signed_value, AtomTable, Eterm};

/// The high limb of `2^100`; its `ErtsDigit` limbs (least significant first) are
/// `[0, 2^36]`.
const BIG_POW2_100_HIGH: u64 = 1u64 << 36;

/// The atoms the fixtures seed: the `{a,b}` tuple argument, the atom `abc`, and
/// `badarg` (the term a raising case's result is encoded as for the oracle
/// comparison).
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["abc", "a", "b", "badarg"]);
    atoms
}

/// A small integer on `ctx`'s heap.
fn int(ctx: &mut BifContext<'_>, value: i64) -> Eterm {
    ctx.int(value).expect("a small int fits")
}

/// A byte-aligned binary `<<bytes>>` on `ctx`'s heap.
fn binary(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    make_binary(ctx.heap(), bytes).expect("the binary fits")
}

/// A proper list of the (already built) `items` on `ctx`'s heap.
fn list(ctx: &mut BifContext<'_>, items: &[Eterm]) -> Eterm {
    ctx.list(items).expect("a short list fits")
}

/// Builds `[item, …]` on `ctx`, evaluating each item expression first so an item
/// may itself build a term (avoiding the two simultaneous `&mut ctx` borrows a
/// nested call would need).
macro_rules! lst {
    ($ctx:expr, [$($item:expr),* $(,)?]) => {{
        let items = vec![$($item),*];
        list($ctx, &items)
    }};
}

/// A seeded atom on `ctx`'s heap.
fn atom(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    ctx.atom(name).expect("the atom is seeded")
}

/// The bignum `2^100`.
fn big(ctx: &mut BifContext<'_>) -> Eterm {
    make_big(ctx.heap(), false, &[0, BIG_POW2_100_HIGH]).expect("the bignum fits")
}

/// The expected nested-list shape of an `io_lib:format` result.
///
/// `I(c)` is one integer element, `Big(limbs)` a bignum element, `S(text)` a flat
/// run of ASCII characters and `L(items)` a sub-list element — the shapes
/// `io_lib_format` builds.
enum E {
    /// One small-integer element.
    I(i64),
    /// A bignum element, given as its `ErtsDigit` limbs (least significant first).
    Big(&'static [u64]),
    /// A flat character list (a run of individual ASCII characters).
    S(&'static str),
    /// A sub-list element.
    L(Vec<E>),
}

/// Materializes an expected shape as a term on `ctx`'s heap.
fn build(ctx: &mut BifContext<'_>, e: &E) -> Eterm {
    match e {
        E::I(value) => int(ctx, *value),
        E::Big(limbs) => make_big(ctx.heap(), false, limbs).expect("the bignum fits"),
        E::S(text) => {
            let elems: Vec<Eterm> = text.bytes().map(|b| int(ctx, i64::from(b))).collect();
            list(ctx, &elems)
        }
        E::L(items) => {
            let elems: Vec<Eterm> = items.iter().map(|item| build(ctx, item)).collect();
            list(ctx, &elems)
        }
    }
}

/// What the oracle returns for one case.
enum Expect {
    /// The oracle's deep list.
    Deep(E),
    /// The oracle raises `error:badarg` (`io_lib:format/2` wraps every raiser in
    /// `io_lib_format` into `error:badarg`).
    BadArg,
}

/// One pinned case: the argument list described once as a Rust builder (used by
/// both the committed goldens and the live-oracle cross-check) and the oracle's
/// expected outcome.
struct Case {
    /// The case's name, used in failure messages.
    name: &'static str,
    /// Builds the *argument list* of the single `~ts` argument on the heap.
    build: fn(&mut BifContext<'_>) -> Vec<Eterm>,
    /// The oracle's expected result.
    expect: Expect,
}

/// The whole pinned corpus — §2.4 (the decoded rows) and §2.5 (the `badarg` rows)
/// of `HANDOFF_io_ts_chars_limit.md`, each re-verified against the live oracle
/// before this file was written.
fn cases() -> Vec<Case> {
    vec![
        // ---- §2.4: the decoded rows ------------------------------------------
        Case {
            name: "binary/utf8-kou",
            build: |c| vec![binary(c, &[230, 151, 165])],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::I(26085)])])),
        },
        Case {
            name: "binary/utf8-kou-kou",
            build: |c| vec![binary(c, &[230, 151, 165, 230, 151, 165])],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::I(26085), E::I(26085)])])),
        },
        Case {
            name: "binary/ascii",
            build: |c| vec![binary(c, b"ab")],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::I(97), E::I(98)])])),
        },
        Case {
            name: "binary/invalid-utf8-falls-back-to-bytes",
            build: |c| vec![binary(c, &[200, 201])],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::I(200), E::I(201)])])),
        },
        Case {
            name: "binary/cesu8-lone-surrogate-falls-back-to-bytes",
            build: |c| vec![binary(c, &[237, 160, 128])],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::I(237), E::I(160), E::I(128)])])),
        },
        Case {
            name: "binary/astral-emoji",
            build: |c| vec![binary(c, &[240, 159, 152, 128])],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::I(128512)])])),
        },
        Case {
            name: "binary/truncated-trailing-sequence-falls-back-to-bytes",
            build: |c| vec![binary(c, &[230, 151, 165, 230])],
            expect: Expect::Deep(E::L(vec![E::L(vec![
                E::I(230),
                E::I(151),
                E::I(165),
                E::I(230),
            ])])),
        },
        Case {
            name: "binary/overlong-sequence-falls-back-to-bytes",
            build: |c| vec![binary(c, &[193, 129])],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::I(193), E::I(129)])])),
        },
        Case {
            name: "binary/empty",
            build: |c| vec![binary(c, b"")],
            expect: Expect::Deep(E::L(vec![E::L(vec![])])),
        },
        Case {
            name: "list/binary-element-decoded-and-nested",
            build: |c| vec![lst!(c, [binary(c, &[230, 151, 165]), int(c, 100)])],
            expect: Expect::Deep(E::L(vec![E::L(vec![
                E::L(vec![E::I(26085)]),
                E::I(100),
            ])])),
        },
        Case {
            name: "list/binary-and-charlist-elements-nested",
            build: |c| {
                vec![lst!(
                    c,
                    [
                        binary(c, b"ab"),
                        lst!(c, [int(c, 99), int(c, 100)])
                    ]
                )]
            },
            expect: Expect::Deep(E::L(vec![E::L(vec![
                E::L(vec![E::I(97), E::I(98)]),
                E::L(vec![E::I(99), E::I(100)]),
            ])])),
        },
        Case {
            name: "list/code-points",
            build: |c| vec![lst!(c, [int(c, 26085), int(c, 26412), int(c, 35486)])],
            expect: Expect::Deep(E::L(vec![E::L(vec![
                E::I(26085),
                E::I(26412),
                E::I(35486),
            ])])),
        },
        Case {
            name: "list/400",
            build: |c| vec![lst!(c, [int(c, 400)])],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::I(400)])])),
        },
        Case {
            name: "list/1114112",
            build: |c| vec![lst!(c, [int(c, 1114112)])],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::I(1114112)])])),
        },
        Case {
            name: "list/surrogate-integer-is-emitted-verbatim",
            build: |c| vec![lst!(c, [int(c, 55296)])],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::I(55296)])])),
        },
        Case {
            name: "list/bignum-is-emitted-verbatim",
            build: |c| vec![lst!(c, [big(c)])],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::Big(&[0, BIG_POW2_100_HIGH])])])),
        },
        Case {
            name: "list/bignum-between-integers",
            build: |c| vec![lst!(c, [int(c, 97), big(c), int(c, 98)])],
            expect: Expect::Deep(E::L(vec![E::L(vec![
                E::I(97),
                E::Big(&[0, BIG_POW2_100_HIGH]),
                E::I(98),
            ])])),
        },
        Case {
            name: "list/nested-empty-list",
            build: |c| vec![lst!(c, [lst!(c, [])])],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::L(vec![])])])),
        },
        Case {
            name: "list/empty",
            build: |c| vec![lst!(c, [])],
            expect: Expect::Deep(E::L(vec![E::L(vec![])])),
        },
        Case {
            name: "deep/binary-element-two-levels",
            build: |c| {
                vec![lst!(
                    c,
                    [lst!(c, [binary(c, &[230, 151, 165]), int(c, 100)])]
                )]
            },
            expect: Expect::Deep(E::L(vec![E::L(vec![E::L(vec![
                E::L(vec![E::I(26085)]),
                E::I(100),
            ])])])),
        },
        Case {
            name: "deep/binary-and-charlist-two-levels",
            build: |c| {
                vec![lst!(
                    c,
                    [lst!(
                        c,
                        [binary(c, b"ab"), lst!(c, [int(c, 99), int(c, 100)])]
                    )]
                )]
            },
            expect: Expect::Deep(E::L(vec![E::L(vec![E::L(vec![
                E::L(vec![E::I(97), E::I(98)]),
                E::L(vec![E::I(99), E::I(100)]),
            ])])])),
        },
        Case {
            name: "deep/code-points-two-levels",
            build: |c| {
                vec![lst!(
                    c,
                    [lst!(c, [int(c, 26085), int(c, 26412), int(c, 35486)])]
                )]
            },
            expect: Expect::Deep(E::L(vec![E::L(vec![E::L(vec![
                E::I(26085),
                E::I(26412),
                E::I(35486),
            ])])])),
        },
        Case {
            name: "deep/empty-list-two-levels",
            build: |c| vec![lst!(c, [lst!(c, [lst!(c, [])])])],
            expect: Expect::Deep(E::L(vec![E::L(vec![E::L(vec![E::L(vec![])])])])),
        },
        Case {
            name: "atom/stays-a-small-control-and-flat",
            build: |c| vec![atom(c, "abc")],
            expect: Expect::Deep(E::S("abc")),
        },
        // ---- §2.5: the `badarg` rows -----------------------------------------
        Case {
            name: "badarg/negative-integer-element",
            build: |c| vec![lst!(c, [int(c, 97), int(c, -1), int(c, 98)])],
            expect: Expect::BadArg,
        },
        Case {
            name: "badarg/negative-bignum-element",
            build: |c| {
                let negative =
                    make_big(c.heap(), true, &[0, BIG_POW2_100_HIGH]).expect("the bignum fits");
                vec![lst!(c, [int(c, 97), negative, int(c, 98)])]
            },
            expect: Expect::BadArg,
        },
        Case {
            name: "badarg/float-element",
            build: |c| {
                let f = c.make_float(1.5).expect("the float fits");
                vec![lst!(c, [f])]
            },
            expect: Expect::BadArg,
        },
        Case {
            name: "badarg/improper-list-tail",
            build: |c| {
                let head = int(c, 97);
                let tail = int(c, 98);
                vec![c.cons(head, tail).expect("a cons fits")]
            },
            expect: Expect::BadArg,
        },
        Case {
            name: "badarg/non-byte-aligned-bitstring",
            build: |c| vec![make_bitstring(c.heap(), &[0b0010_0000], 3).expect("fits")],
            expect: Expect::BadArg,
        },
        Case {
            name: "badarg/integer-argument",
            build: |c| vec![int(c, 97)],
            expect: Expect::BadArg,
        },
        Case {
            name: "badarg/tuple-argument",
            build: |c| {
                let a = atom(c, "a");
                let b = atom(c, "b");
                vec![c.tuple(&[a, b]).expect("a short tuple fits")]
            },
            expect: Expect::BadArg,
        },
    ]
}

/// The format term: a flat proper list of the byte-sized integers of `bytes`.
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let elems: Vec<Eterm> = bytes.iter().map(|&b| int(ctx, i64::from(b))).collect();
    ctx.list(&elems).expect("a short format list fits")
}

/// Runs the plain `io_lib:format("~ts", Args)` through the real registry and
/// returns its result term.
fn format_plain_ts(ctx: &mut BifContext<'_>, reg: &BifRegistry, args: &[Eterm]) -> Result<Eterm> {
    let format_term = byte_list(ctx, b"~ts");
    let args_term = ctx.list(args).expect("a short argument list fits");
    reg.call("io_lib", "format", 2, ctx, &[format_term, args_term])
}

/// Asserts the committed goldens: every case renders the oracle's deep list (or
/// raises `badarg`).
///
/// This runs with **no** oracle — the goldens are the literal oracle output, so
/// the behaviour is pinned even in a sandbox without `erl`.
#[test]
fn plain_ts_matches_the_committed_oracle_goldens() {
    let reg = default_registry();
    let cases = cases();
    assert!(!cases.is_empty(), "the pinned `~ts` corpus must not be empty");

    for case in &cases {
        let mut heap = Heap::new(1 << 20).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let args = (case.build)(&mut ctx);
        let got = format_plain_ts(&mut ctx, &reg, &args);

        match &case.expect {
            Expect::BadArg => match got {
                Err(BeamError::BadArg) => {}
                other => panic!("case `{}`: expected badarg, got {other:?}", case.name),
            },
            Expect::Deep(expected) => {
                let got = got.unwrap_or_else(|e| {
                    panic!(
                        "case `{}`: io_lib:format(\"~ts\", …) failed: {e:?}",
                        case.name
                    )
                });
                let want = build(&mut ctx, expected);
                assert!(
                    beam_term::compare::eq(got, want, ctx.atoms()),
                    "case `{}`: io_lib:format(\"~ts\", …) does not match the \
                     committed oracle deep list",
                    case.name
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The live-oracle cross-check (byte-exact, printer-free; fails loudly)
// ---------------------------------------------------------------------------

/// The lowercase hex of `term`'s `erlang:term_to_binary/1` image.
fn term_to_binary_hex(ctx: &mut BifContext<'_>, reg: &BifRegistry, term: Eterm) -> String {
    let image = reg
        .call("erlang", "term_to_binary", 1, ctx, &[term])
        .expect("erlang:term_to_binary/1");
    let (bytes, offset, size_bits) = bitstring_bytes(image).expect("a binary result");
    assert_eq!(offset, 0, "the image is a whole-byte binary");
    assert_eq!(size_bits % 8, 0, "the image is whole-byte aligned");
    let data = &bytes[..size_bits / 8];
    let mut out = String::with_capacity(data.len() * 2);
    for &byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Flattens an `io_lib:format` result (a nested char list of small integers) to
/// its text — used to read the `~w` rendering of an argument list back as Erlang
/// source.
fn flatten_chars(term: Eterm) -> String {
    let mut out = String::new();
    let mut cursor = term;
    loop {
        if is_nil(cursor) {
            return out;
        }
        let head = cons_head(cursor).expect("a proper list of chars");
        if is_small(head) {
            let byte = u8::try_from(signed_value(head)).expect("a byte-sized char");
            out.push(byte as char);
        } else {
            out.push_str(&flatten_chars(head));
        }
        cursor = cons_tail(cursor).expect("a proper list of chars");
    }
}

/// The Erlang source of an argument list, derived from the **same term** the Rust
/// half renders by `io_lib:format("~w", [Args])` — so the two halves of a case
/// cannot describe different terms, and a case's Erlang and Rust descriptions
/// cannot drift apart silently.
fn erl_source(ctx: &mut BifContext<'_>, reg: &BifRegistry, args: &[Eterm]) -> String {
    let format_term = byte_list(ctx, b"~w");
    let args_term = ctx.list(args).expect("a short argument list fits");
    let wrapped = ctx.list(&[args_term]).expect("a short argument list fits");
    let rendered = reg
        .call("io_lib", "format", 2, ctx, &[format_term, wrapped])
        .expect("io_lib:format(\"~w\", [Args])");
    flatten_chars(rendered)
}

/// The `erl -eval` body for the corpus: one lowercase-hex line per case, the hex of
/// `erlang:term_to_binary/1` of the call's result (or of the atom `badarg` when the
/// call raises — `try … catch error:badarg -> badarg end`, so a raising case is
/// byte-comparable without a stack trace).
///
/// Every argument list is ASCII source (a binary is written as `<<230,151,165>>`),
/// so the eval string never carries a raw non-ASCII byte; a literal non-ASCII byte
/// would corrupt the eval.
fn oracle_eval(sources: &[String]) -> String {
    let mut body = String::from(
        "Hex = fun(B) -> lists:flatten([io_lib:format(\"~2.16.0b\", [X]) \
         || X <- binary_to_list(B)]) end, ",
    );
    for source in sources {
        body.push_str(&format!(
            "io:format(\"~s~n\", [Hex(erlang:term_to_binary((try io_lib:format(\"~ts\", \
             {source}) catch error:badarg -> badarg end)))]), ",
        ));
    }
    body.push_str("halt().");
    body
}

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH` — the
/// same resolution `tools/differential` uses.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs io_lib ~ts] $ERTS_ORACLE='{}' is not an existing executable.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Runs one `erl -noshell -eval <eval>` (the eval must `halt()`) and returns stdout.
fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// Re-derives the whole corpus against the **live** oracle and requires the
/// `erlang:term_to_binary/1` image of every result to agree byte-for-byte.
///
/// A missing or failing oracle is a **failure**, never a silent pass: this test
/// exists to catch the exact class of defect `~ts` had (a silent wrong result), so
/// skipping it when no oracle is installed would re-create that blindness.
#[test]
fn plain_ts_cross_checks_the_live_oracle() {
    let oracle = resolve_oracle().unwrap_or_else(|| {
        panic!(
            "the live-oracle cross-check for `~ts` requires a system `erl` \
             ($ERTS_ORACLE naming an executable, or one on $PATH). Refusing to pass \
             silently: install Erlang/OTP or set $ERTS_ORACLE."
        )
    });

    let reg = default_registry();
    let cases = cases();
    assert!(
        !cases.is_empty(),
        "the live-oracle `~ts` corpus must not be empty"
    );

    // Build every case's arguments, derive the Erlang source of the very same
    // terms, and render the crate's own result — one fresh heap per case (so the
    // term images stay independent).
    let mut sources = Vec::with_capacity(cases.len());
    let mut ours = Vec::with_capacity(cases.len());
    for case in &cases {
        let mut heap = Heap::new(1 << 20).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let args = (case.build)(&mut ctx);
        sources.push(erl_source(&mut ctx, &reg, &args));
        let term = match format_plain_ts(&mut ctx, &reg, &args) {
            Ok(term) => term,
            Err(BeamError::BadArg) => atom(&mut ctx, "badarg"),
            Err(other) => panic!("case `{}`: unexpected error {other:?}", case.name),
        };
        ours.push(term_to_binary_hex(&mut ctx, &reg, term));
    }

    let eval = oracle_eval(&sources);
    let stdout = run_erl(&oracle, &eval);
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();

    assert_eq!(
        lines.len(),
        cases.len(),
        "the oracle printed {} non-empty lines for {} cases — the generated \
         `-eval` did not run every case:\n{stdout}",
        lines.len(),
        cases.len(),
    );

    for (case, (got, want)) in cases.iter().zip(ours.iter().zip(lines.iter())) {
        assert!(
            !want.trim().is_empty(),
            "case `{}`: the oracle line is empty",
            case.name
        );
        assert_eq!(
            got,
            want.trim(),
            "case `{}` disagrees with the live oracle (erlang:term_to_binary image)",
            case.name
        );
    }
}
