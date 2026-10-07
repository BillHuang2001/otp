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
//! # The monitor payload is the C's reference block
//!
//! `ErlNifMonitor` is `typedef ErlDrvMonitor ErlNifMonitor`, an opaque `unsigned char data[32]`
//! that carries an *ordinary internal reference*: `erts_ref_to_driver_monitor` byte-copies
//! `ERTS_REF_THING_SIZE * sizeof(Eterm)` (**24** bytes — the 3-word `[header, w1, w2]` block) into
//! it and `erts_driver_monitor_to_ref` copies it back onto a heap and re-tags the pointer
//! (`io.c:6994-7010`). This file reproduces that exactly, so an `ErlNifMonitor` produced here is
//! byte-identical to the C's and [`enif_make_monitor_term`] really returns an internal reference:
//!
//! * [`enif_monitor_process`] stores the 3-word block of the internal reference returned by the
//!   `NifHost::monitor_process` hook (the C's `erts_make_ref_in_buffer`). The hook must therefore
//!   return an internal reference; anything else is refused as `1` ("could not establish") rather
//!   than stored, so no un-reconstructable payload is ever fabricated.
//! * Bytes 24..32 are zeroed (the C leaves them untouched and never reads them), and
//!   [`enif_compare_monitors`], like the C, compares only the first 24.
//!
//! # Documented substitutions — never a wrong value, never a crash
//!
//! * **A `NULL` `obj` is ignored.** The C reaches the resource's monitor table through `obj`
//!   (`DATA_TO_RESOURCE`); a resource handle is an opaque `void *` here (the resource registry
//!   belongs to the `resources` family and the runtime) and the monitor hooks take the pid/monitor
//!   term alone.
//! * **`enif_monitor_process` never returns the C's `-1`.** That code is chosen from the *resource
//!   type* (`rsrc->monitors` / `type->fn.down`, `:3968-3973`): a type created without a `down`
//!   callback cannot hold monitors. That fact is not reachable from an opaque `obj`, so a monitor
//!   that cannot be established (dead or foreign pid, an undefined `ErlNifPid`, an unusable
//!   `env`/`target_pid`, or a hook that declines) is reported as the C's "target not alive" `1` —
//!   never as a fabricated `0`.
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
//!   `memcmp`-style over the C's 24 bytes, byte for byte.

use super::*;

use libc::{c_char, c_int, c_void};

use crate::common::{ERL_DIRTY_JOB_CPU_BOUND, ERL_DIRTY_JOB_IO_BOUND};
use crate::nif::types::{ErlNifMonitor, ErlNifPid, ErlNifPort};

/// `MAX_ARG` (`erl_vm.h:48`) — the largest argument count `enif_schedule_nif` accepts.
const MAX_ARG: c_int = 255;

/// The C's "the monitor was established" code (`erl_nif.c:4010`).
const MONITOR_ESTABLISHED: c_int = 0;

/// The C's "the target process is not alive / `target_pid` is undefined" code
/// (`erl_nif.c:3976`, `:4004`).
const MONITOR_TARGET_DEAD: c_int = 1;

/// The C's "the monitor was identified and removed" code (`erl_nif.c:4051`).
const DEMONITOR_REMOVED: c_int = 0;

/// The C's "the monitor could not be identified" code (`erl_nif.c:4043-4044`).
const DEMONITOR_NOT_FOUND: c_int = 1;

/// `ERTS_REF_THING_SIZE` (`erl_term.h:990`) — an ordinary internal reference on LP64 is
/// `ErtsORefThing { Eterm header; uint32 num[3]; uint32 marker; }`, i.e. 3 machine words.
const REF_THING_WORDS: usize = 3;

/// `ERTS_REF_THING_SIZE * sizeof(Eterm)` — the `ErlNifMonitor` bytes the C copies (`io.c:6994-7010`)
/// and compares (`erl_nif.c:4057`).
const REF_THING_BYTES: usize = REF_THING_WORDS * core::mem::size_of::<Eterm>();

/// The `ErlNifMonitor` payload length, spelled as the type declares it (`sizeof(void *) * 4`).
const MONITOR_BYTES: usize = core::mem::size_of::<*mut c_void>() * 4;

// `ERTS_CT_ASSERT(ERTS_REF_THING_SIZE*sizeof(Uint) <= sizeof(ErlDrvMonitor))` (`io.c:6996`).
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
// The monitor payload — the C's 3-word reference block (`io.c:6994-7010`)
// ---------------------------------------------------------------------------------------------

/// The first [`REF_THING_BYTES`] bytes of `monitor` — the reference block it carries (`None` for a
/// null pointer).
///
/// # Safety
///
/// `monitor` must be null or point at a readable [`ErlNifMonitor`].
unsafe fn monitor_block(monitor: *const ErlNifMonitor) -> Option<[u8; REF_THING_BYTES]> {
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

/// Store `block` in `monitor` (the C `erts_ref_to_driver_monitor`).
///
/// A null `monitor` is legal in the C (`if (monitor) …`, `erl_nif.c:4007`). The bytes after the
/// reference block are zeroed; the C leaves them untouched and never reads them.
fn store_monitor(monitor: *mut ErlNifMonitor, block: &[u8; REF_THING_BYTES]) {
    if monitor.is_null() {
        return;
    }
    let mut data = [0u8; MONITOR_BYTES];
    data[..REF_THING_BYTES].copy_from_slice(block);
    // SAFETY: `monitor` is non-null and writable (caller contract); `ErlNifMonitor` is `#[repr(C)]`
    // over exactly `MONITOR_BYTES` bytes.
    unsafe { monitor.write(ErlNifMonitor { data }) };
}

/// The 3-word reference block of the internal reference `term` (`None` when it is not one) — the C's
/// `is_internal_ordinary_ref(ref)` + `internal_ref_val(ref)` pair (`io.c:6997-6999`).
fn ref_block_of(term: Eterm) -> Option<[u8; REF_THING_BYTES]> {
    if !is_term_word(term) {
        return None;
    }
    // SAFETY: `is_internal_ref` dereferences only the header word of a boxed term.
    if !unsafe { prelude::is_internal_ref(term) } {
        return None;
    }
    let p = prelude::boxed_val(term);
    let mut block = [0u8; REF_THING_BYTES];
    // SAFETY: `term` is a live internal reference, so `boxed_val` points at its `[header, w1, w2]`
    // block, which is exactly `REF_THING_BYTES` bytes long.
    unsafe { core::ptr::copy_nonoverlapping(p.cast::<u8>(), block.as_mut_ptr(), REF_THING_BYTES) };
    Some(block)
}

/// Copy the reference block `block` onto `env`'s heap and return the tagged internal reference (the C
/// `erts_driver_monitor_to_ref`).
///
/// # Safety
///
/// `env` must be a live env (the returned term points into its host's heap).
unsafe fn ref_term_from_block(env: &mut NifEnv, block: &[u8; REF_THING_BYTES]) -> Option<Eterm> {
    // SAFETY: a bump allocation of `REF_THING_WORDS` words; exhaustion is reported as `None`.
    let hp = unsafe { alloc(env, REF_THING_WORDS) }?;
    // SAFETY: `hp` addresses `REF_THING_WORDS` freshly reserved, exclusively owned words.
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
/// environment by aborting the emulator (`erl_nif.c:2311-2318`); an unusable `env` — or a host that
/// does not implement the hook — answers the C's "not alive" `0` here.
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
    matches!(e.host().is_current_process_alive(), Some(true)) as c_int
}

/// `int enif_is_process_alive(ErlNifEnv *env, ErlNifPid *pid);`
///
/// Returns `1` when `*pid` names a live local process — the C's `!!erts_proc_lookup(proc->pid)`
/// (`erl_nif.c:2330`). The pid term is opaque at this layer; a dead, foreign or malformed id is the
/// host's `false` (the C's lookup failure), and an unsupported host answers `0`.
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
/// Returns [`MONITOR_ESTABLISHED`] (`0`) when the monitor was created and stored in `*monitor`, and
/// [`MONITOR_TARGET_DEAD`] (`1`) otherwise. The C's `-1` ("no `down` callback") is not reachable
/// from an opaque `obj` — see the module docs.
///
/// # Safety
///
/// `env` must be null or a live env; `target_pid` must be null or point at a readable `ErlNifPid`;
/// `monitor` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_monitor_process(
    env: *mut crate::nif::types::ErlNifEnv,
    _obj: *mut c_void,
    target_pid: *const ErlNifPid,
    monitor: *mut ErlNifMonitor,
) -> c_int {
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
    let Some(term) = e.host().monitor_process(pid) else {
        return MONITOR_TARGET_DEAD;
    };
    // The hook's term must be the ordinary internal reference the C builds with
    // `erts_make_ref_in_buffer`; storing anything else would hand the caller a payload
    // `enif_make_monitor_term`/`enif_demonitor_process` could not reconstruct.
    let Some(block) = ref_block_of(term) else {
        return MONITOR_TARGET_DEAD;
    };
    store_monitor(monitor, &block);
    MONITOR_ESTABLISHED
}

/// `int enif_demonitor_process(ErlNifEnv*, void* obj, const ErlNifMonitor *monitor);`
///
/// Returns [`DEMONITOR_REMOVED`] (`0`) when the hook reports the monitor identified and removed, and
/// [`DEMONITOR_NOT_FOUND`] (`1`) when it could not be — never created, already cancelled or already
/// triggered (`erl_nif.c:4043-4051`, `erl_nif.md:1177-1183`).
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
    // SAFETY: `e` is a live env, so the rebuilt reference lives on a live heap.
    let Some(term) = (unsafe { ref_term_from_block(e, &block) }) else {
        return DEMONITOR_NOT_FOUND;
    };
    match e.host().demonitor_process(term) {
        Some(true) => DEMONITOR_REMOVED,
        Some(false) | None => DEMONITOR_NOT_FOUND,
    }
}

/// `int enif_compare_monitors(const ErlNifMonitor *monitor1, const ErlNifMonitor *monitor2);`
///
/// A `memcmp`-style ordering over the monitor payload: the C compares exactly
/// `ERTS_REF_THING_SIZE * sizeof(Eterm)` = 24 bytes (`erl_nif.c:4054-4059`), leaving the rest of the
/// 32-byte holder outside the comparison.
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
/// `alloc_heap(env, ERTS_REF_THING_SIZE)` + `erts_driver_monitor_to_ref`. `badarg` is returned (never
/// a fabricated term) for an unusable `env` or a null `monitor`.
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
mod tests {
    //! Family-local tests: a [`NifHost`] answering every process-family hook from in-memory tables,
    //! driven through the exported `enif_*` functions.

    use super::*;

    use crate::nif::host::NifHost;
    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;
    use beam_term::prelude::{
        atom_val, is_atom, make_atom, make_internal_pid, make_internal_port, make_small,
    };

    /// A host whose hooks answer from hand-written tables.
    struct ProcHost {
        heap: Heap,
        atoms: AtomTable,
        self_pid: Option<Eterm>,
        /// Pids the host calls alive.
        live_pids: Vec<Eterm>,
        /// Ports the host calls alive.
        live_ports: Vec<Eterm>,
        /// Registered names → the pid/port term they are registered to.
        named: Vec<(String, Eterm)>,
        /// Every accepted `send`: `(receiver, message)`.
        sent: Vec<(Eterm, Eterm)>,
        /// Every accepted `port_command`: `(port, data)`.
        port_cmds: Vec<(Eterm, Eterm)>,
        /// The payloads of the monitors currently held.
        monitors: Vec<(u64, u64)>,
        /// The next reference id `monitor_process` hands out.
        next_ref: u64,
        /// Whether `schedule_nif` is supported at all.
        schedule_ok: bool,
        /// Every accepted `schedule_nif`: `(name, flags, argc)`.
        scheduled: Vec<(String, c_int, usize)>,
    }

    impl ProcHost {
        fn new(schedule_ok: bool) -> ProcHost {
            ProcHost {
                heap: Heap::new(1024).expect("a test heap is allocatable"),
                atoms: AtomTable::new(),
                self_pid: None,
                live_pids: Vec::new(),
                live_ports: Vec::new(),
                named: Vec::new(),
                sent: Vec::new(),
                port_cmds: Vec::new(),
                monitors: Vec::new(),
                next_ref: 1,
                schedule_ok,
                scheduled: Vec::new(),
            }
        }
    }

    /// The internal reference `erts_make_ref_in_buffer` would build for `id`.
    fn ref_term(heap: &mut Heap, id: u64) -> Eterm {
        // SAFETY: `REF_THING_WORDS` fresh, exclusively owned words from `heap`.
        let p = unsafe { heap.alloc_words(REF_THING_WORDS) }.expect("the test heap has room");
        // SAFETY: `p` addresses those words; the block is a valid ref thing.
        unsafe {
            *p = prelude::make_header(REF_THING_WORDS - 1, prelude::REF_SUBTAG);
            *p.add(1) = Eterm::from_raw(id as usize);
            *p.add(2) = Eterm::from_raw(0);
        }
        prelude::make_boxed(p)
    }

    /// The `(w1, w2)` payload of an internal reference term (`None` when `term` is not one).
    fn ref_payload(term: Eterm) -> Option<(u64, u64)> {
        if !is_term_word(term) || !unsafe { prelude::is_internal_ref(term) } {
            return None;
        }
        let p = prelude::boxed_val(term);
        // SAFETY: `term` is a live internal reference, so its block's words 1 and 2 exist.
        unsafe { Some(((*(p.add(1))).raw() as u64, (*(p.add(2))).raw() as u64)) }
    }

    impl NifHost for ProcHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }
        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }
        fn atom_name(&mut self, atom: Eterm) -> Option<String> {
            if !is_atom(atom) {
                return None;
            }
            self.atoms.name(atom_val(atom)).map(str::to_string)
        }
        fn self_pid(&mut self) -> Option<Eterm> {
            self.self_pid
        }
        fn is_current_process_alive(&mut self) -> Option<bool> {
            Some(self.self_pid.is_some())
        }
        fn is_process_alive(&mut self, pid: Eterm) -> Option<bool> {
            Some(self.live_pids.iter().any(|p| p.raw() == pid.raw()))
        }
        fn is_port_alive(&mut self, port: Eterm) -> Option<bool> {
            Some(self.live_ports.iter().any(|p| p.raw() == port.raw()))
        }
        fn send(&mut self, to: Eterm, _from: Option<Eterm>, msg: Eterm) -> Option<bool> {
            let alive = self.live_pids.iter().any(|p| p.raw() == to.raw());
            if alive {
                self.sent.push((to, msg));
            }
            Some(alive)
        }
        fn port_command(&mut self, port: Eterm, data: Eterm) -> Option<bool> {
            let alive = self.live_ports.iter().any(|p| p.raw() == port.raw());
            if alive {
                self.port_cmds.push((port, data));
            }
            Some(alive)
        }
        fn whereis_pid(&mut self, name: Eterm) -> Option<Eterm> {
            let name = self.atom_name(name)?;
            self.named
                .iter()
                .find(|(n, v)| *n == name && prelude::is_internal_pid(*v))
                .map(|(_, v)| *v)
        }
        fn whereis_port(&mut self, name: Eterm) -> Option<Eterm> {
            let name = self.atom_name(name)?;
            self.named
                .iter()
                .find(|(n, v)| *n == name && prelude::is_internal_port(*v))
                .map(|(_, v)| *v)
        }
        fn monitor_process(&mut self, pid: Eterm) -> Option<Eterm> {
            if !self.live_pids.iter().any(|p| p.raw() == pid.raw()) {
                return None;
            }
            let id = self.next_ref;
            self.next_ref += 1;
            self.monitors.push((id, 0));
            Some(ref_term(&mut self.heap, id))
        }
        fn demonitor_process(&mut self, monitor: Eterm) -> Option<bool> {
            let Some(payload) = ref_payload(monitor) else {
                return Some(false);
            };
            let before = self.monitors.len();
            self.monitors.retain(|m| *m != payload);
            Some(self.monitors.len() != before)
        }
        fn schedule_nif(
            &mut self,
            fun_name: &str,
            flags: c_int,
            _fptr: unsafe extern "C" fn(
                *mut crate::nif::types::ErlNifEnv,
                c_int,
                *const ERL_NIF_TERM,
            ) -> ERL_NIF_TERM,
            argc: usize,
        ) -> Option<Eterm> {
            if !self.schedule_ok {
                return None;
            }
            self.scheduled.push((fun_name.to_string(), flags, argc));
            Some(make_atom(self.atoms.intern("scheduled")))
        }
    }

    /// An internal pid term for process number `num`.
    fn pid(num: u64) -> Eterm {
        make_internal_pid(prelude::make_pid_data(0, num))
    }

    /// An internal port term for port number `num`.
    fn port_term(num: u64) -> Eterm {
        make_internal_port(prelude::make_port_data(num))
    }

    /// An all-zero monitor holder.
    fn zero_monitor() -> ErlNifMonitor {
        ErlNifMonitor {
            data: [0u8; MONITOR_BYTES],
        }
    }

    /// The NIF entry point `enif_schedule_nif` is handed (never called here).
    ///
    /// # Safety
    ///
    /// Mirrors the C NIF entry-point ABI; the body only returns `NIL`.
    unsafe extern "C" fn scheduled_nif(
        _env: *mut crate::nif::types::ErlNifEnv,
        _argc: c_int,
        _argv: *const ERL_NIF_TERM,
    ) -> ERL_NIF_TERM {
        bare_badarg()
    }

    #[test]
    fn send_reports_one_when_sent_and_zero_otherwise() {
        let mut host = ProcHost::new(false);
        let (live, dead) = (pid(1), pid(2));
        host.self_pid = Some(live);
        host.live_pids = vec![live];
        let mut env = NifEnv::new(&mut host);
        let (to_live, to_dead) = (ErlNifPid { pid: n(live) }, ErlNifPid { pid: n(dead) });
        let msg = n(make_small(7));
        // SAFETY: `env` is live and both ids are initialised locals.
        let sent = unsafe { enif_send(env.as_ptr(), &to_live, core::ptr::null_mut(), msg) };
        // SAFETY: as above.
        let unsent = unsafe { enif_send(env.as_ptr(), &to_dead, core::ptr::null_mut(), msg) };
        // SAFETY: a null `to_pid`/env is explicitly allowed by the contract.
        let null_pid =
            unsafe { enif_send(env.as_ptr(), core::ptr::null(), core::ptr::null_mut(), msg) };
        // SAFETY: as above.
        let no_env =
            unsafe { enif_send(core::ptr::null_mut(), &to_live, core::ptr::null_mut(), msg) };
        assert_eq!((sent, unsent, null_pid, no_env), (1, 0, 0, 0));
        assert_eq!(host.sent, vec![(live, t(msg))], "only the live send reached the host");
    }

    #[test]
    fn port_command_reports_one_for_a_live_port_and_zero_otherwise() {
        let mut host = ProcHost::new(false);
        let (live, dead) = (port_term(1), port_term(2));
        host.live_ports = vec![live];
        let mut env = NifEnv::new(&mut host);
        let to_live = ErlNifPort { port_id: n(live) };
        let to_dead = ErlNifPort { port_id: n(dead) };
        let data = make_small(3);
        // SAFETY: `env` is live and both ids are initialised locals.
        let ok = unsafe { enif_port_command(env.as_ptr(), &to_live, core::ptr::null_mut(), n(data)) };
        // SAFETY: as above.
        let no = unsafe { enif_port_command(env.as_ptr(), &to_dead, core::ptr::null_mut(), n(data)) };
        // SAFETY: a null `to_port`/env is explicitly allowed by the contract.
        let null_port = unsafe {
            enif_port_command(env.as_ptr(), core::ptr::null(), core::ptr::null_mut(), n(data))
        };
        // SAFETY: as above.
        let no_env = unsafe {
            enif_port_command(core::ptr::null_mut(), &to_live, core::ptr::null_mut(), n(data))
        };
        assert_eq!((ok, no, null_port, no_env), (1, 0, 0, 0));
        assert_eq!(host.port_cmds, vec![(live, data)]);
    }

    #[test]
    fn whereis_answers_the_hit_and_the_miss() {
        let mut host = ProcHost::new(false);
        let (live, a_port) = (pid(4), port_term(9));
        let reg = host.intern_atom("registered").expect("interns");
        let port_name = host.intern_atom("registered_port").expect("interns");
        let missing = host.intern_atom("nothing").expect("interns");
        host.named = vec![
            ("registered".to_string(), live),
            ("registered_port".to_string(), a_port),
        ];
        let mut env = NifEnv::new(&mut host);
        let mut out = ErlNifPid { pid: n(pid(0)) };
        // SAFETY: `env` is live and `out` is writable.
        let hit = unsafe { enif_whereis_pid(env.as_ptr(), n(reg), &mut out) };
        assert_eq!((hit, t(out.pid)), (1, live));
        // SAFETY: as above; the remaining calls only read the same locals.
        unsafe {
            assert_eq!(enif_whereis_pid(env.as_ptr(), n(missing), &mut out), 0, "a miss");
            assert_eq!(
                enif_whereis_pid(env.as_ptr(), n(make_small(1)), &mut out),
                0,
                "`is_not_atom(name)` returns 0 before any lookup"
            );
            assert_eq!(
                enif_whereis_pid(env.as_ptr(), n(port_name), &mut out),
                0,
                "a port name is not a pid (`enif_get_local_pid`'s type check)"
            );
            assert_eq!(
                enif_whereis_pid(env.as_ptr(), n(reg), core::ptr::null_mut()),
                0,
                "a null out-parameter is reported, not written through"
            );
        }
        let mut port_out = ErlNifPort {
            port_id: n(port_term(0)),
        };
        // SAFETY: `env` is live and `port_out` is writable.
        let port_hit = unsafe { enif_whereis_port(env.as_ptr(), n(port_name), &mut port_out) };
        assert_eq!((port_hit, t(port_out.port_id)), (1, a_port));
        // SAFETY: as above.
        let wrong_kind = unsafe { enif_whereis_port(env.as_ptr(), n(reg), &mut port_out) };
        assert_eq!(wrong_kind, 0, "a pid name is not a port");
    }

    #[test]
    fn liveness_predicates_report_the_hosts_answer() {
        let mut host = ProcHost::new(false);
        let (live, dead) = (pid(1), pid(2));
        let (a_port, no_port) = (port_term(5), port_term(6));
        host.self_pid = Some(live);
        host.live_pids = vec![live];
        host.live_ports = vec![a_port];
        let mut env = NifEnv::new(&mut host);
        let mut live_pid = ErlNifPid { pid: n(live) };
        let mut dead_pid = ErlNifPid { pid: n(dead) };
        let mut live_port = ErlNifPort {
            port_id: n(a_port),
        };
        let mut dead_port = ErlNifPort {
            port_id: n(no_port),
        };
        // SAFETY: `env` is live and every out-pointer is an initialised local.
        unsafe {
            assert_eq!(enif_is_current_process_alive(env.as_ptr()), 1);
            assert_eq!(enif_is_process_alive(env.as_ptr(), &mut live_pid), 1);
            assert_eq!(enif_is_process_alive(env.as_ptr(), &mut dead_pid), 0);
            assert_eq!(enif_is_port_alive(env.as_ptr(), &mut live_port), 1);
            assert_eq!(enif_is_port_alive(env.as_ptr(), &mut dead_port), 0);
            // Nulls and a null env answer the C's "not alive" 0, never a crash.
            assert_eq!(enif_is_process_alive(env.as_ptr(), core::ptr::null_mut()), 0);
            assert_eq!(enif_is_port_alive(env.as_ptr(), core::ptr::null_mut()), 0);
            assert_eq!(enif_is_process_alive(core::ptr::null_mut(), &mut live_pid), 0);
            assert_eq!(enif_is_current_process_alive(core::ptr::null_mut()), 0);
        }
    }

    #[test]
    fn monitors_round_trip_and_follow_the_c_codes() {
        let mut host = ProcHost::new(false);
        let (live, dead) = (pid(1), pid(2));
        host.self_pid = Some(live);
        host.live_pids = vec![live];
        let mut env = NifEnv::new(&mut host);
        let live_pid = ErlNifPid { pid: n(live) };
        let dead_pid = ErlNifPid { pid: n(dead) };
        let mut mon = zero_monitor();
        let mut undefined = ErlNifPid { pid: UNDEFINED_PID };
        // SAFETY: `env` is live; the locals are initialised and writable.
        let (established, dead_target, undef) = unsafe {
            let established =
                enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &live_pid, &mut mon);
            let dead_target =
                enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &dead_pid, &mut mon);
            let undef =
                enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &undefined, &mut mon);
            (established, dead_target, undef)
        };
        assert_eq!(established, MONITOR_ESTABLISHED);
        // The holder now carries the first monitor's payload; the C stores only on success
        // (`if (monitor) erts_ref_to_driver_monitor(...)`, `erl_nif.c:4007`) and only after the
        // target check (`:3975`), so the two failed attempts must have left it untouched.
        let first_block = unsafe { monitor_block(&mon) }.expect("a non-null monitor");
        assert_eq!(dead_target, MONITOR_TARGET_DEAD, "the C's `>0` code for a dead pid");
        assert_eq!(undef, MONITOR_TARGET_DEAD, "the C's early return for an undefined pid");
        assert_eq!(
            unsafe { monitor_block(&mon) }.expect("still non-null"),
            first_block,
            "a failed `enif_monitor_process` does not touch the caller's holder"
        );

        // Establish a second, independent monitor and check the payload round-trips both ways.
        undefined.pid = n(live);
        // SAFETY: as above.
        let code =
            unsafe { enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &undefined, &mut mon) };
        assert_eq!(code, MONITOR_ESTABLISHED);
        let expected = *host.monitors.last().expect("the monitor was recorded");
        let mut rebuilt = zero_monitor();
        // SAFETY: `mon` is an initialised local.
        let stored = unsafe { monitor_block(&mon) }.expect("a non-null monitor");
        assert_ne!(stored, first_block, "a second establishment writes a new payload");
        store_monitor(&mut rebuilt, &stored);
        // SAFETY: `env` is live and both holders are initialised locals.
        let (same, term) = unsafe {
            (
                enif_compare_monitors(&mon, &rebuilt),
                enif_make_monitor_term(env.as_ptr(), &mon),
            )
        };
        assert_eq!(same, 0, "the stored 24-byte block is what the comparison sees");
        let payload = ref_payload(t(term));
        assert!(
            payload.is_some(),
            "the C rebuilds an ordinary internal reference (`io.c:7002-7010`)"
        );
        assert_eq!(
            payload,
            Some(expected),
            "the rebuilt term carries the payload the host handed out"
        );
        let (removed, again, null_monitor, no_env) = {
            // SAFETY: `env` is live; the monitor is an initialised local.
            unsafe {
                (
                    enif_demonitor_process(env.as_ptr(), core::ptr::null_mut(), &mon),
                    enif_demonitor_process(env.as_ptr(), core::ptr::null_mut(), &mon),
                    enif_demonitor_process(env.as_ptr(), core::ptr::null_mut(), core::ptr::null()),
                    enif_demonitor_process(core::ptr::null_mut(), core::ptr::null_mut(), &mon),
                )
            }
        };
        assert_eq!(removed, DEMONITOR_REMOVED, "0 = identified and removed");
        assert_eq!(again, DEMONITOR_NOT_FOUND, "1 = could not be identified");
        assert_eq!(null_monitor, DEMONITOR_NOT_FOUND);
        assert_eq!(no_env, DEMONITOR_NOT_FOUND);
    }

    #[test]
    fn an_unknown_monitor_is_reported_not_found() {
        let mut host = ProcHost::new(false);
        let mut env = NifEnv::new(&mut host);
        let mut bogus = zero_monitor();
        bogus.data[0] = 0x7f; // a payload no `monitor_process` call ever handed out
        // SAFETY: `env` is live and `bogus` is an initialised local.
        let code = unsafe { enif_demonitor_process(env.as_ptr(), core::ptr::null_mut(), &bogus) };
        assert_eq!(code, DEMONITOR_NOT_FOUND);
    }

    #[test]
    fn compare_monitors_is_a_memcmp_style_ordering() {
        let a = zero_monitor();
        let mut b = zero_monitor();
        // SAFETY: both holders are initialised locals.
        unsafe {
            assert_eq!(enif_compare_monitors(&a, &b), 0);
            b.data[0] = 1;
            assert_eq!(enif_compare_monitors(&a, &b), -1);
            assert_eq!(enif_compare_monitors(&b, &a), 1);
            // Bytes past the 3-word block are outside the C's comparison (`erl_nif.c:4057`).
            b.data[0] = 0;
            b.data[MONITOR_BYTES - 1] = 9;
            assert_eq!(enif_compare_monitors(&a, &b), 0);
            // Nulls: the C faults; the documented substitute is a total order.
            assert_eq!(enif_compare_monitors(core::ptr::null(), core::ptr::null()), 0);
            assert_eq!(enif_compare_monitors(core::ptr::null(), &a), -1);
            assert_eq!(enif_compare_monitors(&a, core::ptr::null()), 1);
        }
    }

    #[test]
    fn schedule_raises_badarg_without_host_support() {
        let mut host = ProcHost::new(false);
        let mut env = NifEnv::new(&mut host);
        let name = c"reschedule";
        // SAFETY: `env` is live, `name` is NUL-terminated and `scheduled_nif` is a valid entry
        // point with no arguments.
        let term = unsafe {
            enif_schedule_nif(
                env.as_ptr(),
                name.as_ptr(),
                0,
                Some(scheduled_nif),
                0,
                core::ptr::null(),
            )
        };
        let badarg = env.host().intern_atom("badarg").expect("interns");
        assert_eq!(t(term), badarg, "no host support ⇒ badarg, never a fabricated success");
        assert_eq!(env.pending_exception(), Some(badarg), "the refusal is raised");
        // SAFETY: `env` is live.
        unsafe { enif_clear_env(env.as_ptr()) };
        assert_eq!(env.pending_exception(), None);
        assert!(host.scheduled.is_empty(), "nothing was scheduled");
    }

    #[test]
    fn schedule_returns_the_hosts_term_and_enforces_the_c_rules() {
        let mut host = ProcHost::new(true);
        let mut env = NifEnv::new(&mut host);
        let name = c"reschedule";
        // SAFETY: `env` is live, `name` is NUL-terminated and `scheduled_nif` is a valid entry
        // point with two arguments.
        let term = unsafe {
            enif_schedule_nif(
                env.as_ptr(),
                name.as_ptr(),
                ERL_DIRTY_JOB_CPU_BOUND,
                Some(scheduled_nif),
                2,
                core::ptr::null(),
            )
        };
        let scheduled = env.host().intern_atom("scheduled").expect("interns");
        assert_eq!(t(term), scheduled);
        assert_eq!(
            host.scheduled,
            vec![("reschedule".to_string(), ERL_DIRTY_JOB_CPU_BOUND, 2)]
        );

        // The rules the C enforces before any host is consulted (`erl_nif.c:3569,3597,3571`).
        let badarg = env.host().intern_atom("badarg").expect("interns");
        // SAFETY: `env` is live and `name`/`scheduled_nif` are valid for every call.
        unsafe {
            let probes = [
                (name.as_ptr(), 0, MAX_ARG + 1),
                (name.as_ptr(), 4, 0),
                (name.as_ptr(), 0, -1),
                (core::ptr::null(), 0, 0),
            ];
            for (fun_name, flags, argc) in probes {
                let term = enif_schedule_nif(
                    env.as_ptr(),
                    fun_name,
                    flags,
                    Some(scheduled_nif),
                    argc,
                    core::ptr::null(),
                );
                assert_eq!(t(term), badarg, "flags={flags} argc={argc} must be refused");
            }
            let null_fptr = enif_schedule_nif(
                env.as_ptr(),
                name.as_ptr(),
                0,
                None,
                0,
                core::ptr::null(),
            );
            assert_eq!(t(null_fptr), badarg);
        }
        assert_eq!(
            host.scheduled.len(),
            1,
            "only the one legal call reached the host"
        );
    }
}
