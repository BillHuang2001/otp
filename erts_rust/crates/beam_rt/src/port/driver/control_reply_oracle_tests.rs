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

//! The **`NULL`-reply `port_control/3` rows against the LIVE ORACLE**, end to end and with the
//! **same `.so` under both runtimes** (contract A.4).
//!
//! `write_port_control_result` (`../../erts/emulator/beam/io.c:4126-4175`) renders a driver's
//! control reply from **two** facts, tested in this order:
//!
//! 1. the driver's reply pointer — `if (!resp_bufp) return NIL;` (`io.c:4134`). A `NULL` reply is
//!    the bare empty list `[]` for **every** `PORT_CONTROL_FLAG_BINARY` setting;
//! 2. only then the port's `PORT_CONTROL_FLAG_BINARY` word — a real buffer becomes a binary
//!    (`io.c:4136-4162`, zero bytes ⇒ `<<>>`) or a list of byte integers (`io.c:4163-4166`, zero
//!    bytes ⇒ `[]`).
//!
//! Two of the four rows below are **zero bytes** and are told apart by nothing but that order, so
//! the bytes cannot prove the fix: a runtime that read the flag alone answers `<<>>` where the
//! oracle answers `[]`. This suite therefore drives the four rows through **both** runtimes and
//! compares the rendered results, and it does so through the Erlang surface of each —
//! `erlang:open_port({spawn_driver, …}, [])` plus `erlang:port_control/3` — never through a
//! shortcut that would skip the port layer.
//!
//! # The four rows (plus the two the fixture's numbering makes free)
//!
//! The verbs are [`super::control_tests`]'s fixture verbs, which are `beam_io`'s own numbering for
//! these exact shapes (`crates/beam_io/tests/fixtures/host_driver.c:826-844`) — no new numbering is
//! invented here:
//!
//! | verb | `PORT_CONTROL_FLAG_BINARY` | `*rbuf` | `cres` | the C's answer | cited |
//! |---|---|---|---|---|---|
//! | `2`  | (untouched, clear) | buffer `"CTL!"` | 4 | `[67,84,76,33]` | `io.c:4163-4166` |
//! | `28` | **set** | `NULL` | 0 | `[]`, **not** `<<>>` | `io.c:4134` before `:4136` |
//! | `26` | clear | `NULL` | 0 | `[]` | `io.c:4134` |
//! | `27` | clear | `NULL` | 5 | `[]` — the pointer is tested before any length use | `io.c:4134` |
//! | `29` | **set** | buffer kept | 0 | `<<>>` | `io.c:4136-4162` |
//! | `30` | clear | buffer kept | 0 | `[]` (`buf_to_intlist` of nothing) | `io.c:4163-4166` |
//!
//! Verb `2` is the liveness row: it proves the driver really ran (its reply is the constant
//! `"CTL!"`, which no default would produce) before the zero-byte rows are read.
//!
//! The flag is **per-port sticky state** (`set_port_control_flags`, `io.c:5318`), so each shape
//! verb states it explicitly rather than relying on the row before it — which is also why the
//! recorded line below is order-sensitive in the flag dimension and not only in the verb dimension.
//!
//! # What is compared
//!
//! [`runtime_line`] renders this runtime's whole session with the registered `~w` printer — the
//! oracle probe's own format control — and [`RECORDED_ORACLE_LINE`] is the line measured on
//! **OTP 29.1.1 / erts-17.1** from the same `.so`:
//!
//! ```text
//! [{2,[67,84,76,33]},{28,[]},{26,[]},{27,[]},{29,<<>>},{30,[]}]
//! ```
//!
//! The test asserts, in this order and with distinct messages: every single row's own rendering
//! (so a drift names the verb and the C line it violates), then that the **oracle** still answers
//! the recorded line, then that **this runtime** answers the oracle's line — "which side drifted"
//! is never left to be guessed from one failing comparison.
//!
//! # Why the fixture gets an address from this runtime and none from the oracle
//!
//! The `.so` is [`super::control_tests`]'s source, compiled once per runtime. A real driver calls
//! `set_port_control_flags` and the loader binds the import out of the emulator's dynamic symbol
//! table, which is what the probe under `erl` relies on (`nm -D beam.smp` shows
//! `T set_port_control_flags`). This crate's `--lib` unit-test binary exports **no** global dynamic
//! symbol at all, so the runtime route hands the entry point's address in the `start` command
//! instead — the convention `../port_attributes_tests.rs` uses for `set_busy_port`. Both routes
//! call the host's own implementation.
//!
//! # Skip policy
//!
//! Needs the host C compiler, the host `erl_driver.h` and a usable `erl`; without any of them the
//! suite prints a **loud** `SKIP:` banner and returns — never a silent pass.
//!
//! [`runtime_line`]: runtime_line

use std::path::Path;

use beam_bifs::NullSink;
use beam_engine::terms;
use beam_term::{make_small, Eterm, NIL};
use super::control_tests::{build_fixture, set_port_control_flags_address, skip, Fixture};
use crate::context::RuntimeContext;
use crate::native::test_support::{atom, call, chars, runtime};
use crate::port::tests::erl_eval;

/// The whole-session rendering measured on the oracle (OTP 29.1.1 / erts-17.1) for
/// [`ROWS`], from the very `.so` this suite's fixture compiles:
/// `erl_ddll:load_driver(Dir, Name)` → `open_port({spawn_driver, Name}, [])` → one
/// `erlang:port_control(P, Verb, <<>>)` per verb, printed with `io:format("~w~n", [R])`.
const RECORDED_ORACLE_LINE: &str = "[{2,[67,84,76,33]},{28,[]},{26,[]},{27,[]},{29,<<>>},{30,[]}]";

/// One row of the session: the fixture verb, what it does to the port, and the rendering the C's
/// `write_port_control_result` must produce for it.
struct Row {
    /// The verb, i.e. the fixture's `control` command (`beam_io`'s numbering).
    verb: u32,
    /// What the row is about, for the assertion messages.
    what: &'static str,
    /// `PORT_CONTROL_FLAG_BINARY` as the verb leaves it (`io.c:5318`, per-port state).
    flag: bool,
    /// `true` when the verb leaves `*rbuf` `NULL` — the fact `io.c:4134` tests **first**.
    null_pointer: bool,
    /// The reply's own `~w` rendering.
    rendered: &'static str,
    /// The C lines this row pins, quoted in every assertion about it.
    cites: &'static str,
}

/// The session's rows, in drive order — verb `2` first so the driver is proven live before the
/// zero-byte rows are read.
const ROWS: [Row; 6] = [
    Row {
        verb: 2,
        what: "a live driver's own reply, flag clear",
        flag: false,
        null_pointer: false,
        rendered: "[67,84,76,33]",
        cites: "io.c:4163-4166 (a real buffer without PORT_CONTROL_FLAG_BINARY is buf_to_intlist)",
    },
    Row {
        verb: 28,
        what: "flag set + a NULL reply pointer",
        flag: true,
        null_pointer: true,
        rendered: "[]",
        cites: "io.c:4134 (the pointer test) precedes io.c:4136-4162 (the flag split), so this is \
                `[]` and never `<<>>`",
    },
    Row {
        verb: 26,
        what: "flag clear + a NULL reply pointer",
        flag: false,
        null_pointer: true,
        rendered: "[]",
        cites: "io.c:4134",
    },
    Row {
        verb: 27,
        what: "flag clear + a NULL reply pointer with a non-zero cres",
        flag: false,
        null_pointer: true,
        rendered: "[]",
        cites: "io.c:4134 (the pointer is tested before any use of the length, io.c:4137-4162)",
    },
    Row {
        verb: 29,
        what: "flag set + an empty reply buffer",
        flag: true,
        null_pointer: false,
        rendered: "<<>>",
        cites: "io.c:4136-4162",
    },
    Row {
        verb: 30,
        what: "flag clear + an empty reply buffer",
        flag: false,
        null_pointer: false,
        rendered: "[]",
        cites: "io.c:4163-4166 (buf_to_intlist of nothing)",
    },
];

/// The oracle probe: the recorded session, as one `erl -eval` expression over the same `.so`.
///
/// `erl_ddll:load_driver/2` really answers the bare atom `ok` (measured; a `{module, ok}` match
/// terminates the emulator), and the driver is then opened by **bare name** — so this side passes
/// no entry-point address and the fixture resolves `set_port_control_flags` out of the emulator's
/// own exported symbol, exactly as a shipped driver would.
fn oracle_expression(dir: &Path, name: &str) -> String {
    let verbs = ROWS
        .iter()
        .map(|row| row.verb.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "Dir = {dir:?}, Name = {name:?}, ok = erl_ddll:load_driver(Dir, Name), \
         P = open_port({{spawn_driver, Name}}, []), \
         R = [{{V, erlang:port_control(P, V, <<>>)}} || V <- [{verbs}]], \
         io:format(\"~w~n\", [R]), ok.",
        dir = dir.display().to_string(),
        name = name,
    )
}

/// One rendered `port_control/3` reply, `~w`-printed as the oracle prints it.
fn rendered(runtime: &mut crate::runtime::Runtime, reply: Eterm) -> String {
    runtime
        .format_term_w(reply, None)
        .unwrap_or_else(|error| panic!("the `~w` printer refused a control reply: {error}"))
        .trim_end_matches('\n')
        .to_string()
}

/// This runtime's whole session, through the **Erlang surface**: `erlang:open_port/2` with a
/// `{spawn_driver, Name}` name and one `erlang:port_control/3` per [`ROWS`] verb, rendered with the
/// registered `~w` printer.
///
/// The name carries the driver object path **and** the address of the host's
/// `set_port_control_flags` (`crate::port::driver_lookup` reads the name up to the first blank,
/// exactly as the C does, and `start` is handed the whole string — see the module docs).
fn runtime_line(fixture: &Fixture) -> String {
    let mut runtime = runtime();
    let name = format!(
        "{} {:x}",
        fixture.stem.with_extension("so").display(),
        set_port_control_flags_address()
    );
    // `{spawn_driver, Name}` — the name as an Erlang string, exactly as an Erlang caller writes it.
    let tag = atom(&mut runtime, "spawn_driver");
    let command = chars(&mut runtime, &name);
    let mut sink = NullSink;
    let name_term = terms::make_tuple(&mut RuntimeContext::with_output(&mut runtime, &mut sink), &[tag, command])
        .expect("a `{spawn_driver, Name}` tuple");
    let port = call(&mut runtime, "erlang", "open_port", &[name_term, NIL])
        .unwrap_or_else(|error| panic!("open_port({{spawn_driver,\"{name}\"}}) failed: {error}"));

    let mut pairs: Vec<Eterm> = Vec::new();
    for row in &ROWS {
        let reply = call(
            &mut runtime,
            "erlang",
            "port_control",
            &[port, make_small(i64::from(row.verb)), NIL],
        )
        .unwrap_or_else(|error| panic!("port_control(P, {}, <<>>) failed: {error}", row.verb));
        // Every row on its own, before the session: a drift then names the verb, the shape it
        // stands for and the C line it violates, instead of pointing at one long string.
        assert_eq!(
            rendered(&mut runtime, reply),
            row.rendered,
            "verb {} ({}): PORT_CONTROL_FLAG_BINARY {} / `*rbuf` {} — the reply pointer is tested \
             first ({}), so the reply renders as {}",
            row.verb,
            row.what,
            if row.flag { "set" } else { "clear" },
            if row.null_pointer {
                "NULL"
            } else {
                "a real buffer"
            },
            row.cites,
            row.rendered
        );        let mut sink = NullSink;
        let pair = terms::make_tuple(
            &mut RuntimeContext::with_output(&mut runtime, &mut sink),
            &[make_small(i64::from(row.verb)), reply],
        )
        .expect("a `{Verb, Reply}` tuple");
        pairs.push(pair);
    }
    let mut sink = NullSink;
    let session = terms::make_list(&mut RuntimeContext::with_output(&mut runtime, &mut sink), &pairs)
        .expect("the session list");
    rendered(&mut runtime, session)
}

/// The four `NULL`-vs-empty-buffer rows (and the two the fixture's numbering makes free) answer
/// **exactly** as the oracle does, in **both** directions of the comparison.
///
/// The two assertions a regression can fail are kept apart on purpose: the oracle line is checked
/// against the recorded constant first, so a failure there says the **oracle** side drifted (a moved
/// `erl`, a moved `erl_driver.h`, a fixture whose numbering changed), while the last assertion
/// compares this runtime's line with the oracle's live line and so can only fail on **our** side.
#[test]
fn the_null_and_empty_buffer_rows_match_the_live_oracle() {
    let Some(fixture) = build_fixture("rt_control_oracle_rows", true) else {
        return;
    };
    let observed = runtime_line(&fixture);
    match erl_eval(&oracle_expression(&fixture.dir, &fixture.name)) {
        Some(oracle) => {
            assert_eq!(
                oracle, RECORDED_ORACLE_LINE,
                "the ORACLE side drifted from the recorded session: `write_port_control_result` \
                 tests the reply pointer first (io.c:4134) and only then the flag \
                 (io.c:4136-4162 / io.c:4163-4166); the recorded line is the one this suite was \
                 written against"
            );
            assert_eq!(
                observed, oracle,
                "the RUST side drifted from the live oracle: a NULL reply pointer is `[]` for \
                 every PORT_CONTROL_FLAG_BINARY setting (io.c:4134, before the split at \
                 io.c:4136-4162), while an empty *buffer* under that flag is `<<>>` and without it \
                 `[]` (io.c:4163-4166)"
            );
        }
        // No usable `erl`: the per-row assertions above already ran, so the rows are pinned on this
        // runtime alone and the cross-check says so loudly.
        None => skip("the `NULL`-vs-empty-buffer control rows have no oracle to be checked against"),
    }
    fixture.cleanup();
}
