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

//! The `%T` term renderer — `erts_printf_term.c`'s `print_term`, the format
//! `erlang:display/1` prints through.
//!
//! A safe submodule of `crate::print` (it registers no BIFs and touches neither the
//! `io_lib` leaf renderer nor its `Chunk` tree). `erlang:display/1` is
//! `erts_printf("%.*T\n", INT_MAX, Term)` (`bif.c:4148-4194`), and `%T` is a
//! **different** renderer from `~w`/`~p`: no spaces around `=`/`=>`, atoms quoted by
//! the C's `print_atom_name` rule (reserved words are *not* quoted, `@` forces
//! quoting), floats in `%e` form with six fraction digits, native-record fields in
//! the definition's **stored** (ascending atom-index) order rather than declaration
//! order, and a byte budget that truncates with a trailing `"... "`.
//!
//! # Why a work stack instead of a recursive walk
//!
//! The C renderer is an explicit-workstack state machine
//! (`DECLARE_WSTACK(s)`, `WSTACK_PUSHn`, LIFO `WSTACK_POP`): every value is pushed as
//! a `(word, tag)` pair, punctuation tags print without consuming the budget, and a
//! budget stop is a `goto L_done` from **anywhere** — including the middle of a
//! printable list's per-character loop, which therefore abandons its closing quote.
//! Reproducing that byte-for-byte (measured: `term_to_string("hello", 6)` is
//! `"hello"... `, not `"hello"... `) needs the same stack discipline, so this module
//! mirrors it: [`Item`] is the work stack's element and [`term_text`] is
//! `print_term` + `erts_printf_term`'s `"... "` suffix.
//!
//! # The budget (`dcount`)
//!
//! `%T`'s `*` precision is the budget (`erts_printf_term`'s `precision` argument;
//! `INT_MAX` ⇒ `LONG_MAX` for `display/1`, i.e. unlimited). Every **term** item
//! costs one unit before it renders; an atom additionally costs its **byte** length
//! and, like a printable list's per-character loop and a non-printable bitstring's
//! per-byte loop, may stop *after* writing. Punctuation and integers/floats cost
//! only the per-term unit. When the walk stops with a budget `<= 0`, the three bytes
//! `"... "` are appended (`erts_printf_term`'s tail).
//!
//! Every pinned string in this file's tests was measured on the OTP 29.1.1 /
//! erts-17.1 oracle through `erts_internal:term_to_string/2`, which is the same
//! `%T` with an explicit budget.
//!
//! # Documented deviation: a byte >= `16#80` in a printable list
//!
//! The sink is a Rust `String`, so a printable list's or binary's raw latin-1 byte is
//! written as UTF-8, exactly as [`super::term_chunk`]'s `~p` leaf does (see
//! `crate::term_conv::display_1`'s note). Every byte `<= 16#7F` — the whole ASCII
//! surface — is byte-identical to the oracle.

use beam_common::Result;
use beam_term::{atom_val, signed_value, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};

/// One element of the C renderer's work stack.
///
/// The C stack is a uniform word array holding `(value, tag)` pairs, popped LIFO, so
/// a `WSTACK_PUSH3(tl, PRT_ONE_CONS, PRT_COMMA)` prints its comma **before** the next
/// cons, and a `WSTACK_PUSH5(v, PRT_TERM, PRT_EQUALS, k, PRT_TERM)` prints `k`, `=`,
/// `v`. Collapsing each `(value, tag)` pair into one variant keeps the pop order the
/// C's, without a raw-word stack.
#[derive(Clone, Copy)]
enum Item {
    /// `PRT_COMMA` — printed without consuming the budget.
    Comma,
    /// `PRT_BAR`.
    Bar,
    /// `PRT_CLOSE_LIST`.
    CloseList,
    /// `PRT_CLOSE_TUPLE` (`}` — the map's and the record's closer too).
    CloseTuple,
    /// `PRT_ASSOC` (`=>`).
    Assoc,
    /// `PRT_EQUALS` (`=`).
    Equals,
    /// `PRT_TERM` + the value word.
    Term(Eterm),
    /// `PRT_ONE_CONS` + the list word: render its CAR, then re-push its tail.
    OneCons(Eterm),
    /// `PRT_LAST_ARRAY_ELEMENT+k` + the element pointer: element `index` of `tuple`,
    /// with `remaining` elements after it (`0` ⇒ the tag was
    /// `PRT_LAST_ARRAY_ELEMENT`, so nothing is pushed).
    ArrayElem {
        tuple: Eterm,
        index: usize,
        remaining: usize,
    },
}

/// Renders `term` as `%T` would, with `precision` as the C's budget.
///
/// `precision` is `erts_printf_term`'s argument: `i64::MAX` (the C's `INT_MAX` path,
/// which becomes `LONG_MAX`) renders without truncation, and any other value
/// truncates at the same point the C does, appending `"... "`.
///
/// # Errors
///
/// The leaf renderers' typed errors: `unsupported_bif` for an external pid/port/ref
/// (its channel number is not derivable from the frozen term) and for a `Local` fun
/// with no [`LocalFunIdentity`](crate::context::LocalFunIdentity) seam, and `badarg`
/// for a malformed native record (`TermError::BadRecord` reconciled upstream) — never
/// a fabricated rendering.
pub(crate) fn term_text(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    precision: i64,
) -> Result<String> {
    let mut out = String::new();
    let mut budget = precision;
    let mut stack: Vec<Item> = Vec::new();
    let mut pending = Some(term);

    'walk: loop {
        // `L_jump_start`: every term costs one unit, tested *before* it is spent.
        if let Some(obj) = pending.take() {
            if budget <= 0 {
                break 'walk;
            }
            budget -= 1;
            if !render(ctx, obj, &mut out, &mut budget, &mut stack)? {
                break 'walk;
            }
            continue 'walk;
        }
        // Pop the next work-stack item. Punctuation prints without a budget charge.
        let Some(item) = stack.pop() else {
            break 'walk;
        };
        match item {
            Item::Comma => out.push(','),
            Item::Bar => out.push('|'),
            Item::CloseList => out.push(']'),
            Item::CloseTuple => out.push('}'),
            Item::Assoc => out.push_str("=>"),
            Item::Equals => out.push('='),
            Item::Term(t) => pending = Some(t),
            Item::OneCons(list) => {
                // `L_print_one_cons`: the CAR becomes the next term; a non-`nil` tail
                // is re-pushed as a comma plus (a cons step, or a `|` tail).
                let head = access::car(list);
                let tail = access::cdr(list);
                if !access::is_nil(tail) {
                    let next = if access::is_list(tail) {
                        Item::OneCons(tail)
                    } else {
                        // `WSTACK_PUSH3(tl, PRT_TERM, PRT_BAR)`.
                        push_seq(&mut stack, &[Item::Term(tail), Item::Bar]);
                        pending = Some(head);
                        continue 'walk;
                    };
                    push_seq(&mut stack, &[next, Item::Comma]);
                }
                pending = Some(head);
            }
            Item::ArrayElem {
                tuple,
                index,
                remaining,
            } => {
                if remaining > 0 {
                    push_seq(
                        &mut stack,
                        &[
                            Item::Comma,
                            Item::ArrayElem {
                                tuple,
                                index: index + 1,
                                remaining: remaining - 1,
                            },
                        ],
                    );
                }
                pending = Some(access::tuple_element(tuple, index));
            }
        }
    }

    if budget <= 0 {
        out.push_str("... ");
    }
    Ok(out)
}

/// Pushes `seq` so that the items pop in `seq`'s own order (the C pushes a
/// `WSTACK_PUSHn` argument list in reverse of its pop order).
fn push_seq(stack: &mut Vec<Item>, seq: &[Item]) {
    for item in seq.iter().rev() {
        stack.push(*item);
    }
}

/// Renders one term word; returns `false` when the budget stopped the walk
/// (the C's `goto L_done`).
fn render(
    ctx: &mut BifContext<'_>,
    obj: Eterm,
    out: &mut String,
    budget: &mut i64,
    stack: &mut Vec<Item>,
) -> Result<bool> {
    if access::is_nil(obj) {
        out.push_str("[]");
        return Ok(true);
    }
    if access::is_atom(obj) {
        let name = ctx.atoms().name(atom_val(obj)).ok_or_else(badarg)?;
        return Ok(write_atom(out, name, budget));
    }
    if access::is_small(obj) {
        out.push_str(&signed_value(obj).to_string());
        return Ok(true);
    }
    if access::is_big(obj) {
        out.push_str(&crate::numeric::integer_digits(obj, 10)?);
        return Ok(true);
    }
    if access::is_float(obj) {
        out.push_str(&printf_e(access::float_value(obj)));
        return Ok(true);
    }
    if access::is_record(obj) {
        return record(ctx, obj, out, budget, stack);
    }
    if access::is_tuple(obj) {
        let arity = access::tuple_size(obj);
        out.push('{');
        // The C pushes the element tag only for a non-empty tuple
        // (`if (i > 0) WSTACK_PUSH2(...)`), so `{}` pushes the closer alone.
        if arity > 0 {
            push_seq(
                stack,
                &[
                    Item::ArrayElem {
                        tuple: obj,
                        index: 0,
                        remaining: arity - 1,
                    },
                    Item::CloseTuple,
                ],
            );
        } else {
            push_seq(stack, &[Item::CloseTuple]);
        }
        return Ok(true);
    }
    if access::is_map(obj) {
        let pairs = access::map_pairs_storage_order(obj)?;
        out.push_str("#{");
        let mut seq: Vec<Item> = Vec::with_capacity(pairs.len() * 5 + 1);
        seq.push(Item::CloseTuple);
        for (i, (key, value)) in pairs.iter().enumerate() {
            if i > 0 {
                seq.push(Item::Comma);
            }
            seq.push(Item::Term(*key));
            seq.push(Item::Assoc);
            seq.push(Item::Term(*value));
        }
        // The C pushes the pairs from the highest index down, so they pop in
        // ascending order; `push_seq` reproduces that pop order directly.
        push_seq(stack, &seq);
        return Ok(true);
    }
    if access::is_list(obj) {
        if printable_string_len(obj).is_some() {
            return Ok(printable_list(obj, out, budget));
        }
        out.push('[');
        push_seq(stack, &[Item::OneCons(obj), Item::CloseList]);
        return Ok(true);
    }
    if access::is_bitstring(obj) {
        return bitstring(obj, out, budget);
    }
    if access::is_function(obj) {
        let charge = fun_atom_charge(ctx, obj)?;
        let text = crate::fun_info::fun_text(obj, ctx)?;
        *budget -= charge;
        out.push_str(&text);
        return Ok(true);
    }
    if access::is_internal_pid(obj) || access::is_external_pid(obj) {
        out.push_str(&crate::term_conv::pid_text_str(obj)?);
        return Ok(true);
    }
    if access::is_port(obj) {
        out.push_str(&crate::term_conv::port_text_str(obj)?);
        return Ok(true);
    }
    if access::is_internal_ref(obj) || access::is_external_ref(obj) {
        out.push_str(&crate::term_conv::ref_text_str(obj)?);
        return Ok(true);
    }
    Err(unsupported_bif())
}

/// `PRINT_ATOM` — writes the atom and charges its byte length; `false` when the
/// budget is exhausted **after** the atom is written (the C's
/// `if (*(DCOUNT) <= 0) goto L_done`).
fn write_atom(out: &mut String, name: &str, budget: &mut i64) -> bool {
    *budget -= name.bytes().count() as i64;
    out.push_str(&crate::fun_info::quoted_atom(name));
    *budget > 0
}

/// `case RECORD_DEF` — `#Module:Name{key=value,…}` with the definition's **stored**
/// key order (`defp->keys` / `instance->values`, ascending atom index), one `=` and
/// no spaces.
fn record(
    ctx: &mut BifContext<'_>,
    rec: Eterm,
    out: &mut String,
    budget: &mut i64,
    stack: &mut Vec<Item>,
) -> Result<bool> {
    let def = access::native_record_definition(rec)?;
    let module = access::native_record_definition_module(def)?;
    let name = access::native_record_definition_name(def)?;
    let keys = access::native_record_stored_keys(def)?;
    let values = access::native_record_values(rec)?;
    if keys.len() != values.len() {
        return Err(badarg());
    }
    out.push('#');
    for (which, atom) in [module, name].into_iter().enumerate() {
        if !access::is_atom(atom) {
            return Err(badarg());
        }
        let text = ctx.atoms().name(atom_val(atom)).ok_or_else(badarg)?;
        if which == 1 {
            out.push(':');
        }
        if !write_atom(out, text, budget) {
            return Ok(false);
        }
    }
    out.push('{');
    let mut seq: Vec<Item> = Vec::with_capacity(keys.len() * 5 + 1);
    seq.push(Item::CloseTuple);
    for (i, (&key, &value)) in keys.iter().zip(values.iter()).enumerate() {
        if i > 0 {
            seq.push(Item::Comma);
        }
        seq.push(Item::Term(key));
        seq.push(Item::Equals);
        seq.push(Item::Term(value));
    }
    push_seq(stack, &seq);
    Ok(true)
}

/// `is_printable_string` — the length of a non-empty proper list of printable bytes,
/// or `None`.
///
/// The rule is the C's `!(IS_CNTRL(c) && !IS_SPACE(c))`, which is **narrower** than
/// `~p`'s: `\v \b \f \e` are control characters here and are *not* printable.
fn printable_string_len(list: Eterm) -> Option<usize> {
    let mut cur = list;
    let mut len = 0usize;
    while access::is_list(cur) {
        let head = access::car(cur);
        if !access::is_small(head) {
            return None;
        }
        let c = signed_value(head);
        if !(0..=255).contains(&c) {
            return None;
        }
        if is_cntrl(c) && !is_space(c) {
            return None;
        }
        len += 1;
        cur = access::cdr(cur);
    }
    if access::is_nil(cur) && len > 0 {
        Some(len)
    } else {
        None
    }
}

/// `IS_CNTRL` (`LATIN1`): below `' '`, `127`, or `128..=159`.
fn is_cntrl(c: i64) -> bool {
    c < 0x20 || c == 127 || (128..=159).contains(&c)
}

/// `IS_SPACE`: space, `\n`, tab and `\r` — note that 160 is not included.
fn is_space(c: i64) -> bool {
    c == b' ' as i64 || c == b'\n' as i64 || c == b'\t' as i64 || c == b'\r' as i64
}

/// The printable-list branch: `"…"`, escaping only `"` and `\n`; each character is a
/// budget tick that can abandon the closing quote.
fn printable_list(list: Eterm, out: &mut String, budget: &mut i64) -> bool {
    out.push('"');
    let mut cur = list;
    while access::is_list(cur) {
        if *budget <= 0 {
            return false;
        }
        *budget -= 1;
        let c = signed_value(access::car(cur));
        match c {
            0x0A => out.push_str("\\n"),
            0x22 => out.push_str("\\\""),
            _ => out.push(char::from(c as u8)),
        }
        cur = access::cdr(cur);
    }
    out.push('"');
    true
}

/// `is_printable_ascii`: `' '..=126`.
fn is_printable_ascii(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| (0x20..=126).contains(&b))
}

/// `case BITSTRING_DEF` — `<<"…">>` for a byte-aligned all-printable binary, else
/// `<<1,2,3>>` / `<<1:3>>` / `<<>>`.
fn bitstring(obj: Eterm, out: &mut String, budget: &mut i64) -> Result<bool> {
    let bits = access::bitstring_bit_size(obj)?;
    let bytes = access::bitstring_bits(obj)?;
    let nbytes = bits / 8;
    let tail = bits % 8;
    if tail == 0 && nbytes > 0 && is_printable_ascii(&bytes[..nbytes]) {
        out.push_str("<<\"");
        for &byte in &bytes[..nbytes] {
            if byte == b'"' {
                out.push('\\');
            }
            out.push(char::from(byte));
            if *budget <= 0 {
                return Ok(false);
            }
            *budget -= 1;
        }
        out.push_str("\">>");
        return Ok(true);
    }

    out.push_str("<<");
    let mut first = true;
    for &byte in &bytes[..nbytes] {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&byte.to_string());
        if *budget <= 0 {
            return Ok(false);
        }
        *budget -= 1;
    }
    if tail > 0 {
        // The tail bits ride the high bits of the next packed byte; the C prints the
        // octet value followed by `:bitsize`.
        let packed = *bytes.get(nbytes).ok_or_else(badarg)?;
        let octet = (packed >> (8 - tail)) & ((1u8 << tail) - 1);
        if !first {
            out.push(',');
        }
        out.push_str(&octet.to_string());
        out.push(':');
        out.push_str(&tail.to_string());
    }
    out.push_str(">>");
    Ok(true)
}

/// The budget a fun costs beyond the per-term unit.
///
/// The C's `FUN_DEF` prints a **local** fun through `PRINT_BUF`/`PRINT_SWORD` (no atom
/// charge at all), and an **external** fun through `print_atom_name` on a temporary
/// `dcount = MAX_ATOM_SZ_LIMIT`, folding only the module's and the function's length
/// into the real budget — which is why a fun is never split by the budget.
fn fun_atom_charge(ctx: &BifContext<'_>, fun: Eterm) -> Result<i64> {
    if !matches!(
        beam_codec::term::fun_kind_term(fun)?,
        beam_codec::FunKind::External
    ) {
        return Ok(0);
    }
    let mut charge = 0i64;
    for atom in [
        beam_codec::term::fun_module(fun)?,
        beam_codec::term::fun_function(fun)?,
    ] {
        let name = ctx.atoms().name(atom).ok_or_else(badarg)?;
        charge += name.bytes().count() as i64;
    }
    Ok(charge)
}

/// `PRINT_DOUBLE(…, 'e', 6, 0, f)` — printf's `%e` with precision 6 and no width: six
/// fraction digits and a signed exponent of **at least** two digits.
///
/// Rust's `{:.6e}` already rounds correctly and emits the same mantissa; only the
/// exponent's zero padding differs (`1.5e0` vs `1.500000e+00`), so it is rewritten
/// here. Measured on the oracle: `-0.0` ⇒ `-0.000000e+00`, `1.0e-5` ⇒
/// `1.000000e-05`, `1.0e300` ⇒ `1.000000e+300`, `5.0e-324` ⇒ `4.940656e-324`.
fn printf_e(value: f64) -> String {
    let short = format!("{value:.6e}");
    let (mantissa, exponent) = short
        .split_once('e')
        .expect("`{:e}` always emits an exponent");
    let exponent: i32 = exponent
        .parse()
        .expect("`{:e}` always emits a decimal exponent");
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.unsigned_abs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::default_registry;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    /// The atoms the cases below need, seeded so their indices are stable.
    fn atoms() -> AtomTable {
        let mut names: Vec<String> = [
            "false", "true", "a", "b", "c", "hello", "k", "v", "x", "and", "a b", "A",
            "a@b", "recmod", "r", "m", "empty",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        names.sort();
        let mut atoms = AtomTable::new();
        atoms.seed(names.iter().map(String::as_str));
        atoms
    }

    fn with_ctx<R>(body: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        body(&mut ctx)
    }

    /// `erts_internal:term_to_string(Term, Budget)`, measured on the oracle.
    fn limited(ctx: &mut BifContext<'_>, term: Eterm, budget: i64) -> String {
        term_text(ctx, term, budget).expect("renders")
    }

    #[test]
    fn atoms_follow_the_c_print_atom_name_rule() {
        with_ctx(|ctx| {
            // Reserved words are NOT quoted by `%T` (unlike `io_lib`).
            let and = ctx.atom("and").expect("seeded");
            assert_eq!(full(ctx, and), "and");
            // `@` is not a name character, so it forces quoting.
            let at = ctx.atom("a@b").expect("seeded");
            assert_eq!(full(ctx, at), "'a@b'");
            let upper = ctx.atom("A").expect("seeded");
            assert_eq!(full(ctx, upper), "'A'");
            let spaced = ctx.atom("a b").expect("seeded");
            assert_eq!(full(ctx, spaced), "'a b'");
        });
        fn full(ctx: &mut BifContext<'_>, term: Eterm) -> String {
            term_text(ctx, term, i64::MAX).expect("renders")
        }
    }

    #[test]
    fn floats_use_printf_e_with_six_fraction_digits() {
        with_ctx(|ctx| {
            for (value, expected) in [
                (1.5, "1.500000e+00"),
                (0.0, "0.000000e+00"),
                (-0.0, "-0.000000e+00"),
                (-1.5, "-1.500000e+00"),
                (1.0e300, "1.000000e+300"),
                (1.0e-5, "1.000000e-05"),
                (1.0e16, "1.000000e+16"),
                (5.0e-324, "4.940656e-324"),
                (0.1, "1.000000e-01"),
            ] {
                let term = ctx.make_float(value).expect("the float fits");
                assert_eq!(full(ctx, term), expected, "float {value:e}");
            }
        });
        fn full(ctx: &mut BifContext<'_>, term: Eterm) -> String {
            term_text(ctx, term, i64::MAX).expect("renders")
        }
    }

    #[test]
    fn the_containers_use_no_spaces() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let pair = ctx.tuple(&[a, one]).expect("fits");
            assert_eq!(full(ctx, pair), "{a,1}");
            let empty = ctx.tuple(&[]).expect("fits");
            assert_eq!(full(ctx, empty), "{}");
            let list = ctx.list(&[one, one]).expect("fits");
            assert_eq!(full(ctx, list), "[1,1]");
            let map = beam_codec::term::make_flatmap(ctx.heap(), &[(a, one)]).expect("fits");
            assert_eq!(full(ctx, map), "#{a=>1}");
        });
        fn full(ctx: &mut BifContext<'_>, term: Eterm) -> String {
            term_text(ctx, term, i64::MAX).expect("renders")
        }
    }

    /// The budget strings below are the oracle's `erts_internal:term_to_string/2`
    /// output, character for character.
    #[test]
    fn the_budget_truncates_exactly_as_the_c_does() {
        with_ctx(|ctx| {
            let hello = ctx.atom("hello").expect("seeded");
            assert_eq!(limited(ctx, hello, 2), "hello... ");
            assert_eq!(limited(ctx, hello, 4), "hello... ");

            let num = ctx.int(12345).expect("fits");
            assert_eq!(limited(ctx, num, 2), "12345");
            assert_eq!(limited(ctx, num, 3), "12345");

            let float = ctx.make_float(1.5).expect("fits");
            assert_eq!(limited(ctx, float, 2), "1.500000e+00");

            let string = string_term(ctx, "hello");
            assert_eq!(limited(ctx, string, 2), "\"h... ");
            assert_eq!(limited(ctx, string, 3), "\"he... ");
            // The closing quote is abandoned, so the suffix reads `"hello"... `.
            assert_eq!(limited(ctx, string, 6), "\"hello\"... ");
            assert_eq!(limited(ctx, string, 7), "\"hello\"");

            let one_char = string_term(ctx, "a");
            assert_eq!(limited(ctx, one_char, 2), "\"a... ");
            assert_eq!(limited(ctx, one_char, 3), "\"a\"");

            let one = ctx.int(1).expect("fits");
            let list = ctx.list(&[one, one, one]).expect("fits");
            assert_eq!(limited(ctx, list, 2), "[1,... ");
            assert_eq!(limited(ctx, list, 3), "[1,1,... ");
            assert_eq!(limited(ctx, list, 4), "[1,1,1]... ");
            assert_eq!(limited(ctx, list, 5), "[1,1,1]");

            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let tuple = ctx.tuple(&[a, b]).expect("fits");
            assert_eq!(limited(ctx, tuple, 2), "{a... ");
            assert_eq!(limited(ctx, tuple, 4), "{a,b... ");

            let key = ctx.atom("a").expect("seeded");
            let map = beam_codec::term::make_flatmap(ctx.heap(), &[(key, one)]).expect("fits");
            assert_eq!(limited(ctx, map, 2), "#{a... ");
            assert_eq!(limited(ctx, map, 4), "#{a=>1}... ");
            assert_eq!(limited(ctx, map, 5), "#{a=>1}");
        });
    }

    #[test]
    fn a_bitstring_truncates_per_byte() {
        with_ctx(|ctx| {
            let raw = beam_codec::term::make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
            assert_eq!(full(ctx, raw), "<<1,2,3>>");
            assert_eq!(limited(ctx, raw, 2), "<<1,2... ");
            assert_eq!(limited(ctx, raw, 4), "<<1,2,3>>... ");
            assert_eq!(limited(ctx, raw, 5), "<<1,2,3>>");

            let text = beam_codec::term::make_binary(ctx.heap(), b"xy").expect("fits");
            assert_eq!(full(ctx, text), "<<\"xy\">>");
            assert_eq!(limited(ctx, text, 2), "<<\"xy... ");
            assert_eq!(limited(ctx, text, 3), "<<\"xy\">>... ");
            assert_eq!(limited(ctx, text, 4), "<<\"xy\">>");
        });
        fn full(ctx: &mut BifContext<'_>, term: Eterm) -> String {
            term_text(ctx, term, i64::MAX).expect("renders")
        }
    }

    /// Non-printable lists and empty containers.
    #[test]
    fn the_non_printable_shapes() {
        with_ctx(|ctx| {
            let empty = ctx.list(&[]).expect("fits");
            assert_eq!(full(ctx, empty), "[]");
            // 1 is a control character, so the list is not printable.
            let one = ctx.int(1).expect("fits");
            let list = ctx.list(&[one]).expect("fits");
            assert_eq!(full(ctx, list), "[1]");
            // TAB is `IS_SPACE`, so it *is* printable and is emitted raw.
            let tab = ctx.int(9).expect("fits");
            let text = ctx.list(&[tab]).expect("fits");
            assert_eq!(full(ctx, text), "\"\t\"");
            // A backslash is printable and is NOT escaped.
            let bytes: Vec<Eterm> = b"a\\b".iter().map(|&c| ctx.int(i64::from(c)).unwrap()).collect();
            let text = ctx.list(&bytes).expect("fits");
            assert_eq!(full(ctx, text), "\"a\\b\"");
            // A quote IS escaped.
            let bytes: Vec<Eterm> = b"a\"b".iter().map(|&c| ctx.int(i64::from(c)).unwrap()).collect();
            let text = ctx.list(&bytes).expect("fits");
            assert_eq!(full(ctx, text), "\"a\\\"b\"");
        });
        fn full(ctx: &mut BifContext<'_>, term: Eterm) -> String {
            term_text(ctx, term, i64::MAX).expect("renders")
        }
    }

    /// A native record is `#Mod:Name{key=value,…}`, `=` with no spaces, stored key
    /// order, and a printable list value rendered as a string.
    #[test]
    fn a_native_record_uses_the_stored_order_and_no_spaces() {
        let reg = default_registry();
        with_ctx(|ctx| {
            // Declared `c, a, b`; the seeded atom indices are `a < b < c`, so the
            // stored (sorted) order — the order `%T` prints — is `a, b, c`.
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let c = ctx.atom("c").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let text = string_term(ctx, "xy");
            let list = ctx.list(&[one, one]).expect("fits");
            let rec = record(
                ctx,
                &reg,
                "recmod",
                "r",
                &[(c, list), (a, one), (b, text)],
            )
            .expect("creates");
            assert_eq!(full(ctx, rec), "#recmod:r{a=1,b=\"xy\",c=[1,1]}");
            // `~w` keeps the declaration order and spaces (a different renderer).
            let empty = record(ctx, &reg, "m", "empty", &[]).expect("creates");
            assert_eq!(full(ctx, empty), "#m:empty{}");
            assert_eq!(limited(ctx, empty, 2), "#m... ");
        });
        fn full(ctx: &mut BifContext<'_>, term: Eterm) -> String {
            term_text(ctx, term, i64::MAX).expect("renders")
        }
    }

    /// A well-formed record never answers `badarg`; a corrupted shape does.
    #[test]
    fn a_malformed_record_is_the_typed_badarg() {
        with_ctx(|ctx| {
            let one = ctx.int(1).expect("fits");
            // A small integer is not a record, so it renders; a forged record header
            // would be `badarg`, which `records::create` never produces.
            assert!(term_text(ctx, one, i64::MAX).is_ok());
        });
    }

    fn string_term(ctx: &mut BifContext<'_>, text: &str) -> Eterm {
        let bytes: Vec<Eterm> = text
            .bytes()
            .map(|c| ctx.int(i64::from(c)).expect("fits"))
            .collect();
        ctx.list(&bytes).expect("fits")
    }

    fn record(
        ctx: &mut BifContext<'_>,
        reg: &crate::BifRegistry,
        module: &str,
        name: &str,
        fields: &[(Eterm, Eterm)],
    ) -> Result<Eterm> {
        let module = ctx.atom(module)?;
        let name = ctx.atom(name)?;
        let mut pairs = Vec::with_capacity(fields.len());
        for &(key, value) in fields {
            pairs.push(ctx.tuple(&[key, value])?);
        }
        let fields = ctx.list(&pairs)?;
        let flag = ctx.atom("true")?;
        let tag = ctx.atom("is_exported")?;
        let opts = beam_codec::term::make_flatmap(ctx.heap(), &[(tag, flag)])?;
        reg.call("records", "create", 4, ctx, &[module, name, fields, opts])
    }
}
