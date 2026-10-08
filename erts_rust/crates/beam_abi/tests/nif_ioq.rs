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

//! End-to-end proof of the **I/O queue / I/O vector** `enif_*` family (workspace contract A.3).
//!
//! `tests/fixtures/nif_ioq.c` is a real C NIF — compiled at test time with the system `cc` against
//! the system `erl_nif.h` — whose ten `enif_ioq_*`/`enif_inspect_iovec`/`enif_free_iovec` calls are
//! **undefined** in the `.so` and are bound at `dlopen(RTLD_NOW)` against this test binary's dynamic
//! symbol table (`build.rs` adds `-Wl,--export-dynamic`). A successful load therefore already proves
//! those `#[no_mangle]` definitions exist and are callable; the assertions below pin the *observable
//! behaviour* of each.
//!
//! # The oracle's contracts reproduced here (ground truth `erts/emulator/beam/erl_nif.c` +
//! `erl_io_queue.c`)
//!
//! * `enif_ioq_size` — the total queued byte count (`erl_nif.c:4384`).
//! * `enif_ioq_enq_binary` — append `bin->data[skip..size]`, then consume the binary
//!   (`erl_nif.c:4368`); `skip` past the end is the C's `0` (`erl_io_queue.c:227`).
//! * `enif_ioq_enqv` — append the vector's bytes after `skip`; `1` on success, `0` when `skip`
//!   exceeds the vector's declared size (`erl_nif.c:4359`).
//! * `enif_ioq_deq` — drop `count` bytes from the head and report the **remaining** count through
//!   `*size` (`erl_nif.c:4394`); a `count` beyond the queue is `0` with `*size` untouched.
//! * `enif_ioq_peek` — one `SysIOVec` per non-empty enqueued buffer (`erl_io_queue.c:259-280`),
//!   `NULL` + `*iovlen = 0` when empty, `NULL` + `*iovlen = -1` for a null queue
//!   (`erl_io_queue.c:401-403`).
//! * `enif_ioq_peek_head` — the head buffer as a binary plus its byte count; `0` and no writes on an
//!   empty queue (`erl_nif.c:4402`).
//! * `enif_inspect_iovec` — an iovec over a list's binaries, cut after `max_elements` list
//!   *elements* (`erl_nif.c:4161`), `*tail` receiving the remainder; with a non-null `env` the
//!   vector lives in the env's term heap and is marked preallocated, with a `NULL` env it owns its
//!   storage and `enif_free_iovec` releases it (`erl_nif.c:4097`).
//!
//! # Documented deviations exercised here
//!
//! The reference implementation documents two deliberate deviations that the null-handle test pins:
//! a null queue handle yields the C's failure code where the C would fault, and — because the
//! enqueue returns before its consume step — the caller's `ErlNifBinary` is left untouched by
//! `enif_ioq_enq_binary(NULL, bin, …)`.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system `erl_nif.h`; `compile_so` prints a loud
//! `SKIP:` banner and returns `None` when either is missing. When the toolchain **is** present every
//! assertion is hard.

mod common;

use std::path::{Path, PathBuf};

use beam_abi::nif::loader::load;
use beam_abi::NifEnv;
use beam_term::prelude::{
    bitstring_parts, car, cdr, is_list, is_nil, is_small, signed_value, tuple_element, tuple_size,
    Eterm,
};

use common::{compile_so, skip, TestHost};

/// The committed C fixture (see the file header for what it proves).
const FIXTURE_C: &str = include_str!("fixtures/nif_ioq.c");

/// The fixture's `ERL_NIF_INIT` name — the loader requires it to match the calling module.
const MODULE: &str = "test_nif_ioq";

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Compile the committed fixture; `None` (after a loud skip) when the toolchain is absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so(MODULE, FIXTURE_C)
}

/// `None` (after a loud skip) when the compiled `.so` path is not valid UTF-8.
fn so_path(so: &Path) -> Option<String> {
    match so.to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// A host plus the fixture loaded against it.
fn loaded(path: &str) -> (TestHost, beam_abi::nif::loader::NifLibrary) {
    let mut host = TestHost::new();
    let lib = load(path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");
    (host, lib)
}

/// The elements of the `n`-tuple `t` the fixture returned.
#[track_caller]
fn tuple_of(t: Eterm, n: usize) -> Vec<Eterm> {
    // SAFETY: `t` is a live tuple term produced by the fixture.
    let size = unsafe { tuple_size(t) };
    assert_eq!(size, n, "expected a {n}-tuple, got {t:?}");
    (0..n)
        // SAFETY: `i < size` for every `i` in the range.
        .map(|i| unsafe { tuple_element(t, i) })
        .collect()
}

/// The element as an `i64` small integer.
#[track_caller]
fn int_of(t: Eterm) -> i64 {
    assert!(is_small(t), "expected a small integer, got {t:?}");
    signed_value(t)
}

/// Every element of `elems` as an `i64` small integer.
#[track_caller]
fn ints(elems: &[Eterm]) -> Vec<i64> {
    elems.iter().copied().map(int_of).collect()
}

/// The byte payload of a binary term.
#[track_caller]
fn bytes_of(t: Eterm) -> Vec<u8> {
    // SAFETY: `t` is a live bitstring term produced by the fixture.
    let (base, offset, bits) = unsafe { bitstring_parts(t) };
    assert_eq!(offset % 8, 0, "a byte-aligned bitstring is expected: {t:?}");
    assert_eq!(bits % 8, 0, "a whole-byte binary is expected: {t:?}");
    if bits == 0 {
        return Vec::new();
    }
    // SAFETY: a live bitstring has `bits / 8` readable bytes at `base + offset / 8`.
    unsafe { std::slice::from_raw_parts(base.add(offset / 8), bits / 8) }.to_vec()
}

/// The binaries of a proper list of binaries.
#[track_caller]
fn binaries_of_list(t: Eterm) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut cur = t;
    while !is_nil(cur) {
        assert!(is_list(cur), "expected a proper list, got {cur:?}");
        // SAFETY: `cur` is a live cons cell.
        let (head, tail) = unsafe { (car(cur), cdr(cur)) };
        out.push(bytes_of(head));
        cur = tail;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// An empty queue
// ---------------------------------------------------------------------------------------------

#[test]
fn a_fresh_queue_is_empty_through_every_accessor() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let mut env = NifEnv::new(&mut host);

    let got = lib.call(&mut env, "empty", 0, &[]).expect("empty/0");
    let elems = tuple_of(got, 6);
    // {Size, IovLen, PeekIsNull, PeekHeadRet, HeadOutParamUntouched, HeadSizeOutParamUntouched}
    assert_eq!(
        ints(&elems),
        vec![0, 0, 1, 0, 1, 1],
        "a fresh queue is size 0, peeks NULL/0 and has no head"
    );
}

// ---------------------------------------------------------------------------------------------
// Null queue handles — the documented refusals
// ---------------------------------------------------------------------------------------------

#[test]
fn a_null_queue_handle_yields_the_cs_failure_codes_instead_of_faulting() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let mut env = NifEnv::new(&mut host);

    let got = lib.call(&mut env, "null_handles", 0, &[]).expect("null_handles/0");
    let elems = tuple_of(got, 8);
    // {Size, PeekIsNull, PeekIovLen, PeekHeadRet, DeqRet, DeqSizeUntouched, EnqRet, BinSize}
    assert_eq!(
        ints(&elems),
        vec![0, 1, -1, 0, 0, 1, 0, 3],
        "`enif_ioq_peek(NULL)` is the C's -1, and every other slot returns its failure code"
    );
    // The enqueue's early return happens before the consume-the-binary step, so the caller's
    // `ErlNifBinary` is still intact (the C reaches `enif_release_binary` and zeroes it).
    assert_eq!(int_of(elems[7]), 3, "an enqueue on a null queue leaves the binary untouched");
}

// ---------------------------------------------------------------------------------------------
// enif_ioq_enq_binary + skip
// ---------------------------------------------------------------------------------------------

#[test]
fn enq_binary_queues_the_bytes_after_the_skip_and_reports_them() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let mut env = NifEnv::new(&mut host);

    // (skip, ret, size, iovlen, bytes)
    let cases: [(i64, i64, i64, i64, &[u8]); 4] = [
        (0, 1, 6, 1, b"abcdef"),
        (2, 1, 4, 1, b"cdef"),
        // `skip == size` succeeds having queued nothing (`erl_io_queue.c:227`).
        (6, 1, 0, 0, b""),
        // Past the end is the C's `0`.
        (7, 0, 0, 0, b""),
    ];
    for (skip, ret, size, iovlen, want) in cases {
        let got = lib
            .call(&mut env, "enq_binary", 1, &[beam_term::prelude::make_small(skip)])
            .expect("enq_binary/1");
        let elems = tuple_of(got, 4);
        assert_eq!(int_of(elems[0]), ret, "enq_binary(skip={skip}) return");
        assert_eq!(int_of(elems[1]), size, "enq_binary(skip={skip}) queue size");
        assert_eq!(int_of(elems[2]), iovlen, "enq_binary(skip={skip}) vector count");
        assert_eq!(bytes_of(elems[3]), want, "enq_binary(skip={skip}) queued bytes");
    }
}

// ---------------------------------------------------------------------------------------------
// enif_ioq_enqv + skip
// ---------------------------------------------------------------------------------------------

#[test]
fn enqv_queues_the_vectors_bytes_after_the_skip() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let mut env = NifEnv::new(&mut host);

    // The fixture's vector is `enif_inspect_iovec` over ["ab","cde"] (5 bytes, 2 vectors).
    // (skip, ret, size, iovlen, bytes)
    let cases: [(i64, i64, i64, i64, &[u8]); 4] = [
        (0, 1, 5, 2, b"abcde"),
        // The skip walks the vector entry by entry: 2 whole vectors are dropped and the third
        // byte of the second entry is cut, leaving "de" as one vector.
        (3, 1, 2, 1, b"de"),
        (5, 1, 0, 0, b""),
        (6, 0, 0, 0, b""),
    ];
    for (skip, ret, size, iovlen, want) in cases {
        let got = lib
            .call(&mut env, "enqv", 1, &[beam_term::prelude::make_small(skip)])
            .expect("enqv/1");
        let elems = tuple_of(got, 4);
        assert_eq!(int_of(elems[0]), ret, "enqv(skip={skip}) return");
        assert_eq!(int_of(elems[1]), size, "enqv(skip={skip}) queue size");
        assert_eq!(int_of(elems[2]), iovlen, "enqv(skip={skip}) vector count");
        assert_eq!(bytes_of(elems[3]), want, "enqv(skip={skip}) queued bytes");
    }
}

// ---------------------------------------------------------------------------------------------
// The peek view: one vector per enqueued buffer, head bytes
// ---------------------------------------------------------------------------------------------

#[test]
fn the_peek_view_has_one_vector_per_enqueued_buffer_and_a_head_binary() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let mut env = NifEnv::new(&mut host);

    // "abc" via enq_binary, ["de","f"] via enqv, "ghij" via enq_binary => 10 bytes in 4 buffers.
    let got = lib.call(&mut env, "mixed", 0, &[]).expect("mixed/0");
    let elems = tuple_of(got, 6);
    assert_eq!(int_of(elems[0]), 10, "queued byte count");
    assert_eq!(int_of(elems[1]), 4, "one SysIOVec per enqueued buffer");
    assert_eq!(
        binaries_of_list(elems[2]),
        vec![b"abc".to_vec(), b"de".to_vec(), b"f".to_vec(), b"ghij".to_vec()],
        "the vectors carry the enqueued bytes in FIFO order"
    );
    assert_eq!(int_of(elems[3]), 1, "enif_ioq_peek_head succeeded");
    assert_eq!(bytes_of(elems[4]), b"abc", "the head buffer is the first enqueue");
    assert_eq!(int_of(elems[5]), 3, "the head's byte count is reported");
}

// ---------------------------------------------------------------------------------------------
// enif_ioq_deq: FIFO order, partial dequeue by size, overshoot
// ---------------------------------------------------------------------------------------------

#[test]
fn deq_drops_from_the_head_in_fifo_order_including_a_partial_buffer() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let mut env = NifEnv::new(&mut host);

    // The fixture enqueues "abc" then "defgh" (8 bytes in 2 buffers) and removes `count`.
    // (count, ret, remaining, size, iovlen, bytes, head_ret, head)
    let cases: [(i64, i64, i64, i64, i64, &[u8], i64, Option<&[u8]>); 5] = [
        (0, 1, 8, 8, 2, b"abcdefgh", 1, Some(b"abc")),
        // Exactly the first buffer.
        (3, 1, 5, 5, 1, b"defgh", 1, Some(b"defgh")),
        // One byte into the second buffer — the partial-head dequeue (`erl_io_queue.c:364-367`).
        (4, 1, 4, 4, 1, b"efgh", 1, Some(b"efgh")),
        // Everything: the queue restarts empty.
        (8, 1, 0, 0, 0, b"", 0, None),
        // Overshoot: the C's `0`, with `*size` left at its sentinel.
        (9, 0, 4242, 8, 2, b"abcdefgh", 1, Some(b"abc")),
    ];
    for (count, ret, remaining, size, iovlen, bytes, head_ret, head) in cases {
        let got = lib
            .call(&mut env, "deq", 1, &[beam_term::prelude::make_small(count)])
            .expect("deq/1");
        let elems = tuple_of(got, 7);
        let ctx = format!("deq({count})");
        assert_eq!(int_of(elems[0]), ret, "{ctx} return");
        assert_eq!(int_of(elems[1]), remaining, "{ctx} remaining count");
        assert_eq!(int_of(elems[2]), size, "{ctx} queue size");
        assert_eq!(int_of(elems[3]), iovlen, "{ctx} vector count");
        assert_eq!(bytes_of(elems[4]), bytes, "{ctx} remaining bytes");
        assert_eq!(int_of(elems[5]), head_ret, "{ctx} peek_head return");
        match head {
            Some(want) => assert_eq!(bytes_of(elems[6]), want, "{ctx} head bytes"),
            None => assert_eq!(int_of(elems[6]), -1, "{ctx} head out-param untouched"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// enif_inspect_iovec + enif_free_iovec
// ---------------------------------------------------------------------------------------------

#[test]
fn inspect_iovec_cuts_the_list_after_max_elements_and_reports_the_tail() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let mut env = NifEnv::new(&mut host);

    // The fixture inspects the three-binary list ["aa","bbb","c"] (6 bytes, 3 elements).
    // (max_length, tail_len, iovcnt, vec_size, bytes)
    let cases: [(i64, i64, i64, i64, &[u8]); 3] = [
        (16, 0, 3, 6, b"aabbbc"),
        // `max_elements` limits list *elements* (`erl_nif.c:4161`): two of the three.
        (2, 1, 2, 5, b"aabbb"),
        (0, 2, 1, 2, b"aa"),
    ];
    for (max_len, tail_len, iovcnt, vec_size, want) in cases {
        let got = lib
            .call(&mut env, "inspect", 1, &[beam_term::prelude::make_small(max_len)])
            .expect("inspect/1");
        let elems = tuple_of(got, 5);
        let ctx = format!("inspect(max_length={max_len})");
        assert_eq!(int_of(elems[0]), 1, "{ctx} return");
        assert_eq!(int_of(elems[1]), tail_len, "{ctx} tail list length");
        assert_eq!(int_of(elems[2]), iovcnt, "{ctx} vector count");
        assert_eq!(int_of(elems[3]), vec_size, "{ctx} declared vector size");
        assert_eq!(bytes_of(elems[4]), want, "{ctx} bytes");
    }
}

#[test]
fn inspect_iovec_with_a_null_env_owns_its_storage_and_free_iovec_releases_it() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let mut env = NifEnv::new(&mut host);

    let got = lib
        .call(&mut env, "inspect_null_env", 0, &[])
        .expect("inspect_null_env/0");
    let elems = tuple_of(got, 4);
    assert_eq!(ints(&elems[..3]), vec![1, 0, 2], "return, tail length, vector count");
    assert_eq!(bytes_of(elems[3]), b"xyz");
    // `enif_free_iovec` ran inside the NIF on a `flags == 0` (malloc'd) vector; a double free or a
    // free of env-owned memory would have aborted the process before this point.
}

#[test]
fn inspect_iovec_uses_a_caller_provided_preallocated_struct() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };
    let (mut host, lib) = loaded(&path);
    let mut env = NifEnv::new(&mut host);

    let got = lib
        .call(&mut env, "inspect_provided", 0, &[])
        .expect("inspect_provided/0");
    let elems = tuple_of(got, 5);
    // {Ret, UsedCallerStruct, InlineIovUsed, IovCnt, Bytes} — an inline `small_iov` marks the
    // vector preallocated, so `enif_free_iovec` (called inside the NIF) left the caller's storage
    // alone instead of freeing stack memory.
    assert_eq!(ints(&elems[..4]), vec![1, 1, 1, 2]);
    assert_eq!(bytes_of(elems[4]), b"pqrst");
}
