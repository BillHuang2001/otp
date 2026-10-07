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
//! * the **Rust** runtime has no `erl_ddll` BIF yet and instead accepts the object **path**
//!   as the name (a documented Milestone-1 extension of `beam_rt::port::driver_lookup`).
//!
//! `open/2` below therefore tries the path form first and falls back to the
//! `erl_ddll` form, so the case reads identically on both sides. (Verified: the oracle
//! raises `badarg` for the path form and accepts the loaded bare name; the Rust runtime
//! raises `badarg` for the unloadable bare name and loads the path.)
//!
//! # The C source (why it is a *subset* of the fixture the `beam_io` tests use)
//!
//! The library is `dlopen(RTLD_NOW)`ed by both runtimes, so it may reference only `driver_*`
//! slots the Rust runtime really exports. It therefore uses just `driver_alloc`/`driver_free`
//! and `driver_output` (all exported by the `beam_rt` binary — see its
//! `-Wl,--export-dynamic` link options) and never the typed gaps documented in
//! `crates/beam_rt/src/port/driver.rs` (`driver_async` has no pool, and the entry's
//! `control` callback behind `port_control/3` is not routed yet). The observable surface is
//! consequently the **port lifecycle**: load → `start` → `output` → `stop`.
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
             receive {P, {data, Bytes}} -> yn(Bytes =:= <<"hello">>) end
    end.

yn(true) -> 1;
yn(_) -> 0.

%% Reach the driver the way each runtime can, without changing the argv:
%%   * the oracle refuses a path in `{spawn_driver, _}` (`badarg`) and needs the object
%%     pre-loaded by `erl_ddll:load_driver/2`, then opened by the bare driver name;
%%   * the Rust runtime has no `erl_ddll` BIF and takes the object path as the name.
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

/// The compiled driver every case loads (written next to the `.beam` as `<name>.so`).
const HOST_DRIVER: NifFixture = NifFixture::CompiledLibrary {
    name: "host_driver",
    c_source: HOST_DRIVER_C,
};

/// Build one module-run case over [`DRIVER_MODULE_SOURCE`].
fn case(
    id: &'static str,
    description: &'static str,
    selector: &'static [i64],
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
            nif: Some(HOST_DRIVER),
        }),
        golden: Golden {
            stdout: Some("1"),
            exit_code: 0,
        },
    }
}

/// The A.4 end-to-end group: a real C driver loaded and driven through the port lifecycle.
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
        ),
        case(
            "driver_c/spawn_driver_port_has_no_os_process",
            "a C driver port reports no OS process: port_info(P, os_pid) is undefined",
            &[2],
        ),
        case(
            "driver_c/port_close_runs_the_driver_stop_callback",
            "port_close/1 stops the driver (its stop callback frees the state) and the port is gone",
            &[3],
        ),
        case(
            "driver_c/port_command_reaches_the_driver_output_callback",
            "port_command/2 hands the bytes to the driver's output callback and is accepted",
            &[4],
        ),
        case(
            "driver_c/spawn_driver_name_may_carry_a_command",
            "a {spawn_driver, Name} whose name carries a command string still opens the driver",
            &[5],
        ),
        case(
            "driver_c/port_command_echo_round_trip",
            "the driver's driver_output bytes arrive as {Port, {data, <<\"hello\">>}}: port_command round trip",
            &[6],
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the A.4 end-to-end group: every case is a module-run case claiming
    //! [`Contract::DriverAbi`], it drives the shared module through `classify/1` with a real
    //! C fixture, and its golden is an integer.

    use super::{group, CaseKind, Contract, NifFixture};
    use std::collections::HashSet;

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
            assert!(
                matches!(run.nif, Some(NifFixture::CompiledLibrary { .. })),
                "case {}: the driver must be a real C-compiled .so",
                case.id
            );
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
    }
}
