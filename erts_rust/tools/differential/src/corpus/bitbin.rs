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
//! Contract **A.1/A.2/A.7** — the **opcode** half of the bit-string surface: the writable
//! binary accumulator and `bs_match` over a **refc** subject.
//!
//! Every case here is a [`CaseKind::Module`] case: the module is compiled by the system
//! `erlc` and executed by both runtimes on the **opcode** path, so what is pinned is the
//! lowered `bs_init_writable`/`bs_private_append`/`bs_create_bin`/`bs_match` machinery — not
//! the `-eval` evaluator, which is what the [`super::bits`] group covers. Three surfaces had
//! no corpus coverage at all before this group:
//!
//! * **the writable accumulator** (`bs_init_writable` + a `private_append` accumulator, i.e.
//!   a binary comprehension and `<<Acc/binary, Seg/binary>>`). `bitbin/writable_accumulator_
//!   quadruples_a_16k_refc_subject` is the `binary_append` benchmark shape: a 16 KiB input
//!   (two 8 KiB halves of a repeating byte pattern) byte-quadrupled through
//!   `<< <<C:8,C:8,C:8,C:8>> || <<C:8>> <= Bin >>` — and, because the generator's subject is
//!   that 16 KiB refc binary, it pins a refc-subject `bs_match` at offset 0 as well.
//!   `bitbin/writable_accumulator_grows_past_its_reservation` appends 4-byte segments until
//!   the payload passes 64 KiB (17 000 × 4 = 68 000 bytes), so the accumulator's *growth
//!   ladder* — several payload reallocations — is pinned byte for byte.
//! * **a refc subject that is not at offset 0 and not byte-aligned** (the inline arm's
//!   `base = payload + (start + pos)/8`, `bit_pos = (start + pos)%8` arithmetic). A 208-byte
//!   refc binary is re-matched from a 3-bit-offset window, and integers are extracted at
//!   several sizes/units *and* several in-window offsets
//!   (`bitbin/refc_subject_bit_offset_window_units`); a byte-aligned offset-0 companion
//!   (`bitbin/refc_subject_byte_aligned_baseline`) keeps the two arithmetics apart, because a
//!   wrong arm still reads the right values on a byte-aligned window.
//! * **binary/bitstring fields taken out of that unaligned window**
//!   (`bitbin/refc_subject_bit_offset_binary_fields`) — a `4/binary` field at bit 11, a
//!   `180/binary` field at bit 24 (both force a copy, since neither is byte-aligned relative
//!   to the payload) and a `37/bitstring` field at bit 19, plus the byte-aligned
//!   `100/binary` + rest split of the same refc binary and a `binary:part/3` window.
//!
//! `ets_copy/large_payload_round_trip` widens the same subject material to the **ETS copy**
//! boundary: a 4 KiB refc binary and a 2 000-element list go in through `ets:insert/2` and
//! come back out through `ets:lookup/2`, nested inside a list in a third object.
//!
//! **What a case asserts.** Every function returns one integer built by two helpers, so a
//! single wrong byte is caught and not merely a wrong length:
//! `stamp(Bin)` = `binary:decode_unsigned(<<(byte_size(Bin)):32, (md5(Bin))/binary>>)` — the
//! byte count *and* the MD5 image of the produced binary in one number; `checksum(Vs)` =
//! `binary:decode_unsigned(md5(term_to_binary(Vs)))` — an MD5 over the exact value list, so
//! each extracted field contributes to the pinned integer. Both sides print the integer with
//! `~w`, which is the module-run golden shape.
//!
//! Two things are deliberately **not** pinned: `binary:referenced_byte_size/1` of a
//! sub-binary (a measured divergence — the oracle answers the parent refc payload's size, the
//! runtime the sub-binary's own; see `divergences.md` §A.7), and any `~w` rendering of a
//! binary (the integers above carry the bytes instead).
use super::{Case, CaseKind, Contract, Golden, ModuleRun};

/// The module both runtimes execute. Every exported function is arity 0 and returns an
/// integer; the bit-string/binary work is done on **real lowered opcodes** because this
/// source is `erlc`-compiled, not `-eval`-interpreted.
pub(crate) const BITBIN_SOURCE: &str = "-module(diff_bitbin).\n\
     -export([writable_quad_16k/0, writable_grow_past_64k/0,\n\
     \x20        refc_window_units/0, refc_window_binary/0, refc_byte_aligned/0,\n\
     \x20        ets_large_payload/0]).\n\
     \n\
     writable_quad_16k() ->\n\
     \x20   Bin = <<(binary:copy(<<\"abcdefgh\">>, 1024))/binary,\n\
     \x20           (binary:copy(<<\"0123456789ABCDEF\">>, 512))/binary>>,\n\
     \x20   Out = << <<C:8, C:8, C:8, C:8>> || <<C:8>> <= Bin >>,\n\
     \x20   stamp(Out).\n\
     \n\
     writable_grow_past_64k() ->\n\
     \x20   Out = << <<\"ABCD\">> || _ <- lists:seq(1, 17000) >>,\n\
     \x20   stamp(Out).\n\
     \n\
     refc_window_units() ->\n\
     \x20   Bin = binary:copy(<<\"0123456789ABCDEF\">>, 13),\n\
     \x20   <<_:3, Sub/bits>> = Bin,\n\
     \x20   <<A:13, _/bits>> = Sub,\n\
     \x20   <<_:5, B:32, _/bits>> = Sub,\n\
     \x20   <<_:3, Cc:8, _/bits>> = Sub,\n\
     \x20   <<_:64, D:16, _/bits>> = Sub,\n\
     \x20   <<_:101, E:37, _/bits>> = Sub,\n\
     \x20   <<_:7, F:64/little, _/bits>> = Sub,\n\
     \x20   <<_:3, G:2/unit:8, _/bits>> = Sub,\n\
     \x20   <<_:3, H:3/unit:4, _/bits>> = Sub,\n\
     \x20   checksum([bit_size(Sub), A, B, Cc, D, E, F, G, H]).\n\
     \n\
     refc_window_binary() ->\n\
     \x20   Bin = binary:copy(<<\"0123456789ABCDEF\">>, 13),\n\
     \x20   <<_:3, Sub/bits>> = Bin,\n\
     \x20   <<_:11, Four:4/binary, _/bits>> = Sub,\n\
     \x20   <<_:19, OddBits:37/bitstring, _/bits>> = Sub,\n\
     \x20   <<_:24, Land:180/binary, _/bits>> = Sub,\n\
     \x20   <<Head:100/binary, Rest/binary>> = Bin,\n\
     \x20   Whole = binary:part(Bin, 3, 200),\n\
     \x20   checksum([byte_size(Four), stamp(Four), bit_size(OddBits), byte_size(Land),\n\
     \x20             stamp(Land), byte_size(Head), stamp(Head), byte_size(Rest),\n\
     \x20             stamp(Rest), stamp(Whole), binary:at(Four, 0), binary:at(Four, 3),\n\
     \x20             binary:at(Land, 0), binary:at(Land, 179)]).\n\
     \n\
     refc_byte_aligned() ->\n\
     \x20   Bin = binary:copy(<<\"0123456789ABCDEF\">>, 13),\n\
     \x20   <<A:13, _/bits>> = Bin,\n\
     \x20   <<_:5, B:32, _/bits>> = Bin,\n\
     \x20   <<_:3, Cc:8, _/bits>> = Bin,\n\
     \x20   <<_:101, D:37, _/bits>> = Bin,\n\
     \x20   checksum([bit_size(Bin), A, B, Cc, D]).\n\
     \n\
     ets_large_payload() ->\n\
     \x20   T = ets:new(diff_bitbin_copy, [set, public]),\n\
     \x20   Bin = binary:copy(<<\"0123456789ABCDEF\">>, 256),\n\
     \x20   L = lists:seq(1, 2000),\n\
     \x20   ets:insert(T, {bin, Bin}),\n\
     \x20   ets:insert(T, {list, L}),\n\
     \x20   ets:insert(T, {mixed, [{Bin, L}]}),\n\
     \x20   [{bin, B1}] = ets:lookup(T, bin),\n\
     \x20   [{list, L1}] = ets:lookup(T, list),\n\
     \x20   [{mixed, M1}] = ets:lookup(T, mixed),\n\
     \x20   [{B1x, L1x}] = M1,\n\
     \x20   R = checksum([stamp(B1), length(L1), lists:sum(L1), stamp(B1x), length(L1x),\n\
     \x20                 lists:sum(L1x), byte_size(B1), ets:info(T, size)]),\n\
     \x20   ets:delete(T),\n\
     \x20   R.\n\
     \n\
     stamp(Bin) ->\n\
     \x20   binary:decode_unsigned(<<(byte_size(Bin)):32, (erlang:md5(Bin))/binary>>).\n\
     \n\
     checksum(Vs) ->\n\
     \x20   binary:decode_unsigned(erlang:md5(term_to_binary(Vs))).\n";

/// Build one arity-0 module-run case over [`BITBIN_SOURCE`]; `golden` is the oracle's own
/// `~w` rendering of the returned integer.
fn case(
    id: &'static str,
    description: &'static str,
    contracts: &'static [Contract],
    function: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts,
        kind: CaseKind::Module(ModuleRun {
            source: BITBIN_SOURCE,
            module: "diff_bitbin",
            function,
            arity: 0,
            args: &[],
            nif: None,
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The writable-append / refc-subject bit-string cases plus the ETS copy-path case.
pub(crate) fn group() -> Vec<Case> {
    use Contract::{BeamAsm, LanguageSemantics, OtpApps};
    vec![
        case(
            "bitbin/writable_accumulator_quadruples_a_16k_refc_subject",
            "a binary comprehension quadruples every byte of a 16 KiB refc subject through the writable accumulator; the produced bytes are pinned by size + md5",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "writable_quad_16k",
            "22300967654429177522959035617670801593045144",
        ),
        case(
            "bitbin/writable_accumulator_grows_past_its_reservation",
            "a writable accumulator grown with 4-byte segments past 64 KiB (17 000 appends) keeps every byte across its payload reallocations",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "writable_grow_past_64k",
            "23139235508306107411073743991828937070720994",
        ),
        case(
            "bitbin/refc_subject_bit_offset_window_units",
            "integers matched out of a 3-bit-offset window of a 208-byte refc binary (sizes 8..64, /unit, /little) read the same fields as the oracle",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "refc_window_units",
            "264646129891673483199472759211817017973",
        ),
        case(
            "bitbin/refc_subject_bit_offset_binary_fields",
            "4/binary at bit 11, 180/binary at bit 24 and 37/bitstring at bit 19 out of the unaligned refc window, plus the byte-aligned head/rest split and a binary:part/3 window",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "refc_window_binary",
            "285482099452275104386212523994981257411",
        ),
        case(
            "bitbin/refc_subject_byte_aligned_baseline",
            "the same field reads on the byte-aligned offset-0 refc subject — the companion that separates the window arithmetic from the baseline",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "refc_byte_aligned",
            "267271197144006763654635128194869687172",
        ),
        case(
            "ets_copy/large_payload_round_trip",
            "a 4 KiB refc binary and a 2 000-element list survive the ETS copy-in/copy-out boundary byte for byte, both flat and nested inside a list",
            &[BeamAsm, LanguageSemantics, OtpApps],
            "ets_large_payload",
            "262297552349978947385535486777633574914",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact ids this group emits, in order — a case may not be added or retitled
    /// without happening here (and the group's contract claims are checked by the registry
    /// tests in [`super::super`]).
    const IDS: [&str; 6] = [
        "bitbin/writable_accumulator_quadruples_a_16k_refc_subject",
        "bitbin/writable_accumulator_grows_past_its_reservation",
        "bitbin/refc_subject_bit_offset_window_units",
        "bitbin/refc_subject_bit_offset_binary_fields",
        "bitbin/refc_subject_byte_aligned_baseline",
        "ets_copy/large_payload_round_trip",
    ];

    #[test]
    fn the_group_carries_exactly_these_ids() {
        let cases = group();
        assert_eq!(cases.len(), IDS.len());
        for (case, id) in cases.iter().zip(IDS) {
            assert_eq!(case.id, id);
            assert!(matches!(case.kind, CaseKind::Module(_)));
            assert_eq!(case.kind_label(), "module-run");
            assert!(
                case.golden.stdout.is_some_and(|golden| golden
                    .chars()
                    .all(|c| c.is_ascii_digit())),
                "case {}: the golden is a plain integer line",
                case.id
            );
        }
    }

    /// The source really builds its binaries through the **writable accumulator** and reads
    /// them back with a refc-subject match: if a rewrite of this module dropped either
    /// shape, the case would still be green against the oracle while pinning nothing.
    #[test]
    fn the_source_keeps_the_writable_append_and_refc_match_shapes() {
        assert!(
            BITBIN_SOURCE.contains("<< <<C:8, C:8, C:8, C:8>> || <<C:8>> <= Bin >>"),
            "the quadrupling comprehension is gone"
        );
        assert!(
            BITBIN_SOURCE.contains("<< <<\"ABCD\">> || _ <- lists:seq(1, 17000) >>"),
            "the growth-ladder comprehension is gone"
        );
        assert!(
            BITBIN_SOURCE.contains("<<_:3, Sub/bits>> = Bin")
                && BITBIN_SOURCE.contains("<<_:5, B:32, _/bits>> = Sub")
                && BITBIN_SOURCE.contains("<<_:11, Four:4/binary, _/bits>> = Sub"),
            "the non-byte-aligned refc window reads are gone"
        );
        // The 4 KiB / 2 000-element payloads are what make the ETS case a copy-path case.
        assert!(BITBIN_SOURCE.contains("binary:copy(<<\"0123456789ABCDEF\">>, 256)"));
        assert!(BITBIN_SOURCE.contains("lists:seq(1, 2000)"));
    }

    /// Every case's result is folded through `stamp/1` (size + md5 of the produced bytes) or
    /// `checksum/1` (md5 over the exact value list), so a silently wrong byte cannot pass as
    /// a right length — and the source keeps its inputs a **refc** subject (> 64 bytes).
    #[test]
    fn every_golden_pins_the_bytes_and_not_only_a_length() {
        assert_eq!(
            BITBIN_SOURCE
                .matches("binary:decode_unsigned(erlang:md5(term_to_binary(Vs)))")
                .count(),
            1,
            "the checksum helper changed shape"
        );
        assert!(
            BITBIN_SOURCE
                .contains("binary:decode_unsigned(<<(byte_size(Bin)):32, (erlang:md5(Bin))/binary>>)"),
            "the stamp helper no longer folds the size together with the md5 image"
        );
        // 208 bytes (> 64) and 16 KiB: the subjects really are refc binaries.
        assert!(BITBIN_SOURCE.contains("binary:copy(<<\"0123456789ABCDEF\">>, 13)"));
        assert!(BITBIN_SOURCE.contains("binary:copy(<<\"abcdefgh\">>, 1024)"));
    }

    /// `binary:referenced_byte_size/1` of a sub-binary is a **measured divergence** (the
    /// oracle answers the parent refc payload's size, `beam_rt` the sub-binary's own), so no
    /// case in this group may pin it — doing so would turn a ledger row into a `mismatch`.
    /// See `divergences.md` §A.7.
    #[test]
    fn no_case_pins_referenced_byte_size() {
        for case in group() {
            assert!(
                !BITBIN_SOURCE.contains("referenced_byte_size"),
                "case {}: the source pins binary:referenced_byte_size/1",
                case.id
            );
        }
    }
}
