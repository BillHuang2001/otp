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

//! The **`prim_buffer` host suite** (contract A.7, the `prim_*` family): the mutable byte queue
//! ERTS serves from a **statically linked** NIF (`erts/emulator/nifs/common/prim_buffer_nif.c`),
//! and the `prim_file` read/write path that is built on it.
//!
//! # What this suite pins
//!
//! | Surface | Shape |
//! |---|---|
//! | `prim_buffer:new/0` | a reference handle (never an immediate), registered in the runtime's one id space |
//! | `size/1`, `write/2`, `read/2`, `read_iovec/2`, `skip/2`, `wipe/1`, `find_byte_index/2` | the queue's answers, and the **`badarg` raise** for every shape the C's `enif_*` inspection rejects |
//! | `try_lock/1`, `unlock/1` | the compare-and-swap pair: `acquired`/`busy`, `ok`, and `error:lock_order_violation` |
//! | the write accumulator | chunk boundaries are observable through `read_iovec/2`'s list shape: two small writes coalesce, a ≥1 KiB write keeps its own element |
//! | `file:open/2` + `write`/`sync`/`position`/`read`/`eof`/`close` | the round trip through `prim_file`, whose read-shaped descriptors carry the queue |
//! | `file:pwrite/2,3` | a positioned write, including an **improper iolist tail** (`[<<"ab">> | <<"cd">>]`) |
//! | `file:read_line/1` | the read-ahead queue: line values **including** the `\n`, `\r\n` translated to `\n`, the `eof` tail, and the queue-invisible `position(F, cur)` |
//!
//! # Two drivers
//!
//! * A fixture module compiled at test time by the system `erlc` ([`QUEUE_FIXTURE`],
//!   [`FILE_FIXTURE`]) run under **both** runtimes, so every answer is compared to the live `erl`
//!   oracle **and** pinned literally — the pin is what makes the suite discriminate rather than
//!   merely agree, and it keeps the assertion meaningful if a future host's oracle moves.
//! * A real [`Runtime`] through its `ExecutionContext` seam (`call_bif`) for the assertions a
//!   `~p` rendering cannot make — that the handle is a reference term, and that the failing arms
//!   raise `badarg` / `lock_order_violation` rather than answering a value. That test needs no
//!   toolchain: the `prim_buffer` stubs are served by the native floor.
//!
//! # Which shapes are only reachable through `prim_file`
//!
//! `prim_buffer.erl` exports nine functions, and every one of them is called here. Two of the nine
//! `-nifs` stubs are **not** exported from the module — `peek_head/1` and `copying_read/2` — so
//! they are reached only through the module's own Erlang bodies (`read/2`, `read_iovec/2`) or,
//! from outside, only through `prim_file`'s read-ahead queue (`prim_file:read_line/1`,
//! `prim_file:position/2`). That is why the `read_iovec/2` shapes below are the way the
//! accumulator's chunk boundaries are observed: `peek_head/1` is the only entry that reports them,
//! and it is reached through `read_iovec/2`'s `Size > 64` arm.
//!
//! # Toolchain
//!
//! The fixture-driven tests need the system `erlc` (a compiled fixture, never a committed `.beam`)
//! and, for the comparison itself, the system `erl`; both are a **loud** skip when absent
//! ([`support::require_oracle`]), never a silent pass. The in-process test
//! ([`new_mints_a_reference_handle_that_the_other_stubs_accept`]) runs everywhere.

mod support;

use std::fs;
use std::path::Path;

use beam_bifs::NullSink;
use beam_common::BeamError;
use beam_engine::ExecutionContext;
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{is_atom, is_small, make_atom, signed_value, Eterm};

/// The suite's name in the loud-skip banner.
const SUITE: &str = "prim_buffer";

// ---------------------------------------------------------------------------------------
// The fixtures
// ---------------------------------------------------------------------------------------

/// The **queue surface** fixture: one function per question, so a failure names its own subject.
///
/// Every entry point is wrapped in `pack/1`, which turns a raise into `{Class, Reason}` — the
/// shape both runtimes then render identically (`~p` of a caught `badarg` is `{error,badarg}`).
/// The one thing never rendered is the handle itself: a `#Ref<…>` carries process-local id words,
/// so a fixture that printed one would be comparing two different numbers.
const QUEUE_FIXTURE: &str = r#"
-module(pbuf).
-export([handle/0, size/0, size_badarg/0, write_iovecs/0, zero_reads/0,
         read_past_end/0, read_iovec_past_end/0, skip/0, find_byte/0, lock/0,
         wipe/0, chunks/0, big_reads/0]).

pack(F) ->
    try F() of
        Value -> Value
    catch
        Class:Reason -> {Class, Reason}
    end.

handle() ->
    B = prim_buffer:new(),
    {is_reference(B), is_integer(B), is_atom(B), prim_buffer:size(B)}.

size() ->
    B = prim_buffer:new(),
    Empty = prim_buffer:size(B),
    ok = prim_buffer:write(B, [<<"abc">>, <<"def">>]),
    {Empty, prim_buffer:size(B)}.

size_badarg() ->
    pack(fun() -> prim_buffer:size(foo) end).

write_iovecs() ->
    B = prim_buffer:new(),
    [pack(fun() -> prim_buffer:write(B, IOVec) end)
     || IOVec <- [[1, 2, 3], [<<"a">>, [<<"b">>]], [1 | 2], foo, [], [<<>>],
                  [<<"a">>, <<"b">>]]].

zero_reads() ->
    B = prim_buffer:new(),
    {prim_buffer:read(B, 0), prim_buffer:read_iovec(B, 0), prim_buffer:size(B)}.

read_past_end() ->
    B = prim_buffer:new(),
    ok = prim_buffer:write(B, [<<"abc">>]),
    {pack(fun() -> prim_buffer:read(B, 5) end), prim_buffer:size(B),
     pack(fun() -> prim_buffer:read_iovec(B, 5) end), prim_buffer:size(B)}.

read_iovec_past_end() ->
    B = prim_buffer:new(),
    ok = prim_buffer:write(B, [<<"abc">>]),
    R = pack(fun() -> prim_buffer:read_iovec(B, 100) end),
    {R, prim_buffer:size(B)}.

skip() ->
    B = prim_buffer:new(),
    ok = prim_buffer:write(B, [<<"abcdef">>]),
    S0 = prim_buffer:skip(B, 0),
    Sz0 = prim_buffer:size(B),
    S6 = prim_buffer:skip(B, 6),
    Sz6 = prim_buffer:size(B),
    S1 = pack(fun() -> prim_buffer:skip(B, 1) end),
    {S0, Sz0, S6, Sz6, S1, prim_buffer:size(B)}.

find_byte() ->
    B = prim_buffer:new(),
    ok = prim_buffer:write(B, [<<"ab">>, <<"c\nd">>]),
    {prim_buffer:find_byte_index(B, $\n),
     prim_buffer:find_byte_index(B, $q),
     pack(fun() -> prim_buffer:find_byte_index(B, -1) end),
     pack(fun() -> prim_buffer:find_byte_index(B, 256) end),
     pack(fun() -> prim_buffer:find_byte_index(B, 1.5) end),
     prim_buffer:size(B)}.

lock() ->
    B = prim_buffer:new(),
    {prim_buffer:try_lock(B), prim_buffer:try_lock(B), prim_buffer:unlock(B),
     pack(fun() -> prim_buffer:unlock(B) end),
     pack(fun() -> prim_buffer:try_lock(foo) end),
     pack(fun() -> prim_buffer:unlock(foo) end)}.

wipe() ->
    B = prim_buffer:new(),
    ok = prim_buffer:write(B, [<<"abc">>, <<"de">>]),
    Before = prim_buffer:size(B),
    W = prim_buffer:wipe(B),
    {Before, W, prim_buffer:size(B)}.

chunks() ->
    Big = binary:copy(<<"x">>, 2048),
    B = prim_buffer:new(),
    ok = prim_buffer:write(B, [Big]),
    ok = prim_buffer:write(B, [<<"0123456789">>]),
    Large = prim_buffer:read_iovec(B, 2058),
    After = prim_buffer:size(B),
    S = prim_buffer:new(),
    ok = prim_buffer:write(S, [<<"ab">>]),
    ok = prim_buffer:write(S, [<<"cd">>]),
    Small = prim_buffer:read_iovec(S, 4),
    {[byte_size(X) || X <- Large], lists:last(Large), After,
     [byte_size(X) || X <- Small], Small}.

big_reads() ->
    B = prim_buffer:new(),
    ok = prim_buffer:write(B, [binary:copy(<<"y">>, 300)]),
    R = prim_buffer:read(B, 100),
    S = prim_buffer:size(B),
    {byte_size(R), S, byte_size(R) =:= 100}.
"#;

/// The **`prim_file`** fixture: the entries whose descriptors carry a `prim_buffer` queue.
///
/// `roundtrip/1` is the `[delayed_write]` open the queue exists for; `steps/2` applies a step
/// list (`sync`, `{pos, At}`, `{read, N}`, `{write, Data}`) in order, so one open covers the whole
/// `sync`/`position`/`read` matrix; `pwrite/2`, `pwrite3/3` and `write_improper/1` drive the
/// iodata writers (including an improper iolist tail); `readlines/1` is `read_line/1` with the
/// queue-invisible `position(F, cur)` interleaved.
const FILE_FIXTURE: &str = r#"
-module(pbfile).
-export([roundtrip/1, steps/2, pwrite/2, pwrite3/3, write_improper/1, readlines/1]).

roundtrip(Path) ->
    {ok, F} = file:open(Path, [raw, binary, read, write, delayed_write]),
    R1 = file:write(F, <<"hello">>),
    R2 = file:write(F, <<"world">>),
    P1 = file:position(F, 5),
    P2 = file:position(F, cur),
    S = file:sync(F),
    R3 = file:read(F, 5),
    R4 = file:read(F, 5),
    R5 = file:read(F, 1),
    C = file:close(F),
    {R1, R2, P1, P2, S, R3, R4, R5, C}.

steps(Path, Steps) ->
    {ok, F} = file:open(Path, [raw, binary, read, write]),
    Results = [step(F, S) || S <- Steps],
    ok = file:close(F),
    Results.

step(F, sync) -> file:sync(F);
step(F, {pos, At}) -> file:position(F, At);
step(F, {read, N}) -> file:read(F, N);
step(F, {write, Data}) -> file:write(F, Data).

pwrite(Path, LocBytes) ->
    {ok, F} = file:open(Path, [raw, binary, read, write]),
    R = file:pwrite(F, LocBytes),
    ok = file:close(F),
    R.

pwrite3(Path, Offset, IOData) ->
    {ok, F} = file:open(Path, [raw, binary, read, write]),
    R = file:pwrite(F, Offset, IOData),
    ok = file:close(F),
    R.

write_improper(Path) ->
    {ok, F} = file:open(Path, [raw, binary, write]),
    R = file:write(F, [<<"ab">> | <<"cd">>]),
    ok = file:close(F),
    R.

readlines(Path) ->
    {ok, F} = file:open(Path, [raw, binary, read]),
    L1 = file:read_line(F),
    P1 = file:position(F, cur),
    L2 = file:read_line(F),
    P2 = file:position(F, cur),
    L3 = file:read_line(F),
    P3 = file:position(F, cur),
    L4 = file:read_line(F),
    P4 = file:position(F, cur),
    L5 = file:read_line(F),
    ok = file:close(F),
    {L1, P1, L2, P2, L3, P3, L4, P4, L5}.
"#;

/// Compiles [`QUEUE_FIXTURE`], or `None` after the loud skip.
fn queue_fixture() -> Option<support::ErlangFixture> {
    support::ErlangFixture::compile(SUITE, "prim_buffer", "pbuf", QUEUE_FIXTURE)
}

/// Compiles [`FILE_FIXTURE`], or `None` after the loud skip.
fn file_fixture() -> Option<support::ErlangFixture> {
    support::ErlangFixture::compile(SUITE, "prim_buffer_file", "pbfile", FILE_FIXTURE)
}

// ---------------------------------------------------------------------------------------
// The two sides of one probe
// ---------------------------------------------------------------------------------------

/// `module:function(arg_list)` — the call both runtimes are asked for.
fn call_expr(module: &str, function: &str, args: &str) -> String {
    format!("{module}:{function}({args})")
}

/// One probe with no path argument: this runtime's answer must be the pinned `golden` **and** the
/// oracle's answer. The call names itself in a failure message.
fn probe(fixture: &support::ErlangFixture, call: &str, golden: &str) {
    let what = call;
    let mine = support::fixture_answer(what, fixture.dir(), call);
    assert_eq!(mine, golden, "`{call}`: this runtime vs the pinned answer");
    let Some(theirs) = support::fixture_oracle_answer(SUITE, fixture.dir(), call) else {
        return;
    };
    assert_eq!(mine, theirs, "`{call}`: this runtime vs the oracle");
}

/// The bytes `path` holds.
fn read_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// One fixture call that takes a **path**, run on both sides in files of their own so the two
/// sides' bytes can be compared as well as their answers.
///
/// `build` renders the full call from the path; `seed` pre-populates both files (byte-identical),
/// so a probe can start from content the fixture only reads.
fn file_probe(
    fixture: &support::ErlangFixture,
    label: &str,
    seed: Option<&[u8]>,
    build: &dyn Fn(&Path) -> String,
    golden: &str,
) {
    let ours_path = fixture.path(&format!("{label}_rust"));
    let oracle_path = fixture.path(&format!("{label}_oracle"));
    if let Some(bytes) = seed {
        fs::write(&ours_path, bytes)
            .unwrap_or_else(|error| panic!("cannot seed {}: {error}", ours_path.display()));
        fs::write(&oracle_path, bytes)
            .unwrap_or_else(|error| panic!("cannot seed {}: {error}", oracle_path.display()));
    }
    let ours_call = build(&ours_path);
    let mine = support::fixture_answer(label, fixture.dir(), &ours_call);
    assert_eq!(
        mine, golden,
        "`{ours_call}`: this runtime vs the pinned answer"
    );
    let Some(theirs) = support::fixture_oracle_answer(SUITE, fixture.dir(), &build(&oracle_path))
    else {
        return;
    };
    assert_eq!(mine, theirs, "`{label}`: this runtime vs the oracle");
    assert_eq!(
        read_bytes(&ours_path),
        read_bytes(&oracle_path),
        "`{label}`: the two sides must leave identical file bytes"
    );
}

// ---------------------------------------------------------------------------------------
// 1. The in-process route: the handle is a reference, the stubs raise
// ---------------------------------------------------------------------------------------

/// `module:function(args…)` through the runtime's `ExecutionContext` seam — the route a
/// registered-BIF or floor call takes from compiled code.
fn call(
    runtime: &mut Runtime,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> beam_common::Result<Eterm> {
    let mut sink = NullSink;
    let mut context = RuntimeContext::with_output(runtime, &mut sink);
    context.call_bif(module, function, args.len() as u32, args)
}

/// The atom `name`, interned in this runtime's shared table.
fn atom(runtime: &mut Runtime, name: &str) -> Eterm {
    make_atom(runtime.code_server_atoms_mut().intern(name))
}

/// `new/0` mints a **reference** handle that every other stub accepts, and the failing arms are
/// raises rather than values.
///
/// A `~p` probe can only show that `is_reference/1` said `true`; this asserts the word itself —
/// not an immediate, decodable as an internal reference (the runtime's own `enif_get_resource`
/// check, [`beam_process::monitor::MonitorRef::from_eterm`]) — and that a term which is not a live
/// handle is `badarg` rather than a fabricated answer. No toolchain needed: the `prim_buffer`
/// stubs are served from the native floor.
#[test]
fn new_mints_a_reference_handle_that_the_other_stubs_accept() {
    let mut runtime = Runtime::with_defaults().expect("the default runtime");

    let handle = call(&mut runtime, "prim_buffer", "new", &[]).expect("new/0 is served");
    assert!(!is_small(handle), "a handle is never an immediate");
    assert!(!is_atom(handle), "a handle is never an atom");
    assert!(
        beam_process::monitor::MonitorRef::from_eterm(handle).is_ok(),
        "a handle decodes as the internal reference `with_prim_buffer` looks up"
    );

    let size = call(&mut runtime, "prim_buffer", "size", &[handle]).expect("size/1 is served");
    assert_eq!(signed_value(size), 0, "a fresh queue is empty");

    let acquired = atom(&mut runtime, "acquired");
    let busy = atom(&mut runtime, "busy");
    let ok = atom(&mut runtime, "ok");
    assert_eq!(
        call(&mut runtime, "prim_buffer", "try_lock", &[handle]).expect("try_lock/1"),
        acquired
    );
    assert_eq!(
        call(&mut runtime, "prim_buffer", "try_lock", &[handle]).expect("a second try_lock/1"),
        busy,
        "the lock is exclusive"
    );
    assert_eq!(
        call(&mut runtime, "prim_buffer", "unlock", &[handle]).expect("unlock/1"),
        ok
    );
    let second_unlock = call(&mut runtime, "prim_buffer", "unlock", &[handle])
        .expect_err("an unlock with no lock held is the C's lock_order_violation");
    match second_unlock {
        BeamError::Raise(raise) => assert_eq!(
            raise.reason.as_atom(),
            Some("lock_order_violation"),
            "the reason is the C's atom, not a plain error"
        ),
        other => panic!("expected a raise, got {other:?}"),
    }

    assert_eq!(
        call(&mut runtime, "prim_buffer", "wipe", &[handle]).expect("wipe/1"),
        ok
    );
    assert_eq!(
        call(&mut runtime, "prim_buffer", "skip", &[handle, beam_term::make_small(0)])
            .expect("skip/2 of nothing"),
        ok
    );
    assert!(
        matches!(
            call(
                &mut runtime,
                "prim_buffer",
                "skip",
                &[handle, beam_term::make_small(5)]
            ),
            Err(BeamError::BadArg)
        ),
        "skipping past the end of an empty queue is badarg"
    );
    let bogus = atom(&mut runtime, "not_a_handle");
    assert!(
        matches!(
            call(&mut runtime, "prim_buffer", "size", &[bogus]),
            Err(BeamError::BadArg)
        ),
        "a term that is not a live handle is refused, never answered"
    );
}

// ---------------------------------------------------------------------------------------
// 2. The byte-queue surface, both runtimes, per function
// ---------------------------------------------------------------------------------------

/// The queue's answers and its `badarg` shapes, one probe per question.
///
/// The `write/2` row is the C's `enif_inspect_iovec` walk: **only** a proper list of byte-aligned
/// binaries is an iovec — an integer byte, a nested list, an improper list tail that is not a
/// binary, an atom, and the bare atom `foo` are all `badarg`, while `[]` and `[<<>>]` are the
/// empty iovec and answer `ok`.
#[test]
fn the_byte_queue_surface_agrees_with_the_oracle() {
    let Some(fixture) = queue_fixture() else {
        return;
    };
    // The handle: a reference, not an integer and not an atom, and an empty queue.
    probe(&fixture, &call_expr("pbuf", "handle", ""), "{true,false,false,0}");
    // `size/1` counts queued bytes across the two-part write.
    probe(&fixture, &call_expr("pbuf", "size", ""), "{0,6}");
    // A term that is not a live queue handle is the C's `enif_get_resource` failure.
    probe(&fixture, &call_expr("pbuf", "size_badarg", ""), "{error,badarg}");
    // The iovec walk: five refusals, then `[]`, `[<<>>]` and a two-element iovec.
    probe(
        &fixture,
        &call_expr("pbuf", "write_iovecs", ""),
        "[{error,badarg},{error,badarg},{error,badarg},{error,badarg},ok,ok,ok]",
    );
    // A zero-byte read is the empty binary (and, as an iovec, the list holding it).
    probe(&fixture, &call_expr("pbuf", "zero_reads", ""), "{<<>>,[<<>>],0}");
    // A read past the end is `badarg` and leaves the buffer untouched (both the small-read arm
    // and the iovec arm, which peek the head before deciding).
    probe(
        &fixture,
        &call_expr("pbuf", "read_past_end", ""),
        "{{error,badarg},3,{error,badarg},3}",
    );
    // ... except the `Size > 64` iovec arm, whose `read_iovec/2` loop skips the head it already
    // peeked before the short read fails — the oracle drains the whole queue to the same effect.
    probe(
        &fixture,
        &call_expr("pbuf", "read_iovec_past_end", ""),
        "{{error,badarg},0}",
    );
    // `skip/2` of nothing is `ok`; of the whole queue `ok`; past the end `badarg`.
    probe(
        &fixture,
        &call_expr("pbuf", "skip", ""),
        "{ok,6,ok,0,{error,badarg},0}",
    );
    // `find_byte_index/2` scans across the accumulator's chunks, answers `not_found` for a miss,
    // and refuses a needle outside `0..=255` (or a non-integer).
    probe(
        &fixture,
        &call_expr("pbuf", "find_byte", ""),
        "{{ok,3},not_found,{error,badarg},{error,badarg},{error,badarg},5}",
    );
    // `wipe/1` is `skip(Buffer, size(Buffer))`.
    probe(&fixture, &call_expr("pbuf", "wipe", ""), "{5,ok,0}");
    // The write accumulator's chunk boundaries: a 2048-byte write keeps its own queue element, so
    // the 10-byte write that follows is a **second** element (no coalescing across a large write),
    // while two 2-byte writes are combined into one.
    probe(
        &fixture,
        &call_expr("pbuf", "chunks", ""),
        "{[2048,10],<<\"0123456789\">>,0,[4],[<<\"abcd\">>]}",
    );
    // A read larger than the on-heap binary limit (64 bytes) still answers a binary of that size,
    // and leaves the remainder queued.
    probe(&fixture, &call_expr("pbuf", "big_reads", ""), "{100,200,true}");
}

/// `try_lock/1` / `unlock/1`: the compare-and-swap pair, and the two non-handle refusals.
#[test]
fn the_lock_is_a_compare_and_swap_pair() {
    let Some(fixture) = queue_fixture() else {
        return;
    };
    probe(
        &fixture,
        &call_expr("pbuf", "lock", ""),
        "{acquired,busy,ok,{error,lock_order_violation},{error,badarg},{error,badarg}}",
    );
}

// ---------------------------------------------------------------------------------------
// 3. `prim_file` over the queue: open, write, seek, sync, read, close
// ---------------------------------------------------------------------------------------

/// The `[delayed_write]` round trip: two writes, a seek, a `cur` re-read of the position, `sync`,
/// three reads (the last one past the end) and the close.
///
/// `delayed_write` is the reason `prim_buffer` exists in the first place — the kernel's
/// `raw_file_io_delayed` wrapper buffers small writes in the queue and flushes on `sync`.
#[test]
fn the_open_round_trip_agrees_with_the_oracle_and_the_bytes() {
    let Some(fixture) = file_fixture() else {
        return;
    };
    file_probe(
        &fixture,
        "roundtrip",
        None,
        &|path| format!("pbfile:roundtrip({path:?})"),
        "{ok,ok,{ok,5},{ok,5},ok,{ok,<<\"world\">>},eof,eof,ok}",
    );
    // The bytes the two writes left — `hello` then `world`, in that order, with no padding.
    assert_eq!(
        read_bytes(&fixture.path("roundtrip_rust")),
        b"helloworld".to_vec(),
        "the round trip wrote the two payloads back to back"
    );
}

/// The `sync` / `position` / `read` step matrix on one freshly opened descriptor over a
/// ten-byte file.
///
/// Each row is one step list, so the matrix covers: `sync` alone, a `bof` seek alone, a read
/// alone, a seek-then-read, a sync-then-seek, a write-then-reread (which the queue's flush makes
/// visible immediately), and a mid-file seek whose `cur` answer accounts for the read.
#[test]
fn the_step_matrix_agrees_with_the_oracle() {
    let Some(fixture) = file_fixture() else {
        return;
    };
    let matrix: [(&str, &str); 7] = [
        ("[sync]", "[ok]"),
        ("[{pos,bof}]", "[{ok,0}]"),
        ("[{read,5}]", "[{ok,<<\"01234\">>}]"),
        ("[{pos,bof},{read,5}]", "[{ok,0},{ok,<<\"01234\">>}]"),
        ("[sync,{pos,bof}]", "[ok,{ok,0}]"),
        (
            "[{write,<<\"zz\">>},{pos,bof},{read,7}]",
            "[ok,{ok,0},{ok,<<\"zz23456\">>}]",
        ),
        (
            "[{pos,3},{read,4},{pos,cur}]",
            "[{ok,3},{ok,<<\"3456\">>},{ok,7}]",
        ),
    ];
    for (index, (steps, golden)) in matrix.iter().enumerate() {
        file_probe(
            &fixture,
            &format!("steps_{index}"),
            Some(b"0123456789"),
            &|path| format!("pbfile:steps({path:?}, {steps})"),
            golden,
        );
    }
}

/// A positioned write, including an **improper iolist tail**.
///
/// `iolist()` allows `[B1 | B2]` — a binary as the tail of a list — and both `file:write/2`
/// (which reaches `prim_file`'s own iodata walk) and `file:pwrite/2,3` (which reaches
/// `erlang:iolist_to_iovec/1`) accept it. This is the shape `dets`' file header relies on, so it
/// is pinned here rather than only through the `dets` suite.
///
/// The reader-facing rule is `[<<"ab">> | <<"cd">>]` == `"abcd"`; the queue is not involved in
/// either call beyond the wipe `prim_file:pwrite/3` performs on a read-shaped descriptor.
#[test]
fn a_pwrite_with_an_improper_iolist_tail() {
    let Some(fixture) = file_fixture() else {
        return;
    };
    // `file:pwrite/2` with a `{Offset, IOData}` list: the tail binary is spliced in.
    file_probe(
        &fixture,
        "pwrite_improper",
        Some(b"0123456789"),
        &|path| format!("pbfile:pwrite({path:?}, [{{6, [<<\"ef\">> | <<\"gh\">>]}}])"),
        "ok",
    );
    assert_eq!(
        read_bytes(&fixture.path("pwrite_improper_rust")),
        b"012345efgh".to_vec(),
        "the improper tail continued the list's bytes at the given offset"
    );
    // `file:pwrite/3` with an integer offset and the same shape.
    file_probe(
        &fixture,
        "pwrite_improper3",
        Some(b"0123456789"),
        &|path| format!("pbfile:pwrite3({path:?}, 2, [<<\"ab\">> | <<\"cd\">>])"),
        "ok",
    );
    assert_eq!(
        read_bytes(&fixture.path("pwrite_improper3_rust")),
        b"01abcd6789".to_vec(),
        "four bytes landed at offset 2"
    );
    // ... and `file:write/2`, the route `dets`' header takes.
    file_probe(
        &fixture,
        "write_improper",
        Some(b""),
        &|path| format!("pbfile:write_improper({path:?})"),
        "ok",
    );
    assert_eq!(
        read_bytes(&fixture.path("write_improper_rust")),
        b"abcd".to_vec(),
        "the whole improper iolist reached the file"
    );
}

// ---------------------------------------------------------------------------------------
// 4. `file:read_line/1` — the read-ahead queue
// ---------------------------------------------------------------------------------------

/// `file:read_line/1` over a four-line file — `alpha`, `beta`, an **empty** line, and a final
/// `gamma` with no terminator — with `position(F, cur)` between every read.
///
/// Two properties are the point of the read-ahead queue:
///
/// * the answer **includes** the terminator (`<<"alpha\n">>`), so the caller's byte count and the
///   line are the same thing; and
/// * `position(F, cur)` names the byte after the line just answered, even though the descriptor's
///   host cursor has run ahead by a whole `max(256, read_ahead)` chunk — the queue is invisible to
///   `position/2`, which wipes it.
#[test]
fn read_line_over_a_four_line_file() {
    let Some(fixture) = file_fixture() else {
        return;
    };
    file_probe(
        &fixture,
        "lines",
        Some(b"alpha\nbeta\n\ngamma"),
        &|path| format!("pbfile:readlines({path:?})"),
        "{{ok,<<\"alpha\\n\">>},{ok,6},{ok,<<\"beta\\n\">>},{ok,11},{ok,<<\"\\n\">>},{ok,12},{ok,<<\"gamma\">>},{ok,17},eof}",
    );
}

/// A `\r\n` terminator answers a bare `\n` (`read_line_1/4`'s last clause), and a drained
/// descriptor answers `eof` from then on while `position(F, cur)` keeps naming the end.
#[test]
fn read_line_translates_a_crlf_terminator() {
    let Some(fixture) = file_fixture() else {
        return;
    };
    file_probe(
        &fixture,
        "crlf",
        Some(b"a\r\nb\r\n"),
        &|path| format!("pbfile:readlines({path:?})"),
        "{{ok,<<\"a\\n\">>},{ok,3},{ok,<<\"b\\n\">>},{ok,6},eof,{ok,6},eof,{ok,6},eof}",
    );
}
