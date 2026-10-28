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

//! The loader's only superinstruction pass: `deallocate N; return` →
//! `DeallocateReturn(N)`.
//!
//! The compiler ends every non-tail function body with the adjacent pair
//! `deallocate N` / `return` (`beam_ssa_codegen` emits the frame pop immediately
//! before the return). Both are `Instr` variants with an exact, unchanged meaning;
//! combining them into one instruction is purely an engine-side dispatch saving
//! (one fewer dispatch-loop iteration per function exit), never a semantic change.
//!
//! Only an **adjacent** pair is fused. `Instr::Label` is the only instruction a
//! branch can target, so two adjacent non-`Label` instructions are guaranteed to be
//! in the same basic block: fusing them can never move work across a branch target
//! (and a `Label` between a `deallocate` and a `return` — which cannot occur in real
//! compiler output anyway — is left untouched, making the pass conservative by
//! construction). Every other sequence is copied through verbatim, so hand-built
//! modules that bypass the loader keep `Instr::Deallocate`/`Instr::Return` live.

use crate::instr::Instr;

/// `deallocate N; return` -> `DeallocateReturn(N)`, `code` still owned.
///
/// Applies [`fuse_in_place`] to an owned body, so the caller that already owns the
/// vector neither pays for a second allocation nor gives up the original capacity.
pub fn fuse(code: Vec<Instr>) -> Vec<Instr> {
    let mut code = code;
    fuse_in_place(&mut code);
    code
}

/// The fuse rule, applied to one adjacent pair: `Deallocate(n)` immediately followed
/// by `Return` yields `Some(n)`, and every other pair `None`.
///
/// This is the **only** statement of the rule. [`fuse_in_place`] applies it to the
/// already-lowered body an owning caller hands over; `group::emit_fused` applies it
/// while it lowers, so the body it emits is the body this pass would have produced —
/// and each body's allocation can be sized to the fused length up front instead of
/// being shrunk (reallocated) afterwards.
pub fn fuses(prev: &Instr, next: &Instr) -> Option<u32> {
    match *prev {
        Instr::Deallocate(n) if matches!(*next, Instr::Return) => Some(n),
        _ => None,
    }
}

/// `deallocate N; return` -> `DeallocateReturn(N)`, compacted in place.
/// Only an ADJACENT pair with no intervening `Instr::Label` is fused (a `Label`
/// is the only branch target, so two adjacent non-`Label` instructions are
/// guaranteed to be in the same block). Everything else is left untouched, so
/// `Instr::Deallocate`/`Instr::Return` stay live for hand-built modules.
///
/// `code` is compacted with a read/write index pair: every element is either moved
/// down into the write slot or replaced by the fused instruction, so the buffer is
/// never reallocated (the capacity is untouched) and `truncate` only lowers the
/// length. The rule keys on the adjacency of the *already-emitted* element, so a
/// `Label` sitting between a `deallocate` and a `return` blocks the fuse exactly as
/// it does when the pair is copied through a fresh vector.
pub fn fuse_in_place(code: &mut Vec<Instr>) {
    let len = code.len();
    let mut read = 0usize;
    let mut write = 0usize;
    while read < len {
        // The next instruction is the `Return` (any `Label` between the two would
        // have been that next instruction instead, so the pair is adjacent by
        // construction). Binds only the `Copy` word, so nothing is moved out.
        let pop = match code.get(read + 1) {
            Some(next) => fuses(&code[read], next),
            None => None,
        };
        match pop {
            Some(n) => {
                // The `Return` is consumed by the fused instruction.
                code[write] = Instr::DeallocateReturn(n);
                read += 2;
            }
            None => {
                // `write <= read` always holds, so this never overwrites an
                // instruction that has not been read yet.
                if write != read {
                    code.swap(write, read);
                }
                read += 1;
            }
        }
        write += 1;
    }
    code.truncate(write);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operand::Label;

    #[test]
    fn fuses_an_adjacent_deallocate_return_pair() {
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: crate::operand::Operand::X(0),
                dst: crate::operand::Operand::X(1),
            },
            Instr::Deallocate(3),
            Instr::Return,
        ];
        assert_eq!(
            fuse(code),
            vec![
                Instr::Label(Label(1)),
                Instr::Move {
                    src: crate::operand::Operand::X(0),
                    dst: crate::operand::Operand::X(1),
                },
                Instr::DeallocateReturn(3),
            ]
        );
    }

    #[test]
    fn fuses_every_pair_in_a_multi_function_body() {
        let code = vec![
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(2)),
            Instr::Deallocate(2),
            Instr::Return,
        ];
        assert_eq!(
            fuse(code),
            vec![
                Instr::DeallocateReturn(1),
                Instr::Label(Label(2)),
                Instr::DeallocateReturn(2),
            ]
        );
    }

    #[test]
    fn leaves_a_lone_deallocate_untouched() {
        let code = vec![Instr::Deallocate(1)];
        assert_eq!(fuse(code.clone()), code);
    }

    #[test]
    fn leaves_a_lone_return_untouched() {
        let code = vec![Instr::Return];
        assert_eq!(fuse(code.clone()), code);
    }

    #[test]
    fn leaves_a_deallocate_not_followed_by_return_untouched() {
        let code = vec![
            Instr::Deallocate(1),
            Instr::Move {
                src: crate::operand::Operand::X(0),
                dst: crate::operand::Operand::X(1),
            },
            Instr::Return,
        ];
        assert_eq!(fuse(code.clone()), code);
    }

    #[test]
    fn never_fuses_across_a_label() {
        // The `deallocate` and the `return` are adjacent instructions but a `Label`
        // is not between them here; the point is that the pass keys on adjacency:
        // a `Label` as the instruction after `deallocate` blocks the fuse.
        let code = vec![
            Instr::Deallocate(1),
            Instr::Label(Label(2)),
            Instr::Return,
        ];
        assert_eq!(fuse(code.clone()), code);
    }

    #[test]
    fn leaves_a_deallocate_at_the_end_of_the_stream_untouched() {
        // A trailing `deallocate` (no following instruction) must not panic or fuse.
        let code = vec![Instr::Return, Instr::Deallocate(0)];
        assert_eq!(fuse(code.clone()), code);
    }

    /// A body with several fusable pairs, a `Label` separating two of them and a
    /// trailing lone `Deallocate`.
    fn mixed_body() -> Vec<Instr> {
        vec![
            Instr::Label(Label(1)),
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(2)),
            Instr::Deallocate(2),
            Instr::Return,
            Instr::Deallocate(3),
        ]
    }

    #[test]
    fn fuse_and_fuse_in_place_agree_on_a_mixed_body() {
        let mut in_place = mixed_body();
        fuse_in_place(&mut in_place);
        assert_eq!(fuse(mixed_body()), in_place);
        assert_eq!(
            in_place,
            vec![
                Instr::Label(Label(1)),
                Instr::DeallocateReturn(1),
                Instr::Label(Label(2)),
                Instr::DeallocateReturn(2),
                Instr::Deallocate(3),
            ]
        );
    }

    #[test]
    fn fuse_in_place_keeps_the_allocation_and_only_lowers_the_length() {
        let mut code = mixed_body();
        let capacity = code.capacity();
        let before = code.len();
        fuse_in_place(&mut code);
        assert_eq!(code.capacity(), capacity);
        // Two adjacent pairs collapsed in this body.
        assert_eq!(code.len(), before - 2);
    }

    #[test]
    fn fuse_in_place_keeps_the_allocation_when_nothing_fuses() {
        let mut code = vec![
            Instr::Deallocate(1),
            Instr::Label(Label(2)),
            Instr::Return,
        ];
        let capacity = code.capacity();
        let before = code.len();
        fuse_in_place(&mut code);
        assert_eq!(code.capacity(), capacity);
        assert_eq!(code.len(), before);
    }
}
