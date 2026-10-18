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

//! **Bit-syntax variants on real compiler output**: `BsMatch`, `BsSetPosition`,
//! `BsGetPosition`, `BsGetTail`, `BsGetBinary2`, `BsSkipBits2`, `BsGetUtf16`.
//!
//! The bit-syntax match compiler emits these from ordinary Erlang patterns
//! (`<<A:8,B/binary>>`, `<<C/utf16>>`, a sized/`bits` segment, a failing segment
//! that must fall through to the next clause), so every row here is written in
//! plain Erlang and compiled with the system `erlc`.
//!
//! See the crate docs of `real_beam_variant_proofs.rs` for the proof shape and
//! `harness` for the shared implementation.
//!
//! # The fixture, and why each shape compiles to the variant it proves
//!
//! Every row drives one export of the committed `tests/fixtures/vb_bitstr.beam`
//! (built from `vb_bitstr.erl` with the system `erlc`; the regeneration recipe —
//! including the `+deterministic` flag that keeps the worktree path out of the
//! binary — is in `tests/fixtures/README.md`). The instruction each export
//! compiles to was **verified with `beam_disasm:file/1` before the row was
//! written**, because the bit-syntax code generator is free to fuse a whole
//! pattern into one `bs_match` command list — "this Erlang source exercises
//! variant X" is a claim to check, not to assume:
//!
//! | export | disassembly | variant |
//! |---|---|---|
//! | `flat/1` | `test bs_start_match3` + `bs_get_position` + `bs_match …[{ensure_at_least,32,8},{integer,…},{integer,…},{get_tail,…}]` | `BsMatch`, `BsGetPosition` |
//! | `rewind/1` | the first clause's fail block runs `bs_set_position` and re-matches from the restored position | `BsSetPosition` |
//! | `whole/2` | `test bs_get_binary2` + `bs_match …[{ensure_at_least,0,8},{get_tail,…}]` + `bs_set_position` + a **standalone** `bs_get_tail` | `BsGetBinary2`, `BsSetPosition`, `BsGetTail` |
//! | `sized/2` | `test bs_get_binary2` (a `case`, so its fail block raises) | `BsGetBinary2` |
//! | `skip/2` | `test bs_skip_bits2` | `BsSkipBits2` |
//! | `utf16/1` | `test bs_get_utf16` | `BsGetUtf16` |
//!
//! Two of those are worth spelling out, because the obvious guess is wrong:
//!
//! * **`bs_get_tail` is normally a match *command*, not an instruction.** For
//!   `case B of <<_:8, R/binary>> -> R end` the compiler folds the tail read into
//!   the `bs_match` command list (`{get_tail,…}`), so the `BsGetTail` instruction
//!   never runs. The standalone instruction is emitted where the tail read cannot
//!   be a command: in `whole/2` the match context must first be **rewound** so the
//!   original subject can be returned, and the reads either side of the rewind
//!   cannot share one command list. The same instruction is also reached on the
//!   compiler's failure path (rewind, tail, `case_end`), which the short-subject
//!   row drives.
//! * **`bs_set_position` is normally on a rewind/recovery path.** In `flat/1`'s
//!   fail block it runs and then jumps to `func_info`; in `rewind/1`'s first-clause
//!   fail block it runs and the second clause *succeeds* from the restored
//!   position, so that row's observable is a returned value rather than a raise.
//!
//! Nothing here is guessed: every expected value is the live system `erl`'s own
//! `~w` rendering of the same call (`harness::oracle_w`), and the raising row
//! compares the engine's **live reason term** against the oracle's
//! `try … catch error:R -> R end` reason.

use beam_common::BeamError;
use beam_engine::terms;
use beam_term::{make_small, Eterm};

use crate::harness::{host, oracle_w, render, run, skip_if_unavailable, Proof};
use crate::support::ModuleHost;

/// The committed fixture (`erlc +deterministic vb_bitstr.erl`).
const FIXTURE: &str = "vb_bitstr.beam";
const MODULE: &str = "vb_bitstr";

/// A [`ModuleHost`] with the fixture registered (plus the system stdlib/kernel, so
/// the fixture's own BIF calls resolve).
fn fixture_host() -> ModuleHost {
    host(&[(MODULE, FIXTURE)])
}

/// A binary argument built on the host's own heap arena, so the fixture reads it
/// with exactly the term layout it was given.
fn bin(host: &mut ModuleHost, bytes: &[u8]) -> Eterm {
    terms::make_heap_binary(host, bytes).expect("a heap binary fits the host arena")
}

/// A small-integer argument.
fn small(value: i64) -> Eterm {
    make_small(value)
}

/// Asserts the engine's returned term renders exactly as the live oracle's `~w`
/// rendering of `expr`, printing both sides on failure.
#[track_caller]
fn assert_matches_oracle(host: &ModuleHost, got: Eterm, expr: &str) {
    let rendered = render(host, got);
    let oracle = oracle_w(expr);
    assert_eq!(
        rendered, oracle,
        "engine vs oracle for `{expr}` (engine `{got:?}`)"
    );
}

/// `1,2,3` — the byte list spelling the oracle expression uses for a binary
/// literal (`<<1,2,3>>`).
fn int_list(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| byte.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

// ---------------------------------------------------------------------------
// `bs_match` — the flattened command list
// ---------------------------------------------------------------------------

/// `BsMatch`: `flat/1`'s two literal-size reads and its tail all live in **one**
/// `bs_match` command list (`ensure_at_least` + two `integer` + `get_tail`), which
/// is the shape the bit-syntax code generator prefers whenever a pattern's reads
/// have literal sizes. The same run also executes the `bs_get_position` the
/// compiler emits so the fail block can rewind.
#[test]
fn bs_match_executes_the_flattened_command_list() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = fixture_host();
    let arg = bin(&mut host, &[0, 1, 0, 2, 3, 4]);
    let proof = run(&mut host, MODULE, "flat", &[arg]);
    proof.require(&["BsMatch", "BsGetPosition"]);
    eprintln!(
        "BsMatch: flat/1 executed bs_match {} times (bs_get_position {})",
        proof.executed("BsMatch"),
        proof.executed("BsGetPosition")
    );
    assert_matches_oracle(&host, proof.value(), "vb_bitstr:flat(<<0,1,0,2,3,4>>)");
}

// ---------------------------------------------------------------------------
// `bs_get_position` — the position the compiler saves for a rewind
// ---------------------------------------------------------------------------

/// `BsGetPosition`: `rewind/1` saves the position with `bs_get_position` **twice**
/// on the path the row drives (once when the match starts, once before the
/// literal-prefix test), and the second snapshot is what the fail block restores.
///
/// The same call also executes `bs_set_position`, so the two halves of one rewind
/// are observed together rather than each in isolation.
#[test]
fn bs_get_position_saves_the_position_a_rewind_restores() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = fixture_host();
    let arg = bin(&mut host, &[1, 2, 3]);
    let proof = run(&mut host, MODULE, "rewind", &[arg]);
    proof.require(&["BsGetPosition", "BsSetPosition"]);
    assert!(
        proof.executed("BsGetPosition") >= 2,
        "rewind/1 should snapshot the position before the literal test as well\n{}",
        proof.report()
    );
    eprintln!(
        "BsGetPosition: rewind/1 executed bs_get_position {} times",
        proof.executed("BsGetPosition")
    );
    assert_matches_oracle(&host, proof.value(), "vb_bitstr:rewind(<<1,2,3>>)");
}

// ---------------------------------------------------------------------------
// `bs_set_position` — the rewind that lets the next clause succeed
// ---------------------------------------------------------------------------

/// `BsSetPosition` on a **success path**: `rewind/1`'s first clause carries the
/// literal prefix `"ab"`, so the compiler emits a fail block that restores the
/// saved position with `bs_set_position` and re-matches from there for the second
/// clause. A subject that does not start with `"ab"` therefore runs
/// `bs_set_position` and then returns a value.
///
/// Both clauses are checked: the same export on a subject that *does* start with
/// the prefix takes the other path, so the rewind is genuinely conditional and the
/// row is not just an unconditional branch.
#[test]
fn bs_set_position_rewinds_so_the_next_clause_can_match() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = fixture_host();
    let other = bin(&mut host, &[1, 2, 3]);
    let proof = run(&mut host, MODULE, "rewind", &[other]);
    proof.require(&["BsSetPosition"]);
    eprintln!(
        "BsSetPosition: rewind/1 executed bs_set_position {} times",
        proof.executed("BsSetPosition")
    );
    assert_matches_oracle(&host, proof.value(), "vb_bitstr:rewind(<<1,2,3>>)");

    let prefixed = bin(&mut host, b"ab\x01\x02");
    let proof = run(&mut host, MODULE, "rewind", &[prefixed]);
    assert_matches_oracle(&host, proof.value(), r#"vb_bitstr:rewind(<<"ab",1,2>>)"#);
}

// ---------------------------------------------------------------------------
// `bs_get_tail` — a standalone tail read from a restored position
// ---------------------------------------------------------------------------

/// `BsGetTail`: `whole/2` binds `<<C:N/binary, R/binary>>` and also returns the
/// whole subject `B`. The compiler keeps `R` as a `bs_match` command but yields
/// `B` by rewinding the context and taking the tail, so the standalone
/// `bs_get_tail` instruction runs **on the success path** — the row that no
/// `case B of <<_:8, R/binary>> -> R end` shape can produce (see the module docs).
///
/// The observable is the returned `{B, C, R}` triple, so a wrong position, a wrong
/// sized window or a wrong tail all show up in one oracle-checked comparison. Two
/// sizes are driven: a middle cut, and a zero-byte head (where `C` is the empty
/// binary and `B`/`R` are the whole subject).
#[test]
fn bs_get_tail_reads_the_subject_from_the_restored_position() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = fixture_host();
    for (bytes, size) in [(vec![1u8, 2, 3, 4], 2i64), (vec![1, 2, 3], 0)] {
        let subject = bin(&mut host, &bytes);
        let proof = run(&mut host, MODULE, "whole", &[subject, small(size)]);
        proof.require(&["BsGetTail", "BsSetPosition", "BsGetBinary2"]);
        eprintln!(
            "BsGetTail: whole/2 (size {size}) executed bs_get_tail {} times",
            proof.executed("BsGetTail")
        );
        let expr = format!("vb_bitstr:whole(<<{}>>, {size})", int_list(&bytes));
        assert_matches_oracle(&host, proof.value(), &expr);
    }
}

// ---------------------------------------------------------------------------
// `bs_get_binary2` — a sized binary segment
// ---------------------------------------------------------------------------

/// `BsGetBinary2`: `sized/2` binds `<<X:N/binary, R/binary>>` with a **dynamic**
/// segment size, which is `{test,bs_get_binary2, Fail, Live, [Ctx, Size, 8,
/// Flags], X}`. The observable is the pair of the sized window and the rest, at
/// three sizes: a middle cut, the whole subject (empty rest) and zero bytes.
#[test]
fn bs_get_binary2_reads_a_dynamically_sized_segment() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = fixture_host();
    for (bytes, size) in [
        (vec![1u8, 2, 3, 4], 2i64),
        (vec![1, 2, 3, 4], 4),
        (vec![1, 2, 3, 4], 0),
    ] {
        let subject = bin(&mut host, &bytes);
        let proof = run(&mut host, MODULE, "sized", &[subject, small(size)]);
        proof.require(&["BsGetBinary2"]);
        eprintln!(
            "BsGetBinary2: sized/2 (size {size}) executed bs_get_binary2 {} times",
            proof.executed("BsGetBinary2")
        );
        let expr = format!("vb_bitstr:sized(<<{}>>, {size})", int_list(&bytes));
        assert_matches_oracle(&host, proof.value(), &expr);
    }
}

// ---------------------------------------------------------------------------
// `bs_skip_bits2` — a dynamically sized skip
// ---------------------------------------------------------------------------

/// `BsSkipBits2`: `skip/2` skips a **dynamic** number of bits
/// (`<<_:N/unit:8, R/binary>>`), which is `{test,bs_skip_bits2, Fail, [Ctx, Size,
/// 8, Flags]}`. The returned `{N, Rest}` is oracle-checked for a middle skip, a
/// whole-subject skip and a zero-byte skip (where the "rest" is the subject).
#[test]
fn bs_skip_bits2_skips_a_dynamically_sized_window() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = fixture_host();
    for (bytes, size) in [
        (vec![1u8, 2, 3], 2i64),
        (vec![1, 2, 3], 3),
        (vec![1, 2, 3], 0),
    ] {
        let subject = bin(&mut host, &bytes);
        let proof = run(&mut host, MODULE, "skip", &[subject, small(size)]);
        proof.require(&["BsSkipBits2"]);
        eprintln!(
            "BsSkipBits2: skip/2 (size {size}) executed bs_skip_bits2 {} times",
            proof.executed("BsSkipBits2")
        );
        let expr = format!("vb_bitstr:skip(<<{}>>, {size})", int_list(&bytes));
        assert_matches_oracle(&host, proof.value(), &expr);
    }
}

// ---------------------------------------------------------------------------
// `bs_get_utf16` — a big-endian UTF-16 scalar
// ---------------------------------------------------------------------------

/// `BsGetUtf16`: `utf16/1` is `<<C/utf16>>` in a function head, i.e.
/// `{test,bs_get_utf16, Fail, [Ctx, Live, Flags, C]}`. The rows cover a BMP scalar
/// (U+20AC, two bytes) and a **surrogate pair** encoding a supplementary scalar
/// (U+1F600, four bytes), which is the multi-unit path distinct from the
/// single-unit one.
#[test]
fn bs_get_utf16_decodes_a_big_endian_scalar() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = fixture_host();
    for bytes in [vec![32u8, 172], vec![216, 61, 222, 0]] {
        let subject = bin(&mut host, &bytes);
        let proof = run(&mut host, MODULE, "utf16", &[subject]);
        proof.require(&["BsGetUtf16"]);
        eprintln!(
            "BsGetUtf16: utf16/1 (<<{}>>) executed bs_get_utf16 {} times",
            int_list(&bytes),
            proof.executed("BsGetUtf16")
        );
        let expr = format!("vb_bitstr:utf16(<<{}>>)", int_list(&bytes));
        assert_matches_oracle(&host, proof.value(), &expr);
    }
}

// ---------------------------------------------------------------------------
// The short-subject paths: a typed raise, compared against the oracle's reason
// ---------------------------------------------------------------------------

/// The **rewind path of a failed bit-syntax match**: when the sized-binary read,
/// the dynamic skip, or the `badmatch`-form match cannot be satisfied, the emitted
/// fail block runs `bs_set_position` and the standalone `bs_get_tail` and then
/// raises — with the subject **rewound to the start**, exactly as the oracle
/// reports it.
///
/// This is the row that proves the standalone `BsGetTail` instruction on the
/// compiler's *failure* path (the success path is
/// `bs_get_tail_reads_the_subject_from_the_restored_position`). The engine's live
/// reason term is rendered by the same `render` and compared with the oracle's
/// `try … catch error:R -> R end` reason, so the whole tuple is checked, not just
/// the tag atom.
#[test]
fn a_short_subject_rewinds_then_raises_case_clause() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = fixture_host();

    let short_sized = bin(&mut host, &[1]);
    let proof = run(&mut host, MODULE, "sized", &[short_sized, small(2)]);
    proof.require(&["BsSetPosition", "BsGetTail", "CaseEnd"]);
    eprintln!(
        "short subject: sized/2 executed bs_set_position={} bs_get_tail={} case_end={}",
        proof.executed("BsSetPosition"),
        proof.executed("BsGetTail"),
        proof.executed("CaseEnd")
    );
    let reason = live_reason(&proof);
    assert_matches_oracle(
        &host,
        reason,
        "try vb_bitstr:sized(<<1>>, 2) catch error:R -> R end",
    );

    let short_skip = bin(&mut host, &[1]);
    let proof = run(&mut host, MODULE, "skip", &[short_skip, small(5)]);
    proof.require(&["BsSkipBits2", "CaseEnd"]);
    let reason = live_reason(&proof);
    assert_matches_oracle(
        &host,
        reason,
        "try vb_bitstr:skip(<<1>>, 5) catch error:R -> R end",
    );

    // The `badmatch` form of the same sized read: `whole/2` binds the segments with
    // `=`, so the fail block ends on `badmatch` instead of `case_end`.
    let short_whole = bin(&mut host, &[1]);
    let proof = run(&mut host, MODULE, "whole", &[short_whole, small(2)]);
    proof.require(&["BsGetBinary2", "BsSetPosition", "BsGetTail", "Badmatch"]);
    let reason = live_reason(&proof);
    assert_matches_oracle(
        &host,
        reason,
        "try vb_bitstr:whole(<<1>>, 2) catch error:R -> R end",
    );
}

/// The engine's **live reason term** of a raising [`Proof`], panicking (with the
/// census) when the run returned normally or the raise carries no reason term.
#[track_caller]
fn live_reason(proof: &Proof) -> Eterm {
    match proof.value {
        Err(BeamError::Raise(ref raise)) => {
            let live = raise.live_reason().unwrap_or_else(|| {
                panic!("the raise carries no live reason term\n{}", proof.report())
            });
            *live.downcast_ref::<Eterm>().unwrap_or_else(|| {
                panic!(
                    "the raise's live reason payload is not an `Eterm`\n{}",
                    proof.report()
                )
            })
        }
        ref other => panic!(
            "expected a raise, the run produced {other:?}\n{}",
            proof.report()
        ),
    }
}
