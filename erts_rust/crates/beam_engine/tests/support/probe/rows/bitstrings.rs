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
//! The **bitstring / match-context** group.
//!
//! Owns the 16 bitstring variants:
//!
//! `BsMatch, BsMatchString, BsGetTail, BsGetPosition, BsSetPosition, BsGetInteger2,
//! BsGetFloat2, BsGetBinary2, BsSkipBits2, BsTestTail2, BsGetUtf8, BsGetUtf16,
//! BsGetUtf32, BsSkipUtf8, BsSkipUtf16, BsSkipUtf32`.
//!
//! # Mechanic — a real match context, over a real subject
//!
//! Every variant in this group takes a **match context** operand, and the only
//! instruction that can produce one is [`Instr::BsStartMatch3`] (`raw::make_ctx` is
//! `pub(crate)` to the engine). So each row:
//!
//! 1. seeds `x(0)` with the 4-byte [`SUBJECT`] (through [`Probe::args`], so
//!    `run_label_outcome` writes it into `x[0..arity)`);
//! 2. opens the match context **in place** — `BsStartMatch3 { bin: X(0), dst: X(0) }`
//!    — which is safe because the family re-reads the subject *before* it writes
//!    `dst` (`matching::exec`'s `enter_match` → `write_operand`), leaving
//!    `x(0)` a context at `start = 0, end = 32`;
//! 3. drives the variant against that context, with a destination in `x(1)` so the
//!    extracted value survives (`x(0)` *is* the context).
//!
//! `fail` is [`NO_FAIL`] on every `fail`-carrying variant (never the probe's success
//! or fail sink): a mis-shaped row must raise the visible `{badmatch, Subject}`
//! instead of silently "executing" through a branch.
//!
//! # The subject
//!
//! `[0x00, 0x00, 0x00, 0x61]` — three leading NUL bytes (required by `bs_get_utf32`,
//! which reads a whole 32-bit scalar) with ASCII `a` in the last byte, so
//! `BsGetUtf8`/`BsGetUtf16`/`BsGetUtf32` each read a valid character (`0`, `0`, `97`)
//! and the fixed reads see a leading zero byte.
//!
//! # Sizes
//!
//! Only sizes that actually fit the 32-bit subject are used: 8 bits for the integer /
//! binary reads and the skips, **16** for `BsGetFloat2` (a `f16` of `0x0000` is
//! `0.0`; the roster's `64` is wider than the subject and takes the fail branch), and
//! `32` for `BsTestTail2` (which compares the *whole* remaining window).
//!
//! # Pinned evidence
//!
//! A small-integer destination is moved into `x(0)` by an epilogue
//! (`Move X(1) → X(0)`) so the run returns it and the row pins the exact value —
//! `BsGetInteger2`/`BsGetUtf8`/`BsGetUtf16` → `0`, `BsGetUtf32` → `97`. A float, a
//! binary and a context term are **not** pin-able by value (their heap words are
//! addresses), so those rows declare the shape (`Expect::Returned(None)`) and record
//! the produced term in the note.

use beam_engine::terms::make_heap_binary;
use beam_engine::MinimalContext;
use beam_loader::{BsGetArgs, Instr, Operand, NO_FAIL};
use beam_term::{make_small, Eterm};

use super::super::{Expect, Probe, ProbeAtoms};

/// The 4-byte match subject every row binds to `x(0)`.
///
/// The three leading NUL bytes are what `bs_get_utf32` needs (a full 32-bit scalar
/// must be a valid Unicode value, and it is read from bit 0); byte 3 is ASCII `a`,
/// so a `bs_get_utf*` at an offset — or the 32-bit read itself — yields `97`.
const SUBJECT: [u8; 4] = [0x00, 0x00, 0x00, 0x61];

/// The argument builder: the subject as an **on-heap** binary in `x(0)`.
///
/// `make_heap_binary` accepts at most `ERL_ONHEAP_BINARY_LIMIT` (64) bytes, so a
/// 4-byte payload is always the on-heap `ErlHeapBits` form — and a non-zero
/// `x(0)` is what the row's `BsStartMatch3` prologue needs (`Some` binary).
fn subject_args(ctx: &mut MinimalContext) -> Vec<Eterm> {
    vec![
        make_heap_binary(ctx, &SUBJECT)
            .expect("a 4-byte heap binary is well inside the 64-byte on-heap limit"),
    ]
}

/// The shared prologue: open a match context over `x(0)`, **in place**.
///
/// `bin == dst == X(0)` is the shape `beam_asm:filter_essentials/1`-style code uses,
/// and it is safe here because `matching::exec` reads the subject (`enter_match`)
/// before it writes `dst`. Afterwards `x(0)` is the context at `start = 0`,
/// `end = 32`.
fn start_match() -> Vec<Instr> {
    vec![Instr::BsStartMatch3 {
        fail: NO_FAIL,
        bin: Operand::X(0),
        live: 1,
        dst: Operand::X(0),
    }]
}

/// The epilogue that returns the value an extraction bound into `x(1)`.
fn return_x1() -> Vec<Instr> {
    vec![Instr::Move {
        src: Operand::X(1),
        dst: Operand::X(0),
    }]
}

/// The shared `bs_get_integer2`/`_float2`/`_binary2` operand shape:
/// `Fail Ctx Live Size Unit Flags Dst`, flags `0` (big-endian, unsigned, no `native`).
fn get_args(size: i64) -> Box<BsGetArgs> {
    Box::new(BsGetArgs {
        fail: NO_FAIL,
        ctx: Operand::X(0),
        live: 1,
        size: Operand::Integer(size),
        unit: 1,
        flags: 0,
        dst: Operand::X(1),
    })
}

/// The bitstring group's converted rows.
pub fn rows(atoms: &ProbeAtoms) -> Vec<Probe> {
    vec![
        bs_match(atoms),
        bs_match_string(),
        bs_get_tail(),
        bs_get_position(),
        bs_set_position(),
        bs_get_integer2(),
        bs_get_float2(),
        bs_get_binary2(),
        bs_skip_bits2(),
        bs_test_tail2(),
        bs_get_utf8(),
        bs_get_utf16(),
        bs_get_utf32(),
        bs_skip_utf8(),
        bs_skip_utf16(),
        bs_skip_utf32(),
    ]
}

/// `bs_match Fail Ctx Commands` — the flattened command interpreter.
///
/// One `integer` command (`integer Live Flags Size Unit Dst`) reads 8 bits of the
/// subject into `x(1)` and **commits** the new context position, `0 → 8`; the value
/// is `0x00` (the subject's leading byte).
///
/// The command atom comes from the canonical universe (`integer` is interned there),
/// and the command loop resolves it **by name** through the module's own atom table.
fn bs_match(atoms: &ProbeAtoms) -> Probe {
    Probe::new(
        Instr::BsMatch {
            fail: NO_FAIL,
            ctx: Operand::X(0),
            commands: vec![
                Operand::Atom(atoms.named("integer")),
                Operand::Integer(1), // Live
                Operand::Nil,        // Flags
                Operand::Integer(8), // Size
                Operand::Integer(1), // Unit
                Operand::X(1),       // Dst
            ],
        },
        Expect::Returned(None),
    )
    .setup(start_match())
    .args(subject_args)
    .note("the command loop reads 8 bits into x(1) and commits the position 0 -> 8")
}

/// `bs_match_string Fail Ctx Bits Offset` — a whole-window bit comparison.
///
/// The `StrT` table holds the subject itself, so the 32-bit comparison matches and the
/// position advances `0 → 32`.
fn bs_match_string() -> Probe {
    Probe::new(
        Instr::BsMatchString {
            fail: NO_FAIL,
            ctx: Operand::X(0),
            bits: 32,
            offset: 0,
        },
        Expect::Returned(None),
    )
    .setup(start_match())
    .strings(SUBJECT.to_vec())
    .args(subject_args)
    .note("all 32 bits compared against the StrT pattern, position 0 -> 32")
}

/// `bs_get_tail Ctx Dst Live` — bind the remaining bits, leaving the position alone.
fn bs_get_tail() -> Probe {
    Probe::new(
        Instr::BsGetTail {
            ctx: Operand::X(0),
            dst: Operand::X(1),
            live: 1,
        },
        Expect::Returned(None),
    )
    .setup(start_match())
    .args(subject_args)
    .note("x(1) = <<0,0,0,97>> (the whole 32-bit window), position unchanged at 0")
}

/// `bs_get_position Ctx Dst Live` — snapshot the (start 0) position.
fn bs_get_position() -> Probe {
    Probe::new(
        Instr::BsGetPosition {
            ctx: Operand::X(0),
            dst: Operand::X(1),
            live: 1,
        },
        Expect::Returned(None),
    )
    .setup(start_match())
    .args(subject_args)
    .note("x(1) is a snapshot match context whose start is the current 0")
}

/// `bs_set_position Ctx Pos` — restore a position.
///
/// The restored position is the already-current `0`, so the row proves the accept path
/// (a skip-then-restore shape lives in `tests/bitstrings_extra_bits.rs`); the operand
/// may be a small integer (`matching::position_of`).
fn bs_set_position() -> Probe {
    Probe::new(
        Instr::BsSetPosition {
            ctx: Operand::X(0),
            pos: Operand::Integer(0),
        },
        Expect::Returned(None),
    )
    .setup(start_match())
    .args(subject_args)
    .note("restores the (already 0) start from a small-integer position operand")
}

/// `bs_get_integer2 Fail Ctx Live Size Unit Flags Dst` — an 8-bit big-endian read.
fn bs_get_integer2() -> Probe {
    Probe::new(
        Instr::BsGetInteger2(get_args(8)),
        Expect::Returned(Some(make_small(0))),
    )
    .setup(start_match())
    .epilogue(return_x1())
    .args(subject_args)
    .note("8 bits of 0x00 -> x(1) = 0, position 0 -> 8")
}

/// `bs_get_float2 Fail Ctx Live Size Unit Flags Dst` — a 16-bit (`f16`) read.
///
/// Size **16**, not the roster's 64: 64 bits exceed the 32-bit subject and would take
/// the fail branch. `0x0000` decodes to `0.0`; a float term cannot be pinned by value
/// (its word is a heap pointer), so the row pins the shape.
fn bs_get_float2() -> Probe {
    Probe::new(
        Instr::BsGetFloat2(get_args(16)),
        Expect::Returned(None),
    )
    .setup(start_match())
    .args(subject_args)
    .note("x(1) = 0.0 (f16 0x0000), position 0 -> 16")
}

/// `bs_get_binary2 Fail Ctx Live Size Unit Flags Dst` — an 8-bit window.
fn bs_get_binary2() -> Probe {
    Probe::new(
        Instr::BsGetBinary2(get_args(8)),
        Expect::Returned(None),
    )
    .setup(start_match())
    .args(subject_args)
    .note("x(1) = <<0>> (an 8-bit sub-binary), position 0 -> 8")
}

/// `bs_skip_bits2 Fail Ctx Size Unit Flags` — advance without binding.
fn bs_skip_bits2() -> Probe {
    Probe::new(
        Instr::BsSkipBits2 {
            fail: NO_FAIL,
            ctx: Operand::X(0),
            size: Operand::Integer(8),
            unit: 1,
            flags: 0,
        },
        Expect::Returned(None),
    )
    .setup(start_match())
    .args(subject_args)
    .note("position 0 -> 8 and no destination is bound")
}

/// `bs_test_tail2 Fail Ctx Bits` — test the size of the remaining window.
///
/// `32` remaining bits against `bits: 32` is exactly the whole subject, so the test
/// succeeds. Real compiler output never emits this opcode (it is in the census's
/// `ZERO_CORPUS` set), which is why the census buckets the row as `ZERO-CORPUS`.
fn bs_test_tail2() -> Probe {
    Probe::new(
        Instr::BsTestTail2 {
            fail: NO_FAIL,
            ctx: Operand::X(0),
            bits: 32,
        },
        Expect::Returned(None),
    )
    .setup(start_match())
    .args(subject_args)
    .note("the remaining window is exactly 32 bits, so the tail test succeeds")
}

/// `bs_get_utf8 Fail Ctx Live Flags Dst` — the subject's first byte is NUL.
fn bs_get_utf8() -> Probe {
    Probe::new(
        Instr::BsGetUtf8 {
            fail: NO_FAIL,
            ctx: Operand::X(0),
            live: 1,
            flags: 0,
            dst: Operand::X(1),
        },
        Expect::Returned(Some(make_small(0))),
    )
    .setup(start_match())
    .epilogue(return_x1())
    .args(subject_args)
    .note("byte 0x00 -> x(1) = 0, position 0 -> 8")
}

/// `bs_get_utf16 Fail Ctx Live Flags Dst` — the first 16 bits are `0x0000`.
fn bs_get_utf16() -> Probe {
    Probe::new(
        Instr::BsGetUtf16 {
            fail: NO_FAIL,
            ctx: Operand::X(0),
            live: 1,
            flags: 0,
            dst: Operand::X(1),
        },
        Expect::Returned(Some(make_small(0))),
    )
    .setup(start_match())
    .epilogue(return_x1())
    .args(subject_args)
    .note("the 16-bit unit 0x0000 -> x(1) = 0, position 0 -> 16")
}

/// `bs_get_utf32 Fail Ctx Live Flags Dst` — the whole 32-bit scalar is `0x61` = `a`.
///
/// This is why the subject carries three leading NUL bytes: a 32-bit read from bit 0
/// must be a valid Unicode scalar, and it pins the largest value in the group (`97`).
fn bs_get_utf32() -> Probe {
    Probe::new(
        Instr::BsGetUtf32 {
            fail: NO_FAIL,
            ctx: Operand::X(0),
            live: 1,
            flags: 0,
            dst: Operand::X(1),
        },
        Expect::Returned(Some(make_small(97))),
    )
    .setup(start_match())
    .epilogue(return_x1())
    .args(subject_args)
    .note("the 32-bit scalar 0x00000061 -> x(1) = 97, position 0 -> 32")
}

/// `bs_skip_utf8 Fail Ctx Live Flags` — skip one UTF-8 character (the NUL byte).
fn bs_skip_utf8() -> Probe {
    Probe::new(
        Instr::BsSkipUtf8 {
            fail: NO_FAIL,
            ctx: Operand::X(0),
            live: 1,
            flags: 0,
        },
        Expect::Returned(None),
    )
    .setup(start_match())
    .args(subject_args)
    .note("one UTF-8 character (1 byte) skipped, position 0 -> 8")
}

/// `bs_skip_utf16 Fail Ctx Live Flags` — skip one UTF-16 unit.
fn bs_skip_utf16() -> Probe {
    Probe::new(
        Instr::BsSkipUtf16 {
            fail: NO_FAIL,
            ctx: Operand::X(0),
            live: 1,
            flags: 0,
        },
        Expect::Returned(None),
    )
    .setup(start_match())
    .args(subject_args)
    .note("one UTF-16 unit skipped, position 0 -> 16")
}

/// `bs_skip_utf32 Fail Ctx Live Flags` — skip one UTF-32 scalar.
fn bs_skip_utf32() -> Probe {
    Probe::new(
        Instr::BsSkipUtf32 {
            fail: NO_FAIL,
            ctx: Operand::X(0),
            live: 1,
            flags: 0,
        },
        Expect::Returned(None),
    )
    .setup(start_match())
    .args(subject_args)
    .note("one UTF-32 scalar skipped, position 0 -> 32")
}
