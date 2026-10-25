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

//! Contract **A.7** — the two externally-observable legacy hashes.
//!
//! `erlang:phash/2` (the C `make_hash`) and `erlang:phash2/1,2` (the C `make_hash2`) are
//! *frozen* observable behaviour: a program that persisted a hash bucket or a term-hash key
//! must read the same value from the rewrite. They are their own group because the pinned
//! values come from bit-exact transcriptions of `erl_term_hashing.c` (`../../crates/beam_bifs`
//! `src/phash.rs`), not from the term API — so the goldens are the strongest possible
//! statement that the transcription is right.
//!
//! Grouped by the input family that exercises a distinct branch of the C:
//! small-int boundaries, the block-hash binary path (by length), a non-byte-aligned
//! bitstring, a map (which routes through the *other* hash), `NIL`/improper lists, and the
//! `Range` argument.
//!
//! The observable is always a tuple of **integers**, rendered with the *value* probe (`~w`),
//! so nothing here depends on a per-VM-start value: no pid/ref/port, no multi-key map. The
//! single-key maps are safe because `phash2/1` of a map hashes its key/value pairs in
//! term order, never a `~w` rendering of the map itself.
//!
//! # A *fun*'s hash — the three module-run cases for a local closure
//!
//! `phash2/1,2` and `phash/2` of a fun hash its **identity** (`module`, `index`/`new_index`,
//! `uniq`/`old_uniq` and the free-variable environment). For a *compiled* fun that identity is
//! stable across VM starts but **not** across compilations: `uniq` is folded into the module's
//! CRC, which moves with the compile-time source path — and every module-run case is compiled
//! into its own temp dir, so a hash *integer* golden would drift between two runs of the very
//! same corpus.
//!
//! All three cases are therefore **module runs** (an `-eval` fun is an `erl_eval` closure, a
//! different identity) and they pin **relations and a 27-bit/1-based range mask**, never the
//! integer:
//!
//! * `phash2/local_fun_make_fun3` — the direct `make_fun3` form through `phash2/1` and `phash2/2`:
//!   `phash2/1` is the full 32-bit hash masked with `(1 bsl 27) - 1` and `phash2/2` is the full
//!   hash modulo `Range` (which is why `phash2(F, 1000) =/= phash2(F) rem 1000`), so the
//!   case pins that relation through the `Range` of `2^32` that asks for the unmasked hash, a
//!   `Range` of 1, stability, the round-trip image identity and two distinct-fun relations.
//! * `phash2/local_fun_via_its_imported_image` — the same closure through
//!   `binary_to_term(term_to_binary(F))`, whose hash it pins as equal to the local form's while
//!   asserting the compile-time identity items (`type`, `module`, `index`) and the image size.
//! * `phash/legacy_local_fun` — the legacy `make_hash` port, `phash/2` (there is no `phash/1`:
//!   the oracle's `erlang:function_exported(erlang, phash, 1)` is `false`): its answer is
//!   `1 + hash rem Range` (a `Range` of 1 answers `1`, and the `2^32` form answers `hash + 1`,
//!   which is what makes the `Range` arithmetic pinnable as a relation).
//!
//! Each claims contract A.2 alongside A.7 (the corpus registry only lets a module-run case claim
//! the contracts it evidences directly). The closure is applied as well as hashed, so a fun whose
//! hash were answered from a substitute while the fun itself did not work cannot pass.

use super::{Case, CaseKind, Contract, Golden, ModuleRun, Probe};

/// The module behind `phash2/local_fun_via_its_imported_image`: two closures (so the fun table
/// is a real `make_fun3` entry) and a function that hashes the **imported** image of one of them
/// (`binary_to_term(term_to_binary(F))`) — see the module docs for why no hash integer is
/// pinned.
///
/// The identity items (`type`, `module`, `index`) are hard `=` matches, so a wrong `fun_info/2`
/// item aborts the run as a `badmatch` instead of passing silently. The answer is **one
/// integer**, as a module run requires: the fun image's byte size (a compile-time constant) in
/// the high digits and a 6-bit flag mask that is `63` when all six hash relations hold. No hash
/// *integer* is pinned: the fun's `uniq` is folded into the module's CRC, which moves with the
/// compile-time source path — a `differential` module case is compiled into its own temp dir, so
/// the hash is stable across VM starts but **not** across compilations (measured: the same
/// source hashed to two different integers from two different compile dirs, each reproduced
/// exactly by both runtimes).
const FUN_HASH_SOURCE: &str = "-module(fun_hash).\n\
-export([multi_closure_hash/0]).\n\
mk_adder(N) -> fun(X) -> X + N end.\n\
mk_scale() -> fun(X) -> X * 3 end.\n\
bit(true, B) -> B;\n\
bit(false, _) -> 0.\n\
multi_closure_hash() ->\n\
F = mk_adder(3),\n\
H = mk_scale(),\n\
G = binary_to_term(term_to_binary(F)),\n\
K = binary_to_term(term_to_binary(H)),\n\
{type, local} = erlang:fun_info(G, type),\n\
{module, fun_hash} = erlang:fun_info(G, module),\n\
{index, 0} = erlang:fun_info(G, index),\n\
Flags = bit(erlang:phash2(G) =:= erlang:phash2(G), 1)\n\
bor bit(is_integer(erlang:phash2(G)), 2)\n\
bor bit(erlang:phash2(G, 4) >= 0 andalso erlang:phash2(G, 4) < 4, 4)\n\
bor bit(erlang:phash2(G, 1) =:= 0, 8)\n\
bor bit(erlang:phash2(G) =/= erlang:phash2(K), 16)\n\
bor bit(erlang:phash2(G) =:= erlang:phash2(binary_to_term(term_to_binary(F))), 32),\n\
byte_size(term_to_binary(F)) * 100 + Flags.\n";

/// The module behind `phash2/local_fun_make_fun3` and `phash/legacy_local_fun`: one capturing
/// closure factory `mk_cap(A)` (so every fun is a `make_fun3` entry carrying a free variable) and
/// one function per hash port.
///
/// Each function hashes a local fun **directly** (no round trip) and answers with **one integer**
/// as a module run requires: the fun image's byte size in the high digits (a compile-time constant,
/// the same kind of *classification* the sibling case uses) followed by a relation mask — `511`
/// when all nine `phash2/1,2` relations hold, `255` when all eight `phash/2` ones do. The funs are
/// applied as well as hashed (`_ = F(1) + G(1) + I(1)`), so a hash answered from a substitute for
/// a fun that does not actually work cannot pass. No hash *integer* is pinned: `uniq` rides the
/// module CRC, which moves with the temp-dir source path.
const FUN_HASH_MK_SOURCE: &str = "-module(fun_hash_mk).\n\
-export([local_fun_phash2/0, local_fun_phash/0]).\n\
bit(true, B) -> B;\n\
bit(false, _) -> 0.\n\
mk_cap(A) -> fun(X) -> X + A end.\n\
local_fun_phash2() ->\n\
F = mk_cap(1),\n\
G = mk_cap(2),\n\
I = binary_to_term(term_to_binary(F)),\n\
Full = erlang:phash2(F, 1 bsl 32),\n\
H = erlang:phash2(F),\n\
_ = F(1) + G(1) + I(1),\n\
Flags = bit(is_integer(H), 1)\n\
bor bit(H >= 0 andalso H < (1 bsl 27), 2)\n\
bor bit(H =:= erlang:phash2(F), 4)\n\
bor bit(H =:= Full band 16#7FFFFFF, 8)\n\
bor bit(erlang:phash2(F, 1000) =:= Full rem 1000, 16)\n\
bor bit(erlang:phash2(F, 1) =:= 0, 32)\n\
bor bit(H =:= erlang:phash2(I), 64)\n\
bor bit(H =/= erlang:phash2(G), 128)\n\
bor bit(erlang:phash2(mk_cap(3)) =/= H, 256),\n\
byte_size(term_to_binary(F)) * 1000 + Flags.\n\
local_fun_phash() ->\n\
F = mk_cap(1),\n\
G = mk_cap(2),\n\
I = binary_to_term(term_to_binary(F)),\n\
Hash = erlang:phash(F, 1 bsl 32) - 1,\n\
P = erlang:phash(F, 1 bsl 27),\n\
_ = F(1) + G(1) + I(1),\n\
Flags = bit(is_integer(P), 1)\n\
bor bit(P >= 1 andalso P =< (1 bsl 27), 2)\n\
bor bit(P =:= erlang:phash(F, 1 bsl 27), 4)\n\
bor bit(erlang:phash(F, 1) =:= 1, 8)\n\
bor bit(erlang:phash(F, 65536) =:= 1 + (Hash rem 65536), 16)\n\
bor bit(P =:= erlang:phash(I, 1 bsl 27), 32)\n\
bor bit(P =/= erlang:phash(G, 1 bsl 27), 64)\n\
bor bit(erlang:phash(mk_cap(3), 1 bsl 27) =/= P, 128),\n\
byte_size(term_to_binary(F)) * 1000 + Flags.\n";
/// The A.7 `phash`/`phash2` cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "phash2/int_boundaries",
            description: "phash2/1 across the small-int sign/magnitude byte-scan boundaries (2^27-1, 2^27 and their negatives)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(134217727), erlang:phash2(134217728), \
                       erlang:phash2(-134217727), erlang:phash2(-134217728)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {112602999,12354923,96386750,69672967}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/float_negative_zero",
            description: "phash2/1 and phash/2 of 0.0 / -0.0 (the legacy hash normalises the sign) plus a plain float",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(0.0), erlang:phash2(-0.0), erlang:phash(0.0, 1000000), \
                       erlang:phash(-0.0, 1000000), erlang:phash2(1.5)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {20875736,20875736,1,1,10380315}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/atom_nil_tuple_improper_list",
            description: "phash2/1 of NIL, an atom, a tuple and an improper list (the terminating non-list tail step)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2([]), erlang:phash2(hello), erlang:phash2({1,2,3}), \
                       erlang:phash2([1|2])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {113427502,7258927,28734975,86124794}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/binary_lengths_0_64_and_large",
            description: "phash2/1 by binary length: the empty and a short binary plus the >=12-byte block-hash path (64/200 bytes)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(<<>>), erlang:phash2(<<1,2,3>>), \
                       erlang:phash2(binary:copy(<<1>>, 64)), erlang:phash2(binary:copy(<<7>>, 200))}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {13708901,6479071,72330118,121929617}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/non_byte_aligned_bitstring",
            description: "phash2/1 of bitstrings whose bit size is not a multiple of 8 (the partial final-byte step)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(<<1:1>>), erlang:phash2(<<1:3,2:5>>), erlang:phash2(<<255:3>>)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {102233125,97170390,63553086}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/map_and_external_fun",
            description: "phash2/1 of a single-key map (which routes through the other hash) and of an external fun",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(#{a => 1}), erlang:phash2(fun lists:reverse/1)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {59617982,134027972}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/range_0_1_and_2pow32_minus_1",
            description: "phash2/2's Range argument: 1, 2^32-1, and 0 (which is badarg)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(<<1,2,3>>, 1), erlang:phash2(<<1,2,3>>, 4294967295), \
                       try erlang:phash2(<<1,2,3>>, 0) catch error:Reason -> Reason end}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {0,4167228639,badarg}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash/legacy_map_and_fun",
            description: "phash/2 (the legacy make_hash) of a single-key map, an external fun, a small int and a binary",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash(#{a => 1}, 1000000), erlang:phash(fun lists:reverse/1, 1000000), \
                       erlang:phash(1, 4), erlang:phash(<<1,2,3>>, 1000000)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {235874,832296,4,692590}"),
                exit_code: 0,
            },
        },
        // --- the remaining Range / input-family branches -------------------------------------
        Case {
            id: "phash2/range_256_and_1000",
            description: "phash2/2 with a 256/1000 Range over an int, an atom and a list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(1, 256), erlang:phash2(hello, 256), \
                       erlang:phash2([1,2,3], 1000)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {234,47,188}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/range_two_and_three",
            description: "phash2/2 with the smallest usable Ranges (2 and 3) over a binary and a tuple",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(<<1,2,3>>, 2), erlang:phash2({a,b,c}, 3)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {1,1}"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/binary_exactly_12_bytes_block_hash",
            description: "phash2/2 of a binary exactly at the 12-byte block-hash threshold",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:phash2(binary:copy(<<1>>, 12), 1000000)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 224998"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/bignum_positive_and_negative",
            description: "phash2/1 of a 30-digit bignum and its negation (the arbitrary-precision path)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:phash2(123456789012345678901234567890), \
                       erlang:phash2(-123456789012345678901234567890)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {132143600,33199935}"),
                exit_code: 0,
            },
        },
        // --- a *fun*'s hash: the fun-identity branch -----------------------------------------
        //
        // The fun's hash is VM-stable but *not* compilation-stable (its `uniq` rides the module
        // CRC, which moves with the temp-dir source path a module-run case compiles into), so the
        // golden pins the two compile-time identity items and the relations a correct
        // fun-identity hash must satisfy — never the integer, which would drift between
        // compilations of the very same source. The direct `make_fun3` form is pinned by
        // `phash2/local_fun_make_fun3` and `phash/legacy_local_fun`; the **imported**
        // (`NEW_FUN_EXT`) image of the same closure, which hashes to the identical value, by
        // `phash2/local_fun_via_its_imported_image`. See the module docs.
        Case {
            id: "phash2/local_fun_via_its_imported_image",
            description: "phash2/1,2 of a local fun's imported NEW_FUN_EXT image: identity items plus stability, range, a Range of 1 and distinct-fun/round-trip relations, classified into one integer",
            contracts: &[Contract::BeamAsm, Contract::OtpApps],
            kind: CaseKind::Module(ModuleRun {
                source: FUN_HASH_SOURCE,
                module: "fun_hash",
                function: "multi_closure_hash",
                arity: 0,
                args: &[],
                nif: None,
            }),
            golden: Golden {
                stdout: Some("7863"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash2/local_fun_make_fun3",
            description: "phash2/1,2 of a compiled module's make_fun3 local fun: the 27-bit full-hash mask relation, stability, the Range arithmetic, a Range of 1, the round-trip image identity and distinct-fun/free-variable relations, classified into one integer",
            contracts: &[Contract::BeamAsm, Contract::OtpApps],
            kind: CaseKind::Module(ModuleRun {
                source: FUN_HASH_MK_SOURCE,
                module: "fun_hash_mk",
                function: "local_fun_phash2",
                arity: 0,
                args: &[],
                nif: None,
            }),
            golden: Golden {
                stdout: Some("81511"),
                exit_code: 0,
            },
        },
        Case {
            id: "phash/legacy_local_fun",
            description: "phash/2 of a compiled module's make_fun3 local fun: the 1-based range answer, stability, the 2^32/65536 range arithmetic, a Range of 1, the round-trip image identity and distinct-fun/free-variable relations, classified into one integer",
            contracts: &[Contract::BeamAsm, Contract::OtpApps],
            kind: CaseKind::Module(ModuleRun {
                source: FUN_HASH_MK_SOURCE,
                module: "fun_hash_mk",
                function: "local_fun_phash",
                arity: 0,
                args: &[],
                nif: None,
            }),
            golden: Golden {
                stdout: Some("81255"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::{CaseKind, Contract};

    /// The eval half of the group: every one of those cases answers with the *value* probe, so a
    /// golden can never be an ETF image (which would be a `Probe::TermToBinary` answer), and
    /// every one claims A.7 only.
    #[test]
    fn eval_cases_are_value_probes_under_the_otp_apps_contract() {
        let cases = group();
        let mut eval_cases = 0;
        for case in &cases {
            let CaseKind::Eval { probe, .. } = &case.kind else {
                continue;
            };
            eval_cases += 1;
            assert_eq!(case.contracts, &[Contract::OtpApps], "{}", case.id);
            assert!(matches!(probe, super::Probe::Value), "{}", case.id);
            assert_eq!(case.golden.exit_code, 0, "{}", case.id);
            assert!(
                case.golden
                    .stdout
                    .is_some_and(|line| line.starts_with("ok ")),
                "{}: a value golden is the oracle's `ok <value>` line",
                case.id
            );
        }
        assert!(eval_cases >= 12, "the group keeps its eval cases");
    }

    /// The fun-hash module-run cases: they evidence the `.beam`/`NEW_FUN_EXT` byte contract (A.2)
    /// alongside the hash contract (A.7), and pin the compile-time identity items plus
    /// relations only — never a fun's hash integer (it moves with the compile-time source path)
    /// and never the fun's image bytes.
    #[test]
    fn the_local_fun_hash_case_is_a_module_run_over_the_imported_image() {
        let cases = group();
        let case = cases
            .iter()
            .find(|c| c.id == "phash2/local_fun_via_its_imported_image")
            .expect("registered");
        assert_eq!(
            case.contracts,
            &[Contract::BeamAsm, Contract::OtpApps],
            "a module-run case must evidence the .beam contract"
        );
        assert_eq!(case.kind_label(), "module-run");
        let CaseKind::Module(run) = &case.kind else {
            panic!("the fun-hash case runs a compiled module");
        };
        assert_eq!(run.module, "fun_hash");
        assert_eq!(run.function, "multi_closure_hash");
        assert_eq!(run.arity, 0);
        assert!(run.args.is_empty());
        assert!(run.nif.is_none());
        assert!(
            !run.source.contains("erlang:phash2(F)"),
            "this case hashes the imported image; the direct make_fun3 form is \
             `phash2/local_fun_make_fun3`"
        );
        // The oracle's integer line: `78 * 100 + 63` — the image size and the 6-bit mask of the
        // six hash relations, all of which hold.
        assert_eq!(case.golden.stdout, Some("7863"));
        assert_eq!(case.golden.exit_code, 0);
    }

    /// The two **direct** (`make_fun3`) hash ports: each is a module run over the same source, one
    /// per port, and each golden is the oracle's integer line — the image size in the high digits
    /// plus a relation mask that is all-ones (`511` for the nine `phash2` relations, `255` for the
    /// eight `phash` ones). The masks are the whole point: no hash integer is pinned, because
    /// `uniq` rides the module CRC and moves with the compile-time source path.
    #[test]
    fn the_direct_make_fun3_ports_pin_relation_masks_only() {
        let cases = group();
        for (id, function, golden) in [
            ("phash2/local_fun_make_fun3", "local_fun_phash2", "81511"),
            ("phash/legacy_local_fun", "local_fun_phash", "81255"),
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(
                case.contracts,
                &[Contract::BeamAsm, Contract::OtpApps],
                "{id}: a module-run case must evidence the .beam contract"
            );
            assert_eq!(case.kind_label(), "module-run", "{id}");
            let CaseKind::Module(run) = &case.kind else {
                panic!("{id}: the fun-hash cases run a compiled module");
            };
            assert_eq!(run.module, "fun_hash_mk", "{id}");
            assert_eq!(run.function, function, "{id}");
            assert_eq!(run.arity, 0, "{id}");
            assert!(run.args.is_empty(), "{id}");
            assert!(run.nif.is_none(), "{id}");
            // The direct form: the fun is hashed without being round-tripped first.
            assert!(
                run.source.contains("(F, 1 bsl 32)") && run.source.contains("(F, 1 bsl 27)"),
                "{id}: the case hashes the local fun directly"
            );
            assert_eq!(case.golden.stdout, Some(golden), "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
        }
    }
}
