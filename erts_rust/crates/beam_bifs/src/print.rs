//! The `~w`/`~p`/`~W`/`~P` term printer used by `io:format`/`io_lib:format`.
//!
//! A safe helper module for [`crate::io`] (it registers no BIFs). It renders the
//! Milestone-1 term set the way `io_lib:write/…` and the `~p` pretty printer do,
//! and pins every case against the OTP oracle (erts-17.1): small integers, atoms
//! (bare vs quoted), `NIL`, floats, proper/improper lists, tuples, maps, binaries,
//! bignums, internal pids and internal refs.
//!
//! [`format_term`] is the depth-unlimited entry point `~w`/`~p` use;
//! [`format_term_depth`] is the depth-limited one `~W`/`~P` use. The depth
//! semantics reproduce `io_lib:write1/4` (`lib/stdlib/src/io_lib.erl`): depth `0`
//! renders `...`, depth `1` elides the *contents* of a container (`{...}`,
//! `[...]`, `<<...>>`, `#{}`), and every nesting step consumes one level; a
//! negative depth is unlimited (`~w` passes `-1`).
//!
//! # The `~p` printable-string rule
//!
//! With `printable = true` (the `~p`/`~P` directives) a **non-empty proper list
//! whose every element is a small integer in `32..=126`** renders as a `"…"`
//! string, escaping `"` and `\`; `~w` (`printable = false`) never applies the
//! rule. So `~p` of `"abc"` is `"abc"` while `~w` of `"abc"` is `[97,98,99]`, and
//! `~p` of `[]` is `[]` (the rule requires a *non-empty* list). The same rule is
//! applied to **binaries**, which then render as `<<"…">>`.
//!
//! # Maps
//!
//! A flat map renders `#{k => v,…}` (note ` => ` between a key and its value and a
//! bare `,` between pairs — the oracle's rendering). Pairs are printed in the
//! oracle's *flatmap key order* (`erts_cmp_flatmap_keys`, `erl_utils.h:267`): two
//! atom keys compare by their raw term word (i.e. by atom index / intern order),
//! anything else by the exact term order (`beam_term::compare::cmp`).
//!
//! # Deviations from the oracle (documented)
//!
//! * **Floats** use Rust's shortest round-trip with a `.0` appended when integral
//!   (`1.0`, `1.5`, `0.1`, `3.14159`), not OTP's `float_to_list(F, [short])`:
//!   large/small magnitudes differ (`1.0e10` renders `10000000000`, the oracle
//!   `1.0e10`).
//! * **The `~p` printer never line-breaks.** The oracle runs `io_lib_pretty`,
//!   which reflows a term that does not fit `line_length` (80 columns by default)
//!   and honours a `~p` field width as a maximum *column*; this printer always
//!   emits the single-line form. [`crate::io`] rejects a `~p`/`~P` whose rendering
//!   exceeds an explicitly given field width rather than emit a mis-formatted line.
//! * **Quoted atoms are not re-escaped** (a `'` inside a name is printed verbatim)
//!   — such atoms are not produced by Milestone-1.
//! * **Non-byte-aligned bitstrings are rejected** (`BeamError::BadArg`): the
//!   oracle prints `<<X:L>>`, but `crate::bits::bytes` only reads a byte-aligned
//!   binary and Milestone-1 has no bit-level reader. Every other term class is
//!   rendered.
//! * **Internal refs** render as `#Ref<Channel.Node.Serial.Id>` with
//!   `Channel`/`Node`/`Serial` always `0` in Milestone-1 (the frozen
//!   `[header, node, serial, id]` thing of [`crate::access::make_ref_thing`]), so
//!   only `Id` varies. The oracle's numbers are the same shape.
//! * External pids/ports/refs, funs and records are outside the Milestone-1 term
//!   set and are rejected with `BeamError::BadArg` rather than guessed — never a
//!   *silently wrong* result.

use beam_codec::term::big_parts;
use beam_common::Result;
use beam_term::{atom_val, compare, pid_number, pid_serial, signed_value, AtomTable, Eterm};

use crate::access::{
    car, cdr, flatmap_pairs, float_value, is_atom, is_big, is_float, is_internal_pid,
    is_internal_ref, is_list, is_map, is_nil, is_small, is_tuple, ref_thing_id, tuple_element,
    tuple_size,
};
use crate::bits;
use crate::context::BifContext;
use crate::error::badarg;

/// The depth value that disables depth limiting — `io_lib` passes `-1` for `~w`.
pub(crate) const UNLIMITED_DEPTH: i64 = -1;

/// Renders `term` the way the `~w` (`printable = false`) or `~p`
/// (`printable = true`) directive does, with no depth limit.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a term outside the
/// Milestone-1 printable set (see the module docs), or an atom whose index is not
/// interned in `ctx`'s atom table.
pub(crate) fn format_term(ctx: &mut BifContext<'_>, term: Eterm, printable: bool) -> Result<String> {
    format_term_depth(ctx, term, printable, UNLIMITED_DEPTH)
}

/// Renders `term` with an `io_lib:write/2` **depth** limit (`~W`/`~P`).
///
/// A negative `depth` is unlimited (see [`UNLIMITED_DEPTH`]); `0` renders `...`
/// and `1` elides a container's contents.
///
/// # Errors
///
/// As [`format_term`].
pub(crate) fn format_term_depth(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
) -> Result<String> {
    let mut out = String::new();
    render(ctx.atoms(), term, printable, depth, &mut out)?;
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

/// Appends the rendering of `term` to `out` (`io_lib:write1/4`).
fn render(
    atoms: &AtomTable,
    term: Eterm,
    printable: bool,
    depth: i64,
    out: &mut String,
) -> Result<()> {
    if depth == 0 {
        out.push_str("...");
        return Ok(());
    }
    if is_small(term) {
        out.push_str(&signed_value(term).to_string());
        return Ok(());
    }
    if is_big(term) {
        out.push_str(&format_big(term)?);
        return Ok(());
    }
    if is_nil(term) {
        out.push_str("[]");
        return Ok(());
    }
    if is_list(term) {
        if depth == 1 {
            out.push_str("[...]");
            return Ok(());
        }
        if printable {
            if let Some(bytes) = printable_string(term) {
                push_quoted(out, &bytes);
                return Ok(());
            }
        }
        return render_list(atoms, term, printable, depth, out);
    }
    if is_tuple(term) {
        let arity = tuple_size(term);
        if arity == 0 {
            out.push_str("{}");
            return Ok(());
        }
        if depth == 1 {
            out.push_str("{...}");
            return Ok(());
        }
        out.push('{');
        render(atoms, tuple_element(term, 0), printable, depth - 1, out)?;
        render_tuple_rest(atoms, term, 1, depth - 1, printable, out)?;
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
    if bits::is_binary(term) {
        return render_binary(term, printable, depth, out);
    }
    if bits::is_bitstring(term) {
        // A non-byte-aligned bitstring: `crate::bits::bytes` cannot read it (see
        // the module docs) — reject rather than guess.
        return Err(badarg());
    }
    if is_map(term) {
        return render_map(atoms, term, printable, depth, out);
    }
    if is_internal_pid(term) {
        out.push_str(&format!("<0.{}.{}>", pid_number(term), pid_serial(term)));
        return Ok(());
    }
    if is_internal_ref(term) {
        out.push_str(&format_ref(term)?);
        return Ok(());
    }
    Err(badarg())
}

/// Appends a list rendering (`[e1,e2]` / `[e1|tail]`), the caller having already
/// handled depth `0`/`1` and the `~p` printable-string rule.
fn render_list(
    atoms: &AtomTable,
    term: Eterm,
    printable: bool,
    depth: i64,
    out: &mut String,
) -> Result<()> {
    out.push('[');
    render(atoms, car(term), printable, depth - 1, out)?;
    render_tail(atoms, cdr(term), printable, depth - 1, out)?;
    out.push(']');
    Ok(())
}

/// Appends a list tail (`""`, `,e…` or `|tail`, `io_lib:write_tail/4`); the
/// closing `]` is the caller's.
fn render_tail(
    atoms: &AtomTable,
    term: Eterm,
    printable: bool,
    depth: i64,
    out: &mut String,
) -> Result<()> {
    if is_nil(term) {
        return Ok(());
    }
    if depth == 1 {
        out.push_str("|...");
        return Ok(());
    }
    if is_list(term) {
        out.push(',');
        render(atoms, car(term), printable, depth - 1, out)?;
        return render_tail(atoms, cdr(term), printable, depth - 1, out);
    }
    out.push('|');
    render(atoms, term, printable, depth - 1, out)
}

/// Appends the remaining tuple elements (`,e…` / `,...`), `io_lib:write_tuple/6`.
fn render_tuple_rest(
    atoms: &AtomTable,
    term: Eterm,
    index: usize,
    depth: i64,
    printable: bool,
    out: &mut String,
) -> Result<()> {
    if index >= tuple_size(term) {
        return Ok(());
    }
    if depth == 1 {
        out.push_str(",...");
        return Ok(());
    }
    out.push(',');
    render(atoms, tuple_element(term, index), printable, depth - 1, out)?;
    render_tuple_rest(atoms, term, index + 1, depth - 1, printable, out)
}

/// Appends a flat-map rendering (`#{k => v,…}`), `io_lib:write_map/4`.
fn render_map(
    atoms: &AtomTable,
    term: Eterm,
    printable: bool,
    depth: i64,
    out: &mut String,
) -> Result<()> {
    if depth == 1 {
        out.push_str("#{}");
        return Ok(());
    }
    let (keys, values) = flatmap_pairs(term)?;
    let mut pairs: Vec<(Eterm, Eterm)> = keys.into_iter().zip(values).collect();
    pairs.sort_by(|a, b| flatmap_key_cmp(a.0, b.0, atoms));
    out.push_str("#{");
    if pairs.is_empty() {
        out.push('}');
        return Ok(());
    }
    let inner = depth - 1;
    render(atoms, pairs[0].0, printable, inner, out)?;
    out.push_str(" => ");
    render(atoms, pairs[0].1, printable, inner, out)?;
    render_map_body(atoms, &pairs, 1, inner, inner, printable, out)?;
    out.push('}');
    Ok(())
}

/// Appends the remaining map pairs (`,k => v…` / `,...`), `io_lib:write_map_body/5`.
///
/// `depth` starts at the first pair's inner depth and decreases per pair; `inner`
/// is the (fixed) depth every key and value is rendered at.
fn render_map_body(
    atoms: &AtomTable,
    pairs: &[(Eterm, Eterm)],
    index: usize,
    depth: i64,
    inner: i64,
    printable: bool,
    out: &mut String,
) -> Result<()> {
    if depth == 1 {
        out.push_str(",...");
        return Ok(());
    }
    if index >= pairs.len() {
        return Ok(());
    }
    out.push(',');
    render(atoms, pairs[index].0, printable, inner, out)?;
    out.push_str(" => ");
    render(atoms, pairs[index].1, printable, inner, out)?;
    render_map_body(atoms, pairs, index + 1, depth - 1, inner, printable, out)
}

/// The oracle's flatmap internal key order (`erts_cmp_flatmap_keys`): two atoms by
/// their raw term word (atom index order), anything else by the exact term order.
fn flatmap_key_cmp(a: Eterm, b: Eterm, atoms: &AtomTable) -> core::cmp::Ordering {
    if is_atom(a) && is_atom(b) {
        a.raw().cmp(&b.raw())
    } else {
        compare::cmp(a, b, atoms)
    }
}

/// Appends a binary rendering (`<<97,98,99>>` / `<<"abc">>` / `<<...>>`).
fn render_binary(term: Eterm, printable: bool, depth: i64, out: &mut String) -> Result<()> {
    let bytes = bits::bytes(term)?;
    if bytes.is_empty() {
        out.push_str("<<>>");
        return Ok(());
    }
    if depth != 1 {
        if printable {
            if printable_bytes(&bytes) {
                out.push_str("<<");
                push_quoted(out, &bytes);
                out.push_str(">>");
                return Ok(());
            }
        }
    }
    out.push_str("<<");
    if depth == 1 {
        out.push_str("...");
    } else {
        for (i, byte) in bytes.iter().enumerate() {
            if i != 0 {
                out.push(',');
            }
            out.push_str(&byte.to_string());
        }
    }
    out.push_str(">>");
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

/// Whether every byte is in `32..=126` and the binary is non-empty (the `~p`
/// printable-binary rule, mirroring [`printable_string`]).
fn printable_bytes(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|&b| (32..=126).contains(&b))
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

/// Renders a bignum in decimal (`erts_big_to_string(wobj, 10, …)`).
///
/// The digits are read with the safe [`beam_codec::term::big_parts`] wrapper (the
/// crate's `access.rs` exposes no bignum digit accessor and is the crate's only
/// `#[allow(unsafe_code)]` module; `beam_codec` is already a declared dependency).
fn format_big(term: Eterm) -> Result<String> {
    let (negative, digits) = big_parts(term)?;
    Ok(format_big_decimal(negative, digits))
}

/// Converts a least-significant-first `u64` digit vector to decimal.
fn format_big_decimal(negative: bool, digits: &[u64]) -> String {
    let mut limbs: Vec<u64> = digits.to_vec();
    while limbs.len() > 1 && limbs.last() == Some(&0) {
        limbs.pop();
    }
    let mut reversed: Vec<char> = Vec::new();
    loop {
        // One base-2^64 long division by 10 (the remainder is the next decimal
        // digit, least significant first).
        let mut rem: u128 = 0;
        let mut nonzero = false;
        for i in (0..limbs.len()).rev() {
            let current = (rem << 64) | u128::from(limbs[i]);
            let quotient = (current / 10) as u64;
            limbs[i] = quotient;
            nonzero |= quotient != 0;
            rem = current % 10;
        }
        reversed.push(char::from(b'0' + rem as u8));
        if !nonzero {
            break;
        }
    }
    let mut out: String = reversed.iter().rev().collect();
    if negative {
        out.insert(0, '-');
    }
    out
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
/// rendering is integral (deviation from `float_to_list(F, [short])`; see the
/// module docs).
fn format_float(value: f64) -> String {
    let mut s = format!("{value}");
    if value.is_finite() && !s.contains('.') && !s.contains('e') && !s.contains('E') {
        s.push_str(".0");
    }
    s
}

/// Renders an internal ref as `#Ref<0.0.0.Id>` (Milestone-1 refs carry no channel,
/// node or serial — see the module docs).
fn format_ref(term: Eterm) -> Result<String> {
    let id = ref_thing_id(term)?;
    Ok(format!("#Ref<0.0.0.{id}>"))
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
        let mut heap = Heap::new(1024).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "hello", "Hello", "a", "b", "c", "abc", "hi"]);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// Renders `term` with `printable`, asserting success.
    fn show(ctx: &mut BifContext<'_>, term: Eterm, printable: bool) -> String {
        format_term(ctx, term, printable).expect("term is printable")
    }

    /// Renders `term` at `depth`.
    fn show_at(ctx: &mut BifContext<'_>, term: Eterm, printable: bool, depth: i64) -> String {
        format_term_depth(ctx, term, printable, depth).expect("term is printable")
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

            // `[]` is `NIL`, `{}` the empty tuple.
            assert_eq!(show(ctx, NIL, false), "[]");
            assert_eq!(show(ctx, NIL, true), "[]");
            let empty = ctx.tuple(&[]).expect("fits");
            assert_eq!(show(ctx, empty, false), "{}");
            assert_eq!(show_at(ctx, empty, false, 1), "{}");

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
    fn binaries_render_in_the_oracle_form() {
        with_ctx(|ctx| {
            // `~w` of `<<"abc">>` is `<<97,98,99>>`; `~p` is `<<"abc">>`.
            // (`erl -noshell -eval 'io:format("~p~n",[io_lib:format("~w",[<<"abc">>])])'`)
            let abc = bits::make_binary(ctx.heap(), b"abc").expect("fits");
            assert_eq!(show(ctx, abc, false), "<<97,98,99>>");
            assert_eq!(show(ctx, abc, true), "<<\"abc\">>");

            // Non-printable bytes stay in the `~w` form under `~p` too.
            let low = bits::make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
            assert_eq!(show(ctx, low, false), "<<1,2,3>>");
            assert_eq!(show(ctx, low, true), "<<1,2,3>>");

            // An empty binary is `<<>>` in both forms (oracle).
            let empty = bits::make_binary(ctx.heap(), &[]).expect("fits");
            assert_eq!(show(ctx, empty, false), "<<>>");
            assert_eq!(show(ctx, empty, true), "<<>>");
            assert_eq!(show_at(ctx, empty, true, 1), "<<>>");

            // `"`/`\` escape inside the `~p` string form.
            let esc = bits::make_binary(ctx.heap(), b"\"\\").expect("fits");
            assert_eq!(show(ctx, esc, true), "<<\"\\\"\\\\\">>");
        });
    }

    #[test]
    fn maps_render_with_spaces_around_the_arrow() {
        with_ctx(|ctx| {
            // `~w` of `#{1 => 2, 3 => 4}` is `#{1 => 2,3 => 4}` (oracle).
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let three = ctx.int(3).expect("fits");
            let four = ctx.int(4).expect("fits");
            let map = crate::access::make_flatmap(ctx.heap(), &[(three, four), (one, two)])
                .expect("fits");
            assert_eq!(show(ctx, map, false), "#{1 => 2,3 => 4}");
            assert_eq!(show(ctx, map, true), "#{1 => 2,3 => 4}");

            // `~p` of `#{<<"k">> => 1}` is `#{<<"k">> => 1}` (printable propagates).
            let key = bits::make_binary(ctx.heap(), b"k").expect("fits");
            let map = crate::access::make_flatmap(ctx.heap(), &[(key, one)]).expect("fits");
            assert_eq!(show(ctx, map, false), "#{<<107>> => 1}");
            assert_eq!(show(ctx, map, true), "#{<<\"k\">> => 1}");

            // An empty map is `#{}`.
            let empty = ctx.atom("hi").expect("seeded");
            let map = crate::access::make_flatmap(ctx.heap(), &[]).expect("fits");
            assert_eq!(show(ctx, map, false), "#{}");
            assert_eq!(show(ctx, empty, false), "hi");
        });
    }

    #[test]
    fn atom_keys_sort_by_atom_index_and_others_by_term_order() {
        with_ctx(|ctx| {
            // Atoms `b` then `a`: `a` was seeded after `b`? The seeded order is
            // ["true","false","ok","hello","Hello","a","b","c","abc","hi"], so `a`
            // has a lower index than `b` and sorts first (oracle rule
            // `erts_cmp_flatmap_keys`: atom keys by raw term word).
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let map =
                crate::access::make_flatmap(ctx.heap(), &[(b, one), (a, two)]).expect("fits");
            assert_eq!(show(ctx, map, false), "#{a => 2,b => 1}");

            // Mixed classes use the term order (`1` < `a` < `{a}`).
            let tuple = ctx.tuple(&[a]).expect("fits");
            let map = crate::access::make_flatmap(ctx.heap(), &[(tuple, one), (a, two), (one, one)])
                .expect("fits");
            assert_eq!(show(ctx, map, false), "#{1 => 1,a => 2,{a} => 1}");
        });
    }

    #[test]
    fn depth_limit_matches_io_lib_write() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let c = ctx.atom("c").expect("seeded");
            let inner = ctx.tuple(&[c, make_small(1)]).expect("fits");
            let mid = ctx.tuple(&[b, inner]).expect("fits");
            let outer = ctx.tuple(&[a, mid]).expect("fits");
            // `~W` of `{a,{b,{c,1}}}`: -1 ⇒ full, 4 ⇒ `{a,{b,...}}`, 3 ⇒ `{a,{...}}`,
            // 2 ⇒ `{a,...}`, 1 ⇒ `{...}`, 0 ⇒ `...`.
            assert_eq!(show_at(ctx, outer, false, -1), "{a,{b,{c,1}}}");
            assert_eq!(show_at(ctx, outer, false, 4), "{a,{b,...}}");
            assert_eq!(show_at(ctx, outer, false, 3), "{a,{...}}");
            assert_eq!(show_at(ctx, outer, false, 2), "{a,...}");
            assert_eq!(show_at(ctx, outer, false, 1), "{...}");
            assert_eq!(show_at(ctx, outer, false, 0), "...");

            // `~W` of `[1,2,3,4,5]`.
            let list = ctx
                .list(&[
                    make_small(1),
                    make_small(2),
                    make_small(3),
                    make_small(4),
                    make_small(5),
                ])
                .expect("fits");
            assert_eq!(show_at(ctx, list, false, 3), "[1,2|...]");
            assert_eq!(show_at(ctx, list, false, 2), "[1|...]");
            assert_eq!(show_at(ctx, list, false, 1), "[...]");
            assert_eq!(show_at(ctx, list, false, 6), "[1,2,3,4,5]");
            assert_eq!(show_at(ctx, NIL, false, 1), "[]");
        });
    }

    #[test]
    fn binary_and_map_depth_limit() {
        with_ctx(|ctx| {
            let bin = bits::make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
            assert_eq!(show_at(ctx, bin, false, 1), "<<...>>");
            assert_eq!(show_at(ctx, bin, false, 2), "<<1,2,3>>");
            // A printable binary at depth 1 is still elided (oracle).
            let abc = bits::make_binary(ctx.heap(), b"abc").expect("fits");
            assert_eq!(show_at(ctx, abc, true, 1), "<<...>>");

            // `~W` of `#{a=>1,b=>2,c=>3}`: 1 ⇒ `#{}`, 2 ⇒ first pair then `,...`,
            // 3 ⇒ two pairs then `,...`.
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let c = ctx.atom("c").expect("seeded");
            let map = crate::access::make_flatmap(
                ctx.heap(),
                &[(a, make_small(1)), (b, make_small(2)), (c, make_small(3))],
            )
            .expect("fits");
            assert_eq!(show_at(ctx, map, false, 1), "#{}");
            assert_eq!(show_at(ctx, map, false, 2), "#{a => 1,...}");
            assert_eq!(show_at(ctx, map, false, 3), "#{a => 1,b => 2,...}");
        });
    }

    #[test]
    fn bignums_render_in_decimal() {
        with_ctx(|ctx| {
            // 123456789012345678901234567890 = 0x18EE90FF6C373E0EE4E3F0AD2.
            // `erl -noshell -eval 'io:format("~w~n",[123456789012345678901234567890])'`
            let value = 123456789012345678901234567890u128;
            let low = value as u64;
            let high = (value >> 64) as u64;
            let big = beam_codec::term::make_big(ctx.heap(), false, &[low, high]).expect("fits");
            assert_eq!(show(ctx, big, false), "123456789012345678901234567890");
            assert_eq!(show(ctx, big, true), "123456789012345678901234567890");

            // A negative bignum keeps the sign.
            let neg = beam_codec::term::make_big(ctx.heap(), true, &[low, high]).expect("fits");
            assert_eq!(show(ctx, neg, false), "-123456789012345678901234567890");

            // 2**128 (a single high digit) — `erl` prints `340282366920938463463374607431768211456`.
            let pow = beam_codec::term::make_big(ctx.heap(), false, &[0, 0, 1]).expect("fits");
            assert_eq!(show(ctx, pow, false), "340282366920938463463374607431768211456");
        });
    }

    #[test]
    fn big_decimal_conversion_edge_cases() {
        assert_eq!(format_big_decimal(false, &[0]), "0");
        assert_eq!(format_big_decimal(false, &[9]), "9");
        assert_eq!(format_big_decimal(false, &[10, 0]), "10");
        assert_eq!(format_big_decimal(true, &[255]), "-255");
        // 2**64 = 18446744073709551616.
        assert_eq!(format_big_decimal(false, &[0, 1]), "18446744073709551616");
    }

    #[test]
    fn internal_refs_render_with_the_ref_shape() {
        with_ctx(|ctx| {
            let reference = crate::access::make_ref_thing(ctx.heap(), 42).expect("fits");
            assert_eq!(show(ctx, reference, false), "#Ref<0.0.0.42>");
            assert_eq!(show(ctx, reference, true), "#Ref<0.0.0.42>");

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
            // An external pid is outside the M1 printable set.
            let node = ctx.atom("hi").expect("seeded");
            let external = beam_codec::term::make_external_pid(ctx.heap(), node, 1, 2, 3)
                .expect("fits");
            assert!(matches!(
                format_term(ctx, external, false),
                Err(beam_common::BeamError::BadArg)
            ));
            // Sanity: the atom itself still renders.
            assert_eq!(show(ctx, node, false), "hi");
        });
    }
}
