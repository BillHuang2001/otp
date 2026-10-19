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

//! **The `enif_select` family on real descriptors (contract A.3)** — the pin, cross-checked case by
//! case against the system `erl`.
//!
//! # Why this needs a real `.so` and the real oracle
//!
//! `enif_select`/`enif_select_x` are the `enif_*` entry points whose *whole* observable contract is a
//! side effect plus a return code: the code says what was accepted, cleared or stopped, and the proof
//! that a registration really was accepted is a message arriving when the descriptor becomes ready
//! (`erts/emulator/sys/common/erl_check_io.c:699-800` builds it, `:2136-2152` clears the interest as
//! it queues it, i.e. the select is **one-shot**). Neither half is visible to an in-process unit test,
//! so this suite drives a **real** C NIF library (compiled here with the host `cc` against the host
//! `erl_nif.h`) from an `.erl` module compiled with the host `erlc`, under **both** runtimes:
//!
//! * the built binary ([`CARGO_BIN_EXE_beam_rt`]), through the CLI's `run` action, and
//! * the system ERTS (`erl` on `$PATH`, `$ERTS_ORACLE` to override), the differential oracle.
//!
//! Both runtimes load the **same** `.so` and the same `.beam`, so the two answers compare element for
//! element. The fixture's own C/Erlang lives in the string constants below and is written into a
//! `$TMPDIR` scratch directory at run time (never committed; `beam_abi`'s `tests/fixtures/test_nif.c`
//! is a different crate's and is not touched).
//!
//! # What each case pins
//!
//! Every row is asserted on **both** runtimes, the oracle's own measured value being the reference.
//!
//! | case | what it does | pinned answer |
//! |---|---|---|
//! | read delivery | `enif_select(…, READ, …)` on a pipe read end | `code=0`, nothing before the write (`early=timeout`), then `outcome={delivered,ready_input}` |
//! | STOP | the above, then `ERL_NIF_SELECT_STOP` on the same descriptor | `stop=17` = `STOP_CALLED \| READ_CANCELLED`; nothing delivered by a later write; a second STOP answers `stop2=1` (`STOP_CALLED` alone) |
//! | CANCEL | the above, then `ERL_NIF_SELECT_CANCEL \| READ` | `cancel=16` = `READ_CANCELLED`; nothing delivered by a later write; a second CANCEL cancels nothing (`cancel2=0`) |
//! | CUSTOM_MSG | `enif_select_x(…, READ \| CUSTOM_MSG, …)` with a 3-tuple message | the caller's message **verbatim** (no `select` tuple, no event atom) |
//! | combined mask | `READ \| WRITE` on a pipe write end, and on a read end with a byte queued | `ready_output` and `ready_input` respectively — the direction that really became ready |
//! | error interest | `READ \| ERROR` on a TCP socket whose peer sent an `SO_LINGER`-RST | `code=0`, then `first={delivered,ready_input}` and `second={delivered,ready_error}`, and nothing after the two |
//! | error-only interest | `ERROR` alone on the same kind of socket | `outcome={delivered,ready_error}`, one-shot |
//! | released then ready | `arm_keep/0` (a registration minting **no** term) then `release/0`, i.e. the NIF drops its own reference, then the write | `dtor_after_release=0` (the interest's reference holds the object), `written=1`, `outcome={delivered,ready_input}`, and the object still alive in the delivered term's own hand (`final_dtor=0`, the oracle's reading too) |
//! | STOP after release | the above, then `ERL_NIF_SELECT_STOP` | `stop=17` = `STOP_CALLED \| READ_CANCELLED`, `dtor_after_stop=1` (the interest's reference was the last one, and it is given back **once**), `outcome=timeout`, `stop2=1` |
//! | owner death | a spawned owner arms and drops its own reference in one call, then exits | `dtor_after_death=1` (the liveness prune gave the interest's reference back), `outcome=timeout` |
//!
//! The **codes and the delivery** are pinned on both runtimes; the **destructor counts** are pinned
//! on this runtime only, for the reason in "The destructor's timing" below.
//!
//! # The destructor's timing, and why the counts are this side's alone
//!
//! The C runs a resource type's destructor as **deferred aux work** — `nif_resource_dtor_prologue`
//! schedules it and answers "don't free" (`erts/emulator/beam/erl_nif.c:2961-2971`), so the
//! destructor may run at any later scheduling point — while this runtime's runs **synchronously**, at
//! the release that takes the reference count to zero (the documented `beam_abi` deviation: see
//! `src/nif/enif/resources_keep.rs` → "The destructor is synchronous").
//! A destructor count read *immediately* after a release or a stop is therefore **exact here and racy
//! on the oracle**, which is why every case prints the oracle's counts (the [`run_case`] probe is
//! repeated where the count is the interesting half, so the spread is visible rather than sampled
//! once) and asserts them only on this runtime.
//!
//! # What this suite does not pin
//!
//! The feature's own limits, one line each:
//!
//! * The resource type's `stop` callback is never invoked by this runtime, so
//!   `ERL_NIF_SELECT_STOP_SCHEDULED` is never emitted.
//! * `ERL_NIF_SELECT_NOTSUP` is unreachable by construction.
//! * The delivered `{select, ResourceTerm, Ref, Event}` matches the arming `Ref` because a copy keeps a
//!   reference's id — a term identity, not a pointer identity (the fixture's `delivered` clause binds
//!   `Ref` in the head, and a mismatched reference would report `{delivered_other_ref, Event}`).
//!
//! # Skip policy
//!
//! Four host facilities are needed: a C compiler (`$CC` or `cc`), the host `erl_nif.h`, `erlc`, and —
//! for the oracle half — `erl`. A missing one prints a **loud** banner and returns; it never passes
//! silently. Every probe is bounded (`support::run_bounded`/`support::oracle_eval`), so an oracle
//! that never shuts down cannot hang the suite.
//!
//! # A note on the shared helpers
//!
//! [`erl_include_dir`]/[`cc`]/[`cc_available`] mirror `tests/nif.rs`'s (resolving the header is a
//! filesystem question, never "run `erl` and see"); they are private to this file so the suite stays
//! self-contained.
//!
//! # Why this file is long
//!
//! It is one contract's whole surface (A.3's `enif_select`) in one place: the fixture's C and Erlang
//! are **string constants** in this file (a `.c`/`.erl` on disk would be a committed artifact the two
//! runtimes would have to agree to rebuild), so the C body and the caller module's clauses each take
//! a few hundred lines that no shared helper could shrink, and then one `#[test]` per case carries
//! the pinning itself. Splitting it would mean a second copy of the toolchain discovery and of
//! [`build_fixture`], which is worse than the length.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_abi::common::{
    ERL_NIF_SELECT_ERROR, ERL_NIF_SELECT_ERROR_CANCELLED, ERL_NIF_SELECT_FAILED,
    ERL_NIF_SELECT_INVALID_EVENT, ERL_NIF_SELECT_NOTSUP, ERL_NIF_SELECT_READ,
    ERL_NIF_SELECT_READ_CANCELLED, ERL_NIF_SELECT_STOP_CALLED, ERL_NIF_SELECT_STOP_SCHEDULED,
    ERL_NIF_SELECT_WRITE, ERL_NIF_SELECT_WRITE_CANCELLED,
};

mod support;
use support::{find_on_path, oracle_eval, require_oracle, run_bounded, skip_loudly};

/// The built runtime — the `run` action is what drives the fixture.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The NIF library (written at run time, compiled with `cc -shared -fPIC`).
///
/// Every `arm_*` export creates a **real** descriptor, wraps it in a resource of the library's own
/// resource type, registers poll interest through `enif_select`/`enif_select_x` with `pid == NULL`
/// (the calling process) and answers `{RawReturnCode, ResourceTerm, RefTerm}`. `poke/0` writes the
/// one byte that makes a pipe's read end readable and answers the byte count; `stop/0` and `cancel/0`
/// address the descriptor the last `arm_*` armed.
///
/// **Why `poke/0` takes no resource term.** A resource term is resolved by `enif_get_resource`
/// through the *term word* the minting call produced: `beam_abi` keys its resource registry by that
/// word, while a NIF result is copied out onto the process heap by this runtime, so the word the
/// Erlang side holds is not the word the registry knows. That is a **separate, already-documented**
/// structural property of the ABI layer, and it must not contaminate this pin: remembering the
/// descriptor and the object pointer in the fixture keeps the identical fixture valid on both
/// runtimes. The registrations themselves name a **genuine** resource of the library's own type
/// (`obj`), exactly as the ABI (and the C) require.
///
/// **Why the `arm_*` cases never release.** A `STOP`, and the C's own `state->driver.stop.resource`,
/// address the object by pointer, so those `arm_*` exports keep the allocator's one reference for the
/// whole (short-lived) run instead of releasing it — a deliberate, documented leak of one small block.
/// The released-then-ready family (`arm_keep/0` + `release/0`, see the Rust-side docs below) is what
/// exercises the release, and it needs its own arming shape to do so safely: the second `STOP` of
/// `stop_after_release_case/1` names the descriptor with a **sentinel** object, because the C's
/// `enif_select` dereferences `DATA_TO_RESOURCE(obj)` before it looks the descriptor up and the
/// interest's object is already destroyed by then.
/// The resource type is also opened with a `stop` callback
/// (`enif_open_resource_type_x`): the C's `ERL_NIF_SELECT_STOP` path calls `erts_resource_stop`
/// unconditionally (`erl_check_io.c:1530`), so a library without one would crash the oracle rather
/// than measure anything.
///
/// **Why `arm_keep/0` mints no resource term.** `enif_make_resource` takes a reference the *term*
/// owns, and this runtime drops a dead resource term's reference at the ordinal park-arm prune
/// (contract A.3's term-release path, `src/context/gc/resources.rs`) — on its own schedule. A
/// destructor count measured in the same call would then be a race against that prune, while the
/// count this suite wants to pin is exactly "allocator(1) + interest keep(1)": with `arm_keep/0`'s
/// plain `enif_select` and no mint, `release/0` leaves precisely the interest's reference and the
/// object can only die at a disarm. The term-minting arming shape is already covered by the
/// `arm/0` cases above.
const SELECT_NIF_C: &str = r#"
#include <erl_nif.h>
#include <unistd.h>
#include <string.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>

static ErlNifResourceType* pipe_type = NULL;

typedef struct { int rd; int wr; } pipe_pair;

/* The descriptor and object the last arm_* registered (see this file's Rust-side docs for why the
   fixture remembers them rather than carrying them in the resource term). */
static int armed_rd = -1;
static int armed_wr = -1;
static void* armed_obj = NULL;

/* How many times pipe_dtor has run in this operating-system process. Every case runs in its own
   process, so the count starts at zero in each probe. */
static int dtor_count = 0;

static void pipe_dtor(ErlNifEnv* env, void* obj) {
    pipe_pair* p = (pipe_pair*) obj;
    (void) env;
    dtor_count++;
    if (armed_obj == obj) armed_obj = NULL;
    if (p->rd >= 0) { close(p->rd); p->rd = -1; }
    if (p->wr >= 0) {
        if (armed_wr == p->wr) armed_wr = -1;
        close(p->wr);
        p->wr = -1;
    }
}

/* The ERL_NIF_SELECT_STOP callback. The oracle calls it from the STOP path, so it must exist for the
   oracle to measure anything; what it does is the library's business. This runtime's STOP path does
   not invoke it (see the module docs' "What this suite does not pin"), so nothing here depends on
   either answer. */
static void pipe_stop(ErlNifEnv* env, void* obj, ErlNifEvent event, int is_direct_call) {
    (void) env; (void) obj; (void) event; (void) is_direct_call;
}

/* A fresh pipe in a fresh resource, remembered as the descriptor stop/0 and cancel/0 address. */
static pipe_pair* new_pair(void) {
    int fds[2];
    pipe_pair* p;
    if (pipe(fds) != 0) return NULL;
    p = (pipe_pair*) enif_alloc_resource(pipe_type, sizeof(pipe_pair));
    if (p == NULL) { close(fds[0]); close(fds[1]); return NULL; }
    p->rd = fds[0];
    p->wr = fds[1];
    armed_rd = p->rd;
    armed_wr = p->wr;
    armed_obj = (void*) p;
    return p;
}

static ERL_NIF_TERM answer(ErlNifEnv* env, int code, pipe_pair* p, ERL_NIF_TERM ref) {
    ERL_NIF_TERM terms[3];
    terms[0] = enif_make_int(env, code);
    terms[1] = enif_make_resource(env, p);
    terms[2] = ref;
    return enif_make_tuple_from_array(env, terms, 3);
}

/* The two-term answer {RawReturnCode, Ref} of the arm_*_keep exports, which mint no resource term
   (see this file's "Why arm_keep/0 mints no resource term"). */
static ERL_NIF_TERM keep_answer(ErlNifEnv* env, int code, ERL_NIF_TERM ref) {
    ERL_NIF_TERM terms[2];
    terms[0] = enif_make_int(env, code);
    terms[1] = ref;
    return enif_make_tuple_from_array(env, terms, 2);
}

/* A live resource of the library's own type, allocated on first use and never released. A STOP names
   its object as the stop callback's operand and the C dereferences it (DATA_TO_RESOURCE(obj),
   erl_check_io.c:1245), so a call addressing a descriptor whose interest object is already destroyed
   must name something alive rather than the freed block. */
static void* sentinel_obj(void) {
    static void* sentinel = NULL;
    if (sentinel == NULL) {
        pipe_pair* p = (pipe_pair*) enif_alloc_resource(pipe_type, sizeof(pipe_pair));
        if (p != NULL) {
            p->rd = -1;
            p->wr = -1;
            sentinel = (void*) p;
        }
    }
    return sentinel;
}

/* The object a STOP/CANCEL addresses: the interest's own while it is alive, else the sentinel. */
static void* addressable_obj(void) {
    if (armed_obj != NULL) return armed_obj;
    return sentinel_obj();
}
/* One connected loopback TCP pair whose client has been closed with an SO_LINGER {1,0} reset, and
   the accepted socket in a fresh resource. The reset is what an ERROR interest is about. */
static pipe_pair* new_reset_pair(int* accepted) {
    int ls, cs, as;
    struct sockaddr_in addr;
    socklen_t len = sizeof(addr);
    struct linger lg;
    pipe_pair* p;
    ls = socket(AF_INET, SOCK_STREAM, 0);
    if (ls < 0) return NULL;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = 0;
    if (bind(ls, (struct sockaddr*) &addr, sizeof(addr)) != 0) { close(ls); return NULL; }
    if (listen(ls, 1) != 0) { close(ls); return NULL; }
    if (getsockname(ls, (struct sockaddr*) &addr, &len) != 0) { close(ls); return NULL; }
    cs = socket(AF_INET, SOCK_STREAM, 0);
    if (cs < 0) { close(ls); return NULL; }
    if (connect(cs, (struct sockaddr*) &addr, sizeof(addr)) != 0) { close(cs); close(ls); return NULL; }
    as = accept(ls, NULL, NULL);
    close(ls);
    if (as < 0) { close(cs); return NULL; }
    lg.l_onoff = 1;
    lg.l_linger = 0;
    (void) setsockopt(cs, SOL_SOCKET, SO_LINGER, &lg, sizeof(lg));
    close(cs);
    usleep(50000);   /* let the reset land before the interest is armed */
    p = (pipe_pair*) enif_alloc_resource(pipe_type, sizeof(pipe_pair));
    if (p == NULL) { close(as); return NULL; }
    p->rd = as;
    p->wr = -1;
    armed_rd = as;
    armed_wr = -1;
    armed_obj = (void*) p;
    *accepted = as;
    return p;
}

/* arm/0: ERL_NIF_SELECT_READ on the read end of a fresh pipe, plain (non-custom) message. */
static ERL_NIF_TERM arm(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    pipe_pair* p;
    ERL_NIF_TERM ref;
    int code;
    (void) argc; (void) argv;
    p = new_pair();
    if (p == NULL) return enif_make_atom(env, "pipe_failed");
    ref = enif_make_ref(env);
    code = enif_select(env, (ErlNifEvent) p->rd, ERL_NIF_SELECT_READ, (void*) p, NULL, ref);
    return answer(env, code, p, ref);
}

/* arm_custom/0: READ | CUSTOM_MSG with the distinctive message {custom, Ref, payload}. */
static ERL_NIF_TERM arm_custom(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    pipe_pair* p;
    ERL_NIF_TERM ref, msg, parts[3];
    int code;
    (void) argc; (void) argv;
    p = new_pair();
    if (p == NULL) return enif_make_atom(env, "pipe_failed");
    ref = enif_make_ref(env);
    parts[0] = enif_make_atom(env, "custom");
    parts[1] = ref;
    parts[2] = enif_make_atom(env, "payload");
    msg = enif_make_tuple_from_array(env, parts, 3);
    code = enif_select_x(env, (ErlNifEvent) p->rd,
                         ERL_NIF_SELECT_READ | ERL_NIF_SELECT_CUSTOM_MSG,
                         (void*) p, NULL, msg, NULL);
    return answer(env, code, p, ref);
}

/* arm_write/0: READ | WRITE on the WRITE end of a fresh pipe — immediately writable. */
static ERL_NIF_TERM arm_write(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    pipe_pair* p;
    ERL_NIF_TERM ref;
    int code;
    (void) argc; (void) argv;
    p = new_pair();
    if (p == NULL) return enif_make_atom(env, "pipe_failed");
    ref = enif_make_ref(env);
    code = enif_select(env, (ErlNifEvent) p->wr,
                       ERL_NIF_SELECT_READ | ERL_NIF_SELECT_WRITE,
                       (void*) p, NULL, ref);
    return answer(env, code, p, ref);
}

/* arm_read_both/0: READ | WRITE on the READ end of a pipe that already holds a byte. */
static ERL_NIF_TERM arm_read_both(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    pipe_pair* p;
    ERL_NIF_TERM ref;
    ssize_t n;
    int code;
    (void) argc; (void) argv;
    p = new_pair();
    if (p == NULL) return enif_make_atom(env, "pipe_failed");
    n = write(p->wr, "x", 1);
    if (n != 1) return enif_make_atom(env, "write_failed");
    ref = enif_make_ref(env);
    code = enif_select(env, (ErlNifEvent) p->rd,
                       ERL_NIF_SELECT_READ | ERL_NIF_SELECT_WRITE,
                       (void*) p, NULL, ref);
    return answer(env, code, p, ref);
}

/* arm_error/0: READ | ERROR on a reset socket. */
static ERL_NIF_TERM arm_error(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    pipe_pair* p;
    ERL_NIF_TERM ref;
    int code, as = -1;
    (void) argc; (void) argv;
    p = new_reset_pair(&as);
    if (p == NULL) return enif_make_atom(env, "socket_failed");
    ref = enif_make_ref(env);
    code = enif_select(env, (ErlNifEvent) as,
                       ERL_NIF_SELECT_READ | ERL_NIF_SELECT_ERROR,
                       (void*) p, NULL, ref);
    return answer(env, code, p, ref);
}

/* arm_error_only/0: ERROR alone on a reset socket. */
static ERL_NIF_TERM arm_error_only(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    pipe_pair* p;
    ERL_NIF_TERM ref;
    int code, as = -1;
    (void) argc; (void) argv;
    p = new_reset_pair(&as);
    if (p == NULL) return enif_make_atom(env, "socket_failed");
    ref = enif_make_ref(env);
    code = enif_select(env, (ErlNifEvent) as, ERL_NIF_SELECT_ERROR, (void*) p, NULL, ref);
    return answer(env, code, p, ref);
}

/* stop/0: ERL_NIF_SELECT_STOP on the descriptor the last arm_* registered. */
static ERL_NIF_TERM stop(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM ref;
    void* obj;
    (void) argc; (void) argv;
    if (armed_rd < 0) return enif_make_atom(env, "nothing_armed");
    obj = addressable_obj();
    if (obj == NULL) return enif_make_atom(env, "nothing_armed");
    ref = enif_make_ref(env);
    return enif_make_int(env, enif_select(env, (ErlNifEvent) armed_rd,
                                          ERL_NIF_SELECT_STOP, obj, NULL, ref));
}

/* cancel/0: ERL_NIF_SELECT_CANCEL | ERL_NIF_SELECT_READ on the same descriptor. */
static ERL_NIF_TERM cancel(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM ref;
    void* obj;
    (void) argc; (void) argv;
    if (armed_rd < 0) return enif_make_atom(env, "nothing_armed");
    obj = addressable_obj();
    if (obj == NULL) return enif_make_atom(env, "nothing_armed");
    ref = enif_make_ref(env);
    return enif_make_int(env, enif_select(env, (ErlNifEvent) armed_rd,
                                          ERL_NIF_SELECT_CANCEL | ERL_NIF_SELECT_READ,
                                          obj, NULL, ref));
}

/* poke/0: the one byte that makes the pipe's read end readable. */
static ERL_NIF_TERM poke(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ssize_t n;
    (void) argc; (void) argv;
    if (armed_wr < 0) return enif_make_atom(env, "no_pipe");
    n = write(armed_wr, "x", 1);
    if (n < 0) return enif_make_atom(env, "write_failed");
    return enif_make_int(env, (int) n);
}

/* arm_keep/0: ERL_NIF_SELECT_READ on a fresh pipe, answering {Code, Ref} and minting no term. */
static ERL_NIF_TERM arm_keep(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    pipe_pair* p;
    ERL_NIF_TERM ref;
    int code;
    (void) argc; (void) argv;
    p = new_pair();
    if (p == NULL) return enif_make_atom(env, "pipe_failed");
    ref = enif_make_ref(env);
    code = enif_select(env, (ErlNifEvent) p->rd, ERL_NIF_SELECT_READ, (void*) p, NULL, ref);
    return keep_answer(env, code, ref);
}

/* arm_keep_release/0: arm_keep/0's body followed by the NIF's own release of the object it
   allocated, so one call arms an interest and drops the caller's own reference (the owner-death
   case's shape). */
static ERL_NIF_TERM arm_keep_release(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    pipe_pair* p;
    ERL_NIF_TERM ref;
    int code;
    (void) argc; (void) argv;
    p = new_pair();
    if (p == NULL) return enif_make_atom(env, "pipe_failed");
    ref = enif_make_ref(env);
    code = enif_select(env, (ErlNifEvent) p->rd, ERL_NIF_SELECT_READ, (void*) p, NULL, ref);
    enif_release_resource((void*) p);
    return keep_answer(env, code, ref);
}

/* release/0: the NIF drops its OWN reference on the object it allocated — the C's
   enif_release_resource (erl_nif.c:3170) — leaving only whatever the registration holds. */
static ERL_NIF_TERM release(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    if (armed_obj == NULL) return enif_make_atom(env, "nothing_armed");
    enif_release_resource(armed_obj);
    return enif_make_atom(env, "released");
}

/* dtor/0: how many times pipe_dtor has run in this operating-system process. */
static ERL_NIF_TERM dtor(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    return enif_make_int(env, dtor_count);
}
static int load(ErlNifEnv* env, void** priv, ERL_NIF_TERM info) {
    ErlNifResourceFlags tried;
    ErlNifResourceTypeInit init;
    (void) info;
    if (pipe_type == NULL) {
        memset(&init, 0, sizeof(init));
        init.dtor = pipe_dtor;
        init.stop = pipe_stop;
        init.down = NULL;
        init.members = 3;
        pipe_type = enif_open_resource_type_x(env, "select_pipe", &init,
                                              ERL_NIF_RT_CREATE, &tried);
        if (pipe_type == NULL) return 1;
    }
    *priv = NULL;
    return 0;
}

static ErlNifFunc select_nif_funcs[] = {
    {"arm", 0, arm, 0},
    {"arm_custom", 0, arm_custom, 0},
    {"arm_write", 0, arm_write, 0},
    {"arm_read_both", 0, arm_read_both, 0},
    {"arm_error", 0, arm_error, 0},
    {"arm_error_only", 0, arm_error_only, 0},
    {"arm_keep", 0, arm_keep, 0},
    {"arm_keep_release", 0, arm_keep_release, 0},
    {"release", 0, release, 0},
    {"dtor", 0, dtor, 0},
    {"stop", 0, stop, 0},
    {"cancel", 0, cancel, 0},
    {"poke", 0, poke, 0}
};

ERL_NIF_INIT(select_nif, select_nif_funcs, load, NULL, NULL, NULL)
"#;

/// The NIF module: `init/1` loads the library, the rest replace their stubs.
const SELECT_NIF_ERL: &str = r#"%% Generated by `beam_rt/tests/nif_select.rs` (never committed).
-module(select_nif).
-export([init/1, arm/0, arm_custom/0, arm_write/0, arm_read_both/0,
         arm_error/0, arm_error_only/0, arm_keep/0, arm_keep_release/0,
         release/0, dtor/0, stop/0, cancel/0, poke/0]).
init(Path) -> erlang:load_nif(Path, 0).
arm() -> erlang:nif_error(not_loaded).
arm_custom() -> erlang:nif_error(not_loaded).
arm_write() -> erlang:nif_error(not_loaded).
arm_read_both() -> erlang:nif_error(not_loaded).
arm_error() -> erlang:nif_error(not_loaded).
arm_error_only() -> erlang:nif_error(not_loaded).
arm_keep() -> erlang:nif_error(not_loaded).
arm_keep_release() -> erlang:nif_error(not_loaded).
release() -> erlang:nif_error(not_loaded).
dtor() -> erlang:nif_error(not_loaded).
stop() -> erlang:nif_error(not_loaded).
cancel() -> erlang:nif_error(not_loaded).
poke() -> erlang:nif_error(not_loaded).
"#;

/// The caller: one export per case, each printing its own `key=value` lines.
///
/// `wait_for_select/1` distinguishes the three answers a report needs: `{delivered, Event}` when the
/// 4-tuple arrived carrying **the very reference** the arming call passed (`Ref` is bound in the head,
/// so this clause only matches a term-equal reference), `{delivered_other_ref, Event}` when a
/// `{select, _, _, Event}` tuple arrived for a different reference, and `timeout` when nothing
/// arrived. `wait_early/1` is the "nothing before readiness" phase: a **short** bounded receive that
/// must not match before the descriptor is poked. `wait_next/1` is one more bounded receive, for the
/// cases that pin what does (and does not) follow the first notification.
///
/// `kept_case/1` is the released-then-ready shape: `arm_keep/0` registers an interest without minting
/// a term, `release/0` drops the NIF's own reference, and the descriptor's readiness must still be
/// delivered because the **interest** holds a reference of its own. Its `final_dtor` reading, taken
/// after the delivered notification has been received and one further bounded park, is the object's
/// state once the term is what holds it: the delivered term took a reference of its own, so the
/// destructor has **not** run — the same reading the oracle reports.
/// `stop_after_release_case/1` is the same arming with a `STOP` instead of a delivery: the disarm
/// gives the interest's reference back exactly once, so the object dies there and a second `STOP`
/// finds the descriptor already gone. `owner_death_case/1` arms and releases *inside a spawned
/// process* and lets that process exit, so the interest's reference is given back by the runtime's
/// liveness prune instead of by a delivery or a stop.
///
/// Every export reaches the NIF through an **external** call (`select_caller` is a different module
/// from `select_nif`), which is the dispatch point this runtime intercepts for NIFs.
const SELECT_CALLER_ERL: &str = r#"%% Generated by `beam_rt/tests/nif_select.rs` (never committed).
-module(select_caller).
-export([deliver/1, stop_case/1, cancel_case/1, custom_case/1,
         combined_write/1, combined_read/1, error_only_case/1, error_case/1,
         kept_case/1, stop_after_release_case/1, owner_death_case/1]).
wait_early(Ref) ->
    receive
        {select, _Resource, Ref, _Event} -> delivered_early
    after 100 ->
        timeout
    end.
wait_for_select(Ref) ->
    receive
        {select, _Resource, Ref, Event} -> {delivered, Event};
        {select, _Resource, _OtherRef, Event} -> {delivered_other_ref, Event};
        Other -> {unexpected, Other}
    after 1000 ->
        timeout
    end.
deliver(Path) ->
    ok = select_nif:init(Path),
    {Code, _Resource, Ref} = select_nif:arm(),
    io:format("code=~w~n", [Code]),
    io:format("early=~w~n", [wait_early(Ref)]),
    io:format("written=~w~n", [select_nif:poke()]),
    io:format("outcome=~w~n", [wait_for_select(Ref)]),
    ok.
stop_case(Path) ->
    ok = select_nif:init(Path),
    {Code, _Resource, Ref} = select_nif:arm(),
    io:format("code=~w~n", [Code]),
    io:format("stop=~w~n", [select_nif:stop()]),
    io:format("written=~w~n", [select_nif:poke()]),
    io:format("outcome=~w~n", [wait_for_select(Ref)]),
    io:format("stop2=~w~n", [select_nif:stop()]),
    ok.
cancel_case(Path) ->
    ok = select_nif:init(Path),
    {Code, _Resource, Ref} = select_nif:arm(),
    io:format("code=~w~n", [Code]),
    io:format("cancel=~w~n", [select_nif:cancel()]),
    io:format("written=~w~n", [select_nif:poke()]),
    io:format("outcome=~w~n", [wait_for_select(Ref)]),
    io:format("cancel2=~w~n", [select_nif:cancel()]),
    ok.
custom_case(Path) ->
    ok = select_nif:init(Path),
    {Code, _Resource, Ref} = select_nif:arm_custom(),
    io:format("code=~w~n", [Code]),
    io:format("written=~w~n", [select_nif:poke()]),
    io:format("outcome=~w~n", [wait_custom(Ref)]),
    ok.
wait_custom(Ref) ->
    receive
        {custom, Ref, payload} -> custom_delivered;
        Other -> {unexpected, Other}
    after 1000 ->
        timeout
    end.
combined_write(Path) ->
    ok = select_nif:init(Path),
    {Code, _Resource, Ref} = select_nif:arm_write(),
    io:format("code=~w~n", [Code]),
    io:format("outcome=~w~n", [wait_for_select(Ref)]),
    ok.
combined_read(Path) ->
    ok = select_nif:init(Path),
    {Code, _Resource, Ref} = select_nif:arm_read_both(),
    io:format("code=~w~n", [Code]),
    io:format("outcome=~w~n", [wait_for_select(Ref)]),
    ok.
error_only_case(Path) ->
    ok = select_nif:init(Path),
    {Code, _Resource, Ref} = select_nif:arm_error_only(),
    io:format("code=~w~n", [Code]),
    io:format("outcome=~w~n", [wait_for_select(Ref)]),
    io:format("second=~w~n", [wait_next(Ref)]),
    ok.
error_case(Path) ->
    ok = select_nif:init(Path),
    {Code, _Resource, Ref} = select_nif:arm_error(),
    io:format("code=~w~n", [Code]),
    io:format("outcome=~w~n", [wait_for_select(Ref)]),
    io:format("second=~w~n", [wait_next(Ref)]),
    io:format("third=~w~n", [wait_next(Ref)]),
    ok.
wait_next(Ref) ->
    receive
        {select, _Resource, Ref, Event} -> {delivered, Event};
        Other -> {unexpected, Other}
    after 300 ->
        timeout
    end.
kept_case(Path) ->
    ok = select_nif:init(Path),
    {Code, Ref} = select_nif:arm_keep(),
    io:format("code=~w~n", [Code]),
    io:format("release=~w~n", [select_nif:release()]),
    io:format("dtor_after_release=~w~n", [select_nif:dtor()]),
    io:format("early=~w~n", [wait_early(Ref)]),
    io:format("written=~w~n", [select_nif:poke()]),
    io:format("outcome=~w~n", [wait_for_select(Ref)]),
    receive after 100 -> ok end,
    io:format("final_dtor=~w~n", [select_nif:dtor()]),
    ok.
stop_after_release_case(Path) ->
    ok = select_nif:init(Path),
    {Code, Ref} = select_nif:arm_keep(),
    io:format("code=~w~n", [Code]),
    io:format("release=~w~n", [select_nif:release()]),
    io:format("dtor_after_release=~w~n", [select_nif:dtor()]),
    io:format("stop=~w~n", [select_nif:stop()]),
    io:format("dtor_after_stop=~w~n", [select_nif:dtor()]),
    io:format("outcome=~w~n", [wait_for_select(Ref)]),
    io:format("stop2=~w~n", [select_nif:stop()]),
    ok.
owner_death_case(Path) ->
    ok = select_nif:init(Path),
    Parent = self(),
    _Child = spawn(fun() ->
                           {ChildCode, ChildRef} = select_nif:arm_keep_release(),
                           Parent ! {armed, ChildCode, ChildRef}
                   end),
    {Code, Ref} = await_armed(),
    io:format("code=~w~n", [Code]),
    io:format("dtor_after_arm=~w~n", [select_nif:dtor()]),
    receive after 300 -> ok end,
    io:format("dtor_after_death=~w~n", [select_nif:dtor()]),
    io:format("outcome=~w~n", [wait_for_select(Ref)]),
    ok.
await_armed() ->
    receive
        {armed, Code, Ref} -> {Code, Ref}
    after 1000 ->
        {no_answer, no_ref}
    end.
"#;

// ---------------------------------------------------------------------------------------------
// Toolchain discovery (filesystem only — never executes the tool)
// ---------------------------------------------------------------------------------------------

/// The header the include-directory search keys on.
const HEADER: &str = "erl_nif.h";

/// True when `dir` holds a readable `erl_nif.h`.
fn has_header(dir: &Path) -> bool {
    dir.join(HEADER).is_file()
}

/// Canonicalize the `erl` on `$PATH` and take the sibling `include` of its `erts-*` directory.
fn from_path_erl() -> Option<PathBuf> {
    let erl = find_on_path("erl")?.canonicalize().ok()?;
    let bin = erl.parent()?;
    let erts = bin.parent()?;
    if bin.file_name()? != "bin" || !erts.file_name()?.to_string_lossy().starts_with("erts-") {
        return None;
    }
    let include = erts.join("include");
    has_header(&include).then_some(include)
}

/// The standard installation roots to probe, `$ERL_TOP` first.
fn standard_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(top) = std::env::var_os("ERL_TOP") {
        roots.push(PathBuf::from(top));
    }
    for base in [
        "/usr/lib/erlang",
        "/usr/local/lib/erlang",
        "/usr/lib64/erlang",
        "/opt/erlang",
    ] {
        roots.push(PathBuf::from(base));
    }
    roots
}

/// Search one installation root for an include directory holding `erl_nif.h`.
fn search_root(root: &Path) -> Option<PathBuf> {
    let direct = root.join("include");
    if has_header(&direct) {
        return Some(direct);
    }
    let lib_erlang = root.join("lib").join("erlang");
    for base in [root.to_path_buf(), lib_erlang.clone()] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("erts-") {
                continue;
            }
            let include = entry.path().join("include");
            if has_header(&include) {
                return Some(include);
            }
        }
    }
    let include = lib_erlang.join("include");
    has_header(&include).then_some(include)
}

/// Locate the host `erl_nif.h` include directory **without running any Erlang program**: the sibling
/// `include` of the `erts-*` tree the `erl` on `$PATH` lives in, else a standard installation root.
fn erl_include_dir() -> Option<PathBuf> {
    from_path_erl().or_else(|| standard_roots().into_iter().find_map(|root| search_root(&root)))
}

/// The C compiler to use: `$CC` when set, else `cc`.
fn cc() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}

/// Whether a C compiler is available.
fn cc_available() -> bool {
    Command::new(cc())
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Print a loud skip banner (never a silent pass).
fn skip(reason: &str) {
    println!("================================================================");
    println!("SKIP: {reason}");
    println!("================================================================");
}

// ---------------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------------

/// A compiled NIF library + its caller, in one scratch directory.
struct Fixture {
    /// The scratch directory holding the sources and the compiled artifacts.
    dir: PathBuf,
    /// The compiled `select_caller.beam` (the `run` file argument).
    caller: PathBuf,
}

impl Fixture {
    /// Removes the scratch directory (`cc`/`erlc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }

    /// The path handed to `erlang:load_nif/2` (the `.so` **without** its suffix — the loader appends
    /// `.so`).
    fn load_path(&self) -> PathBuf {
        self.dir.join("select_nif")
    }
}

/// Compiles the `select_nif` fixture, or `None` when a host facility is missing (the caller then
/// returns early — **loudly**).
fn build_fixture(label: &str) -> Option<Fixture> {
    let Some(include) = erl_include_dir() else {
        skip(&format!(
            "nif_select/{label}: no erl_nif.h include directory found"
        ));
        return None;
    };
    if !cc_available() {
        skip(&format!(
            "nif_select/{label}: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    if find_on_path("erlc").is_none() {
        skip(&format!(
            "nif_select/{label}: no `erlc` on $PATH to compile the fixture module"
        ));
        return None;
    }
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_nif_select_{label}_{}_{n}",
        std::process::id()
    ));
    let c_src = dir.join("select_nif.c");
    if let Err(error) =
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&c_src, SELECT_NIF_C))
    {
        skip(&format!(
            "nif_select/{label}: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    let mut erl_paths = Vec::new();
    for (stem, source) in [
        ("select_nif", SELECT_NIF_ERL),
        ("select_caller", SELECT_CALLER_ERL),
    ] {
        let path = dir.join(format!("{stem}.erl"));
        if let Err(error) = std::fs::write(&path, source) {
            skip(&format!(
                "nif_select/{label}: cannot write {}: {error}",
                path.display()
            ));
            return None;
        }
        erl_paths.push(path);
    }
    // The shared object: `cc -shared -fPIC -I<include> -o <dir>/select_nif.so <dir>/select_nif.c`.
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join("select_nif.so"))
        .arg(&c_src)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "nif_select/{label}: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!(
                "nif_select/{label}: could not run the C compiler: {error}"
            ));
            return None;
        }
    }
    // The Erlang modules: `erlc -o <dir> <dir>/select_nif.erl <dir>/select_caller.erl`.
    let output = Command::new("erlc")
        .arg("-o")
        .arg(&dir)
        .args(&erl_paths)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "nif_select/{label}: erlc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!(
                "nif_select/{label}: could not run `erlc`: {error}"
            ));
            return None;
        }
    }
    let caller = dir.join("select_caller.beam");
    if !caller.is_file() {
        skip(&format!(
            "nif_select/{label}: erlc produced no {}",
            caller.display()
        ));
        return None;
    }
    Some(Fixture { dir, caller })
}

// ---------------------------------------------------------------------------------------------
// The two sides' invocations
// ---------------------------------------------------------------------------------------------

/// The `beam_rt` invocation: `-noshell -pa <dir> run <select_caller.beam> <function> "<load-path>"`,
/// bounded.
///
/// The load path is **double-quoted**: the `run` argument grammar parses a bare token as an atom and
/// only a quoted token as an Erlang char list, and `erlang:load_nif/2` requires a list or a binary.
/// The scratch path carries no `"` or backslash, so no escaping is needed.
fn invoke_rust(fixture: &Fixture, function: &str) -> Option<String> {
    let args = vec![
        "-noshell".to_string(),
        "-pa".to_string(),
        fixture.dir.display().to_string(),
        "run".to_string(),
        fixture.caller.display().to_string(),
        function.to_string(),
        format!("\"{}\"", fixture.load_path().display()),
    ];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = run_bounded(Path::new(BINARY), &args)?;
    let status = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    println!("--- beam_rt (exit {status}) ---");
    println!("stdout: {stdout}");
    println!("stderr: {stderr}");
    assert_eq!(
        status, 0,
        "the fixture must run on this runtime (exit {status}); stdout={stdout} stderr={stderr}"
    );
    Some(stdout)
}

/// The oracle invocation: the same `.so`/`.beam` under the system ERTS, bounded and self-terminating
/// (`oracle_eval` appends `-s init stop`).
///
/// `function` is the full `Mod:Fun/Arity`; the `-eval` expression needs the `Mod:Fun` half.
fn invoke_oracle(fixture: &Fixture, oracle: &Path, function: &str) -> Option<String> {
    let call = function.rsplit_once('/').map_or(function, |(name, _)| name);
    let eval = format!(
        "{call}(\"{}\"), halt(0).",
        fixture.load_path().display()
    );
    let stdout = oracle_eval(oracle, Some(&fixture.dir), &eval)?;
    println!("--- oracle ({}) ---", oracle.display());
    println!("stdout: {stdout}");
    Some(stdout)
}

/// Drives `function` on **both** runtimes over one freshly built fixture, or `None` after a loud skip
/// (a missing host facility, a missing oracle, a probe that did not finish).
fn run_case(label: &str, function: &str) -> Option<Answers> {
    let fixture = build_fixture(label)?;
    let Some(rust) = invoke_rust(&fixture, function) else {
        skip_loudly(
            "nif_select",
            &format!(
                "the built beam_rt binary did not complete {function} inside the probe bound"
            ),
        );
        fixture.cleanup();
        return None;
    };
    let Some(oracle) = require_oracle("nif_select") else {
        fixture.cleanup();
        return None;
    };
    let Some(oracle_stdout) = invoke_oracle(&fixture, &oracle, function) else {
        skip_loudly(
            "nif_select",
            "the oracle did not complete the probe (see the warning above)",
        );
        fixture.cleanup();
        return None;
    };
    fixture.cleanup();
    Some(Answers {
        rust,
        oracle: oracle_stdout,
    })
}

/// As [`run_case`], but over **one** fixture the oracle probe is repeated `probes` times, and the
/// `keys` of every run are printed — the destructor-count half of a case, whose value is not a
/// deterministic function of the program on the oracle (module docs, "The destructor's timing").
///
/// The [`Answers`] carry this runtime's one run and the oracle's **first**, so the pinned comparisons
/// read exactly as [`run_case`]'s do; the returned `Vec` holds one `key=value …` reading per oracle
/// run, the first of which is that same first run.
fn run_repeated(
    label: &str,
    function: &str,
    probes: usize,
    keys: &[&str],
) -> Option<(Answers, Vec<String>)> {
    let fixture = build_fixture(label)?;
    let Some(rust) = invoke_rust(&fixture, function) else {
        skip_loudly(
            "nif_select",
            &format!(
                "the built beam_rt binary did not complete {function} inside the probe bound"
            ),
        );
        fixture.cleanup();
        return None;
    };
    let Some(oracle) = require_oracle("nif_select") else {
        fixture.cleanup();
        return None;
    };
    let probes = probes.max(1);
    let mut outputs = Vec::with_capacity(probes);
    for _ in 0..probes {
        let Some(stdout) = invoke_oracle(&fixture, &oracle, function) else {
            skip_loudly(
                "nif_select",
                "the oracle did not complete the probe (see the warning above)",
            );
            fixture.cleanup();
            return None;
        };
        outputs.push(stdout);
    }
    fixture.cleanup();
    let readings: Vec<String> = outputs
        .iter()
        .map(|stdout| {
            keys.iter()
                .map(|key| format!("{key}={}", field(stdout, key).unwrap_or("<missing>")))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    for (n, reading) in readings.iter().enumerate() {
        println!("oracle probe {}/{}: {reading}", n + 1, probes);
    }
    let oracle_stdout = outputs.remove(0);
    Some((
        Answers {
            rust,
            oracle: oracle_stdout,
        },
        readings,
    ))
}
// ---------------------------------------------------------------------------------------------
// Reading the answers
// ---------------------------------------------------------------------------------------------

/// The value of the `key=value` line `stdout` carries, if any.
fn field<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(key)?.strip_prefix('='))
}

/// Both sides' printed output for one case, with the accessors the cases assert through.
///
/// A missing line means the fixture did not print what its own contract says it prints, so the
/// accessors report a **loud skip** naming the missing side instead of silently skipping the check.
struct Answers {
    /// The `beam_rt` side's stdout.
    rust: String,
    /// The oracle's stdout.
    oracle: String,
}

impl Answers {
    /// `key`'s value on both sides, as `(rust, oracle)`.
    fn text(&self, key: &str) -> Option<(String, String)> {
        let rust = field(&self.rust, key);
        let oracle = field(&self.oracle, key);
        match (rust, oracle) {
            (Some(rust), Some(oracle)) => {
                Some((rust.trim().to_string(), oracle.trim().to_string()))
            }
            _ => {
                skip(&format!(
                    "nif_select: the `{key}=` line is missing (beam_rt: {rust:?}, oracle: \
                     {oracle:?}); beam_rt stdout:\n{}\noracle stdout:\n{}",
                    self.rust, self.oracle
                ));
                None
            }
        }
    }

    /// `key`'s value on both sides as an integer.
    fn int(&self, key: &str) -> Option<(i32, i32)> {
        let (rust, oracle) = self.text(key)?;
        match (rust.parse::<i32>(), oracle.parse::<i32>()) {
            (Ok(rust), Ok(oracle)) => Some((rust, oracle)),
            (rust, oracle) => {
                skip(&format!(
                    "nif_select: `{key}=` is not an integer (beam_rt: {rust:?}, oracle: \
                     {oracle:?})"
                ));
                None
            }
        }
    }
}

/// A key's value on **this** runtime only, as an integer — the destructor-count half of a case, which
/// is asserted here and only printed for the oracle (module docs, "The destructor's timing").
///
/// A missing or unparsable line is a loud skip naming this side, exactly like [`Answers::text`]'s.
fn rust_int(answers: &Answers, key: &str) -> Option<i32> {
    match field(&answers.rust, key).and_then(|value| value.trim().parse::<i32>().ok()) {
        Some(value) => Some(value),
        None => {
            skip(&format!(
                "nif_select: this runtime's `{key}=` line is missing or not an integer; beam_rt \
                 stdout:\n{}",
                answers.rust
            ));
            None
        }
    }
}

/// The case's heading: its name and the one-line description of what it arms.
fn heading(case: &str, detail: &str) {    println!();
    println!("=============== nif_select :: {case} ===============");
    println!("fixture: {detail}");
}

/// The case's closing verdict, its note, and the separator.
fn verdict(agrees: bool, note: &str) {
    println!(
        "verdict : {}",
        if agrees { "AGREE" } else { "DIVERGENCE" }
    );
    println!("note    : {note}");
    println!("====================================================");
    println!();
}

/// Decode a raw `enif_select` return code into the C's documented vocabulary (`erl_nif.h:178-186`),
/// so a report says *why* a code is what it is.
///
/// The positive codes are OR-ed bits (`STOP_CALLED`/`STOP_SCHEDULED` and the three
/// `*_CANCELLED` bits); the negative ones are `INT_MIN`-based bit sets, and `-1` is `beam_abi`'s own
/// `SELECT_UNSUPPORTED` — its "not serviceable" answer for a host whose `NifHost::select` hook
/// declines (module docs of `src/nif/enif/select.rs`), which is **not** one of the C's codes.
fn decode(code: i32) -> String {
    if code == -1 {
        return "-1 — beam_abi's SELECT_UNSUPPORTED: the host's NifHost::select hook answered \
                `None` (not one of the C's codes)"
            .to_string();
    }
    let names: &[(i32, &str)] = &[
        (ERL_NIF_SELECT_STOP_CALLED, "STOP_CALLED"),
        (ERL_NIF_SELECT_STOP_SCHEDULED, "STOP_SCHEDULED"),
        (ERL_NIF_SELECT_INVALID_EVENT, "INVALID_EVENT"),
        (ERL_NIF_SELECT_FAILED, "FAILED"),
        (ERL_NIF_SELECT_READ_CANCELLED, "READ_CANCELLED"),
        (ERL_NIF_SELECT_WRITE_CANCELLED, "WRITE_CANCELLED"),
        (ERL_NIF_SELECT_ERROR_CANCELLED, "ERROR_CANCELLED"),
        (ERL_NIF_SELECT_NOTSUP, "NOTSUP"),
    ];
    let bits = if code < 0 { code & !i32::MIN } else { code };
    let mut set: Vec<&str> = names
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if set.is_empty() {
        set.push("no documented bit");
    }
    let joined = set.join(" | ");
    if code < 0 {
        format!("{code} — INT_MIN | {joined}")
    } else if code == 0 {
        "0 — the C's success answer: the registration was accepted".to_string()
    } else {
        format!("{code} — {joined}")
    }
}

// ---------------------------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------------------------

/// **A read interest on a real pipe read end delivers the C's own notification** — and nothing before
/// the descriptor is ready.
///
/// `enif_select(env, pipe_read_fd, ERL_NIF_SELECT_READ, resource, NULL, Ref)` must answer the C's `0`,
/// the short pre-write receive must find no `{select, …}` at all, and the one byte `poke/0` writes
/// must then produce exactly the C's one-shot `{select, ResourceTerm, Ref, ready_input}` — carrying
/// the very reference the arming call passed (the clause binds `Ref` in the head).
#[test]
fn enif_select_read_on_a_real_pipe_fd_delivers_the_c_notification() {
    let Some(answers) = run_case("deliver", "select_caller:deliver/1") else {
        return;
    };
    let Some((rust_code, oracle_code)) = answers.int("code") else {
        return;
    };
    let Some((rust_early, oracle_early)) = answers.text("early") else {
        return;
    };
    let Some((rust_written, oracle_written)) = answers.int("written") else {
        return;
    };
    let Some((rust_outcome, oracle_outcome)) = answers.text("outcome") else {
        return;
    };
    heading(
        "read delivery",
        &format!(
            "enif_select(env, pipe_read_fd, ERL_NIF_SELECT_READ={ERL_NIF_SELECT_READ}, resource, \
             NULL, Ref) — the same .so/.beam under both runtimes"
        ),
    );
    println!(
        "beam_rt        : code={rust_code} [{}] early={rust_early} written={rust_written} \
         outcome={rust_outcome}",
        decode(rust_code)
    );
    println!(
        "oracle         : code={oracle_code} [{}] early={oracle_early} written={oracle_written} \
         outcome={oracle_outcome}",
        decode(oracle_code)
    );
    let agrees = rust_code == oracle_code
        && rust_early == oracle_early
        && rust_written == oracle_written
        && rust_outcome == oracle_outcome;
    verdict(
        agrees,
        "the registration is accepted, stays silent until the descriptor is readable, and then \
         delivers the one-shot 4-tuple naming the arming reference",
    );

    assert_eq!(oracle_code, 0, "the oracle accepts the registration");
    assert_eq!(rust_code, 0, "this runtime accepts the registration");
    // Nothing may arrive before the write: the oracle's own answer is the reference.
    assert_eq!(oracle_early, "timeout", "the oracle delivers nothing before the write");
    assert_eq!(
        rust_early, "timeout",
        "no {{select, …}} may reach the caller before the descriptor is readable"
    );
    assert_eq!(rust_written, 1, "the fixture's write into the pipe must succeed");
    assert_eq!(oracle_written, 1, "the oracle's write into the pipe must succeed");
    assert_eq!(
        oracle_outcome, "{delivered,ready_input}",
        "the oracle delivers {{select, Resource, Ref, ready_input}}"
    );
    assert_eq!(
        rust_outcome, "{delivered,ready_input}",
        "this runtime delivers {{select, Resource, Ref, ready_input}} for the armed read"
    );
}

/// **`ERL_NIF_SELECT_STOP` releases an armed descriptor** — and reports it in the C's bit vocabulary.
///
/// Two facts are pinned, identically on both runtimes. The code: the C reports the stop *and* the
/// direction it cleared on the way, so a `STOP` on an armed read interest answers `17` =
/// `ERL_NIF_SELECT_STOP_CALLED (1) | ERL_NIF_SELECT_READ_CANCELLED (16)` (`erl_check_io.c:1497-1510`
/// runs with the rewritten mode `READ|WRITE|USE`). The behaviour: after the `STOP` a write into the
/// pipe delivers nothing (the descriptor is out of the poll set, so the "further readiness attempt" the
/// write represents reports nothing), and a second `STOP` answers `ERL_NIF_SELECT_STOP_CALLED` alone
/// (`1`) because no interest is left to clear — which is what proves the first one really did
/// de-register.
#[test]
fn enif_select_stop_on_an_armed_descriptor_releases_it() {
    let Some(answers) = run_case("stop", "select_caller:stop_case/1") else {
        return;
    };
    let Some((rust_code, oracle_code)) = answers.int("code") else {
        return;
    };
    let Some((rust_stop, oracle_stop)) = answers.int("stop") else {
        return;
    };
    let Some((rust_written, oracle_written)) = answers.int("written") else {
        return;
    };
    let Some((rust_outcome, oracle_outcome)) = answers.text("outcome") else {
        return;
    };
    let Some((rust_stop2, oracle_stop2)) = answers.int("stop2") else {
        return;
    };
    heading(
        "STOP",
        "enif_select(env, fd, ERL_NIF_SELECT_READ, …) then enif_select(env, fd, \
         ERL_NIF_SELECT_STOP, …), then the write, then a second STOP",
    );
    println!(
        "beam_rt        : code={rust_code} stop={rust_stop} [{}] written={rust_written} \
         outcome={rust_outcome} stop2={rust_stop2}",
        decode(rust_stop)
    );
    println!(
        "oracle         : code={oracle_code} stop={oracle_stop} [{}] written={oracle_written} \
         outcome={oracle_outcome} stop2={oracle_stop2}",
        decode(oracle_stop)
    );
    let agrees = rust_code == oracle_code
        && rust_stop == oracle_stop
        && rust_written == oracle_written
        && rust_outcome == oracle_outcome
        && rust_stop2 == oracle_stop2;
    verdict(
        agrees,
        "the same code on both runtimes — STOP_CALLED | READ_CANCELLED for the first STOP, \
         STOP_CALLED alone for the second — and the same observable behaviour: the descriptor is \
         de-registered, so a later write delivers nothing",
    );

    assert_eq!(oracle_code, 0, "the oracle accepts the read registration");
    assert_eq!(rust_code, 0, "this runtime accepts the read registration");
    // The C reports the stop *and* the direction it cleared on the way: STOP_CALLED | READ_CANCELLED.
    assert_eq!(
        oracle_stop,
        ERL_NIF_SELECT_STOP_CALLED | ERL_NIF_SELECT_READ_CANCELLED,
        "the oracle reports STOP_CALLED | READ_CANCELLED for a stopped read interest, got \
         {oracle_stop} [{}]",
        decode(oracle_stop)
    );
    assert_eq!(
        rust_stop, oracle_stop,
        "this runtime reports the oracle's exact stop code, got {rust_stop} [{}] against the \
         oracle's {oracle_stop} [{}]",
        decode(rust_stop),
        decode(oracle_stop)
    );
    assert_eq!(rust_written, 1, "the write itself must still succeed");
    assert_eq!(oracle_written, 1, "the write itself must still succeed on the oracle");
    // The fd is genuinely de-registered: the write wakes nobody on either side.
    assert_eq!(
        oracle_outcome, "timeout",
        "the oracle delivers nothing after the stop"
    );
    assert_eq!(
        rust_outcome, "timeout",
        "a stopped descriptor must not deliver the notification of a later write"
    );
    // A second STOP finds nothing left to release on the oracle — which is also what proves the
    // first one really de-registered the read interest (it no longer reports READ_CANCELLED).
    assert_eq!(
        oracle_stop2, ERL_NIF_SELECT_STOP_CALLED,
        "the oracle's second STOP reports STOP_CALLED alone"
    );
    assert_eq!(
        rust_stop2, oracle_stop2,
        "this runtime's second STOP reports the oracle's exact code, got {rust_stop2} [{}]",
        decode(rust_stop2)
    );
}

/// **`ERL_NIF_SELECT_CANCEL` clears exactly the direction that was armed** and reports it.
///
/// The C ORs one `ERL_NIF_SELECT_*_CANCELLED` bit per interest it really cleared
/// (`erl_check_io.c:1470-1482`), so a cancelled read interest answers `ERL_NIF_SELECT_READ_CANCELLED`;
/// a later write must deliver nothing, and a second `CANCEL` must report that it cancelled nothing.
#[test]
fn enif_select_cancel_reports_the_direction_it_cleared() {
    let Some(answers) = run_case("cancel", "select_caller:cancel_case/1") else {
        return;
    };
    let Some((rust_code, oracle_code)) = answers.int("code") else {
        return;
    };
    let Some((rust_cancel, oracle_cancel)) = answers.int("cancel") else {
        return;
    };
    let Some((rust_written, oracle_written)) = answers.int("written") else {
        return;
    };
    let Some((rust_outcome, oracle_outcome)) = answers.text("outcome") else {
        return;
    };
    let Some((rust_cancel2, oracle_cancel2)) = answers.int("cancel2") else {
        return;
    };
    heading(
        "CANCEL",
        "enif_select(env, fd, ERL_NIF_SELECT_READ, …) then enif_select(env, fd, \
         ERL_NIF_SELECT_CANCEL | ERL_NIF_SELECT_READ, …), then the write, then a second CANCEL",
    );
    println!(
        "beam_rt        : code={rust_code} cancel={rust_cancel} [{}] written={rust_written} \
         outcome={rust_outcome} cancel2={rust_cancel2}",
        decode(rust_cancel)
    );
    println!(
        "oracle         : code={oracle_code} cancel={oracle_cancel} [{}] written={oracle_written} \
         outcome={oracle_outcome} cancel2={oracle_cancel2}",
        decode(oracle_cancel)
    );
    let agrees = rust_code == oracle_code
        && rust_cancel == oracle_cancel
        && rust_written == oracle_written
        && rust_outcome == oracle_outcome
        && rust_cancel2 == oracle_cancel2;
    verdict(
        agrees,
        "the cancel reports exactly the armed read interest, de-registers the descriptor and \
         reports an empty cancel the second time",
    );

    assert_eq!(oracle_code, 0, "the oracle accepts the read registration");
    assert_eq!(rust_code, 0, "this runtime accepts the read registration");
    assert_eq!(
        oracle_cancel, ERL_NIF_SELECT_READ_CANCELLED,
        "the oracle reports the read interest it cleared, got {oracle_cancel} [{}]",
        decode(oracle_cancel)
    );
    assert_eq!(
        rust_cancel, ERL_NIF_SELECT_READ_CANCELLED,
        "this runtime reports the read interest it cleared, got {rust_cancel} [{}]",
        decode(rust_cancel)
    );
    assert_eq!(rust_written, 1, "the write itself must still succeed");
    assert_eq!(oracle_written, 1, "the write itself must still succeed on the oracle");
    assert_eq!(
        oracle_outcome, "timeout",
        "the oracle delivers nothing after the cancel"
    );
    assert_eq!(
        rust_outcome, "timeout",
        "a cancelled descriptor must not deliver the notification of a later write"
    );
    assert_eq!(
        oracle_cancel2, 0,
        "the oracle's second CANCEL cancelled nothing and reports 0"
    );
    assert_eq!(
        rust_cancel2, 0,
        "this runtime's second CANCEL cancelled nothing and reports 0"
    );
}

/// **`ERL_NIF_SELECT_CUSTOM_MSG` replaces the notification with the caller's own term.**
///
/// `enif_select_x(env, fd, READ | CUSTOM_MSG, obj, NULL, {custom, Ref, payload}, NULL)` must deliver
/// that 3-tuple **verbatim** — no `{select, _, _, Event}` wrapper and no event atom. The fixture's
/// catch-all clause reports any other shape as `{unexpected, Term}`, so a wrapper cannot slip through.
#[test]
fn enif_select_custom_msg_delivers_the_callers_message_verbatim() {
    let Some(answers) = run_case("custom", "select_caller:custom_case/1") else {
        return;
    };
    let Some((rust_code, oracle_code)) = answers.int("code") else {
        return;
    };
    let Some((rust_written, oracle_written)) = answers.int("written") else {
        return;
    };
    let Some((rust_outcome, oracle_outcome)) = answers.text("outcome") else {
        return;
    };
    heading(
        "CUSTOM_MSG",
        "enif_select_x(env, fd, ERL_NIF_SELECT_READ | ERL_NIF_SELECT_CUSTOM_MSG, resource, NULL, \
         {custom, Ref, payload}, NULL), then the write",
    );
    println!(
        "beam_rt        : code={rust_code} [{}] written={rust_written} outcome={rust_outcome}",
        decode(rust_code)
    );
    println!(
        "oracle         : code={oracle_code} [{}] written={oracle_written} outcome={oracle_outcome}",
        decode(oracle_code)
    );
    let agrees = rust_code == oracle_code
        && rust_written == oracle_written
        && rust_outcome == oracle_outcome;
    verdict(
        agrees,
        "the delivered message is the caller's own `{custom, Ref, payload}` on both runtimes",
    );

    assert_eq!(oracle_code, 0, "the oracle accepts the registration");
    assert_eq!(rust_code, 0, "this runtime accepts the registration");
    assert_eq!(rust_written, 1, "the write itself must succeed");
    assert_eq!(oracle_written, 1, "the write itself must succeed on the oracle");
    assert_eq!(
        oracle_outcome, "custom_delivered",
        "the oracle delivers the caller's message verbatim"
    );
    assert_eq!(
        rust_outcome, "custom_delivered",
        "this runtime delivers the caller's message verbatim (no select tuple, no event atom)"
    );
}

/// **A combined `READ | WRITE` mask reports the direction that really became ready.**
///
/// The C registers one interest per direction and queues the matching event atom for whichever of
/// them fired (`erl_check_io.c:1451-1463`, `:2120-2185`), so the answer is directional rather than
/// fixed: a pipe's write end is immediately writable (`ready_output`) and a read end that already
/// holds a byte is immediately readable (`ready_input`).
#[test]
fn enif_select_combined_mask_reports_the_direction_that_became_ready() {
    let Some(write) = run_case("combined_write", "select_caller:combined_write/1") else {
        return;
    };
    let Some((rust_code, oracle_code)) = write.int("code") else {
        return;
    };
    let Some((rust_outcome, oracle_outcome)) = write.text("outcome") else {
        return;
    };
    let Some(read) = run_case("combined_read", "select_caller:combined_read/1") else {
        return;
    };
    let Some((rust_read_code, oracle_read_code)) = read.int("code") else {
        return;
    };
    let Some((rust_read_outcome, oracle_read_outcome)) = read.text("outcome") else {
        return;
    };
    heading(
        "combined READ | WRITE mask",
        &format!(
            "enif_select(env, fd, ERL_NIF_SELECT_READ | ERL_NIF_SELECT_WRITE={}, …) on a pipe's \
             write end, and on a read end that already holds a byte",
            ERL_NIF_SELECT_READ | ERL_NIF_SELECT_WRITE
        ),
    );
    println!(
        "beam_rt        : write code={rust_code} [{}] outcome={rust_outcome} | read \
         code={rust_read_code} outcome={rust_read_outcome}",
        decode(rust_code)
    );
    println!(
        "oracle         : write code={oracle_code} [{}] outcome={oracle_outcome} | read \
         code={oracle_read_code} outcome={oracle_read_outcome}",
        decode(oracle_code)
    );
    let agrees = rust_code == oracle_code
        && rust_outcome == oracle_outcome
        && rust_read_code == oracle_read_code
        && rust_read_outcome == oracle_read_outcome;
    verdict(
        agrees,
        "one registration per direction, and the delivered event atom names the direction that \
         really became ready",
    );

    assert_eq!(oracle_code, 0, "the oracle accepts the write-end registration");
    assert_eq!(rust_code, 0, "this runtime accepts the write-end registration");
    assert_eq!(
        oracle_outcome, "{delivered,ready_output}",
        "the oracle reports a writable pipe write end as ready_output"
    );
    assert_eq!(
        rust_outcome, "{delivered,ready_output}",
        "this runtime reports a writable pipe write end as ready_output"
    );
    assert_eq!(oracle_read_code, 0, "the oracle accepts the read-end registration");
    assert_eq!(rust_read_code, 0, "this runtime accepts the read-end registration");
    assert_eq!(
        oracle_outcome, "{delivered,ready_output}",
        "the oracle's write end is the writable one"
    );
    assert_eq!(
        oracle_read_outcome, "{delivered,ready_input}",
        "the oracle reports a readable pipe read end as ready_input"
    );
    assert_eq!(
        rust_read_outcome, "{delivered,ready_input}",
        "this runtime reports a readable pipe read end as ready_input"
    );
}

/// **The `ERL_NIF_SELECT_ERROR` interest reports `ready_error` on a reset socket.**
///
/// The socket pair is real: the fixture connects a loopback TCP pair and closes the client with
/// `SO_LINGER {1,0}`, which makes the kernel answer the accepted socket with an `RST`. An `ERROR`
/// interest armed on that socket must deliver the C's `ready_error` atom — once, since the select is
/// one-shot — on both runtimes.
#[test]
fn enif_select_error_interest_reports_ready_error() {
    let Some(answers) = run_case("error_only", "select_caller:error_only_case/1") else {
        return;
    };
    let Some((rust_code, oracle_code)) = answers.int("code") else {
        return;
    };
    let Some((rust_outcome, oracle_outcome)) = answers.text("outcome") else {
        return;
    };
    let Some((rust_second, oracle_second)) = answers.text("second") else {
        return;
    };
    heading(
        "ERROR interest",
        "enif_select(env, reset_socket_fd, ERL_NIF_SELECT_ERROR, …) on the accepted end of a \
         loopback TCP pair whose peer closed with SO_LINGER {1,0}",
    );
    println!(
        "beam_rt        : code={rust_code} [{}] outcome={rust_outcome} second={rust_second}",
        decode(rust_code)
    );
    println!(
        "oracle         : code={oracle_code} [{}] outcome={oracle_outcome} second={oracle_second}",
        decode(oracle_code)
    );
    let agrees = rust_code == oracle_code
        && rust_outcome == oracle_outcome
        && rust_second == oracle_second;
    verdict(
        agrees,
        "the error condition is reported as ready_error, exactly once",
    );

    assert_eq!(oracle_code, 0, "the oracle accepts the error interest");
    assert_eq!(rust_code, 0, "this runtime accepts the error interest");
    assert_eq!(
        oracle_outcome, "{delivered,ready_error}",
        "the oracle reports a reset socket as ready_error"
    );
    assert_eq!(
        rust_outcome, "{delivered,ready_error}",
        "this runtime reports a reset socket as ready_error"
    );
    assert_eq!(
        oracle_second, "timeout",
        "the oracle's select is one-shot, so nothing follows"
    );
    assert_eq!(
        rust_second, "timeout",
        "this runtime's select is one-shot, so nothing follows"
    );
}

/// **A combined `READ | ERROR` interest on a reset socket delivers both directions, in the C's order.**
///
/// The C queues **one message per observed direction**, in (`in`, `out`, `err`) order
/// (`erl_check_io.c:2120-2185`), and a reset socket's readiness reports both `EPOLLIN` and `EPOLLERR`,
/// so the answer is `{delivered,ready_input}` and then `{delivered,ready_error}`. Both runtimes print
/// that same pair for the same `.so`/`.beam`, both accept the combined interest with the C's `0`, and
/// nothing follows the two.
#[test]
fn enif_select_read_and_error_interest_delivers_both_directions() {
    let Some(answers) = run_case("error", "select_caller:error_case/1") else {
        return;
    };
    let Some((rust_code, oracle_code)) = answers.int("code") else {
        return;
    };
    let Some((rust_first, oracle_first)) = answers.text("outcome") else {
        return;
    };
    let Some((rust_second, oracle_second)) = answers.text("second") else {
        return;
    };
    let Some((rust_third, oracle_third)) = answers.text("third") else {
        return;
    };
    heading(
        "READ | ERROR interest",
        &format!(
            "enif_select(env, reset_socket_fd, ERL_NIF_SELECT_READ | ERL_NIF_SELECT_ERROR={}, …) \
             on the accepted end of a reset TCP pair",
            ERL_NIF_SELECT_READ | ERL_NIF_SELECT_ERROR
        ),
    );
    println!(
        "beam_rt        : code={rust_code} [{}] first={rust_first} second={rust_second} \
         third={rust_third}",
        decode(rust_code)
    );
    println!(
        "oracle         : code={oracle_code} [{}] first={oracle_first} second={oracle_second} \
         third={oracle_third}",
        decode(oracle_code)
    );
    let agrees = rust_code == oracle_code
        && rust_first == oracle_first
        && rust_second == oracle_second
        && rust_third == oracle_third;
    verdict(
        agrees,
        "one message per observed direction, in the C's in → out → err order: ready_input and then \
         ready_error, with nothing after the two",
    );

    assert_eq!(oracle_code, 0, "the oracle accepts the combined interest");
    assert_eq!(rust_code, oracle_code, "this runtime accepts the combined interest with the C's 0");
    assert_eq!(
        oracle_first, "{delivered,ready_input}",
        "the oracle's first message names the input direction (the C's in→out→err order)"
    );
    assert_eq!(
        rust_first, oracle_first,
        "this runtime's first message names the input direction too, got {rust_first}"
    );
    assert_eq!(
        oracle_second, "{delivered,ready_error}",
        "the oracle's second message names the error direction"
    );
    assert_eq!(
        rust_second, oracle_second,
        "this runtime's second message names the error direction too, got {rust_second}"
    );
    assert_eq!(
        oracle_third, "timeout",
        "the oracle delivers nothing after the two directions, got {oracle_third}"
    );
    assert_eq!(
        rust_third, oracle_third,
        "this runtime delivers nothing after the two directions either, got {rust_third}"
    );
}

/// **An armed interest keeps its object alive after the NIF drops its own reference.**
///
/// The C takes one `enif_keep_resource` reference per armed interest (`erl_check_io.c:1446`) and gives
/// it back when the interest is dropped (`:1533`, `:1834`, `:2242`), so a NIF that arms a select and
/// then releases the object it allocated still gets its notification. `arm_keep/0` is that shape: it
/// registers a read interest on a fresh pipe and mints **no** resource term, so the object's reference
/// count is exactly the allocator's one plus the interest's one. After `release/0` the object must
/// still be alive (`dtor_after_release=0`, and the pipe's descriptors still open, so `poke/0` writes
/// its byte: `written=1`), and the readiness must still be delivered (`{delivered,ready_input}`) —
/// the mint the delivery makes can only succeed on a live object, which is what pins the
/// mint-before-release ordering: a release of the interest's reference **before** that mint would
/// leave a zero count, so the mint would refuse and nothing would be delivered.
/// `final_dtor=0` afterwards is the rest of the same picture: the delivered term took a reference of
/// its own, so the object is held by the term rather than dropped by the disarm — and the oracle,
/// whose destructor runs later still as aux work, reads the same zero.
/// The "exactly once" half is pinned by the two cases below, where the count does reach zero.
#[test]
fn enif_select_interest_reference_keeps_a_released_object_alive_until_readiness() {
    let Some((answers, readings)) = run_repeated(
        "kept",
        "select_caller:kept_case/1",
        3,
        &["dtor_after_release", "final_dtor"],
    ) else {
        return;
    };
    let Some((rust_code, oracle_code)) = answers.int("code") else {
        return;
    };
    let Some((rust_release, oracle_release)) = answers.text("release") else {
        return;
    };
    let Some((rust_early, oracle_early)) = answers.text("early") else {
        return;
    };
    let Some((rust_written, oracle_written)) = answers.int("written") else {
        return;
    };
    let Some((rust_outcome, oracle_outcome)) = answers.text("outcome") else {
        return;
    };
    let Some(rust_dtor_after_release) = rust_int(&answers, "dtor_after_release") else {
        return;
    };
    let Some(rust_final_dtor) = rust_int(&answers, "final_dtor") else {
        return;
    };
    heading(
        "released then ready",
        &format!(
            "enif_select(env, pipe_read_fd, ERL_NIF_SELECT_READ={ERL_NIF_SELECT_READ}, object, \
             NULL, Ref) with **no** enif_make_resource, then enif_release_resource(object) — the NIF \
             drops its own reference — then the write"
        ),
    );
    println!(
        "beam_rt        : code={rust_code} [{}] release={rust_release} \
         dtor_after_release={rust_dtor_after_release} early={rust_early} written={rust_written} \
         outcome={rust_outcome} final_dtor={rust_final_dtor}",
        decode(rust_code)
    );
    println!(
        "oracle         : code={oracle_code} [{}] release={oracle_release} early={oracle_early} \
         written={oracle_written} outcome={oracle_outcome}",
        decode(oracle_code)
    );
    println!(
        "oracle dtors   : {} — the C's destructor is deferred aux work, so these are a spread, not a \
         constant (see the module docs)",
        readings.join(" | ")
    );
    let agrees = rust_code == oracle_code
        && rust_release == oracle_release
        && rust_early == oracle_early
        && rust_written == oracle_written
        && rust_outcome == oracle_outcome;
    verdict(
        agrees,
        "the interest's own reference is what keeps the object alive: the release reports success, \
         the descriptor is still open, the readiness is delivered, and the delivered term — not the \
         interest — is what holds the object afterwards",
    );

    assert_eq!(oracle_code, 0, "the oracle accepts the registration");
    assert_eq!(rust_code, 0, "this runtime accepts the registration");
    assert_eq!(oracle_release, "released", "the oracle's release reports success");
    assert_eq!(rust_release, "released", "the release of the NIF's own reference must succeed");
    // The reference the release dropped was the allocator's; the interest holds its own.
    assert_eq!(
        rust_dtor_after_release, 0,
        "the object must survive the NIF's own release: the interest holds a reference of its own"
    );
    assert_eq!(
        oracle_early, "timeout",
        "the oracle delivers nothing before the write"
    );
    assert_eq!(
        rust_early, "timeout",
        "no {{select, …}} may reach the caller before the descriptor is readable"
    );
    assert_eq!(
        rust_written, 1,
        "the pipe must still be open after the release (a destroyed object closes its descriptors, \
         so a `no_pipe` answer here is exactly the pre-fix behaviour)"
    );
    assert_eq!(oracle_written, 1, "the oracle's write into the pipe must succeed");
    assert_eq!(
        oracle_outcome, "{delivered,ready_input}",
        "the oracle delivers {{select, Resource, Ref, ready_input}} for the released-but-armed object"
    );
    assert_eq!(
        rust_outcome, "{delivered,ready_input}",
        "this runtime must deliver the readiness of an object whose only remaining reference is the \
         interest's own"
    );
    assert_eq!(
        rust_final_dtor, 0,
        "the delivered term took a reference of its own, so the object is still alive once the \
         interest's reference has been given back: a nonzero count here would mean the delivery \
         destroyed it — a release made before the mint, or a double one"
    );
}

/// **A `STOP` on a released-but-armed interest gives the interest's reference back exactly once.**
///
/// The C disarms with `ERL_NIF_SELECT_STOP` (`erl_check_io.c:1272-1284`) and releases the interest's
/// reference on the way out (`:1533`), so with the NIF's own reference already dropped by `release/0`
/// the object's count reaches zero there — exactly one destructor run, no over-keep (the interest took
/// one) and no leak (it is given back). Two things follow, identically on both runtimes: a write after
/// the stop delivers nothing (`outcome=timeout`), and a second `STOP` finds the descriptor already
/// gone and reports `ERL_NIF_SELECT_STOP_CALLED` alone (`stop2=1`).
///
/// The second `STOP` names the descriptor with a **sentinel** object rather than the interest's own:
/// the C dereferences the object it is handed (`DATA_TO_RESOURCE(obj)`, `:1245`) and this runtime
/// destroys the object synchronously at the first `STOP`, so naming the interest's object again would
/// hand the oracle a freed block. `select_nif:stop/0` makes that substitution itself, answering
/// `nothing_armed` only when nothing was ever armed.
#[test]
fn enif_select_stop_releases_the_interests_reference_exactly_once() {
    let Some((answers, readings)) = run_repeated(
        "stop_release",
        "select_caller:stop_after_release_case/1",
        3,
        &["dtor_after_release", "dtor_after_stop"],
    ) else {
        return;
    };
    let Some((rust_code, oracle_code)) = answers.int("code") else {
        return;
    };
    let Some((rust_stop, oracle_stop)) = answers.int("stop") else {
        return;
    };
    let Some((rust_outcome, oracle_outcome)) = answers.text("outcome") else {
        return;
    };
    let Some((rust_stop2, oracle_stop2)) = answers.int("stop2") else {
        return;
    };
    let Some(rust_dtor_after_release) = rust_int(&answers, "dtor_after_release") else {
        return;
    };
    let Some(rust_dtor_after_stop) = rust_int(&answers, "dtor_after_stop") else {
        return;
    };
    heading(
        "STOP after release",
        "enif_select(env, pipe_read_fd, ERL_NIF_SELECT_READ, object, NULL, Ref) with no \
         enif_make_resource, enif_release_resource(object), then \
         enif_select(env, pipe_read_fd, ERL_NIF_SELECT_STOP, …), then a second STOP",
    );
    println!(
        "beam_rt        : code={rust_code} [{}] stop={rust_stop} [{}] dtor_after_release={} \
         dtor_after_stop={rust_dtor_after_stop} outcome={rust_outcome} stop2={rust_stop2}",
        decode(rust_code),
        decode(rust_stop),
        rust_dtor_after_release
    );
    println!(
        "oracle         : code={oracle_code} [{}] stop={oracle_stop} [{}] outcome={oracle_outcome} \
         stop2={oracle_stop2}",
        decode(oracle_code),
        decode(oracle_stop)
    );
    println!(
        "oracle dtors   : {} — the C's destructor is deferred aux work, so these are a spread, not a \
         constant (see the module docs)",
        readings.join(" | ")
    );
    let agrees = rust_code == oracle_code
        && rust_stop == oracle_stop
        && rust_outcome == oracle_outcome
        && rust_stop2 == oracle_stop2;
    verdict(
        agrees,
        "the same code on both runtimes — STOP_CALLED | READ_CANCELLED for the stop, STOP_CALLED \
         alone for the second — and one destructor run here, because the interest's single reference \
         is what the disarm gives back",
    );

    assert_eq!(oracle_code, 0, "the oracle accepts the read registration");
    assert_eq!(rust_code, 0, "this runtime accepts the read registration");
    assert_eq!(
        oracle_stop,
        ERL_NIF_SELECT_STOP_CALLED | ERL_NIF_SELECT_READ_CANCELLED,
        "the oracle reports STOP_CALLED | READ_CANCELLED for a stopped read interest, got \
         {oracle_stop} [{}]",
        decode(oracle_stop)
    );
    assert_eq!(
        rust_stop, oracle_stop,
        "this runtime reports the oracle's exact stop code, got {rust_stop} [{}] against the \
         oracle's {oracle_stop} [{}]",
        decode(rust_stop),
        decode(oracle_stop)
    );
    assert_eq!(
        rust_dtor_after_release, 0,
        "the object must survive the NIF's own release: the interest holds a reference of its own"
    );
    assert_eq!(
        rust_dtor_after_stop, 1,
        "the disarm gives the interest's one reference back, so the object dies there exactly once \
         (0 would be an over-keep, 2 a double release)"
    );
    assert_eq!(
        oracle_outcome, "timeout",
        "the oracle delivers nothing after the stop"
    );
    assert_eq!(
        rust_outcome, "timeout",
        "a stopped descriptor must not deliver the notification of a later write"
    );
    assert_eq!(
        oracle_stop2, ERL_NIF_SELECT_STOP_CALLED,
        "the oracle's second STOP reports STOP_CALLED alone"
    );
    assert_eq!(
        rust_stop2, oracle_stop2,
        "this runtime's second STOP reports the oracle's exact code, got {rust_stop2} [{}]",
        decode(rust_stop2)
    );
}

/// **A dead owner's interest is pruned, and the interest's reference released with it.**
///
/// The C's select path monitors the owning process and `tty_monitor_down` clears the interest when it
/// dies (`erl_check_io.c:1266-1276`); this runtime's equivalent is
/// [`PortRegistry::prune_dead_select`](crate::port::PortRegistry::prune_dead_select), run at the start
/// of every port round, whose `SelectHold` drop is what gives the interest's reference back
/// (`src/port/select/tests.rs::a_dead_owner_is_pruned_and_a_live_one_is_kept` pins the same half at
/// the unit level). `owner_death_case/1` is the end-to-end shape: a spawned process arms a read
/// interest and drops its own reference in one call (`arm_keep_release/0`), reports the arming
/// reference to its parent, and exits — and the parent then observes the object's destructor having
/// run (`dtor_after_death=1`) and no notification ever arriving (`outcome=timeout`).
#[test]
fn enif_select_interest_of_a_dead_owner_is_pruned_and_released() {
    let Some((answers, readings)) = run_repeated(
        "owner_death",
        "select_caller:owner_death_case/1",
        2,
        &["dtor_after_arm", "dtor_after_death"],
    ) else {
        return;
    };
    let Some((rust_code, oracle_code)) = answers.int("code") else {
        return;
    };
    let Some((rust_outcome, oracle_outcome)) = answers.text("outcome") else {
        return;
    };
    let Some(rust_dtor_after_arm) = rust_int(&answers, "dtor_after_arm") else {
        return;
    };
    let Some(rust_dtor_after_death) = rust_int(&answers, "dtor_after_death") else {
        return;
    };
    heading(
        "owner death",
        "a spawned process calls arm_keep_release/0 — enif_select(env, pipe_read_fd, \
         ERL_NIF_SELECT_READ, object, NULL, Ref) followed by enif_release_resource(object) — sends the \
         reference to its parent and exits; the parent waits, then reads the destructor count",
    );
    println!(
        "beam_rt        : code={rust_code} [{}] dtor_after_arm={rust_dtor_after_arm} \
         dtor_after_death={rust_dtor_after_death} outcome={rust_outcome}",
        decode(rust_code)
    );
    println!(
        "oracle         : code={oracle_code} [{}] outcome={oracle_outcome}",
        decode(oracle_code)
    );
    println!(
        "oracle dtors   : {} — the C's destructor is deferred aux work, so these are a spread, not a \
         constant (see the module docs)",
        readings.join(" | ")
    );
    let agrees = rust_code == oracle_code && rust_outcome == oracle_outcome;
    verdict(
        agrees,
        "the interest is pruned with its owner on both runtimes: no notification is ever delivered, \
         and here the interest's reference is given back, so the object's destructor runs once",
    );

    assert_eq!(
        oracle_code, 0,
        "the oracle accepts the child's registration, got {oracle_code} [{}]",
        decode(oracle_code)
    );
    assert_eq!(
        rust_code, 0,
        "this runtime accepts the child's registration, got {rust_code} [{}]",
        decode(rust_code)
    );
    assert_eq!(
        rust_dtor_after_arm, 0,
        "the armed, released object must still be alive while its owner runs: the interest holds a \
         reference of its own"
    );
    assert_eq!(
        rust_dtor_after_death, 1,
        "the liveness prune must give the dead owner's interest reference back, so the object dies \
         exactly once (0 would be a leaked registration, 2 a double release)"
    );
    assert_eq!(
        oracle_outcome, "timeout",
        "the oracle never delivers a dead owner's readiness"
    );
    assert_eq!(
        rust_outcome, "timeout",
        "a pruned registration must not deliver, and its reference must not be minted into a term"
    );
}
