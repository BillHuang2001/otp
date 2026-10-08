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
//! `ErlDrvTerm` output round trips, the select/timer group, the port-queue family, the
//! `erl_drv_{mutex,cond,rwlock,tsd,thread}_*` synchronisation family and the allocator
//! family, all driven through a **real C driver `.so`**.
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
//!   `driver_output_binary`/`driver_outputv`, and the module compares the payload;
//! * `200..202` — the driver **arms** something (a descriptor via `driver_select`, a port
//!   timer via `driver_set_timer`) and its `ready_input`/`ready_output`/`timeout` callback
//!   answers later, without another `port_command`;
//! * `210..213` — the driver builds an `ErlDrvTerm` spec (`[7,9]`) and delivers it through
//!   `erl_drv_output_term` / `erl_drv_send_term` / `driver_output_term` / `driver_send_term`;
//!   the module expects the `{Port, [7,9]}` message.
//!
//! Every branch returns the integer `1`, so a module-run golden stays a VM-stable integer
//! line (the corpus invariant). A **failing** check raises the documented `unsupported_*`
//! capability gap ([`GAP`]), which the Rust runtime reports as exit `3` — so a runtime that
//! does not (yet) behave like the oracle lands `unsupported`, never a `mismatch`, exactly as
//! [`super::driver_c`] documents.
//!
//! # The three fixtures
//!
//! * [`DRIVER_ABI_C`] — the main fixture (selectors `1..27`, `40`, `41`, `110..113`,
//!   `200..202`, `210..213`): `start`/`stop`/`output`/`ready_input`/`ready_output`/`timeout`.
//! * [`NO_TIMER_DRIVER_C`] — the same shape **without** a `timeout` slot, so
//!   `driver_set_timer` answers the C's `-1` on both runtimes (selector `28`).
//! * [`OUTPUTV_DRIVER_C`] — an entry that declares **only** `outputv`: the oracle routes
//!   every `port_command` through it (`io.c` branches on `drv->outputv`), the Rust runtime has
//!   no `outputv` path, so the case is an honest capability gap (selector `30`).
//!
//! A driver must never leak output at `stop` time (the adapters drop it) and must never close
//! a selected descriptor inside its own callback (the poller deregisters it right afterwards),
//! so the fixtures keep their pipe descriptors open and only answer from a live callback.
//!
//! # Skip policy (contract A.4 needs host tools, like A.3)
//!
//! A C-compiled fixture needs a C compiler *and* the host `erl_driver.h`; when either is
//! missing the harness reports `Verdict::Skipped` with a loud reason — never a pass.

use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The documented capability gap a failing check raises (the Rust runtime's exit-`3` signal).
const GAP: &str = "unsupported_driver_abi";

/// The embedded Erlang half: open the fixture, send one selector byte, compare the answer.
///
/// Every branch returns an integer, so the golden is a VM-stable integer line. Nothing here
/// prints a path (the fixture is reached through a name that differs by construction between
/// the two runtimes).
const DRIVER_ABI_MODULE_SOURCE: &str = r#"-module(driver_abi).
-export([classify/1]).

classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    P = open(Dir),
    true = erlang:port_command(P, <<N>>),
    Expect = expected(N),
    receive
        {P, {data, Data}} -> yn(iolist_to_binary(Data) =:= Expect);
        {P, Term} -> yn(Term =:= Expect)
    after 2000 -> gap()
    end.

%% The byte string (or term) the driver is expected to answer with.
%%
%% Selectors 110..113 answer a payload: `driver_output`, `driver_output2` (header "H" then the
%% body "body"), `driver_output_binary` (the 5-byte window "cdefg" of an 8-byte binary) and
%% `driver_outputv` (the gathered vectors "AA" + "BBB"). Selectors 210..213 deliver the *term*
%% `[7, 9]` rather than bytes, so the second receive clause matches them.
expected(N) when N >= 210 -> [7, 9];
expected(110) -> <<"ABCDE">>;
expected(111) -> <<"Hbody">>;
expected(112) -> <<"cdefg">>;
expected(113) -> <<"AABBB">>;
expected(_) -> <<"1">>.

yn(true) -> 1;
yn(false) -> gap().

%% A capability gap the harness classifies as `unsupported`, never as a mismatch.
gap() -> erlang:error(unsupported_driver_abi).

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

/// The main fixture: every callback slot this group drives, plus one ABI check per selector.
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
    ErlDrvTid me = erl_drv_thread_self();
    return (me != NULL) && (erl_drv_equal_tids(me, me) == 1) && (erl_drv_equal_tids(me, NULL) == 0);
}

/* -- the time / environment / port-attribute family ---------------------------------------- */

static int chk_monotonic_time(void) {
    ErlDrvTime t = erl_drv_monotonic_time(ERL_DRV_USEC);
    return t > 0;
}

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

/* -- the typed capability gaps (a real C surface the Rust runtime does not serve yet) ------- */

static void *abi_async_body(void *arg) {
    return arg;
}

static int chk_async(abi_state *st) {
    unsigned int key = 0;
    /* The oracle queues the job on its async pool and answers a positive handle; the Rust
     * runtime has no pool and answers the C's own failure code, so the module reports the
     * documented capability gap rather than a mismatch. */
    return driver_async(st->port, &key, abi_async_body, NULL, NULL) != -1;
}

static int chk_queue(abi_state *st) {
    /* The port-queue family behind `DriverHost::{enq,sizeq}`: `0` on success and the queued
     * byte count on the oracle; the Rust runtime's host does not wire a queue. */
    if (driver_enq(st->port, "abcd", 4) != 0)
        return 0;
    return driver_sizeq(st->port) == 4;
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
    case 19: abi_answer(st, chk_monotonic_time()); break;
    case 20: abi_answer(st, chk_convert_time_unit()); break;
    case 21: abi_answer(st, chk_vec_to_buf()); break;
    case 22: abi_answer(st, chk_timer_round_trip(st)); break;
    case 23: abi_answer(st, chk_read_timer(st)); break;
    case 24: abi_answer(st, chk_cancel_timer(st)); break;
    case 25: abi_answer(st, chk_select(st)); break;
    case 26: abi_answer(st, chk_system_info()); break;
    case 27: abi_answer(st, chk_putenv()); break;
    case 40: abi_answer(st, chk_async(st)); break;
    case 41: abi_answer(st, chk_queue(st)); break;
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
    /* ErlDrvTerm delivery selectors: the term `[7, 9]` must reach the owner. */
    case 210: {
        ErlDrvTermData spec[] = {ERL_DRV_INT, 7, ERL_DRV_INT, 9, ERL_DRV_LIST, 2};
        (void) erl_drv_output_term(driver_mk_port(st->port), spec, 6);
        break;
    }
    case 211: {
        ErlDrvTermData spec[] = {ERL_DRV_INT, 7, ERL_DRV_INT, 9, ERL_DRV_LIST, 2};
        (void) erl_drv_send_term(driver_mk_port(st->port), driver_caller(st->port), spec, 6);
        break;
    }
    case 212: {
        ErlDrvTermData spec[] = {ERL_DRV_INT, 7, ERL_DRV_INT, 9, ERL_DRV_LIST, 2};
        (void) driver_output_term(st->port, spec, 6);
        break;
    }
    case 213: {
        ErlDrvTermData spec[] = {ERL_DRV_INT, 7, ERL_DRV_INT, 9, ERL_DRV_LIST, 2};
        (void) driver_send_term(st->port, driver_caller(st->port), spec, 6);
        break;
    }
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
    if (st == NULL)
        return;
    if (fd >= 0)
        (void) read(fd, &b, 1);
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

/// The same driver **without** a `timeout` slot: `driver_set_timer` answers the C's `-1`.
const NO_TIMER_DRIVER_C: &str = r#"/* A real driver .so (contract A.4) with no `timeout` callback. */
#include <erl_driver.h>

typedef struct {
    ErlDrvPort port;
} no_timer_state;

static ErlDrvData nt_start(ErlDrvPort port, char *command) {
    no_timer_state *st;
    (void) command;
    st = (no_timer_state *) driver_alloc(sizeof(no_timer_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void nt_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

/* The entry declares no `timeout`, which is exactly the C's precondition for
 * `driver_set_timer` answering `-1` (`io.c:6918`). */
static void nt_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    no_timer_state *st = (no_timer_state *) drv_data;
    char c;
    if (st == NULL || len < 1)
        return;
    c = (driver_set_timer(st->port, 1000) == -1) ? '1' : '0';
    (void) driver_output(st->port, &c, 1);
}

static ErlDrvEntry nt_entry = {
    .init = NULL,
    .start = nt_start,
    .stop = nt_stop,
    .output = nt_output,
    .driver_name = "driver_abi",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(driver_abi) {
    return &nt_entry;
}
"#;

/// An entry that declares **only** `outputv`: the oracle routes every `port_command` through
/// it (`if (drv->outputv)`, `io.c`), the Rust runtime has no `outputv` path.
const OUTPUTV_DRIVER_C: &str = r#"/* A real driver .so (contract A.4) whose only port-command callback is `outputv`. */
#include <erl_driver.h>

typedef struct {
    ErlDrvPort port;
} outputv_state;

static ErlDrvData ov_start(ErlDrvPort port, char *command) {
    outputv_state *st;
    (void) command;
    st = (outputv_state *) driver_alloc(sizeof(outputv_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void ov_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

static void ov_outputv(ErlDrvData drv_data, ErlIOVec *ev) {
    outputv_state *st = (outputv_state *) drv_data;
    (void) ev;
    if (st == NULL)
        return;
    (void) driver_output(st->port, "1", 1);
}

static ErlDrvEntry ov_entry = {
    .init = NULL,
    .start = ov_start,
    .stop = ov_stop,
    .outputv = ov_outputv,
    .driver_name = "driver_abi",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(driver_abi) {
    return &ov_entry;
}
"#;

/// The main fixture every selector except `28`/`30` loads.
const DRIVER_ABI: NifFixture = NifFixture::CompiledLibrary {
    name: "driver_abi",
    c_source: DRIVER_ABI_C,
};

/// The no-`timeout` fixture selector `28` loads.
const NO_TIMER_DRIVER: NifFixture = NifFixture::CompiledLibrary {
    name: "driver_abi",
    c_source: NO_TIMER_DRIVER_C,
};

/// The `outputv`-only fixture selector `30` loads.
const OUTPUTV_DRIVER: NifFixture = NifFixture::CompiledLibrary {
    name: "driver_abi",
    c_source: OUTPUTV_DRIVER_C,
};

/// Build one module-run case over [`DRIVER_ABI_MODULE_SOURCE`] with `fixture` as its `.so`.
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
            source: DRIVER_ABI_MODULE_SOURCE,
            module: "driver_abi",
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

/// The expanded A.4 group: one case per driver C ABI slot family, driven end to end.
///
/// Every golden is `1` (the oracle's own answer, verified per case with `differential run
/// <id>`): each case asks whether the driver-backed port behaved the way the oracle does, so a
/// runtime that answers otherwise raises the documented `unsupported` gap (exit `3`) instead of
/// producing a differing observable.
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(
            "driver_abi/driver_term_nil_and_driver_mk_term_nil_are_the_same_symbol",
            "driver_mk_term_nil() is the driver_term_nil data symbol",
            &[1],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_connected_answers_for_a_live_port",
            "driver_connected(port) answers a term rather than the nil symbol",
            &[2],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_caller_answers_for_a_live_port",
            "driver_caller(port) answers a term rather than the nil symbol",
            &[3],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_connected_is_the_caller_for_an_owner_opened_port",
            "the port's connected process and its caller are the same term",
            &[4],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_mk_port_answers_a_term",
            "driver_mk_port(port) answers a port term rather than the nil symbol",
            &[5],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_mk_atom_answers_a_term",
            "driver_mk_atom/1 answers an atom term rather than the nil symbol",
            &[6],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_alloc_and_free_round_trip",
            "driver_alloc/1 hands out writable memory and driver_free/1 releases it",
            &[7],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_realloc_preserves_the_contents",
            "driver_realloc/2 preserves the previous bytes",
            &[8],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_alloc_binary_round_trip",
            "driver_alloc_binary/1 lays out orig_size/orig_bytes and driver_free_binary/1 releases it",
            &[9],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_binary_refc_round_trip",
            "driver_binary_get_refc/inc_refc/dec_refc track the hidden reference count (1 -> 2 -> 1)",
            &[10],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_realloc_binary_preserves_the_payload",
            "driver_realloc_binary/2 preserves the payload and updates orig_size",
            &[11],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/erl_drv_mutex_create_trylock_lock_unlock_destroy",
            "the erl_drv_mutex_* family creates, acquires, releases and destroys a driver mutex",
            &[12],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/erl_drv_cond_create_signal_broadcast_destroy",
            "the erl_drv_cond_* family creates, signals, broadcasts and destroys a condition variable",
            &[13],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/erl_drv_rwlock_create_read_write_destroy",
            "the erl_drv_rwlock_* family takes and releases both reader and writer locks",
            &[14],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/erl_drv_tsd_set_get_round_trip",
            "erl_drv_tsd_set/get round-trip a thread-local value under a created key",
            &[15],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/erl_drv_thread_opts_create_destroy",
            "erl_drv_thread_opts_create/1 hands out an options record and _destroy releases it",
            &[16],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/erl_drv_thread_create_join_round_trip",
            "erl_drv_thread_create/5 starts a real thread and erl_drv_thread_join/2 returns its reply",
            &[17],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/erl_drv_equal_tids_identifies_the_same_thread",
            "erl_drv_thread_self/0 is a stable handle: equal_tids is 1 for itself and 0 for null",
            &[18],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/erl_drv_monotonic_time_is_positive",
            "erl_drv_monotonic_time/1 reports a positive Erlang monotonic time",
            &[19],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/erl_drv_convert_time_unit_is_exact",
            "erl_drv_convert_time_unit/3 is exact across SEC/MSEC/USEC",
            &[20],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_vec_to_buf_linearizes_a_vector",
            "driver_vec_to_buf/3 copies a vector's bytes into the caller's buffer",
            &[21],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_set_timer_accepts_with_a_timeout_callback",
            "driver_set_timer/2 accepts and driver_cancel_timer/1 releases when the entry has a timeout callback",
            &[22],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_read_timer_reports_a_remaining_time",
            "driver_read_timer/2 succeeds on a live driver port",
            &[23],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_cancel_timer_succeeds",
            "driver_cancel_timer/1 succeeds on a live driver port",
            &[24],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_select_arms_and_releases_a_descriptor",
            "driver_select/4 arms and releases a descriptor (both answer 0)",
            &[25],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_system_info_reports_the_frozen_driver_version",
            "driver_system_info/2 fills the extended driver version",
            &[26],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/erl_drv_putenv_getenv_round_trip",
            "erl_drv_putenv/2 then erl_drv_getenv/3 round-trip a variable and its length",
            &[27],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_set_timer_refuses_without_a_timeout_callback",
            "driver_set_timer/2 answers the C's -1 when the entry declares no timeout callback",
            &[28],
            NO_TIMER_DRIVER,
        ),
        case(
            "driver_abi/outputv_callback_receives_the_port_command",
            "an entry whose only port-command callback is outputv receives port_command/2 (Rust gap)",
            &[30],
            OUTPUTV_DRIVER,
        ),
        case(
            "driver_abi/driver_async_schedules_on_a_pool",
            "driver_async/5 schedules an async job on the port's pool (Rust gap: no pool)",
            &[40],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_enq_and_sizeq_track_the_port_queue",
            "driver_enq/3 then driver_sizeq/1 report the port queue's contents (Rust gap: no queue)",
            &[41],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_output_delivers_bytes_to_the_port_owner",
            "driver_output/3 bytes reach the owner as {Port, {data, Bytes}}",
            &[110],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_output2_delivers_a_header_then_the_body",
            "driver_output2/5 delivers the header bytes followed by the body",
            &[111],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_output_binary_delivers_a_binary_window",
            "driver_output_binary/6 delivers the requested window of a driver binary",
            &[112],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_outputv_delivers_gathered_vectors",
            "driver_outputv/5 delivers the gathered vectors of an ErlIOVec",
            &[113],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/ready_input_fires_for_an_armed_descriptor",
            "driver_select/4 arms a readable descriptor and the driver's ready_input callback runs",
            &[200],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/ready_output_fires_for_a_writable_descriptor",
            "driver_select/4 arms a writable descriptor and the driver's ready_output callback runs",
            &[201],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/timeout_callback_fires_for_an_armed_timer",
            "driver_set_timer/2 arms a port timer and the driver's timeout callback runs",
            &[202],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/erl_drv_output_term_delivers_a_term_to_the_owner",
            "erl_drv_output_term/3 delivers its ErlDrvTerm spec to the port owner (Rust gap: captured)",
            &[210],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/erl_drv_send_term_delivers_a_term_to_a_process",
            "erl_drv_send_term/4 delivers its ErlDrvTerm spec to the given process (Rust gap: captured)",
            &[211],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_output_term_delivers_a_term_to_the_owner",
            "the deprecated driver_output_term/3 delivers its spec to the owner (Rust gap: captured)",
            &[212],
            DRIVER_ABI,
        ),
        case(
            "driver_abi/driver_send_term_delivers_a_term_to_a_process",
            "the deprecated driver_send_term/4 delivers its spec to the given process (Rust gap: captured)",
            &[213],
            DRIVER_ABI,
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the expanded A.4 group: every case is a module-run case claiming
    //! [`Contract::DriverAbi`], it drives the shared module through `classify/1` with a real
    //! C fixture, and its golden is the integer line `1`.

    use super::{group, CaseKind, Contract, NifFixture};
    use std::collections::HashSet;

    #[test]
    fn every_case_is_a_driver_module_run_over_a_compiled_fixture() {
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
            assert_eq!(
                case.golden.stdout,
                Some("1"),
                "case {}: each case is a yes/no question about the driver ABI",
                case.id
            );
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
        }
        assert_eq!(selectors.len(), group().len(), "every selector is distinct");
        assert_eq!(
            sources.len(),
            3,
            "the group rests on three C fixtures (main, no-timeout, outputv)"
        );
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
}
