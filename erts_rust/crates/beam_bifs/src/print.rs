//! The `~w`/`~p` term printer used by `io:format`.
//!
//! A safe helper module for [`crate::io`] (it registers no BIFs). It renders the
//! Milestone-1 term set exactly the way `io_lib:format/2` does and pins every case
//! against the OTP oracle (erts-17.1): small integers, atoms (bare vs quoted),
//! `NIL`, floats, proper/improper lists, tuples and internal pids.
//!
//! # The `~p` printable-string rule
//!
//! With `printable = true` (the `~p` directive) a **non-empty proper list whose
//! every element is a small integer in `32..=126`** renders as a `"…"` string,
//! escaping `"` and `\`; `~w` (`printable = false`) never applies the rule. So
//! `~p` of `"abc"` is `"abc"` while `~w` of `"abc"` is `[97,98,99]`, and `~p` of
//! `[]` is `[]` (the rule requires a *non-empty* list).
//!
//! # Deviations from the oracle (documented)
//!
//! * **Floats** use Rust's shortest round-trip with a `.0` appended when integral
//!   (`1.0`, `1.5`, `0.1`, `3.14159`), not OTP's `%.*g`: large/small magnitudes
//!   differ (`1.0e10` renders `10000000000`). These are not representable
//!   distinctly in Milestone-1 anyway.
//! * **Non-M1 classes** — maps, binaries, funs, refs, records, bignums and external
//!   pid/port/ref — are rejected with [`BeamError::BadArg`](beam_common::BeamError::BadArg)
//!   rather than guessed: they are not constructible in Milestone-1, so this is
//!   unreachable, and it is never a *silently wrong* result.
//! * Quoted atoms are not re-escaped (a `'` inside a name is printed verbatim) —
//!   such atoms are not produced by Milestone-1.

use beam_common::Result;
use beam_term::{atom_val, pid_number, pid_serial, signed_value, AtomTable, Eterm};

use crate::access::{
    car, cdr, float_value, is_atom, is_float, is_internal_pid, is_list, is_nil, is_small, is_tuple,
    tuple_element, tuple_size,
};
use crate::context::BifContext;
use crate::error::badarg;

/// Renders `term` the way the `~w` (`printable = false`) or `~p`
/// (`printable = true`) directive does.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a term outside the
/// Milestone-1 printable set (see the module docs), or an atom whose index is not
/// interned in `ctx`'s atom table.
pub(crate) fn format_term(ctx: &mut BifContext<'_>, term: Eterm, printable: bool) -> Result<String> {
    let mut out = String::new();
    render(ctx.atoms(), term, printable, &mut out)?;
    Ok(out)
}

/// Collects `term` into its element vector iff it is a proper list.
///
/// `NIL` yields `Some(vec![])`; a cons chain ending in anything but `NIL`, or a
/// non-list term, yields `None`. Shared with [`crate::io`] and [`crate::ets`] so
/// the "is this a proper list?" rule lives in exactly one place.
pub(crate) fn proper_list(term: Eterm) -> Option<Vec<Eterm>> {
    let mut elems = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        elems.push(car(cur));
        cur = cdr(cur);
    }
    if is_nil(cur) {
        Some(elems)
    } else {
        None
    }
}

/// Appends the rendering of `term` to `out`.
fn render(atoms: &AtomTable, term: Eterm, printable: bool, out: &mut String) -> Result<()> {
    if is_small(term) {
        out.push_str(&signed_value(term).to_string());
        return Ok(());
    }
    if is_nil(term) {
        out.push_str("[]");
        return Ok(());
    }
    if is_list(term) {
        return render_list(atoms, term, printable, out);
    }
    if is_tuple(term) {
        out.push('{');
        let arity = tuple_size(term);
        for i in 0..arity {
            if i != 0 {
                out.push(',');
            }
            render(atoms, tuple_element(term, i), printable, out)?;
        }
        out.push('}');
        return Ok(());
    }
    if is_atom(term) {
        let name = atoms.name(atom_val(term)).ok_or_else(badarg)?;
        out.push_str(&format_atom(name));
        return Ok(());
    }
    if is_float(term) {
        out.push_str(&format_float(float_value(term)));
        return Ok(());
    }
    if is_internal_pid(term) {
        out.push_str(&format_pid(term));
        return Ok(());
    }
    Err(badarg())
}

/// Appends a list rendering (`[e1,e2]` / `[e1|tail]`), applying the `~p`
/// printable-string rule when `printable`.
fn render_list(atoms: &AtomTable, term: Eterm, printable: bool, out: &mut String) -> Result<()> {
    if printable {
        if let Some(bytes) = printable_string(term) {
            push_quoted(out, &bytes);
            return Ok(());
        }
    }
    out.push('[');
    let mut cur = term;
    let mut first = true;
    loop {
        if !first {
            out.push(',');
        }
        first = false;
        render(atoms, car(cur), printable, out)?;
        let tail = cdr(cur);
        if is_nil(tail) {
            break;
        }
        if is_list(tail) {
            cur = tail;
            continue;
        }
        out.push('|');
        render(atoms, tail, printable, out)?;
        break;
    }
    out.push(']');
    Ok(())
}

/// The byte values of `term` iff it is a non-empty proper list of small integers
/// in `32..=126` (the `~p` printable-string rule).
fn printable_string(term: Eterm) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        let head = car(cur);
        if !is_small(head) {
            return None;
        }
        let value = signed_value(head);
        if !(32..=126).contains(&value) {
            return None;
        }
        bytes.push(value as u8);
        cur = cdr(cur);
    }
    if !is_nil(cur) || bytes.is_empty() {
        return None;
    }
    Some(bytes)
}

/// Appends `bytes` as a double-quoted string, escaping `"` and `\`.
fn push_quoted(out: &mut String, bytes: &[u8]) {
    out.push('"');
    for &byte in bytes {
        let ch = byte as char;
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
}

/// Renders an atom name: bare when it matches `[a-z][A-Za-z0-9_@]*`, else quoted.
fn format_atom(name: &str) -> String {
    if is_bare_atom(name) {
        name.to_string()
    } else {
        format!("'{name}'")
    }
}

/// Whether `name` is a bare atom name (`[a-z][A-Za-z0-9_@]*`).
fn is_bare_atom(name: &str) -> bool {
    let bytes = name.as_bytes();
    match bytes.first() {
        Some(&first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'@')
}

/// Renders a float: Rust's shortest round-trip, with a `.0` appended when the
/// rendering is integral (deviation from OTP `%.*g`; see the module docs).
fn format_float(value: f64) -> String {
    let mut s = format!("{value}");
    if value.is_finite() && !s.contains('.') && !s.contains('e') && !s.contains('E') {
        s.push_str(".0");
    }
    s
}

/// Renders an internal pid as `<Node.Number.Serial>` (Node is always `0` — the
/// local node — in Milestone-1).
fn format_pid(term: Eterm) -> String {
    format!("<0.{}.{}>", pid_number(term), pid_serial(term))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, NIL};

    /// Runs `f` over a fresh context whose atom table is seeded with the names the
    /// tests build terms from.
    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(512).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "hello", "Hello", "a", "b", "abc", "hi"]);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// Renders `term` with `printable`, asserting success.
    fn show(ctx: &mut BifContext<'_>, term: Eterm, printable: bool) -> String {
        format_term(ctx, term, printable).expect("term is printable")
    }

    #[test]
    fn small_ints_and_atoms() {
        with_ctx(|ctx| {
            let positive = ctx.int(7).expect("fits");
            assert_eq!(show(ctx, positive, false), "7");
            let negative = ctx.int(-5).expect("fits");
            assert_eq!(show(ctx, negative, false), "-5");

            let hello = ctx.atom("hello").expect("seeded");
            assert_eq!(show(ctx, hello, false), "hello");
            // `Hello` is not a bare name (`[a-z]…`), so it is quoted.
            let quoted = ctx.atom("Hello").expect("seeded");
            assert_eq!(show(ctx, quoted, false), "'Hello'");
        });
    }

    #[test]
    fn tuple_and_lists() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[a, one]).expect("fits");
            assert_eq!(show(ctx, tuple, false), "{a,1}");

            let two = ctx.int(2).expect("fits");
            let tail = ctx.cons(two, NIL).expect("fits");
            let list = ctx.cons(one, tail).expect("fits");
            assert_eq!(show(ctx, list, false), "[1,2]");

            // `[]` is `NIL`.
            assert_eq!(show(ctx, NIL, false), "[]");
            assert_eq!(show(ctx, NIL, true), "[]");

            // An improper list renders `[head|tail]`.
            let b = ctx.atom("b").expect("seeded");
            let improper = ctx.cons(a, b).expect("fits");
            assert_eq!(show(ctx, improper, false), "[a|b]");
            assert_eq!(show(ctx, improper, true), "[a|b]");
        });
    }

    #[test]
    fn floats_use_shortest_round_trip_with_dot_zero() {
        with_ctx(|ctx| {
            for (value, expected) in [
                (1.0f64, "1.0"),
                (1.5, "1.5"),
                (0.1, "0.1"),
                (3.14159, "3.14159"),
                (-2.5, "-2.5"),
            ] {
                let term = ctx.make_float(value).expect("fits");
                assert_eq!(show(ctx, term, false), expected, "float {value}");
            }
        });
    }

    #[test]
    fn printable_string_rule() {
        with_ctx(|ctx| {
            // "abc" == [97,98,99].
            let abc = ctx
                .list(&[
                    ctx.int(97).expect("fits"),
                    ctx.int(98).expect("fits"),
                    ctx.int(99).expect("fits"),
                ])
                .expect("fits");
            assert_eq!(show(ctx, abc, true), "\"abc\"");
            assert_eq!(show(ctx, abc, false), "[97,98,99]");

            // A byte below 32 disqualifies the whole list (even with `~p`).
            let controls = ctx
                .list(&[ctx.int(31).expect("fits"), ctx.int(32).expect("fits")])
                .expect("fits");
            assert_eq!(show(ctx, controls, true), "[31,32]");

            // 127 is outside `32..=126`.
            let high = ctx.list(&[ctx.int(127).expect("fits")]).expect("fits");
            assert_eq!(show(ctx, high, true), "[127]");

            // Escaping: `"` (34) and `\` (92).
            let quote = ctx.list(&[ctx.int(34).expect("fits")]).expect("fits");
            assert_eq!(show(ctx, quote, true), "\"\\\"\"");
            let backslash = ctx.list(&[ctx.int(92).expect("fits")]).expect("fits");
            assert_eq!(show(ctx, backslash, true), "\"\\\\\"");
        });
    }

    #[test]
    fn printable_rule_propagates_into_tuples() {
        with_ctx(|ctx| {
            let ok = ctx.atom("ok").expect("seeded");
            let hi = ctx
                .list(&[ctx.int(104).expect("fits"), ctx.int(105).expect("fits")])
                .expect("fits");
            let float = ctx.make_float(1.5).expect("fits");
            let tuple = ctx.tuple(&[ok, hi, float]).expect("fits");
            assert_eq!(show(ctx, tuple, true), "{ok,\"hi\",1.5}");
        });
    }

    #[test]
    fn nested_lists_respect_the_rule() {
        with_ctx(|ctx| {
            let inner1 = ctx
                .list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")])
                .expect("fits");
            let inner2 = ctx.list(&[ctx.int(3).expect("fits")]).expect("fits");
            let outer = ctx.list(&[inner1, inner2]).expect("fits");
            assert_eq!(show(ctx, outer, true), "[[1,2],[3]]");
        });
    }

    #[test]
    fn internal_pid_renders_node_number_serial() {
        with_ctx(|ctx| {
            let pid = ProcessId::new(10, 0).to_eterm();
            assert_eq!(show(ctx, pid, false), "<0.10.0>");
        });
    }

    #[test]
    fn bare_atom_name_rule() {
        assert!(is_bare_atom("a"));
        assert!(is_bare_atom("hello"));
        assert!(is_bare_atom("x1_@z"));
        assert!(is_bare_atom("foo@bar"));
        assert!(!is_bare_atom(""));
        assert!(!is_bare_atom("Foo"));
        assert!(!is_bare_atom("_foo"));
        assert!(!is_bare_atom("1x"));
        assert!(!is_bare_atom("a-b"));
        assert_eq!(format_atom("_foo"), "'_foo'");
        assert_eq!(format_atom(""), "''");
    }

    #[test]
    fn proper_list_collects_or_rejects() {
        with_ctx(|ctx| {
            assert_eq!(proper_list(NIL).expect("nil is a proper list").len(), 0);
            let list = ctx
                .list(&[make_small(1), make_small(2)])
                .expect("fits");
            assert_eq!(proper_list(list).expect("proper").len(), 2);
            // An improper list and a non-list are both rejected.
            let improper = ctx.cons(make_small(1), make_small(2)).expect("fits");
            assert!(proper_list(improper).is_none());
            assert!(proper_list(make_small(3)).is_none());
        });
    }

    #[test]
    fn unsupported_terms_are_badarg() {
        with_ctx(|ctx| {
            // A map is outside the M1 printable set.
            let map = ctx
                .list(&[make_small(1)])
                .expect("fits");
            // `make_ref_thing` builds the M1 reference thing (an internal ref).
            let reference = crate::access::make_ref_thing(ctx.heap(), 1).expect("fits");
            assert!(matches!(
                format_term(ctx, reference, false),
                Err(beam_common::BeamError::BadArg)
            ));
            // Sanity: the list itself still renders.
            assert_eq!(show(ctx, map, false), "[1]");
        });
    }
}
