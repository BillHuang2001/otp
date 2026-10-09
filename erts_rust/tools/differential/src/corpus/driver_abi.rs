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

//! Contract **A.4** — the driver C ABI, expanded: the driver callback slots, the
//! `driver_output*` byte round trips, the `select`/timer group, the
//! `erl_drv_{mutex,cond,rwlock,tsd,thread}_*` synchronisation family and the allocator family,
//! all driven through a **real C driver `.so`**.
//!
//! Where [`super::driver_c`] pins the *port lifecycle* (load → `start` → `output` → `stop`)
//! and the four driver-registry slots, this group widens the same end-to-end shape over the
//! **rest of the frozen `ErlDrvApi`** the two runtimes `dlopen` the same object against.
//!
//! # The shape of a case
//!
//! One shared Erlang module ([`DRIVER_ABI_MODULE_SOURCE`]) opens the fixture
//! (`{spawn_driver, Dir ++ "/driver_abi"}`, with the `erl_ddll` fallback the oracle needs)
//! and sends a **one-byte selector** through `port_command/2`. The fixture's `output`
//! callback dispatches on that byte:
//!
//! * selectors `< 100` — the driver runs one ABI check and answers with a single status byte
//!   (`'1'` pass, `'0'` fail);
//! * `110..113` — the driver *delivers bytes* through `driver_output`/`driver_output2`/
//!   `driver_output_binary`/`driver_outputv`;
//! * `200..202` — the driver **arms** something (a descriptor via `driver_select`, a port
//!   timer via `driver_set_timer`) and its `ready_input`/`ready_output`/`timeout` callback
//!   answers later, without another `port_command`.
//!
//! # The observable (honest, never asserted module-side)
//!
//! `classify/1` **returns exactly what the driver delivered**, encoded as a stable integer — it
//! never compares against an expectation and never raises. The base-256 value of the bytes a
//! `driver_output*` call delivered, or `-1` when no `{Port, {data, Bytes}}` message arrived within
//! the module's own 2 s bound. The **golden is the oracle's own answer**, so the corpus self-tests
//! and a runtime that answers differently is a *differing observable* (a `Mismatch`), which is
//! exactly what a module-run case must be. Only the surfaces on which the two runtimes genuinely
//! agree are kept here; every divergence is a documented handoff (see *Known gaps*), never a
//! corpus case — a module-run raise or a differing value is **not** a capability gap, and a
//! `Mismatch` is not something this group may ship.
//!
//! # The fixture
//!
//! One fixture ([`DRIVER_ABI_C`]) carries every callback slot and one ABI check per selector the
//! group drives. Every `driver_*` / `erl_drv_*` slot it references is exported by the loading
//! `beam_rt` binary (`-Wl,--export-dynamic`), so `dlopen(RTLD_NOW)` resolves on both runtimes.
//!
//! A driver must never leak output at `stop` time (the adapters drop it) and must never close
//! a selected descriptor inside its own callback (the poller deregisters it right afterwards),
//! so the fixture keeps its pipe descriptors open and only answers from a live callback.
//!
//! # Known gaps (why the group is not larger)
//!
//! Five A.4 surfaces were driven here but **do not agree**, so they are handoffs to the runtime
//! nodes rather than corpus cases (each is a plain, differing observable, never a capability gap
//! — the Rust runtime answers a different *value*, it does not signal `unsupported`):
//!
//! * `erl_drv_output_term` / `erl_drv_send_term` and their deprecated `driver_output_term` /
//!   `driver_send_term` aliases — the oracle delivers the bare term (`[7,9]`) to the port owner;
//!   the Rust runtime delivers nothing (`-1`). Owner: `beam_rt` / `beam_io`.
//! * `outputv`-only routing — the oracle routes `port_command/2` through the entry's `outputv`;
//!   the Rust runtime has no `outputv` path and delivers nothing (`-1`). Owner: `beam_rt` /
//!   `beam_io`.
//! * `driver_async/5` — the oracle schedules the job on the port's async pool; the Rust runtime
//!   answers the C's own failure code (`-1`, so the fixture reports `'0'`). Owner: `beam_rt` /
//!   `beam_io`.
//! * `driver_set_timer/2` without a `timeout` callback — the oracle *accepts* it (`0`); the Rust
//!   runtime answers the C's `-1`. Owner: `beam_rt` / `beam_io`.
//! * `erl_drv_monotonic_time/1` — the oracle answers a non-positive value for this call site (so
//!   the fixture's `> 0` check reports `'0'`); the Rust runtime answers a positive one (`'1'`).
//!   Owner: `beam_rt` / `beam_abi`.
//!
//! `driver_enq/3` + `driver_sizeq/1` (the port-queue family) **hangs the oracle** for a driver port
//! under a single CLI process, so it is deliberately unpinned. Owner: `beam_rt` / `beam_io`.
//!
//! # Skip policy (contract A.4 needs host tools, like A.3)
//!
//! A C-compiled fixture needs a C compiler *and* the host `erl_driver.h`; when either is
//! missing the harness reports `Verdict::Skipped` with a loud reason — never a pass.

use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded Erlang half: open the fixture, send one selector byte, return the driver's
/// answer as a stable integer.
///
/// The module **reports what actually happened** — it never asserts a value and never raises —
/// so the observable is honest and the golden is simply the oracle's own answer. Nothing here
/// prints a path (the fixture is reached through a name that differs by construction between
/// the two runtimes).
const DRIVER_ABI_MODULE_SOURCE: &str = r#"-module(driver_abi).
-export([classify/1]).

%% Send one selector byte to the C fixture and encode the answer it delivered as an integer.
%%
%% Nothing is compared here. A `driver_output*` byte delivery arrives as `{Port, {data, Bytes}}`
%% and becomes the base-256 value of the bytes; any other first message, or none within the
%% module's own 2 s bound, is the sentinel `-1`. The harness compares this integer against the
%% oracle's, so a runtime that behaves differently is a visible divergence, never a hidden one.
classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    P = open(Dir),
    true = erlang:port_command(P, <<N>>),
    receive
        {P, {data, Data}} -> bytes_code(iolist_to_binary(Data));
        _Other -> -1
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
    try open_port({spawn_driver, Dir ++ "/driver_abi"}, [binary]) of
        P when is_port(P) -> P
    catch
        _:_ -> open_loaded(Dir)
    end.

open_loaded(Dir) ->
    ok = erl_ddll:load_driver(Dir, "driver_abi"),
    open_port({spawn_driver, "driver_abi"}, [binary]).
"#;

/// The fixture: every callback slot this group drives, plus one ABI check per selector.
///
/// Only `driver_*` / `erl_drv_*` slots the Rust runtime exports are referenced; every one of
/// them resolves at `dlopen(RTLD_NOW)` out of the loading process's dynamic symbol table.
const DRIVER_ABI_C: &str = r#"/* A real driver .so (contract A.4): the callback slots plus one ABI check per selector byte. */
#include <erl_driver.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>

typedef struct {
    ErlDrvPort port;
    int rfd; /* read end of the select pipe (or -1) */
    int wfd; /* write end of the select pipe (or -1) */
} abi_state;

/* The pipe the select/readiness selectors arm. Created once, deliberately never closed: the
 * port's poller deregisters a selection *after* the driver's `stop` callback, so closing a
 * selected descriptor in `stop` would deregister a dead one. */
static int abi_pipe(abi_state *st) {
    int fds[2];
    if (st->rfd >= 0)
        return 1;
    if (pipe(fds) != 0)
        return 0;
    st->rfd = fds[0];
    st->wfd = fds[1];
    return 1;
}

static void abi_emit(abi_state *st, const char *bytes, ErlDrvSizeT len) {
    (void) driver_output(st->port, (char *) bytes, len);
}

static void abi_answer(abi_state *st, int ok) {
    char c = ok ? '1' : '0';
    abi_emit(st, &c, 1);
}

/* -- the allocator family ------------------------------------------------------------------ */

static int chk_alloc_free(void) {
    int i, ok = 0;
    char *p = (char *) driver_alloc(32);
    if (p != NULL) {
        for (i = 0; i < 32; i++)
            p[i] = (char) i;
        ok = 1;
        for (i = 0; i < 32 && ok; i++)
            if (p[i] != (char) i)
                ok = 0;
        driver_free(p);
    }
    return ok;
}

static int chk_realloc(void) {
    int i, ok = 0;
    char *p = (char *) driver_alloc(8);
    if (p != NULL) {
        char *q;
        for (i = 0; i < 8; i++)
            p[i] = (char) (i + 1);
        q = (char *) driver_realloc(p, 64);
        if (q != NULL) {
            ok = 1;
            for (i = 0; i < 8 && ok; i++)
                if (q[i] != (char) (i + 1))
                    ok = 0;
            driver_free(q);
        } else {
            driver_free(p);
        }
    }
    return ok;
}

static int chk_alloc_binary(void) {
    int i, ok = 0;
    ErlDrvBinary *b = driver_alloc_binary(16);
    if (b != NULL) {
        for (i = 0; i < 16; i++)
            b->orig_bytes[i] = (char) (i + 100);
        ok = (b->orig_size == 16);
        for (i = 0; i < 16 && ok; i++)
            if (b->orig_bytes[i] != (char) (i + 100))
                ok = 0;
        driver_free_binary(b);
    }
    return ok;
}

static int chk_binary_refc(void) {
    int ok = 0;
    ErlDrvBinary *b = driver_alloc_binary(8);
    if (b != NULL) {
        ok = (driver_binary_get_refc(b) == 1);
        if (ok) {
            (void) driver_binary_inc_refc(b);
            ok = (driver_binary_get_refc(b) == 2);
        }
        if (ok) {
            (void) driver_binary_dec_refc(b);
            ok = (driver_binary_get_refc(b) == 1);
        }
        driver_free_binary(b);
    }
    return ok;
}

static int chk_binary_realloc(void) {
    int i, ok = 0;
    ErlDrvBinary *b = driver_alloc_binary(8);
    if (b != NULL) {
        ErlDrvBinary *c;
        for (i = 0; i < 8; i++)
            b->orig_bytes[i] = (char) (i + 1);
        c = driver_realloc_binary(b, 32);
        if (c != NULL) {
            ok = 1;
            for (i = 0; i < 8 && ok; i++)
                if (c->orig_bytes[i] != (char) (i + 1))
                    ok = 0;
            driver_free_binary(c);
        } else {
            driver_free_binary(b);
        }
    }
    return ok;
}

/* -- the synchronisation family ----------------------------------------------------------- */

static int chk_mutex(void) {
    int ok = 0;
    ErlDrvMutex *m = erl_drv_mutex_create("driver_abi_mutex");
    if (m != NULL) {
        ok = (erl_drv_mutex_trylock(m) == 0);
        if (ok) {
            erl_drv_mutex_unlock(m);
            erl_drv_mutex_lock(m);
            erl_drv_mutex_unlock(m);
        }
        erl_drv_mutex_destroy(m);
    }
    return ok;
}

static int chk_cond(void) {
    int ok = 0;
    ErlDrvCond *c = erl_drv_cond_create("driver_abi_cond");
    if (c != NULL) {
        erl_drv_cond_signal(c);
        erl_drv_cond_broadcast(c);
        erl_drv_cond_destroy(c);
        ok = 1;
    }
    return ok;
}

static int chk_rwlock(void) {
    int ok = 0;
    ErlDrvRWLock *l = erl_drv_rwlock_create("driver_abi_rwlock");
    if (l != NULL) {
        ok = (erl_drv_rwlock_tryrlock(l) == 0);
        if (ok) {
            erl_drv_rwlock_runlock(l);
            erl_drv_rwlock_rlock(l);
            erl_drv_rwlock_runlock(l);
            ok = (erl_drv_rwlock_tryrwlock(l) == 0);
        }
        if (ok) {
            erl_drv_rwlock_rwunlock(l);
            erl_drv_rwlock_rwlock(l);
            erl_drv_rwlock_rwunlock(l);
        }
        erl_drv_rwlock_destroy(l);
    }
    return ok;
}

static int chk_tsd(void) {
    int ok = 0;
    ErlDrvTSDKey key = -1;
    if (erl_drv_tsd_key_create("driver_abi_tsd", &key) == 0) {
        int marker = 7;
        erl_drv_tsd_set(key, &marker);
        ok = (erl_drv_tsd_get(key) == &marker);
        erl_drv_tsd_set(key, NULL);
        ok = ok && (erl_drv_tsd_get(key) == NULL);
        erl_drv_tsd_key_destroy(key);
    }
    return ok;
}

static void *abi_thread_body(void *arg) {
    return arg;
}

static int chk_thread_opts(void) {
    int ok = 0;
    ErlDrvThreadOpts *opts = erl_drv_thread_opts_create("driver_abi_opts");
    if (opts != NULL) {
        erl_drv_thread_opts_destroy(opts);
        ok = 1;
    }
    return ok;
}

static int chk_thread_round_trip(void) {
    int ok = 0, marker = 42;
    ErlDrvTid tid = NULL;
    void *res = NULL;
    if (erl_drv_thread_create("driver_abi_thread", &tid, abi_thread_body, &marker, NULL) == 0) {
        ok = (tid != NULL) && (erl_drv_equal_tids(tid, tid) == 1);
        if (erl_drv_thread_join(tid, &res) == 0)
            ok = ok && (res == (void *) &marker);
        else
            ok = 0;
    }
    return ok;
}

static int chk_thread_self(void) {
    /* Only the self-comparison is safe to observe: the oracle aborts the whole VM when
     * `erl_drv_equal_tids/2` is handed a NULL handle (measured), so the NULL half is omitted. */
    ErlDrvTid me = erl_drv_thread_self();
    return (me != NULL) && (erl_drv_equal_tids(me, me) == 1);
}

/* -- the time / environment / port-attribute family ---------------------------------------- */

static int chk_convert_time_unit(void) {
    return erl_drv_convert_time_unit(1000, ERL_DRV_MSEC, ERL_DRV_SEC) == 1
        && erl_drv_convert_time_unit(1, ERL_DRV_SEC, ERL_DRV_MSEC) == 1000
        && erl_drv_convert_time_unit(1000000, ERL_DRV_USEC, ERL_DRV_SEC) == 1;
}

static int chk_vec_to_buf(void) {
    SysIOVec iov[2];
    ErlIOVec ev;
    char a[3], b[2], out[5];
    memcpy(a, "abc", 3);
    memcpy(b, "de", 2);
    iov[0].iov_base = a;
    iov[0].iov_len = 3;
    iov[1].iov_base = b;
    iov[1].iov_len = 2;
    ev.vsize = 2;
    ev.size = 5;
    ev.iov = iov;
    ev.binv = NULL;
    memset(out, 0, sizeof(out));
    return driver_vec_to_buf(&ev, out, 5) == 5 && memcmp(out, "abcde", 5) == 0;
}

static int chk_timer_round_trip(abi_state *st) {
    int ok = (driver_set_timer(st->port, 60000) == 0);
    if (ok)
        ok = (driver_cancel_timer(st->port) == 0);
    return ok;
}

static int chk_read_timer(abi_state *st) {
    unsigned long left = 0;
    return driver_read_timer(st->port, &left) == 0;
}

static int chk_cancel_timer(abi_state *st) {
    return driver_cancel_timer(st->port) == 0;
}

static int chk_select(abi_state *st) {
    int ok = 0;
    if (abi_pipe(st)) {
        ok = (driver_select(st->port, (ErlDrvEvent) (intptr_t) st->rfd, ERL_DRV_READ, 1) == 0);
        if (ok)
            ok = (driver_select(st->port, (ErlDrvEvent) (intptr_t) st->rfd, ERL_DRV_READ, 0) == 0);
    }
    return ok;
}

static int chk_system_info(void) {
    ErlDrvSysInfo si;
    memset(&si, 0, sizeof(si));
    driver_system_info(&si, sizeof(si));
    return si.driver_major_version == ERL_DRV_EXTENDED_MAJOR_VERSION
        && si.driver_minor_version == ERL_DRV_EXTENDED_MINOR_VERSION;
}

static int chk_putenv(void) {
    char value[32];
    size_t size = sizeof(value);
    memset(value, 0, sizeof(value));
    if (erl_drv_putenv("DRIVER_ABI_PROBE", "42") != 0)
        return 0;
    if (erl_drv_getenv("DRIVER_ABI_PROBE", value, &size) != 0)
        return 0;
    return size == 2 && memcmp(value, "42", 2) == 0;
}

/* -- the selectors ------------------------------------------------------------------------- */

static void abi_dispatch(abi_state *st, unsigned char sel) {
    switch (sel) {
    case 1: abi_answer(st, driver_mk_term_nil() == driver_term_nil); break;
    case 2: abi_answer(st, driver_connected(st->port) != driver_term_nil); break;
    case 3: abi_answer(st, driver_caller(st->port) != driver_term_nil); break;
    case 4: abi_answer(st, driver_connected(st->port) == driver_caller(st->port)); break;
    case 5: abi_answer(st, driver_mk_port(st->port) != driver_term_nil); break;
    case 6: abi_answer(st, driver_mk_atom("driver_abi_atom") != driver_term_nil); break;
    case 7: abi_answer(st, chk_alloc_free()); break;
    case 8: abi_answer(st, chk_realloc()); break;
    case 9: abi_answer(st, chk_alloc_binary()); break;
    case 10: abi_answer(st, chk_binary_refc()); break;
    case 11: abi_answer(st, chk_binary_realloc()); break;
    case 12: abi_answer(st, chk_mutex()); break;
    case 13: abi_answer(st, chk_cond()); break;
    case 14: abi_answer(st, chk_rwlock()); break;
    case 15: abi_answer(st, chk_tsd()); break;
    case 16: abi_answer(st, chk_thread_opts()); break;
    case 17: abi_answer(st, chk_thread_round_trip()); break;
    case 18: abi_answer(st, chk_thread_self()); break;
    case 20: abi_answer(st, chk_convert_time_unit()); break;
    case 21: abi_answer(st, chk_vec_to_buf()); break;
    case 22: abi_answer(st, chk_timer_round_trip(st)); break;
    case 23: abi_answer(st, chk_read_timer(st)); break;
    case 24: abi_answer(st, chk_cancel_timer(st)); break;
    case 25: abi_answer(st, chk_select(st)); break;
    case 26: abi_answer(st, chk_system_info()); break;
    case 27: abi_answer(st, chk_putenv()); break;
    /* Payload selectors: the bytes the *owner* receives are the observable. */
    case 110: abi_emit(st, "ABCDE", 5); break;
    case 111: (void) driver_output2(st->port, "H", 1, "body", 4); break;
    case 112: {
        ErlDrvBinary *b = driver_alloc_binary(8);
        if (b != NULL) {
            memcpy(b->orig_bytes, "abcdefgh", 8);
            (void) driver_output_binary(st->port, NULL, 0, b, 2, 5);
            driver_free_binary(b);
        }
        break;
    }
    case 113: {
        SysIOVec iov[2];
        ErlIOVec ev;
        char a[2], b[3];
        memcpy(a, "AA", 2);
        memcpy(b, "BBB", 3);
        iov[0].iov_base = a;
        iov[0].iov_len = 2;
        iov[1].iov_base = b;
        iov[1].iov_len = 3;
        ev.vsize = 2;
        ev.size = 5;
        ev.iov = iov;
        ev.binv = NULL;
        (void) driver_outputv(st->port, NULL, 0, &ev, 0);
        break;
    }
    /* Readiness selectors: arm the mechanism; the callback answers later. */
    case 200: {
        char b = 'x';
        if (abi_pipe(st)) {
            (void) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->rfd, ERL_DRV_READ, 1);
            if (write(st->wfd, &b, 1) != 1)
                abi_answer(st, 0);
        } else {
            abi_answer(st, 0);
        }
        break;
    }
    case 201:
        if (abi_pipe(st))
            (void) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->wfd, ERL_DRV_WRITE, 1);
        else
            abi_answer(st, 0);
        break;
    case 202:
        if (driver_set_timer(st->port, 20) != 0)
            abi_answer(st, 0);
        break;
    default: abi_answer(st, 0); break;
    }
}

/* -- the entry points ---------------------------------------------------------------------- */

static ErlDrvData abi_start(ErlDrvPort port, char *command) {
    abi_state *st;
    (void) command;
    st = (abi_state *) driver_alloc(sizeof(abi_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    st->rfd = -1;
    st->wfd = -1;
    return (ErlDrvData) st;
}

static void abi_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

static void abi_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    abi_state *st = (abi_state *) drv_data;
    if (st == NULL || len < 1)
        return;
    abi_dispatch(st, (unsigned char) buf[0]);
}

static void abi_ready_input(ErlDrvData drv_data, ErlDrvEvent event) {
    abi_state *st = (abi_state *) drv_data;
    int fd = (int) (intptr_t) event;
    char b;
    ssize_t nread;
    if (st == NULL)
        return;
    if (fd >= 0) {
        nread = read(fd, &b, 1);
        (void) nread;
    }
    (void) driver_select(st->port, event, ERL_DRV_READ, 0);
    abi_answer(st, 1);
}

static void abi_ready_output(ErlDrvData drv_data, ErlDrvEvent event) {
    abi_state *st = (abi_state *) drv_data;
    if (st == NULL)
        return;
    (void) driver_select(st->port, event, ERL_DRV_WRITE, 0);
    abi_answer(st, 1);
}

static void abi_timeout(ErlDrvData drv_data) {
    abi_state *st = (abi_state *) drv_data;
    if (st != NULL)
        abi_answer(st, 1);
}

static ErlDrvEntry abi_entry = {
    .init = NULL,
    .start = abi_start,
    .stop = abi_stop,
    .output = abi_output,
    .ready_input = abi_ready_input,
    .ready_output = abi_ready_output,
    .timeout = abi_timeout,
    .driver_name = "driver_abi",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(driver_abi) {
    return &abi_entry;
}
"#;

/// The compiled driver every case loads (written next to the `.beam` as `driver_abi.so`).
const DRIVER_ABI: NifFixture = NifFixture::CompiledLibrary {
    name: "driver_abi",
    c_source: DRIVER_ABI_C,
};

/// Build one module-run case over [`DRIVER_ABI_MODULE_SOURCE`] with [`DRIVER_ABI`] as its `.so`.
///
/// `golden` is the **oracle's own** integer answer for `selector`, verified per case with
/// `differential run <id>`.
fn case(
    id: &'static str,
    description: &'static str,
    selector: &'static [i64],
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::DriverAbi, Contract::BeamAsm],
        kind: CaseKind::Module(ModuleRun {
            source: DRIVER_ABI_MODULE_SOURCE,
            module: "driver_abi",
            function: "classify",
            arity: 1,
            args: selector,
            nif: Some(DRIVER_ABI),
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The expanded A.4 group: one case per driver C ABI surface the two runtimes drive identically.
///
/// Every golden is the oracle's own answer (the status byte `'1'` is `49`, the four payloads are
/// the base-256 values of the delivered bytes): each case reports the driver-backed port's real
/// behaviour, so a runtime that answers otherwise is a `Mismatch` — which is why only the
/// agreeing surfaces appear here (see the module docs' *Known gaps*).
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(
            "driver_abi/driver_term_nil_and_driver_mk_term_nil_are_the_same_symbol",
            "driver_mk_term_nil() is the driver_term_nil data symbol",
            &[1],
            "49",
        ),
        case(
            "driver_abi/driver_connected_answers_for_a_live_port",
            "driver_connected(port) answers a term rather than the nil symbol",
            &[2],
            "49",
        ),
        case(
            "driver_abi/driver_caller_answers_for_a_live_port",
            "driver_caller(port) answers a term rather than the nil symbol",
            &[3],
            "49",
        ),
        case(
            "driver_abi/driver_connected_is_the_caller_for_an_owner_opened_port",
            "the port's connected process and its caller are the same term",
            &[4],
            "49",
        ),
        case(
            "driver_abi/driver_mk_port_answers_a_term",
            "driver_mk_port(port) answers a port term rather than the nil symbol",
            &[5],
            "49",
        ),
        case(
            "driver_abi/driver_mk_atom_answers_a_term",
            "driver_mk_atom/1 answers an atom term rather than the nil symbol",
            &[6],
            "49",
        ),
        case(
            "driver_abi/driver_alloc_and_free_round_trip",
            "driver_alloc/1 hands out writable memory and driver_free/1 releases it",
            &[7],
            "49",
        ),
        case(
            "driver_abi/driver_realloc_preserves_the_contents",
            "driver_realloc/2 preserves the previous bytes",
            &[8],
            "49",
        ),
        case(
            "driver_abi/driver_alloc_binary_round_trip",
            "driver_alloc_binary/1 lays out orig_size/orig_bytes and driver_free_binary/1 releases it",
            &[9],
            "49",
        ),
        case(
            "driver_abi/driver_binary_refc_round_trip",
            "driver_binary_get_refc/inc_refc/dec_refc track the hidden reference count (1 -> 2 -> 1)",
            &[10],
            "49",
        ),
        case(
            "driver_abi/driver_realloc_binary_preserves_the_payload",
            "driver_realloc_binary/2 preserves the payload and updates orig_size",
            &[11],
            "49",
        ),
        case(
            "driver_abi/erl_drv_mutex_create_trylock_lock_unlock_destroy",
            "the erl_drv_mutex_* family creates, acquires, releases and destroys a driver mutex",
            &[12],
            "49",
        ),
        case(
            "driver_abi/erl_drv_cond_create_signal_broadcast_destroy",
            "the erl_drv_cond_* family creates, signals, broadcasts and destroys a condition variable",
            &[13],
            "49",
        ),
        case(
            "driver_abi/erl_drv_rwlock_create_read_write_destroy",
            "the erl_drv_rwlock_* family takes and releases both reader and writer locks",
            &[14],
            "49",
        ),
        case(
            "driver_abi/erl_drv_tsd_set_get_round_trip",
            "erl_drv_tsd_set/get round-trip a thread-local value under a created key",
            &[15],
            "49",
        ),
        case(
            "driver_abi/erl_drv_thread_opts_create_destroy",
            "erl_drv_thread_opts_create/1 hands out an options record and _destroy releases it",
            &[16],
            "49",
        ),
        case(
            "driver_abi/erl_drv_thread_create_join_round_trip",
            "erl_drv_thread_create/5 starts a real thread and erl_drv_thread_join/2 returns its reply",
            &[17],
            "49",
        ),
        case(
            "driver_abi/erl_drv_equal_tids_identifies_the_same_thread",
            "erl_drv_thread_self/0 is a stable handle: erl_drv_equal_tids/2 is 1 for itself",
            &[18],
            "49",
        ),
        case(
            "driver_abi/erl_drv_convert_time_unit_is_exact",
            "erl_drv_convert_time_unit/3 is exact across SEC/MSEC/USEC",
            &[20],
            "49",
        ),
        case(
            "driver_abi/driver_vec_to_buf_linearizes_a_vector",
            "driver_vec_to_buf/3 copies a vector's bytes into the caller's buffer",
            &[21],
            "49",
        ),
        case(
            "driver_abi/driver_set_timer_accepts_with_a_timeout_callback",
            "driver_set_timer/2 accepts and driver_cancel_timer/1 releases when the entry has a timeout callback",
            &[22],
            "49",
        ),
        case(
            "driver_abi/driver_read_timer_reports_a_remaining_time",
            "driver_read_timer/2 succeeds on a live driver port",
            &[23],
            "49",
        ),
        case(
            "driver_abi/driver_cancel_timer_succeeds",
            "driver_cancel_timer/1 succeeds on a live driver port",
            &[24],
            "49",
        ),
        case(
            "driver_abi/driver_select_arms_and_releases_a_descriptor",
            "driver_select/4 arms and releases a descriptor (both answer 0)",
            &[25],
            "49",
        ),
        case(
            "driver_abi/driver_system_info_reports_the_frozen_driver_version",
            "driver_system_info/2 fills the extended driver version",
            &[26],
            "49",
        ),
        case(
            "driver_abi/erl_drv_putenv_getenv_round_trip",
            "erl_drv_putenv/2 then erl_drv_getenv/3 round-trip a variable and its length",
            &[27],
            "49",
        ),
        case(
            "driver_abi/driver_output_delivers_bytes_to_the_port_owner",
            "driver_output/3 bytes reach the owner as {Port, {data, <<\"ABCDE\">>}}",
            &[110],
            "280284578885",
        ),
        case(
            "driver_abi/driver_output2_delivers_a_header_then_the_body",
            "driver_output2/5 delivers the header bytes followed by the body",
            &[111],
            "310889112697",
        ),
        case(
            "driver_abi/driver_output_binary_delivers_a_binary_window",
            "driver_output_binary/6 delivers the requested window of a driver binary",
            &[112],
            "426886129255",
        ),
        case(
            "driver_abi/driver_outputv_delivers_gathered_vectors",
            "driver_outputv/5 delivers the gathered vectors of an ErlIOVec",
            &[113],
            "280267735618",
        ),
        case(
            "driver_abi/ready_input_fires_for_an_armed_descriptor",
            "driver_select/4 arms a readable descriptor and the driver's ready_input callback runs",
            &[200],
            "49",
        ),
        case(
            "driver_abi/ready_output_fires_for_a_writable_descriptor",
            "driver_select/4 arms a writable descriptor and the driver's ready_output callback runs",
            &[201],
            "49",
        ),
        case(
            "driver_abi/timeout_callback_fires_for_an_armed_timer",
            "driver_set_timer/2 arms a port timer and the driver's timeout callback runs",
            &[202],
            "49",
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the expanded A.4 group: every case is a module-run case claiming
    //! [`Contract::DriverAbi`], it drives the shared module through `classify/1` with the real
    //! C fixture, and its golden is the oracle's integer answer for its one selector byte.

    use super::{group, CaseKind, Contract, NifFixture};
    use std::collections::HashSet;

    #[test]
    fn every_case_is_a_driver_module_run_over_the_compiled_fixture() {
        let mut selectors = HashSet::new();
        let mut sources = HashSet::new();
        for case in group() {
            assert!(
                case.id.starts_with("driver_abi/"),
                "case {} is outside the driver_abi group",
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
            assert_eq!(run.module, "driver_abi", "case {}", case.id);
            assert_eq!(run.function, "classify", "case {}", case.id);
            assert_eq!(run.arity, 1, "case {}", case.id);
            let Some(NifFixture::CompiledLibrary { name, c_source }) = run.nif else {
                panic!("case {}: the driver must be a real C-compiled .so", case.id);
            };
            assert_eq!(name, "driver_abi", "case {}: the driver name is fixed", case.id);
            sources.insert(c_source);
            assert!(
                selectors.insert(run.args[0]),
                "case {}: selector {} is used twice",
                case.id,
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
        assert_eq!(selectors.len(), group().len(), "every selector is distinct");
        assert_eq!(sources.len(), 1, "the group rests on one C fixture");
    }

    #[test]
    fn the_group_is_not_empty_and_stays_within_the_module_run_contract_shape() {
        let cases = group();
        assert!(cases.len() >= 30, "the expanded group is materially larger than driver_c");
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
    /// `49`; the four payload selectors answer the base-256 value of the bytes the fixture emits.
    #[test]
    fn the_goldens_are_the_status_byte_or_a_payload_value() {
        let mut payloads = 0;
        for case in group() {
            let CaseKind::Module(run) = case.kind else { unreachable!() };
            let golden = case.golden.stdout.unwrap();
            if run.args[0] >= 110 && run.args[0] <= 113 {
                payloads += 1;
                assert_ne!(golden, "49", "case {}: a payload selector has a byte payload", case.id);
            } else {
                assert_eq!(golden, "49", "case {}: a status selector is the '1' byte", case.id);
            }
        }
        assert_eq!(payloads, 4, "the four driver_output* byte round trips");
    }
}
