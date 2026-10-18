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

//! Contract **A.4** — the **driver C ABI** end to end: a real C driver `.so` opened as a port.
//!
//! Every other A.4 group drives a *built-in* driver through `open_port/2` (`driver`,
//! `driver_ports`); the C driver ABI itself was only asserted **structurally** in
//! `tests/abi.rs`. This group closes that gap the same way [`super::nif_so`] closes it for
//! A.3: a real driver object — C source compiled in the case's own temp dir against the
//! host `erl_driver.h` ([`crate::module_build::build`]) — is loaded by **both** runtimes and
//! driven from Erlang, and only stdout + exit status are compared.
//!
//! # The one asymmetry: how the object is named
//!
//! The two runtimes reach the *same* `<dir>/host_driver.so` through different names, and the
//! module absorbs the difference so neither runtime's argv changes:
//!
//! * the **oracle** never treats a `{spawn_driver, Name}` name as a path — a slashed name is
//!   `error:badarg` — so the object must first be loaded with `erl_ddll:load_driver/2` and
//!   then opened by its **bare driver name**;
//! * the **Rust** runtime instead accepts the object **path** as the name (a documented
//!   Milestone-1 extension of `beam_rt::port::driver_lookup`). Its `erl_ddll` BIFs
//!   (`try_load/3`/`try_unload/2`/`loaded_drivers/0`/`info/2`, the `ddll` corpus group) landed, but
//!   the Erlang-level `erl_ddll:load_driver/2` it would need is still unusable: the wrapper calls
//!   `try_load/3` with a `{driver_options, …}` option the runtime's option parser rejects with
//!   `badarg` (measured), so the path form stays the Rust side's route.
//!
//! `open/2` below therefore tries the path form first and falls back to the
//! `erl_ddll` form, so the case reads identically on both sides. (Verified: the oracle
//! raises `badarg` for the path form and accepts the loaded bare name; the Rust runtime
//! raises `badarg` for the unloadable bare name and loads the path.)
//!
//! # The C sources (what each fixture is allowed to reference)
//!
//! Every library is `dlopen(RTLD_NOW)`ed by both runtimes, so it may reference only `driver_*`
//! slots the Rust runtime really exports. The original fixture uses `driver_alloc`/`driver_free`
//! and `driver_output` (all exported by the `beam_rt` binary — see its
//! `-Wl,--export-dynamic` link options) and never a typed gap documented in
//! `crates/beam_rt/src/port/driver.rs` (the remaining one is `driver_async` — there is no async
//! pool; the entry's `control` callback behind `port_control/3` **is** routed). The observable
//! surface is consequently the **port lifecycle**: load → `start` → `output` → `stop`.
//!
//! # The four driver-registry slots (their own fixtures)
//!
//! Three further fixtures pin the registry family — `add_driver_entry`, `remove_driver_entry`,
//! `driver_lock_driver` and `driver_create_port`, the four `ErlDrvApi` slots the runtime
//! resolves alongside the rest. Each carries `DRIVER_INIT(host_driver)`, and each case has its
//! **own** temp dir, so exactly one `.so` is loaded per case:
//!
//! * selector `7`, **`driver_c/registry_slots_resolve_in_a_loaded_driver`** — a fixture that
//!   *references all four* (it calls the three that take real operands and anchors
//!   `driver_create_port`'s address in a `volatile` data object) and whose `start` always
//!   succeeds. It **agrees** (`1`): before those slots existed the Rust `dlopen(RTLD_NOW)`
//!   failed, so the port never opened and `is_port/1` answered `0` — a real regression guard.
//!   `driver_create_port` is deliberately never **called**: the oracle blocks forever on a driver
//!   name it does not know (`io.c:786`, measured), so pinning a call would hang the gate.
//! * selector `8`, **`driver_c/driver_lock_driver_answers_the_ports_driver`** — a fixture whose
//!   `output` callback answers `driver_lock_driver(port)` as one byte (`'1'` for the C's `0`).
//!   The oracle answers `0` (the port's driver has a `dlopen` handle) and the port opens; the
//!   Rust runtime implements the `lock_driver` hook and answers `0` too, so both report `'1'` —
//!   the case **agrees**.
//! * selector `9`, **`driver_c/add_then_remove_driver_entry_reports_removed`** — a fixture whose
//!   `output` callback runs `add_driver_entry(&e)` then `remove_driver_entry(&e)` and answers the
//!   removal's value as one byte (`'1'` for the C's `1`). The oracle answers `1` (the entry is
//!   not a dynamic driver, so it may be removed) and the port opens; the Rust runtime implements
//!   the entry-list hooks and answers the same, so the case **agrees**.
//!
//! Selectors `8` and `9` carry the answer over the **proven `driver_output` channel** rather
//! than refusing the port: a driver that refuses its port is an *uncatchable* runtime error on
//! the Rust side (the CLI exits `1`), so a refused port could only ever be a mismatch. The byte
//! channel is observable on both runtimes (it is what selector `6` pins); a runtime that did not
//! implement the hooks would answer something other than `'1'`, so the module would raise the
//! documented `unsupported_*` gap (exit `3`, never a mismatch) and a runtime that does answers
//! `'1'` and the case `agree`s.
//!
//! (The family's `driver_create_port` observable has no case for the blocking reason above.)
//!
//! # The widened port-lifecycle observables
//!
//! Six further selectors (`10`-`15`) stay on the **echo fixture** ([`HOST_DRIVER_C`]) and widen
//! what the port itself reports, with every answer a yes/no question the module computes in
//! Erlang: two sequential `port_command/2` payloads each echoed back in order (`10`), the
//! `erlang:ports/0` membership that appears on open and disappears after `port_close/1` (`11`),
//! `port_info(P, connected)` naming the opening process (`12`), `port_command/3` with
//! `[nosuspend]` (`13`), a three-command sequence whose echoes come back in the same order
//! (`14`), and the fresh port's `{input, 0}`/`{output, 0}` counters (`15`). They need no new C
//! slot — the driver's `output` callback is the only observable channel — so they are pure
//! port-lifecycle coverage over the same `.so`.
//!
//! # The entry's `control` callback (selectors `20`-`25`, `30`)
//!
//! `erlang:port_control/3` reaches the driver entry's `control` slot, which no other corpus group
//! carries a fixture for. [`CONTROL_DRIVER_C`] answers one verb per reply shape: bytes copied into
//! the **reply buffer the emulator hands the callback** (a list of byte integers on return), a
//! `NULL` reply pointer (the empty list), a negative length (`badarg`), a reply that asks for
//! `PORT_CONTROL_FLAG_BINARY` (a binary), and a reply emitted **together with** a
//! `driver_output` (both reach the owner). Selector `25` pins the `UINT_MAX` bound on the verb
//! (`term_to_Uint`, `erl_bif_port.c:334`) and selector `30` the capability gate on the *other*
//! side: an entry with **no** `control` slot is `badarg` for every verb, before any callback
//! (`io.c:4035`) — pinned against the echo fixture, whose entry declares none.
//!
//! # The widened entry surface (selectors `26`-`29`)
//!
//! [`EXTRA_DRIVER_C`] carries the remaining entry/port-state surfaces over one `output` command
//! byte: `erl_drv_output_term`'s **`ERL_DRV_PID`** operand (the owner receives the connected pid
//! itself — pinned by **identity**, since a pid's term image is per-VM-start), the driver-entry
//! list's *"no such entry"* answer (`remove_driver_entry` of an entry that was never added is the
//! C's `0`), and the port's **busy** state (`set_busy_port` → `port_command/3` with `[nosuspend]`
//! answers `false`, and the port still closes cleanly). Selector `29` stays on the echo fixture
//! and pins the port's own byte counters across one round trip (`{input, 5}`/`{output, 5}`).
//!
//! # Measured divergences (shapes that must **not** become cases)
//!
//! Every shape below was measured on both runtimes while this group was widened; each is a gap,
//! never an agreement, so it lives here (and in `divergences.md`) rather than in the corpus.
//!
//! * **`driver_failure_eof` / `driver_failure_atom` / `driver_failure_posix` / `driver_failure` /
//!   `driver_exit`** — the port-closing family (`io.c:7193-7280`) is unimplemented in `beam_rt`:
//!   `DriverHost::failure`/`port_exit` keep the trait's `-1`, so the port never closes and the
//!   `{'EXIT', Port, Reason}` a trapping owner would see never arrives. Both the return code and
//!   the exit reason therefore diverge. Owner: `beam_rt` (`DriverHost::failure`/`port_exit`).
//! * **a control reply from the *request* buffer** (`*rbuf = buf`, verb `3` of the prototype) —
//!   the oracle frees the reply pointer in `write_port_control_result` (`driver_free(resp_bufp)`,
//!   `io.c:4170`), and that pointer is the caller's data buffer: the VM dies of SIGSEGV
//!   (measured, exit `139`). `beam_rt`'s `control_reply` would serve it. Not the oracle's bug to
//!   work around here — unpinnable, not a divergence.
//! * **a zero-length `driver_output(port, buf, 0)`** — the oracle delivers `{Port,{data,<<>>}}`;
//!   `beam_rt` delivers **no** message (its notification fold drops an empty payload). Owner:
//!   `beam_rt` (`PortRegistry::fold_notifications`).
//! * **`erl_drv_output_term` with an `ERL_DRV_PORT` operand** — the oracle delivers the port term;
//!   `beam_rt` delivers nothing (measured: the receiver times out). Owner: `beam_abi`
//!   (`decode_term_spec`'s port operand).
//! * **`driver_output2`'s header/body *term shape*** — with a header the oracle delivers
//!   `[97,98|<<99,100>>]` (an improper list, `deliver_read_message`, `io.c:3404-3435`), `beam_rt`
//!   the flattened `<<97,98,99,100>>`. The **bytes** agree (the sibling `driver_abi` case pins
//!   them through `iolist_to_binary/1`), the shape does not. Owner: `beam_rt`/`beam_io`.
//! * **a plain `port_command/2` on a busy port** — the oracle suspends the caller until the driver
//!   unmarks the port (a hang once the driver is waiting for that same caller), `beam_rt` reports
//!   the typed `unsupported` gap. Only the `[nosuspend]` answer is pinnable (selector `28`).
//!   Owner: `beam_rt`.
//! * **`driver_caller` after `port_connect/2`** — `beam_rt`'s `caller_pid` answers the port's
//!   *owner*, so "connected ≠ caller" has no observable that agrees; the connected/caller identity
//!   is pinned only for an owner-opened port (`driver_abi`, selector `3`). Owner: `beam_rt`.
//! * **`port_control/3` with non-iodata `Data`** — the oracle exits `1` with **no** stdout and
//!   `beam_rt` raises `badarg` (a stderr line, exit `1`); a module-run case needs a non-empty
//!   golden line, so the refusal is pinned only through the verb/port refusals above.
//! * **`driver_set_port_data`/`driver_get_port_data`** — not slots at all: `erl_driver.h` (OTP 30)
//!   and the frozen `ErlDrvApi` carry no such entry point.
//! * **`driver_async`/`driver_async_cancel`** — no async pool in `beam_rt` (`port/driver.rs`), so
//!   the entry point answers the C's failure code from every fixture; already documented by the
//!   sibling `driver_abi` group and left out here too.
//!
//! # Skip policy (contract A.4 needs host tools, like A.3)
//!
//! A C-compiled fixture needs a C compiler *and* the host `erl_driver.h`; when either is
//! missing the harness reports `Verdict::Skipped` with a loud reason
//! ([`crate::module_build::MISSING_CC_REASON`] / `MISSING_ERL_NIF_REASON`) — never a pass,
//! never a crash.
//!
//! # The echo round trip (the driver hands bytes *back* to Erlang)
//!
//! The strongest A.4 evidence is the driver's `driver_output` bytes arriving in the owner's
//! mailbox: `port_command(P, <<"hello">>)` → the driver's `output` callback →
//! `driver_output(P, "hello", 5)` → a `{P, {data, <<"hello">>}}` message. Selector `6` pins
//! exactly that round trip: it sends `<<"hello">>` and verifies the bytes it receives back
//! equal `<<"hello">>` (returning `1`), so the observable is a VM-stable integer line like
//! every other branch (the corpus requires it of a module-run case; the echoed *value* is
//! `<<"hello">>`). It became runnable once `beam_rt run` drove the full scheduler
//! (`Runtime::run_scheduled`): before that, `run` performed a single engine pass and never
//! ran the scheduler's port half, so the receiver blocked and the CLI exited `3` with
//! `beam_rt: unsupported: … blocked` (a capability gap, never a mismatch).

use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded Erlang half: one `classify/1` selector per A.4 port-lifecycle observable.
///
/// Every branch returns an integer, so the golden is a VM-stable integer line (the corpus
/// requires it of a module-run case). Nothing here prints a path: `port_info(P, name)` is
/// deliberately **not** rendered, because the name differs by construction between the two
/// runtimes (the oracle sees the bare driver name, the Rust runtime the object path).
const DRIVER_MODULE_SOURCE: &str = r#"-module(driver_c).
-export([classify/1]).

classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    case N of
        1 -> P = open(Dir, ""), yn(erlang:is_port(P));
        2 -> P = open(Dir, ""),
             case erlang:port_info(P, os_pid) of
                 {os_pid, undefined} -> 1;
                 _ -> 0
             end;
        3 -> P = open(Dir, ""), true = erlang:port_close(P),
             case erlang:port_info(P, name) of
                 undefined -> 1;
                 _ -> 0
             end;
        4 -> P = open(Dir, ""), yn(erlang:port_command(P, <<"hello">>));
        5 -> P = open(Dir, " upper"), yn(erlang:is_port(P));
        6 -> P = open(Dir, ""),
             true = erlang:port_command(P, <<"hello">>),
             receive {P, {data, Bytes}} -> yn(Bytes =:= <<"hello">>) end;
        %% `7` opens the port: the registry fixture references all four registry slots and its
        %% `start` always succeeds.
        7 -> P = open(Dir, ""), yn(erlang:is_port(P));
        %% `8` and `9` ask a fixture to answer a registry question and hand the answer back
        %% through the proven `driver_output` channel (a *refused* port would be an uncatchable
        %% runtime error here, not an Erlang exception, so the answer travels as bytes). `1`
        %% means the runtime answered the slot the way the oracle does; anything else is the
        %% documented capability gap.
        8 -> registry_answer(open(Dir, ""), <<"lock">>, unsupported_driver_lock_driver);
        9 -> registry_answer(open(Dir, ""), <<"entry">>, unsupported_add_remove_driver_entry);
        %% `10`-`15` widen the *port-lifecycle* observables over the same echo driver: the
        %% driver's `output` callback carries the `port_command/2` payload back, so every answer
        %% is a yes/no question about the port rather than about the ABI.
        10 -> P = open(Dir, ""),
              yn(echo(P, <<"ab">>) =:= <<"ab">> andalso echo(P, <<"cd">>) =:= <<"cd">>);
        11 -> P = open(Dir, ""),
              Before = lists:member(P, erlang:ports()),
              true = erlang:port_close(P),
              After = lists:member(P, erlang:ports()),
              yn(Before andalso not After);
        12 -> P = open(Dir, ""),
              yn(erlang:port_info(P, connected) =:= {connected, self()});
        13 -> P = open(Dir, ""),
              true = erlang:port_command(P, <<"sus">>, [nosuspend]),
              receive {P, {data, Nosuspend}} -> yn(Nosuspend =:= <<"sus">>)
              after 2000 -> 0 end;
        14 -> P = open(Dir, ""),
              yn([echo(P, <<Ch>>) || Ch <- "xyz"] =:= [<<"x">>, <<"y">>, <<"z">>]);
        15 -> P = open(Dir, ""),
              yn(erlang:port_info(P, input) =:= {input, 0}
                 andalso erlang:port_info(P, output) =:= {output, 0});
        %% `20`-`25` drive the driver entry's **`control` callback** behind `port_control/3` — the
        %% one A.4 slot no other corpus group carries a fixture for. Each answer is the reply the
        %% owner received (`port_control/3` renders the driver's bytes as a list of byte integers,
        %% or as a binary once the driver asked for `PORT_CONTROL_FLAG_BINARY`).
        20 -> P = open(Dir, ""),
              yn(erlang:port_control(P, 1, <<>>) =:= [97, 98, 99, 100, 101]);
        21 -> P = open(Dir, ""),
              yn(erlang:port_control(P, 2, <<>>) =:= []);
        22 -> P = open(Dir, ""),
              yn(raises_badarg(fun() -> erlang:port_control(P, 4, <<>>) end));
        23 -> P = open(Dir, ""),
              yn(erlang:port_control(P, 5, <<>>) =:= <<"xyz">>);
        24 -> P = open(Dir, ""),
              Reply = erlang:port_control(P, 10, <<>>),
              yn(Reply =:= [111, 107] andalso echoed(P) =:= <<"out">>);
        %% `25` is the `UINT_MAX` bound `term_to_Uint` puts on the verb: the call is `badarg`
        %% before any callback runs.
        25 -> P = open(Dir, ""),
              yn(raises_badarg(fun() -> erlang:port_control(P, 16#100000000, <<>>) end));
        %% `26`-`28` widen the *driver entry* surface the port-lifecycle fixture does not reach:
        %% a term delivered through `erl_drv_output_term`'s `ERL_DRV_PID` operand (the term itself
        %% is the message, so its per-VM-start image is never pinned — only its identity), the
        %% driver-entry list's "no such entry" answer, and the port's **busy** state.
        26 -> P = open(Dir, ""),
              true = erlang:port_command(P, <<"i">>),
              receive Term -> yn(Term =:= self()) after 2000 -> 0 end;
        27 -> registry_answer(open(Dir, ""), <<"r">>,
                              unsupported_remove_driver_entry_of_an_unlisted_entry);
        28 -> P = open(Dir, ""),
              true = erlang:port_command(P, <<"b">>),
              _ = echoed(P),
              Busy = erlang:port_command(P, <<"more">>, [nosuspend]),
              yn(Busy =:= false andalso erlang:port_close(P) =:= true);
        %% `29` pins the port's own byte counters across one command round trip: `output` counts
        %% what `port_command/2` handed the driver, `input` what the driver handed back.
        29 -> P = open(Dir, ""),
              yn(echo(P, <<"abcde">>) =:= <<"abcde">>
                 andalso erlang:port_info(P, input) =:= {input, 5}
                 andalso erlang:port_info(P, output) =:= {output, 5});
        %% `30` is the capability gate on the *other* side: a driver entry with **no** `control`
        %% slot is `badarg` for every verb, before any callback runs.
        30 -> P = open(Dir, ""),
              yn(raises_badarg(fun() -> erlang:port_control(P, 1, <<>>) end))
    end.

%% Send one payload through the driver and answer the bytes it echoed back (`timeout` when the
%% driver answered nothing within the module's own 2 s bound, so a case can never hang).
echo(P, Bin) ->
    true = erlang:port_command(P, Bin),
    echoed(P).
%% The next payload the driver answers with on its own (a callback that emits output without a
%% fresh `port_command/2`, e.g. from inside its `control` callback).
echoed(P) ->
    receive {P, {data, Bytes}} -> Bytes after 2000 -> timeout end.

%% Whether `F` raises the plain `badarg` the C's port-operand refusals answer with.
raises_badarg(F) ->
    try F() of
        _ -> false
    catch
        error:badarg -> true;
        _:_ -> false
    end.
%% Send the fixture's command and classify the one-byte answer it echoes back.
registry_answer(P, Command, Gap) ->
    true = erlang:port_command(P, Command),
    receive
        {P, {data, <<"1">>}} -> 1;
        {P, {data, _}} -> erlang:error(Gap)
    end.

yn(true) -> 1;
yn(_) -> 0.

%% Reach the driver the way each runtime can, without changing the argv:
%%   * the oracle refuses a path in `{spawn_driver, _}` (`badarg`) and needs the object
%%     pre-loaded by `erl_ddll:load_driver/2`, then opened by the bare driver name;
%%   * the Rust runtime takes the object path as the name (`erl_ddll:load_driver/2` is not
%%     yet usable there).
open(Dir, Suffix) ->
    case catch open_port({spawn_driver, Dir ++ "/host_driver" ++ Suffix}, [binary]) of
        P when is_port(P) -> P;
        _ -> open_loaded(Dir, Suffix)
    end.

open_loaded(Dir, Suffix) ->
    ok = erl_ddll:load_driver(Dir, "host_driver"),
    open_port({spawn_driver, "host_driver" ++ Suffix}, [binary]).
"#;

/// A minimal **real** driver: the port-lifecycle callbacks only.
///
/// `DRIVER_INIT(host_driver)` makes the entry's `driver_name` `"host_driver"`, which is the
/// name both the loader's entry validation and the oracle's `erl_ddll` lookup match the file
/// stem against.
const HOST_DRIVER_C: &str = r#"/* A real driver .so (contract A.4): start / output / stop, and nothing else.
 *
 * Every `driver_*` symbol here is exported by the loading `beam_rt` process
 * (`-Wl,--export-dynamic`), so `dlopen(RTLD_NOW)` resolves; the fixture deliberately avoids
 * slots that are documented typed gaps on the Rust side (`driver_async`, the entry's
 * `control` callback). */
#include <erl_driver.h>

typedef struct {
    ErlDrvPort port; /* `output` receives no port argument, so `start` stores the handle */
} difftest_state;

static ErlDrvData dt_start(ErlDrvPort port, char *command) {
    (void)command;
    difftest_state *st = (difftest_state *) driver_alloc(sizeof(difftest_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void dt_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

/* Echo whatever Erlang sent: `output` is the callback behind `port_command/2`. */
static void dt_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    difftest_state *st = (difftest_state *) drv_data;
    if (st == NULL)
        return;
    (void) driver_output(st->port, buf, len);
}

static ErlDrvEntry dt_entry = {
    .init = NULL,
    .start = dt_start,
    .stop = dt_stop,
    .output = dt_output,
    .driver_name = "host_driver",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(host_driver) {
    return &dt_entry;
}
"#;

/// The compiled driver every port-lifecycle case loads (written next to the `.beam` as
/// `<name>.so`).
const HOST_DRIVER: NifFixture = NifFixture::CompiledLibrary {
    name: "host_driver",
    c_source: HOST_DRIVER_C,
};

/// The **registry** fixture (selector `7`): references all four registry slots and always opens.
///
/// The three callable slots are *called* (with ignored results), and `driver_create_port`'s
/// address is anchored in a `volatile` data object — a reference strong enough that the loader's
/// `RTLD_NOW` must resolve it, without ever calling it (the oracle blocks forever on an unknown
/// driver name, `io.c:786`). `start` always succeeds, so both runtimes open the port.
const REGISTRY_DRIVER_C: &str = r#"/* A real driver .so (contract A.4) that references every registry slot and always opens. */
#include <erl_driver.h>

typedef struct {
    ErlDrvPort port;
} registry_state;

/* Forward declaration: `start` hands the entry's own address to the two entry-list slots. */
static ErlDrvEntry registry_entry;

/* Reference `driver_create_port`'s address in a `volatile` data object: the relocation keeps the
 * symbol alive for `dlopen(RTLD_NOW)`, while the slot is never *called* (the oracle blocks
 * forever on a driver name it does not know). A function address is never NULL — the read is
 * only what keeps the object from being optimised away. */
static ErlDrvPort (*volatile create_port_slot)(ErlDrvPort, ErlDrvTermData, char *, ErlDrvData)
    = driver_create_port;

static ErlDrvData registry_start(ErlDrvPort port, char *command) {
    registry_state *st;
    (void) command;
    /* The three callable registry slots, called for the reference; their results are ignored. */
    add_driver_entry(&registry_entry);
    (void) remove_driver_entry(&registry_entry);
    (void) driver_lock_driver(port);
    if (create_port_slot == NULL)
        return (ErlDrvData) -1;
    st = (registry_state *) driver_alloc(sizeof(registry_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void registry_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

static void registry_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    registry_state *st = (registry_state *) drv_data;
    if (st == NULL)
        return;
    (void) driver_output(st->port, buf, len);
}

static ErlDrvEntry registry_entry = {
    .init = NULL,
    .start = registry_start,
    .stop = registry_stop,
    .output = registry_output,
    .driver_name = "host_driver",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(host_driver) {
    return &registry_entry;
}
"#;

/// The **lock** fixture (selector `8`): reports whether its port's own driver is lockable.
///
/// The answer reaches Erlang as one `driver_output` byte (`'1'` when `driver_lock_driver(port)`
/// answers `0`), because a *refused* port would be an uncatchable runtime error rather than an
/// Erlang exception — so the observable travels through the proven `output` round trip instead.
const LOCK_DRIVER_C: &str = r#"/* A real driver .so (contract A.4) that reports whether its port's own driver is lockable. */
#include <erl_driver.h>

typedef struct {
    ErlDrvPort port;
} lock_state;

static ErlDrvData lock_start(ErlDrvPort port, char *command) {
    lock_state *st;
    (void) command;
    st = (lock_state *) driver_alloc(sizeof(lock_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void lock_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

/* Answer the registry question and hand it back as one byte: '1' when the port's *own* driver is
 * lockable — `driver_lock_driver` answers 0 for a driver with a `dlopen` handle and -1 otherwise
 * (the C's `prt->drv_ptr->handle == NULL`, `io.c:7337`) — and '0' otherwise. */
static void lock_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    lock_state *st = (lock_state *) drv_data;
    char answer;
    (void) buf;
    (void) len;
    if (st == NULL)
        return;
    answer = (driver_lock_driver(st->port) == 0) ? '1' : '0';
    (void) driver_output(st->port, &answer, 1);
}

static ErlDrvEntry lock_entry = {
    .init = NULL,
    .start = lock_start,
    .stop = lock_stop,
    .output = lock_output,
    .driver_name = "host_driver",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(host_driver) {
    return &lock_entry;
}
"#;

/// The **entry-list** fixture (selector `9`): reports whether `add_driver_entry` then
/// `remove_driver_entry` round-trips its own entry out of the driver list.
const ENTRY_DRIVER_C: &str = r#"/* A real driver .so (contract A.4) that round-trips its own entry through the driver list. */
#include <erl_driver.h>

typedef struct {
    ErlDrvPort port;
} entry_state;

/* Forward declaration: the answer adds this very entry to the list and removes it again. */
static ErlDrvEntry entry_entry;

static ErlDrvData entry_start(ErlDrvPort port, char *command) {
    entry_state *st;
    (void) command;
    st = (entry_state *) driver_alloc(sizeof(entry_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void entry_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

/* Add this very entry to the driver list (the slot returns void by contract), then remove it
 * again and hand the outcome back as one byte: '1' when the removal reported the entry removed
 * (the C's 1 for a non-dynamic entry), '0' otherwise. */
static void entry_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    entry_state *st = (entry_state *) drv_data;
    char answer;
    (void) buf;
    (void) len;
    if (st == NULL)
        return;
    add_driver_entry(&entry_entry);
    answer = (remove_driver_entry(&entry_entry) == 1) ? '1' : '0';
    (void) driver_output(st->port, &answer, 1);
}

static ErlDrvEntry entry_entry = {
    .init = NULL,
    .start = entry_start,
    .stop = entry_stop,
    .output = entry_output,
    .driver_name = "host_driver",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(host_driver) {
    return &entry_entry;
}
"#;

/// The **`control`** fixture (selectors `20`-`24`): the entry's `control` callback behind
/// `erlang:port_control/3`, one verb per reply shape.
///
/// `*rbuf`/`rlen` are the reply buffer and its capacity the emulator hands the callback
/// (`resp_buf`/`resp_size`, `io.c:4336-4354`); the return value is the reply length, or a
/// negative number for the C's `ERTS_PORT_OP_BADARG` (`io.c:4074`). The five verbs are the five
/// answer shapes that are reachable on both runtimes:
///
/// | verb | the callback does | `port_control/3` answers |
/// |---|---|---|
/// | `1` | copies `"abcde"` into `*rbuf`, returns `5` | `[97,98,99,100,101]` (`buf_to_intlist`) |
/// | `2` | sets `*rbuf = NULL`, returns `0` | `[]` (`write_port_control_result`'s `NIL`) |
/// | `4` | returns `-1` | `badarg`, before any reply is rendered |
/// | `5` | `set_port_control_flags(P, PORT_CONTROL_FLAG_BINARY)` + `"xyz"` | `<<"xyz">>` |
/// | `10` | replies `"ok"` **and** `driver_output`s `"out"` | `[111,107]` and one `{data, <<"out">>}` |
///
/// Verb `3` of the prototype was a reply from the **request** buffer (`*rbuf = buf`): the oracle
/// frees that pointer in `write_port_control_result` (`driver_free(resp_bufp)`, `io.c:4170`) and
/// the VM dies of SIGSEGV (measured, exit `139`), so no such case can exist — see *measured
/// divergences* in the module doc.
const CONTROL_DRIVER_C: &str = r#"/* A real driver .so (contract A.4): the entry's `control` callback, one verb per reply shape. */
#include <erl_driver.h>
#include <string.h>

typedef struct {
    ErlDrvPort port;
} control_state;

static ErlDrvData control_start(ErlDrvPort port, char *command) {
    control_state *st;
    (void) command;
    st = (control_state *) driver_alloc(sizeof(control_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void control_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

/* `port_control/3`'s callback: `buf`/`len` is the caller's data, `*rbuf`/`rlen` the reply buffer
 * and its capacity. A negative answer is the C's badarg, nothing is delivered. */
static ErlDrvSSizeT control_control(ErlDrvData drv_data, unsigned int command,
                                    char *buf, ErlDrvSizeT len,
                                    char **rbuf, ErlDrvSizeT rlen) {
    control_state *st = (control_state *) drv_data;
    (void) buf;
    (void) len;
    if (st == NULL)
        return -1;
    switch (command) {
    case 1:
        if (rlen < 5)
            return -1;
        memcpy(*rbuf, "abcde", 5);
        return 5;
    case 2:
        /* A `NULL` reply pointer is the C's "no reply at all" (`write_port_control_result`). */
        *rbuf = NULL;
        return 0;
    case 4:
        return -1;
    case 5:
        /* Ask for the binary rendering of *this* reply: the emulator reads the flag after the
         * callback returns (`io.c:4403-4411`), so the very reply asking for it is a binary. */
        set_port_control_flags(st->port, PORT_CONTROL_FLAG_BINARY);
        if (rlen < 3)
            return -1;
        memcpy(*rbuf, "xyz", 3);
        return 3;
    case 10:
        /* A `control` callback is an ordinary driver callback: what it emits reaches the owner
         * too, and the reply is returned to the caller. */
        if (rlen < 2)
            return -1;
        memcpy(*rbuf, "ok", 2);
        (void) driver_output(st->port, "out", 3);
        return 2;
    default:
        return -1;
    }
}

static ErlDrvEntry control_entry = {
    .init = NULL,
    .start = control_start,
    .stop = control_stop,
    .control = control_control,
    .driver_name = "host_driver",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(host_driver) {
    return &control_entry;
}
"#;

/// The **widened entry** fixture (selectors `26`-`28`): one `output` command per remaining
/// `ErlDrvEntry`/port-state surface the echo fixture does not carry.
///
/// * `"i"` — delivers `ERL_DRV_PID` over `driver_connected/1` through `erl_drv_output_term/3`,
///   so the owner receives the **pid itself** as the message (`io.c:6162-6176`: the C queues the
///   decoded term, never a `{Port, Term}` wrapper). Its image is per-VM-start, so the case pins
///   the term's *identity* (`=:= self()`), never its bytes.
/// * `"r"` — asks `remove_driver_entry/1` about an entry object that was never added to the
///   driver list and answers whether the C's `0` ("no such entry", `io.c:7733`) came back.
/// * `"b"` — marks the port **busy** (`set_busy_port`, the C's `ERTS_PTS_FLG_BUSY_PORT`) and
///   echoes one byte, so the module knows the state was applied before it probes
///   `port_command/3`'s `[nosuspend]` answer.
///
/// `driver_create_port`'s slot is anchored (never called) for the same reason as
/// [`REGISTRY_DRIVER_C`]: the `RTLD_NOW` reference keeps the symbol resolved, and calling it with
/// a driver name the emulator does not know blocks the oracle forever.
const EXTRA_DRIVER_C: &str = r#"/* A real driver .so (contract A.4): one command per remaining entry / port-state surface. */
#include <erl_driver.h>

typedef struct {
    ErlDrvPort port;
} extra_state;

/* An entry object that is never added to the driver list. */
static ErlDrvEntry unlisted_entry;

static ErlDrvData extra_start(ErlDrvPort port, char *command) {
    extra_state *st;
    (void) command;
    st = (extra_state *) driver_alloc(sizeof(extra_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void extra_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

static void extra_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    extra_state *st = (extra_state *) drv_data;
    ErlDrvTermData spec[2];
    char answer;
    if (st == NULL || len < 1)
        return;
    switch (buf[0]) {
    case 'i':
        /* The connected process, as a term: `ERL_DRV_PID` carries term data, not a spec. */
        spec[0] = ERL_DRV_PID;
        spec[1] = driver_connected(st->port);
        (void) erl_drv_output_term(driver_mk_port(st->port), spec, 2);
        break;
    case 'r':
        /* '1' when the answer is the C's 0: the entry is not in the driver list. */
        answer = (remove_driver_entry(&unlisted_entry) == 0) ? '1' : '0';
        (void) driver_output(st->port, &answer, 1);
        break;
    case 'b':
        set_busy_port(st->port, 1);
        answer = '1';
        (void) driver_output(st->port, &answer, 1);
        break;
    default:
        break;
    }
}

static ErlDrvEntry extra_entry = {
    .init = NULL,
    .start = extra_start,
    .stop = extra_stop,
    .output = extra_output,
    .driver_name = "host_driver",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(host_driver) {
    return &extra_entry;
}
"#;

/// The registry fixture every selector-`7` case loads.
const REGISTRY_DRIVER: NifFixture = NifFixture::CompiledLibrary {
    name: "host_driver",
    c_source: REGISTRY_DRIVER_C,
};

/// The lock fixture every selector-`8` case loads.
const LOCK_DRIVER: NifFixture = NifFixture::CompiledLibrary {
    name: "host_driver",
    c_source: LOCK_DRIVER_C,
};

/// The entry-list fixture every selector-`9` case loads.
const ENTRY_DRIVER: NifFixture = NifFixture::CompiledLibrary {
    name: "host_driver",
    c_source: ENTRY_DRIVER_C,
};

/// The `control`-callback fixture the `20`-`24` cases load.
const CONTROL_DRIVER: NifFixture = NifFixture::CompiledLibrary {
    name: "host_driver",
    c_source: CONTROL_DRIVER_C,
};

/// The widened-entry fixture the `26`-`28` cases load.
const EXTRA_DRIVER: NifFixture = NifFixture::CompiledLibrary {
    name: "host_driver",
    c_source: EXTRA_DRIVER_C,
};

/// Build one module-run case over [`DRIVER_MODULE_SOURCE`] with `fixture` as its `.so`.
fn case(
    id: &'static str,
    description: &'static str,
    selector: &'static [i64],
    fixture: NifFixture,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::DriverAbi, Contract::BeamAsm],
        kind: CaseKind::Module(ModuleRun {
            source: DRIVER_MODULE_SOURCE,
            module: "driver_c",
            function: "classify",
            arity: 1,
            args: selector,
            nif: Some(fixture),
        }),
        golden: Golden {
            stdout: Some("1"),
            exit_code: 0,
        },
    }
}

/// The A.4 end-to-end group: a real C driver loaded and driven through the port lifecycle and
/// the four driver-registry slots.
///
/// Every golden is `1` (the oracle's own answer, verified per case with
/// `differential run <id>`): each case is a yes/no question about the driver-backed port, so
/// a `0` means the load, the `start` callback or the port registration did not behave.
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(
            "driver_c/spawn_driver_opens_a_real_c_driver",
            "open_port({spawn_driver, ...}) loads a real C driver .so and yields a port",
            &[1],
            HOST_DRIVER,
        ),
        case(
            "driver_c/spawn_driver_port_has_no_os_process",
            "a C driver port reports no OS process: port_info(P, os_pid) is undefined",
            &[2],
            HOST_DRIVER,
        ),
        case(
            "driver_c/port_close_runs_the_driver_stop_callback",
            "port_close/1 stops the driver (its stop callback frees the state) and the port is gone",
            &[3],
            HOST_DRIVER,
        ),
        case(
            "driver_c/port_command_reaches_the_driver_output_callback",
            "port_command/2 hands the bytes to the driver's output callback and is accepted",
            &[4],
            HOST_DRIVER,
        ),
        case(
            "driver_c/spawn_driver_name_may_carry_a_command",
            "a {spawn_driver, Name} whose name carries a command string still opens the driver",
            &[5],
            HOST_DRIVER,
        ),
        case(
            "driver_c/port_command_echo_round_trip",
            "the driver's driver_output bytes arrive as {Port, {data, <<\"hello\">>}}: port_command round trip",
            &[6],
            HOST_DRIVER,
        ),
        case(
            "driver_c/registry_slots_resolve_in_a_loaded_driver",
            "a driver referencing all four registry slots loads and opens under both runtimes",
            &[7],
            REGISTRY_DRIVER,
        ),
        case(
            "driver_c/driver_lock_driver_answers_the_ports_driver",
            "driver_lock_driver(port) answers 0 for the port's own driver: the driver echoes 1 (else a capability gap)",
            &[8],
            LOCK_DRIVER,
        ),
        case(
            "driver_c/add_then_remove_driver_entry_reports_removed",
            "remove_driver_entry after add_driver_entry reports the entry removed: the driver echoes 1 (else a capability gap)",
            &[9],
            ENTRY_DRIVER,
        ),
        // --- the port-lifecycle observables widened over the same echo driver ----------------
        case(
            "driver_c/driver_output_delivers_two_sequential_commands_in_order",
            "two port_command/2 payloads are each echoed back by the driver, in order",
            &[10],
            HOST_DRIVER,
        ),
        case(
            "driver_c/ports_membership_follows_port_close",
            "erlang:ports/0 lists an open driver port and drops it once port_close/1 stops the driver",
            &[11],
            HOST_DRIVER,
        ),
        case(
            "driver_c/port_info_connected_is_the_opening_process",
            "port_info(P, connected) names the process that opened the driver port",
            &[12],
            HOST_DRIVER,
        ),
        case(
            "driver_c/port_command3_nosuspend_reaches_the_driver_output_callback",
            "port_command/3 with the nosuspend option hands the bytes to the driver's output callback",
            &[13],
            HOST_DRIVER,
        ),
        case(
            "driver_c/driver_output_preserves_a_command_sequence",
            "a sequence of port_command/2 payloads is echoed back as the same sequence",
            &[14],
            HOST_DRIVER,
        ),
        case(
            "driver_c/port_info2_input_and_output_start_at_zero",
            "a freshly opened driver port reports {input, 0} and {output, 0}",
            &[15],
            HOST_DRIVER,
        ),
        // --- the entry's `control` callback (`erlang:port_control/3`) -------------------------
        case(
            "driver_c/driver_control_replies_from_its_handed_buffer",
            "port_control/3 hands the driver a reply buffer and returns the bytes it wrote there as a list",
            &[20],
            CONTROL_DRIVER,
        ),
        case(
            "driver_c/driver_control_with_a_null_reply_is_the_empty_list",
            "a control callback that answers a NULL reply pointer makes port_control/3 return []",
            &[21],
            CONTROL_DRIVER,
        ),
        case(
            "driver_c/driver_control_negative_result_is_badarg",
            "a control callback that returns a negative length makes port_control/3 raise badarg",
            &[22],
            CONTROL_DRIVER,
        ),
        case(
            "driver_c/driver_control_binary_flag_renders_the_reply_as_a_binary",
            "set_port_control_flags(P, PORT_CONTROL_FLAG_BINARY) in the callback makes the reply a binary",
            &[23],
            CONTROL_DRIVER,
        ),
        case(
            "driver_c/driver_control_output_reaches_the_owner_alongside_the_reply",
            "a control callback's driver_output reaches the owner while port_control/3 still returns its reply",
            &[24],
            CONTROL_DRIVER,
        ),
        case(
            "driver_c/port_control_refuses_a_verb_beyond_uint_max",
            "port_control/3 raises badarg for a verb above UINT_MAX, before any driver callback",
            &[25],
            CONTROL_DRIVER,
        ),
        case(
            "driver_c/port_control_is_badarg_for_a_driver_without_a_control_callback",
            "port_control/3 raises badarg for every verb on a driver entry with no control slot",
            &[30],
            HOST_DRIVER,
        ),
        // --- the rest of the entry / port-state surface --------------------------------------
        case(
            "driver_c/erl_drv_output_term_delivers_the_connected_pid_operand",
            "erl_drv_output_term's ERL_DRV_PID operand arrives as the port's connected process itself",
            &[26],
            EXTRA_DRIVER,
        ),
        case(
            "driver_c/remove_driver_entry_of_an_unlisted_entry_answers_zero",
            "remove_driver_entry/1 answers 0 (no such entry) for an entry that is not in the driver list",
            &[27],
            EXTRA_DRIVER,
        ),
        case(
            "driver_c/set_busy_port_makes_port_command_nosuspend_report_false",
            "a busy driver port answers false to port_command/3 with [nosuspend] and still closes cleanly",
            &[28],
            EXTRA_DRIVER,
        ),
        // --- the port's own byte counters ----------------------------------------------------
        case(
            "driver_c/port_info2_counters_follow_one_command_round_trip",
            "after one port_command/2 round trip port_info/2 reports the payload on both {output, N} and {input, N}",
            &[29],
            HOST_DRIVER,
        ),
    ]
}
#[cfg(test)]
mod tests {
    //! Invariants of the A.4 end-to-end group: every case is a module-run case claiming
    //! [`Contract::DriverAbi`], it drives the shared module through `classify/1` with a real
    //! C fixture, and its golden is an integer. The three registry-family selectors (`7`, `8`,
    //! `9`) each carry their **own** C source, so a case's temp dir holds exactly one `.so`.

    use super::{
        group, CaseKind, Contract, NifFixture, CONTROL_DRIVER_C, ENTRY_DRIVER_C, EXTRA_DRIVER_C,
        HOST_DRIVER_C, LOCK_DRIVER_C, REGISTRY_DRIVER_C,
    };
    use std::collections::HashSet;

    /// The selectors that own a **dedicated** C source rather than the shared echo fixture. Each
    /// carries one family's checks, so it must resolve to its family's source and never fall back
    /// to [`HOST_DRIVER_C`], whose `output` callback only echoes.
    const OWN_SOURCE_SELECTORS: &[i64] = &[7, 8, 9, 20, 21, 22, 23, 24, 25, 26, 27, 28];

    /// The source each own-source selector must carry (its family's fixture).
    fn family_source(selector: i64) -> Option<&'static str> {
        match selector {
            7 => Some(REGISTRY_DRIVER_C),
            8 => Some(LOCK_DRIVER_C),
            9 => Some(ENTRY_DRIVER_C),
            20..=25 => Some(CONTROL_DRIVER_C),
            26..=28 => Some(EXTRA_DRIVER_C),
            _ => None,
        }
    }

    #[test]
    fn every_case_is_a_driver_module_run_over_a_compiled_fixture() {
        let mut selectors = HashSet::new();
        for case in group() {
            assert!(
                case.id.starts_with("driver_c/"),
                "case {} is outside the driver_c group",
                case.id
            );
            assert!(!case.description.is_empty(), "case {} has no description", case.id);
            assert_eq!(
                case.contracts,
                &[Contract::DriverAbi, Contract::BeamAsm],
                "case {}: a driver case evidences A.4 and the compiled .beam (A.2)",
                case.id
            );
            let CaseKind::Module(run) = case.kind else {
                panic!("case {}: an A.4 case must be a module-run case", case.id);
            };
            assert_eq!(run.module, "driver_c", "case {}", case.id);
            assert_eq!(run.function, "classify", "case {}", case.id);
            assert_eq!(run.arity, 1, "case {}", case.id);
            let Some(NifFixture::CompiledLibrary { name, c_source }) = run.nif else {
                panic!("case {}: the driver must be a real C-compiled .so", case.id);
            };
            assert_eq!(name, "host_driver", "case {}: the driver name is fixed", case.id);
            assert!(
                [
                    HOST_DRIVER_C,
                    REGISTRY_DRIVER_C,
                    LOCK_DRIVER_C,
                    ENTRY_DRIVER_C,
                    CONTROL_DRIVER_C,
                    EXTRA_DRIVER_C
                ]
                .contains(&c_source),
                "case {}: the fixture is not one of the group's C sources",
                case.id
            );
            if OWN_SOURCE_SELECTORS.contains(&run.args[0]) {
                // A family fixture must not ride the echo fixture: each case gets its own `.so`,
                // and a source shared across families would mean one family's checks riding on
                // another family's callback.
                assert_eq!(
                    Some(c_source),
                    family_source(run.args[0]),
                    "case {}: selector {} does not carry its family's C source",
                    case.id,
                    run.args[0]
                );
                assert_ne!(
                    c_source, HOST_DRIVER_C,
                    "case {}: selector {} must not ride the echo fixture",
                    case.id,
                    run.args[0]
                );
            } else {
                assert_eq!(
                    c_source, HOST_DRIVER_C,
                    "case {}: every port-lifecycle selector drives the echo fixture",
                    case.id
                );
            }
            assert!(
                selectors.insert(run.args[0]),
                "case {}: selector {} is used twice",
                case.id,
                run.args[0]
            );
            assert_eq!(
                case.golden.stdout,
                Some("1"),
                "case {}: each case is a yes/no question about the driver port",
                case.id
            );
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
        }
        assert_eq!(selectors.len(), group().len(), "every selector is distinct");
        assert_eq!(selectors.len(), 26, "the group covers twenty-six distinct selectors");
    }
}
