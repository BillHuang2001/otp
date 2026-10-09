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

/// `deallocate N; return` -> `DeallocateReturn(N)`.
/// Only an ADJACENT pair with no intervening `Instr::Label` is fused (a `Label`
/// is the only branch target, so two adjacent non-`Label` instructions are
/// guaranteed to be in the same block). Everything else is left untouched, so
/// `Instr::Deallocate`/`Instr::Return` stay live for hand-built modules.
pub fn fuse(code: Vec<Instr>) -> Vec<Instr> {
    let mut out: Vec<Instr> = Vec::with_capacity(code.len());
    let mut iter = code.into_iter().peekable();
    while let Some(instr) = iter.next() {
        match instr {
            // The very next instruction is the `Return` (any `Label` between the
            // two would have been that next instruction instead, so the pair is
            // adjacent by construction).
            Instr::Deallocate(n) if matches!(iter.peek(), Some(Instr::Return)) => {
                // Consume the `Return`; the pair becomes one instruction.
                iter.next();
                out.push(Instr::DeallocateReturn(n));
            }
            other => out.push(other),
        }
    }
    out
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
}
