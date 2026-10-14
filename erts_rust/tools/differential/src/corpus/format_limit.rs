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

//! Contract **A.7** — `io_lib:format/3`'s `~ts` under a character limit.
//!
//! `~ts` renders the *Unicode* text of its argument (a binary is decoded from UTF-8, a
//! charlist is taken as code points), so a program's output depends on both the decoding
//! and — when the format is limited — on exactly how the tail is elided. The limit is the
//! **third** argument of `io_lib:format/3`: an option list (here `[{chars_limit, N}]`),
//! *not* an element of the arguments list. Unlimited (`io_lib:format/2`) prints the whole
//! character sequence; a `{chars_limit, N}` elides it.
//!
//! Every probe flattens the (possibly deep) result (`lists:flatten/1`), so the compared
//! stdout line is the exact code-point sequence the program would see.
//!
//! # The elision rule (pinned by the goldens, `lib/stdlib/src/io_lib_format.erl`)
//! For a plain `~ts` (no field width) with `{chars_limit, N}`, `io_lib_format` keeps
//! `max(0, N - 3)` **graphemes**, then looks at what remains: four or more graphemes are
//! replaced by exactly the three dots `...`; fewer than four are kept verbatim. Two
//! consequences are observable and pinned here:
//!
//! * a *short* subject can survive elision intact even though `N` is below its length
//!   (`"abc"` at `N = 2` prints `"abc"`, because only three graphemes remain at the cut);
//! * the count is **graphemes, not bytes** — a 5-grapheme/7-byte subject (`"ab€cd"`) fits
//!   `N = 5`, and an elided head always ends on a whole code point (`"€bcdef"` at `N = 4`
//!   starts with the full `€`, never a split byte).
//!
//! The explicit **field-width** form `~<W>ts` combines with the limit through
//! `limit_field(W, N) = max(3, min(W, N))` (`io_lib_format:limit_field/2`), so `~0ts`
//! under `{chars_limit, 4}` clamps the field to 3 whole characters and appends **no**
//! dots (`"hel"`). `chars_limit` also drives the non-`~ts` controls (`~p`/`~w`), which is
//! why the last case pins a `~p` tail.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.7 `~ts` / `chars_limit` cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "format/ts_binary_unlimited",
            description: "unlimited ~ts of a UTF-8 binary prints the whole character sequence",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<\"hello\">>]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [104,101,108,108,111]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_charlist_unlimited",
            description: "unlimited ~ts of a charlist prints the whole character sequence",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[\"hello\"]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [104,101,108,108,111]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_chars_limit_zero",
            description: "~ts under {chars_limit, 0} of \"hello\" is just the three dots",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<\"hello\">>],[{chars_limit,0}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [46,46,46]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_chars_limit_one",
            description: "~ts under {chars_limit, 1} of \"hello\" is just the three dots",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<\"hello\">>],[{chars_limit,1}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [46,46,46]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_chars_limit_two",
            description: "~ts under {chars_limit, 2} of \"hello\" is just the three dots",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<\"hello\">>],[{chars_limit,2}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [46,46,46]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_chars_limit_partial",
            description: "~ts under {chars_limit, 4} of \"hello\" keeps one grapheme then the dots",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<\"hello\">>],[{chars_limit,4}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [104,46,46,46]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_chars_limit_keeps_limit_minus_3",
            description: "~ts under {chars_limit, 6} of a 10-char subject keeps exactly 3 graphemes",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<\"abcdefghij\">>],[{chars_limit,6}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [97,98,99,46,46,46]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_chars_limit_short_string_not_elided",
            description: "~ts under {chars_limit, 2} of \"abc\" survives intact (fewer than 4 graphemes remain)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<\"abc\">>],[{chars_limit,2}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [97,98,99]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_chars_limit_equal_length",
            description: "~ts under {chars_limit, 5} of the 5-char \"hello\" is not elided",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<\"hello\">>],[{chars_limit,5}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [104,101,108,108,111]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_chars_limit_above_length",
            description: "~ts under {chars_limit, 10} of the 5-char \"hello\" is not elided",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<\"hello\">>],[{chars_limit,10}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [104,101,108,108,111]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_field_width_and_chars_limit",
            description: "an explicit ~0ts field clamps to max(3,min(0,4)) characters and appends no dots",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~0ts\",[<<\"hello\">>],[{chars_limit,4}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [104,101,108]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_grapheme_limit_within",
            description: "a 5-grapheme/7-byte subject fits {chars_limit, 5}: the limit counts graphemes, not bytes",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<97,98,226,130,172,99,100>>],[{chars_limit,5}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [97,98,8364,99,100]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_grapheme_limit_elide",
            description: "the same multibyte subject under {chars_limit, 4} keeps one grapheme then the dots",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<97,98,226,130,172,99,100>>],[{chars_limit,4}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [97,46,46,46]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_multibyte_head_elide",
            description: "an elided head ends on a whole code point: \"€bcdef\" at {chars_limit, 4} keeps the full €",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<226,130,172,98,99,100,101,102>>],[{chars_limit,4}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [8364,46,46,46]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_mixed_ts_s_args",
            description: "a mixed ~ts/~s format with two arguments, unlimited",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts/~s\",[<<\"ab\">>,<<\"cd\">>]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [97,98,47,99,100]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_mixed_with_chars_limit",
            description: "a mixed ~ts~s format under {chars_limit, 5} fits both arguments with no dots",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts~s\",[<<\"ab\">>,<<\"cd\">>],[{chars_limit,5}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [97,98,99,100]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/chars_limit_non_ts_control",
            description: "chars_limit also drives a non-~ts control: ~p of a list under {chars_limit, 10}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~p\",[[1,2,3,4,5,6,7,8]],[{chars_limit,10}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [91,49,44,50,44,51,124,46,46,46,93]"),
                exit_code: 0,
            },
        },
        // --- widened: the N = 3 floor, a charlist subject, `~s`, and a wider field --------
        Case {
            id: "format/ts_chars_limit_three_is_dots",
            description: "~ts under {chars_limit, 3} keeps max(0, 3-3) = 0 graphemes, so \"hello\" is just the dots",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[<<\"hello\">>],[{chars_limit,3}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [46,46,46]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_charlist_chars_limit_partial",
            description: "a *charlist* subject is elided by the same rule: \"abcdefghij\" at {chars_limit, 8} keeps five then dots",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~ts\",[\"abcdefghij\"],[{chars_limit,8}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [97,98,99,100,101,46,46,46]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/s_control_chars_limit",
            description: "the bytes-oriented ~s control is elided by the same chars_limit rule",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~s\",[<<\"hello\">>],[{chars_limit,4}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [104,46,46,46]"),
                exit_code: 0,
            },
        },
        Case {
            id: "format/ts_field_width_five_with_chars_limit",
            description: "a ~5ts field wider than the subject prints the whole 5-char subject with no dots",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:flatten(io_lib:format(\"~5ts\",[<<\"hello\">>],[{chars_limit,10}]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [104,101,108,108,111]"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::{CaseKind, Contract, Probe};
    use std::collections::HashSet;

    /// Every case id in this group, in declaration order.
    const IDS: [&str; 21] = [
        "format/ts_binary_unlimited",
        "format/ts_charlist_unlimited",
        "format/ts_chars_limit_zero",
        "format/ts_chars_limit_one",
        "format/ts_chars_limit_two",
        "format/ts_chars_limit_partial",
        "format/ts_chars_limit_keeps_limit_minus_3",
        "format/ts_chars_limit_short_string_not_elided",
        "format/ts_chars_limit_equal_length",
        "format/ts_chars_limit_above_length",
        "format/ts_field_width_and_chars_limit",
        "format/ts_grapheme_limit_within",
        "format/ts_grapheme_limit_elide",
        "format/ts_multibyte_head_elide",
        "format/ts_mixed_ts_s_args",
        "format/ts_mixed_with_chars_limit",
        "format/chars_limit_non_ts_control",
        "format/ts_chars_limit_three_is_dots",
        "format/ts_charlist_chars_limit_partial",
        "format/s_control_chars_limit",
        "format/ts_field_width_five_with_chars_limit",
    ];

    fn case(id: &str) -> crate::corpus::Case {
        group()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no case {id}"))
    }

    fn golden(id: &str) -> &'static str {
        case(id).golden.stdout.expect("a golden stdout")
    }

    #[test]
    fn the_group_is_compact_and_every_case_is_an_otp_apps_value_probe() {
        let cases = group();
        assert_eq!(cases.len(), IDS.len(), "a case count change is deliberate");
        assert!(cases.len() <= 24, "keep this group compact");
        let mut seen = HashSet::new();
        for id in IDS {
            assert!(seen.insert(id), "duplicate id {id}");
            let case = case(id);
            assert!(case.id.contains('/'), "{id} is not <group>/<name>");
            assert!(!case.description.is_empty(), "{id} has no description");
            assert_eq!(case.contracts, &[Contract::OtpApps], "{id}");
            // A flattened `~w` render of the char list is the compared observable.
            assert_eq!(case.kind_label(), "value", "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
            assert!(
                case.golden.stdout.is_some_and(|s| !s.is_empty()),
                "{id}: empty golden"
            );
            assert!(
                matches!(
                    case.kind,
                    CaseKind::Eval {
                        probe: Probe::Value,
                        ..
                    }
                ),
                "{id}: expected the value probe"
            );
        }
    }

    #[test]
    fn every_case_ids_are_prefixed_and_probe_io_lib_format() {
        for id in IDS {
            assert!(id.starts_with("format/"), "{id}");
            assert!(
                case(id)
                    .expr()
                    .expect("an eval expression")
                    .contains("io_lib:format"),
                "{id} does not call io_lib:format"
            );
        }
    }

    #[test]
    fn the_unlimited_goldens_print_the_whole_character_sequence() {
        // A binary and a charlist of the same text render identically ("hello").
        assert_eq!(
            golden("format/ts_binary_unlimited"),
            "ok [104,101,108,108,111]"
        );
        assert_eq!(
            golden("format/ts_charlist_unlimited"),
            "ok [104,101,108,108,111]"
        );
        // No third argument at all ⇒ no elision.
        assert!(!case("format/ts_binary_unlimited")
            .expr()
            .unwrap()
            .contains("chars_limit"));
    }

    #[test]
    fn the_elision_goldens_pin_the_keep_n_minus_3_then_dots_rule() {
        // `{chars_limit, N}` is the THIRD argument, never an element of the args list.
        for id in [
            "format/ts_chars_limit_zero",
            "format/ts_chars_limit_one",
            "format/ts_chars_limit_two",
            "format/ts_chars_limit_partial",
            "format/ts_chars_limit_keeps_limit_minus_3",
            "format/ts_chars_limit_equal_length",
            "format/ts_chars_limit_above_length",
        ] {
            let expr = case(id).expr().unwrap();
            assert!(
                expr.contains("],[{chars_limit,"),
                "{id}: the limit must be the third argument: {expr}"
            );
        }
        // N = 0/1/2 ⇒ keep none, then the dots.
        let dots = "ok [46,46,46]";
        for id in [
            "format/ts_chars_limit_zero",
            "format/ts_chars_limit_one",
            "format/ts_chars_limit_two",
        ] {
            assert_eq!(golden(id), dots, "{id}");
        }
        // N = 4 ⇒ keep max(0, 4-3) = 1 grapheme ("h") then the dots.
        assert_eq!(golden("format/ts_chars_limit_partial"), "ok [104,46,46,46]");
        // N = 6 ⇒ keep 3 graphemes ("abc") then the dots.
        assert_eq!(
            golden("format/ts_chars_limit_keeps_limit_minus_3"),
            "ok [97,98,99,46,46,46]"
        );
        // N at/above the length ⇒ no elision at all.
        let full = "ok [104,101,108,108,111]";
        assert_eq!(golden("format/ts_chars_limit_equal_length"), full);
        assert_eq!(golden("format/ts_chars_limit_above_length"), full);
    }

    #[test]
    fn a_short_subject_survives_a_below_length_limit_intact() {
        // "abc" at N = 2: only 3 graphemes remain at the cut (< 4), so they are kept.
        assert_eq!(
            golden("format/ts_chars_limit_short_string_not_elided"),
            "ok [97,98,99]"
        );
    }

    #[test]
    fn the_grapheme_goldens_pin_grapheme_not_byte_counting() {
        // "ab€cd" is 5 graphemes but 7 UTF-8 bytes: it fits N = 5 unchanged (the code
        // points, not the bytes, are counted) yet is elided at N = 4.
        assert_eq!(
            golden("format/ts_grapheme_limit_within"),
            "ok [97,98,8364,99,100]"
        );
        assert_eq!(golden("format/ts_grapheme_limit_elide"), "ok [97,46,46,46]");
        // The elided head keeps the whole multibyte grapheme (€ = 8364), never a split byte.
        assert_eq!(
            golden("format/ts_multibyte_head_elide"),
            "ok [8364,46,46,46]"
        );
        // Both multibyte cases use an explicit UTF-8 byte literal for the subject.
        for id in [
            "format/ts_grapheme_limit_within",
            "format/ts_grapheme_limit_elide",
            "format/ts_multibyte_head_elide",
        ] {
            assert!(
                case(id).expr().unwrap().contains("<<"),
                "{id}: expected a binary subject"
            );
        }
    }

    #[test]
    fn the_remaining_goldens_pin_the_field_width_and_non_ts_controls() {
        // ~0ts clamps the field to max(3, min(0, 4)) = 3 whole characters, no dots.
        assert_eq!(
            golden("format/ts_field_width_and_chars_limit"),
            "ok [104,101,108]"
        );
        assert!(case("format/ts_field_width_and_chars_limit")
            .expr()
            .unwrap()
            .starts_with("lists:flatten(io_lib:format(\"~0ts\""));
        // A mixed multi-argument format, unlimited and with a limit that fits both.
        assert_eq!(golden("format/ts_mixed_ts_s_args"), "ok [97,98,47,99,100]");
        assert_eq!(
            golden("format/ts_mixed_with_chars_limit"),
            "ok [97,98,99,100]"
        );
        // chars_limit of a non-~ts control (~p): tail elided at `|...`.
        assert_eq!(
            golden("format/chars_limit_non_ts_control"),
            "ok [91,49,44,50,44,51,124,46,46,46,93]"
        );
    }
}
