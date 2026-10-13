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

//! Contract **A.7** — the `re` (regular expression) BIF family.
//!
//! The whole `re` surface the runtime exposes (`re:compile/1,2`, `re:run/2,3`,
//! `re:internal_run/4`, `re:inspect/2`, `re:import/1`, `re:replace/3,4`, `re:version/0`),
//! promoted from the live-oracle case source at `crates/beam_bifs/src/re/oracle.rs`. That
//! source already runs each case twice — under the real system `erl` and through the
//! crate's real dispatch path — and compares the `term_to_binary/1` images; this group
//! makes the same agreement **gate-checked** by the differential harness instead of only
//! crate-tested. Every case is a default [`Probe::TermToBinary`] `-eval` case run under
//! **both** runtimes with identical argv, so it `agree`s only when the Rust runtime's `re`
//! engine returns the byte-identical result term (the result term only — a compiled
//! pattern's opaque 5th payload element is implementation-private and is never pinned).
//!
//! Coverage:
//!
//! * **capture shapes** — `run/2,3` over `{capture, …}` (`none`, an explicit index/name
//!   list, `all`/`all_but_first`/`first`/`all_names`, the `list`/`binary`/`index` value
//!   types), unmatched groups (`-1`), `global`, and `{offset, N}`;
//! * **run options** — `caseless` / `dotall` / `multiline` / `extended` / `ungreedy` /
//!   `firstline` / `no_auto_capture` / `anchored` / `notempty*` / `notbol` / `noteol` /
//!   `dollar_endonly` / `{newline,crlf}` / `report_errors` / `{match_limit,0}`;
//! * **compile errors** — a malformed pattern is a *returned* `{error, {Msg, Pos}}` with a
//!   byte-comparable message and 0-based position, never a raise;
//! * **pattern syntax** — classes, anchors, quantifiers (greedy/lazy/possessive),
//!   backreferences, named/inline-option groups, `\Q…\E`, and a `global` sweep;
//! * **subjects and unicode** — iodata/iolist subjects, an empty subject, non-iodata
//!   refusals, and the `[unicode]` / `/utf8` codepoint-offset surface;
//! * **pre-compiled / run variants** — `re:version/0`, `re:inspect/2` over a compiled
//!   pattern, running a pattern from `re:compile/1,2`, the `[export]` round trip through
//!   `re:import/1`, `re:internal_run/4`, and the `[export]` forms `re:run/2` refuses;
//! * the **`re:replace/3,4`** surface added on top of that source: `[global]`,
//!   `{return,binary}` / `{return,list}`, `caseless`, a `\1` backreference replacement,
//!   and the `replace/3` single-match default.
//!
//! Two further source cases — `unicode/utf8-binary-subject` and
//! `non-unicode/dot-is-a-byte`, both built on the `<<"…"/utf8>>` binary **literal** — are
//! deliberately **not** promoted: the Rust runtime's `-eval` path cannot construct that
//! literal (it raises a plain `error:badarg`), so a case pinning either would be a
//! `mismatch`, not an honest gap. Both are recorded in `divergences.md` under
//! `A.7 — re`; the equivalent byte-sequence subject (`<<195,165,…>>`) *is* evaluable and is
//! covered here by the other subject/unicode cases.
//!
//! # The probe wrapper binds `V` / `C` / `R`
//!
//! The harness wraps every expression in `try (…) of V -> … catch C:R -> … end`, so an
//! expression that *also* binds one of those names makes the oracle's `erl_eval` reject the
//! probe with `unsafe_var` instead of evaluating it (a guaranteed false mismatch — see
//! [`super::driver`] and `super::errors`). The source's `compile/control-caseless-shape`
//! names its projected fourth element `C`, so it is promoted with that one variable renamed
//! `Ca`: a semantics-preserving projection rename (the variable name is not part of the
//! observable). Every other expression is verbatim; the group's tests assert that no other
//! expression binds `V`/`C`/`R` (string literals stripped, so a pattern's `(?<C>C)` is not
//! mistaken for a variable).
//!
//! # Why this file is long
//!
//! Like the other corpus groups it is a **data registry**, not logic: one flat table of `re`
//! case literals (subject, pattern, options, expected image) grown one area at a time.
//! Splitting it would scatter one table and make a duplicate or missing case harder to see,
//! so it stays in one place.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// A default-probe (`term_to_binary`) `re` case that returns a value (`exit 0`).
fn t(id: &'static str, description: &'static str, expr: &'static str, stdout: &'static str) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::OtpApps],
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::TermToBinary,
        },
        golden: Golden {
            stdout: Some(stdout),
            exit_code: 0,
        },
    }
}

/// A default-probe `re` case whose expression raises — the probe catches the class and
/// reason and prints `exception <class> <reason>`, exiting `1`.
fn terr(id: &'static str, description: &'static str, expr: &'static str, stdout: &'static str) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::OtpApps],
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::TermToBinary,
        },
        golden: Golden {
            stdout: Some(stdout),
            exit_code: 1,
        },
    }
}

/// The A.7 `re` (regular expression) group.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // -- promoted live-oracle cases (`crates/beam_bifs/src/re/oracle.rs`) -----------
        t("re/run2_simple", "re capture shapes / global / {offset,N} case `run2/simple`", r#"re:run("xabcy","b")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,2,97,1,106>>"),
        t("re/run2_two-groups", "re capture shapes / global / {offset,N} case `run2/two-groups`", r#"re:run("xabcy","(b)(c)")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,104,2,97,2,97,2,104,2,97,2,97,1,104,2,97,3,97,1,106>>"),
        t("re/capture_none", "re capture shapes / global / {offset,N} case `capture/none`", r#"re:run("xabcy","(b)(c)",[{capture,none}])"#, "ok <<131,119,5,109,97,116,99,104>>"),
        t("re/capture_empty-list", "re capture shapes / global / {offset,N} case `capture/empty-list`", r#"re:run("xabcy","(b)(c)",[{capture,[]}])"#, "ok <<131,119,5,109,97,116,99,104>>"),
        t("re/capture_all-index", "re capture shapes / global / {offset,N} case `capture/all-index`", r#"re:run("xabcy","(b)(c)",[{capture,all,index}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,104,2,97,2,97,2,104,2,97,2,97,1,104,2,97,3,97,1,106>>"),
        t("re/capture_all-list", "re capture shapes / global / {offset,N} case `capture/all-list`", r#"re:run("xabcy","(b)(c)",[{capture,all,list}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,107,0,2,98,99,107,0,1,98,107,0,1,99,106>>"),
        t("re/capture_all-binary", "re capture shapes / global / {offset,N} case `capture/all-binary`", r#"re:run("xabcy","(b)(c)",[{capture,all,binary}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,109,0,0,0,2,98,99,109,0,0,0,1,98,109,0,0,0,1,99,106>>"),
        t("re/capture_all_but_first-index", "re capture shapes / global / {offset,N} case `capture/all_but_first-index`", r#"re:run("xabcy","(b)(c)",[{capture,all_but_first,index}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,2,104,2,97,2,97,1,104,2,97,3,97,1,106>>"),
        t("re/capture_first-index", "re capture shapes / global / {offset,N} case `capture/first-index`", r#"re:run("xabcy","(b)(c)",[{capture,first,index}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,2,97,2,106>>"),
        t("re/capture_first-list", "re capture shapes / global / {offset,N} case `capture/first-list`", r#"re:run("xabcy","(b)(c)",[{capture,first,list}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,107,0,2,98,99,106>>"),
        t("re/capture_nonparticipating-index", "re capture shapes / global / {offset,N} case `capture/nonparticipating-index`", r#"re:run("xabcy","(z)?(b)",[{capture,all,index}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,104,2,97,2,97,1,104,2,98,255,255,255,255,97,0,104,2,97,2,97,1,106>>"),
        t("re/capture_nonparticipating-list", "re capture shapes / global / {offset,N} case `capture/nonparticipating-list`", r#"re:run("xabcy","(z)?(b)",[{capture,all,list}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,107,0,1,98,106,107,0,1,98,106>>"),
        t("re/capture_nonparticipating-binary", "re capture shapes / global / {offset,N} case `capture/nonparticipating-binary`", r#"re:run("xabcy","(z)?(b)",[{capture,all,binary}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,109,0,0,0,1,98,109,0,0,0,0,109,0,0,0,1,98,106>>"),
        t("re/capture_all_names-index", "re capture shapes / global / {offset,N} case `capture/all_names-index`", r#"re:run("xabcy","(?<foo>b)(c)",[{capture,all_names,index}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,2,97,1,106>>"),
        t("re/capture_all_names-list", "re capture shapes / global / {offset,N} case `capture/all_names-list`", r#"re:run("xabcy","(?<foo>b)(c)",[{capture,all_names,list}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,107,0,1,98,106>>"),
        t("re/capture_ids-index", "re capture shapes / global / {offset,N} case `capture/ids-index`", r#"re:run("xabcy","(b)(c)",[{capture,[2,1],index}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,2,104,2,97,3,97,1,104,2,97,2,97,1,106>>"),
        t("re/capture_ids-list", "re capture shapes / global / {offset,N} case `capture/ids-list`", r#"re:run("xabcy","(b)(c)",[{capture,[2,1],list}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,2,107,0,1,99,107,0,1,98,106>>"),
        t("re/capture_ids-binary", "re capture shapes / global / {offset,N} case `capture/ids-binary`", r#"re:run("xabcy","(b)(c)",[{capture,[2,1],binary}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,2,109,0,0,0,1,99,109,0,0,0,1,98,106>>"),
        t("re/capture_out-of-range-id", "re capture shapes / global / {offset,N} case `capture/out-of-range-id`", r#"re:run("xabcy","(b)(c)",[{capture,[5],index}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,98,255,255,255,255,97,0,106>>"),
        t("re/capture_names-mixed", "re capture shapes / global / {offset,N} case `capture/names-mixed`", r#"re:run("xabcy","(?<foo>b)(c)",[{capture,["foo","zz"],list}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,2,107,0,1,98,106,106>>"),
        t("re/nomatch", "re capture shapes / global / {offset,N} case `nomatch`", r#"re:run("xabcy","z")"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        terr("re/badarg_non-iodata-re", "re capture shapes / global / {offset,N} case `badarg/non-iodata-re`", r#"re:run("abc",123)"#, "exception error badarg"),
        t("re/global_bare", "re capture shapes / global / {offset,N} case `global/bare`", r#"re:run("abab","a",[global])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,2,108,0,0,0,1,104,2,97,0,97,1,106,108,0,0,0,1,104,2,97,2,97,1,106,106>>"),
        t("re/global_all-index", "re capture shapes / global / {offset,N} case `global/all-index`", r#"re:run("abab","(a)(b)",[global,{capture,all,index}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,2,108,0,0,0,3,104,2,97,0,97,2,104,2,97,0,97,1,104,2,97,1,97,1,106,108,0,0,0,3,104,2,97,2,97,2,104,2,97,2,97,1,104,2,97,3,97,1,106,106>>"),
        t("re/global_none", "re capture shapes / global / {offset,N} case `global/none`", r#"re:run("abab","a",[global,{capture,none}])"#, "ok <<131,119,5,109,97,116,99,104>>"),
        t("re/global_offset", "re capture shapes / global / {offset,N} case `global/offset`", r#"re:run("abcabc","b",[global,{offset,2}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,108,0,0,0,1,104,2,97,4,97,1,106,106>>"),
        t("re/global_empty-match", "re capture shapes / global / {offset,N} case `global/empty-match`", r#"re:run("abc","x{0}",[global])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,4,108,0,0,0,1,104,2,97,0,97,0,106,108,0,0,0,1,104,2,97,1,97,0,106,108,0,0,0,1,104,2,97,2,97,0,106,108,0,0,0,1,104,2,97,3,97,0,106,106>>"),
        t("re/offset_in-range", "re capture shapes / global / {offset,N} case `offset/in-range`", r#"re:run("abcabc","b",[{offset,2}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,4,97,1,106>>"),
        t("re/offset_at-end", "re capture shapes / global / {offset,N} case `offset/at-end`", r#"re:run("abc","b",[{offset,3}])"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        terr("re/offset_past-end", "re capture shapes / global / {offset,N} case `offset/past-end`", r#"re:run("abc","b",[{offset,5}])"#, "exception error badarg"),
        t("re/binary-subject", "re capture shapes / global / {offset,N} case `binary-subject`", r#"re:run(<<"xabcy">>,"b")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,2,97,1,106>>"),
        t("re/caseless_simple", "re run options case `caseless/simple`", r#"re:run("ABC","abc",[caseless])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/caseless_all-index", "re run options case `caseless/all-index`", r#"re:run("xabcy","(b)(c)",[caseless,{capture,all,index}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,104,2,97,2,97,2,104,2,97,2,97,1,104,2,97,3,97,1,106>>"),
        t("re/caseless_backref", "re run options case `caseless/backref`", r#"re:run("aA","(a)\\1",[caseless])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,2,104,2,97,0,97,2,104,2,97,0,97,1,106>>"),
        t("re/caseless_nomatch", "re run options case `caseless/nomatch`", r#"re:run("ABC","abd",[caseless])"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/dotall_match", "re run options case `dotall/match`", r#"re:run("a\nb","a.b",[dotall])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/dotall_nomatch-without", "re run options case `dotall/nomatch-without`", r#"re:run("a\nb","a.b")"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/multiline_end-anchor", "re run options case `multiline/end-anchor`", r#"re:run("a\nb","b$",[multiline])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,2,97,1,106>>"),
        t("re/multiline_global-caret", "re run options case `multiline/global-caret`", r#"re:run("a\nb\n","^",[global,multiline])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,2,108,0,0,0,1,104,2,97,0,97,0,106,108,0,0,0,1,104,2,97,2,97,0,106,106>>"),
        t("re/dollar_endonly_nomatch", "re run options case `dollar_endonly/nomatch`", r#"re:run("a\nb","a$",[dollar_endonly])"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/extended_whitespace", "re run options case `extended/whitespace`", r#"re:run("abc","a  b  c",[extended])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/extended_class-space", "re run options case `extended/class-space`", r#"re:run("abc","a[ ]b[ ]c",[extended])"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/ungreedy_lazy-default", "re run options case `ungreedy/lazy-default`", r#"re:run("aaa","a+",[ungreedy])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,1,106>>"),
        t("re/ungreedy_flips-explicit-lazy", "re run options case `ungreedy/flips-explicit-lazy`", r#"re:run("aaa","a+?",[ungreedy])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/firstline_nomatch", "re run options case `firstline/nomatch`", r#"re:run("x\naaa","a+",[firstline])"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/no_auto_capture_suppresses-groups", "re run options case `no_auto_capture/suppresses-groups`", r#"re:run("xabcy","(b)(c)",[no_auto_capture])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,2,97,2,106>>"),
        t("re/newline_crlf_multiline-caret", "re run options case `newline_crlf/multiline-caret`", r#"re:run("a\r\nb","^b",[multiline,{newline,crlf}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,3,97,1,106>>"),
        t("re/anchored_nomatch", "re run options case `anchored/nomatch`", r#"re:run("bac","a",[anchored,{capture,all,index}])"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/anchored_match", "re run options case `anchored/match`", r#"re:run("abc","a",[anchored,{capture,all,index}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,1,106>>"),
        t("re/notempty_refuses-empty", "re run options case `notempty/refuses-empty`", r#"re:run("","",[notempty,{capture,all,index}])"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/notempty_atstart_match-later", "re run options case `notempty_atstart/match-later`", r#"re:run("abc","b*",[notempty_atstart,{capture,all,index}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,1,97,1,106>>"),
        t("re/notbol_refuses-start", "re run options case `notbol/refuses-start`", r#"re:run("abc","^a",[notbol])"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/noteol_refuses-end", "re run options case `noteol/refuses-end`", r#"re:run("abc","c$",[noteol])"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/report_errors_match", "re run options case `report_errors/match`", r#"re:run("abc","a",[report_errors])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,1,106>>"),
        t("re/match_limit_zero_report-errors", "re run options case `match_limit_zero/report-errors`", r#"re:run("abc","a",[{match_limit,0},report_errors])"#, "ok <<131,104,2,119,5,101,114,114,111,114,119,11,109,97,116,99,104,95,108,105,109,105,116>>"),
        t("re/match_limit_zero_nomatch", "re run options case `match_limit_zero/nomatch`", r#"re:run("abc","a",[{match_limit,0}])"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/compile-error_a(b", "re compile errors case `compile-error/a(b`", r#"re:compile("a(b")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,27,109,105,115,115,105,110,103,32,99,108,111,115,105,110,103,32,112,97,114,101,110,116,104,101,115,105,115,97,3>>"),
        t("re/compile-error_[a", "re compile errors case `compile-error/[a`", r#"re:compile("[a")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,41,109,105,115,115,105,110,103,32,116,101,114,109,105,110,97,116,105,110,103,32,93,32,102,111,114,32,99,104,97,114,97,99,116,101,114,32,99,108,97,115,115,97,2>>"),
        t("re/compile-error_[]", "re compile errors case `compile-error/[]`", r#"re:compile("[]")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,41,109,105,115,115,105,110,103,32,116,101,114,109,105,110,97,116,105,110,103,32,93,32,102,111,114,32,99,104,97,114,97,99,116,101,114,32,99,108,97,115,115,97,2>>"),
        t("re/compile-error_a{2,1}", "re compile errors case `compile-error/a{2,1}`", r#"re:compile("a{2,1}")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,37,110,117,109,98,101,114,115,32,111,117,116,32,111,102,32,111,114,100,101,114,32,105,110,32,123,125,32,113,117,97,110,116,105,102,105,101,114,97,5>>"),
        t("re/compile-error_a{3,2}", "re compile errors case `compile-error/a{3,2}`", r#"re:compile("a{3,2}")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,37,110,117,109,98,101,114,115,32,111,117,116,32,111,102,32,111,114,100,101,114,32,105,110,32,123,125,32,113,117,97,110,116,105,102,105,101,114,97,5>>"),
        t("re/compile-error_*a", "re compile errors case `compile-error/*a`", r#"re:compile("*a")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,44,113,117,97,110,116,105,102,105,101,114,32,100,111,101,115,32,110,111,116,32,102,111,108,108,111,119,32,97,32,114,101,112,101,97,116,97,98,108,101,32,105,116,101,109,97,1>>"),
        t("re/compile-error_{2}", "re compile errors case `compile-error/{2}`", r#"re:compile("{2}")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,44,113,117,97,110,116,105,102,105,101,114,32,100,111,101,115,32,110,111,116,32,102,111,108,108,111,119,32,97,32,114,101,112,101,97,116,97,98,108,101,32,105,116,101,109,97,3>>"),
        t("re/compile-error_a**", "re compile errors case `compile-error/a**`", r#"re:compile("a**")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,44,113,117,97,110,116,105,102,105,101,114,32,100,111,101,115,32,110,111,116,32,102,111,108,108,111,119,32,97,32,114,101,112,101,97,116,97,98,108,101,32,105,116,101,109,97,3>>"),
        t("re/compile-error_a\\", "re compile errors case `compile-error/a\\`", r#"re:compile("a\\")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,19,92,32,97,116,32,101,110,100,32,111,102,32,112,97,116,116,101,114,110,97,2>>"),
        t("re/compile-error_a)", "re compile errors case `compile-error/a)`", r#"re:compile("a)")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,29,117,110,109,97,116,99,104,101,100,32,99,108,111,115,105,110,103,32,112,97,114,101,110,116,104,101,115,105,115,97,2>>"),
        t("re/compile-error_(?i", "re compile errors case `compile-error/(?i`", r#"re:compile("(?i")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,27,109,105,115,115,105,110,103,32,99,108,111,115,105,110,103,32,112,97,114,101,110,116,104,101,115,105,115,97,3>>"),
        t("re/compile-error_duplicate-subpattern-name", "re compile errors case `compile-error/duplicate-subpattern-name`", r#"re:compile("(?<n>a)(?<n>b)")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,65,116,119,111,32,110,97,109,101,100,32,115,117,98,112,97,116,116,101,114,110,115,32,104,97,118,101,32,116,104,101,32,115,97,109,101,32,110,97,109,101,32,40,80,67,82,69,50,95,68,85,80,78,65,77,69,83,32,110,111,116,32,115,101,116,41,97,12>>"),
        t("re/compile-error_[z-a]", "re compile errors case `compile-error/[z-a]`", r#"re:compile("[z-a]")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,37,114,97,110,103,101,32,111,117,116,32,111,102,32,111,114,100,101,114,32,105,110,32,99,104,97,114,97,99,116,101,114,32,99,108,97,115,115,97,4>>"),
        t("re/compile-error_(?<1a>x)", "re compile errors case `compile-error/(?<1a>x)`", r#"re:compile("(?<1a>x)")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,43,115,117,98,112,97,116,116,101,114,110,32,110,97,109,101,32,109,117,115,116,32,115,116,97,114,116,32,119,105,116,104,32,97,32,110,111,110,45,100,105,103,105,116,97,4>>"),
        t("re/compile-error_hex-brace-non-hex", "re compile errors case `compile-error/hex-brace-non-hex`", r#"re:compile("\\x{zz}")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,50,110,111,110,45,104,101,120,32,99,104,97,114,97,99,116,101,114,32,105,110,32,92,120,123,125,32,40,99,108,111,115,105,110,103,32,98,114,97,99,101,32,109,105,115,115,105,110,103,63,41,97,4>>"),
        t("re/compile-error_unknown-posix-class", "re compile errors case `compile-error/unknown-posix-class`", r#"re:compile("[[:foo:]]")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,24,117,110,107,110,111,119,110,32,80,79,83,73,88,32,99,108,97,115,115,32,110,97,109,101,97,8>>"),
        t("re/compile-error_hex-brace-too-large", "re compile errors case `compile-error/hex-brace-too-large`", r#"re:compile("\\x{110000}")"#, "ok <<131,104,2,119,5,101,114,114,111,114,104,2,107,0,55,99,104,97,114,97,99,116,101,114,32,99,111,100,101,32,112,111,105,110,116,32,118,97,108,117,101,32,105,110,32,92,120,123,125,32,111,114,32,92,111,123,125,32,105,115,32,116,111,111,32,108,97,114,103,101,97,9>>"),
        t("re/compile_control-caseless-shape", "re compile errors case `compile/control-caseless-shape`", r#"case re:compile("a",[caseless]) of
                     {ok,{re_pattern,N,U,Ca,_}} -> {ok,{re_pattern,N,U,Ca}};
                     X -> X
                   end"#, "ok <<131,104,2,119,2,111,107,104,4,119,10,114,101,95,112,97,116,116,101,114,110,97,0,97,0,97,0>>"),
        t("re/syntax_dot", "re pattern syntax case `syntax/dot`", r#"re:run("xabcy",".")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,1,106>>"),
        t("re/syntax_dotall-newline", "re pattern syntax case `syntax/dotall-newline`", r#"re:run("a\nb","a.b",[dotall])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/syntax_class-range", "re pattern syntax case `syntax/class-range`", r#"re:run("abc","[a-c]+")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/syntax_digit-class", "re pattern syntax case `syntax/digit-class`", r#"re:run("abc1","\\d+")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,3,97,1,106>>"),
        t("re/syntax_word-class", "re pattern syntax case `syntax/word-class`", r#"re:run("abc_1","\\w+")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,5,106>>"),
        t("re/syntax_space-class", "re pattern syntax case `syntax/space-class`", r#"re:run("a b","\\s")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,1,97,1,106>>"),
        t("re/syntax_anchor-absolute-start", "re pattern syntax case `syntax/anchor-absolute-start`", r#"re:run("abc","\\Aabc")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/syntax_anchor-absolute-end", "re pattern syntax case `syntax/anchor-absolute-end`", r#"re:run("abc","bc\\z")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,1,97,2,106>>"),
        t("re/syntax_word-boundary-nomatch", "re pattern syntax case `syntax/word-boundary-nomatch`", r#"re:run("abc","\\bb")"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/syntax_posix-class", "re pattern syntax case `syntax/posix-class`", r#"re:run("abc","[[:alpha:]]+")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/syntax_quoted-literal", "re pattern syntax case `syntax/quoted-literal`", r#"re:run("a+b","\\Qa+b\\E")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/syntax_backreference", "re pattern syntax case `syntax/backreference`", r#"re:run("abab","(a)(b)\\1")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,104,2,97,0,97,3,104,2,97,0,97,1,104,2,97,1,97,1,106>>"),
        t("re/syntax_quantifier-open-range", "re pattern syntax case `syntax/quantifier-open-range`", r#"re:run("aaa","a{2,}")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/syntax_quantifier-closed-range", "re pattern syntax case `syntax/quantifier-closed-range`", r#"re:run("aaaa","a{2,3}")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/syntax_quantifier-lazy", "re pattern syntax case `syntax/quantifier-lazy`", r#"re:run("aaa","a+?")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,1,106>>"),
        t("re/syntax_quantifier-possessive-nomatch", "re pattern syntax case `syntax/quantifier-possessive-nomatch`", r#"re:run("aaa","a*+a")"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/syntax_group-repeat", "re pattern syntax case `syntax/group-repeat`", r#"re:run("aaa","(a)*")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,2,104,2,97,0,97,3,104,2,97,2,97,1,106>>"),
        t("re/syntax_hex-brace-escape", "re pattern syntax case `syntax/hex-brace-escape`", r#"re:run("A","\\x{41}")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,1,106>>"),
        t("re/syntax_named-group", "re pattern syntax case `syntax/named-group`", r#"re:run("abc","(?<foo>b)(c)")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,104,2,97,1,97,2,104,2,97,1,97,1,104,2,97,2,97,1,106>>"),
        t("re/syntax_inline-caseless", "re pattern syntax case `syntax/inline-caseless`", r#"re:run("ABC","(?i)abc")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/syntax_scoped-caseless", "re pattern syntax case `syntax/scoped-caseless`", r#"re:run("abc","a(?i:BC)")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/syntax_inline-dotall", "re pattern syntax case `syntax/inline-dotall`", r#"re:run("a\nb","(?s)a.b")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/syntax_non-capturing-group", "re pattern syntax case `syntax/non-capturing-group`", r#"re:run("abc","(?:b)(c)",[{capture,all,index}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,2,104,2,97,1,97,2,104,2,97,2,97,1,106>>"),
        t("re/syntax_tab-escape", "re pattern syntax case `syntax/tab-escape`", r#"re:run("a\tb","a\\tb")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        t("re/syntax_anchored-alternation", "re pattern syntax case `syntax/anchored-alternation`", r#"re:run("abc","^(?:a|b)(c)$")"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/syntax_quantifier-partial-range", "re pattern syntax case `syntax/quantifier-partial-range`", r#"re:run("xaay","a{1,2}")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,1,97,2,106>>"),
        t("re/syntax_non-word-boundary", "re pattern syntax case `syntax/non-word-boundary`", r#"re:run("abc","\\B")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,1,97,0,106>>"),
        t("re/syntax_global-group-repeat", "re pattern syntax case `syntax/global-group-repeat`", r#"re:run("abab","(a|b)+",[global])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,108,0,0,0,2,104,2,97,0,97,4,104,2,97,3,97,1,106,106>>"),
        t("re/subject_nested-iodata", "re subjects and unicode case `subject/nested-iodata`", r#"re:run([<<"xa">>,"bcy"],"b")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,2,97,1,106>>"),
        t("re/subject_empty-charlist", "re subjects and unicode case `subject/empty-charlist`", r#"re:run([],"a")"#, "ok <<131,119,7,110,111,109,97,116,99,104>>"),
        t("re/subject_empty-both", "re subjects and unicode case `subject/empty-both`", r#"re:run("", "")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,0,106>>"),
        terr("re/subject_float-is-not-iodata", "re subjects and unicode case `subject/float-is-not-iodata`", r#"re:run([1.0],"a")"#, "exception error badarg"),
        t("re/re_charlist", "re subjects and unicode case `re/charlist`", r#"re:run("abc",[97])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,1,106>>"),
        t("re/re_binary", "re subjects and unicode case `re/binary`", r#"re:run("abc",<<"a">>)"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,1,106>>"),
        t("re/unicode_charlist-subject", "re subjects and unicode case `unicode/charlist-subject`", r#"re:run("åäö","ä",[unicode])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,2,97,2,106>>"),
        t("re/unicode_capture-list", "re subjects and unicode case `unicode/capture-list`", r#"re:run("åäö","ä",[unicode,{capture,all,list}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,107,0,1,228,106>>"),
        t("re/unicode_capture-binary", "re subjects and unicode case `unicode/capture-binary`", r#"re:run("åäö","ä",[unicode,{capture,all,binary}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,109,0,0,0,2,195,164,106>>"),
        t("re/unicode_non-ascii-class", "re subjects and unicode case `unicode/non-ascii-class`", r#"re:run("åäö","[åä]",[unicode])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,2,106>>"),
        t("re/unicode_dot-is-a-codepoint", "re subjects and unicode case `unicode/dot-is-a-codepoint`", r#"re:run("åäö",".",[unicode])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,2,106>>"),
        t("re/unicode_offset-counts-bytes", "re subjects and unicode case `unicode/offset-counts-bytes`", r#"re:run("åäö","ö",[unicode,{offset,4}])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,4,97,2,106>>"),
        terr("re/unicode_invalid-utf8-subject", "re subjects and unicode case `unicode/invalid-utf8-subject`", r#"re:run(<<255>>,".",[unicode])"#, "exception error badarg"),
        t("re/non-unicode_raw-byte-subject", "re subjects and unicode case `non-unicode/raw-byte-subject`", r#"re:run(<<255>>,".")"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,1,106>>"),
        t("re/version_0", "re pre-compiled patterns / run variants case `version/0`", r#"re:version()"#, "ok <<131,109,0,0,0,16,49,48,46,52,55,32,50,48,50,53,45,49,48,45,50,49>>"),
        t("re/inspect_namelist-named", "re pre-compiled patterns / run variants case `inspect/namelist-named`", r#"re:inspect(element(2,re:compile("(?<A>A)|(?<B>B)|(?<C>C)")), namelist)"#, "ok <<131,104,2,119,8,110,97,109,101,108,105,115,116,108,0,0,0,3,109,0,0,0,1,65,109,0,0,0,1,66,109,0,0,0,1,67,106>>"),
        t("re/inspect_namelist-empty", "re pre-compiled patterns / run variants case `inspect/namelist-empty`", r#"re:inspect(element(2,re:compile("(b)(c)")), namelist)"#, "ok <<131,104,2,119,8,110,97,109,101,108,105,115,116,106>>"),
        t("re/precompiled_run2", "re pre-compiled patterns / run variants case `precompiled/run2`", r#"re:run("xabcy", element(2,re:compile("(b)(c)")))"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,104,2,97,2,97,2,104,2,97,2,97,1,104,2,97,3,97,1,106>>"),
        t("re/precompiled_global", "re pre-compiled patterns / run variants case `precompiled/global`", r#"re:run("abab", element(2,re:compile("a")), [global])"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,2,108,0,0,0,1,104,2,97,0,97,1,106,108,0,0,0,1,104,2,97,2,97,1,106,106>>"),
        t("re/precompiled_baked-caseless", "re pre-compiled patterns / run variants case `precompiled/baked-caseless`", r#"re:run("ABC", element(2,re:compile("abc", [caseless])))"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,104,2,97,0,97,3,106>>"),
        terr("re/precompiled_compile-only-caseless", "re pre-compiled patterns / run variants case `precompiled/compile-only-caseless`", r#"re:run("abc", element(2,re:compile("a")), [caseless])"#, "exception error badarg"),
        terr("re/precompiled_compile-only-unicode", "re pre-compiled patterns / run variants case `precompiled/compile-only-unicode`", r#"re:run("abc", element(2,re:compile("a")), [unicode])"#, "exception error badarg"),
        terr("re/precompiled_compile-only-export", "re pre-compiled patterns / run variants case `precompiled/compile-only-export`", r#"re:run("abc", element(2,re:compile("a")), [export])"#, "exception error badarg"),
        t("re/internal_run_offset-true", "re pre-compiled patterns / run variants case `internal_run/offset-true`", r#"re:internal_run("xabcy", element(2,re:compile("(b)(c)")), [{offset,0}], true)"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,104,2,97,2,97,2,104,2,97,2,97,1,104,2,97,3,97,1,106>>"),
        t("re/internal_run_offset-false", "re pre-compiled patterns / run variants case `internal_run/offset-false`", r#"re:internal_run("xabcy", element(2,re:compile("(b)(c)")), [{offset,0}], false)"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,104,2,97,2,97,2,104,2,97,2,97,1,104,2,97,3,97,1,106>>"),
        t("re/internal_run_global", "re pre-compiled patterns / run variants case `internal_run/global`", r#"re:internal_run("xabcy", element(2,re:compile("(b)(c)")), [global], true)"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,1,108,0,0,0,3,104,2,97,2,97,2,104,2,97,2,97,1,104,2,97,3,97,1,106,106>>"),
        t("re/internal_run_textual-pattern", "re pre-compiled patterns / run variants case `internal_run/textual-pattern`", r#"re:internal_run("xabcy", "(b)(c)", [], true)"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,104,2,97,2,97,2,104,2,97,2,97,1,104,2,97,3,97,1,106>>"),
        terr("re/internal_run_non-boolean-flag", "re pre-compiled patterns / run variants case `internal_run/non-boolean-flag`", r#"re:internal_run("xabcy", "(b)(c)", [], 1)"#, "exception error badarg"),
        t("re/export-import_round-trip", "re pre-compiled patterns / run variants case `export-import/round-trip`", r#"re:run("xabcy", re:import(element(2,re:compile("(b)(c)", [export]))))"#, "ok <<131,104,2,119,5,109,97,116,99,104,108,0,0,0,3,104,2,97,2,97,2,104,2,97,2,97,1,104,2,97,3,97,1,106>>"),
        terr("re/import_non-exported", "re pre-compiled patterns / run variants case `import/non-exported`", r#"re:import(zzz)"#, "exception error badarg"),
        terr("re/run_raw-export-form-is-badarg", "re pre-compiled patterns / run variants case `run/raw-export-form-is-badarg`", r#"re:run("xabcy", element(2,re:compile("(b)(c)", [export])))"#, "exception error badarg"),
        t("re/export_projected-pattern-and-options", "re pre-compiled patterns / run variants case `export/projected-pattern-and-options`", r#"case re:compile("(b)(c)",[export]) of
                     {ok,{re_exported_pattern,_,P,O,_}} -> {P,O};
                     X -> X
                   end"#, "ok <<131,104,2,109,0,0,0,6,40,98,41,40,99,41,108,0,0,0,1,119,6,101,120,112,111,114,116,106>>"),
        // -- re:replace/3,4 (added on top of the live-oracle source) --------------------
        t("re/replace_global", "re:replace/4 with [global] replaces every occurrence", r#"re:replace("hello world","o","0",[global])"#, "ok <<131,108,0,0,0,4,109,0,0,0,4,104,101,108,108,109,0,0,0,1,48,109,0,0,0,2,32,119,109,0,0,0,1,48,109,0,0,0,3,114,108,100>>"),
        t("re/replace_return_binary", "re:replace/4 with {return,binary} returns a binary subject copy", r#"re:replace("hello world","o","0",[{return,binary},global])"#, "ok <<131,109,0,0,0,11,104,101,108,108,48,32,119,48,114,108,100>>"),
        t("re/replace_caseless", "re:replace/4 with [caseless] matches without regard to case", r#"re:replace("ABC","abc","x",[caseless,global])"#, "ok <<131,108,0,0,0,1,109,0,0,0,1,120,106>>"),
        t("re/replace_backreference", "re:replace/4 with a `\\1` backreference splices a captured group into the replacement", r#"re:replace("abab","(a)(b)","[\\1]",[global])"#, "ok <<131,108,0,0,0,2,108,0,0,0,3,109,0,0,0,1,91,109,0,0,0,1,97,109,0,0,0,1,93,106,108,0,0,0,3,109,0,0,0,1,91,109,0,0,0,1,97,109,0,0,0,1,93,106,106>>"),
        t("re/replace_arity3_default", "re:replace/3 replaces only the first match (the default {global,false})", r#"re:replace("hello","l","L")"#, "ok <<131,108,0,0,0,2,109,0,0,0,2,104,101,109,0,0,0,1,76,109,0,0,0,2,108,111>>"),
        t("re/replace_return_list", "re:replace/4 with {return,list} returns a charlist subject copy", r#"re:replace("a-b-c","-",",",[global,{return,list}])"#, "ok <<131,107,0,5,97,44,98,44,99>>"),
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::{CaseKind, Contract, Probe};
    use std::collections::HashSet;

    /// The number of cases in the group (all 136 live-oracle cases + the 6 `re:replace`
    /// additions).
    const CASE_COUNT: usize = 140;

    /// A representative case id from each area the group covers, so a whole area cannot be
    /// dropped without a red test.
    const AREA_IDS: [&str; 12] = [
        "re/run2_simple",
        "re/capture_all_names-list",
        "re/global_all-index",
        "re/offset_in-range",
        "re/caseless_simple",
        "re/match_limit_zero_report-errors",
        "re/compile-error_duplicate-subpattern-name",
        "re/syntax_backreference",
        "re/unicode_offset-counts-bytes",
        "re/version_0",
        "re/internal_run_offset-true",
        "re/export-import_round-trip",
    ];

    /// The `re:replace/3,4` cases added on top of the live-oracle source.
    const REPLACE_IDS: [&str; 6] = [
        "re/replace_global",
        "re/replace_return_binary",
        "re/replace_caseless",
        "re/replace_backreference",
        "re/replace_arity3_default",
        "re/replace_return_list",
    ];

    #[test]
    fn re_cases_are_well_formed_default_probe_a7_eval_cases() {
        let cases = group();
        assert_eq!(cases.len(), CASE_COUNT, "the A.7 re group's case count");
        let mut seen = HashSet::new();
        for case in &cases {
            assert!(case.id.starts_with("re/"), "unexpected id {}", case.id);
            assert!(seen.insert(case.id), "duplicate case id {}", case.id);
            assert!(
                !case.description.is_empty(),
                "case {} needs a description",
                case.id
            );
            assert_eq!(
                case.contracts,
                &[Contract::OtpApps],
                "case {} must evidence the A.7 OTP-app contract",
                case.id
            );
            assert_eq!(case.kind_label(), "term_to_binary", "case {}", case.id);
            assert!(
                matches!(
                    case.kind,
                    CaseKind::Eval {
                        probe: Probe::TermToBinary,
                        extra_argv: [],
                        ..
                    }
                ),
                "case {} must be a default-probe eval case",
                case.id
            );
            let expr = case.expr().expect("every re case is an eval case");
            assert!(!expr.is_empty(), "case {} has an empty expression", case.id);
            assert!(case.extra_argv().is_empty(), "case {} sets extra argv", case.id);
            assert!(case.env().is_empty(), "case {} sets an env", case.id);
            let golden = case.golden.stdout.expect("every case has a stdout golden");
            assert!(!golden.is_empty(), "case {}: empty golden", case.id);
            assert!(!golden.ends_with('\n'), "case {}: the golden is a line", case.id);
            assert!(
                golden.starts_with("ok ") || golden.starts_with("exception "),
                "case {}: unexpected golden {golden:?}",
                case.id
            );
            assert_eq!(
                golden.starts_with("ok "),
                case.golden.exit_code == 0,
                "case {}: the exit code disagrees with the golden shape",
                case.id
            );
        }
    }

    /// No expression binds the probe wrapper's `V`/`C`/`R`: a collision makes the oracle's
    /// `erl_eval` reject the wrapped probe with `unsafe_var` — a guaranteed false mismatch,
    /// not a capability gap.
    #[test]
    fn no_expression_binds_the_reserved_probe_variables() {
        for case in group() {
            let expr = case.expr().expect("every re case is an eval case");
            let tokens = identifiers(expr);
            for reserved in ["V", "C", "R"] {
                assert!(
                    !tokens.contains(&reserved),
                    "case {}: the expression binds the probe variable `{reserved}`: {expr:?}",
                    case.id
                );
            }
        }
    }

    /// The identifiers of `expr` with string literals removed, so a pattern's `(?<C>C)` is
    /// not mistaken for a bound variable.
    fn identifiers(expr: &str) -> Vec<&str> {
        let bytes = expr.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        let mut in_string = false;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    in_string = !in_string;
                    i += 1;
                }
                b'\\' if in_string => i += 2,
                b if !in_string && (b.is_ascii_alphanumeric() || b == b'_' || b == b'@') => {
                    let start = i;
                    while i < bytes.len()
                        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'@')
                    {
                        i += 1;
                    }
                    out.push(&expr[start..i]);
                }
                _ => i += 1,
            }
        }
        out
    }

    #[test]
    fn re_cases_cover_every_area_and_the_replace_surface() {
        let cases = group();
        for id in AREA_IDS.iter().chain(REPLACE_IDS.iter()) {
            assert!(
                cases.iter().any(|c| c.id == *id),
                "expected the re case {id}"
            );
        }
    }

    /// The raising cases pin the probe's `exception <class> <reason>` line: the oracle's
    /// `re` raises `error:badarg` for each of them.
    #[test]
    fn the_raising_cases_pin_the_exception_line() {
        let raising: Vec<&str> = group()
            .iter()
            .filter(|c| c.golden.exit_code == 1)
            .map(|c| c.id)
            .collect();
        assert_eq!(raising.len(), 10, "the group's raising cases: {raising:?}");
        for case in group() {
            if case.golden.exit_code == 1 {
                assert_eq!(
                    case.golden.stdout,
                    Some("exception error badarg"),
                    "case {}",
                    case.id
                );
            }
        }
    }

    /// The `compile/control-caseless-shape` promotion renames the one variable that would
    /// collide with the probe wrapper (`C` -> `Ca`), and every other expression stays
    /// verbatim: the round trip through a compiled pattern still returns the same
    /// payload-free `{ok,{re_pattern,N,U,C}}` projection.
    #[test]
    fn the_control_compile_case_is_promoted_with_the_probe_collision_renamed() {
        let case = group()
            .into_iter()
            .find(|c| c.id == "re/compile_control-caseless-shape")
            .expect("the compile control case");
        let expr = case.expr().expect("an eval expression");
        assert!(expr.contains("{re_pattern,N,U,Ca,_}} -> {ok,{re_pattern,N,U,Ca}}"), "{expr}");
        assert!(!expr.contains("U,C,_"), "the colliding `C` must be renamed: {expr}");
        assert!(expr.contains("re:compile(\"a\",[caseless])"), "{expr}");
    }
}
