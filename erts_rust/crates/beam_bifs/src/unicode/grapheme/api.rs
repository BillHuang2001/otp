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
//! The two grapheme-cluster entry points.
//!
//! * [`next_grapheme`] is `string:next_grapheme/1`, which is literally
//!   `unicode_util:gc/1` (`lib/stdlib/src/string.erl` @1119) — see [`super::gc`].
//! * [`grapheme_count`] is `string:length/1` (`string.erl` @257, @1145-1199), which
//!   counts clusters by repeatedly splitting with the same walk.
//!
//! `string:length/1` is ported clause-by-clause, **including its fast paths**: the
//! ASCII-run clauses and the `length_b/4` binary walk are not merely faster, they
//! decide which *error* a malformed chardata raises (a proper `[97,[98|99]]` is a
//! `function_clause` for `string:length/1` even though `gc/1` walks it happily).
//! The one clause deliberately omitted is `length_b/4`'s 8-byte SWAR ASCII run
//! skip, which is a pure optimisation: whenever it fires, the following clause
//! fires on the same bytes and produces the identical count.
//!
//! The whole `length/1` family is tail-recursive in the oracle; here it is a
//! single state machine so a long input cannot exhaust the native stack.

use beam_term::{Eterm, NIL};
use beam_common::Result;

use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::error::{function_clause, structured_error};
use crate::unicode::Decode;

use super::{
    binary_opt, cp, decode_one_prefix, error_payload, gc, head_tail, is_error_tuple, latin1_value,
};

/// `string:next_grapheme(CD)` — the first extended grapheme cluster of `CD`.
///
/// Returns the oracle's three shapes: `[]`, `[Cluster | Rest]` (with `Rest` a
/// binary when the input's tail is one) or `{error, Rest}`.
///
/// # Errors
///
/// The oracle's own raises: the bare `error:function_clause` for an argument no
/// `gc/1`/`cp/1` clause matches (a float, a tuple, an out-of-range code point, a
/// non-byte-aligned bitstring, an improper integer tail).
pub(crate) fn next_grapheme(ctx: &mut BifContext<'_>, term: Eterm) -> Result<Eterm> {
    gc(ctx, term)
}

/// `?ASCII_LIST(CP1, CP2)`'s first half: `is_integer(X, 0, 255)` and `X =/= $\r`.
fn ascii_head(term: Eterm) -> bool {
    latin1_value(term).is_some_and(|value| value != 13)
}

/// The `string:length/1` walk — one state of its tail-recursion cycle.
enum State {
    /// `length_1(Str, N)`.
    L1 { str: Eterm, n: i64 },
    /// `length_disp(Str, N)`.
    Disp { str: Eterm, n: i64 },
    /// `length_gc(Str, N)`.
    Gc { str: Eterm, n: i64 },
    /// `length_b(Bytes[off..], CP1, Cont, N)`.
    B {
        bytes: Vec<u8>,
        off: usize,
        cp1: Eterm,
        cont: Eterm,
        n: i64,
    },
}

/// `string:length(CD)` — the number of extended grapheme clusters in `CD`.
///
/// # Errors
///
/// The oracle's own raises: the bare `error:function_clause` (an improper
/// chardata that reaches `length_gc/2`, an argument `gc/1` cannot walk) and the
/// structured `error:{badarg, Rest}` of `length_gc/2` on a binary whose head is
/// not a valid UTF-8 sequence.
pub(crate) fn grapheme_count(ctx: &mut BifContext<'_>, term: Eterm) -> Result<Eterm> {
    // length(<<CP1/utf8, Bin/binary>>) -> length_b(Bin, CP1, [], 0);
    if let Some(bytes) = binary_opt(term) {
        if let Decode::Ok(value, len) = decode_one_prefix(&bytes) {
            let cp1 = ctx.int(i64::from(value))?;
            return run(ctx, State::B { bytes, off: len, cp1, cont: NIL, n: 0 });
        }
    }
    // length(CD) -> length_1(CD, 0).
    run(ctx, State::L1 { str: term, n: 0 })
}

/// Drives the `length` family to its answer; every oracle tail call is a state
/// transition, so the native stack is bounded.
fn run(ctx: &mut BifContext<'_>, mut state: State) -> Result<Eterm> {
    loop {
        state = match state {
            // length_1([CP1|[CP2|_]=Cont], N) when ?ASCII_LIST(CP1,CP2)
            State::L1 { str, n } => {
                if let Some((cp1, cont)) = head_tail(str) {
                    if ascii_head(cp1) {
                        if let Some((cp2, _)) = head_tail(cont) {
                            if ascii_head(cp2) {
                                state = State::L1 { str: cont, n: n + 1 }; continue;
                            }
                        }
                    }
                }
                // length_1(Str, N) -> length_disp(Str, N)
                State::Disp { str, n }
            }
            State::Disp { str, n } => {
                // length_disp([CP1,Bin|Cont], N) when is_integer(CP1,0,255),
                // CP1 =/= $\r, is_binary(Bin) -> length_b(Bin, CP1, Cont, N)
                if let Some((cp1, rest)) = head_tail(str) {
                    if ascii_head(cp1) {
                        if let Some((bin, cont)) = head_tail(rest) {
                            if let Some(bytes) = binary_opt(bin) {
                                state = State::B { bytes, off: 0, cp1, cont, n }; continue;
                            }
                        }
                    }
                }
                // length_disp([Bin|Cont], N) when is_binary(Bin) -> case Bin of ...
                if let Some((bin, cont)) = head_tail(str) {
                    if let Some(bytes) = binary_opt(bin) {
                        if let Decode::Ok(value, len) = decode_one_prefix(&bytes) {
                            let cp1 = ctx.int(i64::from(value))?;
                            state = State::B { bytes, off: len, cp1, cont, n }; continue;
                        }
                        if bytes.is_empty() {
                            state = State::L1 { str: cont, n }; continue;
                        }
                        state = State::Gc { str, n }; continue;
                    }
                }
                // length_disp(Bin, N) when is_binary(Bin) -> case Bin of ...
                if let Some(bytes) = binary_opt(str) {
                    if let Decode::Ok(value, len) = decode_one_prefix(&bytes) {
                        let cp1 = ctx.int(i64::from(value))?;
                        state = State::B { bytes, off: len, cp1, cont: NIL, n }; continue;
                    }
                    if bytes.is_empty() {
                        return ctx.int(n);
                    }
                    state = State::Gc { str, n }; continue;
                }
                // length_disp(Str, N) -> length_gc(Str, N)
                State::Gc { str, n }
            }
            State::Gc { str, n } => {
                // length_gc(Str, N) -> case unicode_util:gc(Str) of
                let cluster = gc(ctx, str)?;
                if access::is_nil(cluster) {
                    return ctx.int(n);
                }
                if let Some((_head, rest)) = head_tail(cluster) {
                    // [_|Rest] -> length_1(Rest, N+1)
                    state = State::L1 { str: rest, n: n + 1 }; continue;
                }
                // {error, Err} -> error({badarg, Err})
                if is_error_tuple(ctx, cluster) {
                    let payload = error_payload(cluster);
                    return Err(structured_error(ctx, "badarg", &[payload])?);
                }
                return Err(structured_error(ctx, "case_clause", &[cluster])?);
            }
            State::B { bytes, off, cp1, cont, n } => {
                let bin0 = &bytes[off..];
                // length_b(<<CP2/utf8, Rest/binary>>, CP1, Cont, N) when ?ASCII_LIST(CP1,CP2)
                if ascii_head(cp1) {
                    if let Decode::Ok(cp2, len) = decode_one_prefix(bin0) {
                        if cp2 < 256 {
                            let cp2 = ctx.int(i64::from(cp2))?;
                            state = State::B {
                                bytes,
                                off: off + len,
                                cp1: cp2,
                                cont,
                                n: n + 1,
                            }; continue;
                        }
                    }
                }
                // length_b(<<>>, CP1, Cont, N) -> case Cont of [] -> N+1; _ -> length_gc/2
                if bin0.is_empty() {
                    if access::is_nil(cont) {
                        return ctx.int(n + 1);
                    }
                    let list = ctx.cons(cp1, cont)?;
                    state = State::Gc { str: list, n }; continue;
                }
                // length_b(Bin0, CP1, Cont, N) ->
                //     [_|Cont1] = unicode_util:gc([CP1,Bin0|Cont]),
                //     case unicode_util:cp(Cont1) of ...
                //
                // The list is `[CP1,Bin0|Cont]` — a **flat** two-element prefix in
                // front of `Cont`, not `[[CP1,Bin0]|Cont]`: `gc/1`'s "losing work
                // done on T" roll-backs hand `T0` (this very term) to `gc_ext_pict`'s
                // `++`, and an improper prefix would make that `++` raise.
                let bin0 = bits::make_binary(ctx.heap(), bin0)?;
                let inner = ctx.cons(bin0, cont)?;
                let combined = ctx.cons(cp1, inner)?;
                let cluster = gc(ctx, combined)?;
                let Some((_head, cont1)) = head_tail(cluster) else {
                    // `[_|Cont1]` always matches for a `gc/1` result headed by an
                    // integer, but never fabricate one when it does not.
                    return Err(function_clause());
                };
                let normalized = cp(ctx, cont1)?;
                // [] -> N+1
                if access::is_nil(normalized) {
                    return ctx.int(n + 1);
                }
                // [CP3|Bin] when is_binary(Bin) -> length_b(Bin, CP3, [], N+1)
                if let Some((cp3, bin)) = head_tail(normalized) {
                    if let Some(rest) = binary_opt(bin) {
                        state = State::B {
                            bytes: rest,
                            off: 0,
                            cp1: cp3,
                            cont: NIL,
                            n: n + 1,
                        }; continue;
                    }
                }
                // {error, Err} -> error({badarg, Err})
                if is_error_tuple(ctx, normalized) {
                    let payload = error_payload(normalized);
                    return Err(structured_error(ctx, "badarg", &[payload])?);
                }
                // [_|_] = Rest -> length_gc(Rest, N+1)
                state = State::Gc { str: normalized, n: n + 1 }; continue;
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_codec::term::make_binary;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    fn context<'a>(
        heap: &'a mut Heap,
        atoms: &'a AtomTable,
    ) -> BifContext<'a> {
        BifContext::new(heap, atoms, ProcessId::new(1, 0))
    }

    fn seeded() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["error", "true", "false"]);
        atoms
    }

    #[test]
    fn length_counts_clusters() {
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let atoms = seeded();
        let mut ctx = context(&mut heap, &atoms);
        let cases: &[(&[i64], i64)] = &[
            (&[], 0),
            (&[97], 1),
            (&[97, 98, 99], 3),
            (&[97, 769, 98], 2),
            (&[13, 10, 97], 2),
            (&[255, 254], 2),
            (&[1536, 97], 1),
            (&[127462, 127463, 127464], 2),
        ];
        for (input, want) in cases {
            let items: Vec<Eterm> = input.iter().map(|v| ctx.int(*v).unwrap()).collect();
            let list = ctx.list(&items).unwrap();
            let got = grapheme_count(&mut ctx, list).expect("a valid chardata counts");
            assert_eq!(
                beam_term::signed_value(got),
                *want,
                "length of {input:?}"
            );
        }
        let bin = make_binary(ctx.heap(), &[97, 204, 129, 98]).unwrap();
        let got = grapheme_count(&mut ctx, bin).expect("a valid binary counts");
        assert_eq!(beam_term::signed_value(got), 2);
        let empty = make_binary(ctx.heap(), &[]).unwrap();
        let got = grapheme_count(&mut ctx, empty).expect("an empty binary counts");
        assert_eq!(beam_term::signed_value(got), 0);
    }

    #[test]
    fn length_raises_on_an_improper_integer_tail() {
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let atoms = seeded();
        let mut ctx = context(&mut heap, &atoms);
        let head = ctx.int(97).unwrap();
        let tail = ctx.int(98).unwrap();
        let improper = ctx.cons(head, tail).unwrap();
        let error = grapheme_count(&mut ctx, improper).expect_err("must raise");
        assert_eq!(error.as_atom(), Some("function_clause"));
        // `next_grapheme/1` raises it too: `gc/1`'s list clauses all fall through to
        // `cp/1`, which has no clause for the integer tail `98`
        // (`unicode_util:cp/1` line 417 via `gc_1/2` line 601; live-oracle confirmed).
        let error = next_grapheme(&mut ctx, improper).expect_err("must raise");
        assert_eq!(error.as_atom(), Some("function_clause"));
    }

    #[test]
    fn next_grapheme_returns_the_first_cluster() {
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let atoms = seeded();
        let mut ctx = context(&mut heap, &atoms);
        let items: Vec<Eterm> = [97, 769, 98].iter().map(|v| ctx.int(*v).unwrap()).collect();
        let list = ctx.list(&items).unwrap();
        let got = next_grapheme(&mut ctx, list).unwrap();
        let Some((cluster, rest)) = head_tail(got) else {
            panic!("next_grapheme must return a cons");
        };
        // The remainder keeps the input's own tail **as a list**: the answer is
        // `[[97,769],98]`, i.e. `[[97,769] | [98]]`, so the cdr is the one-element
        // list `[98]` rather than the bare integer `98`
        // (`string:next_grapheme([97,769,98])`, live-oracle confirmed).
        let Some((tail_head, tail_tail)) = head_tail(rest) else {
            panic!("the remainder is a list, not the bare integer tail");
        };
        assert_eq!(beam_term::signed_value(tail_head), 98);
        assert!(access::is_nil(tail_tail));
        // The cluster is the list `[97, 769]` — walk it element by element.
        let Some((first, cluster_rest)) = head_tail(cluster) else {
            panic!("the cluster must be a list");
        };
        assert_eq!(beam_term::signed_value(first), 97);
        let Some((second, cluster_tail)) = head_tail(cluster_rest) else {
            panic!("the cluster holds both code points");
        };
        assert_eq!(beam_term::signed_value(second), 769);
        assert!(access::is_nil(cluster_tail));
        let empty = next_grapheme(&mut ctx, NIL).unwrap();
        assert!(access::is_nil(empty));
    }
}
