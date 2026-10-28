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
//! The decoded-operand vector: [`RawOperands`].
//!
//! [`crate::decode::RawInstr`] holds the operands of one decoded instruction. A
//! `Vec<Operand>` there would cost one heap allocation **per instruction** — a bare
//! `beam_rt` boot decodes ~304 000 instructions, i.e. ~304 000 malloc/free pairs on
//! the load path — while almost every instruction has at most three operands
//! (weighted by the `genop.tab` arity: 0 ⇒ 5.0 %, 1 ⇒ 30.1 %, 2 ⇒ 47.8 %, 3 ⇒
//! 14.6 %, 4 ⇒ 0.76 %, 5 ⇒ 1.56 %, 6 ⇒ 0.09 %). So the first
//! [`INLINE_OPERANDS`] operands live inside the value and only a wider instruction
//! spills to the heap: **≈97.5 % of decoded instructions allocate nothing**.
//!
//! # Layout
//!
//! `Operand` is 16 bytes, so the inline payload is 48 bytes and the whole type is
//! **56 bytes** (48 + `len` + the discriminant, both in the trailing padding):
//! `size_of::<RawInstr>()` stays **64** (1 byte opcode + 56 + the `u32` offset),
//! which `tests` pins. Spilling never returns to the inline form, so an
//! `Inline`'s tail slots stay [`Operand::Nil`] for the value's whole life.
//!
//! This type is decode-time only: it is not part of the frozen IR (`Instr` owns
//! `Vec<Operand>` and its layout is pinned), and lowering materializes a `Vec` only
//! for the variants that need one.

use crate::operand::Operand;
use core::ops::{Deref, DerefMut};

/// How many operands an [`RawOperands`] holds without a heap allocation.
///
/// Three covers ≈97.5 % of the instructions in the OTP corpus (see the module
/// docs) and is exactly what keeps [`crate::decode::RawInstr`] at 64 bytes: three
/// 16-byte [`Operand`]s plus the length and the discriminant fit its padding.
pub const INLINE_OPERANDS: usize = 3;

/// The operands of one decoded instruction: inline up to [`INLINE_OPERANDS`], then
/// a heap vector.
///
/// Built by [`push`](RawOperands::push) from [`crate::decode::decode_stream`] (in
/// file operand order) and read as a slice through [`Deref`], so every reader —
/// lowering, validation, the tests — indexes and iterates it exactly like the
/// `Vec<Operand>` it replaces.
#[derive(Clone, Debug, PartialEq)]
pub enum RawOperands {
    /// Up to [`INLINE_OPERANDS`] operands stored in the value itself.
    Inline {
        /// How many of `items` are initialized. [`Deref`] clamps it to `items.len()`,
        /// so a hand-set out-of-range length can never index out of bounds.
        len: u8,
        /// The initialized operands, in file order; `items[len..]` is inert.
        items: [Operand; INLINE_OPERANDS],
    },
    /// More than [`INLINE_OPERANDS`] operands, in a heap vector.
    Spilled(Vec<Operand>),
}

impl RawOperands {
    /// An empty inline operand vector (no allocation).
    pub fn new() -> RawOperands {
        RawOperands::default()
    }

    /// Appends `operand`, spilling to the heap when the inline payload is full.
    ///
    /// The spill moves the [`INLINE_OPERANDS`] resident operands into a vector with
    /// room for this one, so it reallocates at most once and never copies an
    /// `Operand`.
    ///
    /// `#[inline]`: the decoder calls this once per decoded operand (≈1.3 M times for a
    /// bare `beam_rt` boot) and every one of those calls but the spill is the
    /// two-instruction inline arm — as its own symbol it was 0.76 % of the load path's
    /// retired instructions.
    #[inline]
    pub fn push(&mut self, operand: Operand) {
        match self {
            RawOperands::Inline { len, items } => {
                let used = usize::from(*len);
                if used < INLINE_OPERANDS {
                    items[used] = operand;
                    *len += 1;
                    return;
                }
                // The array is full: move its operands out (leaving inert `Nil`s) and
                // hand them, plus this operand, to a freshly allocated vector.
                let moved = core::mem::replace(items, [const { Operand::Nil }; INLINE_OPERANDS]);
                *len = 0;
                let mut spilled = Vec::with_capacity(INLINE_OPERANDS + 1);
                spilled.extend(moved);
                spilled.push(operand);
                *self = RawOperands::Spilled(spilled);
            }
            RawOperands::Spilled(items) => items.push(operand),
        }
    }

    /// The number of operands (the length of the slice [`Deref`] yields).
    pub fn len(&self) -> usize {
        match self {
            RawOperands::Inline { len, items } => usize::from(*len).min(items.len()),
            RawOperands::Spilled(items) => items.len(),
        }
    }

    /// Whether the instruction has no operands.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for RawOperands {
    /// An empty inline vector — the shape of every decoded instruction's operand
    /// list before its first [`push`](RawOperands::push).
    fn default() -> RawOperands {
        RawOperands::Inline {
            len: 0,
            items: [const { Operand::Nil }; INLINE_OPERANDS],
        }
    }
}

impl Deref for RawOperands {
    type Target = [Operand];

    /// The initialized operands, in file order (inline ⇒ `&items[..len]`, spilled ⇒
    /// the vector's elements).
    fn deref(&self) -> &[Operand] {
        match self {
            RawOperands::Inline { items, .. } => &items[..self.len()],
            RawOperands::Spilled(items) => items,
        }
    }
}

impl DerefMut for RawOperands {
    /// The operands as a mutable slice.
    ///
    /// It can only reach the **initialized prefix** (the inline arm slices at
    /// `len`), so a caller such as the wide-integer rewrite in
    /// [`crate::lower`] can replace operands but cannot lengthen or shorten the
    /// vector behind [`push`](RawOperands::push).
    fn deref_mut(&mut self) -> &mut [Operand] {
        let len = self.len();
        match self {
            RawOperands::Inline { items, .. } => &mut items[..len],
            RawOperands::Spilled(items) => items,
        }
    }
}

impl From<Vec<Operand>> for RawOperands {
    /// Adopts an existing vector: a wider-than-inline one keeps its allocation,
    /// otherwise the operands move into the inline payload (and the vector is
    /// freed).
    fn from(operands: Vec<Operand>) -> RawOperands {
        if operands.len() > INLINE_OPERANDS {
            return RawOperands::Spilled(operands);
        }
        let mut out = RawOperands::default();
        for operand in operands {
            out.push(operand);
        }
        out
    }
}

impl FromIterator<Operand> for RawOperands {
    fn from_iter<I: IntoIterator<Item = Operand>>(iter: I) -> RawOperands {
        let mut out = RawOperands::default();
        for operand in iter {
            out.push(operand);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::RawInstr;
    use crate::opcode::Opcode;

    /// The inline payload must be exactly three operands: it is what keeps
    /// `RawInstr` at 64 bytes (the decode array's density).
    #[test]
    fn raw_operands_and_raw_instr_keep_their_sizes() {
        assert_eq!(core::mem::size_of::<Operand>(), 16);
        assert_eq!(core::mem::size_of::<RawOperands>(), 56);
        assert_eq!(core::mem::size_of::<RawInstr>(), 64);
    }

    /// The operand at `index` as an `i` integer payload (the test's stand-in for a
    /// decoded operand).
    fn int(index: usize) -> Operand {
        Operand::Integer(index as i64)
    }

    /// Arity 0..=3 stays inline and preserves order and length.
    #[test]
    fn an_inline_operand_list_preserves_order_and_length() {
        for arity in 0..=INLINE_OPERANDS {
            let mut ops = RawOperands::default();
            assert!(ops.is_empty());
            for index in 0..arity {
                ops.push(int(index));
            }
            assert!(
                matches!(ops, RawOperands::Inline { .. }),
                "arity {arity} must stay inline"
            );
            assert_eq!(ops.len(), arity);
            assert_eq!(ops.is_empty(), arity == 0);
            assert_eq!(ops.to_vec(), (0..arity).map(int).collect::<Vec<_>>());
        }
    }

    /// Arity 4..=6 spills to the heap, preserving order and length.
    #[test]
    fn a_wider_operand_list_spills_and_preserves_order_and_length() {
        for arity in (INLINE_OPERANDS + 1)..=6 {
            let mut ops = RawOperands::default();
            for index in 0..arity {
                ops.push(int(index));
            }
            assert!(
                matches!(ops, RawOperands::Spilled(_)),
                "arity {arity} must spill"
            );
            assert_eq!(ops.len(), arity);
            assert_eq!(ops.to_vec(), (0..arity).map(int).collect::<Vec<_>>());
        }
    }

    /// A pushed-after-spill operand appends to the vector, not to the (stale) inline
    /// array, and `DerefMut` reaches every one of them.
    #[test]
    fn a_spilled_list_keeps_growing_and_is_mutable_through_its_prefix() {
        let mut ops: RawOperands = (0..INLINE_OPERANDS).map(int).collect();
        ops.push(int(3));
        ops.push(int(4));
        assert_eq!(ops.len(), 5);
        for operand in ops.iter_mut() {
            if let Operand::Integer(value) = operand {
                *value += 10;
            }
        }
        assert_eq!(
            ops.to_vec(),
            (10..15).map(Operand::Integer).collect::<Vec<_>>()
        );
    }

    /// `From<Vec<Operand>>` adopts a wide vector's allocation and inlines a narrow one.
    #[test]
    fn from_vec_inlines_up_to_the_capacity_and_keeps_a_wider_vector() {
        let narrow: RawOperands = vec![int(0), int(1)].into();
        assert!(matches!(narrow, RawOperands::Inline { .. }));
        assert_eq!(narrow.to_vec(), vec![int(0), int(1)]);

        let wide: RawOperands = (0..5).map(int).collect::<Vec<_>>().into();
        assert!(matches!(wide, RawOperands::Spilled(_)));
        assert_eq!(wide.to_vec(), (0..5).map(int).collect::<Vec<_>>());
    }

    /// A raw length past the array is clamped by both `len` and `Deref` rather than
    /// producing an out-of-bounds slice.
    #[test]
    fn an_out_of_range_inline_len_is_clamped() {
        let ops = RawOperands::Inline {
            len: u8::MAX,
            items: [int(0), int(1), int(2)],
        };
        assert_eq!(ops.len(), INLINE_OPERANDS);
        assert_eq!(ops.to_vec(), vec![int(0), int(1), int(2)]);
    }

    /// The opcode field is untouched by the operand representation: a decoded
    /// instruction still carries its own opcode and offset.
    #[test]
    fn a_raw_instr_carries_its_opcode_and_offset() {
        let instr = RawInstr {
            opcode: Opcode::r#move,
            operands: vec![Operand::X(0), Operand::X(1)].into(),
            offset: 7,
        };
        assert_eq!(instr.opcode, Opcode::r#move);
        assert_eq!(instr.offset, 7);
        assert_eq!(instr.operands.to_vec(), vec![Operand::X(0), Operand::X(1)]);
    }
}
