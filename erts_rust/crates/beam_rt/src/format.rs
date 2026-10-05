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

use beam_alloc::Heap;
use beam_bifs::{BifContext, BifRegistry, BytesSink};
use beam_common::Result;
use beam_process::ProcessId;
use beam_term::{make_small, AtomTable, Eterm, NIL};

/// The format string every call uses: print the term followed by a newline.
const FORMAT: &[u8] = b"~p~n";

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
    let mut format = NIL;
    for byte in FORMAT.iter().rev() {
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
