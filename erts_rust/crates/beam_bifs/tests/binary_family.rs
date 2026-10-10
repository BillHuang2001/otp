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

//! Integration tests for the `binary` BIF family, driven through
//! [`BifRegistry::call`](beam_bifs::BifRegistry::call) (the crate's public seam),
//! over an **off-heap** refc binary — the `[BinRef][ErlSubBits]` representation
//! `beam_engine`'s `bs_create_bin`/`bs_init_writable` builds for any payload above
//! `ERL_ONHEAP_BINARY_LIMIT` (64) bytes
//! (`beam_engine/src/families/bitstrings/raw.rs::make_binary`).
//!
//! This file is the regression gate for `access::binary_bytes`, the single byte
//! reader behind `bits::bytes` and therefore behind `binary:at/2`, `binary:part/2,3`,
//! `binary:first/1`, `binary:last/1`, `binary:copy/1,2`, `binary:match/2,3`,
//! `binary:matches/2,3`, `binary:split/2,3`, `binary:decode_unsigned/1,2`,
//! `binary:longest_common_prefix/1`, `binary:longest_common_suffix/1`,
//! `binary_to_list/1`, `binary:bin_to_list/1,2,3`, `iolist_to_binary/1` and the
//! `erlang` digests. That reader once assumed *every* binary was an on-heap
//! `ErlHeapBits`; an off-heap `ErlSubBits` slipped past its tag mask
//! (`SUB_BITS_SUBTAG = 0x24` masked with `BITSTRING_TAG_MASK = 0x3B` is
//! `HEAP_BITS_SUBTAG = 0x20`), so the bytes were read from `boxed_val + 2` — i.e.
//! out of the `ErlSubBits` fields — a **silent** wrong value, not an error.
//! Building the subject *in runtime* (never handing it in as a fixture) is what makes
//! the difference visible, so every test here builds the 96-byte
//! `<<(X rem 256) || X <- lists:seq(1, 96)>>` off-heap first.
//!
//! Every expectation is a **golden pinned to the system oracle** (OTP 29.1.1 /
//! erts-17.1), derived once with the real `erl` (`-eval`, or an escript for the
//! multi-line cases):
//!
//! ```text
//! erl -noshell -eval 'B = << <<(X rem 256)>> || X <- lists:seq(1,96) >>,
//!   io:format("~p ~p ~p~n",[binary:at(B,0),binary:first(B),binary:last(B)]),halt().'
//!   % 1 1 96
//! erl -noshell -eval 'B = << <<(X rem 256)>> || X <- lists:seq(1,96) >>,
//!   io:format("~p~n",[binary:part(B,0,4)]),halt().'            % <<1,2,3,4>>
//! erl -noshell -eval 'B = << <<(X rem 256)>> || X <- lists:seq(1,96) >>,
//!   io:format("~p~n",[lists:sum(binary_to_list(B))]),halt().'   % 4656
//! erl -noshell -eval 'B = << <<(X rem 256)>> || X <- lists:seq(1,96) >>,
//!   io:format("~p~n",[erlang:md5(B)]),halt().'
//!   % <<174,52,151,181,35,32,197,210,50,26,201,39,51,241,197,145>>
//! erl -noshell -eval 'B = << <<(X rem 256)>> || X <- lists:seq(1,96) >>,
//!   io:format("~p ~p~n",[erlang:crc32(B),erlang:adler32(B)]),halt().'
//!   % 2026415647 1386091057
//! ```
//!
//! The bitstring arguments/results are built and read with the sibling **safe**
//! `beam_codec::term` helpers (`bitstring_bytes`/`make_binary`/`cons_head`/
//! `cons_tail`/`tuple_element`) because the crate's own bitstring helpers are
//! private; the off-heap term itself is built here through `beam_term`'s public
//! frozen writers, exactly as the engine builds it.

use beam_alloc::{Heap, RefcBinary};
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term as codec_term;
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{compare::eq, is_list, is_nil, is_small, make_atom, signed_value, AtomTable, Eterm};

/// The oracle's in-runtime subject: `<<(X rem 256) || X <- lists:seq(1, 96)>>` —
/// the bytes `1..96`.
///
/// It deliberately exceeds `ERL_ONHEAP_BINARY_LIMIT` (64) so the engine's
/// `bs_create_bin` stores it off-heap behind a `[BinRef][ErlSubBits]` pair.
fn subject() -> Vec<u8> {
    (1..=96u32).map(|x| (x % 256) as u8).collect()
}

/// A registry + heap + atom table the tests drive BIF calls through, plus the
/// off-heap payloads the refc-binary terms point into (kept alive here so the raw
/// base pointer stays valid for as long as the term is used).
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
    payloads: Vec<RefcBinary>,
}

impl Fixture {
    /// No optional seam is installed: none of these BIFs needs one. The seeded atoms
    /// are the ones the `binary` BIFs mint or compare against (`nomatch` for
    /// `match/2,3`, `little` for `decode_unsigned/2`) plus the option atoms of
    /// `split/2,3`.
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true", "false", "ok", "nomatch", "little", "global", "trim", "trim_all", "scope",
        ]);
        Fixture {
            registry: default_registry(),
            heap: Heap::new(1 << 16).expect("test heap"),
            atoms,
            payloads: Vec::new(),
        }
    }

    /// Calls `module:function(Args)` through the real registry.
    fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry.call(module, function, args.len() as u32, &mut ctx, args)
    }

    /// Calls `module:function(Args)` and unwraps the result.
    fn ok(&mut self, module: &str, function: &str, args: &[Eterm]) -> Eterm {
        self.call(module, function, args)
            .unwrap_or_else(|e| panic!("{module}:{function}/{} failed: {e:?}", args.len()))
    }

    /// The small-int terms for `values`.
    fn ints(&mut self, values: &[i64]) -> Vec<Eterm> {
        let ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        values.iter().map(|&v| ctx.int(v).expect("a small int fits")).collect()
    }

    /// The proper list `[Items…]`.
    fn list(&mut self, items: &[Eterm]) -> Eterm {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        ctx.list(items).expect("the list fits")
    }

    /// The on-heap binary `<<Bytes>>`.
    fn binary(&mut self, bytes: &[u8]) -> Eterm {
        codec_term::make_binary(&mut self.heap, bytes).expect("the binary fits")
    }

    /// The **off-heap** refc binary over `bytes` whose bit window is
    /// `offset_bits..offset_bits + size_bits`, built exactly as the engine's
    /// `bs_create_bin` builds it (a `[BinRef][ErlSubBits]` pair over a
    /// `beam_alloc::RefcBinary` payload, `orig = boxed(BinRef)`).
    fn refc(&mut self, bytes: &[u8], offset_bits: usize, size_bits: usize) -> Eterm {
        let payload = RefcBinary::new(bytes).expect("the off-heap payload allocates");
        let base = payload.data_ptr();
        // Moving the handle into `payloads` does not move the payload it points at
        // (that is off-heap), so `base` stays valid for the term's whole lifetime.
        self.payloads.push(payload);
        // SAFETY: `alloc_words` hands out `ERL_REFC_BITS_SIZE` (8) exclusively-owned,
        // writable words — exactly the pair `write_refc_binary` fully writes; `base`
        // is the first byte of the (now fixture-owned) payload's live, 4-byte-aligned
        // data area and covers bits `offset_bits..offset_bits + size_bits`.
        let hp = unsafe { self.heap.alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE) }
            .expect("the 8-word pair fits");
        unsafe {
            beam_term::refc::write_refc_binary(
                hp,
                base.cast::<Eterm>(),
                core::ptr::null_mut(),
                base,
                offset_bits,
                size_bits,
            )
        }
    }

    /// The whole-binary off-heap form of `bytes` (`start = 0`, the full bit size).
    fn whole_refc(&mut self, bytes: &[u8]) -> Eterm {
        self.refc(bytes, 0, bytes.len() * 8)
    }

    /// Whether `term` is the seeded atom `name`.
    fn is_atom(&self, term: Eterm, name: &str) -> bool {
        self.atoms.lookup(name).is_some_and(|index| eq(term, make_atom(index), &self.atoms))
    }
}

/// The bytes of a byte-aligned binary result.
fn bytes_of(term: Eterm) -> Vec<u8> {
    let (bytes, offset, size_bits) = codec_term::bitstring_bytes(term).expect("a bitstring result");
    assert_eq!(offset % 8, 0, "the result must start on a byte boundary");
    assert_eq!(size_bits % 8, 0, "the result must be a whole number of bytes");
    bytes[offset / 8..offset / 8 + size_bits / 8].to_vec()
}

/// The small integers of a proper-list-of-bytes result.
fn ints_of(term: Eterm) -> Vec<i64> {
    let mut values = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        let head = codec_term::cons_head(cur).expect("a cons head");
        assert!(is_small(head), "a byte-list element must be a small int");
        values.push(signed_value(head));
        cur = codec_term::cons_tail(cur).expect("a cons tail");
    }
    assert!(is_nil(cur), "the result must be a proper list");
    values
}

/// The terms of a proper-list result.
fn terms_of(term: Eterm) -> Vec<Eterm> {
    let mut items = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        items.push(codec_term::cons_head(cur).expect("a cons head"));
        cur = codec_term::cons_tail(cur).expect("a cons tail");
    }
    assert!(is_nil(cur), "the result must be a proper list");
    items
}

/// The two small integers of a `{Start, Len}` result.
fn small_pair(term: Eterm) -> (i64, i64) {
    assert_eq!(codec_term::tuple_arity(term).expect("a tuple result"), 2);
    let start = codec_term::tuple_element(term, 0).expect("element 0");
    let len = codec_term::tuple_element(term, 1).expect("element 1");
    (signed_value(start), signed_value(len))
}

/// The seeded atom `name` as a term.
fn seeded_atom(atoms: &AtomTable, name: &str) -> Eterm {
    make_atom(atoms.lookup(name).unwrap_or_else(|| panic!("`{name}` must be seeded")))
}

/// The oracle's `binary:decode_unsigned(B)` for the 96-byte subject `<<1,…,96>>`
/// (OTP 29.1.1 / erts-17.1).
const ORACLE_DECIMAL: &str = concat!(
    "61121819550785311403020184397754810326145762457804587843382902227553870647860435",
    "66331485930951687787086887490326267662295731249385519300026293541598009109905979",
    "861189716840843073708775420056930031023879947580407539078335941205856"
);

#[test]
fn at_first_last_and_part_read_the_off_heap_payload() {
    let mut f = Fixture::new();
    let b = f.whole_refc(&subject());

    // `binary:at(B,0)`⇒1, `at(B,4)`⇒5, `at(B,70)`⇒71, `at(B,95)`⇒96 (oracle).
    let indices = f.ints(&[0, 4, 70, 95]);
    for (index, expected) in indices.iter().zip([1i64, 5, 71, 96]) {
        assert_eq!(
            signed_value(f.ok("binary", "at", &[b, *index])),
            expected,
            "binary:at/2 of an off-heap binary"
        );
    }
    // `binary:first(B)`⇒1, `binary:last(B)`⇒96.
    assert_eq!(signed_value(f.ok("binary", "first", &[b])), 1);
    assert_eq!(signed_value(f.ok("binary", "last", &[b])), 96);

    // `binary:part/3`: `part(B,0,4)`⇒`<<1,2,3,4>>`, `part(B,4,1)`⇒`<<5>>`,
    // `part(B,92,4)`⇒`<<93,94,95,96>>`, `part(B,64,10)`⇒`<<"ABCDEFGHIJ">>`.
    let args = f.ints(&[0, 4, 92, 64, 4, 1, 4, 10]);
    let cases = [
        (args[0], args[4], vec![1u8, 2, 3, 4]),
        (args[1], args[5], vec![5]),
        (args[2], args[6], vec![93, 94, 95, 96]),
        (args[3], args[7], (65..=74u8).collect()),
    ];
    for (start, len, expected) in cases {
        assert_eq!(
            bytes_of(f.ok("binary", "part", &[b, start, len])),
            expected,
            "binary:part/3 of an off-heap binary"
        );
    }

    // The off-heap window is byte-for-byte the in-runtime subject.
    assert_eq!(bytes_of(b), subject());
}

#[test]
fn binary_to_list_and_bin_to_list_read_the_off_heap_payload() {
    let mut f = Fixture::new();
    let b = f.whole_refc(&subject());

    // `binary_to_list(B)` is `[1,…,96]`; `lists:sum` of it is 4656 (oracle) —
    // pinned here as the golden aggregate the old reader returned garbage for.
    let listed = ints_of(f.ok("erlang", "binary_to_list", &[b]));
    assert_eq!(listed, (1..=96i64).collect::<Vec<i64>>());
    assert_eq!(listed.iter().sum::<i64>(), 4656);

    // `binary:bin_to_list/1` is the identical list.
    assert_eq!(ints_of(f.ok("binary", "bin_to_list", &[b])), listed);

    // `bin_to_list(B,2,4)`⇒`[3,4,5,6]`, `bin_to_list(B,0,3)`⇒`[1,2,3]` (oracle).
    let args = f.ints(&[2, 4, 0, 3]);
    assert_eq!(ints_of(f.ok("binary", "bin_to_list", &[b, args[0], args[1]])), vec![3, 4, 5, 6]);
    assert_eq!(ints_of(f.ok("binary", "bin_to_list", &[b, args[2], args[3]])), vec![1, 2, 3]);

    // The tail of the subject (past the 64-byte on-heap limit) reads correctly too.
    let args = f.ints(&[64, 32]);
    assert_eq!(
        ints_of(f.ok("binary", "bin_to_list", &[b, args[0], args[1]])),
        (65..=96i64).collect::<Vec<i64>>()
    );
}

#[test]
fn match_matches_and_split_read_the_off_heap_payload() {
    let mut f = Fixture::new();
    let b = f.whole_refc(&subject());
    let two = f.binary(&[2]);
    let pair = f.binary(&[16, 17]);
    let absent = f.binary(&[200]);

    // `binary:match(B,<<2>>)`⇒`{1,1}` (`2` is the second byte), `match(B,<<16,17>>)`
    // ⇒`{15,2}` (byte 16 is at index 15), `match(B,<<200>>)`⇒`nomatch` (oracle).
    assert_eq!(small_pair(f.ok("binary", "match", &[b, two])), (1, 1));
    assert_eq!(small_pair(f.ok("binary", "match", &[b, pair])), (15, 2));
    let nomatch = f.ok("binary", "match", &[b, absent]);
    assert!(f.is_atom(nomatch, "nomatch"), "binary:match/2 of an absent needle");

    // `binary:matches(B,<<2>>)`⇒`[{1,1}]`, `matches(B,<<16,17>>)`⇒`[{15,2}]` (oracle).
    let found = terms_of(f.ok("binary", "matches", &[b, two]));
    assert_eq!(found.len(), 1);
    assert_eq!(small_pair(found[0]), (1, 1));
    let found = terms_of(f.ok("binary", "matches", &[b, pair]));
    assert_eq!(found.len(), 1);
    assert_eq!(small_pair(found[0]), (15, 2));
    // A needle in the off-heap tail (byte 70) is found at the right index.
    let late = f.binary(&[70]);
    let found = terms_of(f.ok("binary", "matches", &[b, late]));
    assert_eq!(found.len(), 1);
    assert_eq!(small_pair(found[0]), (69, 1));

    // `binary:split(B,<<2>>)`⇒`[<<1>>,<<3,…,96>>]` (first occurrence only),
    // `split(B,<<50>>)`⇒`[<<1,…,49>>,<<51,…,96>>]` (oracle).
    let parts = terms_of(f.ok("binary", "split", &[b, two]));
    assert_eq!(parts.len(), 2);
    assert_eq!(bytes_of(parts[0]), vec![1]);
    assert_eq!(bytes_of(parts[1]), (3..=96u8).collect::<Vec<u8>>());
    let fifty = f.binary(&[50]);
    let parts = terms_of(f.ok("binary", "split", &[b, fifty]));
    assert_eq!(parts.len(), 2);
    assert_eq!(bytes_of(parts[0]), (1..=49u8).collect::<Vec<u8>>());
    assert_eq!(bytes_of(parts[1]), (51..=96u8).collect::<Vec<u8>>());
}

#[test]
fn longest_common_prefix_and_suffix_read_the_off_heap_payload() {
    let mut f = Fixture::new();
    let b = f.whole_refc(&subject());

    // `longest_common_prefix([B, <<1,2,3,4,5,6,7,99>>])`⇒7 and
    // `longest_common_suffix([B, <<1,2,3,95,96>>])`⇒2 (oracle).
    let head = f.binary(&[1, 2, 3, 4, 5, 6, 7, 99]);
    let pair = f.list(&[b, head]);
    assert_eq!(signed_value(f.ok("binary", "longest_common_prefix", &[pair])), 7);
    let tail = f.binary(&[1, 2, 3, 95, 96]);
    let pair = f.list(&[b, tail]);
    assert_eq!(signed_value(f.ok("binary", "longest_common_suffix", &[pair])), 2);

    // The subject against itself shares every byte (oracle: 96 both ways) — this is
    // the case that reads *two* off-heap windows.
    let same = f.list(&[b, b]);
    assert_eq!(signed_value(f.ok("binary", "longest_common_prefix", &[same])), 96);
    assert_eq!(signed_value(f.ok("binary", "longest_common_suffix", &[same])), 96);
}

#[test]
fn decode_unsigned_reads_the_off_heap_payload() {
    let mut f = Fixture::new();
    let b = f.whole_refc(&subject());

    // `binary:decode_unsigned(<<0,…,0,1>>)` over a 65-byte (>64) payload is the small
    // int 1 — the cleanest golden for the off-heap byte reader.
    let mut mostly_zero = vec![0u8; 65];
    *mostly_zero.last_mut().expect("non-empty") = 1;
    let z = f.whole_refc(&mostly_zero);
    assert_eq!(signed_value(f.ok("binary", "decode_unsigned", &[z])), 1);

    // `decode_unsigned(B)` is the 96-byte big-endian magnitude (oracle). The golden is
    // the oracle's decimal rendering, materialized as a bignum through the
    // independent `erlang:list_to_integer/1` path.
    let got = f.ok("binary", "decode_unsigned", &[b]);
    let digits: Vec<Eterm> = ORACLE_DECIMAL
        .bytes()
        .map(|d| f.ints(&[i64::from(d)]).remove(0))
        .collect();
    let decimal = f.list(&digits);
    let expected = f.ok("erlang", "list_to_integer", &[decimal]);
    assert!(eq(got, expected, &f.atoms), "binary:decode_unsigned/1 of the 96-byte subject");

    // `binary:decode_unsigned/2` with `little` (oracle:
    // `binary:decode_unsigned(<<1,2,3>>, little)` = 197121).
    let three = f.binary(&[1, 2, 3]);
    let little = seeded_atom(&f.atoms, "little");
    assert_eq!(signed_value(f.ok("binary", "decode_unsigned", &[three, little])), 197121);
}

#[test]
fn iolist_to_binary_and_copy_read_the_off_heap_payload() {
    let mut f = Fixture::new();
    let b = f.whole_refc(&subject());

    // `iolist_to_binary([B])`⇒`B` and `iolist_to_binary([B,<<7>>])`⇒`B ++ <<7>>`.
    let wrapped = f.list(&[b]);
    assert_eq!(bytes_of(f.ok("erlang", "iolist_to_binary", &[wrapped])), subject());
    let seven = f.binary(&[7]);
    let wrapped = f.list(&[b, seven]);
    let mut with_seven = subject();
    with_seven.push(7);
    assert_eq!(bytes_of(f.ok("erlang", "iolist_to_binary", &[wrapped])), with_seven);

    // `binary:copy/1,2` copy the off-heap bytes verbatim (`copy(B,2)`⇒`B ++ B`).
    assert_eq!(bytes_of(f.ok("binary", "copy", &[b])), subject());
    let two = f.ints(&[2]).remove(0);
    let mut doubled = subject();
    doubled.extend(subject());
    assert_eq!(bytes_of(f.ok("binary", "copy", &[b, two])), doubled);
}

#[test]
fn digests_read_the_off_heap_payload() {
    let mut f = Fixture::new();
    let b = f.whole_refc(&subject());

    // Oracle: `erlang:md5(B)` =
    // `<<174,52,151,181,35,32,197,210,50,26,201,39,51,241,197,145>>`.
    assert_eq!(
        bytes_of(f.ok("erlang", "md5", &[b])),
        vec![174, 52, 151, 181, 35, 32, 197, 210, 50, 26, 201, 39, 51, 241, 197, 145]
    );
    // Oracle: `erlang:crc32(B)` = 2026415647, `erlang:adler32(B)` = 1386091057.
    assert_eq!(signed_value(f.ok("erlang", "crc32", &[b])), 2026415647);
    assert_eq!(signed_value(f.ok("erlang", "adler32", &[b])), 1386091057);

    // The same digests over a byte-equal **on-heap** binary agree, so the digest
    // itself is not at issue — only which bytes the reader handed it.
    let on_heap = f.binary(&subject());
    assert_eq!(
        bytes_of(f.ok("erlang", "md5", &[on_heap])),
        bytes_of(f.ok("erlang", "md5", &[b]))
    );
}

#[test]
fn a_byte_aligned_off_heap_sub_window_reads_its_slice() {
    let mut f = Fixture::new();
    let bytes = subject();

    // A window that skips the first byte (`start = 8` bits): every reader must see
    // `bytes[1..]`, not the payload from index 0.
    let b = f.refc(&bytes, 8, (bytes.len() - 1) * 8);
    assert_eq!(bytes_of(b), bytes[1..].to_vec());
    let expected: Vec<i64> = bytes[1..].iter().map(|&x| i64::from(x)).collect();
    assert_eq!(ints_of(f.ok("erlang", "binary_to_list", &[b])), expected);
    assert_eq!(signed_value(f.ok("binary", "first", &[b])), 2);
    assert_eq!(signed_value(f.ok("binary", "last", &[b])), 96);
    let zero = f.ints(&[0]).remove(0);
    assert_eq!(signed_value(f.ok("binary", "at", &[b, zero])), 2);
    // `md5` of the window is `md5(<<2,…,96>>)` — an oracle-pinned golden
    // (`erlang:md5(<< <<(X rem 256)>> || X <- lists:seq(2,96) >>)`).
    assert_eq!(
        bytes_of(f.ok("erlang", "md5", &[b])),
        vec![38, 37, 199, 69, 31, 159, 72, 36, 141, 207, 192, 35, 86, 73, 139, 0]
    );
    // Oracle: `erlang:crc32(<<2,…,96>>)` = 906204577, `erlang:adler32(…)` =
    // 1379734064.
    assert_eq!(signed_value(f.ok("erlang", "crc32", &[b])), 906204577);
    assert_eq!(signed_value(f.ok("erlang", "adler32", &[b])), 1379734064);

    // A window that skips a whole word (`start = 64` bits, 8 bytes).
    let inner = f.refc(&bytes, 64, 8 * 8);
    assert_eq!(bytes_of(inner), bytes[8..16].to_vec());
    assert_eq!(signed_value(f.ok("binary", "first", &[inner])), 9);
    assert_eq!(signed_value(f.ok("binary", "last", &[inner])), 16);
}

#[test]
fn a_zero_length_off_heap_window_is_the_empty_binary() {
    let mut f = Fixture::new();
    // `bs_init_writable` produces exactly this: a 0-bit window over a payload.
    let b = f.refc(&subject(), 0, 0);
    assert_eq!(bytes_of(b), Vec::<u8>::new());
    assert_eq!(ints_of(f.ok("erlang", "binary_to_list", &[b])), Vec::<i64>::new());
    assert_eq!(
        signed_value(f.ok("binary", "decode_unsigned", &[b])),
        0,
        "decode_unsigned(<<>>) is 0"
    );
    let zero = f.ints(&[0]).remove(0);
    assert!(matches!(f.call("binary", "at", &[b, zero]), Err(BeamError::BadArg)));
}
