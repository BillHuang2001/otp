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

//! The `-eval` binary-segment **match** reader and its oracle table.
//!
//! Two rules of the `<<…>>` **pattern** side are pinned here against the system Erlang
//! oracle, because both are easy to get subtly wrong and impossible to see from a
//! construction-only test:
//!
//! * **A little-endian integer segment is read byte-wise, with a trailing partial field
//!   entering *above* the whole bytes** — the exact inverse of the construction path
//!   ([`BitBuf::push_integer_bits`] with `little = true`). The 12-bit field of
//!   `<<212,14:4>>` is `3796` (`16#0E bsl 8 bor 16#D4`), not the byte reversal `57556`.
//! * **A literal string is one segment per character** — expanded in the parser
//!   ([`super::parse`]) so construction, matching and the comprehensions all see
//!   `<<"abc">>` as `<<97,98,99>>`, and `<<"abc"/utf16>>` as three utf16 segments.
//!
//! The reader is the production implementation: [`read_little_endian_bits`] is what
//! [`super::read_bits_le`] delegates to, so it is the one the evaluator's `<= 64`-bit
//! integer pattern path (`super::integer_segment_term`) actually runs — there is no
//! second copy to drift.
//!
//! Each test case is checked **twice**: against this runtime's own `-eval` and — when a
//! live `erl` is on `PATH` — against the system Erlang, with one hardcoded expectation
//! that both must print. A missing toolchain is a **loud skip**, never a silent pass: the
//! hardcoded expectation still has to hold for this runtime.

/// Reads `n` bits (`n <= 64`) starting at bit `start` of `bytes` as a **little-endian**
/// unsigned integer: `n / 8` whole bytes, least significant byte first, plus — when
/// `n % 8 != 0` — the remaining `n % 8` bits taken from the *low* bits of the next byte
/// and pushed as an `n % 8`-bit field, i.e. entering the value **above** the whole bytes.
///
/// That partial field is exactly how the writer laid it out
/// ([`BitBuf::push_integer_bits`] with `little = true` writes it into the *high* bits of
/// the last byte), so this reader is the writer's exact inverse: `<<212,14:4>>` read as
/// `<<A:12/little>>` is `16#0E bsl 8 bor 16#D4` = 3796, while the byte-aligned
/// `<<0,1>>` as `<<A:16/little>>` is still 256. A width of at most 8 bits is unaffected by
/// `little` — its single partial field *is* the whole value — exactly as the oracle's own
/// reader treats it (`erts_bs_get_integer_2`'s `num_bits <= 8` branch).
///
/// `read_bits_be` masks its own width, so the byte-aligned path never consults a partial
/// field.
pub(super) fn read_little_endian_bits(bytes: &[u8], start: usize, n: usize) -> u64 {
    let whole_bytes = n / 8;
    let partial_bits = n % 8;
    let mut value = if partial_bits == 0 {
        0
    } else {
        super::read_bits_be(bytes, start + 8 * whole_bytes, partial_bits)
    };
    for index in (0..whole_bytes).rev() {
        value = (value << 8) | super::read_bits_be(bytes, start + 8 * index, 8);
    }
    value
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::process::Command;

    use crate::config::RuntimeConfig;
    use crate::context::RuntimeContext;
    use crate::eval::eval;
    use crate::runtime::Runtime;

    use super::super::{mask_bits, read_bits_le, BitBuf};
    use super::read_little_endian_bits;

    /// One probe: the expression, and the exact single line both runtimes must print.
    ///
    /// The line is the corpus' canonical shape — `ok <value>` for a value, `exc <class>
    /// <reason>` for a raise — so the same expectation is meaningful for both runtimes
    /// and for the differential harness.
    type Case = (&'static str, &'static str);

    /// The source **both** runtimes are handed: every case is evaluated inside the
    /// `(fun() -> … end)()` wrapper (`erl_eval` cannot evaluate a `case` or a binary
    /// pattern directly inside a `try` body — it exits 1 with empty stdout), and the
    /// result is printed on one line.
    ///
    /// `halt` appends the `halt(0)` the oracle probe needs to terminate; this runtime's
    /// `-eval` returns when the comma sequence ends, so it is omitted there (a `halt`
    /// would only turn the evaluation into its uncatchable fatal flow).
    fn probe_source(cases: &[Case], halt: bool) -> String {
        let mut source = String::from(
            "Emit = fun(F) -> try F() of V -> io:format(\"ok ~w~n\", [V]) \
             catch C:R -> io:format(\"exc ~w ~w~n\", [C,R]) end end",
        );
        for (expr, _) in cases {
            source.push_str(&format!(", Emit(fun() -> {expr} end)"));
        }
        if halt {
            source.push_str(", halt(0)");
        }
        source.push_str(", ok");
        source
    }

    /// Everything **this** runtime's `-eval` printed for `cases`, one line per case.
    fn this_runtime(cases: &[Case]) -> String {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::BytesSink::new();
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let source = probe_source(cases, false);
        eval(&mut ctx, &source).unwrap_or_else(|error| panic!("`{source}` failed: {error}"));
        drop(ctx);
        String::from_utf8_lossy(sink.contents()).into_owned()
    }

    /// Everything the system `erl` printed for `cases`, or `None` — with a loud skip —
    /// when no `erl` is on `PATH`.
    fn the_oracle(cases: &[Case]) -> Option<String> {
        let source = probe_source(cases, true);
        let output = match Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&source)
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                eprintln!(
                    "SKIP: no erl on PATH — the {} binary-segment match cases below are only \
                     checked against this runtime's own hardcoded expectations, not \
                     cross-checked against the system Erlang",
                    cases.len()
                );
                return None;
            }
            Err(error) => panic!("running the erl oracle: {error}"),
        };
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The same hardcoded expectation has to hold for this runtime **and** for the
    /// oracle, so the table cannot rot.
    fn cross_check(cases: &[Case]) {
        assert_printed(this_runtime(cases), cases, "this runtime");
        let Some(oracle) = the_oracle(cases) else {
            return;
        };
        assert_printed(oracle, cases, "the oracle");
    }

    /// Asserts `output` holds one line per case, each the case's expectation.
    fn assert_printed(output: String, cases: &[Case], who: &str) {
        let lines: Vec<&str> = output.lines().collect();
        for (index, (expr, expected)) in cases.iter().enumerate() {
            let printed = lines.get(index).copied().unwrap_or("<no output>");
            assert_eq!(
                printed, *expected,
                "{who}: `{expr}` (all output: {output:?})"
            );
        }
        assert_eq!(
            lines.len(),
            cases.len(),
            "{who}: expected one line per case (all output: {output:?})"
        );
    }

    /// The reader is the **exact inverse** of the construction path: for every width the
    /// writer's own little-endian image (`BitBuf::push_integer_bits` with
    /// `little = true`) reads back as the value that was written.
    ///
    /// Purely structural — no oracle needed — and it is what makes the non-byte-aligned
    /// widths (`n % 8 != 0`) provably symmetric rather than merely spot-checked. The
    /// production entry point [`read_bits_le`] is asserted to agree with the reader it
    /// delegates to, so the writer's inverse is the one the evaluator actually runs.
    #[test]
    fn the_little_endian_reader_inverts_the_writer() {
        for n_bits in 1..=64usize {
            for value in [0u64, 1, 2, 255, 256, 0x1234, 0xAAAA_AAAA_AAAA_AAAA] {
                let limbs = [value];
                let mut buf = BitBuf::default();
                buf.push_integer_bits(&limbs, false, n_bits, true);
                let expected = value & mask_bits(n_bits);
                assert_eq!(
                    read_little_endian_bits(&buf.bytes, 0, n_bits),
                    expected,
                    "width {n_bits}, value {value:#x}"
                );
                assert_eq!(
                    read_bits_le(&buf.bytes, 0, n_bits),
                    expected,
                    "width {n_bits}, value {value:#x} (through `read_bits_le`)"
                );
            }
        }
        // The oracle's own non-byte-aligned image: `<<-300:12/little>>` is `<<212,14:4>>`,
        // whose 12 bits are `16#ED4` = 3796 — *not* the byte reversal `16#E0D4`.
        assert_eq!(read_little_endian_bits(&[212, 0b1110_0000], 0, 12), 3796);
        assert_eq!(read_bits_le(&[212, 0b1110_0000], 0, 12), 3796);
    }

    /// A little-endian integer pattern segment of a width that is not a whole number of
    /// bytes reads the value's whole bytes least significant first and its trailing
    /// `n % 8` bits *above* them, and does not disturb the byte-aligned reads.
    #[test]
    fn little_endian_integer_segments_match_the_oracle() {
        cross_check(&[
            // 12 bits: one whole byte, then a 4-bit field above it.
            ("case <<212,14:4>> of <<A:12/little>> -> A end", "ok 3796"),
            ("case <<52,2:4>> of <<A:12/little>> -> A end", "ok 564"),
            ("case <<1,2:4>> of <<A:12/little>> -> A end", "ok 513"),
            // 9 bits: one whole byte, then a 1-bit field above it.
            ("case <<0,1:1>> of <<A:9/little>> -> A end", "ok 256"),
            // Byte-aligned widths are unchanged (the regression this fix must not cause).
            ("case <<0,1>> of <<A:16/little>> -> A end", "ok 256"),
            ("case <<1,2,3>> of <<A:24/little>> -> A end", "ok 197121"),
            ("case <<1,2,3,4>> of <<A:32/little>> -> A end", "ok 67305985"),
            (
                "case <<255>> of <<A:8/signed-little>> -> A end",
                "ok -1",
            ),
            (
                "case <<255,255>> of <<A:16/signed-little>> -> A end",
                "ok -1",
            ),
            // At most 8 bits the whole value *is* the single partial field, so `little`
            // does not change it (the oracle's own reader takes the same branch).
            ("case <<10:4>> of <<A:4/little>> -> A end", "ok 10"),
            ("case <<10:4>> of <<A:4>> -> A end", "ok 10"),
        ]);
    }

    /// A literal string in a binary is one 8-bit integer segment per character, so a
    /// `<<"abc">>` pattern is a three-segment pattern — and every other specifier is
    /// kept on each expanded segment.
    #[test]
    fn a_string_literal_is_one_segment_per_character() {
        cross_check(&[
            // Matching (the gap the expansion closes): `no` was the old answer everywhere.
            (
                "case <<\"abc\">> of <<\"abc\">> -> yes; _ -> no end",
                "ok yes",
            ),
            (
                "case <<65,66,67>> of <<\"ABC\">> -> yes; _ -> no end",
                "ok yes",
            ),
            (
                "case <<1,\"abc\">> of <<X:8,\"abc\">> -> X end",
                "ok 1",
            ),
            (
                "case <<\"abc\",1>> of <<\"abc\", X:8>> -> X end",
                "ok 1",
            ),
            // A shorter binary does not match three segments, and an empty string is no
            // segment at all (it matches only the empty binary).
            (
                "case <<97,98>> of <<\"abc\">> -> yes; _ -> no end",
                "ok no",
            ),
            (
                "case <<\"\">> of <<\"\">> -> yes; _ -> no end",
                "ok yes",
            ),
            ("<<\"\">>", "ok <<>>"),
            // Construction is unchanged, and the expansion keeps the segment's specifiers:
            // a string with an explicit size/type is one segment per character on the
            // oracle too (`eval_bits.erl`'s string bin element is a per-character
            // comprehension), which is why `<<"abc":16>>` is three 16-bit segments.
            ("<<\"abc\">>", "ok <<97,98,99>>"),
            ("<<\"abc\"/integer>>", "ok <<97,98,99>>"),
            ("<<\"abc\":16>>", "ok <<0,97,0,98,0,99>>"),
            (
                "begin N = 16, <<\"abc\":N>> end",
                "ok <<0,97,0,98,0,99>>",
            ),
            (
                "<<\"abc\"/float>>",
                "ok <<64,88,64,0,0,0,0,0,64,88,128,0,0,0,0,0,64,88,192,0,0,0,0,0>>",
            ),
            ("<<\"abc\":16/float>>", "ok <<86,16,86,32,86,48>>"),
            // A binary-comprehension template keeps the same parser path.
            ("<< <<C>> || C <- \"ab\" >>", "ok <<97,98>>"),
            // A **non-literal** string value must stay the oracle's `badarg`: the
            // expansion is a parse-time rule for a literal, not a licence to coerce a list.
            (
                "begin S = \"abc\", <<S/utf16>> end",
                "exc error badarg",
            ),
            // A bitstring-typed segment wants a bitstring value, so a string is `badarg`
            // before and after the expansion (each expanded segment raises the same way).
            ("<<\"abc\"/binary>>", "exc error badarg"),
            ("<<\"abc\"/bitstring>>", "exc error badarg"),
        ]);
    }

    /// A `utf8`/`utf16`/`utf32`-typed string expands to one *encoded* segment per
    /// character, and the default expansion truncates each character to 8 bits.
    ///
    /// The **pattern** side of a utf-typed segment is a separate, pre-existing typed gap:
    /// `case <<0,97,0,98,0,99>> of <<"abc"/utf16>> -> yes end` reaches
    /// `match_binary_segments`' `_ =>` arm (`unsupported: this binary pattern segment
    /// type`) while the oracle prints `ok yes`. The expansion does not regress it — a
    /// `<<X/utf16>>` pattern segment was already unsupported — and closing it belongs to
    /// the match-path work, not to the parser.
    #[test]
    fn utf_string_segments_expand_per_character() {
        cross_check(&[
            ("<<\"abc\"/utf8>>", "ok <<97,98,99>>"),
            ("<<\"abc\"/utf16>>", "ok <<0,97,0,98,0,99>>"),
            ("<<\"abc\"/utf16-little>>", "ok <<97,0,98,0,99,0>>"),
            ("<<\"abc\"/utf32>>", "ok <<0,0,0,97,0,0,0,98,0,0,0,99>>"),
            // An escaped character is a code point: the default segment truncates it to
            // its low byte, the utf types encode it.
            ("<<\"\\xff\">>", "ok <<255>>"),
            ("<<\"\\xff\"/utf8>>", "ok <<195,191>>"),
            ("<<\"\\x{100}\">>", "ok <<0>>"),
            ("<<\"\\x{100}\"/utf8>>", "ok <<196,128>>"),
            ("<<\"\\xe5\"/utf8>>", "ok <<195,165>>"),
        ]);
    }

    /// `unit:` still resolves to a `size * unit` **bit** count on both sides of the read
    /// (it is resolved in `Evaluator::segment_size_bits`, which this change does not
    /// touch): the byte-aligned and little-endian `unit` forms are unchanged.
    #[test]
    fn unit_specifiers_are_unaffected() {
        cross_check(&[
            (
                "case <<16#12,16#34>> of <<A:2/unit:8>> -> A end",
                "ok 4660",
            ),
            ("<<1:2/unit:8>>", "ok <<0,1>>"),
            ("<<256:2/unit:8>>", "ok <<1,0>>"),
            ("<<16#1234:2/little-unit:8>>", "ok <<52,18>>"),
            (
                "case <<16#12,16#34>> of <<A:1/little-unit:8>> -> matched; _ -> nomatch end",
                "ok nomatch",
            ),
        ]);
    }
}
