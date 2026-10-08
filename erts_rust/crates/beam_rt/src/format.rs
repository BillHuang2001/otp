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

//! Printing a term: `term_to_string` drives the registered `io:format/2` `~p` printer.
//!
//! There is exactly **one** term printer in the workspace and it is the `io:format`
//! BIF of [`beam_bifs`]; this module only knows how to *call* it. It builds the format
//! list `"~p~n"` and the argument list `[Term]` on the destination heap, runs
//! `io:format(Fmt, Args)` with the caller's atom table against a capture sink and hands
//! back the captured text.
//!
//! `~p` (rather than `~w`) is used because it is what the Erlang shell and
//! `io:format/2` users see: it applies the printable-string rule, so an Erlang string
//! renders as `"17.1"` — exactly as `erlang:system_info(version)` prints in the oracle.
//!
//! The `run` action of the CLI prints its result with `~w` instead ([`term_to_string_w`]):
//! that is what `erl ... -eval 'io:format("~w~n",[V])'` produces, and — unlike `~p` — it
//! does **not** collapse a list of printable characters into a string literal
//! (`~w` of `[97,98,99]` is `[97,98,99]`).
//!
//! [`term_to_string_with`] is the one printer: it builds the format list from the
//! requested format control and drives the BIF. Every other entry point is a thin
//! wrapper over it, so a new format control never means a second printer.

use beam_alloc::Heap;
use beam_bifs::{BifContext, BifRegistry, BytesSink};
use beam_common::Result;
use beam_process::ProcessId;
use beam_term::{make_small, AtomTable, Eterm, NIL};

/// The format control [`term_to_string`] uses: `~p` (printable-string rule) + newline.
///
/// `pub(crate)` so [`crate::Runtime::format_term`]/[`crate::Runtime::format_term_w`] can
/// name their control without a second printer being introduced.
pub(crate) const FORMAT: &[u8] = b"~p~n";

/// The format control [`term_to_string_w`] uses: `~w` (raw) + newline.
pub(crate) const FORMAT_W: &[u8] = b"~w~n";

/// Renders `term` with the registered `~p` printer.
///
/// # Errors
///
/// * the heap's allocation failure while building the format/argument lists;
/// * whatever `io:format/2` reports — in particular
///   [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a term outside the
///   Milestone-1 printable set (maps, binaries, funs, …) and
///   [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) for an atom not
///   interned in `atoms`.
pub fn term_to_string(
    registry: &BifRegistry,
    heap: &mut Heap,
    atoms: &AtomTable,
    self_pid: ProcessId,
    term: Eterm,
) -> Result<String> {
    term_to_string_with(registry, heap, atoms, self_pid, term, FORMAT)
}

/// Renders `term` with the registered `~w` printer (`~w~n`): the raw standard syntax,
/// without the `~p` printable-string rule.
///
/// This is the `run` action's printer — the oracle's `io:format("~w~n",[V])`.
///
/// # Errors
///
/// As [`term_to_string`].
pub fn term_to_string_w(
    registry: &BifRegistry,
    heap: &mut Heap,
    atoms: &AtomTable,
    self_pid: ProcessId,
    term: Eterm,
) -> Result<String> {
    term_to_string_with(registry, heap, atoms, self_pid, term, FORMAT_W)
}

/// Renders `term` with the registered `io:format/2` printer using the format control
/// `control` (a byte string such as `b"~p~n"`).
///
/// `control` is handed to `io:format/2` verbatim, so the directive's semantics (and its
/// error behaviour) are the BIF's, never re-implemented here. The built format list is
/// ASCII-only, so its elements are byte-sized small integers.
///
/// # Errors
///
/// As [`term_to_string`].
pub fn term_to_string_with(
    registry: &BifRegistry,
    heap: &mut Heap,
    atoms: &AtomTable,
    self_pid: ProcessId,
    term: Eterm,
    control: &[u8],
) -> Result<String> {
    let mut format = NIL;
    for byte in control.iter().rev() {
        format = heap.cons(make_small(i64::from(*byte)), format)?;
    }
    let args = heap.cons(term, NIL)?;

    let mut sink = BytesSink::new();
    let mut context = BifContext::new(heap, atoms, self_pid).with_output(&mut sink);
    registry.call("io", "format", 2, &mut context, &[format, args])?;
    Ok(sink.as_str())
}

#[cfg(test)]
mod tests {
    use beam_common::BeamError;
    use beam_term::{make_atom, make_small};

    use super::*;

    fn registry() -> BifRegistry {
        BifRegistry::with_core_bifs()
    }

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["ok", "true", "false", "hello", "world"]);
        atoms
    }

    fn render(term: Eterm, heap: &mut Heap, atoms: &AtomTable) -> String {
        term_to_string(&registry(), heap, atoms, ProcessId::new(1, 0), term).expect("printable")
    }

    #[test]
    fn prints_small_integers_with_a_newline() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        assert_eq!(render(make_small(0), &mut heap, &atoms), "0\n");
        assert_eq!(render(make_small(5), &mut heap, &atoms), "5\n");
        assert_eq!(render(make_small(-7), &mut heap, &atoms), "-7\n");
    }

    #[test]
    fn prints_atoms_and_nil() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let ok = make_atom(atoms.lookup("ok").expect("seeded"));
        assert_eq!(render(ok, &mut heap, &atoms), "ok\n");
        assert_eq!(render(NIL, &mut heap, &atoms), "[]\n");
    }

    #[test]
    fn applies_the_printable_string_rule() {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = atoms();
        // "hi" as an Erlang string: [104, 105].
        let tail = heap.cons(make_small(105), NIL).expect("fits");
        let text = heap.cons(make_small(104), tail).expect("fits");
        assert_eq!(render(text, &mut heap, &atoms), "\"hi\"\n");
    }

    #[test]
    fn the_w_control_does_not_apply_the_string_rule() {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = atoms();
        // The `run` action prints with `~w`, which keeps a char list as a list:
        // `io:format("~w~n", ["hi"])` is `[104,105]`, not `"hi"`.
        let tail = heap.cons(make_small(105), NIL).expect("fits");
        let text = heap.cons(make_small(104), tail).expect("fits");
        let rendered = term_to_string_w(&registry(), &mut heap, &atoms, ProcessId::new(1, 0), text)
            .expect("printable");
        assert_eq!(rendered, "[104,105]\n");
        // Non-char-list terms render identically under both controls.
        assert_eq!(render(make_small(5), &mut heap, &atoms), "5\n");
    }
    #[test]
    fn prints_a_tuple() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let tuple = heap
            .tuple(&[make_small(1), make_small(2)])
            .expect("fits");
        assert_eq!(render(tuple, &mut heap, &atoms), "{1,2}\n");
    }

    #[test]
    fn an_uninterned_atom_is_an_error() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        // An index past the end of the table: the printer cannot name it, so
        // `io:format` reports `badarg` rather than inventing a name.
        let absent = make_atom(beam_term::Atom::from_raw(4096));
        assert!(matches!(
            term_to_string(&registry(), &mut heap, &atoms, ProcessId::new(1, 0), absent),
            Err(BeamError::BadArg)
        ));
    }
}
