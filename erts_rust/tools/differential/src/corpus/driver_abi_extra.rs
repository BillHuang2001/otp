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

//! Contract **A.4** — the driver C ABI, widened: a second block of live driver cases.
//!
//! A continuation of [`super::driver_abi`]: each case drives one `erl_drv_*` / `driver_*`
//! callback slot end to end through a real C driver `.so` opened as a port and compares both
//! runtimes. Case ids use the `driver_extra/…` prefix. See the node `CONTEXT.md`
//! ("How to add a corpus case").
//!
//! # What this group adds
//!
//! [`super::driver_abi`] already pins the port lifecycle, the allocator family, the
//! `erl_drv_{mutex,cond,rwlock,tsd,thread}_*` synchronisation family, the timers, `driver_select`,
//! `driver_system_info`, the registry slots and the six `ErlDrvTerm` operand shapes its delivery
//! family drives. This group widens the same end-to-end shape over the surfaces that group left
//! untouched, one selector per slot:
//!
//! * **the deprecated wall clock** — `driver_get_now/1`, the only `ERL_DRV_DEPRECATED_FUNC` of the
//!   frozen table: the `NULL`-out-parameter refusal, the three-field `megasecs`/`secs`/`microsecs`
//!   breakdown, its epoch anchoring and its non-decreasing read (selectors `1`, `2`, `5`, `6`);
//! * **the port-timeslice accounting** — `erl_drv_consume_timeslice/2`, whose small-`percent` arm
//!   is the frozen `0` both runtimes answer (selectors `3`, `7`, `8`);
//! * **the time-offset family** — `erl_drv_time_offset/1`'s **unit consistency** (the phase-free
//!   property; its absolute value differs by construction, see *Known gaps*) (`4`, `9`);
//! * **the zero-length allocator edges** — `driver_alloc(0)` / `driver_alloc_binary(0)` (`10`, `11`);
//! * **further synchronisation shapes** — a *shared* reader lock taken twice
//!   (`erl_drv_rwlock_tryrlock/1`), a held mutex's second `trylock`, and two live threads that
//!   `erl_drv_equal_tids/2` orders apart (`12`, `13`, `14`);
//! * **further `driver_select` shapes** — a combined read+write arm and an idempotent re-arm
//!   (`15`, `16`);
//! * **further timer shapes** — a re-armed port timer whose remaining time must not grow (`17`);
//! * **a shorter `driver_vec_to_buf/3`** — the truncated linearisation (`18`);
//! * **the driver-private port queue** — `driver_sizeq/1` on a **fresh** port and the
//!   `driver_enq`/`driver_pushq`/`driver_peekq`/`driver_deq` round trip, drained back to empty
//!   (`19`, `20`; see *Known gaps* for the hang a non-empty queue causes);
//! * **the port state flags** — `get_port_flags/1` reports the frozen `PORT_FLAG_BINARY` for a
//!   `binary` port (`21`);
//! * **the port data lock family** — `driver_pdl_create/1` and the `driver_pdl_inc_refc`/
//!   `driver_pdl_dec_refc`/`driver_pdl_get_refc`/`driver_pdl_lock`/`driver_pdl_unlock` reference
//!   count shape (`22`, `23`, `24`);
//! * **the built-in driver name resolution** — `open_port({spawn_driver, "tcp_inet"}, Opts)`
//!   resolves the name the emulator links in statically to a live driver port
//!   ([`DRIVER_BUILTIN_MODULE_SOURCE`]; no C fixture);
//! * **the remaining `ErlDrvTerm` operand tags** — the delivery family over `ERL_DRV_UINT`,
//!   `ERL_DRV_INT64`, `ERL_DRV_UINT64`, `ERL_DRV_BINARY`, `ERL_DRV_MAP`, `ERL_DRV_TUPLE`,
//!   `ERL_DRV_STRING_CONS`, `ERL_DRV_BUF2BINARY`, `ERL_DRV_EXT2TERM`, `ERL_DRV_FLOAT` and
//!   combinations of them, which [`super::driver_abi`] pinned for only six shapes (selectors
//!   `100..=120`).
//!
//! # The shape of a case
//!
//! Two Erlang modules carry the group. The **fixture** cases use
//! [`DRIVER_EXTRA_MODULE_SOURCE`], which opens the fixture
//! (`{spawn_driver, Dir ++ "/driver_extra"}`, with the `erl_ddll` fallback the oracle needs) and
//! sends a **one-byte selector** through `port_command/2`; the fixture's `output` callback
//! dispatches on that byte:
//!
//! * selectors `< 100` — the driver runs one ABI check and answers with a single status byte
//!   (`'1'` pass `= 49`); the module's `classify/1` reads that byte;
//! * selectors `100..=120` — the driver *delivers a decoded term* through `erl_drv_output_term/3`
//!   and emits **no** byte payload, so the owner's own message is the whole observable; the
//!   module's `classify_term/1` encodes it as the base-256 value of its canonical
//!   `term_to_binary/1` image. The C queues the term itself as the message, never a
//!   `{Port, Term}` wrapper (`io.c:6162-6176`).
//!
//! The **built-in-driver** cases use [`DRIVER_BUILTIN_MODULE_SOURCE`], which opens a driver port by
//! a name the emulator itself owns; they carry no C fixture (`nif: None`), so they need no C
//! compiler.
//!
//! # The observable (honest, never asserted module-side)
//!
//! `classify/1`, `classify_term/1` and `probe/1` **return exactly what the runtime did**, encoded
//! as a stable integer — they never compare against an expectation and never raise. Nothing
//! arriving within the module's own 2 s bound is the sentinel `-1`. The **golden is the oracle's
//! own answer** (verified per case with `differential run <id>`), so the corpus self-tests and a
//! runtime that answers differently is a *differing observable* (a `Mismatch`). Only the surfaces
//! on which the two runtimes genuinely agree are kept here; every divergence is a documented
//! handoff (see *Known gaps*), never a corpus case.
//!
//! # The fixture
//!
//! One fixture ([`DRIVER_EXTRA_C`]) carries one ABI check (or one delivery spec) per selector the
//! group drives. Every `driver_*` / `erl_drv_*` slot it references is exported by the loading
//! `beam_rt` binary (`-Wl,--export-dynamic`), so `dlopen(RTLD_NOW)` resolves on both runtimes.
//! The two `driver_get_now/1` calls carry the header's deliberate
//! `ERL_DRV_DEPRECATED_FUNC` attribute, so the fixture silences that one diagnostic for itself
//! rather than let a warning pollute a build log.
//!
//! A driver must never leak output at `stop` time (the adapters drop it) and must never close a
//! selected descriptor inside its own callback (the poller deregisters it right afterwards), so
//! the fixture keeps its pipe descriptors open and only answers from a live callback.
//!
//! # Known gaps (why the group is not larger)
//!
//! Every slot below is exported by `beam_rt` yet cannot be pinned end to end, because either the
//! two runtimes answer *differently* for a live port (a genuine divergence, a handoff to the owning
//! crate) or the oracle itself cannot complete the call:
//!
//! * **a non-empty driver-private queue at port close hangs the oracle** — `driver_enq` /
//!   `driver_pushq` leave bytes in `prt->ioq` when the callback returns; measured, the oracle then
//!   answers the probe but **never exits** (killed after 25 s, exit `137`), so a shape that leaves
//!   the queue non-empty is unpinnable. Only the round trip that **drains the queue back to empty**
//!   before returning is pinned (`20`). Owner: the oracle's port-close path, not a runtime gap.
//! * **`driver_pdl_get_refc(NULL)` SIGSEGVs the oracle** (measured), so the PDL family's
//!   `NULL`/foreign-handle shapes are unpinnable; only the live-handle create/refcount shapes are
//!   pinned (`22`, `23`, `24`). Owner: the oracle itself.
//! * **`ERL_DRV_LIST`** — the C pops the top of the decode stack as the **tail** and conses the
//!   `n - 1` values below it onto that tail (`io.c:6043-6055`), so a spec of `LIST 2` over the
//!   stack `[1, 2]` builds the *improper* `[1|2]`; [`beam_abi`'s decoder] builds a **proper** list
//!   of all `n` values instead (`[1,2]`). Measured: the oracle delivers `[1|2]` (ETF image
//!   base-256 `620622257615890077016322`) where the Rust runtime delivers `[1,2]`
//!   (`144495584870658`). Owner: `beam_abi` (`decode_term_spec`).
//! * **`driver_dl_open/1` + `driver_dl_close/1`** — the oracle **aborts the whole VM** (a core
//!   dump) on `driver_dl_open(NULL)` followed by `driver_dl_close/1` (measured), so the
//!   dynamic-loading round trip is unpinnable end to end. The refusal half (`driver_dl_open/1` on
//!   a non-library path answers `NULL` with a message in `driver_dl_error/0`) is already pinned by
//!   [`super::driver_abi`]'s selector `49`. Owner: `beam_abi` / the oracle itself.
//! * **the process-monitor family** (`driver_monitor_process` / `driver_demonitor_process` /
//!   `driver_get_monitored_process` / `driver_compare_monitors`) — `beam_rt`'s driver host leaves
//!   its hooks at the `DriverHost` default (`monitor_process` answers `None`), so the oracle
//!   creates a monitor the Rust runtime refuses. Owner: `beam_rt` / `beam_io`.
//! * **`erl_drv_time_offset/1`'s absolute value** — the C reports the real monotonic↔system
//!   offset, the Rust runtime anchors monotonic time on the system clock and so reports `0`; only
//!   its unit consistency is pinned (`4`, `9`), never the magnitude. Owner: `beam_rt` / `beam_abi`.
//! * **`driver_async/5`** and `erl_drv_set_os_pid/2` / `erl_drv_init_ack/2` / `driver_exit/2` /
//!   `driver_failure_*` — the async pool, the OS-pid record and the port-closing slots either have
//!   no Rust hook or terminate the connected process, so no live-port shape can agree (the first
//!   is already documented by [`super::driver_abi`]). Owner: `beam_rt` / `beam_io`.
//!
//! # Skip policy (contract A.4 needs host tools, like A.3)
//!
//! A C-compiled fixture needs a C compiler *and* the host `erl_driver.h`; when either is missing
//! the harness reports `Verdict::Skipped` with a loud reason — never a pass. The built-in-driver
//! cases carry no fixture, so they are not subject to that skip.
use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded Erlang half: open the fixture, send one selector byte, return the driver's
/// answer as a stable integer.
///
/// The module **reports what actually happened** — it never asserts a value and never raises —
/// so the observable is honest and the golden is simply the oracle's own answer. Nothing here
/// prints a path (the fixture is reached through a name that differs by construction between the
/// two runtimes).
const DRIVER_EXTRA_MODULE_SOURCE: &str = r#"-module(driver_extra).
-export([classify/1, classify_term/1]).

%% Send one selector byte to the C fixture and encode the answer it delivered as an integer.
%%
%% Nothing is compared here. A status selector leaves the answer on the `driver_output` byte
%% channel as `{Port, {data, Bytes}}`, which becomes the base-256 value of the bytes; no such
%% message within the module's own 2 s bound is the sentinel `-1`. The harness compares this
%% integer against the oracle's, so a runtime that behaves differently is a visible divergence,
%% never a hidden one.
classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    P = open(Dir),
    true = erlang:port_command(P, <<N>>),
    wait(P).

%% Only the `driver_output*` byte channels carry the answer as `{Port, {data, Bytes}}`. Any other
%% message (a delivery selector emits its term instead, so `classify_term/1` is the entry point
%% for those) is skipped rather than mistaken for the answer.
wait(P) ->
    receive
        {P, {data, Data}} -> bytes_code(iolist_to_binary(Data));
        _Other -> wait(P)
    after 2000 -> -1
    end.

%% Send one selector byte to the C fixture and encode the term it **delivered** as an integer.
%%
%% The driver's `erl_drv_output_term` call queues the decoded term itself as the message
%% (`io.c:6162-6176`), so the message that arrives *is* the whole observable — its canonical
%% `term_to_binary/1` image, read as a base-256 integer, is byte-exact and stable across VM starts
%% (no delivery spec here touches a pid, a ref or a port, whose image is per-VM-start). The
%% delivery selectors emit no byte payload, so the first message is the delivered term; nothing
%% arriving within the module's own 2 s bound is the sentinel `-1`.
classify_term(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    P = open(Dir),
    true = erlang:port_command(P, <<N>>),
    wait_term(P).

wait_term(_P) ->
    receive
        Term -> bytes_code(term_to_binary(Term))
    after 2000 -> -1
    end.

%% The bytes a `driver_output`-family call delivered, as a base-256 integer.
bytes_code(Bin) -> bytes_code(Bin, 0).

bytes_code(<<>>, Acc) -> Acc;
bytes_code(<<B, Rest/binary>>, Acc) -> bytes_code(Rest, Acc * 256 + B).

%% Reach the driver the way each runtime can, without changing the argv:
%%   * the oracle refuses a path in `{spawn_driver, _}` (`badarg`) and needs the object
%%     pre-loaded by `erl_ddll:load_driver/2`, then opened by the bare driver name;
%%   * the Rust runtime has no `erl_ddll` BIF and takes the object path as the name.
open(Dir) ->
    try open_port({spawn_driver, Dir ++ "/driver_extra"}, [binary]) of
        P when is_port(P) -> P
    catch
        _:_ -> open_loaded(Dir)
    end.

open_loaded(Dir) ->
    ok = erl_ddll:load_driver(Dir, "driver_extra"),
    open_port({spawn_driver, "driver_extra"}, [binary]).
"#;

/// The fixture: one ABI check (or one delivery spec) per selector the group drives.
///
/// Only `driver_*` / `erl_drv_*` slots the Rust runtime exports are referenced; every one of
/// them resolves at `dlopen(RTLD_NOW)` out of the loading process's dynamic symbol table.
const DRIVER_EXTRA_C: &str = r#"/* A real driver .so (contract A.4): the deprecated wall clock, the port-timeslice
 * accounting, the time-offset unit consistency, the zero-length allocator edges, further
 * synchronisation / select / timer / linearisation shapes, and the delivery family over the
 * remaining ErlDrvTerm operand tags. */
#include <erl_driver.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>

/* `driver_get_now/1` is the frozen table's one `ERL_DRV_DEPRECATED_FUNC`; silence only that
 * diagnostic, so the fixture builds warning-free without weakening anything else. */
#if defined(__GNUC__)
#pragma GCC diagnostic ignored "-Wdeprecated-declarations"
#endif

typedef struct {
    ErlDrvPort port;
    int rfd; /* read end of the select pipe (or -1) */
    int wfd; /* write end of the select pipe (or -1) */
} extra_state;

static void ex_answer(extra_state *st, int ok) {
    char c = ok ? '1' : '0';
    (void) driver_output(st->port, &c, 1);
}

/* -- the deprecated wall clock (`driver_get_now/1`) ---------------------------------------- */

/* The C's `NULL` out-parameter refusal (`io.c:6962`). */
static int chk_get_now_null(void) { return driver_get_now(NULL) == -1; }

/* The three-field breakdown: `secs` is the second within a megasecond and `microsecs` the
 * microsecond within a second, so both are strictly below a million on either runtime. */
static int chk_get_now_breakdown(void) {
    ErlDrvNowData now;
    memset(&now, 0, sizeof(now));
    if (driver_get_now(&now) != 0)
        return 0;
    return now.secs < 1000000 && now.microsecs < 1000000;
}

/* The epoch is a Unix-anchored wall clock on both runtimes: `megasecs` counts whole megaseconds
 * since it, so any date past 2001-09-09 leaves it at or above a thousand. */
static int chk_get_now_epoch(void) {
    ErlDrvNowData now;
    memset(&now, 0, sizeof(now));
    if (driver_get_now(&now) != 0)
        return 0;
    return now.megasecs >= 1000;
}

/* The C enforces a non-decreasing clock (`io.c:6863-6876`); the Rust runtime reads the realtime
 * clock. A read taken a few milliseconds later is at or after the first on either. */
static int chk_get_now_non_decreasing(void) {
    ErlDrvNowData first, later;
    memset(&first, 0, sizeof(first));
    memset(&later, 0, sizeof(later));
    if (driver_get_now(&first) != 0)
        return 0;
    usleep(1500);
    if (driver_get_now(&later) != 0)
        return 0;
    return later.megasecs > first.megasecs
        || (later.megasecs == first.megasecs && later.secs >= first.secs);
}

/* -- the port-timeslice accounting (`erl_drv_consume_timeslice/2`) -------------------------- */

/* The C charges `percent` (clamped to [1,100]) hundredths of a timeslice against the port's
 * reduction budget and only forces a yield once that budget is spent (`CONTEXT_REDS` = 4000,
 * `erl_port_task.c:1282-1297`). A freshly started port has a spent budget of zero, so any small
 * `percent` — including the `0` and the negative the C clamps to `1` — answers the frozen "no
 * yield yet" code `0`, which is also this runtime's whole answer. */
static int chk_consume_small(extra_state *st) {
    return erl_drv_consume_timeslice(st->port, 1) == 0;
}

static int chk_consume_zero(extra_state *st) {
    return erl_drv_consume_timeslice(st->port, 0) == 0;
}

static int chk_consume_negative(extra_state *st) {
    return erl_drv_consume_timeslice(st->port, -5) == 0;
}

/* -- the time-offset family (`erl_drv_time_offset/1`) -------------------------------------- */

/* The absolute offset is not pinnable (the C reports the real monotonic↔system offset, the Rust
 * runtime always `0`); its **unit consistency** is: a coarse reading is the finer one converted
 * down, on both. */
static int chk_time_offset_sec_from_usec(void) {
    return erl_drv_time_offset(ERL_DRV_SEC)
        == erl_drv_convert_time_unit(erl_drv_time_offset(ERL_DRV_USEC), ERL_DRV_USEC, ERL_DRV_SEC);
}

static int chk_time_offset_msec_from_usec(void) {
    return erl_drv_time_offset(ERL_DRV_MSEC)
        == erl_drv_convert_time_unit(erl_drv_time_offset(ERL_DRV_USEC), ERL_DRV_USEC, ERL_DRV_MSEC);
}

/* -- the zero-length allocator edges -------------------------------------------------------- */

/* `driver_alloc(0)` must hand out a pointer a `driver_free/1` accepts, never `NULL`. */
static int chk_alloc_zero(void) {
    char *p = (char *) driver_alloc(0);
    if (p == NULL)
        return 0;
    driver_free(p);
    return 1;
}

/* `driver_alloc_binary(0)` must lay out a zero-length payload. */
static int chk_alloc_binary_zero(void) {
    ErlDrvBinary *b = driver_alloc_binary(0);
    int ok;
    if (b == NULL)
        return 0;
    ok = (b->orig_size == 0);
    driver_free_binary(b);
    return ok;
}

/* -- further synchronisation shapes --------------------------------------------------------- */

/* Read locks are shared: a second `tryrlock` on the same thread must succeed as the first did. */
static int chk_rwlock_shared_readers(void) {
    ErlDrvRWLock *l = erl_drv_rwlock_create("driver_extra_rwlock");
    int ok = 0;
    if (l != NULL) {
        ok = (erl_drv_rwlock_tryrlock(l) == 0) && (erl_drv_rwlock_tryrlock(l) == 0);
        if (ok) {
            erl_drv_rwlock_runlock(l);
            erl_drv_rwlock_runlock(l);
        }
        erl_drv_rwlock_destroy(l);
    }
    return ok;
}

/* A mutex this thread already holds must refuse the second `trylock`. */
static int chk_mutex_second_trylock_refused(void) {
    ErlDrvMutex *m = erl_drv_mutex_create("driver_extra_mutex");
    int ok = 0;
    if (m != NULL) {
        ok = (erl_drv_mutex_trylock(m) == 0) && (erl_drv_mutex_trylock(m) != 0);
        erl_drv_mutex_unlock(m);
        erl_drv_mutex_destroy(m);
    }
    return ok;
}

static void *ex_thread_body(void *arg) { return arg; }

/* Two live threads are two distinct handles. */
static int chk_tids_distinct(void) {
    ErlDrvTid a = NULL, b = NULL;
    void *res = NULL;
    int ok;
    if (erl_drv_thread_create("driver_extra_t1", &a, ex_thread_body, NULL, NULL) != 0)
        return 0;
    if (erl_drv_thread_create("driver_extra_t2", &b, ex_thread_body, NULL, NULL) != 0) {
        (void) erl_drv_thread_join(a, &res);
        return 0;
    }
    ok = (erl_drv_equal_tids(a, b) == 0);
    (void) erl_drv_thread_join(a, &res);
    (void) erl_drv_thread_join(b, &res);
    return ok;
}

/* -- further `driver_select` / timer / linearisation shapes --------------------------------- */

/* The pipe the select selectors arm; created once and deliberately never closed (the port's
 * poller deregisters a selection *after* the driver's `stop` callback). */
static int ex_pipe(extra_state *st) {
    int fds[2];
    if (st->rfd >= 0)
        return 1;
    if (pipe(fds) != 0)
        return 0;
    st->rfd = fds[0];
    st->wfd = fds[1];
    return 1;
}

/* A combined read+write interest arms and releases as one. */
static int chk_select_read_write(extra_state *st) {
    int ok = 0;
    if (ex_pipe(st)) {
        ok = (driver_select(st->port, (ErlDrvEvent) (intptr_t) st->rfd,
                            ERL_DRV_READ | ERL_DRV_WRITE, 1) == 0);
        if (ok)
            ok = (driver_select(st->port, (ErlDrvEvent) (intptr_t) st->rfd,
                                ERL_DRV_READ | ERL_DRV_WRITE, 0) == 0);
    }
    return ok;
}

/* Re-arming an already armed interest is idempotent. */
static int chk_select_rearm_idempotent(extra_state *st) {
    int ok = 0;
    if (ex_pipe(st)) {
        ok = (driver_select(st->port, (ErlDrvEvent) (intptr_t) st->rfd, ERL_DRV_READ, 1) == 0);
        if (ok)
            ok = (driver_select(st->port, (ErlDrvEvent) (intptr_t) st->rfd, ERL_DRV_READ, 1) == 0);
        if (ok)
            ok = (driver_select(st->port, (ErlDrvEvent) (intptr_t) st->rfd, ERL_DRV_READ, 0) == 0);
    }
    return ok;
}

/* Re-arming a port timer with a shorter delay must leave a positive remaining time no larger
 * than the first reading. */
static int chk_timer_rearm_shrinks(extra_state *st) {
    unsigned long first = 0, second = 0;
    int a = driver_set_timer(st->port, 50000);
    int r1 = driver_read_timer(st->port, &first);
    int b = driver_set_timer(st->port, 1000);
    int r2 = driver_read_timer(st->port, &second);
    (void) driver_cancel_timer(st->port);
    return a == 0 && b == 0 && r1 == 0 && r2 == 0 && first > 0 && second > 0 && second <= first;
}

/* A buffer shorter than the vector linearises only the requested prefix. */
static int chk_vec_to_buf_prefix(void) {
    SysIOVec iov[1];
    ErlIOVec ev;
    char in[4], out[4];
    memcpy(in, "wxyz", 4);
    iov[0].iov_base = in;
    iov[0].iov_len = 4;
    ev.vsize = 1;
    ev.size = 4;
    ev.iov = iov;
    ev.binv = NULL;
    memset(out, 0, sizeof(out));
    return driver_vec_to_buf(&ev, out, 3) == 3 && memcmp(out, "wxy", 3) == 0;
}

/* -- the driver-private port queue (`driver_enq` / `driver_pushq` / `driver_sizeq` /
 *    `driver_peekq` / `driver_deq`) ---------------------------------------------------------- */

/* A **fresh** driver port's private queue is empty: `driver_sizeq` answers `0`
 * (`io.c:6827-6834`). */
static int chk_sizeq_fresh(extra_state *st) {
    return driver_sizeq(st->port) == 0;
}

/* The queue round trip: `driver_enq` appends at the tail, `driver_pushq` prepends at the head,
 * `driver_peekq` reports one iovec entry per enqueue and `driver_deq` drops from the head —
 * answering the bytes that **remain** (`io.c:6807-6813`), or `(ErlDrvSizeT) -1` for a request
 * larger than the queue (`io.c:6810`).
 *
 * The queue is drained back to **empty** before the callback returns: a driver that leaves its
 * private queue non-empty at port close makes the oracle hang (measured — see *Known gaps*), so
 * only the fully drained round trip is pinnable. */
static int chk_queue_round_trip(extra_state *st) {
    SysIOVec *iov;
    int vlen = -1;
    if (driver_enq(st->port, "abc", 3) != 0)
        return 0;
    if (driver_sizeq(st->port) != 3)
        return 0;
    if (driver_pushq(st->port, "de", 2) != 0)
        return 0;
    if (driver_sizeq(st->port) != 5)
        return 0;
    iov = driver_peekq(st->port, &vlen);
    if (iov == NULL || vlen != 2)
        return 0;
    if (iov[0].iov_len != 2 || iov[1].iov_len != 3)
        return 0;
    if (memcmp(iov[0].iov_base, "de", 2) != 0 || memcmp(iov[1].iov_base, "abc", 3) != 0)
        return 0;
    if (driver_deq(st->port, 2) != 3)
        return 0;
    if (driver_deq(st->port, 99) != (ErlDrvSizeT) -1)
        return 0;
    if (driver_sizeq(st->port) != 3)
        return 0;
    if (driver_deq(st->port, 3) != 0)
        return 0;
    return driver_sizeq(st->port) == 0;
}

/* -- the port state flags (`get_port_flags`) ------------------------------------------------- */

/* `get_port_flags` reads the port **state** bits, not `prt->control_flags` (`io.c:5325-5344`): a
 * port opened with the `binary` option reports the frozen `PORT_FLAG_BINARY` (1) and nothing else. */
static int chk_port_flags_binary(extra_state *st) {
    return get_port_flags(st->port) == PORT_FLAG_BINARY;
}

/* -- the port data lock family (`driver_pdl_*`) ---------------------------------------------- */

/* A valid port admits exactly one data lock and its handle is non-null (`io.c:6717-6734`). */
static int chk_pdl_create(extra_state *st) {
    return driver_pdl_create(st->port) != NULL;
}

/* A fresh PDL's reference count is 1 on creation (`pdl_init_refc`, `io.c:6634-6638`), and
 * `inc_refc`/`dec_refc` return the value *after* the change (`pdl_inctest_refc`/`pdl_dectest_refc`,
 * `io.c:6655-6678`). */
static int chk_pdl_refc_inc_dec(extra_state *st) {
    ErlDrvPDL pdl = driver_pdl_create(st->port);
    if (pdl == NULL)
        return 0;
    if (driver_pdl_get_refc(pdl) != 1)
        return 0;
    if (driver_pdl_inc_refc(pdl) != 2)
        return 0;
    if (driver_pdl_dec_refc(pdl) != 1)
        return 0;
    return driver_pdl_get_refc(pdl) == 1;
}

/* `driver_pdl_lock` raises the count **before** taking the lock and `driver_pdl_unlock` lowers it
 * **after** releasing it, so the count reads one higher for the duration of the critical section
 * (`io.c:6736-6757`). */
static int chk_pdl_lock_unlock_refc(extra_state *st) {
    ErlDrvPDL pdl = driver_pdl_create(st->port);
    int ok;
    if (pdl == NULL)
        return 0;
    ok = (driver_pdl_get_refc(pdl) == 1);
    driver_pdl_lock(pdl);
    ok = ok && (driver_pdl_get_refc(pdl) == 2);
    driver_pdl_unlock(pdl);
    return ok && (driver_pdl_get_refc(pdl) == 1);
}

/* -- the driver-private queue's zero-length / empty-queue edges ----------------------------- */

/* A zero-length append is accepted and touches nothing: the byte count stays `0`
 * (`erts_ioq_enqv`'s `size <= skip` early return, `erl_io_queue.c:246-247`). */
static int chk_enq_zero(extra_state *st) {
    char one = 'z';
    return driver_enq(st->port, &one, 0) == 0 && driver_sizeq(st->port) == 0;
}

/* The same zero-length no-op for the head-inserting sibling. */
static int chk_pushq_zero(extra_state *st) {
    char one = 'z';
    return driver_pushq(st->port, &one, 0) == 0 && driver_sizeq(st->port) == 0;
}

/* `driver_enq_bin` with `len == 0` returns before touching the queue (`io.c:6846-6848`). */
static int chk_enq_bin_zero(extra_state *st) {
    ErlDrvBinary *b = driver_alloc_binary(4);
    int ok;
    if (b == NULL)
        return 0;
    ok = (driver_enq_bin(st->port, b, 0, 0) == 0) && (driver_sizeq(st->port) == 0);
    driver_free_binary(b);
    return ok;
}

/* The same zero-length no-op for `driver_pushq_bin`. */
static int chk_pushq_bin_zero(extra_state *st) {
    ErlDrvBinary *b = driver_alloc_binary(4);
    int ok;
    if (b == NULL)
        return 0;
    ok = (driver_pushq_bin(st->port, b, 0, 0) == 0) && (driver_sizeq(st->port) == 0);
    driver_free_binary(b);
    return ok;
}

/* `driver_deq` of zero bytes drains nothing and reports the remaining size, which is `0` on a
 * fresh port (`io.c:6807-6813`). */
static int chk_deq_zero(extra_state *st) {
    return driver_deq(st->port, 0) == 0;
}

/* `driver_deq` of more than the queue holds answers the `(ErlDrvSizeT) -1` refusal and leaves the
 * queue untouched (`erts_ioq_deq`, `erl_io_queue.c:347-352`). */
static int chk_deq_over_size(extra_state *st) {
    return driver_deq(st->port, 1) == (ErlDrvSizeT) -1 && driver_sizeq(st->port) == 0;
}

/* `driver_peekqv` on an empty queue reports a zero size and zeroes the caller's vector
 * (`erts_ioq_peekqv`, `erl_io_queue.c:379-394`). */
static int chk_peekqv_empty(extra_state *st) {
    ErlIOVec ev;
    ErlDrvSizeT n;
    memset(&ev, 0xff, sizeof(ev));
    n = driver_peekqv(st->port, &ev);
    return n == 0 && ev.vsize == 0 && ev.size == 0;
}

/* A vector whose every byte is skipped enqueues nothing and answers `0`
 * (`erts_ioq_enqv`, `erl_io_queue.c:300-306`). */
static int chk_enqv_all_skipped(extra_state *st) {
    SysIOVec iov[1];
    ErlIOVec ev;
    char in[3];
    memcpy(in, "abc", 3);
    iov[0].iov_base = in;
    iov[0].iov_len = 3;
    ev.vsize = 1;
    ev.size = 3;
    ev.iov = iov;
    ev.binv = NULL;
    return driver_enqv(st->port, &ev, 3) == 0 && driver_sizeq(st->port) == 0;
}

/* The same all-skipped no-op for `driver_pushqv`. */
static int chk_pushqv_all_skipped(extra_state *st) {
    SysIOVec iov[1];
    ErlIOVec ev;
    char in[3];
    memcpy(in, "abc", 3);
    iov[0].iov_base = in;
    iov[0].iov_len = 3;
    ev.vsize = 1;
    ev.size = 3;
    ev.iov = iov;
    ev.binv = NULL;
    return driver_pushqv(st->port, &ev, 3) == 0 && driver_sizeq(st->port) == 0;
}

/* `driver_vec_to_buf` with a zero length copies nothing and reports `0` (`io.c:6510-6530`). */
static int chk_vec_to_buf_zero_len(void) {
    SysIOVec iov[1];
    ErlIOVec ev;
    char in[3], out[1];
    memcpy(in, "abc", 3);
    iov[0].iov_base = in;
    iov[0].iov_len = 3;
    ev.vsize = 1;
    ev.size = 3;
    ev.iov = iov;
    ev.binv = NULL;
    out[0] = 0;
    return driver_vec_to_buf(&ev, out, 0) == 0;
}

/* -- the time-conversion zero edge ---------------------------------------------------------- */

/* A zero reading converts to zero in every direction (`io.c:6984-6992`). */
static int chk_convert_zero(void) {
    return erl_drv_convert_time_unit(0, ERL_DRV_SEC, ERL_DRV_NSEC) == 0
        && erl_drv_convert_time_unit(0, ERL_DRV_USEC, ERL_DRV_MSEC) == 0;
}

/* -- further synchronisation accessors ------------------------------------------------------ */

/* A freshly created opts record carries the C's "platform default" stack marker `-1`
 * (`erl_drv_thread.c:585-593`). */
static int chk_thread_opts_default_stack(void) {
    ErlDrvThreadOpts *opts = erl_drv_thread_opts_create("driver_extra_opts");
    int ok = 0;
    if (opts != NULL) {
        ok = (opts->suggested_stack_size == -1);
        erl_drv_thread_opts_destroy(opts);
    }
    return ok;
}

/* A named condition variable's accessor returns the name it was created with
 * (`erl_drv_thread.c:283-286`). */
static int chk_cond_name(void) {
    ErlDrvCond *c = erl_drv_cond_create("driver_extra_cond");
    int ok = 0;
    if (c != NULL) {
        char *n = erl_drv_cond_name(c);
        ok = (n != NULL) && (strcmp(n, "driver_extra_cond") == 0);
        erl_drv_cond_destroy(c);
    }
    return ok;
}

/* The same for a named read/write lock (`erl_drv_thread.c:365-368`). */
static int chk_rwlock_name(void) {
    ErlDrvRWLock *l = erl_drv_rwlock_create("driver_extra_rwlock_name");
    int ok = 0;
    if (l != NULL) {
        char *n = erl_drv_rwlock_name(l);
        ok = (n != NULL) && (strcmp(n, "driver_extra_rwlock_name") == 0);
        erl_drv_rwlock_destroy(l);
    }
    return ok;
}

/* A null handle's name accessor is `NULL` on both runtimes (`erl_drv_thread.c:283,365`). */
static int chk_cond_name_null(void) {
    return erl_drv_cond_name(NULL) == NULL;
}

static int chk_rwlock_name_null(void) {
    return erl_drv_rwlock_name(NULL) == NULL;
}

/* -- the ErlDrvTerm *delivery* family over the remaining operand tags ----------------------- */
static void ex_send(extra_state *st, ErlDrvTermData *spec, int len) {
    (void) erl_drv_output_term(driver_mk_port(st->port), spec, len);
}

/* Each `shape` builds one well-formed `ErlDrvTerm` spec and delivers it. No byte payload is
 * emitted, so the delivered term is the whole observable; every operand is a value whose ETF
 * image is byte-exact and VM-start-independent (no pid, ref or port). */
static void ex_deliver(extra_state *st, int shape) {
    ErlDrvTermData sp[24];
    static int64_t i64_negative = -7;
    static int64_t i64_large = 4611686018427387904LL; /* 2^62 */
    static uint64_t u64_small = 5;
    static uint64_t u64_huge = 9223372036854775808ULL; /* 2^63 */
    static double f64 = 1.5;
    static const char ext_ok42[] = {131, 104, 2, 119, 2, 111, 107, 97, 42};
    static const char ext_list[] = {131, 108, 0, 0, 0, 2, 97, 1, 97, 2, 106};
    static const char three[] = {1, 2, 3};
    static const char abc[] = "abc";
    ErlDrvBinary *b;
    switch (shape) {
    case 0: sp[0] = ERL_DRV_UINT; sp[1] = 42; ex_send(st, sp, 2); return;
    case 1: sp[0] = ERL_DRV_UINT; sp[1] = (ErlDrvTermData) 1099511627776ULL; ex_send(st, sp, 2); return;
    case 2: sp[0] = ERL_DRV_INT64; sp[1] = (ErlDrvTermData) (intptr_t) &i64_negative; ex_send(st, sp, 2); return;
    case 3: sp[0] = ERL_DRV_UINT64; sp[1] = (ErlDrvTermData) (intptr_t) &u64_small; ex_send(st, sp, 2); return;
    case 4:
        b = driver_alloc_binary(3);
        if (b == NULL)
            return;
        memcpy(b->orig_bytes, three, 3);
        sp[0] = ERL_DRV_BINARY; sp[1] = (ErlDrvTermData) (intptr_t) b; sp[2] = 3; sp[3] = 0;
        ex_send(st, sp, 4);
        driver_free_binary(b);
        return;
    case 5: sp[0] = ERL_DRV_UINT; sp[1] = 0; ex_send(st, sp, 2); return;
    case 6: sp[0] = ERL_DRV_INT64; sp[1] = (ErlDrvTermData) (intptr_t) &i64_large; ex_send(st, sp, 2); return;
    case 7: sp[0] = ERL_DRV_UINT64; sp[1] = (ErlDrvTermData) (intptr_t) &u64_huge; ex_send(st, sp, 2); return;
    case 8:
        /* An `ERL_DRV_BINARY` window: offset 1, size 2 of a three-byte payload. */
        b = driver_alloc_binary(3);
        if (b == NULL)
            return;
        memcpy(b->orig_bytes, three, 3);
        sp[0] = ERL_DRV_BINARY; sp[1] = (ErlDrvTermData) (intptr_t) b; sp[2] = 2; sp[3] = 1;
        ex_send(st, sp, 4);
        driver_free_binary(b);
        return;
    case 9:
        /* `ERL_DRV_STRING` with a zero length: the empty list. */
        sp[0] = ERL_DRV_STRING; sp[1] = (ErlDrvTermData) (intptr_t) "x"; sp[2] = 0;
        ex_send(st, sp, 3);
        return;
    case 10:
        /* `ERL_DRV_STRING_CONS` onto the empty list: the character list "hi". */
        sp[0] = ERL_DRV_NIL; sp[1] = ERL_DRV_STRING_CONS;
        sp[2] = (ErlDrvTermData) (intptr_t) "hi"; sp[3] = 2;
        ex_send(st, sp, 4);
        return;
    case 11:
        sp[0] = ERL_DRV_INT; sp[1] = 1;
        sp[2] = ERL_DRV_INT; sp[3] = 2;
        sp[4] = ERL_DRV_INT; sp[5] = 3;
        sp[6] = ERL_DRV_TUPLE; sp[7] = 3;
        ex_send(st, sp, 8);
        return;
    case 12:
        sp[0] = ERL_DRV_INT; sp[1] = 1;
        sp[2] = ERL_DRV_INT; sp[3] = 2;
        sp[4] = ERL_DRV_INT; sp[5] = 3;
        sp[6] = ERL_DRV_INT; sp[7] = 4;
        sp[8] = ERL_DRV_MAP; sp[9] = 2;
        ex_send(st, sp, 10);
        return;
    case 13:
        /* An `ERL_DRV_EXT2TERM` image of `[1,2]`. */
        sp[0] = ERL_DRV_EXT2TERM; sp[1] = (ErlDrvTermData) (intptr_t) ext_list;
        sp[2] = (ErlDrvTermData) sizeof(ext_list);
        ex_send(st, sp, 3);
        return;
    case 14:
        sp[0] = ERL_DRV_BUF2BINARY; sp[1] = (ErlDrvTermData) (intptr_t) abc; sp[2] = 3;
        ex_send(st, sp, 3);
        return;
    case 15:
        sp[0] = ERL_DRV_ATOM; sp[1] = (ErlDrvTermData) (intptr_t) driver_mk_atom("ok");
        sp[2] = ERL_DRV_INT; sp[3] = 7;
        sp[4] = ERL_DRV_TUPLE; sp[5] = 2;
        ex_send(st, sp, 6);
        return;
    case 16:
        sp[0] = ERL_DRV_INT; sp[1] = (ErlDrvTermData) -7;
        ex_send(st, sp, 2);
        return;
    case 17:
        sp[0] = ERL_DRV_INT; sp[1] = 0;
        ex_send(st, sp, 2);
        return;
    case 18:
        /* An atom (both atom tables already hold) paired with a byte-string operand. */
        sp[0] = ERL_DRV_ATOM; sp[1] = (ErlDrvTermData) (intptr_t) driver_mk_atom("error");
        sp[2] = ERL_DRV_BUF2BINARY; sp[3] = (ErlDrvTermData) (intptr_t) abc; sp[4] = 3;
        sp[5] = ERL_DRV_TUPLE; sp[6] = 2;
        ex_send(st, sp, 7);
        return;
    case 19:
        sp[0] = ERL_DRV_NIL; sp[1] = ERL_DRV_NIL; sp[2] = ERL_DRV_TUPLE; sp[3] = 2;
        ex_send(st, sp, 4);
        return;
    case 20:
        sp[0] = ERL_DRV_FLOAT; sp[1] = (ErlDrvTermData) (intptr_t) &f64;
        ex_send(st, sp, 2);
        return;
    default:
        return;
    }
}

/* -- thread lifecycle, lock refusals and condition waiting ---------------------------------- */

/* `erl_drv_thread_exit/1` terminates the calling thread and hands its argument to the joiner
 * (`erl_drv_thread.c:705-730`): `erl_drv_thread_join/2` answers `0` and writes that very pointer
 * through `respp`. Only pointer identity is compared — the value never leaves C. */
static void *ex_exiting_worker(void *arg) {
    erl_drv_thread_exit(arg);
    return NULL; /* unreachable: `erl_drv_thread_exit` never returns */
}

static int chk_thread_exit_reaches_joiner(void) {
    ErlDrvTid tid;
    void *resp = NULL;
    void *magic = (void *) (intptr_t) 0x1234abcd;
    if (erl_drv_thread_create("driver_extra_exit", &tid, ex_exiting_worker, magic, NULL) != 0)
        return 0;
    if (erl_drv_thread_join(tid, &resp) != 0)
        return 0;
    return resp == magic;
}

/* A read/write lock refuses the *opposite* acquisition mode: a writer trylock under a held read
 * lock, and a reader trylock under a held write lock, both answer the non-zero `EBUSY`
 * (`erl_drv_thread.c:371-435`). The two success directions are pinned in `driver_abi`. */
static int chk_tryrwlock_refused_while_reader(void) {
    ErlDrvRWLock *l = erl_drv_rwlock_create("driver_extra_rw1");
    int ok = 0;
    if (l != NULL) {
        if (erl_drv_rwlock_tryrlock(l) == 0) {
            ok = (erl_drv_rwlock_tryrwlock(l) != 0);
            erl_drv_rwlock_runlock(l);
        }
        erl_drv_rwlock_destroy(l);
    }
    return ok;
}

static int chk_tryrlock_refused_while_writer(void) {
    ErlDrvRWLock *l = erl_drv_rwlock_create("driver_extra_rw2");
    int ok = 0;
    if (l != NULL) {
        if (erl_drv_rwlock_tryrwlock(l) == 0) {
            ok = (erl_drv_rwlock_tryrlock(l) != 0);
            erl_drv_rwlock_rwunlock(l);
        }
        erl_drv_rwlock_destroy(l);
    }
    return ok;
}

/* `erl_drv_cond_wait/2` releases the mutex and returns once the condition has been signalled
 * (`erl_drv_thread.c:306`). A worker thread locks the mutex and waits; this thread signals in a
 * bounded loop (a signal that arrives before the worker waits would be lost, so it repeats) and
 * then joins, so the case can never hang: the worker wakes on the first signal that lands after it
 * entered the wait, and the loop's final signal covers the rest. The worker's `woke` flag and the
 * `0` join answer are the whole observable. */
static ErlDrvMutex *ex_cw_mutex = NULL;
static ErlDrvCond *ex_cw_cond = NULL;
static volatile int ex_cw_woke = 0;

static void *ex_cw_worker(void *arg) {
    (void) arg;
    erl_drv_mutex_lock(ex_cw_mutex);
    erl_drv_cond_wait(ex_cw_cond, ex_cw_mutex);
    ex_cw_woke = 1;
    erl_drv_mutex_unlock(ex_cw_mutex);
    return NULL;
}

static int chk_cond_wait_wakes(void) {
    ErlDrvTid tid;
    int i;
    if (ex_cw_mutex == NULL)
        ex_cw_mutex = erl_drv_mutex_create("driver_extra_cw_mutex");
    if (ex_cw_cond == NULL)
        ex_cw_cond = erl_drv_cond_create("driver_extra_cw_cond");
    if (ex_cw_mutex == NULL || ex_cw_cond == NULL)
        return 0;
    ex_cw_woke = 0;
    if (erl_drv_thread_create("driver_extra_cw", &tid, ex_cw_worker, NULL, NULL) != 0)
        return 0;
    for (i = 0; i < 300 && !ex_cw_woke; i++) {
        usleep(2000);
        erl_drv_cond_signal(ex_cw_cond);
    }
    erl_drv_cond_signal(ex_cw_cond);
    if (erl_drv_thread_join(tid, NULL) != 0)
        return 0;
    return ex_cw_woke;
}

/* -- the selectors ------------------------------------------------------------------------- */

static void ex_dispatch(extra_state *st, unsigned char sel) {
    switch (sel) {
    case 1: ex_answer(st, chk_get_now_null()); break;
    case 2: ex_answer(st, chk_get_now_breakdown()); break;
    case 3: ex_answer(st, chk_consume_small(st)); break;
    case 4: ex_answer(st, chk_time_offset_sec_from_usec()); break;
    case 5: ex_answer(st, chk_get_now_epoch()); break;
    case 6: ex_answer(st, chk_get_now_non_decreasing()); break;
    case 7: ex_answer(st, chk_consume_zero(st)); break;
    case 8: ex_answer(st, chk_consume_negative(st)); break;
    case 9: ex_answer(st, chk_time_offset_msec_from_usec()); break;
    case 10: ex_answer(st, chk_alloc_zero()); break;
    case 11: ex_answer(st, chk_alloc_binary_zero()); break;
    case 12: ex_answer(st, chk_rwlock_shared_readers()); break;
    case 13: ex_answer(st, chk_mutex_second_trylock_refused()); break;
    case 14: ex_answer(st, chk_tids_distinct()); break;
    case 15: ex_answer(st, chk_select_read_write(st)); break;
    case 16: ex_answer(st, chk_select_rearm_idempotent(st)); break;
    case 17: ex_answer(st, chk_timer_rearm_shrinks(st)); break;
    case 18: ex_answer(st, chk_vec_to_buf_prefix()); break;
    /* The driver-private queue family (drained to empty), the port state flags and the port data
     * lock family. */
    case 19: ex_answer(st, chk_sizeq_fresh(st)); break;
    case 20: ex_answer(st, chk_queue_round_trip(st)); break;
    case 21: ex_answer(st, chk_port_flags_binary(st)); break;
    case 22: ex_answer(st, chk_pdl_create(st)); break;
    case 23: ex_answer(st, chk_pdl_refc_inc_dec(st)); break;
    case 24: ex_answer(st, chk_pdl_lock_unlock_refc(st)); break;
    /* The queue's zero-length / empty-queue edges, the conversion zero edge, the further
     * synchronisation accessors and the port-to-term null edge. */
    case 25: ex_answer(st, chk_enq_zero(st)); break;
    case 26: ex_answer(st, chk_pushq_zero(st)); break;
    case 27: ex_answer(st, chk_enq_bin_zero(st)); break;
    case 28: ex_answer(st, chk_pushq_bin_zero(st)); break;
    case 29: ex_answer(st, chk_deq_zero(st)); break;
    case 30: ex_answer(st, chk_deq_over_size(st)); break;
    case 31: ex_answer(st, chk_peekqv_empty(st)); break;
    case 32: ex_answer(st, chk_enqv_all_skipped(st)); break;
    case 33: ex_answer(st, chk_pushqv_all_skipped(st)); break;
    case 34: ex_answer(st, chk_vec_to_buf_zero_len()); break;
    case 35: ex_answer(st, chk_convert_zero()); break;
    case 36: ex_answer(st, chk_thread_opts_default_stack()); break;
    case 37: ex_answer(st, chk_cond_name()); break;
    case 38: ex_answer(st, chk_rwlock_name()); break;
    case 39: ex_answer(st, chk_cond_name_null()); break;
    case 40: ex_answer(st, chk_rwlock_name_null()); break;
    /* The thread-lifecycle, lock-refusal and condition-wait family. */
    case 41: ex_answer(st, chk_thread_exit_reaches_joiner()); break;
    case 42: ex_answer(st, chk_tryrwlock_refused_while_reader()); break;
    case 43: ex_answer(st, chk_tryrlock_refused_while_writer()); break;
    case 44: ex_answer(st, chk_cond_wait_wakes()); break;
    /* The delivery family: nothing is answered through the byte channel — the delivered term is
     * the observable. */
    case 100: ex_deliver(st, 0); break;
    case 101: ex_deliver(st, 1); break;
    case 102: ex_deliver(st, 2); break;
    case 103: ex_deliver(st, 3); break;
    case 104: ex_deliver(st, 4); break;
    case 105: ex_deliver(st, 5); break;
    case 106: ex_deliver(st, 6); break;
    case 107: ex_deliver(st, 7); break;
    case 108: ex_deliver(st, 8); break;
    case 109: ex_deliver(st, 9); break;
    case 110: ex_deliver(st, 10); break;
    case 111: ex_deliver(st, 11); break;
    case 112: ex_deliver(st, 12); break;
    case 113: ex_deliver(st, 13); break;
    case 114: ex_deliver(st, 14); break;
    case 115: ex_deliver(st, 15); break;
    case 116: ex_deliver(st, 16); break;
    case 117: ex_deliver(st, 17); break;
    case 118: ex_deliver(st, 18); break;
    case 119: ex_deliver(st, 19); break;
    case 120: ex_deliver(st, 20); break;
    default: ex_answer(st, 0); break;
    }
}

/* -- the entry points ---------------------------------------------------------------------- */

static ErlDrvData ex_start(ErlDrvPort port, char *command) {
    extra_state *st;
    (void) command;
    st = (extra_state *) driver_alloc(sizeof(extra_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    st->rfd = -1;
    st->wfd = -1;
    return (ErlDrvData) st;
}

static void ex_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

static void ex_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    extra_state *st = (extra_state *) drv_data;
    if (st == NULL || len < 1)
        return;
    ex_dispatch(st, (unsigned char) buf[0]);
}

static ErlDrvEntry ex_entry = {
    .init = NULL,
    .start = ex_start,
    .stop = ex_stop,
    .output = ex_output,
    .driver_name = "driver_extra",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(driver_extra) {
    return &ex_entry;
}
"#;

/// The compiled driver every case loads (written next to the `.beam` as `driver_extra.so`).
const DRIVER_EXTRA: NifFixture = NifFixture::CompiledLibrary {
    name: "driver_extra",
    c_source: DRIVER_EXTRA_C,
};

/// The embedded Erlang half of the **built-in-driver** sub-family: open a driver port by the name
/// the emulator links in statically, and encode the open/resolution shape as a stable integer.
///
/// `tcp_inet` is one of the oracle's statically linked drivers
/// (`erts/emulator/drivers/common/inet_drv.c:1348`), so the C resolves `{spawn_driver, "tcp_inet"}`
/// against its **loaded-driver list** rather than a `.so` (`io.c:599-628`). The Rust runtime must
/// resolve the same name the same way. The module compares nothing and asserts nothing: it encodes
/// what it observed, so a runtime that resolves differently reports a differing integer (a visible
/// divergence), never an exit-code-only difference. A refused/raised open is caught and encoded as
/// `-1` for the same reason.
///
/// This sub-family needs **no** C fixture: it drives a name the emulator itself owns.
const DRIVER_BUILTIN_MODULE_SOURCE: &str = r#"-module(driver_builtin).
-export([probe/1]).

%% Open a built-in emulator driver port and encode the observed open/resolution shape as an integer.
%%
%% Nothing is compared here. The integer is the observed shape, so the harness's diff against the
%% oracle is honest; a failed open is caught and encoded rather than raised, so a runtime that cannot
%% resolve the name still reports a stable integer.
probe(N) ->
    try
        P = open_port({spawn_driver, "tcp_inet"}, opts(N)),
        Shape = shape(P),
        _ = (catch erlang:port_close(P)),
        Shape
    catch
        _:_ -> -1
    end.

%% The option list a case drives.
opts(1) -> [];
opts(_) -> [binary].

%% The observable shape of a resolved built-in driver port:
%%   * 1 — a live port which reports `tcp_inet` as its registered driver name;
%%   * 0 — a live port whose name is not `tcp_inet` (a wrong resolution);
%%   * -1 — no port at all (a refused or raised open, encoded by `probe/1`).
shape(P) ->
    case is_port(P) andalso erlang:port_info(P, name) =:= {name, "tcp_inet"} of
        true -> 1;
        false -> 0
    end.
"#;

/// Build one module-run case over [`DRIVER_EXTRA_MODULE_SOURCE`] with [`DRIVER_EXTRA`] as its
/// `.so`.
///
/// `golden` is the **oracle's own** integer answer for `selector`, verified per case with
/// `differential run <id>`.
fn case(
    id: &'static str,
    description: &'static str,
    selector: &'static [i64],
    golden: &'static str,
) -> Case {
    module_case(id, description, selector, golden, "classify")
}

/// The same over the **delivery** entry point (`classify_term/1`): the fixture's selector emits an
/// `ErlDrvTerm` spec through `erl_drv_output_term/3` and *no* byte payload, so the observable is
/// the ETF image of the term the owner received.
fn delivery_case(
    id: &'static str,
    description: &'static str,
    selector: &'static [i64],
    golden: &'static str,
) -> Case {
    module_case(id, description, selector, golden, "classify_term")
}

/// One module-run case over [`DRIVER_EXTRA_MODULE_SOURCE`] with [`DRIVER_EXTRA`] as its `.so`.
fn module_case(
    id: &'static str,
    description: &'static str,
    selector: &'static [i64],
    golden: &'static str,
    function: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::DriverAbi, Contract::BeamAsm],
        kind: CaseKind::Module(ModuleRun {
            source: DRIVER_EXTRA_MODULE_SOURCE,
            module: "driver_extra",
            function,
            arity: 1,
            args: selector,
            nif: Some(DRIVER_EXTRA),
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// One module-run case over [`DRIVER_BUILTIN_MODULE_SOURCE`]: it opens a **built-in** emulator
/// driver port by name and needs no C fixture (`nif: None`).
///
/// `golden` is the **oracle's own** integer answer for `arg`, verified per case with
/// `differential run <id>`.
fn builtin_case(
    id: &'static str,
    description: &'static str,
    arg: &'static [i64],
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::DriverAbi, Contract::BeamAsm],
        kind: CaseKind::Module(ModuleRun {
            source: DRIVER_BUILTIN_MODULE_SOURCE,
            module: "driver_builtin",
            function: "probe",
            arity: 1,
            args: arg,
            nif: None,
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The widened A.4 group: one case per driver C ABI surface the two runtimes drive identically.
///
/// Every golden is the oracle's own answer (the status byte `'1'` is `49`; a delivery case's
/// golden is the base-256 value of the delivered term's canonical ETF image; a built-in-driver
/// case's golden is the observed open/resolution shape): each case reports the driver-backed
/// port's real behaviour, so a runtime that answers otherwise is a `Mismatch` — which is why only
/// the agreeing surfaces appear here (see the module docs' *Known gaps*).
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- thread lifecycle, lock refusal and condition waiting --------------------------
        case(
            "driver_extra/erl_drv_thread_exit_response_reaches_the_joiner",
            "a thread that leaves through erl_drv_thread_exit/1 is joinable and delivers its argument",
            &[41],
            "49",
        ),
        case(
            "driver_extra/erl_drv_rwlock_tryrwlock_refused_under_a_read_lock",
            "erl_drv_rwlock_tryrwlock/1 is refused while a shared read lock is held",
            &[42],
            "49",
        ),
        case(
            "driver_extra/erl_drv_rwlock_tryrlock_refused_under_a_write_lock",
            "erl_drv_rwlock_tryrlock/1 is refused while the exclusive write lock is held",
            &[43],
            "49",
        ),
        case(
            "driver_extra/erl_drv_cond_wait_wakes_on_signal",
            "erl_drv_cond_wait/2 blocks a worker thread until erl_drv_cond_signal/1 wakes it",
            &[44],
            "49",
        ),
        case(
            "driver_extra/driver_get_now_refuses_a_null_out_parameter",
            "driver_get_now/1 answers its -1 refusal for a NULL out-parameter",
            &[1],
            "49",
        ),
        case(
            "driver_extra/driver_get_now_fills_a_bounded_breakdown",
            "driver_get_now/1 fills megasecs/secs/microsecs with secs and microsecs below a million",
            &[2],
            "49",
        ),
        case(
            "driver_extra/driver_get_now_is_anchored_at_the_unix_epoch",
            "driver_get_now/1's megasecs field counts whole megaseconds since a Unix-like epoch",
            &[5],
            "49",
        ),
        case(
            "driver_extra/driver_get_now_reads_are_non_decreasing",
            "a later driver_get_now/1 read is at or after an earlier one",
            &[6],
            "49",
        ),
        case(
            "driver_extra/erl_drv_consume_timeslice_small_percent_answers_no_yield",
            "erl_drv_consume_timeslice/2 with a small percent does not exhaust the port's budget: 0",
            &[3],
            "49",
        ),
        case(
            "driver_extra/erl_drv_consume_timeslice_zero_percent_answers_no_yield",
            "erl_drv_consume_timeslice/2 with the C's 0 percent (clamped to 1) answers no yield: 0",
            &[7],
            "49",
        ),
        case(
            "driver_extra/erl_drv_consume_timeslice_negative_percent_answers_no_yield",
            "erl_drv_consume_timeslice/2 with a negative percent (clamped to 1) answers no yield: 0",
            &[8],
            "49",
        ),
        case(
            "driver_extra/erl_drv_time_offset_seconds_is_the_microsecond_reading_converted",
            "erl_drv_time_offset/1 in seconds is its microsecond reading converted to seconds",
            &[4],
            "49",
        ),
        case(
            "driver_extra/erl_drv_time_offset_milliseconds_is_the_microsecond_reading_converted",
            "erl_drv_time_offset/1 in milliseconds is its microsecond reading converted to milliseconds",
            &[9],
            "49",
        ),
        case(
            "driver_extra/driver_alloc_answers_a_pointer_for_a_zero_length",
            "driver_alloc/1 hands out a pointer for a zero length and driver_free/1 releases it",
            &[10],
            "49",
        ),
        case(
            "driver_extra/driver_alloc_binary_lays_out_a_zero_length_payload",
            "driver_alloc_binary/1 answers a zero-orig_size binary for a zero length",
            &[11],
            "49",
        ),
        case(
            "driver_extra/erl_drv_rwlock_tryrlock_admits_two_readers",
            "two erl_drv_rwlock_tryrlock/1 read locks are shared: both succeed",
            &[12],
            "49",
        ),
        case(
            "driver_extra/erl_drv_mutex_trylock_refuses_a_held_mutex",
            "erl_drv_mutex_trylock/1 succeeds once then refuses the same thread's second attempt",
            &[13],
            "49",
        ),
        case(
            "driver_extra/erl_drv_equal_tids_orders_two_threads_apart",
            "erl_drv_equal_tids/2 answers 0 for two distinct live threads",
            &[14],
            "49",
        ),
        case(
            "driver_extra/driver_select_arms_and_releases_a_read_write_interest",
            "driver_select/4 arms and releases a combined read+write descriptor interest",
            &[15],
            "49",
        ),
        case(
            "driver_extra/driver_select_re_arm_is_idempotent",
            "a second driver_select/4 arm of the same interest is accepted, then released",
            &[16],
            "49",
        ),
        case(
            "driver_extra/driver_set_timer_re_arm_leaves_a_shorter_remaining_time",
            "driver_read_timer/2's remaining time does not grow when the timer is re-armed shorter",
            &[17],
            "49",
        ),
        case(
            "driver_extra/driver_vec_to_buf_linearises_a_shorter_prefix",
            "driver_vec_to_buf/3 copies only the requested prefix of a vector",
            &[18],
            "49",
        ),
        case(
            "driver_extra/driver_sizeq_is_zero_on_a_fresh_port",
            "driver_sizeq/1 answers 0 for a freshly opened driver port's empty private queue",
            &[19],
            "49",
        ),
        case(
            "driver_extra/driver_queue_enq_pushq_peekq_deq_round_trip",
            "driver_enq/3 appends, driver_pushq/3 prepends, driver_peekq/2 chunks per enqueue and driver_deq/2 drains from the head, answering the remaining size",
            &[20],
            "49",
        ),
        case(
            "driver_extra/driver_get_port_flags_reports_the_binary_stream_flag",
            "get_port_flags/1 reports the port state's PORT_FLAG_BINARY (1) for a `binary` port",
            &[21],
            "49",
        ),
        case(
            "driver_extra/driver_pdl_create_answers_a_non_null_handle",
            "driver_pdl_create/1 admits one port data lock for a valid port and answers a non-null handle",
            &[22],
            "49",
        ),
        case(
            "driver_extra/driver_pdl_reference_count_follows_inc_and_dec",
            "a fresh PDL's driver_pdl_get_refc/1 is 1 and driver_pdl_inc_refc/1 / driver_pdl_dec_refc/1 return the value reached",
            &[23],
            "49",
        ),
        case(
            "driver_extra/driver_pdl_lock_raises_and_unlock_lowers_the_reference_count",
            "driver_pdl_lock/1 raises the count before locking and driver_pdl_unlock/1 lowers it after unlocking",
            &[24],
            "49",
        ),
        case(
            "driver_extra/driver_enq_of_zero_bytes_is_a_no_op",
            "driver_enq/3 with a zero length is accepted and leaves the private queue empty",
            &[25],
            "49",
        ),
        case(
            "driver_extra/driver_pushq_of_zero_bytes_is_a_no_op",
            "driver_pushq/3 with a zero length is accepted and leaves the private queue empty",
            &[26],
            "49",
        ),
        case(
            "driver_extra/driver_enq_bin_of_zero_length_is_a_no_op",
            "driver_enq_bin/4 with a zero length is accepted and leaves the private queue empty",
            &[27],
            "49",
        ),
        case(
            "driver_extra/driver_pushq_bin_of_zero_length_is_a_no_op",
            "driver_pushq_bin/4 with a zero length is accepted and leaves the private queue empty",
            &[28],
            "49",
        ),
        case(
            "driver_extra/driver_deq_of_zero_bytes_reports_the_remaining_size",
            "driver_deq/2 with a zero size drains nothing and reports the remaining size (0 on a fresh port)",
            &[29],
            "49",
        ),
        case(
            "driver_extra/driver_deq_beyond_the_queue_size_is_refused",
            "driver_deq/2 for more bytes than the queue holds answers (ErlDrvSizeT)-1 and leaves the queue untouched",
            &[30],
            "49",
        ),
        case(
            "driver_extra/driver_peekqv_on_an_empty_queue_reports_a_zero_vector",
            "driver_peekqv/2 on an empty queue reports 0 and zeroes the caller's vsize/size",
            &[31],
            "49",
        ),
        case(
            "driver_extra/driver_enqv_with_every_byte_skipped_enqueues_nothing",
            "driver_enqv/3 whose skip covers the whole vector answers 0 and leaves the queue empty",
            &[32],
            "49",
        ),
        case(
            "driver_extra/driver_pushqv_with_every_byte_skipped_enqueues_nothing",
            "driver_pushqv/3 whose skip covers the whole vector answers 0 and leaves the queue empty",
            &[33],
            "49",
        ),
        case(
            "driver_extra/driver_vec_to_buf_with_a_zero_length_copies_nothing",
            "driver_vec_to_buf/3 with a zero length copies nothing and reports 0",
            &[34],
            "49",
        ),
        case(
            "driver_extra/erl_drv_convert_time_unit_maps_zero_to_zero",
            "erl_drv_convert_time_unit/3 converts a zero reading to zero in either direction",
            &[35],
            "49",
        ),
        case(
            "driver_extra/erl_drv_thread_opts_create_seeds_the_default_stack",
            "erl_drv_thread_opts_create/1 seeds suggested_stack_size with the C's platform-default marker -1",
            &[36],
            "49",
        ),
        case(
            "driver_extra/erl_drv_cond_name_reports_the_created_name",
            "erl_drv_cond_name/1 answers the name its condition variable was created with",
            &[37],
            "49",
        ),
        case(
            "driver_extra/erl_drv_rwlock_name_reports_the_created_name",
            "erl_drv_rwlock_name/1 answers the name its read/write lock was created with",
            &[38],
            "49",
        ),
        case(
            "driver_extra/erl_drv_cond_name_answers_null_for_a_null_handle",
            "erl_drv_cond_name/1 answers NULL for a null condition-variable handle",
            &[39],
            "49",
        ),
        case(
            "driver_extra/erl_drv_rwlock_name_answers_null_for_a_null_handle",
            "erl_drv_rwlock_name/1 answers NULL for a null read/write lock handle",
            &[40],
            "49",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_uint_operand",
            "erl_drv_output_term/3 delivers 42 built from an ERL_DRV_UINT operand",
            &[100],
            "8610090",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_large_uint_operand",
            "erl_drv_output_term/3 delivers 2^40 built from an ERL_DRV_UINT operand",
            &[101],
            "620659583449595801239553",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_negative_int64_operand",
            "erl_drv_output_term/3 delivers -7 built from an ERL_DRV_INT64 operand",
            &[102],
            "144461225000953",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_uint64_operand",
            "erl_drv_output_term/3 delivers 5 built from an ERL_DRV_UINT64 operand",
            &[103],
            "8610053",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_driver_binary_operand",
            "erl_drv_output_term/3 delivers <<1,2,3>> built from an ERL_DRV_BINARY operand",
            &[104],
            "2424377751406135804419",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_zero_uint_operand",
            "erl_drv_output_term/3 delivers 0 built from an ERL_DRV_UINT operand",
            &[105],
            "8610048",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_large_int64_operand",
            "erl_drv_output_term/3 delivers 2^62 built from an ERL_DRV_INT64 operand",
            &[106],
            "40675555905685676169325707328",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_bignum_uint64_operand",
            "erl_drv_output_term/3 delivers 2^63 built from an ERL_DRV_UINT64 operand",
            &[107],
            "40675555905685676169325707392",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_driver_binary_window",
            "erl_drv_output_term/3's ERL_DRV_BINARY operand delivers the requested offset+size window",
            &[108],
            "9470225591430152707",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_an_empty_string_operand",
            "erl_drv_output_term/3 delivers [] built from a zero-length ERL_DRV_STRING operand",
            &[109],
            "33642",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_conses_a_string_operand_onto_nil",
            "erl_drv_output_term/3 delivers \"hi\" from an ERL_DRV_STRING_CONS operand onto the empty list",
            &[110],
            "144495584897129",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_three_tuple_operand",
            "erl_drv_output_term/3 delivers {1,2,3} built from a three-arity ERL_DRV_TUPLE",
            &[111],
            "2424018414519376306435",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_two_pair_map_operand",
            "erl_drv_output_term/3 delivers #{1=>2,3=>4} built from a two-pair ERL_DRV_MAP",
            &[112],
            "2666186124930067372160847936971012",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_an_ext2term_list_operand",
            "erl_drv_output_term/3 delivers [1,2] decoded from an ERL_DRV_EXT2TERM image",
            &[113],
            "144495584870658",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_buf2binary_operand",
            "erl_drv_output_term/3 delivers <<\"abc\">> built from an ERL_DRV_BUF2BINARY operand",
            &[114],
            "2424377751406142120547",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_an_existing_atom_with_an_int",
            "erl_drv_output_term/3 delivers {ok,7} — an atom both atom tables already hold",
            &[115],
            "2424018157238192136455",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_negative_int_operand",
            "erl_drv_output_term/3 delivers -7 built from an ERL_DRV_INT operand",
            &[116],
            "144461225000953",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_zero_int_operand",
            "erl_drv_output_term/3 delivers 0 built from an ERL_DRV_INT operand",
            &[117],
            "8610048",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_an_atom_and_binary_tuple",
            "erl_drv_output_term/3 delivers {error,<<\"abc\">>} from an atom plus a byte-string operand",
            &[118],
            "11447102099669137158451310979070794806682211",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_pair_of_empty_lists",
            "erl_drv_output_term/3 delivers {[],[]} built from two ERL_DRV_NIL operands",
            &[119],
            "564385704554",
        ),
        delivery_case(
            "driver_extra/erl_drv_output_term_delivers_a_float_operand",
            "erl_drv_output_term/3 delivers 1.5 built from an ERL_DRV_FLOAT operand",
            &[120],
            "619925890775301805309952",
        ),
        builtin_case(
            "driver_extra/builtin_tcp_inet_resolves_to_a_live_port",
            "open_port({spawn_driver,\"tcp_inet\"},[]) resolves the built-in driver name to a live port whose name is tcp_inet",
            &[1],
            "1",
        ),
        builtin_case(
            "driver_extra/builtin_tcp_inet_resolves_with_the_binary_option",
            "open_port({spawn_driver,\"tcp_inet\"},[binary]) resolves the built-in driver name to a live port whose name is tcp_inet",
            &[2],
            "1",
        ),
    ]
}
#[cfg(test)]
mod tests {
    //! Invariants of the widened A.4 group: every case is a module-run case claiming
    //! [`Contract::DriverAbi`], it drives the shared module with the real C fixture, and its
    //! golden is the oracle's integer answer for its one selector byte.

    use super::{group, CaseKind, Contract, ModuleRun, NifFixture};
    use std::collections::HashSet;

    /// The one selector family whose observable is a **delivered term** rather than a
    /// `driver_output` status byte, and the module entry point it must drive.
    const DELIVERY_SELECTORS: std::ops::RangeInclusive<i64> = 100..=120;

    /// The module name and entry point of the **built-in-driver** sub-family — the cases that open a
    /// driver name the emulator itself links in, so they carry no C fixture.
    const BUILTIN_MODULE: &str = "driver_builtin";
    const BUILTIN_FUNCTION: &str = "probe";

    /// Whether `run` is a built-in-driver case rather than a fixture case.
    fn is_builtin(run: &ModuleRun) -> bool {
        run.module == BUILTIN_MODULE
    }

    /// The module entry point a **fixture** case drives: `classify/1` for the status family,
    /// `classify_term/1` for the `ErlDrvTerm` delivery family.
    fn expected_function(selector: i64) -> &'static str {
        if DELIVERY_SELECTORS.contains(&selector) {
            "classify_term"
        } else {
            "classify"
        }
    }

    #[test]
    fn every_case_is_a_driver_module_run_over_the_right_fixture() {
        let mut selectors = HashSet::new();
        let mut sources = HashSet::new();
        let mut builtin = 0;
        for case in group() {
            assert!(
                case.id.starts_with("driver_extra/"),
                "case {} is outside the driver_extra group",
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
            assert_eq!(run.arity, 1, "case {}", case.id);
            if is_builtin(&run) {
                // The built-in-driver sub-family drives a name the emulator owns: it must carry
                // **no** fixture (a `.so` it never loads would only make the case depend on a C
                // compiler that it does not need).
                builtin += 1;
                assert!(
                    run.nif.is_none(),
                    "case {}: a built-in-driver case needs no C fixture",
                    case.id
                );
                assert_eq!(run.function, BUILTIN_FUNCTION, "case {}", case.id);
            } else {
                assert_eq!(run.module, "driver_extra", "case {}", case.id);
                assert_eq!(
                    run.function,
                    expected_function(run.args[0]),
                    "case {}: selector {} must drive its family's entry point",
                    case.id,
                    run.args[0]
                );
                let Some(NifFixture::CompiledLibrary { name, c_source }) = run.nif else {
                    panic!("case {}: the driver must be a real C-compiled .so", case.id);
                };
                assert_eq!(name, "driver_extra", "case {}: the driver name is fixed", case.id);
                sources.insert(c_source);
            }
            assert!(
                selectors.insert((run.module, run.args[0])),
                "case {}: (module {}, selector {}) is used twice",
                case.id,
                run.module,
                run.args[0]
            );
            // The golden is the oracle's own integer answer: it is never asserted module-side.
            let golden = case.golden.stdout.expect("a golden stdout");
            assert!(
                !golden.is_empty() && golden.bytes().all(|b| b.is_ascii_digit()),
                "case {}: the golden must be the oracle's integer answer, got {golden:?}",
                case.id
            );
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
        }
        assert_eq!(
            selectors.len(),
            group().len(),
            "every (module, selector) pair is distinct"
        );
        assert_eq!(sources.len(), 1, "the fixture sub-family rests on one C fixture");
        assert_eq!(builtin, 2, "the two built-in-driver cases");
    }
    #[test]
    fn the_group_is_materially_larger_than_the_status_family_alone() {
        let cases = group();
        assert!(cases.len() >= 25, "the widened group must add at least 25 cases");
        for case in &cases {
            let CaseKind::Module(run) = case.kind else {
                panic!("case {}: not a module-run case", case.id);
            };
            assert_eq!(run.args.len(), 1, "case {}: one selector byte", case.id);
            let selector = run.args[0];
            assert!(
                selector > 0 && selector < 256,
                "case {}: the selector must be a port_command byte, got {selector}",
                case.id
            );
        }
    }

    /// The status selectors answer the single status byte `'1'` (integer `49`), so their golden is
    /// `49`; the delivery selectors answer the base-256 value of the delivered term's ETF image —
    /// never `49`; the built-in-driver cases answer the observed open/resolution shape.
    #[test]
    fn the_goldens_are_the_status_byte_a_delivered_term_value_or_a_builtin_shape() {
        let mut status = 0;
        let mut deliveries = 0;
        let mut builtin = 0;
        for case in group() {
            let CaseKind::Module(run) = case.kind else { unreachable!() };
            let golden = case.golden.stdout.unwrap();
            if is_builtin(&run) {
                builtin += 1;
                assert_eq!(
                    golden, "1",
                    "case {}: a resolved built-in driver port is the shape 1",
                    case.id
                );
            } else if DELIVERY_SELECTORS.contains(&run.args[0]) {
                deliveries += 1;
                assert_ne!(
                    golden, "49",
                    "case {}: a delivery selector has the delivered term as its payload",
                    case.id
                );
            } else {
                status += 1;
                assert_eq!(golden, "49", "case {}: a status selector is the '1' byte", case.id);
            }
        }
        assert_eq!(status, 44, "the forty-four status selectors");
        assert_eq!(deliveries, DELIVERY_SELECTORS.count(), "one delivery case per selector");
        assert_eq!(builtin, 2, "the two built-in-driver cases");
    }
    /// The delivery family must actually cover the operand tags [`super::driver_abi`] left
    /// unexercised — `ERL_DRV_UINT`, `ERL_DRV_INT64`, `ERL_DRV_UINT64`, `ERL_DRV_BINARY`,
    /// `ERL_DRV_MAP`, a three-arity `ERL_DRV_TUPLE`, `ERL_DRV_STRING_CONS` and `ERL_DRV_EXT2TERM`
    /// — because a dropped selector would silently shrink the surface.
    #[test]
    fn the_delivery_family_covers_the_widened_operand_tags() {
        let selectors: HashSet<i64> = group()
            .into_iter()
            .map(|case| match case.kind {
                CaseKind::Module(run) => run.args[0],
                _ => unreachable!("every A.4 case is a module-run case"),
            })
            .collect();
        for selector in [100, 101, 102, 103, 104, 106, 107, 108, 110, 111, 112, 113, 114] {
            assert!(
                selectors.contains(&selector),
                "selector {selector} (a widened operand tag) is missing"
            );
        }
    }
}
