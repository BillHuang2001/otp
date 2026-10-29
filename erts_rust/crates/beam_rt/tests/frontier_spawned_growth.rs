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
//! A **spawned** process that builds large, deeply nested BIF results — the growth ladder that
//! `binary_module_SUITE:error_info/1` drives through `error_info_lib`, and that the D1 defect
//! report used to reach the pooled BIF region's reuse hazard.
//!
//! # The shape
//!
//! One round of the ladder (`frontier_spawned_growth:ladder/1`) chains six registered BIF calls
//! whose transients are large and nested — `binary:copy/2` (16 KiB) → `binary.bin_to_list/1`
//! (16 384 cells) → `list_to_tuple/1` (16 384 words) → `io_lib:format/2` (~122 KB of nested
//! iolist) → `lists:flatten/1` (~244 KB) → `iolist_to_binary/1` (122 KB) — plus the
//! `erl_error:format_exception/4` leg that `error_info_lib`'s failure path actually calls. Every
//! round's `md5` chains the *previous* round's digest, so the value returned after [`ROUNDS`]
//! rounds depends on **every byte of every round**; a pooled region that were rewound early or
//! reused while a nested result still named it would change that digest.
//!
//! The child is `spawn_monitor`ed, so it takes a pooled region of its own and its term heap is a
//! *process* heap that can be collected while the parent waits — which is what makes the shape
//! different from the same ladder in the `-eval` process, and is the shape the report named.
//!
//! # Neither side can hang
//!
//! The parent's wait is bounded **in Erlang** (`after 5000 -> exit(Pid, kill), timeout`), so a
//! child that never answers is a *value* (`timeout`) rather than a parked process, and the harness
//! bound ([`support::PROBE_TIMEOUT`]) is only a backstop behind it. Both sides are therefore
//! bounded twice over, and the test asserts the answer is the `ok` shape before comparing.
//!
//! # Why this is not a load-sensitive flake
//!
//! Nothing here asserts a wall time, a reduction count or a ratio: the observable is a
//! **deterministic digest** over fixed inputs, and the two sides are compared byte for byte. The
//! ladder allocates the same words in the same order on every run.
//!
//! # The golden and the one field that is not pinned
//!
//! [`LADDER_PREFIX`] pins the ladder's own counts and [`LADDER_DIGEST`] its chained digest — the
//! invariants of the fixture. The element between them is `erlang:md5/1` of
//! `erl_error:format_exception/4`'s bytes, which is *stdlib text*: the host oracle renders it from
//! its own stdlib while this runtime renders it from the tree's, so it is compared with the live
//! oracle instead of being frozen into the suite (a host whose oracle words that message
//! differently must not turn this test red).
//!
//! # Budget
//!
//! One compiled fixture, two children: ~0.5 s on this runtime and ~0.7 s on the oracle in the dev
//! profile, measured on this host. A missing `erl`/`erlc` is a **loud skip**, never a pass.
#[allow(dead_code)]
mod support;

/// Rounds per child. Eight keeps the ladder's chained digest long enough that a single mishandled
/// round shows up, while staying inside the ~1 s budget the suite asks for.
const ROUNDS: u32 = 8;
/// The fixture's own module name.
const MODULE: &str = "frontier_spawned_growth";
/// The `{ok, …}` answer's leading counts: the 16 KiB binary, its 16 384 cells, the tuple of the
/// same size, and the two derived sizes — the ladder's deterministic shape.
const LADDER_PREFIX: &str = "{ok,{16384,16384,16384,121934,121934,";
/// The chained digest of everything the ladder itself built over [`ROUNDS`] rounds. This is the
/// byte-sensitive part of the assertion; the element before it is the stdlib-rendered
/// `erl_error:format_exception/4` text, compared against the live oracle instead.
const LADDER_DIGEST: &str = r#",<<"536F472C44ECEEA3234C3C24A61C88A2">>}}"#;
/// The `rounds/2` call the fixture's `run/0` makes — the one place [`ROUNDS`] appears in the
/// Erlang side, so the two cannot drift apart silently.
const ROUNDS_CALL: &str = "rounds(8, <<>>)";
/// The fixture: a spawned child runs the ladder [`ROUNDS`] times and hands the parent its counts.
///
/// The parent's `receive` is bounded by `after 5000` **in Erlang**, so a child that blocks (the
/// failure mode an unbounded probe would turn into a harness timeout) answers the atom `timeout`
/// instead — a value the assertions below reject with a named message.
const FIXTURE_SOURCE: &str = r#"
-module(frontier_spawned_growth).
-export([run/0]).

%% One spawned child, one bounded wait: no `receive` here can park the `-eval` process.
run() ->
    Parent = self(),
    {Pid, Ref} = spawn_monitor(fun() -> Parent ! {self(), rounds(8, <<>>)} end),
    receive
        {Pid, Counts} ->
            erlang:demonitor(Ref, [flush]),
            {ok, Counts};
        {'DOWN', Ref, process, Pid, Why} ->
            {died, Why}
    after 5000 ->
        exit(Pid, kill),
        timeout
    end.

rounds(N, Seed) ->
    {Counts, _Last} = lists:foldl(fun(_, {_C, Prev}) -> ladder(Prev) end,
                                  {none, Seed}, lists:seq(1, N)),
    Counts.

%% The growth ladder: large nested BIF transients, each round chained into the next digest.
ladder(Prev) ->
    B0 = binary:copy(<<"abcdefgh">>, 2048),
    L = binary:bin_to_list(B0),
    T = list_to_tuple(L),
    S = io_lib:format("~p", [T]),
    F = lists:flatten([S, S]),
    Io = iolist_to_binary(F),
    Ex = erl_error:format_exception(error, {badkey, lists:duplicate(32, k)},
                                    [{m, f, 1, []}], #{}),
    ExB = unicode:characters_to_binary(Ex),
    LadderDigest = erlang:md5(iolist_to_binary([Prev, Io])),
    Counts = {byte_size(B0), length(L), tuple_size(T), byte_size(Io), length(F),
              byte_size(ExB), erlang:md5(ExB), binary:encode_hex(LadderDigest)},
    {Counts, LadderDigest}.
"#;
/// A spawned child building the growth ladder agrees with the system Erlang oracle, byte for byte.
///
/// The child runs on this runtime's scheduler as an ordinary process, so its pooled BIF region is
/// taken, used and reclaimed across [`ROUNDS`] rounds while its own heap is collected; the digest
/// it returns is what makes any premature rewind or early reuse of that region visible.
#[test]
fn a_spawned_child_building_the_ladder_agrees_with_the_oracle() {
    const BANNER: &str = "frontier_spawned_growth";
    assert!(
        FIXTURE_SOURCE.contains(ROUNDS_CALL) && ROUNDS_CALL.contains(&ROUNDS.to_string()),
        "the fixture's round count and ROUNDS must agree ({ROUNDS_CALL} vs ROUNDS = {ROUNDS})"
    );
    let Some(fixture) = support::ErlangFixture::compile(BANNER, "fsg", MODULE, FIXTURE_SOURCE)
    else {
        return;
    };
    let ours = support::fixture_answer(BANNER, fixture.dir(), "frontier_spawned_growth:run()");
    assert!(
        ours.starts_with(LADDER_PREFIX),
        "the spawned child's ladder shape changed: {ours}"
    );
    assert!(
        ours.ends_with(LADDER_DIGEST),
        "the spawned child's chained ladder digest changed (a pooled-region reuse hazard reads as \
         a different digest here): {ours}"
    );
    let Some(theirs) =
        support::fixture_oracle_answer(BANNER, fixture.dir(), "frontier_spawned_growth:run()")
    else {
        return;
    };
    assert_eq!(
        ours, theirs,
        "the spawned child's answer differs from the oracle's"
    );
}
