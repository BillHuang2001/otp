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

//! **`dets` acceptance**: a real `dets` table — open, insert, `sync`, `lookup` (hit and miss),
//! `match_object`, `close` — has to reach the oracle's answers **and** leave a `.dets` file whose
//! bytes are the oracle's bytes.
//!
//! # Why `dets` is the acceptance test for the `prim_buffer` floor
//!
//! `dets` writes its file through `raw_file_io_delayed` (`lib/kernel/src/disk_log_1.erl`'s
//! `dlog_open` on `[raw, binary, delayed_write]`), which buffers small writes in the
//! `prim_buffer` queue this runtime now serves, and its file header is built with an **improper
//! iolist tail** (`{0, [FileHeader | <<0:...>>]}`) — the shape `prim_file`'s own iodata walk has
//! to accept. So one `dets` round trip exercises the whole chain at once: the queue's
//! write/flush semantics, the `prim_file` write path, `sync`, and the byte layout the file ends up
//! with.
//!
//! # What is compared
//!
//! Every answer is pinned literally **and** compared to the live `erl` oracle
//! ([`support::fixture_answer`] / [`support::fixture_oracle_answer`]), and the two scratch tables'
//! file bytes must be **equal** — the strongest observable a file-writing library has. The
//! cross-read leg then opens each side's file under the *other* runtime, so a file both sides
//! call identical is also proved readable by both.
//!
//! A missing `erl`/`erlc` is a **loud** skip, never a silent pass.

mod support;

use std::fs;
use std::path::Path;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "dets";

/// The fixture: one `set` table with five entries of mixed types (an atom-keyed integer, a binary,
/// a list, a 100-byte binary and a nested tuple), then `sync`, the two lookups, `match_object`,
/// the close, and a `read_back/1` leg that opens an existing table read-only.
///
/// The table **name** is fixed (`pbtab`): `dets` records it in the file header and refuses a file
/// whose header names another table, so both sides must use the same one.
const FIXTURE: &str = r#"
-module(dets_probe).
-export([go/1, read_back/1]).

go(Path) ->
    {ok, T} = dets:open_file(pbtab, [{file, Path}, {type, set}]),
    R1 = dets:insert(T, [{k1, 1}, {k2, <<"two">>}, {k3, [a, b, c]},
                         {k4, binary:copy(<<"z">>, 100)}, {k5, {nested, [1, 2.5]}}]),
    R2 = dets:sync(T),
    R3 = dets:lookup(T, k2),
    R4 = dets:lookup(T, nokey),
    R5 = dets:match_object(T, {k1, '_'}),
    Count = length(dets:match_object(T, {'_', '_'})),
    R6 = dets:close(T),
    {R1, R2, R3, R4, R5, Count, R6}.

read_back(Path) ->
    {ok, T} = dets:open_file(pbtab, [{file, Path}, {access, read}]),
    Lookup = dets:lookup(T, k2),
    Match = dets:match_object(T, {k1, '_'}),
    Size = dets:info(T, size),
    FileSize = dets:info(T, file_size),
    ok = dets:close(T),
    {Lookup, Match, Size, FileSize}.
"#;

/// Compiles [`FIXTURE`], or `None` after the loud skip.
fn fixture() -> Option<support::ErlangFixture> {
    support::ErlangFixture::compile(SUITE, "dets", "dets_probe", FIXTURE)
}

/// The `.dets` bytes `path` holds.
fn read_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// One fixture call that takes a **path**, run on both sides in files of their own; answers the
/// two sides' transcripts after asserting they are the pinned `golden` and equal to each other.
fn path_probe(
    fixture: &support::ErlangFixture,
    label: &str,
    build: &dyn Fn(&Path) -> String,
    golden: &str,
) -> (Vec<u8>, Vec<u8>) {
    let ours_path = fixture.path(&format!("{label}_rust.dets"));
    let oracle_path = fixture.path(&format!("{label}_oracle.dets"));
    let ours_call = build(&ours_path);
    let mine = support::fixture_answer(&ours_call, fixture.dir(), &ours_call);
    assert_eq!(
        mine, golden,
        "`{ours_call}`: this runtime vs the pinned answer"
    );
    let Some(theirs) = support::fixture_oracle_answer(
        SUITE,
        fixture.dir(),
        &build(&oracle_path),
    ) else {
        return (read_bytes(&ours_path), read_bytes(&ours_path));
    };
    assert_eq!(mine, theirs, "`{label}`: this runtime vs the oracle");
    (read_bytes(&ours_path), read_bytes(&oracle_path))
}

/// **The acceptance test**: a `dets` table's answers and its file, against the live oracle.
///
/// The pinned answer is `{ok, ok, [{k2,<<"two">>}], [], [{k1,1}], 5, ok}` — the insert and the sync
/// succeed, the hit answers its object, the miss answers `[]`, `match_object/2` selects exactly the
/// matching object, all five entries are countable, and the close succeeds.
#[test]
fn a_dets_table_answers_like_the_oracle_and_writes_the_same_bytes() {
    let Some(fixture) = fixture() else {
        return;
    };
    let (ours, theirs) = path_probe(
        &fixture,
        "table",
        &|path| format!("dets_probe:go({path:?})"),
        "{ok,ok,[{k2,<<\"two\">>}],[],[{k1,1}],5,ok}",
    );
    assert_eq!(
        ours, theirs,
        "the two tables must be byte-identical: {} vs {} bytes",
        ours.len(),
        theirs.len()
    );
    assert!(
        !ours.is_empty(),
        "a closed `dets` table is a non-empty file, not an empty one"
    );
}

/// **The cross-read leg**: each side's table, opened `{access, read}` by the **other** runtime.
///
/// This is what "identical bytes" buys: a `.dets` file this runtime wrote is a file the oracle can
/// read back (and the other way round), with the same objects, the same size field and the same
/// file size. The pinned answer is built from the file the probe actually wrote, because the file
/// size is the one number that follows the fixture's content.
#[test]
fn each_side_reads_the_other_side_s_table() {
    let Some(fixture) = fixture() else {
        return;
    };
    let (ours, theirs) = path_probe(
        &fixture,
        "crossread",
        &|path| format!("dets_probe:go({path:?})"),
        "{ok,ok,[{k2,<<\"two\">>}],[],[{k1,1}],5,ok}",
    );
    assert_eq!(
        ours, theirs,
        "the two tables differ, so a cross-read would prove nothing"
    );
    let mut golden = String::from(r#"{[{k2,<<"two">>}],[{k1,1}],5,"#);
    golden.push_str(&ours.len().to_string());
    golden.push('}');
    // This runtime reads the table the **oracle** wrote.
    let call = format!("dets_probe:read_back({:?})", fixture.path("crossread_oracle.dets"));
    assert_eq!(
        support::fixture_answer(&call, fixture.dir(), &call),
        golden,
        "this runtime reading the oracle's table"
    );
    // ... and the oracle reads the table **this runtime** wrote.
    let call = format!("dets_probe:read_back({:?})", fixture.path("crossread_rust.dets"));
    let Some(theirs) = support::fixture_oracle_answer(SUITE, fixture.dir(), &call) else {
        return;
    };
    assert_eq!(
        theirs, golden,
        "the oracle must read the table this runtime wrote"
    );
}
