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

//! The driver host seam — the runtime services the `erl_drv_*` layer needs.
//!
//! `beam_abi` freezes the *shape* of the driver C ABI (the `ErlDrv*` types and the
//! `driver_*`/`erl_drv_*` table in [`super::api`]). The driver functions cannot do their job alone:
//! `erl_drv_output_term`/`erl_drv_send_term` must turn a **term-data array** into real terms
//! (allocating on a heap, interning atoms), `driver_select` must register an event with the poller,
//! and `driver_set_timer` must talk to the timer wheel. Those are **runtime** services — `beam_abi`
//! must **not** pull in the scheduler, `beam_io` or a specific module's atom table.
//!
//! [`DriverHost`] is that seam. [`DriverEnv`] is the concrete per-port environment behind the
//! C-facing opaque `*mut ErlDrvPort_` (`struct _erl_drv_port`, opaque in [`super::types`]).
//!
//! **The `ErlDrvPort` a runtime passes to a driver callback IS the pointer returned by
//! [`DriverEnv::as_port`]**, recovered with [`DriverEnv::from_port`] — the exact analogue of
//! [`NifEnv::as_ptr`](crate::nif::host::NifEnv::as_ptr) /
//! [`NifEnv::from_opaque`](crate::nif::host::NifEnv::from_opaque). `unsafe` is confined to
//! [`DriverEnv::host`], [`DriverEnv::from_port`] and [`decode_term_spec`]'s operand readers, each
//! documented with a `# Safety` section / `// SAFETY:` rationale (workspace `CONTEXT.md` §3).
//!
//! Two frozen slots carry **no** port operand — `add_driver_entry`/`remove_driver_entry` take only
//! an `ErlDrvEntry *` (`io.c:7640`, `io.c:7698`) — so there is no [`DriverEnv`] to recover a host
//! from. [`with_current_host`] (re-exported here from [`crate::driver::current_host`]) is the scoped
//! publication the runtime wraps those entry points in, so they can reach a [`DriverHost`].

use libc::{c_char, c_int, c_long, c_uint, c_ulong, c_void};

use beam_alloc::Heap;
use beam_term::prelude::{self, Eterm, NIL};

use crate::common::SysIOVec;
use crate::driver::types::{
    ErlDrvBinary, ErlDrvData, ErlDrvEntry, ErlDrvEvent, ErlDrvMonitor, ErlDrvNowData, ErlDrvPDL,
    ErlDrvPort, ErlDrvPort_, ErlDrvSInt, ErlDrvTermData, ERTS_INVALID_ERL_DRV_PORT, ERL_DRV_ATOM,
    ERL_DRV_BINARY, ERL_DRV_BUF2BINARY, ERL_DRV_EXT2TERM, ERL_DRV_FLOAT, ERL_DRV_INT,
    ERL_DRV_INT64, ERL_DRV_LIST, ERL_DRV_MAP, ERL_DRV_NIL, ERL_DRV_PID, ERL_DRV_PORT,
    ERL_DRV_STRING, ERL_DRV_STRING_CONS, ERL_DRV_TUPLE, ERL_DRV_UINT, ERL_DRV_UINT64,
};
// Re-exported so the runtime reaches the publication through the same module as the seam it belongs
// to (`beam_abi::driver::host::with_current_host`); `current_host` documents the mechanism.
pub use crate::driver::current_host::with_current_host;

/// The reason a driver asks for its port to be closed — the C's whole `driver_failure*` family
/// (`erts/emulator/beam/io.c`) collapsed into one seam hook ([`DriverHost::failure`]).
///
/// The C funnels all four `driver_failure*` spellings through one function,
/// `driver_failure_term(ix, term, eof)` (`io.c:7196-7229`), which closes the port with the reason
/// `eof ? am_normal : term`. The variants therefore differ only in how the **reason term** is
/// expressed and in whether the `eof` route is taken, so they model as variants rather than four
/// hooks:
///
/// | variant | C entry point | reason the port owner observes |
/// |---|---|---|
/// | [`Eof`](PortFailure::Eof) | `driver_failure_eof` (`io.c:7278`) and `driver_failure(P, 0)` (`io.c:7258`) | `normal` |
/// | [`Atom`](PortFailure::Atom) | `driver_failure_atom` (`io.c:7263`) | the **bare atom** `string` |
/// | [`Posix`](PortFailure::Posix) | `driver_failure_posix` (`io.c:7273`) | the **bare atom** `erl_errno_id(error)` — the lowercase POSIX name |
/// | [`Error`](PortFailure::Error) | `driver_failure(P, code)` with `code != 0` (`io.c:7258`) | the **bare integer** `code` |
///
/// None of the reasons is wrapped: a port's exit reason is never `{error, …}` (that shape belongs to
/// the *BIF* raises a driver can trigger, not to `driver_failure*`).
///
/// [`Eof`](PortFailure::Eof) is the **eof route**, not merely "reason `normal`": besides discarding
/// the reason term, it flushes the port's buffered `[line]` data (a trailing partial line arrives as
/// `{Port,{data,{noeol,…}}}` *before* the exit — `flush_linebuf_messages`, `io.c:3475-3499`) and, on
/// a port opened with the `eof` option, takes the **soft-eof** branch: the owner gets `{Port,eof}` and
/// the port **stays alive** instead of being closed (`ERTS_PORT_SFLG_SOFT_EOF`,
/// `erl_bif_port.c:996`). Reproducing those consequences is the host's job — see
/// [`DriverHost::failure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortFailure<'a> {
    /// The **eof route** — `driver_failure_eof(port)` (`io.c:7278`, i.e.
    /// `driver_failure_term(ix, NIL, 1)`) *and* `driver_failure(port, 0)` (`io.c:7258`, i.e.
    /// `driver_failure_term(ix, make_small(0), 1)`).
    ///
    /// Both are the same request: the C's `eof` flag discards the reason term, so the port closes
    /// with the atom `normal` and the eof route's extra consequences apply (buffered-line flush,
    /// `[eof]` soft-eof — see the enum docs). `driver_failure(P, 0)` therefore never arrives as
    /// `Error(0)`.
    Eof,
    /// `driver_failure_atom(port, string)` — close with `string` as the **bare** reason atom
    /// (`io.c:7263`).
    ///
    /// The C operand is a NUL-terminated `char *`; this seam carries the name as a `&str` and the
    /// host interns it, exactly as [`DriverHost::intern_atom`] does for the `ERL_DRV_ATOM`
    /// term-data operand. (The C interns the raw bytes as Latin-1 while this seam requires UTF-8 —
    /// the one documented divergence, see `lifecycle.rs`.)
    Atom(&'a str),
    /// `driver_failure_posix(port, error)` — close with the **bare atom** the errno's lowercase
    /// POSIX name (`erl_errno_id(error)`, `io.c:7273`; `eacces` for `EACCES`), never `{error, …}`.
    ///
    /// `0` names `errno_0` and an errno the runtime does not know names `errno_<n>` (the C's
    /// `erl_errno_str.c:47` convention). The errno→name mapping itself is the host's.
    Posix(c_int),
    /// `driver_failure(port, code)` with `code != 0` — close with the **bare integer** `code`
    /// (`io.c:7258`), never `{error, code}`.
    ///
    /// A `0` code is the eof route instead and arrives as [`Eof`](PortFailure::Eof).
    Error(c_int),
}

/// The three-way result of `driver_monitor_process` (workspace contract **A.4**) — the driver C
/// ABI's *process monitor* result, mirroring the C's `0` / `> 0` / `< 0` reason codes
/// (`erts/emulator/beam/io.c:7012-7038`, `do_driver_monitor_process`).
///
/// The C entry point has three outcomes, and the Rust seam must be able to express all three, so
/// [`DriverHost::monitor_process_result`] returns this enum:
///
/// * [`MonitorResult::Ok`] — the C's `0` (`ERL_DRV_MONITOR_OK`, `io.c:7037`): the monitor was armed
///   and the driver's holder was filled with the opaque [`ErlDrvMonitor`].
/// * [`MonitorResult::NotAlive`] — the C's `> 0` (`ERL_DRV_MONITOR_NOT_ALIVE`, value `1`,
///   `io.c:7031`): the advertised process is no longer alive. The C reaches this when its
///   `erts_proc_sig_send_monitor` fails; a host that cannot resolve the operand to a live process
///   reports it here.
/// * [`MonitorResult::Error`] — the C's `< 0` (`-1`, `ERL_DRV_MONITOR_ERROR`, `io.c:7020-7021`,
///   `:7055`): an invalid port, a driver with no `process_exit` callback, or any other failure that
///   is *not* "the process is dead".
///
/// This is the **driver** monitor result. It is deliberately distinct from the **NIF** monitor
/// family ([`NifHost::monitor_process`](crate::nif::host::NifHost::monitor_process),
/// `enif_monitor_process`), which has its own return-code convention and its own host seam.
//
// Derives only `Debug, Clone, Copy`: `ErlDrvMonitor` carries no `PartialEq` (its payload is opaque
// bytes the crate never interprets), so the enum cannot derive one either.
#[derive(Debug, Clone, Copy)]
pub enum MonitorResult {
    /// The C's `0` (`ERL_DRV_MONITOR_OK`): the monitor was armed; the holder is filled.
    Ok(ErlDrvMonitor),
    /// The C's `> 0` (`ERL_DRV_MONITOR_NOT_ALIVE`, value `1`): the process is not alive.
    NotAlive,
    /// The C's `< 0` (`-1`): invalid port, or the host cannot monitor.
    Error,
}

/// Services the runtime provides to the `erl_drv_*` implementations of one port.
pub trait DriverHost {
    /// The term-allocation arena for the duration of the current call. Must not reallocate while the
    /// driver holds a pointer into it (see [`beam_alloc::Heap`]).
    fn heap(&mut self) -> &mut Heap;

    /// The runtime's global atom table: intern `name`, returning the atom term.
    fn intern_atom(&mut self, name: &str) -> Option<Eterm>;

    /// Deliver `data` to the port owner (`driver_output`/`erl_drv_output`). `0` = ok, `-1` = fail.
    fn output(&mut self, port: ErlDrvPort, data: &[u8]) -> c_int;

    /// Deliver a **split** read to the port owner — the C's `driver_output2` (`io.c:6387`) /
    /// `driver_output_binary` (`io.c:6339`) / `driver_outputv` (`io.c:6448`) shape.
    ///
    /// The C does not hand the owner one flat byte string: it builds the reply as
    /// `{Port,{data, Reply}}` where `Reply` is `[header bytes | Body]` — `header` is the `hbuf`/`hlen`
    /// list prefix (`deliver_read_message`, `io.c:3363-3442`; `deliver_vec_message`, `io.c:3502`) and
    /// `body` is the `buf`/`len` tail, which becomes a binary in `[binary]` port mode or an int list
    /// otherwise. The header/body boundary is therefore *observable* to Erlang code: with
    /// `port_command`-style `[binary]` options, `[h… | <<tail>>]` is not the same term as
    /// `[h… | <<head>>, <<tail>>]` or a fully flattened list. This hook is that boundary.
    ///
    /// * `body: None` — the C's `buf == NULL`: **no tail at all**, i.e. the reply is the proper list
    ///   `[h…]`. The C tests `buf != NULL` (`io.c:3372`), never `len != 0`, so a null buffer with a
    ///   zero length is a *header-only* reply.
    /// * `body: Some(&[])` — a **present, empty** tail: the C's `buf != NULL && len == 0`, which
    ///   builds `<<>>` (`io.c:3373-3375`), i.e. the improper list `[h… | <<>>]`.
    ///
    /// The default is byte-identical to the pre-hook behaviour: it concatenates `header` then `body`
    /// (`None` counted as empty) and makes exactly one [`output`](Self::output) call with the joined
    /// bytes — the single call the `driver_output*` implementations made before this hook existed, so
    /// a host that overrides only `output` keeps that (flattened) result. Overriding this hook is
    /// what preserves the split. Every `output2` call makes **exactly one** `output` call; no
    /// allocation happens when either side is empty.
    ///
    /// `0` = ok, `-1` = fail.
    fn output2(&mut self, port: ErlDrvPort, header: &[u8], body: Option<&[u8]>) -> c_int {
        let body = body.unwrap_or(&[]);
        if header.is_empty() {
            return self.output(port, body);
        }
        if body.is_empty() {
            return self.output(port, header);
        }
        let mut joined = Vec::with_capacity(header.len() + body.len());
        joined.extend_from_slice(header);
        joined.extend_from_slice(body);
        self.output(port, &joined)
    }
    /// Deliver a decoded term (`erl_drv_output_term`/`erl_drv_send_term`): to the port owner when
    /// `to` is `None`, else to that specific process. `0` = ok, `-1` = fail.
    fn send_term(&mut self, port: ErlDrvPort, to: Option<Eterm>, term: Eterm) -> c_int;

    /// Decode an **External Term Format** image — the `ERL_DRV_EXT2TERM` operand of a term-data spec
    /// (`io.c:5826-5848`, the C's `erts_binary2term_*` two-pass decoder).
    ///
    /// `bytes` is the `(char *ext, ErlDrvUInt size)` operand pair (never empty — the C rejects
    /// `size <= 0` before asking). The hook allocates the decoded term on [`heap`](Self::heap) and
    /// returns it, or `None` for an invalid image — which the decoder reports exactly as the C's
    /// `ERTS_DDT_FAIL` ("Invalid data"), never as a wrong term.
    ///
    /// The default returns `None`: `beam_abi` has **no** ETF decoder (the codec is
    /// `beam_codec`'s, a crate this one must not depend on — see the crate `CONTEXT.md`), so the
    /// runtime that owns this host (`beam_rt`/`beam_io`) implements the hook over
    /// `beam_codec`'s decoder. Until then `ERL_DRV_EXT2TERM` answers `None`, exactly as the C does
    /// for an undecodable image.
    fn binary_to_term(&mut self, bytes: &[u8]) -> Option<Eterm> {
        let _ = bytes;
        None
    }
    /// Register/cancel interest in `event` (`driver_select`). `mode` is `ERL_DRV_READ`/
    /// `ERL_DRV_WRITE`/`ERL_DRV_USE`; `on` non-zero selects, zero deselects. `0` = ok, `-1` = fail
    /// (default: unsupported).
    fn select(&mut self, port: ErlDrvPort, event: ErlDrvEvent, mode: c_int, on: c_int) -> c_int {
        let _ = (port, event, mode, on);
        -1
    }

    /// Start a `ms`-millisecond timer for `port` (`driver_set_timer`). `0` = ok, `-1` = fail
    /// (default: unsupported).
    fn set_timer(&mut self, port: ErlDrvPort, ms: c_ulong) -> c_int {
        let _ = (port, ms);
        -1
    }

    /// Cancel `port`'s timer (`driver_cancel_timer`). `0` = ok, `-1` = fail (default: unsupported).
    fn cancel_timer(&mut self, port: ErlDrvPort) -> c_int {
        let _ = port;
        -1
    }

    /// The remaining milliseconds of `port`'s timer (`driver_read_timer`), or `None` when there is
    /// no timer / the host does not support them (default: unsupported).
    fn read_timer(&mut self, port: ErlDrvPort) -> Option<c_ulong> {
        let _ = port;
        None
    }

    /// Schedule an asynchronous `invoke` on `port` (`driver_async`). `key` identifies the job,
    /// `data` is passed to `invoke`, and `free` releases `data` when no `invoke` runs. Returns a
    /// queue position (`=> 0`) or a negative error (default: unsupported, `-1`).
    ///
    /// A raw identifier because `async` is a Rust keyword; the C name is `driver_async`.
    fn r#async(
        &mut self,
        port: ErlDrvPort,
        key: *mut c_uint,
        invoke: Option<unsafe extern "C" fn(*mut c_void)>,
        data: *mut c_void,
        free: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_long {
        let _ = (port, key, invoke, data, free);
        -1
    }

    /// The pid the port is connected to (`driver_connected`), or `None` (default: unsupported).
    fn connected_pid(&mut self, port: ErlDrvPort) -> Option<Eterm> {
        let _ = port;
        None
    }

    /// The pid that opened the port (`driver_caller`), or `None` (default: unsupported).
    fn caller_pid(&mut self, port: ErlDrvPort) -> Option<Eterm> {
        let _ = port;
        None
    }

    /// The port's own **id term** — the C's `driver_mk_port(ix)` answer (`prt->common.id`,
    /// `io.c:7295-7303`, reached through `erts_drvport2port`; `NIL` for an invalid handle) — or
    /// `None` when `port` is not a live handle of this host (default: unsupported).
    ///
    /// This hook is what closes the **`ERL_DRV_PORT` term-data operand**. The C idiom
    /// `{ERL_DRV_PORT, driver_mk_port(port)}` carries the port's *id term*, while this crate's
    /// [`driver_mk_port`](crate::driver::drv::driver_mk_port) returns the **handle** encoding its
    /// docs describe (a port handle is the [`DriverEnv`] pointer, [`DriverEnv::as_port`]), because
    /// the term-data layer here has no port table. [`decode_term_spec`] therefore resolves a
    /// non-port operand through this hook, so a driver written the C way delivers the port term.
    ///
    /// The returned term is what the decoded message carries, so it should be an internal port term
    /// ([`prelude::is_internal_port`](beam_term::prelude::is_internal_port)). The handle is
    /// **opaque**: an implementor must not dereference it to validate it — compare it against the
    /// handles it published. The default answers `None` ("no such handle"), which refuses the spec
    /// exactly as a runtime with no port terms always has.
    fn port_term(&mut self, port: ErlDrvPort) -> Option<Eterm> {
        let _ = port;
        None
    }
    // ---------------------------------------------------------------------------------------
    // Port queue — `driver_enq`/`driver_pushq`/`driver_deq`/`driver_sizeq`/`driver_peekq[v]`
    //
    // The C functions take an `ErlDrvPort`; here the host **is** the port's environment, so these
    // new hooks take no port operand (the same "own port" convention the `lifecycle`/`pdl`/`queues`
    // families are specified against). The existing hooks above keep their `ErlDrvPort` operand for
    // signature stability.
    // ---------------------------------------------------------------------------------------

    /// Append `data` to the port's output queue (`driver_enq`/`driver_enq_bin`). `is_binary` says
    /// whether the driver presented those bytes as an `ErlDrvBinary` window rather than a plain
    /// buffer (the seam hands the host a byte slice either way). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn enq(&mut self, data: &[u8], is_binary: bool) -> c_int {
        let _ = (data, is_binary);
        -1
    }

    /// Prepend `data` to the port's output queue (`driver_pushq`/`driver_pushq_bin`). `is_binary` as
    /// for [`enq`](DriverHost::enq). `0` = ok, `-1` = fail (default: unsupported).
    fn pushq(&mut self, data: &[u8], is_binary: bool) -> c_int {
        let _ = (data, is_binary);
        -1
    }

    /// Drop up to `len` bytes from the head of the port's output queue (`driver_deq`). Returns the
    /// number of bytes actually dropped, or `-1` (default: unsupported).
    fn deq(&mut self, len: c_int) -> c_int {
        let _ = len;
        -1
    }

    /// The number of bytes queued on the port (`driver_sizeq`), or `-1` (default: unsupported).
    fn sizeq(&mut self) -> c_int {
        -1
    }

    /// A `SysIOVec` view of the head of the port's output queue (`driver_peekq`/`driver_peekqv`), or
    /// `None` when unsupported or the queue is empty (default: unsupported).
    ///
    /// The C writes the vector length through a `int *` out-parameter and returns a pointer into the
    /// port's own queue; returning an owned `Vec<SysIOVec>` is the safe, ownership-clean spelling of
    /// the same information.
    fn peekq(&mut self) -> Option<Vec<SysIOVec>> {
        None
    }

    // ---------------------------------------------------------------------------------------
    // Port attributes / flags — `set_busy_port`/`set_port_control_flags`/`get_port_flags`
    // ---------------------------------------------------------------------------------------

    /// Set (`true`) or clear (`false`) the port's busy state (`set_busy_port`). `0` = ok, `-1` =
    /// fail (default: unsupported).
    fn set_busy_port(&mut self, busy: bool) -> c_int {
        let _ = busy;
        -1
    }

    /// Set the port's control `mode`/`flags` (`set_port_control_flags`; `mode` is the C macro's
    /// first argument, `flags` the `PORT_CONTROL_FLAG_*` bit set). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn set_control_flags(&mut self, mode: c_int, flags: c_int) -> c_int {
        let _ = (mode, flags);
        -1
    }

    /// The port's flags (`get_port_flags`, the `PORT_FLAG_*` bit set), or `None` (default:
    /// unsupported).
    fn get_port_flags(&mut self) -> Option<c_int> {
        None
    }

    // ---------------------------------------------------------------------------------------
    // Lifecycle & failures — `driver_failure*`/`driver_exit`/`erl_drv_busy_msgq_limits`/
    // `erl_drv_init_ack`/`erl_drv_set_os_pid`/`driver_async_port_key`
    // ---------------------------------------------------------------------------------------

    /// Close the port with `reason` — the single hook behind the C's `driver_failure*` family.
    /// `0` = ok, `-1` = fail (default: unsupported).
    ///
    /// The host owns everything the C's `driver_failure_term(ix, term, eof)` (`io.c:7196-7229`) does
    /// *after* the variant has chosen the reason, so it must reproduce three things:
    ///
    /// 1. **The exit reason the port's owner observes.** For [`PortFailure::Eof`] it is the atom
    ///    `normal`; otherwise the bare term the variant names — the atom of [`PortFailure::Atom`],
    ///    the lowercase POSIX-name atom `erl_errno_id` yields for [`PortFailure::Posix`]
    ///    (`eacces`, or `errno_0`/`errno_<n>` for 0/unknown), and the integer of
    ///    [`PortFailure::Error`]. Measured against a `trap_exit` owner: `{'EXIT',Port,boom}`,
    ///    `{'EXIT',Port,eacces}`, `{'EXIT',Port,7}` — never `{error, …}`.
    /// 2. **The eof route** for [`PortFailure::Eof`]: flush the port's buffered `[line]` data first
    ///    (the owner sees `{Port,{data,{noeol,Partial}}}` before the exit,
    ///    `flush_linebuf_messages`, `io.c:3475-3499`), and on a port opened with the `eof` option
    ///    deliver `{Port,eof}` and **keep the port alive** (`ERTS_PORT_SFLG_SOFT_EOF`,
    ///    `erl_bif_port.c:996`) rather than closing it.
    /// 3. **The closed state**: after a real close the port is gone, so `port_info(P, …)` answers
    ///    `undefined` and `port_close(P)` raises `badarg`, exactly as the C's terminated port does.
    fn failure(&mut self, reason: PortFailure<'_>) -> c_int {
        let _ = reason;
        -1
    }

    /// Immediately terminate the port's connected process with exit `code` (`driver_exit`). `0` =
    /// ok, `-1` = fail (default: unsupported).
    ///
    /// The C (`io.c:7234-7253`) does two things, in this order, and both are the host's obligation:
    ///
    /// 1. **Unlink the connected process *before* closing**, so the close delivers no
    ///    `{'EXIT',Port,Reason}` message at all — a *monitor* on the port still receives
    ///    `{'DOWN',Ref,port,Port,Reason}` (measured).
    /// 2. Close with the atom `normal` for `code == 0`, else with the **bare atom** `erl_errno_id`
    ///    names (`eacces`; never `{error, …}`).
    ///
    /// Afterwards `port_info(P, …)` answers `undefined` and `port_close(P)` raises `badarg`.
    fn port_exit(&mut self, code: c_int) -> c_int {
        let _ = code;
        -1
    }

    /// Set the port's busy-message-queue watermarks (`erl_drv_busy_msgq_limits`): the emulator
    /// blocks the port on `high` queued messages and unblocks it on `low`. `0` = ok, `-1` = fail
    /// (default: unsupported).
    fn busy_msgq_limits(&mut self, low: c_int, high: c_int) -> c_int {
        let _ = (low, high);
        -1
    }

    /// Acknowledge an asynchronous port init (`erl_drv_init_ack`). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn init_ack(&mut self) -> c_int {
        -1
    }

    /// Record the port's OS pid as `pid` for `port_info(_, os_pid)` (`erl_drv_set_os_pid`). `0` =
    /// ok, `-1` = fail (default: unsupported).
    fn set_os_pid(&mut self, pid: c_int) -> c_int {
        let _ = pid;
        -1
    }

    /// Write the port's async key into `key` (`driver_async_port_key`; the C returns it directly, a
    /// `*mut c_uint` out-parameter keeps a failure reportable). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn async_port_key(&mut self, key: *mut c_uint) -> c_int {
        let _ = key;
        -1
    }

    // ---------------------------------------------------------------------------------------
    // Monitors — `driver_monitor_process`/`driver_demonitor_process`/`driver_get_monitored_process`
    // ---------------------------------------------------------------------------------------

    /// Monitor the process `pid` (`driver_monitor_process`), returning the monitor holder the driver
    /// later passes to [`demonitor_process`](DriverHost::demonitor_process) /
    /// [`get_monitored_process`](DriverHost::get_monitored_process). `None` = unsupported (default).
    ///
    /// This is the **two-way compatibility subset**: it can only say "armed" (`Some`) or "could not
    /// arm" (`None` → the C's `-1`). The C's third outcome — "the process is not alive" (`1`, a
    /// *positive* code) — is inexpressible here, so the full three-way hook
    /// [`monitor_process_result`](DriverHost::monitor_process_result) **defaults** to this method and
    /// a host that must report "not alive" overrides that one instead. New hosts should implement
    /// whichever of the two matches the granularity they can honestly answer.
    fn monitor_process(&mut self, pid: Eterm) -> Option<ErlDrvMonitor> {
        let _ = pid;
        None
    }

    /// Monitor the process `pid` (`driver_monitor_process`), returning the full three-way
    /// [`MonitorResult`] (the C's `0` / `> 0` / `< 0`).
    ///
    /// This is the complete hook. Its default preserves the old two-way behaviour by delegating to
    /// [`monitor_process`](DriverHost::monitor_process) — `Some(mon)` ⇒ [`MonitorResult::Ok`], `None`
    /// ⇒ [`MonitorResult::Error`] — so an existing implementor of the two-way hook keeps working,
    /// byte-identically, with no change. A host that can distinguish "the process is not alive"
    /// from "cannot monitor" (the C's `1` vs `-1`) overrides **this** method and returns
    /// [`MonitorResult::NotAlive`] for the former.
    fn monitor_process_result(&mut self, pid: Eterm) -> MonitorResult {
        match self.monitor_process(pid) {
            Some(mon) => MonitorResult::Ok(mon),
            None => MonitorResult::Error,
        }
    }

    /// Cancel the monitor `mon` (`driver_demonitor_process`). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn demonitor_process(&mut self, mon: *const ErlDrvMonitor) -> c_int {
        let _ = mon;
        -1
    }

    /// The pid behind the monitor `mon` (`driver_get_monitored_process`), or `None` (default:
    /// unsupported).
    fn get_monitored_process(&mut self, mon: *const ErlDrvMonitor) -> Option<Eterm> {
        let _ = mon;
        None
    }

    // ---------------------------------------------------------------------------------------
    // Port data lock — `driver_pdl_*`
    //
    // The C models a PDL as an opaque `ErlDrvPDL` handle to a per-port refcounted lock object. The
    // host owns the object (it must outlive the port) and hands the driver the same handle back on
    // every call, so the seam passes `ErlDrvPDL` through unchanged — a `Vec<u8>` blob would not
    // survive the round-trip and could not be identified per port.
    // ---------------------------------------------------------------------------------------

    /// Create (or return the existing) port data lock, as its opaque `ErlDrvPDL` handle
    /// (`driver_pdl_create`); `None` = unsupported (default).
    fn pdl_create(&mut self) -> Option<ErlDrvPDL> {
        None
    }

    /// Take the port data lock `pdl` (`driver_pdl_lock`). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn pdl_lock(&mut self, pdl: ErlDrvPDL) -> c_int {
        let _ = pdl;
        -1
    }

    /// Release the port data lock `pdl` (`driver_pdl_unlock`). `0` = ok, `-1` = fail (default:
    /// unsupported).
    fn pdl_unlock(&mut self, pdl: ErlDrvPDL) -> c_int {
        let _ = pdl;
        -1
    }

    /// The port data lock's reference count (`driver_pdl_get_refc`), or `None` (default:
    /// unsupported).
    fn pdl_get_refc(&mut self, pdl: ErlDrvPDL) -> Option<ErlDrvSInt> {
        let _ = pdl;
        None
    }

    /// Increment the port data lock's reference count, returning the new value
    /// (`driver_pdl_inc_refc`), or `None` (default: unsupported).
    fn pdl_inc_refc(&mut self, pdl: ErlDrvPDL) -> Option<ErlDrvSInt> {
        let _ = pdl;
        None
    }

    /// Decrement the port data lock's reference count, returning the new value
    /// (`driver_pdl_dec_refc`), or `None` (default: unsupported).
    fn pdl_dec_refc(&mut self, pdl: ErlDrvPDL) -> Option<ErlDrvSInt> {
        let _ = pdl;
        None
    }

    // ---------------------------------------------------------------------------------------
    // Deprecated wall clock — `driver_get_now`
    // ---------------------------------------------------------------------------------------

    /// The wall-clock breakdown (`driver_get_now`, deprecated in the C), or `None` (default:
    /// unsupported).
    fn now(&mut self) -> Option<ErlDrvNowData> {
        None
    }

    // ---------------------------------------------------------------------------------------
    // Driver registry & port creation — `add_driver_entry`/`remove_driver_entry`/
    // `driver_lock_driver`/`driver_create_port`
    //
    // These four reach **process-global** runtime structure (the emulator's driver list, a loaded
    // driver's shared-object handle, the port table) rather than one port's term services. Every one
    // is defaulted to the C's own failure value, so a host that does not model the service answers
    // exactly as the C does for a request it cannot honour — never a fabricated success.
    // ---------------------------------------------------------------------------------------

    /// Make the statically linked driver entry `de` known to the emulator — the `void add_driver_entry(ErlDrvEntry *drv)`
    /// wrapper (`io.c:7640`) over `int erts_add_driver_entry(de, handle, driver_list_locked, taint)`
    /// (`io.c:7650`), which records `de` in the global `driver_list` and runs `init_driver` on it.
    ///
    /// `0` = recorded, non-zero = `init_driver` refused the entry, `-1` = unsupported (default).
    /// The C wrapper **discards** this result on purpose ("the init is not allowed to fail when
    /// drivers are added by drivers", `io.c:7642-7645`), so a caller sees nothing through the C ABI;
    /// it is a value here only so the refusal is modelable at all.
    fn add_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int {
        let _ = de;
        -1
    }

    /// Remove the driver entry `de` from the emulator's global `driver_list`
    /// (`int remove_driver_entry(ErlDrvEntry *drv)`, `io.c:7698`).
    ///
    /// `1` = removed, `0` = no such entry (the default: unsupported), `-1` = `de` names a
    /// **dynamically** loaded driver (`dp->handle != NULL`, "not allowed for dynamic drivers",
    /// `io.c:7709-7713`).
    fn remove_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int {
        let _ = de;
        0
    }

    /// Lock the driver behind `port`'s **own** loaded shared object for the rest of the machine's
    /// lifetime (`int driver_lock_driver(ErlDrvPort ix)`, `io.c:7324`).
    ///
    /// `0` = locked; `-1` = unsupported (default) or the port's driver has no `dlopen` handle — a
    /// statically linked driver (`(dh = prt->drv_ptr->handle) == NULL`, `io.c:7337`). The C consults
    /// `prt->drv_ptr->handle` (the handle of the *port's own* driver, `io.c:7337`), **not** a lookup
    /// by name in any global table.
    fn lock_driver(&mut self, port: ErlDrvPort) -> c_int {
        let _ = port;
        -1
    }

    /// Create a new port owned by the emitting driver — `ErlDrvPort driver_create_port(ErlDrvPort
    /// creator_port_ix, ErlDrvTermData pid, char *name, ErlDrvData drv_data)` (`io.c:787`): `name`
    /// selects the driver to instantiate, `pid` becomes the new port's owner/caller and `drv_data`
    /// its driver-private data.
    ///
    /// Returns the new port handle, or the invalid-port sentinel [`ERTS_INVALID_ERL_DRV_PORT`]
    /// (pointer `-1`, `erl_port.h:67`) — which is the C's answer for **every** failure and hence the
    /// default here.
    fn create_port(
        &mut self,
        creator: ErlDrvPort,
        connected: ErlDrvTermData,
        name: *mut c_char,
        drv_data: ErlDrvData,
    ) -> ErlDrvPort {
        let _ = (creator, connected, name, drv_data);
        ERTS_INVALID_ERL_DRV_PORT
    }
}

/// Does `port` name **no** [`DriverEnv`] — is it `NULL` or the C's invalid-port sentinel
/// [`ERTS_INVALID_ERL_DRV_PORT`] (pointer `-1`, `erts/emulator/beam/erl_port.h:60-67`)?
///
/// This is the **single definition** every port-argument recovery site consults
/// ([`DriverEnv::from_port`] and the `drv` layer's `env_for`), so no C entry point can dereference
/// either non-handle.
///
/// The two operands differ in the C and get the same answer here:
///
/// * the **sentinel** is the C's own "invalid port" operand — every port-carrying entry point tests
///   it and returns its failure value (`-1`, or `0` for the port queries) without dereferencing it,
///   so refusing it is ABI fidelity;
/// * a **null** operand is *not* that path: the C does not test for it and the real VM faults
///   (measured). Refusing it is this crate's deliberate safety divergence — a library function must
///   not crash its host process (workspace `CONTEXT.md` §3), and the returned failure value is the
///   one a driver following the documented `< 0` idiom already handles.
pub fn is_invalid_port_handle(port: ErlDrvPort) -> bool {
    port.is_null() || port == ERTS_INVALID_ERL_DRV_PORT
}

/// The concrete per-port driver environment behind the C-facing opaque `*mut ErlDrvPort_`.
///
/// Holds a raw pointer to the host, so the environment itself is lifetime-free and can be recovered
/// from the opaque C pointer by [`DriverEnv::from_port`] (see the module docs).
pub struct DriverEnv {
    /// The host, borrowed for the lifetime of this environment.
    ///
    /// A raw fat pointer rather than `&'a mut dyn DriverHost`: the C side only ever sees
    /// `*mut ErlDrvPort_`, and the recover path ([`DriverEnv::from_port`]) has no way to name the
    /// borrow's lifetime. The caller must keep the host alive for as long as the environment is.
    /// (The `'static` that a `*mut dyn Trait` spells by default is erased by the documented
    /// lifetime-erasing cast in [`DriverEnv::new`].)
    host: *mut dyn DriverHost,
}

impl DriverEnv {
    /// Wrap `host` in a fresh per-port environment.
    pub fn new(host: &mut dyn DriverHost) -> DriverEnv {
        // SAFETY: the only thing reinterpreted here is the erased trait-object lifetime. The
        // C-facing env has no lifetime parameter (`struct _erl_drv_port` is plain C), so the
        // borrow cannot be expressed in the type; `host as *mut dyn DriverHost` alone would be
        // rejected because a raw pointer to a trait object is invariant and defaults to `'static`
        // (raw pointer *casts* may not extend a trait-object lifetime either — rust#141402).
        // `&mut dyn DriverHost` and `*mut dyn DriverHost` are layout-identical (data word + vtable
        // word), and `mem::transmute` between them is a bit-preserving cast, so the caller's borrow
        // is reproduced exactly by `DriverEnv::host`. The caller must keep the host alive for as
        // long as the environment is used (`DriverEnv` is not `Clone`, so there is exactly one
        // handle).
        let host: *mut dyn DriverHost = unsafe { core::mem::transmute(host) };
        DriverEnv { host }
    }

    /// Reborrow the host (the env borrows the host for its lifetime).
    pub fn host(&mut self) -> &mut dyn DriverHost {
        // SAFETY: `host` was produced from a live `&mut dyn DriverHost` in `new` (or a previous
        // `host`/`as_port`/`from_port` round-trip), and `DriverEnv` is not `Clone`, so this is the
        // only outstanding handle to it; the returned reborrow is tied to `&mut self`.
        unsafe { &mut *self.host }
    }

    /// Convenience: `self.host().heap()`.
    pub fn heap(&mut self) -> &mut Heap {
        self.host().heap()
    }

    /// The C-facing opaque port pointer. Casts `self as *mut DriverEnv as *mut ErlDrvPort_`.
    pub fn as_port(&mut self) -> ErlDrvPort {
        self as *mut DriverEnv as *mut ErlDrvPort_
    }

    /// Recover the concrete env from an opaque port pointer.
    ///
    /// # Safety
    ///
    /// `port` must be a pointer previously returned by [`DriverEnv::as_port`] for an env that is
    /// still alive (and whose host is still alive). Returns `None` for any operand that names no
    /// environment — `NULL` and the invalid-port sentinel [`ERTS_INVALID_ERL_DRV_PORT`], the one
    /// predicate [`is_invalid_port_handle`] — so neither is ever dereferenced.
    pub unsafe fn from_port<'a>(port: ErlDrvPort) -> Option<&'a mut DriverEnv> {
        if is_invalid_port_handle(port) {
            return None;
        }
        // SAFETY: the caller guarantees `port` came from `DriverEnv::as_port` on a live env, i.e. it
        // is a `*mut DriverEnv` with the provenance required to reborrow the same allocation; `'a`
        // is the caller's promise that the env outlives the returned reference.
        Some(unsafe { &mut *(port as *mut DriverEnv) })
    }
}

// The term-data array decoder (`erl_drv_output_term`/`erl_drv_send_term`).

/// Turn a driver **term-data array** into an [`Eterm`], building the term on the host heap.
///
/// The array is the C `ErlDrvTermData spec[]`: tag/operand pairs in *reverse Polish* order —
/// operands are pushed first, and a `TUPLE`/`LIST` tag pops its operands (ground truth
/// `erts/emulator/beam/io.c`, `ERTS_DDT_*`). It must leave **exactly one** value on the stack and be
/// fully consumed, as the C requires (`io.c`: `if (depth != 1 || ptr != ptr_end) fail`). Returns
/// `None` for any malformed spec — never a wrong term.
///
/// Handled tags: **every one of the 17** `ERL_DRV_*` tags — `ERL_DRV_NIL`, `ERL_DRV_ATOM`,
/// `ERL_DRV_INT`, `ERL_DRV_UINT`, `ERL_DRV_INT64`, `ERL_DRV_UINT64`, `ERL_DRV_STRING`,
/// `ERL_DRV_STRING_CONS`, `ERL_DRV_BUF2BINARY`, `ERL_DRV_BINARY`, `ERL_DRV_TUPLE`, `ERL_DRV_LIST`,
/// `ERL_DRV_PORT`, `ERL_DRV_PID`, `ERL_DRV_FLOAT`, `ERL_DRV_EXT2TERM`, `ERL_DRV_MAP`. Any other tag
/// is malformed and yields `None`.
///
/// # Documented deviations from the C (never a wrong term)
///
/// * `ERL_DRV_ATOM`'s operand is treated as a NUL-terminated `char*` interned through
///   [`DriverHost::intern_atom`]. In the C the operand is already an atom term from
///   `driver_mk_atom`; this seam has no `driver_mk_atom`, so the spec carries the *name* and the
///   host interns it — the same atom either way.
/// * `ERL_DRV_PID`'s operand is the **pid term itself**, exactly as in the C (`mess = ptr[0]`,
///   `io.c:6089`); the C's `driver_connected`-derived term data fits the same slot, so this is a
///   note rather than a difference.
/// * `ERL_DRV_PORT`'s operand is the **port term itself** when it is one (the C's own check,
///   `is_internal_port`, `io.c:5744-5747`), and is otherwise resolved as a **port handle** through
///   the additive, defaulted [`DriverHost::port_term`] hook — which is what makes the C idiom
///   `{ERL_DRV_PORT, driver_mk_port(port)}` work on a host whose `driver_mk_port` answers with the
///   handle encoding ([`crate::driver::drv::driver_mk_port`]). A host that leaves the hook at its
///   default refuses such an operand (`None`), never delivering a wrong term.
/// * `ERL_DRV_EXT2TERM`'s bytes are handed to the **additive, defaulted** [`DriverHost::binary_to_term`]
///   hook, because the ETF decoder lives outside this crate (see that hook's docs). With the hook
///   left at its default the tag answers `None` — the C's "invalid data" outcome (`ERTS_DDT_FAIL`),
///   never a wrong term. A host that installs the hook gets the C's behaviour.
/// * `ERL_DRV_MAP` builds its map with [`beam_term::map::write_map`], which reproduces the C's
///   flatmap-vs-hashmap choice (the `MAP_SMALL_MAP_LIMIT` = 32 threshold) and **rejects duplicate
///   keys** (`io.c:6151`'s `erts_validate_and_sort_flatmap` / `hashmap_from_unsorted_array`'s
///   `reject_dupkeys`) — a duplicate-keyed spec is `None`, as the C's `ERTS_DDT_FAIL`. The
///   comparison/ordering the builder needs comes from the terms themselves (atoms carry their
///   runtime index), so a local empty [`AtomTable`](beam_term::atom::AtomTable) is sufficient.
/// * Counts (`LIST`/`TUPLE`/`MAP` arity, `STRING`/`BUF2BINARY` length) are read as non-negative
///   32-bit values, mirroring the C's `(int)` casts, so a malformed spec cannot request an
///   astronomically large copy. `ERL_DRV_BINARY`'s `size`/`offset` are read as full words and
///   bounds-checked against the binary's `orig_size`, and `ERL_DRV_EXT2TERM`'s `size` as a full-word
///   `Sint` rejected when `<= 0` — exactly as the C does (`io.c:5758`, `io.c:5834`).
pub fn decode_term_spec(env: &mut DriverEnv, spec: &[ErlDrvTermData]) -> Option<Eterm> {
    // The intermediate value stack (the C's `ESTACK_PUSH`/`ESTACK_POP`).
    let mut stack: Vec<Eterm> = Vec::new();
    let mut ix = 0usize;

    while ix < spec.len() {
        let tag = spec[ix];
        ix += 1;
        match tag {
            ERL_DRV_NIL => stack.push(NIL),

            ERL_DRV_ATOM => {
                let name = *spec.get(ix)?;
                ix += 1;
                // SAFETY: the `ERL_DRV_ATOM` operand is a NUL-terminated C string (see the docs).
                let bytes = unsafe { cstr(name as *const c_char) }?;
                stack.push(env.host().intern_atom(core::str::from_utf8(bytes).ok()?)?);
            }

            ERL_DRV_INT => {
                let value = *spec.get(ix)?;
                ix += 1;
                stack.push(int_from_i64(env, value as i64)?);
            }

            ERL_DRV_UINT => {
                let value = *spec.get(ix)?;
                ix += 1;
                stack.push(int_from_u64(env, value)?);
            }

            ERL_DRV_INT64 => {
                let ptr = *spec.get(ix)?;
                ix += 1;
                // SAFETY: the `ERL_DRV_INT64` operand is a `Sint64*`.
                let value = unsafe { read_scalar(ptr as *const i64) }?;
                stack.push(int_from_i64(env, value)?);
            }

            ERL_DRV_UINT64 => {
                let ptr = *spec.get(ix)?;
                ix += 1;
                // SAFETY: the `ERL_DRV_UINT64` operand is a `Uint64*`.
                let value = unsafe { read_scalar(ptr as *const u64) }?;
                stack.push(int_from_u64(env, value)?);
            }

            ERL_DRV_STRING | ERL_DRV_STRING_CONS => {
                let ptr = *spec.get(ix)?;
                let len = *spec.get(ix + 1)?;
                ix += 2;
                let n = count_operand(len)?;
                if ptr == 0 {
                    return None; // The C rejects a NULL buffer even for a zero length.
                }
                let tail = if tag == ERL_DRV_STRING_CONS {
                    stack.pop()? // The C requires a value to cons onto (`depth < 1` fails).
                } else {
                    NIL
                };
                // SAFETY: `(char*, length)` declares `n` readable bytes at `ptr`.
                let bytes = unsafe { buf(ptr as *const c_char, n) }?;
                stack.push(byte_list(env, bytes, tail)?);
            }

            ERL_DRV_BUF2BINARY => {
                let ptr = *spec.get(ix)?;
                let len = *spec.get(ix + 1)?;
                ix += 2;
                let n = count_operand(len)?;
                // SAFETY: as `ERL_DRV_STRING`; the C allows a NULL only when the length is 0.
                let bytes = unsafe { buf(ptr as *const c_char, n) }?;
                stack.push(binary_term(env, bytes)?);
            }

            ERL_DRV_BINARY => {
                let bin = *spec.get(ix)?;
                let size = *spec.get(ix + 1)?;
                let offset = *spec.get(ix + 2)?;
                ix += 3;
                // SAFETY: the operand is an `ErlDrvBinary*`; `driver_binary_bytes` bounds-checks
                // `(offset, size)` against `orig_size` before reading.
                let bytes =
                    unsafe { driver_binary_bytes(bin as *const ErlDrvBinary, size, offset) }?;
                stack.push(binary_term(env, bytes)?);
            }

            ERL_DRV_TUPLE => {
                let n = count_operand(*spec.get(ix)?)?;
                ix += 1;
                if n == 0 {
                    stack.push(prelude::tuple0()); // The C's shared empty-tuple literal.
                    continue;
                }
                if stack.len() < n {
                    return None;
                }
                let elems = stack.split_off(stack.len() - n);
                stack.push(env.heap().tuple(&elems).ok()?);
            }

            ERL_DRV_LIST => {
                let n = count_operand(*spec.get(ix)?)?;
                ix += 1;
                if n == 0 || stack.len() < n {
                    return None; // The C rejects `ERL_DRV_LIST 0` outright (`ptr[0] <= 0`).
                }
                let elems = stack.split_off(stack.len() - n);
                let mut acc = NIL;
                for elem in elems.into_iter().rev() {
                    acc = env.heap().cons(elem, acc).ok()?;
                }
                stack.push(acc);
            }

            ERL_DRV_PORT => {
                // The C's operand is the port's **id term** (`mess = ptr[0]`, `io.c:5969-5972`),
                // which it checks with `is_internal_port` (`io.c:5744-5747`).
                let operand = *spec.get(ix)?;
                ix += 1;
                let term = Eterm::from_raw(operand as usize);
                if prelude::is_internal_port(term) {
                    stack.push(term);
                } else {
                    // A host that has no id term for `driver_mk_port` hands out the **handle**
                    // encoding instead (see that function), so the operand is resolved back to the
                    // id term through the host. The two encodings are disjoint: a live handle is a
                    // non-null 8-aligned pointer (low bits 0) and can never satisfy
                    // `is_internal_port`'s `x & 0xF == 0x7`.
                    stack.push(env.host().port_term(operand as usize as ErlDrvPort)?);
                }
            }
            ERL_DRV_PID => {
                // The operand is the pid term itself (`mess = ptr[0]`, `io.c:6089`); the C rejects
                // anything that is not an internal pid (`io.c:5816`).
                let term = Eterm::from_raw(*spec.get(ix)? as usize);
                ix += 1;
                if !prelude::is_internal_pid(term) {
                    return None;
                }
                stack.push(term);
            }

            ERL_DRV_EXT2TERM => {
                let ptr = *spec.get(ix)?;
                let size = *spec.get(ix + 1)?;
                ix += 2;
                // The C reads the length as a full-word `Sint` and rejects `size <= 0` (`io.c:5834`).
                let n = usize::try_from(size as i64).ok()?;
                if n == 0 {
                    return None;
                }
                // SAFETY: `(char*, length)` declares `n` readable bytes at `ptr` (a null `ptr` is
                // refused by `buf`).
                let bytes = unsafe { buf(ptr as *const c_char, n) }?;
                // The ETF decoder is outside this crate; a host without the hook answers `None`
                // (the C's "invalid data" failure, `io.c:5842-5843`).
                stack.push(env.host().binary_to_term(bytes)?);
            }

            ERL_DRV_MAP => {
                let n = count_operand(*spec.get(ix)?)?;
                ix += 1;
                // The C pops `2*n` values (`depth -= 2*ptr[0]`, `io.c:5858`).
                let Some(consumed) = n.checked_mul(2) else {
                    return None;
                };
                if stack.len() < consumed {
                    return None;
                }
                // Key/value pairs are pushed in spec order, so the first two of the consumed items
                // are the first pair (the C pops the value first, then the key — `io.c:6147-6150`).
                let items = stack.split_off(stack.len() - consumed);
                let pairs: Vec<(Eterm, Eterm)> =
                    items.chunks_exact(2).map(|pair| (pair[0], pair[1])).collect();
                stack.push(map_term(env, &pairs)?);
            }
            ERL_DRV_FLOAT => {
                // SAFETY: the `ERL_DRV_FLOAT` operand is a `double*`.
                let value = unsafe { read_scalar(*spec.get(ix)? as *const f64) }?;
                ix += 1;
                stack.push(float_term(env, value)?);
            }

            // Any tag outside the 17 the header defines (`erl_driver.h:540-555`).
            _ => return None,
        }
    }
    // Exactly one value, array fully consumed (the C's final depth check).
    if stack.len() != 1 {
        return None;
    }
    stack.pop()
}

// Operand readers and term builders.

/// A count operand (`LIST`/`TUPLE` arity, `STRING`/`BUF2BINARY` length) as a `usize`.
///
/// The C reads these through an `(int)` cast and rejects a negative result, so a word whose low 32
/// bits are negative is malformed rather than an enormous count.
fn count_operand(word: ErlDrvTermData) -> Option<usize> {
    let n = word as i32;
    if n < 0 {
        None
    } else {
        Some(n as usize)
    }
}

/// A borrowed `&[u8]` view over the `len` bytes at `ptr`.
///
/// # Safety
///
/// `ptr` must point at `len` readable bytes (a null `ptr` is accepted only for `len == 0`).
unsafe fn buf<'a>(ptr: *const c_char, len: usize) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return if len == 0 { Some(&[]) } else { None };
    }
    // SAFETY: the caller guarantees `len` readable bytes at `ptr`.
    Some(unsafe { core::slice::from_raw_parts(ptr.cast::<u8>(), len) })
}

/// A borrowed `&[u8]` view over a NUL-terminated C string (`None` when the pointer is null).
///
/// # Safety
///
/// `ptr` must be null or point at a NUL-terminated byte sequence valid for the call.
unsafe fn cstr<'a>(ptr: *const c_char) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a NUL-terminated string; `CStr::from_ptr` only reads up to it.
    Some(unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes())
}

/// Read one `Copy` value through `ptr` (`None` when the pointer is null).
///
/// # Safety
///
/// `ptr` must be null or point at one readable, properly-aligned value of type `T`.
unsafe fn read_scalar<T: Copy>(ptr: *const T) -> Option<T> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees one readable, aligned `T` at `ptr`.
    Some(unsafe { ptr.read() })
}

/// The `size` bytes at byte `offset` inside an `ErlDrvBinary` (`None` when malformed).
///
/// # Safety
///
/// `bin` must be null or point at a live `ErlDrvBinary` whose `orig_bytes` flexible array holds at
/// least `orig_size` bytes.
unsafe fn driver_binary_bytes<'a>(
    bin: *const ErlDrvBinary,
    size: ErlDrvTermData,
    offset: ErlDrvTermData,
) -> Option<&'a [u8]> {
    if bin.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a live `ErlDrvBinary` at `bin`.
    let orig_size = unsafe { (*bin).orig_size };
    if orig_size < 0 {
        return None;
    }
    // The C rejects `size + offset > b->orig_size` (an overflow-free checked add here).
    if offset.checked_add(size)? > orig_size as u64 {
        return None;
    }
    if size == 0 {
        return Some(&[]);
    }
    // SAFETY: the caller guarantees `orig_size` readable bytes at `orig_bytes`; `offset + size` is
    // within `orig_size` (checked above), so `base.add(offset)` heads `size` readable bytes.
    let base = unsafe { core::ptr::addr_of!((*bin).orig_bytes) } as *const u8;
    Some(unsafe { core::slice::from_raw_parts(base.add(offset as usize), size as usize) })
}

/// A list of small ints from `bytes` (unsigned, as the C's `buf_to_intlist`), ending in `tail`.
fn byte_list(env: &mut DriverEnv, bytes: &[u8], tail: Eterm) -> Option<Eterm> {
    let mut acc = tail;
    for &byte in bytes.iter().rev() {
        acc = env.heap().cons(prelude::make_small(byte as i64), acc).ok()?;
    }
    Some(acc)
}

/// An on-heap byte-aligned binary term from `bytes` (`None` on heap exhaustion).
fn binary_term(env: &mut DriverEnv, bytes: &[u8]) -> Option<Eterm> {
    let bits = bytes.len().checked_mul(8)?;
    // SAFETY: `heap_bits_size_words(n_bits)` sizes the `ErlHeapBits` block for `n_bits` bits.
    let hp = unsafe { env.heap().alloc_words(prelude::heap_bits_size_words(bits)) }.ok()?;
    // SAFETY: `hp` heads the reserved block and `bytes` provides the payload.
    unsafe { prelude::write_binary(hp, bytes) }.ok()
}

/// A float term from `value` (`None` for a non-finite value, which the C rejects).
fn float_term(env: &mut DriverEnv, value: f64) -> Option<Eterm> {
    if !value.is_finite() {
        return None;
    }
    // SAFETY: `FLOAT_SIZE_WORDS` (2) sizes the `[HEADER_FLONUM][bits]` block.
    let hp = unsafe { env.heap().alloc_words(prelude::FLOAT_SIZE_WORDS) }.ok()?;
    // SAFETY: `hp` heads the reserved 2-word block, both words of which are written.
    Some(unsafe { prelude::write_float(hp, value) })
}

/// An `ERL_DRV_MAP` term from `pairs` — the C's `ERL_DRV_MAP` arm (`io.c:6116-6156`).
///
/// The C builds a flat map for at most `MAP_SMALL_MAP_LIMIT` (32) pairs and a hash map beyond that,
/// and **rejects duplicate keys** in both cases (`erts_validate_and_sort_flatmap`, `io.c:6151`;
/// `hashmap_from_unsorted_array`'s `reject_dupkeys`, `io.c:6126`). `None` mirrors that failure.
///
/// Ordering/hashing come from the terms themselves — atoms carry their runtime index — so the
/// builder only needs a table to name atoms, and a fresh empty one is enough here (see the
/// [`decode_term_spec`] deviations).
fn map_term(env: &mut DriverEnv, pairs: &[(Eterm, Eterm)]) -> Option<Eterm> {
    let atoms = prelude::AtomTable::new();
    let mut keys: Vec<Eterm> = pairs.iter().map(|(key, _)| *key).collect();
    keys.sort_by(|a, b| prelude::cmp_flatmap_keys(*a, *b, &atoms));
    if keys
        .windows(2)
        .any(|pair| prelude::cmp_flatmap_keys(pair[0], pair[1], &atoms).is_eq())
    {
        return None; // Duplicate keys: the C's `ERTS_DDT_FAIL`.
    }
    // SAFETY: `map_words_capacity(n)` is the builder's documented reservation for `n` pairs.
    let hp = unsafe { env.heap().alloc_words(prelude::map_words_capacity(pairs.len())) }.ok()?;
    // SAFETY: `hp` heads the reserved block; `write_map` writes every word of the map it produces
    // (the reservation is an upper bound, so trailing words may stay unwritten — `beam_term`'s
    // contract, exactly as `nif::enif::map` uses it).
    unsafe { prelude::write_map(hp, pairs, &atoms) }.ok()
}
/// `make_small` when the value fits, else a one-limb bignum of the right sign.
///
/// Equivalent to `nif/enif.rs`'s `small_or_big`, which cannot be shared without touching that
/// module (out of this step's scope); it is a pure function of `beam_term`'s public writers.
fn small_or_big(env: &mut DriverEnv, negative: bool, magnitude: u64) -> Option<Eterm> {
    let fits_small = if negative {
        magnitude <= prelude::MIN_SMALL.unsigned_abs()
    } else {
        magnitude <= prelude::MAX_SMALL as u64
    };
    if fits_small {
        let value = if negative {
            (magnitude as i64).wrapping_neg()
        } else {
            magnitude as i64
        };
        return Some(prelude::make_small(value));
    }
    // SAFETY: `big_size_words(1)` sizes the `[hdr][digit]` block.
    let hp = unsafe { env.heap().alloc_words(prelude::big_size_words(1)) }.ok()?;
    // SAFETY: `hp` heads the reserved `1 + 1`-word block, which `write_big` writes in full.
    Some(unsafe { prelude::write_big(hp, negative, &[magnitude]) })
}

/// An `ERL_DRV_INT` operand (`ErlDrvSInt`, a `long` on LP64) as a term.
fn int_from_i64(env: &mut DriverEnv, value: i64) -> Option<Eterm> {
    if value < 0 {
        small_or_big(env, true, value.unsigned_abs())
    } else {
        small_or_big(env, false, value as u64)
    }
}

/// An `ERL_DRV_UINT`/`ERL_DRV_UINT64` operand (`ErlDrvUInt`) as a term.
fn int_from_u64(env: &mut DriverEnv, value: u64) -> Option<Eterm> {
    small_or_big(env, false, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_term::atom::AtomTable;
    use beam_term::prelude::{
        atom_val, car, cdr, is_atom, is_big, is_list, is_small, is_tuple, make_atom, make_small,
        signed_value, tuple_element, tuple_size,
    };
    use std::ffi::CString;

    use crate::driver::types::{
        term_data, ERL_DRV_ATOM, ERL_DRV_BINARY, ERL_DRV_BUF2BINARY, ERL_DRV_EXT2TERM,
        ERL_DRV_FLOAT, ERL_DRV_INT, ERL_DRV_INT64, ERL_DRV_LIST, ERL_DRV_MAP, ERL_DRV_NIL,
        ERL_DRV_PID, ERL_DRV_PORT, ERL_DRV_STRING, ERL_DRV_STRING_CONS, ERL_DRV_TUPLE,
        ERL_DRV_UINT, ERL_DRV_UINT64,
    };

    /// A minimal [`DriverHost`]: a bump heap, an atom table, and recording output/term buffers.
    struct TestDriverHost {
        heap: Heap,
        atoms: AtomTable,
        output: Vec<u8>,
        sent: Vec<Eterm>,
    }

    impl TestDriverHost {
        fn new() -> TestDriverHost {
            TestDriverHost {
                heap: Heap::new(1024).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
                output: Vec::new(),
                sent: Vec::new(),
            }
        }

        fn atoms(&self) -> &AtomTable {
            &self.atoms
        }
    }

    impl DriverHost for TestDriverHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }

        fn output(&mut self, _port: ErlDrvPort, data: &[u8]) -> c_int {
            self.output.extend_from_slice(data);
            0
        }

        fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, term: Eterm) -> c_int {
            self.sent.push(term);
            0
        }
    }

    /// A host whose `binary_to_term` hook is a **stub ETF decoder**: the single byte `42` decodes to
    /// the small int 42, anything else is "invalid data". It records every window it is handed, so a
    /// test can pin *which* bytes `ERL_DRV_EXT2TERM` forwarded.
    struct EtfHost {
        heap: Heap,
        atoms: AtomTable,
        seen: Vec<Vec<u8>>,
    }

    impl EtfHost {
        fn new() -> EtfHost {
            EtfHost {
                heap: Heap::new(1024).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
                seen: Vec::new(),
            }
        }
    }

    impl DriverHost for EtfHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }

        fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
            -1
        }

        fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
            -1
        }

        fn binary_to_term(&mut self, bytes: &[u8]) -> Option<Eterm> {
            self.seen.push(bytes.to_vec());
            match bytes {
                [42] => Some(make_small(42)),
                _ => None,
            }
        }
    }

    /// A host that resolves **exactly one** `ErlDrvPort` handle to a port id term, and every other
    /// operand to `None` — the [`DriverHost::port_term`] seam (`driver_mk_port`'s C answer).
    ///
    /// It records the handles it is asked about, so a test can pin *which* operand reached the hook.
    struct PortTermHost {
        heap: Heap,
        atoms: AtomTable,
        handle: ErlDrvPort,
        term: Eterm,
        asked: Vec<ErlDrvPort>,
    }

    impl PortTermHost {
        fn new(handle: ErlDrvPort, term: Eterm) -> PortTermHost {
            PortTermHost {
                heap: Heap::new(1024).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
                handle,
                term,
                asked: Vec::new(),
            }
        }

        fn atoms(&self) -> &AtomTable {
            &self.atoms
        }
    }

    impl DriverHost for PortTermHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }

        fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
            -1
        }

        fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
            -1
        }

        /// Identity comparison only: the handle is opaque, never dereferenced.
        fn port_term(&mut self, port: ErlDrvPort) -> Option<Eterm> {
            self.asked.push(port);
            (port == self.handle).then_some(self.term)
        }
    }

    /// The payload bytes of a byte-aligned binary [`Eterm`] (a test-only reader).
    ///
    /// # Safety
    ///
    /// `term` must be a live, byte-aligned binary term.
    unsafe fn binary_bytes_of(term: Eterm) -> Vec<u8> {
        // SAFETY: `term` is a live binary (the caller's contract).
        assert!(unsafe { prelude::is_binary(term) }, "{term:?} is not a binary");
        // SAFETY: `term` is a live binary (asserted above).
        let (ptr, bit_offset, bit_len) = unsafe { prelude::bitstring_parts(term) };
        assert_eq!(bit_offset % 8, 0, "the test reader only handles byte-aligned binaries");
        // SAFETY: `ptr` heads `bit_len` readable bits starting at `bit_offset`, and the block is a
        // whole number of bytes (the `is_binary` invariant).
        unsafe { core::slice::from_raw_parts(ptr.add(bit_offset / 8), bit_len / 8) }.to_vec()
    }
    #[test]
    fn as_port_round_trips_through_from_port() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        assert!(!port.is_null(), "the port pointer is never null");
        // SAFETY: `port` came from `DriverEnv::as_port` on `env`, which is still alive here.
        let recovered = unsafe { DriverEnv::from_port(port) }.expect("non-null port");
        // The recovered env drives the very same host: a term sent through it lands in `host`.
        // (`DriverEnv::new` erases the borrow, so `host` is usable again here.)
        assert_eq!(recovered.host().send_term(port, None, make_small(7)), 0);
        assert_eq!(host.sent, vec![make_small(7)]);
    }

    #[test]
    fn from_port_of_null_is_none() {
        // SAFETY: a null pointer is explicitly allowed by the contract.
        let recovered = unsafe { DriverEnv::from_port(std::ptr::null_mut()) };
        assert!(recovered.is_none());
    }

    #[test]
    fn decode_term_spec_handles_an_atom() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let name = CString::new("hello").expect("no interior NUL");
        let spec = [ERL_DRV_ATOM, term_data(name.as_ptr() as usize)];
        let term = decode_term_spec(&mut env, &spec).expect("a valid atom spec");
        assert!(is_atom(term));
        assert_eq!(host.atoms().name(atom_val(term)).as_deref(), Some("hello"));
    }

    #[test]
    fn decode_term_spec_handles_ints_and_bignums() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);

        let term = decode_term_spec(&mut env, &[ERL_DRV_INT, 42]).expect("a valid int spec");
        assert!(is_small(term));
        assert_eq!(signed_value(term), 42);

        let term = decode_term_spec(&mut env, &[ERL_DRV_INT, (-1i64) as ErlDrvTermData])
            .expect("a negative int is valid");
        assert_eq!(signed_value(term), -1);

        // A magnitude outside the 60-bit small range becomes a bignum.
        let term =
            decode_term_spec(&mut env, &[ERL_DRV_UINT, u64::MAX]).expect("a valid uint spec");
        // SAFETY: `term` is a valid boxed term (a bignum, never a small int here).
        assert!(unsafe { is_big(term) });
    }

    #[test]
    fn decode_term_spec_handles_a_string() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let text = b"abc";
        let spec = [
            ERL_DRV_STRING,
            term_data(text.as_ptr() as usize),
            text.len() as ErlDrvTermData,
        ];
        let term = decode_term_spec(&mut env, &spec).expect("a valid string spec");
        // "abc" is the list [97, 98, 99].
        assert!(is_list(term));
        // SAFETY: `term` is a live three-element list.
        let (a, rest) = unsafe { (car(term), cdr(term)) };
        // SAFETY: `rest` is a live two-element list.
        let (b, rest) = unsafe { (car(rest), cdr(rest)) };
        // SAFETY: `rest` is a live one-element list.
        let (c, tail) = unsafe { (car(rest), cdr(rest)) };
        assert_eq!(
            [signed_value(a), signed_value(b), signed_value(c)],
            [97, 98, 99]
        );
        assert_eq!(tail, NIL);
    }

    #[test]
    fn decode_term_spec_handles_a_tuple_and_a_list() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let name = CString::new("a").expect("no interior NUL");

        // {a, 1}
        let spec = [
            ERL_DRV_ATOM,
            term_data(name.as_ptr() as usize),
            ERL_DRV_INT,
            1,
            ERL_DRV_TUPLE,
            2,
        ];
        let term = decode_term_spec(&mut env, &spec).expect("a valid tuple spec");
        // SAFETY: `term` is a live tuple (asserted by the size check below).
        assert!(unsafe { is_tuple(term) });
        // SAFETY: as above.
        assert_eq!(unsafe { tuple_size(term) }, 2);
        // SAFETY: element 0 of a 2-tuple.
        assert!(is_atom(unsafe { tuple_element(term, 0) }));
        // SAFETY: element 1 of a 2-tuple.
        assert_eq!(signed_value(unsafe { tuple_element(term, 1) }), 1);

        // [1, 2]
        let term = decode_term_spec(&mut env, &[ERL_DRV_INT, 1, ERL_DRV_INT, 2, ERL_DRV_LIST, 2])
            .expect("a valid list spec");
        assert!(is_list(term));
        // SAFETY: `term` is a live two-element list.
        let (head, rest) = unsafe { (car(term), cdr(term)) };
        // SAFETY: `rest` is a live one-element list.
        let (second, tail) = unsafe { (car(rest), cdr(rest)) };
        assert_eq!(signed_value(head), 1);
        assert_eq!(signed_value(second), 2);
        assert_eq!(tail, NIL);
    }

    #[test]
    fn decode_term_spec_rejects_malformed_specs() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let digits = CString::new("digits").expect("no interior NUL");

        // Every entry below is a spec the C's two-pass decoder fails (`ERTS_DDT_FAIL`): an empty
        // spec leaves no value; a tag whose operands are missing; two values left on the stack
        // (final depth != 1); a tuple arity larger than the stack; `ERL_DRV_LIST 0` (the C rejects
        // `ptr[0] <= 0`); a port/pid operand that is not an internal port/pid (`NIL`, a small int);
        // a NULL string buffer; a null float pointer; a negative count; an unknown tag; a `MAP` with
        // fewer than `2n` values or a negative arity; an `EXT2TERM` with a zero/NULL/negative size;
        // and an `EXT2TERM` whose bytes the (default) host hook cannot decode.
        for spec in [
            &[] as &[ErlDrvTermData],
            &[ERL_DRV_INT],
            &[ERL_DRV_INT, 1, ERL_DRV_INT, 2],
            &[ERL_DRV_TUPLE, 1],
            &[ERL_DRV_TUPLE, -1i64 as ErlDrvTermData],
            &[ERL_DRV_LIST, 0],
            &[ERL_DRV_LIST, -1i64 as ErlDrvTermData],
            &[ERL_DRV_PORT, 0],
            &[ERL_DRV_PORT, 1], // low nibble 1 != the port tag 7, so not an internal port
            &[ERL_DRV_PID, 0],
            &[ERL_DRV_PID, 7],
            &[ERL_DRV_STRING, 0, 0],
            &[ERL_DRV_STRING, term_data(digits.as_ptr() as usize), 6, ERL_DRV_INT, 1],
            &[ERL_DRV_STRING_CONS, term_data(digits.as_ptr() as usize), 0],
            &[ERL_DRV_FLOAT, 0],
            &[ERL_DRV_MAP, -1i64 as ErlDrvTermData],
            &[ERL_DRV_MAP, 1],
            &[ERL_DRV_INT, 1, ERL_DRV_MAP, 1],
            &[ERL_DRV_MAP, 2, ERL_DRV_INT, 1, ERL_DRV_INT, 2],
            &[ERL_DRV_EXT2TERM, 0, 0],
            &[ERL_DRV_EXT2TERM, 0, 1],
            &[ERL_DRV_EXT2TERM, term_data(digits.as_ptr() as usize), 0],
            &[ERL_DRV_EXT2TERM, term_data(digits.as_ptr() as usize), 6],
            &[99, 0], // An unknown tag (`erl_driver.h` stops at 17).
        ] {
            assert_eq!(decode_term_spec(&mut env, spec), None, "spec {spec:?}");
        }
    }
    #[test]
    fn decode_term_spec_handles_a_pid_and_a_port_operand() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);

        // The operand is the term itself, exactly as in the C (`mess = ptr[0]`, `io.c:6089`).
        let pid = prelude::make_internal_pid(prelude::make_pid_data(3, 42));
        let term = decode_term_spec(&mut env, &[ERL_DRV_PID, term_data(pid.raw())])
            .expect("an internal pid is a valid operand");
        assert_eq!(term, pid);

        let port = prelude::make_internal_port(prelude::make_port_data(9));
        let term = decode_term_spec(&mut env, &[ERL_DRV_PORT, term_data(port.raw())])
            .expect("an internal port is a valid operand");
        assert_eq!(term, port);

        // `NIL` is the operand both `driver_term_nil` and `driver_connected` produce when there is
        // no port/pid; the C refuses it (`is_not_internal_pid`, `io.c:5816`), and so does this.
        assert_eq!(decode_term_spec(&mut env, &[ERL_DRV_PID, term_data(NIL.raw())]), None);
        assert_eq!(decode_term_spec(&mut env, &[ERL_DRV_PORT, term_data(NIL.raw())]), None);

        // The C's `is_internal_port` is only `x & 0xF == 0x7` (`erl_term.h`), so the raw word `7`
        // *passes* it and the spec is accepted verbatim — the same tag-overlap quirk the C has.
        // (`is_internal_pid`'s tag is `0x3`, which `7 & 0xF` does not match, so `ERL_DRV_PID` is
        // unaffected.) Reproduced exactly rather than "fixed", since the C is the contract.
        assert_eq!(
            decode_term_spec(&mut env, &[ERL_DRV_PORT, 7]).map(Eterm::raw),
            Some(7)
        );

        // A **handle**-encoded operand (what this crate's `driver_mk_port` answers) is not a port
        // term, so a host without the `port_term` hook — this one is at the trait default — refuses
        // it. `7` above passes only because it satisfies `is_internal_port`.
        assert_eq!(decode_term_spec(&mut env, &[ERL_DRV_PORT, term_data(0x1000)]), None);
    }

    #[test]
    fn decode_term_spec_resolves_a_port_handle_through_the_port_term_hook() {
        // The id term this host answers for its one live handle — a synthesized internal port term,
        // exactly the shape `driver_mk_port` returns in the C (`prt->common.id`).
        let handle = 0x1000usize as ErlDrvPort;
        let port = prelude::make_internal_port(prelude::make_port_data(11));
        let mut host = PortTermHost::new(handle, port);
        let mut env = DriverEnv::new(&mut host);

        // The handle encoding (`{ERL_DRV_PORT, driver_mk_port(port)}`, the C's own idiom) resolves
        // to the id term through the hook.
        let term = decode_term_spec(&mut env, &[ERL_DRV_PORT, term_data(handle as usize)])
            .expect("a live handle resolves through `port_term`");
        assert_eq!(term, port);
        assert_eq!(host.asked, vec![handle], "the hook is asked about the operand's handle");

        // An unknown handle, a null word and a plain non-port word are all refused (`None` from the
        // hook), never turned into a wrong term.
        assert_eq!(decode_term_spec(&mut env, &[ERL_DRV_PORT, term_data(0x2000)]), None);
        assert_eq!(decode_term_spec(&mut env, &[ERL_DRV_PORT, 0]), None);
        assert_eq!(decode_term_spec(&mut env, &[ERL_DRV_PORT, 1]), None);
        assert_eq!(
            host.asked,
            vec![handle, 0x2000usize as ErlDrvPort, 0 as ErlDrvPort, 1usize as ErlDrvPort],
            "every non-port operand is offered to the hook"
        );

        // A genuine port term still takes the C's own path: pushed as-is, with no hook query.
        let direct = decode_term_spec(&mut env, &[ERL_DRV_PORT, term_data(port.raw())])
            .expect("an internal port is a valid operand");
        assert_eq!(direct, port);
        assert_eq!(host.asked.len(), 4, "a port term never reaches the hook");

        // And the resolved term participates in compounds like any other value.
        let name = CString::new("tag").expect("no interior NUL");
        let spec = [
            ERL_DRV_ATOM,
            term_data(name.as_ptr() as usize),
            ERL_DRV_PORT,
            term_data(handle as usize),
            ERL_DRV_TUPLE,
            2,
        ];
        let term = decode_term_spec(&mut env, &spec).expect("a tuple of [atom, port handle]");
        // SAFETY: `term` is a live term the decoder just built; the tuple readers run only after
        // `is_tuple` confirms the shape.
        unsafe {
            assert!(is_tuple(term));
            assert_eq!(tuple_size(term), 2);
        }
        // SAFETY: `term` is a live 2-tuple (checked above).
        let (first, second) = unsafe { (tuple_element(term, 0), tuple_element(term, 1)) };
        assert!(is_atom(first));
        assert_eq!(host.atoms().name(atom_val(first)).as_deref(), Some("tag"));
        assert_eq!(second, port);
    }

    #[test]
    fn decode_term_spec_handles_a_zero_length_and_a_full_binary() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);

        // `ERL_DRV_BUF2BINARY` with a NULL buffer and a zero length is the C's allowed empty binary
        // (`if (!bufp && size > 0) ERTS_DDT_FAIL`, `io.c:5773`).
        let term = decode_term_spec(&mut env, &[ERL_DRV_BUF2BINARY, 0, 0])
            .expect("a zero-length buffer builds an empty binary");
        // SAFETY: `term` is a live binary.
        assert!(unsafe { binary_bytes_of(term) }.is_empty());

        // A zero-length *string* over a valid (non-NULL) buffer is the empty list
        // (`buf_to_intlist(&factory.hp, (char*)ptr[0], 0, NIL)`, `io.c:6037`). A NULL buffer is
        // refused by the decoder *even for length 0* (`(char*)ptr[0] == NULL`, `io.c:5778`), which
        // the malformed-spec test pins.
        let empty = b"";
        let term = decode_term_spec(
            &mut env,
            &[ERL_DRV_STRING, term_data(empty.as_ptr() as usize), 0],
        )
        .expect("a zero-length string over a non-NULL buffer is valid");
        assert_eq!(term, NIL, "a zero-length string is []");

        // A full `ERL_DRV_BINARY`: `{ptr, size, offset}` windows the payload of an `ErlDrvBinary`.
        // SAFETY: the contract of `driver_alloc_binary` (a live driver binary with `orig_size == 5`).
        let bin = unsafe { crate::driver::drv::driver_alloc_binary(5) };
        assert!(!bin.is_null(), "the driver-binary allocator must succeed");
        // SAFETY: `orig_bytes` (offset 8, pinned in `crate::driver::types`) heads the 5-byte payload.
        let payload = unsafe { core::ptr::addr_of_mut!((*bin).orig_bytes) } as *mut u8;
        // SAFETY: the payload is 5 writable bytes, as requested above.
        unsafe { core::ptr::copy_nonoverlapping(b"hello".as_ptr(), payload, 5) };

        let term = decode_term_spec(&mut env, &[ERL_DRV_BINARY, term_data(bin as usize), 5, 0])
            .expect("an in-bounds window is valid");
        // SAFETY: `term` is a live binary built from the 5 payload bytes.
        assert_eq!(unsafe { binary_bytes_of(term) }, b"hello");
        // A sub-window (`size` + `offset` still inside `orig_size`) is legal.
        let term = decode_term_spec(&mut env, &[ERL_DRV_BINARY, term_data(bin as usize), 3, 2])
            .expect("an in-bounds sub-window is valid");
        // SAFETY: as above.
        assert_eq!(unsafe { binary_bytes_of(term) }, b"llo");
        // `size + offset > orig_size` is refused (the C's "outside the binary", `io.c:5758`).
        assert_eq!(
            decode_term_spec(&mut env, &[ERL_DRV_BINARY, term_data(bin as usize), 4, 2]),
            None
        );
        // SAFETY: `bin` came from `driver_alloc_binary` and is freed exactly once.
        unsafe { crate::driver::drv::driver_free_binary(bin) };
    }
    #[test]
    fn decode_term_spec_handles_int64_and_uint64_operands() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);

        // `ERL_DRV_INT64`/`ERL_DRV_UINT64` carry a *pointer* to the value (`Sint64*`/`Uint64*`).
        let signed: i64 = -5;
        let term = decode_term_spec(&mut env, &[ERL_DRV_INT64, term_data(&signed as *const i64 as usize)])
            .expect("a valid INT64 spec");
        assert_eq!(signed_value(term), -5);

        let unsigned: u64 = u64::MAX;
        let term = decode_term_spec(
            &mut env,
            &[ERL_DRV_UINT64, term_data(&unsigned as *const u64 as usize)],
        )
        .expect("a valid UINT64 spec");
        // SAFETY: `u64::MAX` does not fit a 60-bit small int, so the builder produced a bignum.
        assert!(unsafe { is_big(term) });
    }

    #[test]
    fn decode_term_spec_handles_an_empty_tuple() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);

        // The C accepts `ERL_DRV_TUPLE 0` (it yields the global empty-tuple literal, `io.c:6068`) —
        // unlike `ERL_DRV_LIST 0`, which it rejects.
        let term = decode_term_spec(&mut env, &[ERL_DRV_TUPLE, 0]).expect("an empty tuple is valid");
        // SAFETY: `term` is the live empty tuple.
        assert!(unsafe { is_tuple(term) });
        // SAFETY: as above.
        assert_eq!(unsafe { tuple_size(term) }, 0);
    }

    #[test]
    fn decode_term_spec_cons_a_string_onto_a_stack_value() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let text = b"ab";

        // `ERL_DRV_STRING_CONS` builds the character list and uses the popped stack value as its
        // *tail* (`buf_to_intlist(&factory.hp, (char*)ptr[0], ptr[1], ESTACK_POP(stack))`,
        // `io.c:6043-6045`), so it **prepends**: the characters come first and the popped value ends
        // the list — `"ab" ++ [1]` = `[97, 98 | 1]`, an improper list, exactly as the C builds it.
        let term = decode_term_spec(
            &mut env,
            &[
                ERL_DRV_INT,
                1,
                ERL_DRV_STRING_CONS,
                term_data(text.as_ptr() as usize),
                2,
            ],
        )
        .expect("a valid STRING_CONS spec");
        assert!(is_list(term));
        // SAFETY: `term` is the live cons cell built above.
        let (first, rest) = unsafe { (car(term), cdr(term)) };
        assert_eq!(signed_value(first), 97);
        // SAFETY: `rest` is a live cons cell.
        let (second, tail) = unsafe { (car(rest), cdr(rest)) };
        assert_eq!(signed_value(second), 98);
        assert_eq!(signed_value(tail), 1, "the popped value becomes the list tail");

        // Consing onto `[]` is the plain character list, `[97, 98]`.
        let term = decode_term_spec(
            &mut env,
            &[
                ERL_DRV_NIL,
                ERL_DRV_STRING_CONS,
                term_data(text.as_ptr() as usize),
                2,
            ],
        )
        .expect("consing onto [] is a proper list");
        // SAFETY: `term` is a live cons cell.
        let (first, rest) = unsafe { (car(term), cdr(term)) };
        // SAFETY: `rest` is a live cons cell.
        let (second, tail) = unsafe { (car(rest), cdr(rest)) };
        assert_eq!([signed_value(first), signed_value(second)], [97, 98]);
        assert_eq!(tail, NIL);
    }

    #[test]
    fn decode_term_spec_nests_a_list_inside_a_tuple() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);

        // `{[1,2], 3}` — the list is built first (`ERL_DRV_LIST`) and the tuple pops it together
        // with the trailing int, exactly the reverse-Polish nesting the C specifies.
        let term = decode_term_spec(
            &mut env,
            &[
                ERL_DRV_INT,
                1,
                ERL_DRV_INT,
                2,
                ERL_DRV_LIST,
                2,
                ERL_DRV_INT,
                3,
                ERL_DRV_TUPLE,
                2,
            ],
        )
        .expect("a valid nested spec");
        // SAFETY: `term` is the live 2-tuple built above.
        assert!(unsafe { is_tuple(term) });
        // SAFETY: as above.
        assert_eq!(unsafe { tuple_size(term) }, 2);
        // SAFETY: element 0 of the live 2-tuple is the list.
        let list = unsafe { tuple_element(term, 0) };
        // SAFETY: `list` is a live two-element list.
        let (first, rest) = unsafe { (car(list), cdr(list)) };
        // SAFETY: `rest` is a live one-element list.
        let (second, tail) = unsafe { (car(rest), cdr(rest)) };
        assert_eq!([signed_value(first), signed_value(second)], [1, 2]);
        assert_eq!(tail, NIL);
        // SAFETY: element 1 of the live 2-tuple.
        assert_eq!(signed_value(unsafe { tuple_element(term, 1) }), 3);
    }

    #[test]
    fn decode_term_spec_builds_a_small_map() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let atoms = AtomTable::new();
        let b = CString::new("b").expect("no interior NUL");
        let a = CString::new("a").expect("no interior NUL");

        // `#{b => 2, a => 1}` — pairs in spec order `(b,2), (a,1)`.
        let spec = [
            ERL_DRV_ATOM,
            term_data(b.as_ptr() as usize),
            ERL_DRV_INT,
            2,
            ERL_DRV_ATOM,
            term_data(a.as_ptr() as usize),
            ERL_DRV_INT,
            1,
            ERL_DRV_MAP,
            2,
        ];
        let term = decode_term_spec(&mut env, &spec).expect("a valid map spec");
        // SAFETY: `term` is a live map written by `beam_term::map::write_map`.
        assert!(unsafe { prelude::is_map(term) });
        // SAFETY: as above — 2 <= MAP_SMALL_MAP_LIMIT, so the builder chose a flat map.
        assert!(unsafe { prelude::is_flatmap(term) });
        assert_eq!(prelude::map_size(term, &atoms).expect("a map"), 2);

        // The keys carry the indices `TestDriverHost` interned them at, so a lookup finds both.
        let a = host.intern_atom("a").expect("the test table is unbounded");
        let b = host.intern_atom("b").expect("the test table is unbounded");
        assert_eq!(prelude::map_get(term, a, &atoms).expect("a map"), Some(make_small(1)));
        assert_eq!(prelude::map_get(term, b, &atoms).expect("a map"), Some(make_small(2)));
    }
    #[test]
    fn decode_term_spec_builds_an_empty_map() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let atoms = AtomTable::new();

        // The C allows `ERL_DRV_MAP 0` (`if ((int) ptr[0] < 0) ERTS_DDT_FAIL`, then a flat map with
        // `size == 0` and the global empty-tuple keys, `io.c:6129-6142`).
        let term = decode_term_spec(&mut env, &[ERL_DRV_MAP, 0]).expect("an empty map is valid");
        // SAFETY: `term` is the live empty flat map.
        assert!(unsafe { prelude::is_flatmap(term) });
        assert_eq!(prelude::map_size(term, &atoms).expect("a map"), 0);
    }

    #[test]
    fn decode_term_spec_switches_a_map_to_a_hash_map_past_the_flat_map_limit() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let atoms = AtomTable::new();

        // `MAP_SMALL_MAP_LIMIT` + 1 pairs: the C takes the hash-map arm (`io.c:6118`), and
        // `beam_term::map::write_map` switches at the same threshold.
        let n = prelude::MAP_SMALL_MAP_LIMIT + 1;
        let mut spec: Vec<ErlDrvTermData> = Vec::new();
        for i in 0..n {
            spec.push(ERL_DRV_UINT);
            spec.push(i as ErlDrvTermData);
            spec.push(ERL_DRV_INT);
            spec.push((i * 2) as ErlDrvTermData);
        }
        spec.push(ERL_DRV_MAP);
        spec.push(n as ErlDrvTermData);

        let term = decode_term_spec(&mut env, &spec).expect("a valid large map spec");
        // SAFETY: `term` is a live map.
        assert!(unsafe { prelude::is_map(term) });
        // SAFETY: as above — more than `MAP_SMALL_MAP_LIMIT` pairs, so it is a HAMT.
        assert!(unsafe { prelude::is_hashmap(term) }, "{n} pairs must produce a hash map");
        assert_eq!(prelude::map_size(term, &atoms).expect("a map"), n);
        // Every key resolves to its value (the builder hashed each key exactly once).
        for i in 0..n {
            assert_eq!(
                prelude::map_get(term, make_small(i as i64), &atoms).expect("a map"),
                Some(make_small((i * 2) as i64)),
                "key {i}"
            );
        }

        // And the limit itself is still a flat map.
        let mut spec: Vec<ErlDrvTermData> = Vec::new();
        for i in 0..prelude::MAP_SMALL_MAP_LIMIT {
            spec.push(ERL_DRV_UINT);
            spec.push(i as ErlDrvTermData);
            spec.push(ERL_DRV_INT);
            spec.push(0);
        }
        spec.push(ERL_DRV_MAP);
        spec.push(prelude::MAP_SMALL_MAP_LIMIT as ErlDrvTermData);
        let term = decode_term_spec(&mut env, &spec).expect("a valid limit-sized map spec");
        // SAFETY: `term` is a live flat map of exactly `MAP_SMALL_MAP_LIMIT` pairs.
        assert!(unsafe { prelude::is_flatmap(term) });
    }

    #[test]
    fn decode_term_spec_rejects_a_map_with_duplicate_keys() {
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);

        // Identical keys: the C fails (`erts_validate_and_sort_flatmap` returns 0 on `c == 0`,
        // `erl_map.c:3202`).
        let dup = [
            ERL_DRV_INT,
            1,
            ERL_DRV_INT,
            10,
            ERL_DRV_INT,
            1,
            ERL_DRV_INT,
            20,
            ERL_DRV_MAP,
            2,
        ];
        assert_eq!(decode_term_spec(&mut env, &dup), None);

        // `1` and `1.0` are *different* map keys (the C compares exact), so that spec is accepted
        // with two entries.
        let one: f64 = 1.0;
        let mixed = [
            ERL_DRV_INT,
            1,
            ERL_DRV_INT,
            10,
            ERL_DRV_FLOAT,
            term_data(&one as *const f64 as usize),
            ERL_DRV_INT,
            20,
            ERL_DRV_MAP,
            2,
        ];
        let term = decode_term_spec(&mut env, &mixed).expect("1 and 1.0 are distinct keys");
        assert_eq!(prelude::map_size(term, &AtomTable::new()).expect("a map"), 2);
    }

    #[test]
    fn decode_term_spec_routes_ext2term_through_the_host_hook() {
        let mut host = EtfHost::new();
        let mut env = DriverEnv::new(&mut host);
        let image = [42u8];

        // The hook decodes `[42]` to the small int 42 and is handed exactly the declared bytes.
        let term = decode_term_spec(
            &mut env,
            &[
                ERL_DRV_EXT2TERM,
                term_data(image.as_ptr() as usize),
                image.len() as ErlDrvTermData,
            ],
        )
        .expect("the stub hook decodes this image");
        assert_eq!(signed_value(term), 42);
        assert_eq!(host.seen, vec![image.to_vec()], "the hook receives the declared window");

        // An un-decodable image is the C's `ERTS_DDT_FAIL` ("Invalid data"), never a wrong term.
        let bad = [7u8, 7, 7];
        assert_eq!(
            decode_term_spec(
                &mut env,
                &[
                    ERL_DRV_EXT2TERM,
                    term_data(bad.as_ptr() as usize),
                    bad.len() as ErlDrvTermData,
                ],
            ),
            None
        );
        assert_eq!(host.seen.last().map(Vec::as_slice), Some(&bad[..]));

        // The decoded value nests like any other term: `{42, 7}`.
        let term = decode_term_spec(
            &mut env,
            &[
                ERL_DRV_EXT2TERM,
                term_data(image.as_ptr() as usize),
                1,
                ERL_DRV_INT,
                7,
                ERL_DRV_TUPLE,
                2,
            ],
        )
        .expect("a nested EXT2TERM term is valid");
        // SAFETY: `term` is the live 2-tuple built above.
        assert_eq!(unsafe { tuple_size(term) }, 2);
        // SAFETY: element 0 of the live 2-tuple.
        assert_eq!(signed_value(unsafe { tuple_element(term, 0) }), 42);
    }

    #[test]
    fn decode_term_spec_rejects_ext2term_without_a_host_hook() {
        // `TestDriverHost` leaves `binary_to_term` at its trait default (`None`), which is the
        // `beam_rt`/`beam_io` situation until either installs the hook.
        let mut host = TestDriverHost::new();
        let mut env = DriverEnv::new(&mut host);
        let image = [42u8];
        assert_eq!(
            decode_term_spec(
                &mut env,
                &[ERL_DRV_EXT2TERM, term_data(image.as_ptr() as usize), 1],
            ),
            None
        );
    }    #[test]
    fn the_new_family_hooks_default_to_unsupported() {
        let mut host = TestDriverHost::new();

        // Port queue: every hook reports failure / no data.
        assert_eq!(host.enq(b"abc", false), -1);
        assert_eq!(host.enq(b"abc", true), -1);
        assert_eq!(host.pushq(b"abc", false), -1);
        assert_eq!(host.pushq(b"abc", true), -1);
        assert_eq!(host.deq(4), -1);
        assert_eq!(host.sizeq(), -1);
        assert!(host.peekq().is_none());

        // Port attributes / flags.
        assert_eq!(host.set_busy_port(true), -1);
        assert_eq!(host.set_busy_port(false), -1);
        assert_eq!(host.set_control_flags(0, 0), -1);
        assert!(host.get_port_flags().is_none());

        // Lifecycle & failures: each `driver_failure*` spelling lands on the same hook.
        for reason in [
            PortFailure::Eof,
            PortFailure::Atom("enoent"),
            PortFailure::Posix(libc::ENOENT),
            PortFailure::Error(libc::EIO),
        ] {
            assert_eq!(host.failure(reason), -1);
        }
        assert_eq!(host.port_exit(1), -1);
        assert_eq!(host.busy_msgq_limits(1, 8), -1);
        assert_eq!(host.init_ack(), -1);
        assert_eq!(host.set_os_pid(42), -1);
        let mut key: c_uint = 7;
        assert_eq!(host.async_port_key(&mut key), -1);
        assert_eq!(key, 7, "an unsupported hook must not touch the out-parameter");

        // Monitors.
        assert!(host.monitor_process(NIL).is_none());
        // The three-way hook's default delegates to the two-way one: `None` ⇒ the C's `-1`.
        assert!(matches!(host.monitor_process_result(NIL), MonitorResult::Error));
        assert_eq!(host.demonitor_process(core::ptr::null()), -1);
        assert!(host.get_monitored_process(core::ptr::null()).is_none());

        // Port data lock.
        assert!(host.pdl_create().is_none());
        assert_eq!(host.pdl_lock(core::ptr::null_mut()), -1);
        assert_eq!(host.pdl_unlock(core::ptr::null_mut()), -1);
        assert!(host.pdl_get_refc(core::ptr::null_mut()).is_none());
        assert!(host.pdl_inc_refc(core::ptr::null_mut()).is_none());
        assert!(host.pdl_dec_refc(core::ptr::null_mut()).is_none());

        // Deprecated wall clock.
        assert!(host.now().is_none());
    }
}
