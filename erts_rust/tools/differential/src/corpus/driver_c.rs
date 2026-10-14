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
                 andalso erlang:port_info(P, output) =:= {output, 0})
    end.

%% Send one payload through the driver and answer the bytes it echoed back (`timeout` when the
%% driver answered nothing within the module's own 2 s bound, so a case can never hang).
echo(P, Bin) ->
    true = erlang:port_command(P, Bin),
    receive {P, {data, Bytes}} -> Bytes after 2000 -> timeout end.

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
    ]
}
#[cfg(test)]
mod tests {
    //! Invariants of the A.4 end-to-end group: every case is a module-run case claiming
    //! [`Contract::DriverAbi`], it drives the shared module through `classify/1` with a real
    //! C fixture, and its golden is an integer. The three registry-family selectors (`7`, `8`,
    //! `9`) each carry their **own** C source, so a case's temp dir holds exactly one `.so`.

    use super::{group, CaseKind, Contract, NifFixture, HOST_DRIVER_C};
    use std::collections::HashSet;

    #[test]
    fn every_case_is_a_driver_module_run_over_a_compiled_fixture() {
        let mut selectors = HashSet::new();
        let mut sources = HashSet::new();
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
            // The registry-family cases must not share a C source: each gets its own `.so` so a
            // case's temp dir holds exactly one library.
            if (7..=9).contains(&run.args[0]) {
                assert!(
                    sources.insert(c_source),
                    "case {}: selector {} reuses another case's C source",
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
        assert_eq!(
            selectors.len(),
            group().len(),
            "every selector is distinct"
        );
        assert_eq!(selectors.len(), 15, "the group covers fifteen distinct selectors");
        assert_eq!(sources.len(), 3, "the three registry selectors carry three C sources");
    }
}
