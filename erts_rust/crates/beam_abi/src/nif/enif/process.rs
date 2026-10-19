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

//! `enif_*` — the **process family** (workspace contract **A.3**): monitors, message sending, port
//! commands, registered-name lookup, process/port liveness and dirty-scheduler NIF scheduling.
//!
//! All twelve slots this file owns are populated: `enif_monitor_process`, `enif_demonitor_process`,
//! `enif_compare_monitors`, `enif_make_monitor_term`, `enif_is_process_alive`,
//! `enif_is_current_process_alive`, `enif_is_port_alive`, `enif_send`, `enif_port_command`,
//! `enif_whereis_pid`, `enif_whereis_port`, `enif_schedule_nif`. Every one drives the
//! already-declared `NifHost` hooks (`nif::host`) — this file adds no runtime state and no new hook.
//!
//! # Reproduced C contracts
//!
//! Ground truth: `erts/emulator/beam/erl_nif.c` (line numbers below) and the prose in
//! `erts/doc/references/erl_nif.md`.
//!
//! | function | return value reproduced |
//! |---|---|
//! | `enif_send` | `1` sent, `0` not sent (`0` is the C's `!rp` answer, `erl_nif.c:880/899`; `1` after queueing, `:1093`) |
//! | `enif_port_command` | `1` accepted, `0` otherwise (unknown port ⇒ `0`, `:1124-1132`) |
//! | `enif_whereis_pid` / `_port` | `1` + the id written, else `0`: not an atom (`:1174/1186`), nothing registered, or the registered id has the wrong kind (`enif_get_local_*`'s type check, `:1179/1191`) |
//! | `enif_is_process_alive` / `_port_alive` / `_current_process_alive` | `1`/`0` (`:2304-2357`) |
//! | `enif_monitor_process` | `0` established, `1` dead/undefined target (`:3975-3976`, `:4004`), `-1` no `down` callback (`:3968-3972`) |
//! | `enif_demonitor_process` | `0` identified **and removed** (`:4051`), `1` could not be identified (`:4043-4044`) |
//! | `enif_compare_monitors` | `memcmp`-style `<0`/`0`/`>0` over the monitor payload (`:4054-4059`) |
//! | `enif_make_monitor_term` | an ordinary internal reference holding the monitor's payload (`:4013-4017`) |
//! | `enif_schedule_nif` | the term the scheduled call will produce; `badarg` for `argc > MAX_ARG` (`:3569`) or a bad `flags` (`:3597`) |
//!
//! # The monitor payload is the tree's internal-reference block
//!
//! `ErlNifMonitor` is `typedef ErlDrvMonitor ErlNifMonitor`, an opaque `unsigned char data[32]`
//! that carries an *ordinary internal reference*: `erts_ref_to_driver_monitor` byte-copies the
//! reference block into it and `erts_driver_monitor_to_ref` copies it back onto a heap and re-tags
//! the pointer (`io.c:6994-7010`).
//!
//! The C's block is `ERTS_REF_THING_SIZE * sizeof(Eterm)` = **24** bytes, because in the C the
//! three packed `num[]` ids of `ErtsORefThing` (`erl_term.h:990`) hold the whole reference identity.
//! **This tree does not build that block.** The frozen internal-reference thing is the 4-word M1
//! block `[make_header(3, REF_SUBTAG), node, serial, id]`
//! (`beam_term::boxed::{write_internal_ref, INTERNAL_REF_WORDS}`, which `beam_process::MonitorRef`,
//! `beam_bifs`'s `make_ref_thing` and `beam_rt` all mint), whose identity lives in **word 3** while
//! words 1..2 (`node`, `serial`) are pinned to `make_small(0)`. So the block copied here is the
//! whole **32** bytes of it — `REF_THING_BYTES` (`REF_THING_WORDS` = 4 machine words), which fills
//! the `ErlNifMonitor` payload ([`MONITOR_BYTES`] = `sizeof(void *) * 4` = 32) exactly. Copying only
//! the C's 24 bytes would drop the id, leaving every monitor's block byte-identical.
//!
//! * [`enif_monitor_process`] stores the whole 4-word block of the internal reference the host hands
//!   over — [`NifHost::monitor_process_result`]'s `Armed(term)`, or the legacy
//!   [`NifHost::monitor_process`]'s term (the C's `erts_make_ref_in_buffer`). The hook must
//!   therefore return an internal reference of the frozen shape (header arity
//!   `INTERNAL_REF_PAYLOAD_WORDS`); anything else is refused as `1` ("could not establish") rather
//!   than stored, so no un-reconstructable payload is ever fabricated.
//! * There is no zeroed tail: the block is the whole 32-byte payload, so
//!   [`enif_compare_monitors`] compares every byte of the holder — and therefore two monitors armed
//!   from different references stay distinct.
//!
//! # Documented substitutions — never a wrong value, never a crash
//!
//! * **A `NULL` `obj` is ignored by the monitor family.** The C reaches the resource's monitor table
//!   through `obj` (`DATA_TO_RESOURCE`); here the only fact read from it is
//!   [`super::resources::resource_type_has_down`], which recognises the payload pointers this crate
//!   minted and answers `None` for anything else (a null `obj`, a foreign object). Only a
//!   recognised object of a type opened **without** a `down` callback selects the C's `-1`
//!   ([`MONITOR_NO_DOWN_CALLBACK`]); every unrecognised `obj` keeps the historical behaviour, so an
//!   `obj` from another NIF module is never dereferenced.
//! * **`enif_monitor_process` reaches all three C codes** (`0` established, `1` dead/undefined
//!   target, `-1` no `down` callback): the `-1` gate is answered from this crate's resource registry
//!   first, exactly as the C orders it (`erl_nif.c:3968-3972` precedes `:3975`), and the arm itself is
//!   the host's [`NifHost::monitor_process_result`] hook. With no host support (or no reference from
//!   it) the answer is the C's "target not alive" `1` — never a fabricated `0`.
//! * **`enif_whereis_pid`/`_port` answer `0` for a `NULL` out-parameter** (the C dereferences it).
//! * **The liveness predicates answer `0` with no host support.** The C's answer for a dead or
//!   invalid id is `0` too (`!!erts_proc_lookup(...)`, `:2330`), so "unknown" and "dead" coincide; a
//!   fabricated `1` ("alive") is never produced. A `NULL` pid/port pointer answers `0` (the C
//!   faults).
//! * **`NULL` `to_pid`/`to_port` answer `0`** (the C dereferences them), and a **`NULL` `env`
//!   answers `0`** for `enif_send`/`enif_port_command` (this seam reaches the runtime only through
//!   the env's host) — `enif_port_command`'s C counterpart aborts the emulator (`:1108-1109`).
//! * **`enif_send` passes `from = None`.** `enif_send` always sends *from the calling process* —
//!   the C derives `from` from the caller's env (`from = c_p ? c_p->common.id : am_undefined`,
//!   `:996`) and cannot send on another process's behalf — which is exactly what
//!   `NifHost::send`'s `None` means. The host resolves the concrete sender itself. `msg_env` (the
//!   message's own environment in the C) is accepted for signature fidelity and unused: the hook
//!   performs the term movement itself.
//! * **`enif_schedule_nif` refuses what it cannot honour.** `argc > 255` (`MAX_ARG`,
//!   `erl_vm.h:48`) and a `flags` with any bit outside `ERL_NIF_DIRTY_JOB_CPU_BOUND |
//!   ERL_NIF_DIRTY_JOB_IO_BOUND` raise `badarg`, exactly as the C does; a `NULL` `fun_name` and a
//!   `NULL` function pointer also raise `badarg` (the C would fault in `strlen`/call through the
//!   null pointer), and a negative `argc` cannot be expressed to the hook (which counts with a
//!   `usize`). With no host support it raises `badarg` — it never pretends to have scheduled
//!   anything.
//! * **`enif_schedule_nif` does not carry `argv` across the seam.** `NifHost::schedule_nif` takes
//!   `(fun_name, flags, fptr, argc)`; the C idiom re-schedules the *current* call's own argument
//!   vector, which the host's NIF-call frame already holds. The name is handed over as `&str`
//!   (interning it, and any atom-table limit, is the host's concern).
//! * **`enif_make_monitor_term`/`enif_demonitor_process` need a usable `env`** (they rebuild the
//!   reference on its heap); with a `NULL` env they report the crate's "cannot" answer (`badarg`
//!   term / `1`) instead of faulting.
//! * **`enif_compare_monitors` with a `NULL` operand** answers `0` for two nulls and `-1`/`1` for
//!   exactly one (the C reads through the pointer and faults). Non-null operands are ordered
//!   `memcmp`-style over the whole stored reference block ([`REF_THING_BYTES`] bytes, the
//!   `ErlNifMonitor` payload), byte for byte.

use super::*;

use libc::{c_char, c_int, c_void};

use crate::common::{ERL_DIRTY_JOB_CPU_BOUND, ERL_DIRTY_JOB_IO_BOUND};
use crate::nif::host::NifMonitorResult;
use crate::nif::types::{ErlNifMonitor, ErlNifPid, ErlNifPort};

/// `MAX_ARG` (`erl_vm.h:48`) — the largest argument count `enif_schedule_nif` accepts.
const MAX_ARG: c_int = 255;

/// The C's "the resource type carries no `down` callback" code (`erl_nif.c:3968-3972`).
const MONITOR_NO_DOWN_CALLBACK: c_int = -1;

/// The C's "the monitor was established" code (`erl_nif.c:4010`).
const MONITOR_ESTABLISHED: c_int = 0;

/// The C's "the target process is not alive / `target_pid` is undefined" code
/// (`erl_nif.c:3976`, `:4004`).
const MONITOR_TARGET_DEAD: c_int = 1;

/// The C's "the monitor was identified and removed" code (`erl_nif.c:4051`).
const DEMONITOR_REMOVED: c_int = 0;

/// The C's "the monitor could not be identified" code (`erl_nif.c:4043-4044`).
const DEMONITOR_NOT_FOUND: c_int = 1;

/// The word count of the internal-reference block copied into an `ErlNifMonitor`.
///
/// The C's `ERTS_REF_THING_SIZE` (`erl_term.h:990`) is `3` — `ErtsORefThing { Eterm header;
/// uint32 num[3]; uint32 marker; }`, whose three packed `num[]` ids hold the whole reference
/// identity. This tree builds a different thing: the frozen 4-word M1 internal reference
/// `[make_header(3, REF_SUBTAG), node, serial, id]`
/// (`beam_term::boxed::{INTERNAL_REF_WORDS, write_internal_ref}`), whose identity lives in word 3
/// while `node`/`serial` are pinned to `make_small(0)`. So the block this crate must copy is the
/// frozen block's whole word count — anything less would drop the id and make two distinct
/// monitors compare equal.
const REF_THING_WORDS: usize = prelude::INTERNAL_REF_WORDS;

/// The whole stored reference block: [`REF_THING_WORDS`] machine words, i.e. **32** bytes on LP64 —
/// which fills the `ErlNifMonitor` payload ([`MONITOR_BYTES`]) exactly. This is the byte range the
/// C copies (`io.c:6994-7010`) and compares (`erl_nif.c:4057`), widened from the C's 24 bytes to
/// this tree's 4-word frozen block. Also the key of the resource monitor tree
/// ([`super::resources::MonitorId`]), which is why the sibling monitor module reads it.
pub(super) const REF_THING_BYTES: usize = REF_THING_WORDS * core::mem::size_of::<Eterm>();

/// The `ErlNifMonitor` payload length, spelled as the type declares it (`sizeof(void *) * 4`).
pub(super) const MONITOR_BYTES: usize = core::mem::size_of::<*mut c_void>() * 4;

// The C's own assert is `ERTS_REF_THING_SIZE*sizeof(Uint) <= sizeof(ErlDrvMonitor)` (`io.c:6996`);
// with this tree's 4-word block the block and the holder payload are the same 32 bytes.
const _: () = assert!(REF_THING_BYTES <= MONITOR_BYTES);

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_monitor_process = Some(enif_monitor_process);
    api.enif_demonitor_process = Some(enif_demonitor_process);
    api.enif_compare_monitors = Some(enif_compare_monitors);
    api.enif_make_monitor_term = Some(enif_make_monitor_term);
    api.enif_is_process_alive = Some(enif_is_process_alive);
    api.enif_is_current_process_alive = Some(enif_is_current_process_alive);
    api.enif_is_port_alive = Some(enif_is_port_alive);
    api.enif_send = Some(enif_send);
    api.enif_port_command = Some(enif_port_command);
    api.enif_whereis_pid = Some(enif_whereis_pid);
    api.enif_whereis_port = Some(enif_whereis_port);
    api.enif_schedule_nif = Some(enif_schedule_nif);
    api
}

// ---------------------------------------------------------------------------------------------
// The monitor payload — the tree's internal-reference block (`io.c:6994-7010`)
// ---------------------------------------------------------------------------------------------

/// The first [`REF_THING_BYTES`] bytes of `monitor` — the reference block it carries (`None` for a
/// null pointer).
///
/// # Safety
///
/// `monitor` must be null or point at a readable [`ErlNifMonitor`].
pub(super) unsafe fn monitor_block(monitor: *const ErlNifMonitor) -> Option<[u8; REF_THING_BYTES]> {
    if monitor.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a readable `ErlNifMonitor`; the array copies the first
    // `REF_THING_BYTES` of its `MONITOR_BYTES`-byte payload out, so it borrows nothing.
    let data = unsafe { (*monitor).data };
    let mut block = [0u8; REF_THING_BYTES];
    block.copy_from_slice(&data[..REF_THING_BYTES]);
    Some(block)
}

/// The `ErlNifMonitor` **value** a C caller is handed for the reference block `block` — the C
/// `erts_ref_to_driver_monitor` (`erl_nif.c:3105`, `:4008-4009`): the reference block is copied into
/// the holder and the rest of the payload is left zero.
///
/// The block is the whole [`REF_THING_BYTES`]-byte frozen reference block, so it fills the
/// [`MONITOR_BYTES`]-byte payload exactly (a copy, never a borrow).
///
/// This is the one place the block→holder mapping is written; [`store_monitor`] writes it into a
/// caller's pointer and the `down` operand builder ([`super::resources`]'s monitor module) takes the
/// value by return, so the two can never disagree.
pub(super) fn holder_from_block(block: &[u8; REF_THING_BYTES]) -> ErlNifMonitor {
    let mut data = [0u8; MONITOR_BYTES];
    data[..REF_THING_BYTES].copy_from_slice(block);
    ErlNifMonitor { data }
}

/// Store `block` in `monitor` (the C `erts_ref_to_driver_monitor`).
///
/// A null `monitor` is legal in the C (`if (monitor) …`, `erl_nif.c:4007`). The block is the whole
/// [`MONITOR_BYTES`]-byte payload, so every byte of the holder is written (on a narrower block the
/// bytes past it are zeroed; the C leaves them untouched and never reads them).
fn store_monitor(monitor: *mut ErlNifMonitor, block: &[u8; REF_THING_BYTES]) {
    if monitor.is_null() {
        return;
    }
    // SAFETY: `monitor` is non-null and writable (caller contract); `ErlNifMonitor` is `#[repr(C)]`
    // over exactly `MONITOR_BYTES` bytes.
    unsafe { monitor.write(holder_from_block(block)) };
}

/// The whole reference block of the internal reference `term` (`None` when it is not one) — the C's
/// `is_internal_ordinary_ref(ref)` + `internal_ref_val(ref)` pair (`io.c:6997-6999`).
///
/// The block is the frozen 4-word M1 internal reference `[header, node, serial, id]`
/// (`beam_term::boxed::INTERNAL_REF_WORDS`), so its header **arity** must be
/// `INTERNAL_REF_PAYLOAD_WORDS` (3): `is_internal_ref` tests the subtag only and is therefore also
/// true for the C `ErtsORefThing` (header arity 2, 3 words), whose block this crate must not copy —
/// copying [`REF_THING_BYTES`] out of a 3-word block would read past it. That arity check is the
/// same precondition `beam_term::boxed::internal_ref_id` enforces. `None` is the sound refusal: the
/// caller answers the C's `1` (`enif_monitor_process`) or `badarg`
/// (`enif_make_monitor_term`) rather than storing or rebuilding an unusable payload.
pub(super) fn ref_block_of(term: Eterm) -> Option<[u8; REF_THING_BYTES]> {
    if !is_term_word(term) {
        return None;
    }
    // SAFETY: `is_internal_ref` dereferences only the header word of a boxed term.
    if !unsafe { prelude::is_internal_ref(term) } {
        return None;
    }
    let p = prelude::boxed_val(term);
    // SAFETY: `is_internal_ref` confirmed a boxed term, so its header word is a live, aligned term
    // word.
    if prelude::header_arity(unsafe { *p }) != prelude::INTERNAL_REF_PAYLOAD_WORDS {
        // A C-layout ref block (arity 2): its block is one word shorter than
        // `REF_THING_BYTES`, so refuse it instead of reading past it.
        return None;
    }
    let mut block = [0u8; REF_THING_BYTES];
    // SAFETY: the header arity says the block has `INTERNAL_REF_PAYLOAD_WORDS` payload words, so
    // `[header, node, serial, id]` is exactly `REF_THING_BYTES` bytes long.
    unsafe { core::ptr::copy_nonoverlapping(p.cast::<u8>(), block.as_mut_ptr(), REF_THING_BYTES) };
    Some(block)
}

/// Copy the reference block `block` onto `env`'s heap and return the tagged internal reference (the C
/// `erts_driver_monitor_to_ref`).
///
/// The allocation is [`REF_THING_WORDS`] words and the block is exactly that long, so the rebuilt
/// term is a valid frozen 4-word M1 internal reference (`[header, node, serial, id]`) — the same
/// shape `beam_term::boxed::write_internal_ref` builds. Before the block was widened to the frozen
/// block's word count this under-allocated (3 words for a header claiming 4), so a term rebuilt
/// through `enif_make_monitor_term` had its id word past the allocation; that out-of-bounds read is
/// closed by the widened allocation copying the whole block.
///
/// # Safety
///
/// `env` must be a live env (the returned term points into its host's heap).
unsafe fn ref_term_from_block(env: &mut NifEnv, block: &[u8; REF_THING_BYTES]) -> Option<Eterm> {
    // SAFETY: a bump allocation of `REF_THING_WORDS` words; exhaustion is reported as `None`.
    let hp = unsafe { alloc(env, REF_THING_WORDS) }?;
    // SAFETY: `hp` addresses `REF_THING_WORDS` freshly reserved, exclusively owned words, and the
    // block is exactly `REF_THING_WORDS * size_of::<Eterm>()` bytes.
    unsafe { core::ptr::copy_nonoverlapping(block.as_ptr(), hp.cast::<u8>(), REF_THING_BYTES) };
    Some(prelude::make_boxed(hp))
}

// ---------------------------------------------------------------------------------------------
// Messaging and port commands
// ---------------------------------------------------------------------------------------------

/// `int enif_send(ErlNifEnv* env, const ErlNifPid* to_pid, ErlNifEnv* msg_env, ERL_NIF_TERM msg);`
///
/// Returns `1` when the message was sent and `0` when it was not (dead receiver, no host support, or
/// an unusable `env`/`to_pid`) — the C's only two outcomes.
///
/// # Safety
///
/// `env`/`msg_env` must be null or live envs; `to_pid` must be null or point at a readable
/// `ErlNifPid`.
#[no_mangle]
pub unsafe extern "C" fn enif_send(
    env: *mut crate::nif::types::ErlNifEnv,
    to_pid: *const ErlNifPid,
    _msg_env: *mut crate::nif::types::ErlNifEnv,
    msg: ERL_NIF_TERM,
) -> c_int {
    if to_pid.is_null() {
        // The C dereferences `to_pid` unconditionally (`erl_nif.c:870`).
        return 0;
    }
    // SAFETY: `to_pid` is non-null and readable (caller contract).
    let receiver = t(unsafe { (*to_pid).pid });
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    // `None`: the sender is the calling process (see the module docs).
    match e.host().send(receiver, None, t(msg)) {
        Some(sent) => sent as c_int,
        None => 0,
    }
}

/// `int enif_port_command(ErlNifEnv* env, const ErlNifPort* to_port, ErlNifEnv* msg_env, ERL_NIF_TERM msg);`
///
/// Returns `1` when the port accepted the command and `0` otherwise (unknown port, no host support,
/// or an unusable `env`/`to_port`).
///
/// # Safety
///
/// `env`/`msg_env` must be null or live envs; `to_port` must be null or point at a readable
/// `ErlNifPort`.
#[no_mangle]
pub unsafe extern "C" fn enif_port_command(
    env: *mut crate::nif::types::ErlNifEnv,
    to_port: *const ErlNifPort,
    _msg_env: *mut crate::nif::types::ErlNifEnv,
    msg: ERL_NIF_TERM,
) -> c_int {
    // The C aborts the emulator on a null env (`erl_nif.c:1108-1109`); "not delivered" is the safe
    // substitute — and without an env there is no host to deliver through either.
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    if to_port.is_null() {
        // The C dereferences `to_port` (`erl_nif.c:1117`).
        return 0;
    }
    // SAFETY: `to_port` is non-null and readable (caller contract).
    let port = t(unsafe { (*to_port).port_id });
    match e.host().port_command(port, t(msg)) {
        Some(sent) => sent as c_int,
        None => 0,
    }
}

// ---------------------------------------------------------------------------------------------
// Registered-name lookup
// ---------------------------------------------------------------------------------------------

/// `int enif_whereis_pid(ErlNifEnv *env, ERL_NIF_TERM name, ErlNifPid *pid);`
///
/// Returns `1` and writes `*pid` when `name` is an atom registered to a **local pid**, `0`
/// otherwise.
///
/// # Safety
///
/// `env` must be null or a live env; `pid` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_whereis_pid(
    env: *mut crate::nif::types::ErlNifEnv,
    name: ERL_NIF_TERM,
    pid: *mut ErlNifPid,
) -> c_int {
    let name = t(name);
    if !prelude::is_atom(name) {
        return 0;
    }
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let Some(found) = e.host().whereis_pid(name) else {
        return 0;
    };
    if !prelude::is_internal_pid(found) {
        return 0;
    }
    // SAFETY: `pid` is null (reported as "not found") or writable (caller contract).
    (unsafe { write_out(pid, ErlNifPid { pid: n(found) }) }) as c_int
}

/// `int enif_whereis_port(ErlNifEnv *env, ERL_NIF_TERM name, ErlNifPort *port);`
///
/// The port counterpart of [`enif_whereis_pid`].
///
/// # Safety
///
/// `env` must be null or a live env; `port` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_whereis_port(
    env: *mut crate::nif::types::ErlNifEnv,
    name: ERL_NIF_TERM,
    port: *mut ErlNifPort,
) -> c_int {
    let name = t(name);
    if !prelude::is_atom(name) {
        return 0;
    }
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let Some(found) = e.host().whereis_port(name) else {
        return 0;
    };
    if !prelude::is_internal_port(found) {
        return 0;
    }
    // SAFETY: `port` is null (reported as "not found") or writable (caller contract).
    (unsafe { write_out(port, ErlNifPort { port_id: n(found) }) }) as c_int
}

// ---------------------------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------------------------

/// `int enif_is_current_process_alive(ErlNifEnv *env);`
///
/// Returns `1` when the calling process is alive. The C refuses a non-scheduler or process-less
/// environment by aborting the emulator (`erl_nif.c:2311-2318`); an unusable `env` answers the C's
/// "not alive" `0` here. When the host implements no explicit `is_current_process_alive` hook it is
/// answered from [`NifHost::self_pid`]: a host that can name the calling process (`beam_rt` does)
/// proves the process exists, which is the C's own condition (`erts_get_current_process()` non-null).
/// Only a host that can do neither answers `0`.
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_is_current_process_alive(
    env: *mut crate::nif::types::ErlNifEnv,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    match e.host().is_current_process_alive() {
        Some(alive) => alive as c_int,
        // No hook: the calling process is alive iff the host can name it.
        None => e.host().self_pid().is_some() as c_int,
    }
}

/// `int enif_is_process_alive(ErlNifEnv *env, ErlNifPid *pid);`
///
/// Returns `1` when `*pid` names a live local process — the C's `!!erts_proc_lookup(proc->pid)`
/// (`erl_nif.c:2330`). The **calling** process is answered first, from [`NifHost::self_pid`]: the C's
/// `erts_proc_lookup` always finds it, and the real runtime has no `is_process_alive` hook, so this is
/// what makes `enif_is_process_alive(env, enif_self(env, &pid))` answer the oracle's `1` there. Every
/// other pid is the host's [`NifHost::is_process_alive`] answer (a dead, foreign or malformed id is
/// the host's `false`; an unsupported host answers `0`).
///
/// # Safety
///
/// `env` must be null or a live env; `pid` must be null or point at a readable `ErlNifPid`.
#[no_mangle]
pub unsafe extern "C" fn enif_is_process_alive(
    env: *mut crate::nif::types::ErlNifEnv,
    pid: *mut ErlNifPid,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    if pid.is_null() {
        // The C dereferences `pid` (`erl_nif.c:2330`).
        return 0;
    }
    // SAFETY: `pid` is non-null and readable (caller contract).
    let pid = t(unsafe { (*pid).pid });
    // The calling process is alive by definition, and the host can always name it through
    // `self_pid`; answering this *before* the hook is what keeps the answer correct on a host
    // without an `is_process_alive` hook (the real runtime).
    if let Some(self_pid) = e.host().self_pid() {
        if self_pid == pid {
            return 1;
        }
    }
    matches!(e.host().is_process_alive(pid), Some(true)) as c_int
}

/// `int enif_is_port_alive(ErlNifEnv *env, ErlNifPort *port_id);`
///
/// The port counterpart of [`enif_is_process_alive`].
///
/// # Safety
///
/// `env` must be null or a live env; `port_id` must be null or point at a readable `ErlNifPort`.
#[no_mangle]
pub unsafe extern "C" fn enif_is_port_alive(
    env: *mut crate::nif::types::ErlNifEnv,
    port_id: *mut ErlNifPort,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    if port_id.is_null() {
        // The C dereferences `port_id` (`erl_nif.c:2350`).
        return 0;
    }
    // SAFETY: `port_id` is non-null and readable (caller contract).
    let port = t(unsafe { (*port_id).port_id });
    matches!(e.host().is_port_alive(port), Some(true)) as c_int
}

// ---------------------------------------------------------------------------------------------
// Monitors
// ---------------------------------------------------------------------------------------------

/// `int enif_monitor_process(ErlNifEnv*, void* obj, const ErlNifPid*, ErlNifMonitor *monitor);`
///
/// Reproduces the C's three codes **in the C's own order** (`erl_nif.c:3954-4011`):
///
/// 1. **`-1`** ([`MONITOR_NO_DOWN_CALLBACK`]) when `obj` is a resource object of a type this crate
///    opened **without** a `down` callback (`:3968-3972`). This is tested *first*, before the pid is
///    even read, because that is what the C does — so an undefined target on such a resource answers
///    `-1`, not `1` (measured on the oracle). The answer comes from the crate's own resource registry
///    ([`super::resources::resource_type_has_down`]), so it needs no host; an `obj` this crate did
///    not mint keeps the historical behaviour.
/// 2. **`1`** ([`MONITOR_TARGET_DEAD`]) for a null `target_pid`, an `enif_set_pid_undefined` pid
///    (`:3975-3976`), or a target the host reports as already dead (`:4004`). `*monitor` is left
///    untouched — the C only stores on the success path (`:4007`).
/// 3. **`0`** ([`MONITOR_ESTABLISHED`]) after storing the monitor's reference block — the whole
///    [`REF_THING_BYTES`]-byte frozen internal-reference block — in `*monitor` (`:4007-4010`); a null
///    `monitor` pointer is legal there and is honoured.
///
/// The arm itself is the host's: [`NifHost::monitor_process_result`] is consulted first (it can
/// express all three outcomes), and when it answers `None` the legacy single-outcome
/// [`NifHost::monitor_process`] is used exactly as before, so no existing host changes behaviour.
///
/// # Safety
///
/// `env` must be null or a live env; `target_pid` must be null or point at a readable `ErlNifPid`;
/// `monitor` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_monitor_process(
    env: *mut crate::nif::types::ErlNifEnv,
    obj: *mut c_void,
    target_pid: *const ErlNifPid,
    monitor: *mut ErlNifMonitor,
) -> c_int {
    // The C reads the resource's monitor table first (`rm = rsrc->monitors; if (!rm) return -1;`,
    // `erl_nif.c:3968-3972`) — before the pid, which is why an undefined target still answers `-1`
    // on a monitor-incapable resource (measured). `None` = "not an object of ours": fall through to
    // the historical behaviour rather than dereferencing a foreign pointer.
    if super::resources::resource_type_has_down(obj) == Some(false) {
        return MONITOR_NO_DOWN_CALLBACK;
    }
    let Some(e) = (unsafe { env_mut(env) }) else {
        return MONITOR_TARGET_DEAD;
    };
    if target_pid.is_null() {
        // The C dereferences `target_pid` (`erl_nif.c:3975`).
        return MONITOR_TARGET_DEAD;
    }
    // SAFETY: `target_pid` is non-null and readable (caller contract).
    let pid = t(unsafe { (*target_pid).pid });
    if pid.raw() == UNDEFINED_PID.raw() {
        // `enif_set_pid_undefined` wrote the "undefined" marker (`erl_nif.c:3975-3976`).
        return MONITOR_TARGET_DEAD;
    }
    // The three-way hook first: it is the only seam that can say `-1` or distinguish "unsupported"
    // from "dead". A host that declines (`None`) falls back to the legacy single-outcome hook, whose
    // contract (a term, or `None` for "nothing established") is unchanged.
    let armed = match e.host().monitor_process_result(pid) {
        Some(NifMonitorResult::Armed(term)) => Some(term),
        Some(NifMonitorResult::NotAlive) => return MONITOR_TARGET_DEAD,
        Some(NifMonitorResult::NoDownCallback) => return MONITOR_NO_DOWN_CALLBACK,
        None => e.host().monitor_process(pid),
    };
    let Some(term) = armed else {
        return MONITOR_TARGET_DEAD;
    };
    // The hook's term must be the ordinary internal reference the C builds with
    // `erts_make_ref_in_buffer`; storing anything else would hand the caller a payload
    // `enif_make_monitor_term`/`enif_demonitor_process` could not reconstruct.
    let Some(block) = ref_block_of(term) else {
        return MONITOR_TARGET_DEAD;
    };
    // The C inserts the monitor into the **resource object's own monitor tree** before it copies the
    // reference block into the caller's holder (`erts_monitor_tree_insert(&rm->root, &mdp->origin)`,
    // `erl_nif.c:3983-3986`, then `erts_ref_to_driver_monitor`, `:4008-4009`) — that tree is what
    // `down` is later fired from and what `enif_demonitor_process` looks the reference up in. Our
    // stand-in is the registry's one monitor table (`super::resources`'s `monitors` module); an `obj`
    // that is not a live payload pointer of this crate is simply not tracked, exactly as the C's
    // `DATA_TO_RESOURCE(obj)` has to be one of its own.
    super::resources::record_monitor(obj, &block, pid);
    store_monitor(monitor, &block);
    // The C's `0` does not depend on any bookkeeping of ours, so the code is the same either way.
    MONITOR_ESTABLISHED
}

/// `int enif_demonitor_process(ErlNifEnv*, void* obj, const ErlNifMonitor *monitor);`
///
/// Returns [`DEMONITOR_REMOVED`] (`0`) when the monitor was identified and removed, and
/// [`DEMONITOR_NOT_FOUND`] (`1`) when it could not be — never created, already cancelled or already
/// triggered (`erl_nif.c:4043-4051`, `erl_nif.md:1177-1183`).
///
/// The C keys the lookup on the **resource's** monitor tree
/// (`erts_monitor_tree_lookup(DATA_TO_RESOURCE(obj)->monitors->root, ref)`, `:4046`); this crate's
/// single registry table (`super::resources`'s `monitors`) stands in for that per-object tree, so the
/// `obj` operand is not consulted — it is kept in the signature because the C ABI has it and because a
/// foreign object's monitor is the host's business. The answer is our own removal **or** the host's
/// cancel (`NifHost::demonitor_process`), so an object this crate did not mint keeps its previous
/// behaviour.
///
/// # Safety
///
/// `env` must be null or a live env; `monitor` must be null or point at a readable `ErlNifMonitor`.
#[no_mangle]
pub unsafe extern "C" fn enif_demonitor_process(
    env: *mut crate::nif::types::ErlNifEnv,
    _obj: *mut c_void,
    monitor: *const ErlNifMonitor,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return DEMONITOR_NOT_FOUND;
    };
    let Some(block) = (unsafe { monitor_block(monitor) }) else {
        return DEMONITOR_NOT_FOUND;
    };
    // Our tree first: a monitor this crate armed is ours to cancel, and its removal is authoritative
    // even when the host has forgotten it (`:4046-4048`).
    let ours = super::resources::take_monitor(&block);
    // The host's cancel, through the reference rebuilt on the caller's heap (the C rebuilds it with
    // `erts_driver_monitor_to_ref` and hands it to `erts_proc_sig_send_demonitor`).
    // SAFETY: `e` is a live env, so the rebuilt reference lives on a live heap.
    let host_cancelled = match unsafe { ref_term_from_block(e, &block) } {
        Some(term) => matches!(e.host().demonitor_process(term), Some(true)),
        None => false,
    };
    if ours || host_cancelled {
        DEMONITOR_REMOVED
    } else {
        DEMONITOR_NOT_FOUND
    }
}

/// `int enif_compare_monitors(const ErlNifMonitor *monitor1, const ErlNifMonitor *monitor2);`
///
/// A `memcmp`-style ordering over the stored monitor payload, byte for byte (`erl_nif.c:4054-4059`).
///
/// The C compares `ERTS_REF_THING_SIZE * sizeof(Eterm)` = 24 bytes because its 3 packed `num[]` ids
/// carry the whole reference identity. Here the compared range is the whole stored reference block —
/// [`REF_THING_BYTES`] bytes, the entire `ErlNifMonitor` payload — because this tree's frozen 4-word
/// M1 block keeps the identity in word 3; comparing fewer bytes would make two distinct monitors
/// compare equal.
///
/// # Safety
///
/// Each pointer must be null or point at a readable [`ErlNifMonitor`].
#[no_mangle]
pub unsafe extern "C" fn enif_compare_monitors(
    monitor1: *const ErlNifMonitor,
    monitor2: *const ErlNifMonitor,
) -> c_int {
    // SAFETY: both pointers are null or readable (caller contract).
    let a = unsafe { monitor_block(monitor1) };
    let b = unsafe { monitor_block(monitor2) };
    match (a, b) {
        (Some(a), Some(b)) => match a.cmp(&b) {
            core::cmp::Ordering::Less => -1,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => 1,
        },
        // The C reads through both pointers; a total order over "no payload" is the documented
        // substitute (never a fabricated equality).
        (None, None) => 0,
        (None, Some(_)) => -1,
        (Some(_), None) => 1,
    }
}

/// `ERL_NIF_TERM enif_make_monitor_term(ErlNifEnv* env, const ErlNifMonitor*);`
///
/// Builds an ordinary internal reference holding the monitor's payload — the C's
/// `alloc_heap(env, ERTS_REF_THING_SIZE)` + `erts_driver_monitor_to_ref`, with this tree's frozen
/// block width ([`REF_THING_WORDS`] words, so the id word is materialised). `badarg` is returned
/// (never a fabricated term) for an unusable `env` or a null `monitor`.
///
/// # Safety
///
/// `env` must be null or a live env; `monitor` must be null or point at a readable `ErlNifMonitor`.
#[no_mangle]
pub unsafe extern "C" fn enif_make_monitor_term(
    env: *mut crate::nif::types::ErlNifEnv,
    monitor: *const ErlNifMonitor,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    let Some(block) = (unsafe { monitor_block(monitor) }) else {
        return bare_badarg();
    };
    // SAFETY: `e` is a live env, so the rebuilt reference lives on a live heap.
    match unsafe { ref_term_from_block(e, &block) } {
        Some(term) => n(term),
        None => bare_badarg(),
    }
}

// ---------------------------------------------------------------------------------------------
// Dirty-scheduler NIF scheduling
// ---------------------------------------------------------------------------------------------

/// `ERL_NIF_TERM enif_schedule_nif(ErlNifEnv*, const char*, int, ERL_NIF_TERM (*)(ErlNifEnv*,int,const ERL_NIF_TERM[]), int, const ERL_NIF_TERM[]);`
///
/// Delegates to `NifHost::schedule_nif` (the runtime's reschedule onto a regular or dirty scheduler)
/// and returns the term the scheduled call will produce. Raises `badarg` for `argc > MAX_ARG`, a
/// `flags` bit outside the two dirty-job bits, a null `fun_name`, a null function pointer, a
/// negative `argc`, or no host support — it never pretends to have scheduled anything.
///
/// # Safety
///
/// `env` must be null or a live env; `fun_name` must be null or a NUL-terminated string; `fptr` must
/// be null or a valid NIF entry point; `argv` must be null or readable for `argc` terms.
#[no_mangle]
pub unsafe extern "C" fn enif_schedule_nif(
    env: *mut crate::nif::types::ErlNifEnv,
    fun_name: *const c_char,
    flags: c_int,
    fptr: Option<
        unsafe extern "C" fn(
            *mut crate::nif::types::ErlNifEnv,
            c_int,
            *const ERL_NIF_TERM,
        ) -> ERL_NIF_TERM,
    >,
    argc: c_int,
    _argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    // C: `if (argc > MAX_ARG) return enif_make_badarg(env);` (`erl_nif.c:3569`). A negative `argc`
    // cannot be expressed to the hook (which counts with a `usize`) and is refused too.
    if argc > MAX_ARG || argc < 0 {
        // SAFETY: delegated to `enif_make_badarg`, which tolerates a null env.
        return unsafe { enif_make_badarg(env) };
    }
    // C: `flags` must be `0` or a subset of the two dirty-job bits (`erl_nif.c:3584-3598`).
    if flags != 0 && (flags & !(ERL_DIRTY_JOB_CPU_BOUND | ERL_DIRTY_JOB_IO_BOUND)) != 0 {
        // SAFETY: as above.
        return unsafe { enif_make_badarg(env) };
    }
    // SAFETY: `fun_name` is null (rejected) or NUL-terminated (caller contract).
    let Some(name) = (unsafe { cstr(fun_name) }) else {
        // The C would fault in `enif_make_atom(env, NULL)` (`erl_nif.c:3571`).
        // SAFETY: as above.
        return unsafe { enif_make_badarg(env) };
    };
    let Ok(name) = core::str::from_utf8(name) else {
        // The hook names the NIF with a `&str`; the C interns raw bytes.
        // SAFETY: as above.
        return unsafe { enif_make_badarg(env) };
    };
    let Some(fptr) = fptr else {
        // The C would call through the null pointer.
        // SAFETY: as above.
        return unsafe { enif_make_badarg(env) };
    };
    let scheduled = match unsafe { env_mut(env) } {
        Some(e) => e.host().schedule_nif(name, flags, fptr, argc as usize),
        None => None,
    };
    match scheduled {
        Some(term) => n(term),
        // SAFETY: as above — no host, so nothing was scheduled.
        None => unsafe { enif_make_badarg(env) },
    }
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
