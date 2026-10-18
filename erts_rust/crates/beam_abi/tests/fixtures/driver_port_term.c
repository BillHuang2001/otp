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
 * A REAL driver shared object exercising the **`ERL_DRV_PORT` term-data operand** of the driver C
 * ABI (workspace contract A.4).
 *
 * # Why this needs real C
 *
 * The C idiom for passing a port in a term-data array is
 *
 *     ErlDrvTermData spec[] = {ERL_DRV_PORT, driver_mk_port(drv_port)};
 *     erl_drv_output_term(driver_mk_port(drv_port), spec, 2);
 *
 * i.e. the operand is whatever `driver_mk_port` answers — in the emulator the port's **id term**
 * (`prt->common.id`, `io.c:7295-7303`), which the decoder's `ERL_DRV_PORT` case then pushes as-is
 * (`mess = ptr[0]`, `io.c:5969-5972`) after checking `is_internal_port` (`io.c:5744-5747`). A port
 * term is an internal term, so only a real `.so` driven by a real driver API can show whether a
 * runtime reproduces that round trip: the encoding of `driver_mk_port`'s answer is runtime-owned.
 *
 * # Commands (all through `control`, i.e. `port_control/3`)
 *
 *  * `1` — `erl_drv_output_term(driver_mk_port(port), {ERL_DRV_PORT, driver_mk_port(port)}, 2)`:
 *    the port term must reach the port owner as the message itself.
 *  * `2` — the same spec through `erl_drv_send_term(..., driver_connected(port), ...)`: the port
 *    term must reach the connected process.
 *  * `3` — an operand that is neither a port term nor a port handle (`1`): both runtimes must
 *    refuse the spec. Note `7` would NOT do: `is_internal_port` is only `x & 0xF == 0x7`
 *    (`erl_term.h`), so the raw word `7` passes the C check and is delivered verbatim.
 *  * `4` — the port term nested inside a tuple (`{ERL_DRV_TUPLE, 2, ERL_DRV_ATOM, "tag",
 *    ERL_DRV_PORT, driver_mk_port(port)}`): the value stack must carry the resolved port term into
 *    the compound, delivering `{tag, Port}`.
 *
 * Each command publishes its own `erl_drv_*_term` return code through `driver_output` — as ASCII
 * `<label>=<code>` — because the driver's `control` reply buffer truncates, while `driver_output`
 * reaches the owner verbatim. The code is the C's `1`/`0`/`-1` (delivered / + / failed); the Rust
 * host code for "delivered" is `0` (a documented deviation with the same `< 0` predicate), so the
 * two sides' raw codes are compared against their own recorded values.
 *
 * # Portability note
 *
 * The fixture references ONLY `driver_*` / `erl_drv_*` names whose `ErlDrvApi` slots this crate
 * populates (`Some`): `driver_alloc`, `driver_free`, `driver_output`, `driver_mk_atom`,
 * `driver_mk_port`, `driver_connected`, `erl_drv_output_term`, `erl_drv_send_term`. A reference to
 * an unimplemented slot would make `dlopen(RTLD_NOW)` fail on the unresolved import.
 */
#include <erl_driver.h>

#include <stdio.h>

/* The longest report is a short label plus a code; the buffer keeps `snprintf` bounded. */
#define PT_REPORT_CAP 64

/* The command numbers (the driver-side `port_control/3` command and the test's iteration order). */
#define PT_CMD_OUTPUT 1
#define PT_CMD_SEND 2
#define PT_CMD_BAD 3
#define PT_CMD_TUPLE 4

/* Private per-port state: the port every command reports to and builds term data for. */
typedef struct {
    ErlDrvPort port;
} pt_state;

/* Publish `<label>=<code>` to the port owner through `driver_output` (never the control reply). */
static void pt_report(ErlDrvPort port, const char *label, long code) {
    char buf[PT_REPORT_CAP];
    int n = snprintf(buf, sizeof(buf), "%s=%ld", label, code);
    if (n > 0)
        (void) driver_output(port, buf, (ErlDrvSizeT) n);
}

static ErlDrvData pt_start(ErlDrvPort port, char *command) {
    pt_state *st;
    (void) command;
    st = (pt_state *) driver_alloc(sizeof(pt_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void pt_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

static ErlDrvSSizeT pt_control(ErlDrvData drv_data, unsigned int command, char *buf,
                               ErlDrvSizeT len, char **rbuf, ErlDrvSizeT rlen) {
    pt_state *st = (pt_state *) drv_data;
    ErlDrvTermData port_id;
    (void) buf;
    (void) len;
    (void) rbuf;
    (void) rlen;

    /* `driver_mk_port`'s answer -- in the emulator the port's id term (`io.c:7295-7303`), in a
     * runtime that has no id term yet the handle's own encoding. The `ERL_DRV_PORT` operand below
     * is this value either way: that is the whole point of the fixture. */
    port_id = driver_mk_port(st->port);

    switch (command) {
    case PT_CMD_OUTPUT: {
        ErlDrvTermData spec[] = {ERL_DRV_PORT, port_id};
        pt_report(st->port, "output", (long) erl_drv_output_term(port_id, spec, 2));
        break;
    }
    case PT_CMD_SEND: {
        ErlDrvTermData spec[] = {ERL_DRV_PORT, port_id};
        pt_report(st->port, "send",
                  (long) erl_drv_send_term(port_id, driver_connected(st->port), spec, 2));
        break;
    }
    case PT_CMD_BAD: {
        /* `1` is neither an internal port term (low nibble 1, not the port tag 7) nor a port
         * handle, so both runtimes refuse the spec and nothing is delivered. */
        ErlDrvTermData spec[] = {ERL_DRV_PORT, (ErlDrvTermData) 1};
        pt_report(st->port, "bad", (long) erl_drv_output_term(port_id, spec, 2));
        break;
    }
    case PT_CMD_TUPLE: {
        /* The array is scanned left to right into one value stack, so a compound's tag and arity
         * come AFTER the operands they pop (the C's `ESTACK_POP`, `io.c:5987`). */
        ErlDrvTermData spec[] = {ERL_DRV_ATOM,      driver_mk_atom("tag"),
                                 ERL_DRV_PORT,      port_id,
                                 ERL_DRV_TUPLE,     2};
        pt_report(st->port, "tuple", (long) erl_drv_output_term(port_id, spec, 6));
        break;
    }
    default:
        pt_report(st->port, "unknown", (long) command);
        break;
    }
    return 0;
}

static ErlDrvEntry pt_driver_entry = {
    .init = NULL,
    .start = pt_start,
    .stop = pt_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "portterm",
    .finish = NULL,
    .handle = NULL,
    .control = pt_control,
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

DRIVER_INIT(portterm) {
    return &pt_driver_entry;
}
