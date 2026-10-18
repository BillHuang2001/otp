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
/*
 * A REAL driver shared object exercising the **port-closing family** of the driver C ABI
 * (workspace contract A.4): `driver_failure_eof`, `driver_failure_atom`, `driver_failure_posix`,
 * `driver_failure` and `driver_exit`.
 *
 * # Why this needs real C
 *
 * Every spelling funnels through the C's `driver_failure_term(ix, term, eof)`
 * (`erts/emulator/beam/io.c:7196-7229`), which unlinks the connected process, delivers the port
 * exit reason the *owner observes* and hands the port back to the emulator to be closed. None of
 * those three observations is expressible from Rust: the reason term is built inside the VM, the
 * `{'EXIT',Port,Reason}` message and the `{'DOWN',…}` monitor message are VM-internal, and the
 * closed state is the VM's port table. Only a real `.so` driven by a real driver API, on both
 * runtimes, can show whether a runtime reproduces them.
 *
 * # Commands (all through `control`, i.e. `port_control/3`)
 *
 * Each command calls exactly one C entry point and returns **its** `int` (the `control` return
 * value), so the caller observes the C function's own answer:
 *
 *  * `1` — `driver_failure_eof(port)`                        (`io.c:7278`)
 *  * `2` — `driver_failure_atom(port, "boom")`               (`io.c:7263`)
 *  * `3` — `driver_failure_posix(port, EACCES)`              (`io.c:7273`)
 *  * `4` — `driver_failure(port, 7)`                         (`io.c:7258`, a bare integer reason)
 *  * `5` — `driver_failure(port, 0)`                         (`io.c:7258`, the **eof route**)
 *  * `6` — `driver_exit(port, 0)`                            (`io.c:7234`, unlink then `normal`)
 *  * `7` — `driver_exit(port, EACCES)`                       (`io.c:7234`, unlink then `eacces`)
 *  * `8` — `driver_failure_eof((ErlDrvPort) -1)`             (`io.c:7204`, the invalid-port sentinel)
 *
 * Command 5 is deliberately *not* a second way to spell the integer `0`: the C's third argument to
 * `driver_failure_term` is the **eof flag**, and it is `code == 0`, so the reason term is discarded
 * and the owner observes `normal` — byte-for-byte what command 1 produces. Command 8 names the
 * C's `ERTS_INVALID_ERL_DRV_PORT` (`erl_port.h:60-67`, `(struct _erl_drv_port *)(SWord)-1`), the
 * one port operand the C answers `-1` for **without dereferencing it**: nothing is closed and the
 * port stays alive.
 *
 * A negative `control` return value makes `port_control/3` raise `error:badarg` (`n < 0` is the
 * C's failure idiom), which is what command 8 produces — so the probe asserts the *port's* fate,
 * never the control return, on that path.
 *
 * **One port per spelling.** A second `driver_failure_*` from the same callback on the port it just
 * terminated is a measured use-after-free in the C (SIGSEGV on ~3 of 4 runs), so the harness opens a
 * fresh port — and a fresh VM — per command and never exercises that shape.
 *
 * # Portability note
 *
 * The fixture references ONLY `driver_*` names whose `ErlDrvApi` slots this crate populates
 * (`Some`): `driver_alloc`, `driver_free`, `driver_failure_eof`, `driver_failure_atom`,
 * `driver_failure_posix`, `driver_failure`, `driver_exit`. A reference to an unimplemented slot
 * would make `dlopen(RTLD_NOW)` fail on the unresolved import.
 *
 * `output` / `ready_input` are left unset on purpose: they return `void` in erts-17.1, and the
 * family under test never needs them (`control` is the only callback the probe drives, with no
 * reply buffer).
 */
#include <erl_driver.h>
#include <errno.h>

/* The command numbers (the driver-side `port_control/3` command and the harness's iteration
 * order; kept in step with `COMMANDS` in `tests/driver_failure.rs`). */
#define DF_CMD_EOF 1
#define DF_CMD_ATOM 2
#define DF_CMD_POSIX 3
#define DF_CMD_ERROR_7 4
#define DF_CMD_ERROR_0 5
#define DF_CMD_EXIT_0 6
#define DF_CMD_EXIT_EACCES 7
#define DF_CMD_INVALID_PORT 8

/* Private per-port state: the port every command closes (or, for the sentinel, does not). */
typedef struct {
    ErlDrvPort port;
} df_state;

static ErlDrvData df_start(ErlDrvPort port, char *command) {
    df_state *st;
    (void) command;
    st = (df_state *) driver_alloc(sizeof(df_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void df_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

static ErlDrvSSizeT df_control(ErlDrvData drv_data, unsigned int command, char *buf,
                               ErlDrvSizeT len, char **rbuf, ErlDrvSizeT rlen) {
    df_state *st = (df_state *) drv_data;
    (void) buf;
    (void) len;
    (void) rbuf;
    (void) rlen;
    switch (command) {
    case DF_CMD_EOF:
        return (ErlDrvSSizeT) driver_failure_eof(st->port);
    case DF_CMD_ATOM:
        return (ErlDrvSSizeT) driver_failure_atom(st->port, "boom");
    case DF_CMD_POSIX:
        return (ErlDrvSSizeT) driver_failure_posix(st->port, EACCES);
    case DF_CMD_ERROR_7:
        return (ErlDrvSSizeT) driver_failure(st->port, 7);
    case DF_CMD_ERROR_0:
        return (ErlDrvSSizeT) driver_failure(st->port, 0);
    case DF_CMD_EXIT_0:
        return (ErlDrvSSizeT) driver_exit(st->port, 0);
    case DF_CMD_EXIT_EACCES:
        return (ErlDrvSSizeT) driver_exit(st->port, EACCES);
    case DF_CMD_INVALID_PORT:
        /* The C's `ERTS_INVALID_ERL_DRV_PORT` value; the cast goes through a pointer-sized
         * integer so it is the C's own `(SWord) -1`, not a truncated 32-bit word. */
        return (ErlDrvSSizeT) driver_failure_eof((ErlDrvPort) (ErlDrvSInt) -1);
    default:
        return 0;
    }
}

static ErlDrvEntry df_driver_entry = {
    .init = NULL,
    .start = df_start,
    .stop = df_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "drvfail",
    .finish = NULL,
    .handle = NULL,
    .control = df_control,
    .timeout = NULL,
    .outputv = NULL,
    .ready_async = NULL,
    .flush = NULL,
    .call = NULL,
    .unused_event_callback = NULL,
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
    .driver_flags = 0,
    .handle2 = NULL,
    .process_exit = NULL,
    .stop_select = NULL,
    .emergency_close = NULL,
};

DRIVER_INIT(drvfail) {
    return &df_driver_entry;
}
