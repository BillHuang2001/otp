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

//! The port half of the runtime-native BIF floor: `erlang:port_command/2,3`,
//! `erlang:port_control/3` and the closed-port half of `erlang:port_info/1,2`.
//!
//! `beam_bifs` owns the *roster* of port BIFs (`open_port/2`, `port_close/1`,
//! `port_info/1,2`, `list_to_port/1`, `port_to_list/1` — see
//! `../../beam_bifs/src/port.rs`) and reaches the port *machinery* through the
//! [`PortHook`](beam_bifs::PortHook) seam, which [`crate::port::RuntimePorts`] implements.
//! Three things do **not** fit that seam and are served here instead, ahead of the registry:
//!
//! | Call | Why it is runtime-native |
//! |---|---|
//! | `erlang:port_command/2,3` | writing to the port's write stream is the port's *input* direction, which the `PortHook` seam has no method for — the runtime owns the write handle |
//! | `erlang:port_control/3` | the driver's `control` callback, which the `PortHook` seam has no method for either (an `erlang.erl` wrapper over `erts_internal:port_control/3`) |
//! | `erlang:port_info/1,2` | two answers a `PortHook` returning `{Item, Value}` pairs cannot express: `undefined` for a port the runtime does not hold, and the bare `[]` for a live, **unregistered** port's `registered_name` item |
//!
//! # `port_command/2,3`
//!
//! `Data` is Erlang **iodata** (`binary() | iolist()`) — a binary, a char list, or a nested
//! iolist of integers/binaries (`io_lib:format/2` output is the common case). A bare integer
//! is **not** iodata: `iolist_to_binary(42)` is `badarg` in the oracle, and so is
//! `port_command(P, 42)`, even though `42` is a valid *element* (`port_command(P, [42])` is
//! `true`). It is written to the
//! port's write stream — the child's stdin for a `spawn`/`spawn_executable` port, and the
//! **adopted, duplex descriptor** for a `{fd, N}` port (the oracle writes to the descriptor
//! for a `{fd, N}` port too) — which is what makes `open_port({spawn,"cat"},[])` echo its
//! input back. The read side is
//! [`RuntimeContext::poll_port_messages`](crate::context::RuntimeContext).
//!
//! `/3` `Options` are validated **and honored**, and the port's **busy state** decides the answer
//! exactly as it does in the C (`erts_port_output`, `io.c:1749-1764`; `erl_bif_port.c:192-220`).
//! A driver marks its port busy with `set_busy_port`; this runtime records that bit on the
//! `{spawn_driver, Name}` port's metadata and reads it back here:
//!
//! | Port state | Options | Answer |
//! |---|---|---|
//! | busy | `[nosuspend]` (no `force`) | the atom `false` — the command is dropped, nothing written |
//! | busy | `[force]` | `true` if the driver entry advertises `ERL_DRV_FLAG_SOFT_BUSY`, else `error:notsup` |
//! | busy | neither | the C **suspends** the caller until the port is idle, then `true` — a typed capability gap here (no BIF-suspend channel; see [`RuntimeContext::port_command`]) |
//! | idle | any | `true` — except `[force]` on a driver **without** `ERL_DRV_FLAG_SOFT_BUSY`, which is `error:notsup` |
//!
//! The `force` rule is a property of the driver **entry**, not of the current busy state: the C
//! rejects `[force]` with `notsup` whenever the driver does not advertise `ERL_DRV_FLAG_SOFT_BUSY`
//! (`erl_bif_port.c:195-198`), idle or busy alike. A `spawn`/`fd` port — and a built-in driver
//! port, whose entry flags the built-in registry does not expose — therefore always answers
//! `notsup` for `[force]`, which is the oracle's own answer for `{spawn, Cmd}` and `{fd, N}`.
//! `nosuspend` on an idle port, like an empty option list, changes nothing.
//!
//! Every other option — an unknown atom, a non-atom element, an improper tail or a non-list
//! container — is `error:badarg`, and the option list is validated **before** the port is even
//! looked up, exactly as the C loops the options first. The port liveness check then precedes the
//! busy/`force` decision, so a dead port with `[force]` is `badarg` rather than `notsup`. The
//! result on the ordinary path is the atom `true` (the oracle's own value). A non-port argument, a
//! port the runtime does not hold (closed, or never opened), and non-iodata are
//! [`BeamError::BadArg`] — the oracle's own reason.
//!
//! # `port_info/1,2`
//!
//! The floor claims exactly two answers that `beam_bifs`'s `{Item, Value}` pair list cannot
//! express:
//!
//! * a port the runtime does **not** hold (closed, or never opened) is the atom
//!   `undefined` — the oracle's answer for both arities, where `beam_bifs` would build `[]`
//!   from the empty pair list for `/1` and `badarg` for `/2`;
//! * the `/2` item `registered_name` on a **live** port is the bare **empty list** `[]` when
//!   the port is unregistered — the oracle's own value (`erlang.erl` passes
//!   `erts_internal:port_info/2`'s `[]` straight through), which a `{registered_name, _}`
//!   pair cannot carry. `beam_rt` keeps no port registrations (`erlang:register/2` registers
//!   pids only), so a live port is always unregistered here.
//!
//! Everything else falls through to the registry unchanged: a **live** port's other items
//! are `beam_bifs`'s own `port_info/1`/`port_info/2` (including its `badarg`-for-an-unknown-
//! *item* rule and its `=:=` lookup), so no other item vocabulary is duplicated.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_process::PortId;
use beam_term::{is_small, make_small, signed_value, Eterm, NIL};

use crate::context::RuntimeContext;
use crate::error;

/// The two recognized `erlang:port_command/3` options, as
/// [`RuntimeContext::port_command_options`] parsed them.
///
/// Both are meaningful: `force` bypasses a busy port (and is the oracle's `error:notsup` unless the
/// driver entry advertises `ERL_DRV_FLAG_SOFT_BUSY`), while `nosuspend` turns a busy port's suspend
/// into the atom `false`. `/2` — and a `/3` with `[]` — is [`CommandOptions::default`], i.e. both
/// unset.
#[derive(Clone, Copy, Default)]
struct CommandOptions {
    /// The atom `force` was named — the C's `ERTS_PORT_SIG_FLG_FORCE`.
    force: bool,
    /// The atom `nosuspend` was named — the C's `ERTS_PORT_SIG_FLG_NOSUSPEND`.
    nosuspend: bool,
}

impl RuntimeContext<'_> {
    /// The single entry the [`try_native`](RuntimeContext::try_native) dispatch arm added
    /// for the port surface calls: `erlang:port_command/2,3` and the native half of
    /// `erlang:port_info/1,2`.
    ///
    /// Anything this floor does not claim — every other `(function, arity)` of the two
    /// names — is `None`, i.e. "not a native BIF, fall through to the `beam_bifs`
    /// registry", exactly as if the arm did not exist.
    pub(super) fn port_native(
        &mut self,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        match (function, arity) {
            ("port_command", 2) => Some(self.port_command(args, false)),
            ("port_command", 3) => Some(self.port_command(args, true)),
            ("port_control", 3) => Some(self.port_control(args)),
            ("port_info", 1) | ("port_info", 2) => self.port_info_native(args),
            _ => None,
        }
    }

    /// `erlang:port_command(Port, Data[, Options])`: write `Data`'s bytes to the port's write
    /// stream and answer the atom `true` — or the atom `false`, or a raise, when the port's
    /// **busy state** intervenes.
    ///
    /// The steps follow the oracle's own order (`erts_internal_port_command_3`,
    /// `../../erts/emulator/beam/erl_bif_port.c:168-236`, with the `badopt` → `badarg` mapping
    /// `erlang.erl`'s `port_command/3` wrapper applies):
    ///
    /// 1. decode the port (a non-port is [`BeamError::BadArg`]);
    /// 2. validate **every** `/3` option ([`RuntimeContext::port_command_options`]) — before the
    ///    port is even looked up, exactly as the C loops the option list first;
    /// 3. the port must still be live — the same check the `/2` path makes, and it
    ///    deliberately precedes the `force`/busy decisions, so a dead port with `[force]` is
    ///    `badarg` rather than `notsup`;
    /// 4. `force` asks to bypass the busy state. The C rejects it with `error:notsup` unless the
    ///    driver **entry** advertises `ERL_DRV_FLAG_SOFT_BUSY` (`erl_bif_port.c:195-198`) — and
    ///    that test is **independent of the port's current busy state**: `[force]` on an *idle*
    ///    port whose driver lacks the flag is `notsup` too (measured against erts-17.1);
    /// 5. otherwise, a **busy** port follows the C's busy arms (`erl_bif_port.c:211-220`):
    ///    `nosuspend` answers the atom `false` (nothing is written, and — matching the C, which
    ///    checks the busy state *before* it converts the iolist, `io.c:1760-1764` — even
    ///    non-iodata is not rejected), while a plain command makes the C **suspend** the caller
    ///    until the port goes idle and then answer `true`;
    /// 6. otherwise collect `Data` as iodata and write it — to the child's stdin, or to the
    ///    adopted duplex descriptor of a `{fd, N}` port.
    ///
    /// # The one remaining divergence
    ///
    /// Step 5's plain-command-on-a-busy-port arm is the C's `erts_suspend` + `ERTS_BIF_YIELD3`: the
    /// caller blocks until the port is idle and the BIF is then **re-run**. This runtime's BIF seam
    /// (`ExecutionContext::call_bif`) has no way to express "block the caller and re-run me" — its
    /// only shapes are a value and an error, and the engine's sole suspend is the `receive` family's
    /// `Step::Wait` — so fabricating the C's eventual `true` would be a **wrong result**, and there
    /// is no truthful suspend to take. That case therefore raises the runtime's typed capability gap
    /// ([`crate::error::unsupported`], exit 3) rather than answering at all. Every other arm —
    /// including both busy arms that *do* have an answer (`false`, `notsup`) — matches the oracle.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a non-port, a port the runtime does not hold, a malformed
    /// or unrecognized option list, or non-iodata `Data`; the `notsup` raise for `force` on a port
    /// whose driver does not advertise `ERL_DRV_FLAG_SOFT_BUSY`; the typed capability gap for a
    /// plain command on a busy port (above); [`BeamError::Io`] when the write fails.
    fn port_command(&mut self, args: &[Eterm], with_options: bool) -> Result<Eterm> {
        let port = PortId::from_eterm(*args.first().ok_or(BeamError::BadArg)?)?;
        let data = *args.get(1).ok_or(BeamError::BadArg)?;
        let options = if with_options {
            self.port_command_options(*args.get(2).ok_or(BeamError::BadArg)?)?
        } else {
            CommandOptions::default()
        };
        // The registry is reached through a cheap clone of its `Rc` handle so writing to the
        // child never holds a borrow of the runtime while a term (the atom `true`) is built —
        // the same snapshot rule the port poller follows (`crate::port`). The liveness check and
        // the two busy facts are read under **one** borrow: they are the C's `sig_lookup_port`
        // followed by its `prt->drv_ptr->flags`/`ERTS_PTS_FLG_BUSY_PORT` reads (`erl_bif_port.c:192-198`,
        // `io.c:1749-1764`), taken before any decision.
        let ports = std::rc::Rc::clone(&self.runtime.ports);
        let (busy, soft_busy) = {
            let registry = ports
                .try_borrow()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            if !registry.is_live(port) {
                return Err(BeamError::BadArg);
            }
            (registry.is_busy(port), registry.soft_busy(port))
        };
        if options.force {
            // `force` pushes the command through a busy port. Only a driver that advertises
            // `ERL_DRV_FLAG_SOFT_BUSY` can accept it; every other driver — the spawn/`fd` drivers
            // included — is the oracle's `error:notsup`, whether or not the port is busy.
            // Deliberately **not** a capability gap (see `crate::error`'s `unsupported` convention):
            // the oracle raises the same `notsup`.
            if !soft_busy {
                return Err(error::message("notsup"));
            }
        } else if busy {
            if options.nosuspend {
                // The C answers the atom `false` and drops the command (nothing is written).
                return self.atom("false");
            }
            // The C suspends the caller (`erts_suspend` + `ERTS_BIF_YIELD3`) and re-runs the BIF
            // once the port is idle, then answers `true`. This runtime has no BIF-suspend channel,
            // so there is no truthful answer to give — a typed capability gap, never a fabricated
            // `true`.
            return Err(error::unsupported(
                "port_command on a busy port: suspending the caller until the port is idle is not \
                 implemented",
            ));
        }
        let mut bytes = Vec::new();
        self.iodata_bytes(data, &mut bytes)?;
        {
            let mut registry = ports
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            registry.write_command(port, &bytes)?;
        }
        self.atom("true")
    }

    /// Validates `erlang:port_command/3`'s `Options` and reports which of the two recognized
    /// option atoms were named.
    ///
    /// The oracle accepts exactly the atoms `force` and `nosuspend` as elements
    /// (`erts_internal_port_command_3`'s `is_not_nil`/`is_list` loop, `erl_bif_port.c:175-190`)
    /// and maps everything else to `badarg`: an unknown atom, a non-atom element, an **improper**
    /// tail (`[force|bogus]`) and a non-list container all fail. Both accepted options are reported
    /// back because both change the answer: `force` bypasses a busy port (and is rejected `notsup`
    /// unless the driver advertises `ERL_DRV_FLAG_SOFT_BUSY`), and `nosuspend` turns a busy port's
    /// suspend into the atom `false`.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a malformed option container/tail or an unrecognized
    /// option element.
    fn port_command_options(&self, options: Eterm) -> Result<CommandOptions> {
        let mut parsed = CommandOptions::default();
        for element in terms::list_elements(options).map_err(|_| BeamError::BadArg)? {
            match self.atom_name(element).as_deref() {
                Some("force") => parsed.force = true,
                Some("nosuspend") => parsed.nosuspend = true,
                _ => return Err(BeamError::BadArg),
            }
        }
        Ok(parsed)
    }

    /// `erlang:port_control(Port, Operation, Data)`: hand the drive's own `control` callback an
    /// operation and answer the **bytes it replied with**.
    ///
    /// `erlang:port_control/3` is an `erlang.erl` wrapper over `erts_internal:port_control/3`
    /// (`../../erts/preloaded/src/erlang.erl:12212`), which is the same shape as `port_command/3`
    /// — the `PortHook` seam has no `control` method — so it is served here, on the floor, ahead
    /// of the registry.
    ///
    /// The decode follows `erts_internal_port_control_3`
    /// (`../../erts/emulator/beam/erl_bif_port.c:322-360`):
    ///
    /// 1. `Port` must be a port term the runtime holds — anything else is [`BeamError::BadArg`]
    ///    (`sig_lookup_port`), the oracle's own reason for a closed/unknown/live-less port;
    /// 2. `Operation` is read with `term_to_Uint`: a non-negative integer up to `UINT_MAX`
    ///    ([`RuntimeContext::term_to_uint`]); out of range is `badarg`;
    /// 3. `Data` is iodata ([`RuntimeContext::iodata_bytes`], `erts_iolist_to_buf`), so
    ///    `port_control(P, 1, <<"x">>)` and `port_control(P, 1, [$x])` are both accepted; a bare
    ///    integer is not iodata and is `badarg`, exactly as for `port_command/2`.
    ///
    /// The verb is forwarded to the driver as its **decimal `c_uint` string** — the convention
    /// [`RuntimePorts::port_control`](crate::port::RuntimePorts::port_control) documents and
    /// `beam_io`'s C-driver host parses. The reply is the **driver's own bytes**: a driver with no
    /// `control` slot, or one that returns a negative result, is `badarg` (the C's
    /// `call_driver_control`, `io.c:4035/4074`).
    ///
    /// An **internal** port — a `{spawn, Cmd}` / `{spawn_executable, Path}` / `{fd, N}` port, whose
    /// built-in driver has no `control` callback — is `badarg` for **every** verb: the C rejects it
    /// before any callback (`call_driver_control`, `io.c:4035`) and the oracle answers `badarg`
    /// (measured). Only a `{spawn_driver, Name}` port carries a user driver whose `control` slot may
    /// answer ([`PortRegistry::is_driver_port`]).
    ///
    /// The reply is an Erlang **list of byte integers** — `write_port_control_result`
    /// (`io.c:4098-4175`) renders the bytes with `buf_to_intlist` whenever the driver did **not**
    /// set `PORT_CONTROL_FLAG_BINARY`. A driver that *did* set the flag — `set_port_control_flags`
    /// from its `control` callback, recorded in the port's control-flag cell — gets a **binary**
    /// instead, which is the oracle's own shape for both modes (measured: a list reply is
    /// `[97,98,99]`, a flagged reply `<<97,98,99>>`, and a `NULL` reply pointer `[]`).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a non-port / unknown port, an **internal** port, an out-of-range
    /// `Operation`, non-iodata `Data`, a driver with no `control` slot or a negative driver result;
    /// the typed capability gap of [`crate::port::RuntimePorts::port_control`] for a reply the
    /// driver answers from memory of its own, and of the port table already being borrowed.
    fn port_control(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let port = PortId::from_eterm(*args.first().ok_or(BeamError::BadArg)?)?;
        let operation = Self::term_to_uint(*args.get(1).ok_or(BeamError::BadArg)?)?;
        let data = *args.get(2).ok_or(BeamError::BadArg)?;
        // The registry is reached through a cheap clone of its `Rc` handle, exactly as
        // `port_command` does, so the reply term is built after the borrow is released. The
        // binary/list decision is read *after* the call: that is when the driver's `control`
        // callback has had its chance to set `PORT_CONTROL_FLAG_BINARY` (`io.c:4403-4411`).
        let ports = std::rc::Rc::clone(&self.runtime.ports);
        let (reply, binary) = {
            let mut registry = ports
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            let mut bytes = Vec::new();
            self.iodata_bytes(data, &mut bytes)?;
            let reply = registry.port_control(port, &operation.to_string(), &bytes)?;
            let binary = registry.control_binary(port);
            (reply, binary)
        };
        if binary {
            terms::make_heap_binary(self, &reply)
        } else {
            Self::int_list(self, &reply)
        }
    }

    /// `term` as a `Uint` — the C's `term_to_Uint`: a non-negative integer that fits `UINT_MAX`,
    /// else [`BeamError::BadArg`].
    ///
    /// This runtime's small integers already reach `2^59` on 64-bit, so a **bignum** is always
    /// past `UINT_MAX` and `term_to_Uint`'s own overflow arm covers it; a float, an atom or a
    /// negative integer fails the same way (the C's `badarg`).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for anything that is not a small integer in `0..=u32::MAX`.
    fn term_to_uint(term: Eterm) -> Result<u32> {
        if !is_small(term) {
            return Err(BeamError::BadArg);
        }
        u32::try_from(signed_value(term)).map_err(|_| BeamError::BadArg)
    }

    /// `bytes` as an Erlang **list of byte integers** — the `buf_to_intlist` rendering of a port
    /// control reply (`io.c:4160`).
    ///
    /// # Errors
    ///
    /// Whatever the heap reports when the list cannot be built.
    fn int_list(&mut self, bytes: &[u8]) -> Result<Eterm> {
        let elements: Vec<Eterm> = bytes.iter().map(|byte| make_small(*byte as i64)).collect();
        terms::make_list(self, &elements)
    }

    /// The native half of `erlang:port_info/1,2`: `undefined` for a port the runtime does
    /// not hold, and the bare `[]` for a **live** port's `registered_name` item.
    ///
    /// `None` (fall through to `beam_bifs`) when the argument is not a port term at all (the
    /// registry's own `badarg` is the oracle's answer there), and — for a **live** port —
    /// for every item except `registered_name` (whose `{Item, Value}` pair list answers
    /// them).
    pub(super) fn port_info_native(&mut self, args: &[Eterm]) -> Option<Result<Eterm>> {
        let port = args.first().copied().and_then(|term| PortId::from_eterm(term).ok())?;
        let live = {
            let registry = self.runtime.ports.try_borrow().ok()?;
            registry.is_live(port)
        };
        if !live {
            // A closed/unknown port is `undefined` for **both** arities, whatever the item.
            return Some(self.atom("undefined"));
        }
        // A **live** port: the floor owns exactly the `registered_name` item. `beam_rt` keeps
        // no port registrations (`erlang:register/2` registers pids only), so the port is
        // unregistered and the oracle's answer is the bare empty list `[]` — deliberately
        // **not** a `{registered_name, _}` pair, which is what `beam_bifs`'s pair search
        // would build. Every other item falls through unchanged.
        if args.len() == 2 && self.atom_name(args[1]).as_deref() == Some("registered_name") {
            return Some(Ok(NIL));
        }
        None
    }

    /// Collects `term` as Erlang **iodata** into `out`: a binary, or a (possibly nested)
    /// list of byte-sized integers/binaries.
    ///
    /// This is `iolist_to_binary/1`'s acceptance rule, restricted to what the port stream
    /// can carry: a non-integer *element*, an out-of-range element integer, an improper tail
    /// and a non-iodata term are all [`BeamError::BadArg`]. The **top level** must be a
    /// binary or a list — a bare integer is deliberately rejected, because Erlang's `iodata()`
    /// is `binary() | iolist()` (`iolist_to_binary(42)` and `port_command(P, 42)` are both
    /// `badarg` in the oracle), even though `42` is a valid *element*
    /// (`port_command(P, [42])` is `true`). Unlike the printer helper `chars_to_string` (a
    /// `String`, so bytes must be UTF-8), the port stream is a **byte** stream: a binary of
    /// arbitrary bytes goes through untouched, so a `port_command` of `<<255,0,1>>` writes
    /// exactly those three bytes.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a term that is not iodata.
    fn iodata_bytes(&self, term: Eterm, out: &mut Vec<u8>) -> Result<()> {
        iodata_bytes(term, out)
    }
}

/// Collects `term` as Erlang **iodata** into `out` — a binary, or a (possibly nested) list of
/// byte-sized integers/binaries.
///
/// This is `iolist_to_binary/1`'s acceptance rule, restricted to what a port stream can carry: a
/// non-integer *element*, an out-of-range element integer, an improper tail and a non-iodata term
/// are all [`BeamError::BadArg`]. The **top level** must be a binary or a list — a bare integer is
/// deliberately rejected, because Erlang's `iodata()` is `binary() | iolist()`
/// (`iolist_to_binary(42)` is `badarg`), even though `42` is a valid *element*.
///
/// It is a **free** function rather than a method so a caller that holds no [`RuntimeContext`] can
/// still flatten iodata — the NIF host's `enif_port_command` hook
/// ([`crate::nif::RuntimeNifServices`]) is exactly such a caller. The method form
/// [`RuntimeContext::iodata_bytes`] forwards here.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a term that is not iodata.
pub(crate) fn iodata_bytes(term: Eterm, out: &mut Vec<u8>) -> Result<()> {
    iolist_bytes(term, out, true)
}

/// Collects one iodata *element* into `out`: a byte-sized small integer, a binary, or a
/// (possibly nested) list of those.
///
/// `top_level` distinguishes the outermost call from a list element: a bare integer is only iodata
/// as an element (`iodata() = binary() | iolist()`, never an integer), so the outermost integer is
/// [`BeamError::BadArg`] while an element integer is a byte.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a term that is not iodata.
fn iolist_bytes(term: Eterm, out: &mut Vec<u8>, top_level: bool) -> Result<()> {
    if is_small(term) {
        if top_level {
            // `iodata() = binary() | iolist()`: a bare integer is not iodata.
            return Err(BeamError::BadArg);
        }
        out.push(u8::try_from(signed_value(term)).map_err(|_| BeamError::BadArg)?);
        return Ok(());
    }
    if let Ok(bytes) = terms::binary_bytes(term) {
        out.extend_from_slice(bytes);
        return Ok(());
    }
    if let Ok(elements) = terms::list_elements(term) {
        for element in elements {
            iolist_bytes(element, out, false)?;
        }
        return Ok(());
    }
    Err(BeamError::BadArg)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::test_support::{atom, call, chars, runtime};
    use crate::port::driver::BusyFlag;
    use crate::runtime::Runtime;
    use beam_bifs::NullSink;
    use beam_term::{make_small, NIL};

    /// An iolist `[72, <<"i">>, [33]]` (⇒ `"Hi!"`) built on the context's heap.
    fn iodata(context: &mut RuntimeContext<'_>) -> Eterm {
        let inner = terms::make_list(&mut *context, &[make_small(33)]).expect("a nested list");
        let binary = terms::make_heap_binary(&mut *context, b"i").expect("a binary");
        terms::make_list(&mut *context, &[make_small(72), binary, inner]).expect("an iolist")
    }

    /// A real, **live** `cat` port opened through the runtime's own `open_port/2` — the
    /// same child the write test below drives (and the one `cat` echoes back).
    fn live_port(runtime: &mut Runtime) -> PortId {
        let name = {
            let mut sink = NullSink;
            let mut context = RuntimeContext::with_output(runtime, &mut sink);
            let command = context.charlist("cat").expect("a command");
            let spawn = context.atom("spawn").expect("the spawn atom");
            terms::make_tuple(&mut context, &[spawn, command]).expect("a name tuple")
        };
        let port = call(runtime, "erlang", "open_port", &[name, NIL]).expect("open_port");
        PortId::from_eterm(port).expect("a port term")
    }

    /// `elements` as a list on the runtime's heap (the `/3` option-container shape).
    fn list_of(runtime: &mut Runtime, elements: &[Eterm]) -> Eterm {
        terms::make_list(&mut RuntimeContext::new(runtime), elements).expect("a list")
    }

    /// The **error**-class raise reason atom of `error`, panicking when it is not one.
    ///
    /// The floor builds its message raises with [`crate::error::message`], whose reason is an
    /// atom backed by an owned `String`; [`BeamError::as_atom`] must return a `&'static str`
    /// and so reports `None` for it — the name has to be read through
    /// [`ExitReason::as_atom`](beam_common::ExitReason::as_atom).
    fn raise_atom(error: &BeamError) -> String {
        match error {
            BeamError::Raise(raise) => {
                assert_eq!(
                    raise.class,
                    beam_common::ExceptionClass::Error,
                    "the oracle raises `error`: {raise}"
                );
                raise
                    .reason
                    .as_atom()
                    .unwrap_or_else(|| panic!("a non-atom reason: {raise}"))
                    .to_string()
            }
            other => panic!("not a raise: {other}"),
        }
    }

    #[test]
    fn port_command_writes_the_char_list_bytes_and_answers_true() {
        let mut runtime = runtime();
        // Open a real `cat` through the runtime's own port table, then write to it. (What
        // the child does with the bytes — `cat` echoes them — is asserted through the
        // port's own stream handle in `crate::port`'s tests, and through the poller at the
        // CLI boundary in `tests/eval.rs`.)
        let port = live_port(&mut runtime);
        let data = chars(&mut runtime, "hello");

        let answer = call(&mut runtime, "erlang", "port_command", &[port.to_eterm(), data])
            .expect("port_command/2");
        assert_eq!(runtime.atom_name_of(answer), Some("true"));

        // The port is still live (a write never closes it) and its child stdin was consumed
        // by the write rather than by `open_port`/`port_close`.
        let ports = std::rc::Rc::clone(&runtime.ports);
        let registry = ports.try_borrow().expect("the port table");
        assert!(registry.is_live(port), "port_command must not close the port");
    }
    #[test]
    fn port_command_rejects_a_non_port_a_dead_port_and_non_iodata() {
        let mut runtime = runtime();
        let data = chars(&mut runtime, "x");

        // A non-port argument.
        assert!(matches!(
            call(&mut runtime, "erlang", "port_command", &[make_small(1), data]),
            Err(BeamError::BadArg)
        ));
        // A port the runtime does not hold (never opened, i.e. closed from here).
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_command",
                &[PortId::new(7).to_eterm(), data]
            ),
            Err(BeamError::BadArg)
        ));
        // Non-iodata: an atom is neither an integer, a binary nor a list.
        let bad = atom(&mut runtime, "not_iodata");
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_command",
                &[PortId::new(7).to_eterm(), bad]
            ),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn port_command_3_tolerates_a_list_of_options() {
        let mut runtime = runtime();
        let data = chars(&mut runtime, "x");
        // `nosuspend` is a real `port_command/3` option and is accepted; the write itself
        // still happens (here: it fails the port-liveness check on a dead port, so the
        // option list was clearly *not* the reason) — the liveness check precedes both the
        // `force` check and the write, exactly as the oracle looks the port up first.
        let option = atom(&mut runtime, "nosuspend");
        let options = list_of(&mut runtime, &[option]);
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_command",
                &[PortId::new(7).to_eterm(), data, options]
            ),
            Err(BeamError::BadArg)
        ));
        // An empty option list is fine too (the `/2` arity is `port_command(P, D, [])`).
        let empty = list_of(&mut runtime, &[]);
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_command",
                &[PortId::new(7).to_eterm(), data, empty]
            ),
            Err(BeamError::BadArg)
        ));
        // A non-list option container is a different badarg: the `/3` arity validates it.
        let bad = atom(&mut runtime, "not_options");
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_command",
                &[PortId::new(7).to_eterm(), data, bad]
            ),
            Err(BeamError::BadArg)
        ));
    }

    /// `[force]` on a **live** port: the spawn driver does not carry
    /// `ERL_DRV_FLAG_SOFT_BUSY`, so the oracle raises `error:notsup` (never `badarg`, and
    /// never a capability gap — `notsup` does not start with `unsupported`).
    #[test]
    fn port_command_3_force_on_a_live_port_is_notsup() {
        let mut runtime = runtime();
        let port = live_port(&mut runtime);
        let data = chars(&mut runtime, "x");
        let force = atom(&mut runtime, "force");
        let nosuspend = atom(&mut runtime, "nosuspend");
        for options in [
            list_of(&mut runtime, &[force]),
            list_of(&mut runtime, &[force, nosuspend]),
            list_of(&mut runtime, &[nosuspend, force]),
        ] {
            let error = call(
                &mut runtime,
                "erlang",
                "port_command",
                &[port.to_eterm(), data, options],
            )
            .expect_err("`force` is not supported by the spawn driver");
            assert_eq!(raise_atom(&error), "notsup", "{error}");
            // `notsup` is a genuine error, **not** a capability gap: the differential
            // harness turns any `unsupported`-prefixed reason into the exit-3 signal, and
            // the oracle exits non-zero with `error:notsup`.
            assert_eq!(error::unsupported_reason(&error), None, "{error}");
        }
        // Rejecting `force` neither closes the port nor consumes it: a plain `/2` write
        // still succeeds afterwards.
        let answer = call(
            &mut runtime,
            "erlang",
            "port_command",
            &[port.to_eterm(), data],
        )
        .expect("port_command/2 after the notsup");
        assert_eq!(runtime.atom_name_of(answer), Some("true"));
    }

    /// Registers a live `{spawn_driver, _}`-kind port whose **busy state** the test controls,
    /// returning its id together with the shared handle a driver would write through
    /// `set_busy_port` (bypasses `open_port/2` so no C compiler is needed).
    fn busy_port(runtime: &mut Runtime, busy: bool, soft_busy: bool) -> (PortId, BusyFlag) {
        crate::port::test_support::register_busy_port(runtime, "native_port_busy", busy, soft_busy)
    }

    /// The busy-port arms of `erlang:port_command/2,3`, pinned against the oracle
    /// (erts-17.1, `erts_port_output`/`erl_bif_port.c:192-220`).
    ///
    /// The table this test locks down, for a driver that does **not** advertise
    /// `ERL_DRV_FLAG_SOFT_BUSY`:
    ///
    /// | port | options | oracle |
    /// |---|---|---|
    /// | busy | `[nosuspend]` | the atom `false` (nothing written) |
    /// | busy | `[force]` | `error:notsup` |
    /// | busy | `[]` / none | the caller is **suspended**, then `true` — a capability gap here |
    /// | idle | `[]` / none / `[nosuspend]` / **`[force]`** | `true` **except** `[force]` ⇒ `notsup` |
    ///
    /// The last row's `[force]` cell is the subtle one: the C's `notsup` test is on the driver
    /// **entry** (`prt->drv_ptr->flags & ERL_DRV_FLAG_SOFT_BUSY`), so it fires on an *idle* port
    /// too. That is measured, and it is why `{spawn, Cmd}`/`{fd, N}` ports answer `notsup` for
    /// `[force]` here exactly as the oracle does.
    #[test]
    fn port_command_consults_the_ports_busy_state_like_the_oracle() {
        let mut runtime = runtime();
        let data = chars(&mut runtime, "x");
        let nosuspend = atom(&mut runtime, "nosuspend");
        let force = atom(&mut runtime, "force");

        // A busy port whose driver does not advertise `SOFT_BUSY`.
        let (port, flag) = busy_port(&mut runtime, true, false);
        // `[nosuspend]` ⇒ the atom `false`, nothing written.
        let nosuspend_only = list_of(&mut runtime, &[nosuspend]);
        let answer = call(
            &mut runtime,
            "erlang",
            "port_command",
            &[port.to_eterm(), data, nosuspend_only],
        )
        .expect("nosuspend on a busy port is not an error");
        assert_eq!(runtime.atom_name_of(answer), Some("false"));
        // `[force]` ⇒ `notsup` (the entry lacks `SOFT_BUSY`), whatever `nosuspend` adds.
        for options in [
            list_of(&mut runtime, &[force]),
            list_of(&mut runtime, &[force, nosuspend]),
            list_of(&mut runtime, &[nosuspend, force]),
        ] {
            let error = call(
                &mut runtime,
                "erlang",
                "port_command",
                &[port.to_eterm(), data, options],
            )
            .expect_err("a driver without SOFT_BUSY rejects `force`");
            assert_eq!(raise_atom(&error), "notsup", "{error}");
            assert_eq!(
                error::unsupported_reason(&error),
                None,
                "`notsup` is a genuine error, not a capability gap: {error}"
            );
        }
        // A plain command on the busy port is the C's `erts_suspend`: a typed capability gap here
        // (the runtime has no BIF-suspend channel), and deliberately **not** a fabricated `true`.
        for options in [data, list_of(&mut runtime, &[])] {
            let args: Vec<Eterm> = if options == data {
                vec![port.to_eterm(), data]
            } else {
                vec![port.to_eterm(), data, options]
            };
            let error = call(&mut runtime, "erlang", "port_command", &args)
                .expect_err("a plain command on a busy port cannot answer");
            let reason = error::unsupported_reason(&error)
                .unwrap_or_else(|| panic!("a capability gap, not {error}"));
            assert!(
                reason.contains("busy port"),
                "the reason names the busy state: {reason}"
            );
        }
        // A command the port accepts must not have been written by the refused arms: the port is
        // still busy (the driver never cleared it).
        {
            let ports = std::rc::Rc::clone(&runtime.ports);
            let registry = ports.try_borrow().expect("the port table");
            assert!(registry.is_busy(port), "nothing cleared the busy state");
            assert!(!registry.soft_busy(port), "the fixture driver is not SOFT_BUSY");
        }

        // Now clear it: the same three shapes all answer `true` **except** `[force]`, which is
        // still `notsup` because the *driver* lacks `SOFT_BUSY` (the entry test, not a state test).
        flag.set_busy(false);
        let answer = call(
            &mut runtime,
            "erlang",
            "port_command",
            &[port.to_eterm(), data],
        )
        .expect("an idle port accepts the write");
        assert_eq!(runtime.atom_name_of(answer), Some("true"));
        let answer = call(
            &mut runtime,
            "erlang",
            "port_command",
            &[port.to_eterm(), data, nosuspend_only],
        )
        .expect("`nosuspend` on an idle port is a no-op");
        assert_eq!(runtime.atom_name_of(answer), Some("true"));
        let force_only = list_of(&mut runtime, &[force]);
        let error = call(
            &mut runtime,
            "erlang",
            "port_command",
            &[port.to_eterm(), data, force_only],
        )
        .expect_err("`force` still needs SOFT_BUSY on an idle port");
        assert_eq!(raise_atom(&error), "notsup", "{error}");
    }

    /// A driver that **does** advertise `ERL_DRV_FLAG_SOFT_BUSY` lets `[force]` through — busy or
    /// idle — and the busy state then no longer gates a plain command either.
    #[test]
    fn port_command_force_goes_through_for_a_soft_busy_driver() {
        let mut runtime = runtime();
        let data = chars(&mut runtime, "x");
        let force = atom(&mut runtime, "force");
        let nosuspend = atom(&mut runtime, "nosuspend");
        let (port, flag) = busy_port(&mut runtime, true, true);
        // Busy + `[force]` (with or without `nosuspend`) ⇒ `true`: `force` bypasses the busy arm.
        for options in [
            list_of(&mut runtime, &[force]),
            list_of(&mut runtime, &[force, nosuspend]),
        ] {
            let answer = call(
                &mut runtime,
                "erlang",
                "port_command",
                &[port.to_eterm(), data, options],
            )
            .expect("a SOFT_BUSY driver accepts `force`");
            assert_eq!(runtime.atom_name_of(answer), Some("true"));
        }
        // Busy + `[nosuspend]` (no `force`) is still the atom `false` — `SOFT_BUSY` only widens the
        // `force` arm.
        let nosuspend_only = list_of(&mut runtime, &[nosuspend]);
        let answer = call(
            &mut runtime,
            "erlang",
            "port_command",
            &[port.to_eterm(), data, nosuspend_only],
        )
        .expect("nosuspend answers false");
        assert_eq!(runtime.atom_name_of(answer), Some("false"));
        // Idle + `[force]` ⇒ `true`.
        flag.set_busy(false);
        let force_only = list_of(&mut runtime, &[force]);
        let answer = call(
            &mut runtime,
            "erlang",
            "port_command",
            &[port.to_eterm(), data, force_only],
        )
        .expect("`force` on an idle SOFT_BUSY port is the plain write");
        assert_eq!(runtime.atom_name_of(answer), Some("true"));
    }

    /// The C checks the busy state **before** it converts the iolist (`io.c:1760-1764`), so a busy
    /// port's `[nosuspend]` answers `false` even for `Data` that is **not** iodata — the command is
    /// dropped, never `badarg`. A `[force]` on a `SOFT_BUSY` driver bypasses the busy arm and *does*
    /// reach the conversion, so the same bad `Data` is `badarg` there.
    #[test]
    fn port_command_busy_short_circuits_before_iodata_conversion() {
        let mut runtime = runtime();
        let not_iodata = make_small(42);
        // Busy, no `SOFT_BUSY`: `[nosuspend]` ⇒ `false`, not `badarg`.
        let (port, _flag) = busy_port(&mut runtime, true, false);
        let nosuspend = atom(&mut runtime, "nosuspend");
        let nosuspend_only = list_of(&mut runtime, &[nosuspend]);
        let answer = call(
            &mut runtime,
            "erlang",
            "port_command",
            &[port.to_eterm(), not_iodata, nosuspend_only],
        )
        .expect("the busy arm precedes the iodata conversion");
        assert_eq!(runtime.atom_name_of(answer), Some("false"));
        // Busy, `SOFT_BUSY`: `[force]` bypasses the busy arm, so the bad iodata really is `badarg`.
        let (port, _flag) = busy_port(&mut runtime, true, true);
        let force = atom(&mut runtime, "force");
        let force_only = list_of(&mut runtime, &[force]);
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_command",
                &[port.to_eterm(), not_iodata, force_only]
            ),
            Err(BeamError::BadArg)
        ));
    }

    /// `port_command_options` reports **both** recognized atoms (the caller needs both to pick the
    /// busy arm), and still rejects everything else with `badarg`.
    #[test]
    fn port_command_options_reports_force_and_nosuspend() {
        let mut runtime = runtime();
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        let force = context.atom("force").expect("an atom");
        let nosuspend = context.atom("nosuspend").expect("an atom");
        let bogus = context.atom("bogus").expect("an atom");
        let list = |context: &mut RuntimeContext<'_>, elements: &[Eterm]| {
            terms::make_list(&mut *context, elements).expect("a list")
        };
        let options = list(&mut context, &[force, nosuspend]);
        let parsed = context
            .port_command_options(options)
            .expect("both atoms are recognized");
        assert!(parsed.force && parsed.nosuspend);
        let options = list(&mut context, &[nosuspend]);
        let parsed = context
            .port_command_options(options)
            .expect("`nosuspend` alone is recognized");
        assert!(!parsed.force && parsed.nosuspend);
        // An empty list leaves both unset (the `/2` shape).
        let parsed = context
            .port_command_options(NIL)
            .expect("[] is a valid option container");
        assert!(!parsed.force && !parsed.nosuspend);
        // An unknown atom is still `badarg`.
        let options = list(&mut context, &[bogus]);
        assert!(matches!(
            context.port_command_options(options),
            Err(BeamError::BadArg)
        ));
    }

    /// A live port with a malformed/unrecognized option list is `badarg` — the oracle's
    /// `badopt`, which `erlang.erl`'s `port_command/3` wrapper maps onto `badarg`. This is
    /// checked **before** `force` would be honored, so `[force|bogus]` is `badarg`, not
    /// `notsup`.
    #[test]
    fn port_command_3_rejects_unknown_and_malformed_options_on_a_live_port() {
        let mut runtime = runtime();
        let port = live_port(&mut runtime);
        let data = chars(&mut runtime, "x");
        let force = atom(&mut runtime, "force");
        let bogus = atom(&mut runtime, "bogus");
        let one = make_small(1);
        let not_a_list = atom(&mut runtime, "not_options");
        // `[force|bogus]`: a cons whose tail is an atom, i.e. an **improper** list.
        let improper = terms::make_cons(&mut RuntimeContext::new(&mut runtime), force, bogus)
            .expect("an improper list");
        for (label, options) in [
            ("an unknown atom", list_of(&mut runtime, &[bogus])),
            (
                "`force` mixed with an unknown atom",
                list_of(&mut runtime, &[force, bogus]),
            ),
            ("a non-atom element", list_of(&mut runtime, &[one])),
            ("an improper tail", improper),
            ("a non-list container", not_a_list),
        ] {
            match call(
                &mut runtime,
                "erlang",
                "port_command",
                &[port.to_eterm(), data, options],
            ) {
                Err(BeamError::BadArg) => {}
                Err(other) => panic!("{label} must be badarg, got {other}"),
                Ok(_) => panic!("{label} must not be accepted"),
            }
        }
    }

    #[test]
    fn port_info_on_an_unknown_port_is_undefined() {
        let mut runtime = runtime();
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        for arity in [1usize, 2] {
            let mut args = vec![PortId::new(42).to_eterm()];
            if arity == 2 {
                args.push(make_small(1));
            }
            let answer = context
                .port_info_native(&args)
                .unwrap_or_else(|| panic!("port_info/{arity} must claim an unknown port"))
                .expect("the atom undefined");
            assert_eq!(context.atom_name(answer).as_deref(), Some("undefined"));
        }
        // A non-port argument falls through (the registry's `badarg` is the oracle's answer).
        assert!(context.port_info_native(&[make_small(1)]).is_none());
    }

    /// A **live** port's `registered_name` item is the bare empty list `[]` — the oracle's
    /// value for an unregistered port (`erlang.erl` passes `erts_internal:port_info/2`'s `[]`
    /// through unchanged). `beam_rt` keeps no port registrations, so a live port is always
    /// unregistered here; every other item falls through to `beam_bifs`.
    #[test]
    fn port_info_registered_name_on_a_live_port_is_the_empty_list() {
        let mut runtime = runtime();
        let port = live_port(&mut runtime);
        let registered_name = atom(&mut runtime, "registered_name");
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        let answer = context
            .port_info_native(&[port.to_eterm(), registered_name])
            .expect("a live port's registered_name is claimed")
            .expect("the empty list");
        assert_eq!(answer, NIL, "the bare empty list, not a {{registered_name,_}} pair");
        // A different item on a live port falls through (the pair list answers it), and so
        // does `port_info/1` (whose seven-item roster never includes `registered_name`).
        assert!(context.port_info_native(&[port.to_eterm(), make_small(1)]).is_none());
        assert!(context.port_info_native(&[port.to_eterm()]).is_none());
    }

    #[test]
    fn port_native_claims_only_the_command_and_closed_info_pairs() {
        let mut runtime = runtime();
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        // The arities this floor does *not* claim fall through untouched.
        for (function, arity) in [("port_command", 1), ("port_info", 0), ("port_info", 3)] {
            assert!(
                context.port_native(function, arity, &[]).is_none(),
                "{function}/{arity} must not be claimed"
            );
        }
    }

    #[test]
    fn iodata_bytes_flattens_integers_binaries_and_nested_lists() {
        let mut runtime = runtime();
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        let term = iodata(&mut context);
        let mut out = Vec::new();
        context.iodata_bytes(term, &mut out).expect("an iolist");
        assert_eq!(out, b"Hi!");

        // The empty list is iodata and contributes nothing.
        let mut out = Vec::new();
        context.iodata_bytes(NIL, &mut out).expect("[]");
        assert!(out.is_empty());
    }

    /// A bare top-level integer is **not** iodata (`iodata() = binary() | iolist()`), so
    /// `port_command(P, 42)` is `badarg`; the same integer as a list **element** is a byte.
    /// The oracle: `iolist_to_binary(42)` ⇒ `badarg`, `iolist_to_binary([42])` ⇒ `<<"*">>`.
    #[test]
    fn iodata_rejects_a_bare_integer_but_accepts_it_as_an_element() {
        let mut runtime = runtime();
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        let mut out = Vec::new();
        assert!(matches!(
            context.iodata_bytes(make_small(42), &mut out),
            Err(BeamError::BadArg)
        ));

        // `[42]`: accepted as a byte, and `42` decodes to the byte `b"*"`.
        let one = terms::make_list(&mut context, &[make_small(42)]).expect("a one-element list");
        let mut out = Vec::new();
        context.iodata_bytes(one, &mut out).expect("[42] is iodata");
        assert_eq!(out, b"*");
    }

    #[test]
    fn iodata_rejects_non_iodata_and_out_of_range_elements() {        let mut runtime = runtime();
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        let mut out = Vec::new();
        // An atom is neither a binary nor a list.
        let bad = context.atom("not_iodata").expect("an atom");
        assert!(matches!(
            context.iodata_bytes(bad, &mut out),
            Err(BeamError::BadArg)
        ));
        // An **element** integer outside `0..=255` is `badarg` (the oracle's `[256]`).
        let out_of_range =
            terms::make_list(&mut context, &[make_small(256)]).expect("a one-element list");
        assert!(matches!(
            context.iodata_bytes(out_of_range, &mut out),
            Err(BeamError::BadArg)
        ));
        // An improper tail is not an iolist either.
        let improper = terms::make_cons(&mut context, make_small(1), make_small(2))
            .expect("an improper list");
        assert!(matches!(
            context.iodata_bytes(improper, &mut out),
            Err(BeamError::BadArg)
        ));
    }

    /// Registers a live `{spawn_driver, _}`-kind port whose `control` answers `reply` for verb
    /// `1`, and returns its id. Bypasses `open_port/2` so no C compiler is needed.
    fn driver_port(runtime: &mut Runtime, reply: &[u8]) -> PortId {
        let (port, _flags) = crate::port::test_support::register_control_port(
            runtime,
            "native_port_ctl",
            vec![crate::port::test_support::ControlVerb::replying(1, reply)],
        );
        port
    }

    /// A driver's reply is an Erlang **list of byte integers** — `write_port_control_result`'s
    /// `buf_to_intlist` arm (`io.c:4171`), which is the oracle's shape whenever the driver did not
    /// ask for `PORT_CONTROL_FLAG_BINARY`. The 300-byte case pins that there is **no** size
    /// threshold: a long reply is still an int list, never a binary.
    #[test]
    fn port_control_renders_a_driver_reply_as_a_list_of_byte_integers() {
        let mut runtime = runtime();
        for (label, reply) in [
            ("a three-byte reply", b"abc".to_vec()),
            ("a 300-byte reply", (0u8..=255).chain(0u8..44).collect()),
        ] {
            let expected: Vec<Eterm> = reply.iter().map(|byte| make_small(*byte as i64)).collect();
            let port = driver_port(&mut runtime, &reply);
            let answer = call(
                &mut runtime,
                "erlang",
                "port_control",
                &[port.to_eterm(), make_small(1), NIL],
            )
            .unwrap_or_else(|error| panic!("{label} must answer: {error}"));
            let elements = terms::list_elements(answer).unwrap_or_else(|_| {
                panic!("{label} must be a list, not a binary")
            });
            assert_eq!(elements, expected, "{label} is the driver's own bytes as ints");
        }
    }

    /// A driver that answers no bytes is the oracle's `[]` — an empty **list**, never a binary and
    /// never an error.
    #[test]
    fn port_control_with_no_reply_bytes_is_the_empty_list() {
        let mut runtime = runtime();
        let port = driver_port(&mut runtime, b"");
        let answer = call(
            &mut runtime,
            "erlang",
            "port_control",
            &[port.to_eterm(), make_small(1), NIL],
        )
        .expect("an empty reply is not an error");
        assert_eq!(answer, NIL, "the bare empty list");
    }

    /// `Data` is iodata, exactly as `erts_internal:port_control/3` reads it with
    /// `erts_iolist_to_buf` (`erl_bif_port.c:322-360`): a nested iolist is accepted.
    #[test]
    fn port_control_accepts_iodata_data() {
        let mut runtime = runtime();
        let port = driver_port(&mut runtime, b"ok");
        let data = iodata(&mut RuntimeContext::new(&mut runtime));
        let answer = call(
            &mut runtime,
            "erlang",
            "port_control",
            &[port.to_eterm(), make_small(1), data],
        )
        .expect("an iolist is iodata");
        assert_eq!(terms::list_elements(answer).expect("a list"), vec![
            make_small(111),
            make_small(107)
        ]);
    }

    /// The oracle's `badarg` cases for `erlang:port_control/3` (`erts_internal_port_control_3`):
    /// a bare-integer `Data` (not iodata), an `Operation` past `UINT_MAX` (`1 bsl 70`), a negative
    /// `Operation`, an atom and a float `Operation`.
    #[test]
    fn port_control_rejects_bad_operations_and_non_iodata_data() {
        let mut runtime = runtime();
        let port = driver_port(&mut runtime, b"ok");
        let big = call(&mut runtime, "erlang", "bsl", &[make_small(1), make_small(70)])
            .expect("1 bsl 70");
        let float = call(&mut runtime, "erlang", "float", &[make_small(1)]).expect("1.0");
        let an_atom = atom(&mut runtime, "not_a_verb");
        for (label, operation, data) in [
            ("a bare-integer Data", make_small(1), make_small(42)),
            ("an Operation past UINT_MAX", big, NIL),
            ("a negative Operation", make_small(-1), NIL),
            ("an atom Operation", an_atom, NIL),
            ("a float Operation", float, NIL),
        ] {
            match call(
                &mut runtime,
                "erlang",
                "port_control",
                &[port.to_eterm(), operation, data],
            ) {
                Err(BeamError::BadArg) => {}
                Err(other) => panic!("{label} must be badarg, got {other}"),
                Ok(value) => panic!("{label} must not be accepted: {value:?}"),
            }
        }
    }

    /// An **internal** port — a built-in `spawn`/`spawn_executable`/`fd` one — is the oracle's
    /// `badarg` for every verb, because the C rejects a driver with no `control` slot before any
    /// callback (`call_driver_control`, `io.c:4035`).
    #[test]
    fn port_control_on_an_internal_port_is_badarg() {
        let mut runtime = runtime();
        let port = live_port(&mut runtime);
        for verb in [0i64, 1, 4, 200] {
            match call(
                &mut runtime,
                "erlang",
                "port_control",
                &[port.to_eterm(), make_small(verb), NIL],
            ) {
                Err(BeamError::BadArg) => {}
                Err(other) => panic!("verb {verb} must be badarg, got {other}"),
                Ok(value) => panic!("verb {verb} must not be accepted: {value:?}"),
            }
        }
        // A port the runtime does not hold is `badarg` too.
        assert!(matches!(
            call(
                &mut runtime,
                "erlang",
                "port_control",
                &[PortId::new(9999).to_eterm(), make_small(1), NIL]
            ),
            Err(BeamError::BadArg)
        ));
    }

    /// `PORT_CONTROL_FLAG_BINARY` set from inside a `control` callback makes the reply a
    /// **binary**, and the flag **persists** for later calls on the same port — the C's
    /// `prt->control_flags` is per-port state (`io.c:5318`), read after every callback
    /// (`write_port_control_result`, `io.c:4133`).
    #[test]
    fn port_control_binary_flag_makes_the_reply_a_binary_and_persists() {
        use beam_abi::driver::types::PORT_CONTROL_FLAG_BINARY;
        let mut runtime = runtime();
        let (port, flags) = crate::port::test_support::register_control_port(
            &mut runtime,
            "native_port_ctl_flag",
            vec![
                // Verb 1: a plain list reply, no flag.
                crate::port::test_support::ControlVerb::replying(1, b"abc"),
                // Verb 2: asks for binary-mode replies, then answers.
                crate::port::test_support::ControlVerb::setting(
                    2,
                    b"bin",
                    PORT_CONTROL_FLAG_BINARY,
                ),
                // Verb 3: answers without touching the flags — the reply must still be binary.
                crate::port::test_support::ControlVerb::replying(3, b"xy"),
            ],
        );
        let reply = |runtime: &mut Runtime, verb: i64| {
            call(
                runtime,
                "erlang",
                "port_control",
                &[port.to_eterm(), make_small(verb), NIL],
            )
            .unwrap_or_else(|error| panic!("verb {verb} must answer: {error}"))
        };
        // Before the flag: a list.
        assert_eq!(
            terms::list_elements(reply(&mut runtime, 1)).expect("a list"),
            vec![make_small(97), make_small(98), make_small(99)]
        );
        // The flag-setting call: a binary.
        let binary = reply(&mut runtime, 2);
        assert_eq!(
            terms::binary_bytes(binary).expect("a binary"),
            b"bin",
            "the flag applies to the very call that set it"
        );
        assert_ne!(flags.get() & PORT_CONTROL_FLAG_BINARY, 0, "the flag is recorded");
        // ...and it persists: verb 3 never sets it, yet the reply is a binary again.
        assert_eq!(
            terms::binary_bytes(reply(&mut runtime, 3)).expect("still a binary"),
            b"xy",
            "the flag persists across calls on the same port"
        );
    }

    /// The per-port byte counters `port_info(Port, input | output)` report are real: a write
    /// bumps the port's `output`, and a driver reply bumps nothing on the *input* side (a driver
    /// port's input is harvested by the pump, not by `port_control`).
    ///
    /// `erlang:port_info/2` answers the **`{Item, Value}` pair**, never the bare value — the
    /// oracle's own shape (`erl -eval 'erlang:port_info(open_port({spawn,"cat"},[]), input)'` ⇒
    /// `{input,0}`). The helper below therefore unwraps the pair *and* pins its item, so a
    /// regression that answered the pair for the wrong item, or stopped answering a pair, fails
    /// here rather than comparing a 2-tuple against a small int.
    #[test]
    fn port_info_io_items_read_the_ports_own_byte_totals() {
        let mut runtime = runtime();
        let port = driver_port(&mut runtime, b"ok");
        let item = |runtime: &mut Runtime, name: &str| -> Eterm {
            let key = atom(runtime, name);
            let pair = call(runtime, "erlang", "port_info", &[port.to_eterm(), key])
                .unwrap_or_else(|error| panic!("port_info({name}) must answer: {error}"));
            assert_eq!(
                terms::tuple_arity(pair).expect("a tuple"),
                2,
                "`{name}` is a {{Item,Value}} pair"
            );
            assert_eq!(
                terms::tuple_element(pair, 0).expect("the pair's item"),
                key,
                "the pair's item is `{name}`"
            );
            terms::tuple_element(pair, 1).expect("the pair's value")
        };
        // A fresh port has moved no bytes — the oracle's own answer right after `open_port`.
        assert_eq!(item(&mut runtime, "input"), make_small(0));
        assert_eq!(item(&mut runtime, "output"), make_small(0));
        // Writing to the port credits its `output` total.
        let data = chars(&mut runtime, "hello");
        call(&mut runtime, "erlang", "port_command", &[port.to_eterm(), data])
            .expect("the driver accepts the write");
        assert_eq!(item(&mut runtime, "output"), make_small(5), "5 bytes written");
        // A driver port's *input* is what the pump harvests from `driver_output`, not what
        // `port_command` wrote, so it is still zero here (the write was an output).
        assert_eq!(item(&mut runtime, "input"), make_small(0));
    }
}