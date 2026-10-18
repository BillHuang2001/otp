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
 * A REAL driver shared object exercising **`erl_errno_id/1`**'s answer of the driver C ABI
 * (workspace contract A.4).
 *
 * # Why this needs real C
 *
 * `erl_errno_id` has no documented encoding of its own: what matters is the *bytes* a driver sees,
 * and the emulator produces them through an internal table (`erl_errno_str.c`'s `errno_map` for
 * `0..max_errno`, a hash table beyond that) that no Erlang-level BIF exposes. A driver `.so` is the
 * only way to observe the answer on both runtimes through the same call, so this fixture is
 * deliberately tiny: `control` parses one decimal errno out of the request data, asks
 * `erl_errno_id` for it and publishes the answer's bytes through `driver_output` (never the control
 * reply buffer, which truncates).
 *
 * # Commands (all through `control`, i.e. `port_control/3`)
 *
 *  * `1` — the request `Data` is a decimal `int` (possibly negative): answer that errno's id.
 *  * anything else — the fixture answers `unknown` (a smoke check that the command path is live).
 *
 * The request travels as data rather than as the command number so that values outside
 * `port_control/3`'s command range (e.g. `9999`, or a negative errno) can be asked for.
 *
 * # Portability note
 *
 * The fixture references ONLY `driver_*` names whose `ErlDrvApi` slots this crate populates
 * (`Some`): `driver_alloc`, `driver_free`, `driver_output`, `erl_errno_id`. A reference to an
 * unimplemented slot would make `dlopen(RTLD_NOW)` fail on the unresolved import.
 */
#include <erl_driver.h>
#include <stdlib.h>
#include <string.h>

/* The `control` command that asks for one errno's id. */
#define EID_CMD_LOOKUP 1
/* The longest decimal `int` request the fixture accepts (a sign plus ten digits, with slack). */
#define EID_INPUT_CAP 32

typedef struct {
    ErlDrvPort port;
} eid_state;

static ErlDrvData eid_start(ErlDrvPort port, char *command) {
    eid_state *st;
    (void) command;
    st = (eid_state *) driver_alloc(sizeof(eid_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void eid_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

static ErlDrvSSizeT eid_control(ErlDrvData drv_data, unsigned int command, char *buf,
                                ErlDrvSizeT len, char **rbuf, ErlDrvSizeT rlen) {
    eid_state *st = (eid_state *) drv_data;
    char arg[EID_INPUT_CAP];
    char *name;
    (void) rbuf;
    (void) rlen;
    if (command != EID_CMD_LOOKUP) {
        (void) driver_output(st->port, "unknown", (ErlDrvSizeT) 7);
        return 0;
    }
    if (len >= (ErlDrvSizeT) sizeof(arg)) {
        (void) driver_output(st->port, "toolong", (ErlDrvSizeT) 7);
        return 0;
    }
    if (len > 0)
        (void) memcpy(&arg[0], buf, (size_t) len);
    arg[len] = '\0';
    name = erl_errno_id((int) strtol(&arg[0], NULL, 10));
    if (name == NULL) {
        (void) driver_output(st->port, "null", (ErlDrvSizeT) 4);
        return 0;
    }
    (void) driver_output(st->port, name, (ErlDrvSizeT) strlen(name));
    return 0;
}

static ErlDrvEntry eid_driver_entry = {
    .init = NULL,
    .start = eid_start,
    .stop = eid_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "errnoid",
    .finish = NULL,
    .handle = NULL,
    .control = eid_control,
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

DRIVER_INIT(errnoid) {
    return &eid_driver_entry;
}
