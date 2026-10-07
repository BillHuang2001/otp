/*
 * A REAL NIF shared object — the **process family** end-to-end fixture of the `beam_abi` NIF C ABI
 * (workspace contract A.3).
 *
 * It is compiled by the integration test `tests/nif_process.rs` with the system `cc` against the
 * system `erl_nif.h` and loaded through `beam_abi::nif::loader::load`. Its `enif_*` imports are
 * deliberately left undefined by the linker: they are resolved (`RTLD_NOW`) out of the *loading
 * process*'s dynamic symbol table, which the test binary publishes via `-Wl,--export-dynamic`
 * (`build.rs`, plus the `#[used] static ENIF_API` that keeps the `#[no_mangle]` symbols linked). A
 * successful load is therefore the empirical proof that the Rust process-family host functions are
 * exported as dynamic symbols and bind a real C NIF.
 *
 * `ERL_NIF_INIT` seeds `ErlNifEntry.name` with the stringified module name, so the only module this
 * library may be loaded as is `nif_process`.
 *
 * The family exercised here is the one implemented by `src/nif/enif/process.rs`:
 * `enif_send`, `enif_port_command`, `enif_whereis_pid`, `enif_whereis_port`,
 * `enif_is_current_process_alive`, `enif_is_process_alive`, `enif_is_port_alive`,
 * `enif_monitor_process`, `enif_demonitor_process`, `enif_compare_monitors`,
 * `enif_make_monitor_term` and `enif_schedule_nif`.
 *
 * The `enif_*` slots it references: `enif_get_int`, `enif_make_int`, `enif_make_atom`,
 * `enif_make_badarg`, `enif_make_tuple_from_array`, `enif_make_new_binary`, `enif_is_ref`,
 * `enif_self`, `enif_get_local_pid`, `enif_get_local_port` — plus the twelve family slots above.
 *
 * Portability note: this fixture uses ONLY `enif_*` functions whose table slots are populated on
 * stable Rust. In particular it never calls a C-variadic `enif_*` (`enif_make_tuple`,
 * `enif_make_list`, `enif_set_option`, `enif_fprintf`, `enif_snprintf`, `enif_vfprintf`,
 * `enif_vsnprintf`) — those table slots are `None` and their symbols are not exported, so
 * `dlopen(RTLD_NOW)` would fail. `enif_make_tuple_from_array` stands in for `enif_make_tuple`.
 *
 * The return codes below are the ones `src/nif/enif/process.rs` documents and reproduces; the
 * oracle values they are pinned to were observed with the system OTP 29.1.1 / erts-17.1 (see the
 * "ORACLE" comments in `tests/nif_process.rs`).
 */
#include <erl_nif.h>
#include <string.h>

/* `{A, B}` — built with `enif_make_tuple_from_array` (the C-variadic `enif_make_tuple` slot is
 * `None` in this runtime and must never be referenced). */
static ERL_NIF_TERM pair(ErlNifEnv* env, ERL_NIF_TERM a, ERL_NIF_TERM b) {
    ERL_NIF_TERM arr[2];
    arr[0] = a;
    arr[1] = b;
    return enif_make_tuple_from_array(env, arr, 2);
}

/* send_to_self/1 -> the `enif_send` code (1 = sent, 0 = not sent).
 *
 * The message is the 2-tuple `{Int, self()}` — built from the integer argument, as the family's
 * `enif_send` contract is "the calling process sends a *constructed* term to a receiver".
 *
 * Oracle: `Pid ! Msg` always succeeds for a live `Pid`, so the C's `enif_send` returns 1 for a
 * live receiver and 0 only when the receiver is dead/foreign (`erl_nif.c:880/899/1093`). */
static ERL_NIF_TERM send_to_self(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid me;
    int n;
    if (argc != 1 || !enif_get_int(env, argv[0], &n))
        return enif_make_badarg(env);
    if (enif_self(env, &me) == NULL)
        return enif_make_badarg(env);
    return enif_make_int(env, enif_send(env, &me, NULL, pair(env, enif_make_int(env, n), me.pid)));
}

/* alive/0 -> `{is_current_process_alive(env), is_process_alive(env, self())}`.
 *
 * Oracle: `erlang:is_process_alive(self())` is `true` while the process runs, `false` once it is
 * dead; the NIF's codes are 1/0 for the same question (`erl_nif.c:2304-2357`). */
static ERL_NIF_TERM alive(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid me;
    (void)argc;
    (void)argv;
    if (enif_self(env, &me) == NULL)
        return enif_make_badarg(env);
    return pair(env,
                enif_make_int(env, enif_is_current_process_alive(env)),
                enif_make_int(env, enif_is_process_alive(env, &me)));
}

/* is_alive/1 -> `enif_is_process_alive` for a pid argument (0 for a dead/foreign one). */
static ERL_NIF_TERM is_alive(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid pid;
    if (argc != 1 || !enif_get_local_pid(env, argv[0], &pid))
        return enif_make_badarg(env);
    return enif_make_int(env, enif_is_process_alive(env, &pid));
}

/* is_port_alive/1 -> `enif_is_port_alive` for a port argument (0 for a closed one).
 *
 * Oracle: the `erlang` API for the same question is `port_info(Port, connected)` — `{connected,
 * Owner}` while the port is alive and `undefined` after `port_close/1`; the NIF's codes are 1/0. */
static ERL_NIF_TERM is_port_alive(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPort port;
    if (argc != 1 || !enif_get_local_port(env, argv[0], &port))
        return enif_make_badarg(env);
    return enif_make_int(env, enif_is_port_alive(env, &port));
}

/* port_command_code/1 -> the `enif_port_command` code (1 = accepted, 0 = not accepted).
 *
 * The data is a real 4-byte binary `<<"ping">>`, built in place with `enif_make_new_binary` — the
 * `erlang:port_command/2` rules are applied to the payload, so a non-iolist term would be rejected
 * even by a live port (observed on the oracle: with the atom `ping` as the data,
 * `enif_port_command` answers 0 and the VM reports `Bad value on output port`).
 *
 * Oracle: `erlang:port_command(Port, Data)` returns `true` for a live port and raises `badarg` for
 * a closed one — i.e. the C's 1, and "not delivered" for everything else
 * (`erl_nif.c:1124-1132`). */
static ERL_NIF_TERM port_command_code(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPort port;
    ERL_NIF_TERM data;
    unsigned char* payload = enif_make_new_binary(env, 4, &data);
    if (argc != 1 || !enif_get_local_port(env, argv[0], &port) || payload == NULL)
        return enif_make_badarg(env);
    memcpy(payload, "ping", 4);
    return enif_make_int(env, enif_port_command(env, &port, NULL, data));
}

/* monitor_code/1 -> the `enif_monitor_process` code.
 *
 * Oracle: `erlang:monitor(process, Pid)` returns a reference for a **live** pid; for a dead pid it
 * also returns a reference but immediately delivers `{'DOWN', Ref, process, Pid, noproc}` — i.e.
 * the monitor was *not* established. The NIF reports that distinction itself: 0 = established, 1 =
 * the target is dead/undefined (`erl_nif.c:3975-3976`, `:4004`; `-1` is the "resource type has no
 * `down` callback" code, which this seam cannot reach — see the module docs of `process.rs`).
 *
 * `obj` is `NULL`: the C reaches the resource's monitor table through it (`DATA_TO_RESOURCE`), so
 * the real oracle needs a live resource instance created by `enif_alloc_resource` — a slot this
 * runtime leaves `None` (the `resources` family is a stub), so a fixture that used one could not be
 * `dlopen`ed here at all. A `NULL` `obj` is the documented substitution of `process.rs`, and it is
 * the reason the monitor NIFs are driven by this crate's own host rather than by the oracle (the
 * oracle faults: `enif_monitor_process(env, NULL, …)` dereferences the null `obj`). */
static ERL_NIF_TERM monitor_code(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid pid;
    ErlNifMonitor mon;
    int code;
    if (argc != 1 || !enif_get_local_pid(env, argv[0], &pid))
        return enif_make_badarg(env);
    memset(&mon, 0, sizeof(mon));
    code = enif_monitor_process(env, NULL, &pid, &mon);
    return enif_make_int(env, code);
}

/* monitor_roundtrip/1 -> the 5-tuple
 *   `{MonitorCode, DemonitorCode, SecondDemonitorCode, CompareSelf, MonitorTermIsRef}`
 *
 * for the pid argument: establish a monitor, rebuild its term, compare the holder with itself, then
 * demonitor twice. Only a **successful** establishment makes the holder a real payload, so the last
 * four elements are `-1` when `MonitorCode != 0` (a zero-filled `ErlNifMonitor` is not a term the C
 * could reconstruct either).
 *
 * Oracle codes: 0 = established (`erl_nif.c:4010`); 0 = the monitor was identified and removed
 * (`:4051`), 1 = it could not be identified any more (`:4043-4044`) — which is the second
 * `enif_demonitor_process` here, and also what `erlang:demonitor/1` reports as `false`.
 * `enif_compare_monitors` is a `memcmp` over the first 24 bytes (`:4054-4059`), so a holder
 * compares equal to itself (0); `enif_make_monitor_term` rebuilds an **ordinary internal
 * reference** (`io.c:6994-7010`), so `enif_is_ref` answers 1. */
static ERL_NIF_TERM monitor_roundtrip(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM arr[5];
    ErlNifPid pid;
    ErlNifMonitor mon;
    int code, demonitor_code, again_code, compare_self, is_ref;
    if (argc != 1 || !enif_get_local_pid(env, argv[0], &pid))
        return enif_make_badarg(env);
    memset(&mon, 0, sizeof(mon));
    code = enif_monitor_process(env, NULL, &pid, &mon);
    if (code == 0) {
        compare_self = enif_compare_monitors(&mon, &mon);
        is_ref = enif_is_ref(env, enif_make_monitor_term(env, &mon));
        demonitor_code = enif_demonitor_process(env, NULL, &mon);
        again_code = enif_demonitor_process(env, NULL, &mon);
    } else {
        compare_self = -1;
        is_ref = -1;
        demonitor_code = -1;
        again_code = -1;
    }
    arr[0] = enif_make_int(env, code);
    arr[1] = enif_make_int(env, demonitor_code);
    arr[2] = enif_make_int(env, again_code);
    arr[3] = enif_make_int(env, compare_self);
    arr[4] = enif_make_int(env, is_ref);
    return enif_make_tuple_from_array(env, arr, 5);
}

/* compare_monitors/0 -> `{Equal, Less, Greater}` over two local holders whose payloads differ in
 * their very first byte: `enif_compare_monitors(&a,&a)` = 0, `(&a,&b)` < 0, `(&b,&a)` > 0. */
static ERL_NIF_TERM compare_monitors(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM arr[3];
    ErlNifMonitor a, b;
    (void)argc;
    (void)argv;
    memset(&a, 0, sizeof(a));
    memset(&b, 0, sizeof(b));
    b.data[0] = 1;
    arr[0] = enif_make_int(env, enif_compare_monitors(&a, &a));
    arr[1] = enif_make_int(env, enif_compare_monitors(&a, &b));
    arr[2] = enif_make_int(env, enif_compare_monitors(&b, &a));
    return enif_make_tuple_from_array(env, arr, 3);
}

/* whereis/1 -> `{Code, Pid}`: `enif_whereis_pid` for an atom name. The out-parameter is seeded with
 * the atom `undefined`, which is what the caller sees on a miss — the C leaves the out-parameter
 * untouched when it returns 0 (`erl_nif.c:1174/1186`), so seeding it is the *caller's* choice and
 * identical on both runtimes.
 *
 * Oracle: `whereis(Name)` is the pid for a registered name and the atom `undefined` for a name that
 * is not registered; `whereis(42)` raises `badarg` — the C's `is_not_atom(name)` early return 0. */
static ERL_NIF_TERM whereis(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid pid;
    int code;
    if (argc != 1)
        return enif_make_badarg(env);
    pid.pid = enif_make_atom(env, "undefined");
    code = enif_whereis_pid(env, argv[0], &pid);
    return pair(env, enif_make_int(env, code), pid.pid);
}

/* whereis_port/1 -> `{Code, Port}`: the `enif_whereis_port` counterpart of `whereis/1`. */
static ERL_NIF_TERM whereis_port(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPort port;
    int code;
    if (argc != 1)
        return enif_make_badarg(env);
    port.port_id = enif_make_atom(env, "undefined");
    code = enif_whereis_port(env, argv[0], &port);
    return pair(env, enif_make_int(env, code), port.port_id);
}

/* do_work/1 -> `{dirty, Arg}`: the NIF `reschedule/1` hands to `enif_schedule_nif`. */
static ERL_NIF_TERM do_work(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    if (argc != 1)
        return enif_make_badarg(env);
    return pair(env, enif_make_atom(env, "dirty"), argv[0]);
}

/* reschedule/1 -> the term `enif_schedule_nif(env, "do_work", ERL_NIF_DIRTY_JOB_CPU_BOUND,
 * do_work, 1, argv)` produces: the runtime runs `do_work/1` (on a dirty CPU scheduler when one is
 * available) with this call's own argument vector and its result becomes the result of
 * `reschedule/1`, i.e. `{dirty, Arg}` on the oracle. The seam of this crate hands the host
 * `(fun_name, flags, fptr, argc)`; with no host support it raises `badarg` (`erl_nif.c:3569-3598`
 * are the two argument rules it reproduces: `argc > MAX_ARG` and a `flags` bit outside the two
 * dirty-job bits). */
static ERL_NIF_TERM reschedule(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    if (argc != 1)
        return enif_make_badarg(env);
    return enif_schedule_nif(env, "do_work", ERL_NIF_DIRTY_JOB_CPU_BOUND, do_work, 1, argv);
}

static ErlNifFunc nif_process_funcs[] = {
    {"send_to_self", 1, send_to_self, 0},
    {"alive", 0, alive, 0},
    {"is_alive", 1, is_alive, 0},
    {"is_port_alive", 1, is_port_alive, 0},
    {"port_command_code", 1, port_command_code, 0},
    {"monitor_code", 1, monitor_code, 0},
    {"monitor_roundtrip", 1, monitor_roundtrip, 0},
    {"compare_monitors", 0, compare_monitors, 0},
    {"whereis", 1, whereis, 0},
    {"whereis_port", 1, whereis_port, 0},
    {"reschedule", 1, reschedule, 0},
    {"do_work", 1, do_work, 0}
};

ERL_NIF_INIT(nif_process, nif_process_funcs, NULL, NULL, NULL, NULL)
