/*
 * %CopyrightBegin%
 *
 * SPDX-License-Identifier: Apache-2.0
 *
 * Copyright Ericsson AB 2025-2026. All Rights Reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 * %CopyrightEnd%
 */
/*
 * The **busy-port** driver fixture for `beam_rt`'s A.4 end-to-end `port_command` test
 * (`tests/port_busy.rs`). It is a real driver shared object, compiled with the host `cc` against
 * the host `erl_driver.h`, and `dlopen`ed like any other driver.
 *
 * `erlang:port_command/2,3`'s answer depends on the port's **busy state**, which a driver sets with
 * `set_busy_port` (`io.c:5208-5264`), and on whether the loaded **entry** advertises
 * `ERL_DRV_FLAG_SOFT_BUSY` (`erl_bif_port.c:195-198`). A driver may only be loaded under its
 * `driver_name`, so this one source is compiled twice — once with `BUSY_DRIVER_SOFT=0` and once
 * with `BUSY_DRIVER_SOFT=1` — and each build both declares its own `driver_name`/`driver_flags`
 * pair and is compiled under the matching `.so` stem:
 *
 *   `-DBUSY_DRIVER_SOFT=0` (the committed default) → `driver_name = "busy"`,     `driver_flags = 0`
 *   `-DBUSY_DRIVER_SOFT=1`                         → `driver_name = "busysoft"`, `driver_flags = ERL_DRV_FLAG_SOFT_BUSY`
 *
 * ## The callbacks
 *
 *   * `start` allocates the port's private state, answers the **start-up data message**
 *     (`driver_output` — the one `{Port, {data, _}}` a host can observe before it has sent
 *     anything, so a test can prove `start` really ran) and then marks the port **busy** with
 *     `set_busy_port(port, 1)`. The emitted payload is the C `command` tail when the port was
 *     opened with one (`{spawn_driver, "busy go"}` passes `"go"`), and the literal `"started"` when
 *     it was opened with a bare name — a port therefore *always* reports its start-up message, so
 *     a test never has to guess whether the callback ran.
 *   * `control` serves two verbs — `$B` (`'B'`, 66) sets the busy bit and `$N` (`'N'`, 78) clears
 *     it — both through `set_busy_port`, and answers the C's `0` (so `port_control/3` yields the
 *     **empty list**, never a fabricated value). Clearing the bit also resumes any process the C
 *     had suspended on the busy port (`erts_port_resume_procs`, `io.c:5261-5262`).
 *   * `output` **echoes** the bytes it is handed back through `driver_output`, so the command
 *     really reaching the driver (rather than being dropped by a busy/no-`force` arm) is
 *     observable at the port's owner.
 *   * `stop` frees the private state.
 *
 * Portability note: this fixture references ONLY `driver_*` symbols whose host slots are
 * implemented today (`driver_alloc`/`driver_free`/`driver_output`/`set_busy_port`). Referencing an
 * unimplemented slot would make `dlopen(RTLD_NOW)` fail.
 */
#include <erl_driver.h>

#include <string.h>

/* ---------------------------------------------------------------------------------------------
 * The compile-time variant
 * ---------------------------------------------------------------------------------------------
 *
 * One macro selects both halves of the variant: the `ERL_DRV_FLAG_SOFT_BUSY` bit the entry
 * declares, and the `driver_name` the loader validates the object against (a name mismatch is the
 * loader's `bad_driver_name`, so the name must follow the `.so` stem the test compiles under).
 */
#ifndef BUSY_DRIVER_SOFT
#define BUSY_DRIVER_SOFT 0
#endif

#if BUSY_DRIVER_SOFT
#define BUSY_DRIVER_NAME "busysoft"
#define BUSY_DRIVER_FLAGS ERL_DRV_FLAG_SOFT_BUSY
#else
#define BUSY_DRIVER_NAME "busy"
#define BUSY_DRIVER_FLAGS 0
#endif

/* The start-up payload a port opened by a bare driver name reports (a name without a blank has no
 * `command` tail for the emulator to hand over). */
#define BUSY_STARTUP_MESSAGE "started"

/* The two `control` verbs, as the `unsigned int` `port_control/3`'s `Operation` argument arrives
 * as. `'B'` sets the busy bit, `'N'` clears it. */
#define BUSY_CONTROL_SET ((unsigned int) 'B')
#define BUSY_CONTROL_CLEAR ((unsigned int) 'N')

/* The per-port private state: the handle the driver answers through. */
typedef struct {
    ErlDrvPort port;
} busy_state;

static ErlDrvData busy_start(ErlDrvPort port, char *command) {
    busy_state *st = (busy_state *) driver_alloc(sizeof(busy_state));
    if (st == NULL)
        return (ErlDrvData) ERL_DRV_ERROR_GENERAL;
    st->port = port;
    /* The start-up data message: the sole `{Port, {data, _}}` a host can see before it sent
     * anything, so it is the proof that this callback ran at all. */
    if (command != NULL && command[0] != '\0') {
        (void) driver_output(port, command, (ErlDrvSizeT) strlen(command));
    } else {
        (void) driver_output(port, BUSY_STARTUP_MESSAGE,
                             (ErlDrvSizeT) strlen(BUSY_STARTUP_MESSAGE));
    }
    /* A port starts busy: the busy arms of `port_command` are the ones under test. */
    set_busy_port(port, 1);
    return (ErlDrvData) st;
}

static void busy_stop(ErlDrvData data) {
    if (data != NULL)
        driver_free(data);
}

static void busy_output(ErlDrvData data, char *buf, ErlDrvSizeT len) {
    busy_state *st = (busy_state *) data;
    if (st == NULL)
        return;
    /* An unconditional echo: the bytes reaching this callback is what makes a `true` answer
     * distinguishable from a command the busy arm dropped. */
    (void) driver_output(st->port, buf, len);
}

static ErlDrvSSizeT busy_control(ErlDrvData data, unsigned int command, char *buf,
                                 ErlDrvSizeT len, char **rbuf, ErlDrvSizeT rlen) {
    busy_state *st = (busy_state *) data;
    (void) buf;
    (void) len;
    (void) rbuf;
    (void) rlen;
    if (st == NULL)
        return (ErlDrvSSizeT) -1;
    switch (command) {
    case BUSY_CONTROL_SET:
        set_busy_port(st->port, 1);
        /* `cres == 0`: `port_control/3` answers the empty list, the C's own reply shape. */
        return (ErlDrvSSizeT) 0;
    case BUSY_CONTROL_CLEAR:
        set_busy_port(st->port, 0);
        return (ErlDrvSSizeT) 0;
    default:
        /* An unrecognised verb: a negative result, which the C maps to `badarg`. */
        return (ErlDrvSSizeT) -1;
    }
}

static ErlDrvEntry busy_driver_entry = {
    .init = NULL,
    .start = busy_start,
    .stop = busy_stop,
    .output = busy_output,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = BUSY_DRIVER_NAME,
    .finish = NULL,
    .handle = NULL,
    .control = busy_control,
    .timeout = NULL,
    .outputv = NULL,
    .ready_async = NULL,
    .flush = NULL,
    .call = NULL,
    .unused_event_callback = NULL,
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
    .driver_flags = BUSY_DRIVER_FLAGS,
    .handle2 = NULL,
    .process_exit = NULL,
    .stop_select = NULL,
    .emergency_close = NULL,
};

DRIVER_INIT(busy_driver) {
    return &busy_driver_entry;
}
