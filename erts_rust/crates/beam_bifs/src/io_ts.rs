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

//! `~ts` under an active `{chars_limit, N}` — the unicode arm of
//! `io_lib_format:control_limited($s, …, unicode, …)`.
//!
//! Placeholder: the probe below settles how the crate's grapheme port reports a
//! binary continuation before the walk is written.

use beam_term::Eterm;

use crate::context::BifContext;
use crate::print::Chunk;

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    fn seeded() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["error", "true", "false", "function_clause", "case_clause", "badarg"]);
        atoms
    }

    fn run(bytes: &[u8], label: &str) {
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let term = crate::bits::make_binary(ctx.heap(), bytes).unwrap();
        let got = crate::unicode::grapheme::next_grapheme(&mut ctx, term).unwrap();
        let shown = crate::print::format_term(&mut ctx, got, false).unwrap();
        std::eprintln!("{label}: {shown}");
    }

    #[test]
    fn probe_gc() {
        run(&[230, 151, 165, 230, 151, 165], "KOU2");
        run(&[230, 151, 165], "KOU");
        run(&[97, 200, 201], "a+inv");
        run(&[200, 201], "BAD");
        run(&[], "empty");
    }
}
