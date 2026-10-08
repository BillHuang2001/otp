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

//! `string:list_to_float/1` — the one real `bif.tab` BIF of the `string` module.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/bif.c:3521-3688`
//! (`string_list_to_float_1`, the character-at-a-time scanner built from the
//! `SIGN`/`INT`/`FRAC`/`EXP_SIGN`/`EXP0`/`EXP1`/`END` states, `SAVE_E`/`LOAD_E`,
//! `IS_DOT`/`IS_E`/`IS_DIGIT` and the `back_to_e:` label) and
//! `../../erts/emulator/beam/bif.tab:479` (`bif string:list_to_float/1`).
//! `../../lib/stdlib/src/string.erl:182` shows the loader replaces
//! `string.erl`'s `list_to_float(_) -> erlang:nif_error(undef).` stub with this
//! BIF, so the BIF — not a stdlib function — is the observable behaviour.
//!
//! # Result shape (pinned against the oracle)
//!
//! This is **not** `erlang:list_to_float/1`. `string:list_to_float/1` consumes a
//! *prefix* of a character list and reports the leftover:
//!
//! * a float was scanned ⇒ `{Float, Rest}` — `string:list_to_float("1.5")` ⇒
//!   `{1.5, []}`, `string:list_to_float("1.2abc")` ⇒ `{1.2, "abc"}`,
//!   `string:list_to_float("2.3e+4")` ⇒ `{2.3e4, []}`,
//!   `string:list_to_float("2.3e+")` ⇒ `{2.3, "e+"}` (an exponent that never
//!   starts is *given back* — the `SAVE_E`/`LOAD_E` restore),
//!   `string:list_to_float("1.2" ++ [a])` ⇒ `{1.2, [a]}`, and for an improper tail
//!   the raw tail: `string:list_to_float([49,46,50|97])` ⇒ `{1.2, 97}`;
//! * nothing float-like was scanned ⇒ `{error, no_float}` — `"1"` (the scanner
//!   *requires* an integer part, a radix and a fraction: `"1e2"`, `".5"`, `"1."`,
//!   `"2.3e"`... are `no_float`), `[]`, `"abc"`;
//! * the argument is not a list at all ⇒ `{error, not_a_list}` (`string:list_to_float(5)`).
//!
//! The value itself is `sys_chars_to_double` over the scanned prefix (a `,` radix
//! is accepted: `"2,3"` ⇒ `{2.3, []}`), which [`crate::numeric::parse_float`]
//! already mirrors — including its failure mode (a non-finite overflow such as
//! `"1.0e309"` is `{error, no_float}`, never a `±inf` float).
use beam_common::Result;
use beam_term::{signed_value, Eterm};
use crate::access;
use crate::context::BifContext;
use crate::numeric::parse_float;
use crate::registry::BifRegistry;
/// The scanner states of `string_list_to_float_1` (`bif.c:3521`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Part {
    /// Expecting an optional leading `+`/`-` (or a digit).
    Sign,
    /// Inside the integer part.
    Int,
    /// Inside the fractional part (after `.`/`,`).
    Frac,
    /// Just after an `e`/`E`: expecting an optional exponent sign.
    ExpSign,
    /// Expecting the first exponent digit.
    Exp0,
    /// Inside the exponent (at least one digit seen).
    Exp1,
    /// The prefix ended.
    End,
}
/// The C's `back_to_e:` tail: a pending `e`/`E` that turned out **not** to belong
/// to the float is given back, i.e. the buffer and the list position are restored
/// to just past the fractional part (`LOAD_E`).
fn back_to_e(part: Part, buf: &mut Vec<u8>, list: &mut Eterm, saved_len: usize, saved_list: Eterm) {
    if matches!(part, Part::ExpSign | Part::Exp0) {
        buf.truncate(saved_len);
        *list = saved_list;
    }
}
/// The `{error, Reason}` failure shape.
fn error_tuple(ctx: &mut BifContext<'_>, reason: &str) -> Result<Eterm> {
    let error = ctx.intern_atom("error")?;
    let reason = ctx.intern_atom(reason)?;
    ctx.tuple(&[error, reason])
}
/// `string:list_to_float/1`.
fn list_to_float_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let arg = args[0];
    // `is_nil(list)` ⇒ `{error, no_float}`; `is_not_list(list)` ⇒ `{error, not_a_list}`.
    if access::is_nil(arg) {
        return error_tuple(ctx, "no_float");
    }
    if !access::is_list(arg) {
        return error_tuple(ctx, "not_a_list");
    }
    let mut list = arg;
    let mut buf: Vec<u8> = Vec::new();
    let mut part = Part::Sign;
    // `SAVE_E`'s saved position: the buffer length and the list cell holding the
    // `e`/`E` that may or may not belong to the float.
    let mut saved_len = 0usize;
    let mut saved_list = arg;
    loop {
        let head = access::car(list);
        if !access::is_small(head) {
            // `is_not_small(CAR(list))` ⇒ `back_to_e`.
            back_to_e(part, &mut buf, &mut list, saved_len, saved_list);
            break;
        }
        let ch = signed_value(head);
        // A leading `+` is consumed but *not* copied into the buffer (`goto skip`).
        let mut append = true;
        if ch == i64::from(b'-') {
            match part {
                Part::Sign => part = Part::Int,
                Part::ExpSign => part = Part::Exp0,
                Part::Exp0 => {
                    back_to_e(part, &mut buf, &mut list, saved_len, saved_list);
                    part = Part::End;
                }
                _ => part = Part::End,
            }
        } else if ch == i64::from(b'+') {
            match part {
                Part::Sign => {
                    part = Part::Int;
                    append = false;
                }
                Part::ExpSign => part = Part::Exp0,
                Part::Exp0 => {
                    back_to_e(part, &mut buf, &mut list, saved_len, saved_list);
                    part = Part::End;
                }
                _ => part = Part::End,
            }
        } else if ch == i64::from(b'.') || ch == i64::from(b',') {
            // `IS_DOT`
            match part {
                Part::Int => part = Part::Frac,
                Part::ExpSign | Part::Exp0 => {
                    back_to_e(part, &mut buf, &mut list, saved_len, saved_list);
                    part = Part::End;
                }
                _ => part = Part::End,
            }
        } else if ch == i64::from(b'e') || ch == i64::from(b'E') {
            // `IS_E`: an `e` in the fractional part *may* be an exponent, so the
            // position is remembered before committing to it.
            match part {
                Part::Frac => {
                    saved_len = buf.len();
                    saved_list = list;
                    part = Part::ExpSign;
                }
                Part::Exp0 | Part::ExpSign => {
                    back_to_e(part, &mut buf, &mut list, saved_len, saved_list);
                    part = Part::End;
                }
                _ => part = Part::End,
            }
        } else if (i64::from(b'0')..=i64::from(b'9')).contains(&ch) {
            // `IS_DIGIT`: a `SIGN` digit starts the integer part, an exponent digit
            // (`EXP_SIGN`/`EXP0`) starts the exponent; `INT`/`FRAC`/`EXP1` continue.
            match part {
                Part::Sign => part = Part::Int,
                Part::ExpSign | Part::Exp0 => part = Part::Exp1,
                _ => {}
            }
        } else {
            // "character not part of float - done" ⇒ `back_to_e`.
            back_to_e(part, &mut buf, &mut list, saved_len, saved_list);
            break;
        }
        if part == Part::End {
            // "we require a fractional part": `i < 3` means no complete
            // `<int><radix><fraction>` prefix was scanned.
            if buf.len() < 3 {
                return error_tuple(ctx, "no_float");
            }
            break;
        }
        if append {
            // `buf[i++] = unsigned_val(...)`: the value is truncated to a byte.
            buf.push(ch as u8);
        }
        // `skip:`
        list = access::cdr(list);
        if access::is_nil(list) || !access::is_list(list) {
            back_to_e(part, &mut buf, &mut list, saved_len, saved_list);
            break;
        }
    }
    if buf.is_empty() {
        return error_tuple(ctx, "no_float");
    }
    // `sys_chars_to_double` over the scanned prefix.
    match parse_float(&String::from_utf8_lossy(&buf)) {
        Ok(value) => {
            let float = ctx.make_float(value)?;
            ctx.tuple(&[float, list])
        }
        Err(_) => error_tuple(ctx, "no_float"),
    }
}
/// Registers `string:list_to_float/1` (the module's only `bif.tab` entry).
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("string", "list_to_float", 1, list_to_float_1);
}
#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{atom_val, AtomTable, NIL};
    /// A context with the atoms the scanner needs (`error`, `no_float`, `not_a_list`).
    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["error", "no_float", "not_a_list", "a"]);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }
    /// The character list of `text`.
    fn chars(ctx: &mut BifContext<'_>, text: &str) -> Eterm {
        let elems: Vec<Eterm> = text.bytes().map(|b| ctx.int(i64::from(b)).expect("a byte")).collect();
        ctx.list(&elems).expect("a short list fits")
    }
    /// The float value of a `{Float, Rest}` result, or `None` for an `{error, _}`.
    fn value_of(term: Eterm) -> Option<f64> {
        if !access::is_tuple(term) {
            return None;
        }
        let first = access::tuple_element(term, 0);
        if access::is_float(first) {
            Some(access::float_value(first))
        } else {
            None
        }
    }
    /// The reason atom of an `{error, Reason}` result.
    fn reason_of(atoms: &AtomTable, term: Eterm) -> Option<String> {
        if !access::is_tuple(term) || access::tuple_size(term) != 2 {
            return None;
        }
        let tag = access::tuple_element(term, 0);
        let reason = access::tuple_element(term, 1);
        if access::is_atom(tag) && atoms.name(atom_val(tag)) == Some("error") && access::is_atom(reason)
        {
            atoms.name(atom_val(reason)).map(|s| s.to_string())
        } else {
            None
        }
    }
    /// `string:list_to_float(Text)` (oracle-pinned below).
    fn call(ctx: &mut BifContext<'_>, text: &str) -> Result<Eterm> {
        let arg = chars(ctx, text);
        list_to_float_1(ctx, &[arg])
    }
    #[test]
    fn scans_a_float_and_returns_the_rest() {
        with_ctx(|ctx| {
            // `{Float, Rest}`, every value from the oracle.
            for (text, value, rest) in [
            ("1.5", 1.5f64, ""),
            ("2.3", 2.3, ""),
            ("2,3", 2.3, ""),
            ("0.0", 0.0, ""),
            ("1.2E10", 1.2e10, ""),
            ("2.3e+4", 23000.0, ""),
            ("+1.2", 1.2, ""),
            ("-1.2", -1.2, ""),
            ("1.2abc", 1.2, "abc"),
            ("1.2-3", 1.2, "-3"),
            ("1.2.3", 1.2, ".3"),
            ("2.3e", 2.3, "e"),
            ("2.3e+", 2.3, "e+"),
            ("2.3e+x", 2.3, "e+x"),
        ] {
            let result = call(ctx, text).expect("a scanned float or a rest");
            assert_eq!(value_of(result), Some(value), "{text} value");
            let rest_term = access::tuple_element(result, 1);
            let expected: Vec<Eterm> = rest
                .bytes()
                .map(|b| ctx.int(i64::from(b)).expect("a byte"))
                .collect();
            let expected = if expected.is_empty() { NIL } else { ctx.list(&expected).unwrap() };
            // Structural (`=:=`) equality: the rest is the *tail of the original
            // argument*, so its raw words differ from a freshly built list.
            assert!(
                beam_term::compare::eq(rest_term, expected, ctx.atoms()),
                "{text} rest"
            );
        }
        });
    }
    #[test]
    fn rejects_what_the_scanner_cannot_use() {
        with_ctx(|ctx| {
            // `{error, no_float}`: no complete `<int><radix><fraction>` prefix.
            for text in ["", "1", "abc", "1e2", ".5", "1.", "+", "-.5", "e"] {
                let result = call(ctx, text).expect("an error tuple");
                assert_eq!(reason_of(ctx.atoms(), result).as_deref(), Some("no_float"), "{text}");
            }
            // A non-list argument is `{error, not_a_list}`.
            for not_a_list in [beam_term::make_small(5), beam_term::NIL] {
                let result = list_to_float_1(ctx, &[not_a_list]).expect("an error tuple");
                let expected = if access::is_nil(not_a_list) { "no_float" } else { "not_a_list" };
                assert_eq!(reason_of(ctx.atoms(), result).as_deref(), Some(expected));
            }
        });
    }
    #[test]
    fn an_improper_tail_is_returned_verbatim() {
        with_ctx(|ctx| {
            // `[49,46,50|97]` ⇒ `{1.2, 97}` (the tail, not a list).
            let one = ctx.int(49).unwrap();
            let dot = ctx.int(46).unwrap();
            let two = ctx.int(50).unwrap();
            let tail = ctx.int(97).unwrap();
            let proper = ctx.list(&[one, dot, two]).unwrap();
            let improper = {
                // Build `[49,46,50|97]` by replacing the tail of the proper list.
                let pair = ctx.cons(two, tail).unwrap();
                let pair = ctx.cons(dot, pair).unwrap();
                ctx.cons(one, pair).unwrap()
            };
            assert!(proper != improper);
            let result = list_to_float_1(ctx, &[improper]).expect("a scanned float");
            assert_eq!(value_of(result), Some(1.2));
            assert_eq!(access::tuple_element(result, 1), tail);
        });
    }
    #[test]
    fn a_trailing_non_integer_element_ends_the_scan() {
        with_ctx(|ctx| {
            // A non-small element is not a character: `back_to_e` and the tail is
            // the whole remaining list.
            let prefix = chars(ctx, "1.2");
            let tuple = ctx.tuple(&[ctx.int(7).unwrap()]).unwrap();
            // `[49,46,50 | {7}]`
            let elems = crate::print::proper_list(prefix).expect("a proper list");
            let mut improper = tuple;
            for &elem in elems.iter().rev() {
                improper = ctx.cons(elem, improper).unwrap();
            }
            let result = list_to_float_1(ctx, &[improper]).expect("a scanned float");
            assert_eq!(value_of(result), Some(1.2));
            assert_eq!(access::tuple_element(result, 1), tuple);
        });
    }
    #[test]
    fn badarg_is_never_reported() {
        with_ctx(|ctx| {
            // Every failure shape is a *result*, never a raise.
            let big = ctx.tuple(&[beam_term::make_small(1)]).unwrap();
            assert!(list_to_float_1(ctx, &[big]).is_ok());
            assert!(!matches!(call(ctx, "1"), Err(BeamError::BadArg)));
        });
    }
    #[test]
    fn the_bif_is_registered() {
        assert!(crate::registry::default_registry().contains("string", "list_to_float", 1));
    }
}
