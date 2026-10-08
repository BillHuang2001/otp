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

//! The BIF execution context: a heap, the atom table, the calling process and the
//! optional runtime seams.
//!
//! A `BifFn` (see [`crate::registry`]) receives a [`BifContext`] and its argument
//! slice. The context carries exactly what a BIF may touch:
//!
//! * the destination [`beam_alloc::Heap`] (all BIF allocation goes through it —
//!   `beam_term::heap::Heap` is a test arena only, see `../beam_alloc/CONTEXT.md`);
//! * the [`AtomTable`] (atom construction never invents an index);
//! * the calling process's [`ProcessId`] — there is no ambient "current process"
//!   in the Rust core, so identity is injected here;
//! * the calling module's name, when the runtime knows it ([`BifContext::current_module`])
//!   — the oracle derives a BIF's call site from it (`erlang:load_nif/2`);
//! * an expanding set of **optional seams**: an [`OutputSink`] (`io:format`), a
//!   [`SendHook`] (`send/2`), a [`TableRegistry`] (`ets:*`), an [`AtomInterner`]
//!   (the atom-creating conversions), a [`FunCaller`] (the `fun`-taking
//!   `lists`/`maps` BIFs), a [`PortHook`] (the
//!   port BIFs), a [`DictHook`] (the `put/2`/`get/1`/`erase/1`/`get_keys/*`
//!   process-dictionary BIFs), a [`NifLoader`] (`erlang:load_nif/2`), a
//!   [`RecordSource`] (`records:get_definition/2`), a [`LocalNode`] (the
//!   local node identity for encoding internal pid/port ids), a
//!   [`LocalFunIdentity`] (the `NEW_FUN_EXT` identity of a local `make_fun3` fun,
//!   for `term_to_binary`), a [`ModuleInfo`] (`erlang:get_module_info/1,2`) and a
//!   [`ProcessHook`] (the process/name/global-state queries — `whereis/1`,
//!   `register/2`, `unregister/1`, `registered/0`, `is_process_alive/1`,
//!   `processes/0`, `ports/0`, `process_info/1,2`, `process_flag/2` and
//!   `group_leader/0`), a [`PersistentTerm`] (the node-global persistent-term store
//!   behind the seven `persistent_term:*` BIFs), a [`MemoryHook`] (the node's memory
//!   accounting behind `erlang:memory/0,1`) and a [`RuntimeHooks`] (the single
//!   dispatcher behind every `bif.tab` entry that only the *running* runtime can
//!   answer — process control, timers, code loading, system/nodes introspection,
//!   the `atomics`/`counters` stores, the ETS C interior, `erts_debug`/
//!   `erl_debugger`, `re`, `erl_ddll` and distribution).
//!   `beam_engine` depends only on `beam_loader`, so `beam_rt` supplies the
//!   concrete implementations; an unset seam means `io:format` writes to stdout
//!   and the `send`/`ets`/`call_fun`/port/dict/nif/process BIFs report
//!   `unsupported_bif`.

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_process::{PortId, ProcessId};
use beam_term::prelude::{make_atom, try_make_small};
use beam_term::{Atom, AtomTable, Eterm, NIL};

use crate::access;
use crate::ets::TableRegistry;

/// A destination for `io:format` output.
pub trait OutputSink {
    /// Writes `s` to the sink.
    ///
    /// # Errors
    ///
    /// A sink that fails (e.g. a broken stdout pipe) reports [`BeamError::Io`].
    fn write_str(&mut self, s: &str) -> Result<()>;
}

/// The default sink: the process's standard output.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdoutSink;

impl OutputSink for StdoutSink {
    fn write_str(&mut self, s: &str) -> Result<()> {
        use std::io::Write;
        let mut out = std::io::stdout();
        out.write_all(s.as_bytes()).map_err(BeamError::Io)
    }
}

/// A sink that discards everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl OutputSink for NullSink {
    fn write_str(&mut self, _s: &str) -> Result<()> {
        Ok(())
    }
}

/// An in-memory capture sink (tests and embedding).
#[derive(Debug, Default, Clone)]
pub struct BytesSink {
    buf: Vec<u8>,
}

impl BytesSink {
    /// An empty capture buffer.
    pub fn new() -> BytesSink {
        BytesSink { buf: Vec::new() }
    }

    /// The captured bytes.
    pub fn contents(&self) -> &[u8] {
        &self.buf
    }

    /// The captured bytes as a `String` (lossily decoded).
    pub fn as_str(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }

    /// Discards the captured bytes.
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

impl OutputSink for BytesSink {
    fn write_str(&mut self, s: &str) -> Result<()> {
        self.buf.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// Delivers `term` to the process `to` (an internal pid).
///
/// The runtime supplies this over `beam_process`/`beam_message`; Milestone-1 has no
/// in-crate delivery, so the `send/2` and `"!"/2` BIFs raise `unsupported_bif`
/// when no hook is installed.
pub trait SendHook {
    /// Sends `term` to `to`.
    ///
    /// # Errors
    ///
    /// Reports a delivery failure as a [`BeamError`].
    fn send(&mut self, to: Eterm, term: Eterm) -> Result<()>;
}

/// Calls an Erlang fun from inside a BIF.
///
/// A [`BifFn`](crate::registry::BifFn) is a plain Rust function: it cannot execute
/// BEAM code itself. The `fun`-taking `lists`/`maps` BIFs (`lists:map/2`,
/// `lists:foldl/3`, `lists:sort/2`, `maps:map/2`, …) must nevertheless *call* an
/// Erlang fun, so the runtime supplies this hook over its evaluator's closure
/// table (`beam_rt` wires it to `beam_engine::ExecutionContext`'s apply-fun seam).
/// Milestone-1 has no in-crate evaluator, so those BIFs raise `unsupported_bif`
/// when no hook is installed — never a wrong result.
pub trait FunCaller {
    /// Calls `fun` with `args`, returning its result term.
    ///
    /// # Errors
    ///
    /// Propagates whatever the call raises: an argument mismatch
    /// (`badarg`/`badarity`), or a BEAM raise (`error`/`exit`/`throw`) raised by
    /// the fun body, as a [`BeamError`].
    fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm>;
}

/// Interns a new atom name into the runtime's *global* atom table.
///
/// The context only borrows the atom table immutably ([`BifContext::atoms`]),
/// because atom indices are global and owned by the runtime (`beam_rt`), not by a
/// BIF call. The `list_to_atom`/`binary_to_atom` BIFs must nonetheless be able to
/// add a *new* atom; the runtime supplies this hook over the real, mutable
/// `AtomTable`. With no hook installed those BIFs can still resolve an *existing*
/// atom ([`BifContext::intern_atom`] looks it up first) and otherwise report
/// [`BeamError::TermNotFound`] — never a fabricated index.
pub trait AtomInterner {
    /// Interns `name`, returning its [`Atom`] index (creating it if new).
    fn intern(&mut self, name: &str) -> Atom;
}

/// Creates, closes and inspects the runtime's ports.
///
/// A port *is* an OS process and a driver (`spawn`/`fd`/…) plus a port table
/// slot — the Stage-5 machinery that lives in `beam_io`/`beam_rt`, never in a BIF.
/// The `erlang:open_port/2`/`port_close/1`/`port_info/1,2` BIFs therefore route
/// every port operation through this hook; `beam_rt` supplies the concrete
/// implementation over `beam_io` and `beam_process`. Milestone-1 has no such
/// provider, so those BIFs raise `unsupported_bif` when no hook is installed —
/// never a fabricated port id and never a silently dropped close
/// (workspace `CONTEXT.md` §3).
///
/// The hook owns argument validation: it is the only side that knows which
/// `{spawn, …}`/`{spawn_executable, …}`/`{fd, …}` names and option lists its
/// driver set accepts, and it reports an invalid Name/Options as
/// [`BeamError::BadArg`].
pub trait PortHook {
    /// `open_port(Name, Options)` — creates a port and returns its id.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a Name/Options the installed drivers do not
    /// accept; a [`BeamError::Raise`] to propagate a failure to the caller.
    fn open_port(&mut self, name: Eterm, options: Eterm) -> Result<PortId>;

    /// `port_close(Port)` — closes `port`.
    ///
    /// # Errors
    ///
    /// Reports a close failure as a [`BeamError`].
    fn close_port(&mut self, port: PortId) -> Result<()>;

    /// The `{Item, Value}` pairs of `port_info(Port)`, in the runtime's order.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a port the runtime does not know.
    fn port_info(&mut self, port: PortId) -> Result<Vec<(Eterm, Eterm)>>;
}

/// Reads and writes the calling process's **dictionary**.
///
/// The process dictionary is a per-process key/value store owned by
/// [`beam_process::Dict`] and reached at the language level through the
/// `erlang:put/2`, `erlang:get/1`, `erlang:erase/1`, `erlang:get_keys/0` and
/// `erlang:get_keys/1` BIFs. The store is a genuine GC sub-rootset of *a*
/// process, so a BIF must never reach into it directly — `beam_rt` supplies this
/// hook over the calling process's real dictionary. Milestone-1 has no such
/// provider, so those BIFs raise `unsupported_bif` when no hook is installed
/// (never a silently fabricated value; workspace `CONTEXT.md` §3).
///
/// `None` from `put`/`get`/`erase` means **absent** and the BIF layer maps it to
/// the atom `undefined` (the C's `THE_NON_VALUE`/`am_undefined`). Keys and values
/// are stored by **raw `Eterm` word equality** — the [`beam_process::Dict`]
/// contract; the hook is responsible for copying a key/value into the process
/// heap before storing it.
pub trait DictHook {
    /// `put(Key, Value)` — stores `value` under `key`, returning the **replaced**
    /// value (`None` when the key was absent).
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn put(&mut self, key: Eterm, value: Eterm) -> Result<Option<Eterm>>;

    /// `get(Key)` — the value stored under `key`, or `None` when absent.
    ///
    /// # Errors
    ///
    /// Reports a lookup failure as a [`BeamError`].
    fn get(&mut self, key: Eterm) -> Result<Option<Eterm>>;

    /// `erase(Key)` — removes `key`, returning the value that was stored under it
    /// (`None` when absent).
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn erase(&mut self, key: Eterm) -> Result<Option<Eterm>>;

    /// `get_keys/0` — every key, in the store's order.
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn get_keys(&mut self) -> Result<Vec<Eterm>>;

    /// `get_keys(Value)` — every key whose value raw-equals `value`, in the
    /// store's order.
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn get_keys_for_value(&mut self, value: Eterm) -> Result<Vec<Eterm>>;

    /// `get/0` — every `{Key, Value}` pair in the store, in the store's order.
    ///
    /// # Errors
    ///
    /// Defaults to a typed `unsupported_bif` capability gap: a hook written
    /// before this method existed must keep compiling, and the BIF reports the
    /// gap rather than inventing an empty dictionary.
    fn get_all(&mut self) -> Result<Vec<(Eterm, Eterm)>> {
        Err(crate::error::unsupported_bif())
    }

    /// `erase/0` — empties the store, returning every `{Key, Value}` pair it
    /// held, in the store's order.
    ///
    /// # Errors
    ///
    /// Defaults to a typed `unsupported_bif` capability gap (see
    /// [`DictHook::get_all`]).
    fn erase_all(&mut self) -> Result<Vec<(Eterm, Eterm)>> {
        Err(crate::error::unsupported_bif())
    }
}

/// Reads and writes the node-global **persistent-term** store.
///
/// Persistent terms are a *node-global* key/value store — unlike the process
/// dictionary they are shared by every process — held outside every process heap in
/// their own literal area (`the_hash_table`, `erl_bif_persistent.c:187`) and
/// updated under a table-wide permission lock with thread-progress hand-off. That
/// machinery belongs to the Stage-5 runtime (`beam_rt`), so the seven
/// `persistent_term:get/0`, `get/1`, `get/2`, `put/2`, `put_new/2`, `erase/1` and
/// `info/0` BIFs route every operation through this hook; `beam_rt` supplies the
/// concrete implementation. With no hook installed **every** one of them reports a
/// typed `unsupported_bif` capability gap — never `[]`, never `undefined`, never
/// `ok` and never a made-up count (workspace `CONTEXT.md` §3).
///
/// The BIF layer owns the rendering (the `{Key, Value}` 2-tuples of `get/0`, the
/// `#{count, memory}` map of `info/0`, the `true`/`false` of `erase/1`) and the
/// oracle's two `badarg` cases, which are *store* outcomes reported here: an absent
/// key for `get/1` ([`PersistentTerm::get`] answers `None`) and a `put_new/2` onto a
/// key already holding a different value ([`PersistentTerm::put`] answers `false`).
pub trait PersistentTerm {
    /// `get(Key)` — the value stored under `key`, or `None` when the key is absent
    /// (the BIF then raises `badarg` for `get/1`, or yields the default for
    /// `get/2`).
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn get(&mut self, key: Eterm) -> Result<Option<Eterm>>;

    /// `put(Key, Value)` / `put_new(Key, Value)` — stores `value` under `key`.
    ///
    /// `only_if_new` is `false` for `persistent_term:put/2` and `true` for
    /// `persistent_term:put_new/2` (the C's `new` flag,
    /// `erl_bif_persistent.c:642,730`). Returns `true` on success — the store was
    /// updated, **or** `key` already held a value raw-equal to `value` (a no-op
    /// `ok`, `:726-729`) — and `false` for the one failure the oracle raises
    /// `badarg` for: `put_new` of a key that already holds a *different* value
    /// (`:730-733`).
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn put(&mut self, key: Eterm, value: Eterm, only_if_new: bool) -> Result<bool>;

    /// `erase(Key)` — erases `key`, returning whether it was present
    /// (`persistent_term_erase_1:417`; `true` at `:539`, `false` at `:548`).
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn erase(&mut self, key: Eterm) -> Result<bool>;

    /// `get/0` — every stored `{Key, Value}` pair, in the store's own order.
    ///
    /// The order is a hash-table walk and is **not** observable; this layer
    /// preserves whatever the hook supplies and never pins it (`do_get_all:830`).
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn get_all(&mut self) -> Result<Vec<(Eterm, Eterm)>>;

    /// `info/0` — the runtime's bookkeeping `(count, memory)` in bytes: the number
    /// of stored entries and the byte total of the table plus its literal areas
    /// (`do_info:929-978`).
    ///
    /// These are pure runtime accounting figures with no BIF-visible derivation, so
    /// they must come from the hook; this layer never invents them.
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn info(&mut self) -> Result<(u64, u64)>;
}
/// The structured failure of a NIF-load attempt.
///
/// The host ([`NifLoader`]) reports *why* a load failed; the `erlang:load_nif/2`
/// BIF renders each variant as the oracle's `{error, {Reason, Text}}` term —
/// `Reason` is the atom named here and `Text` the accompanying message
/// (`../../erts/emulator/beam/erl_nif.c`, `load_nif_error`/`erts_load_nif`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NifLoadError {
    /// `{error, {load_failed, Text}}` — the shared object could not be loaded.
    LoadFailed(String),
    /// `{error, {bad_lib, Text}}` — the library is not a usable NIF library.
    BadLib(String),
    /// `{error, {reload, Text}}` — the library is already loaded.
    Reload(String),
    /// `{error, {load, Text}}` — a load error.
    Load(String),
    /// `{error, {upgrade, Text}}` — an upgrade error.
    Upgrade(String),
    /// `{error, {old_code, Text}}` — `load_nif` called from old code.
    OldCode(String),
}

/// Loads a NIF library for the calling module.
///
/// A NIF library is a shared object whose symbols are bound into the emulator's
/// NIF/driver ABI (Stage-5 machinery that lives in `beam_rt`/`beam_abi`, never in
/// a BIF), so `erlang:load_nif/2` routes every load through this hook; `beam_rt`
/// supplies the concrete implementation. Milestone-1 has no such provider, so the
/// BIF raises `unsupported_bif` when no hook is installed (or the calling module is
/// unknown) — never a fabricated `ok` and never a silently dropped load.
///
/// `module` is the module whose code contains the call site (the oracle derives it
/// from the call instruction); `path` is the decoded string from the `Path`
/// argument. The hook owns the shared-object suffix/suffixes it appends.
///
/// The return type is a plain [`core::result::Result`] (not the crate's
/// [`beam_common::Result`] alias): a `NifLoadError` is *not* a [`BeamError`], and
/// the BIF renders it as a term, so no `From<NifLoadError> for BeamError` is needed.
pub trait NifLoader {
    /// Loads the NIF library `path` for `module`, with the caller's `load_info`.
    ///
    /// # Errors
    ///
    /// A [`NifLoadError`] naming the failure; the BIF renders it as the oracle's
    /// `{error, {Reason, Text}}` term (a normal return, not a raise — see `nif.rs`).
    fn load_nif(
        &mut self,
        module: &str,
        path: &str,
        load_info: Eterm,
    ) -> core::result::Result<(), NifLoadError>;
}

/// One field of a loaded native-record definition (`Recs` chunk), name-resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordFieldView {
    /// The field-name atom's name.
    pub name: String,
    /// The field's default value, or `None` when it has no default.
    pub default: Option<Eterm>,
}

/// A loaded native-record definition, name-resolved (the shape
/// `records:get_definition/2` renders).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordDefinitionView {
    /// Whether the record is exported from its module.
    pub is_exported: bool,
    /// Fields in declaration order.
    pub fields: Vec<RecordFieldView>,
}

/// Supplies a loaded module's native-record definitions.
///
/// A native-record definition lives in the `Recs` chunk of a loaded
/// [`beam_loader::Module`], whose atoms belong to the *loader's* atom table — not
/// this context's. The concrete provider (the runtime's code server, which owns the
/// loaded modules) therefore implements this hook and resolves the module's atom
/// table for us, handing back a name-resolved [`RecordDefinitionView`]. With no hook
/// installed, [`BifContext::record_definition`] answers `Ok(None)` — never a
/// fabricated definition.
pub trait RecordSource {
    /// The `Recs`-chunk definition named `name` in the loaded module `module`,
    /// or `Ok(None)` when no loaded module defines it.
    ///
    /// # Errors
    ///
    /// Reports a resolution failure as a [`BeamError`].
    fn record_definition(&mut self, module: &str, name: &str)
        -> Result<Option<RecordDefinitionView>>;
}

/// The runtime's local node identity for encoding internal pid/port ids: the
/// node's name atom (interned in the context's atom table) and its creation.
///
/// `term_to_binary`/`binary_to_term` of an *internal* pid/port id need the local
/// node's name atom and creation to emit a `NEW_PID_EXT`/`NEW_PORT_EXT` image. The
/// identity is runtime state (the node's own name and creation), so it is injected
/// through this hook; `beam_rt` supplies the concrete implementation. With no hook
/// installed, [`BifContext::local_node`] falls back to the Milestone-1 runtime's
/// fixed, non-distributed identity (`nonode@nohost`, creation 0) when that atom is
/// interned here, and `None` otherwise.
pub trait LocalNode {
    /// The local node identity `(node_atom, creation)`, or `None` when unknown.
    fn local_node(&mut self) -> Option<(Atom, u32)>;
}

/// The `NEW_FUN_EXT` (`112`) identity of a local (`make_fun3`) fun, for
/// `term_to_binary`.
///
/// A `make_fun3` closure inlines only its engine-internal descriptor
/// (`[uniq: u64][index: u32]`); its wire form additionally needs the owning
/// module's atom and 16-byte MD5, the legacy `old_index`/`old_uniq` and a creator
/// pid — none of which the term carries. Only the runtime that loaded the module
/// knows them (they come from its lambda table), so they are injected through this
/// hook; `beam_rt` supplies the concrete implementation. See
/// [`beam_codec::FunIdentity`] for the field meanings.
///
/// The codec never invents those fields: answering `Ok(None)` for a fun whose
/// identity the runtime cannot resolve preserves the codec's typed
/// `unsupported_local_fun` rather than producing a wrong image. With no hook
/// installed, [`BifContext::local_fun_identity`] likewise answers `Ok(None)`.
pub trait LocalFunIdentity {
    /// The `NEW_FUN_EXT` identity of `term` when it is a local (`make_fun3`) fun
    /// this runtime can attribute to a loaded module, or `Ok(None)` when the
    /// identity is unknown.
    ///
    /// # Errors
    ///
    /// Propagates the hook's own failure.
    fn local_fun_identity(&mut self, term: Eterm) -> Result<Option<beam_codec::FunIdentity>>;
}

/// Supplies the loaded-module information `erlang:get_module_info/1,2` renders.
///
/// A module's introspection data (`exports`, `attributes`, `compile`, `md5`,
/// `native`, …) lives with the *loaded* `beam_loader::Module` owned by the
/// runtime's code server — Stage-5 state that no BIF can reach. The concrete
/// provider there implements this hook and builds the requested item's term on
/// the caller's heap; the BIF layer itself owns only the argument decoding and
/// the oracle's `badarg` for an unknown module/item. With no hook installed,
/// [`BifContext::module_info`] reports a typed `unsupported_bif` capability gap
/// — never a fabricated or empty value (workspace `CONTEXT.md` §3).
pub trait ModuleInfo {
    /// The value of `item` for `module`, built on `heap`, or `Ok(None)` when the
    /// module or item is unknown (the BIF then raises `badarg`, as the oracle
    /// does).
    ///
    /// # Errors
    ///
    /// Reports a resolution/allocation failure as a [`BeamError`].
    fn module_info(
        &mut self,
        heap: &mut Heap,
        atoms: &AtomTable,
        module: &str,
        item: &str,
    ) -> Result<Option<Eterm>>;
}

/// What one `erlang:process_info/2` item resolves to.
///
/// The three outcomes are exactly the ones the oracle distinguishes, and their
/// *order* matters: the C validates every requested item (`pi_arg2ix`) **before** it
/// inspects the process (`process_info_bif`, `erl_bif_info.c:1556-1564`), so an
/// unknown item is `badarg` even for a dead pid (probed: `process_info(<dead>,
/// [status, frobnicate])` ⇒ `badarg` while `process_info(<dead>, [status])` ⇒
/// `undefined`). A hook therefore answers [`ProcessInfoItem::Unknown`] without
/// consulting the process table, and only then reports [`ProcessInfoItem::Dead`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProcessInfoItem {
    /// The item's value, built on the caller's heap.
    Value(Eterm),
    /// The item is not one this runtime reports — the oracle's `badarg`.
    Unknown,
    /// `pid` names no live process — the oracle's `undefined`.
    Dead,
}

/// The runtime's process/name/global-state queries: the name registry, process
/// liveness, the process and port inventories, per-process introspection and the
/// per-process flags.
///
/// The eleven BIFs behind this seam — `erlang:whereis/1`, `register/2`,
/// `unregister/1`, `registered/0`, `is_process_alive/1`, `processes/0`,
/// `ports/0`, `process_info/1`, `process_info/2`, `process_flag/2` and
/// `group_leader/0` — answer questions about the *running* node: the name registry
/// (`beam_process`/`beam_rt`'s process table), the port table and the per-process
/// state (flags, group leader, message queue, …). None of that is reachable from a
/// BIF, so every operation routes through this hook, whose concrete implementation
/// the runtime supplies. With no hook installed **every** one of those BIFs reports
/// a typed `unsupported_bif` capability gap — never a fabricated empty list, never
/// `undefined`, never `false` and never a made-up previous flag value
/// (workspace `CONTEXT.md` §3).
///
/// The BIF layer still owns the `badarg` cases the arguments alone decide (a
/// non-atom name, the atom `undefined` as a name, an `id` that is neither an
/// internal pid nor an internal port, a non-pid, a non-atom flag, an item
/// specification that is neither an atom nor a proper list of atoms) and checks
/// them *before* the hook is consulted.
///
/// Ground truth (read-only): `../../erts/emulator/beam/bif.c`
/// (`register_2:2310`, `unregister_1:2324`, `whereis_1:2342`, `process_flag_2:2028`,
/// `process_flag_aux:1897`, `group_leader_0:4903`, `processes_0:4101`,
/// `ports_0:4129`), `../../erts/emulator/beam/register.c`
/// (`erts_register_name:175`, `erts_whereis_name_to_id:277`, `registered_0:576`),
/// `../../erts/emulator/beam/erl_bif_info.c` (`process_info_1:1561`,
/// `process_info_2:1556`, `process_info_aux:1567`, `pi_1_keys:1008`,
/// `is_process_alive_1:4069`) and `../../erts/emulator/beam/bif.tab`
/// (`group_leader/0:78`, `process_flag/2:129`, `process_info/1:131`,
/// `process_info/2:132`).
pub trait ProcessHook {
    /// `whereis(Name)` — the id registered under `name`, or `None` when `name` is
    /// not registered (the BIF renders the atom `undefined`;
    /// `erts_whereis_name_to_id`, `register.c:277`).
    ///
    /// The id is a **pid or a port** term — the oracle's registry holds either
    /// (`erts_register_name`, `register.c:190`) — so it is handed back as the
    /// term it is.
    ///
    /// # Errors
    ///
    /// Reports a registry failure as a [`BeamError`].
    fn whereis(&mut self, name: Atom) -> Result<Option<Eterm>>;

    /// `register(Name, Id)` — registers the process/port `id` under `name`.
    ///
    /// Returns `true` on success; `false` for the failure the oracle renders as
    /// `badarg` — the name is already taken (`erts_register_name:235`), `id` is
    /// not alive (`:212`) or `id` already has a name (`:216`). The BIF layer has
    /// already checked that `name` is an atom other than `undefined` and that `id`
    /// is an internal pid or an internal port (`:184,190`).
    ///
    /// # Errors
    ///
    /// Reports a registry failure as a [`BeamError`].
    fn register(&mut self, name: Atom, id: Eterm) -> Result<bool>;

    /// `unregister(Name)` — removes `name` from the registry.
    ///
    /// Returns `true` on success; `false` when the oracle raises `badarg` — the
    /// name is not registered (`erts_unregister_name`, `bif.c:2330`).
    ///
    /// # Errors
    ///
    /// Reports a registry failure as a [`BeamError`].
    fn unregister(&mut self, name: Atom) -> Result<bool>;

    /// `registered/0` — every registered name, in the registry's own order
    /// (`registered_0`, `register.c:576`).
    ///
    /// The order is an artefact of the registry's hash layout and is **not**
    /// observable; the runtime renders whatever order its table walks in and the
    /// BIF layer preserves it verbatim.
    ///
    /// # Errors
    ///
    /// Reports a registry failure as a [`BeamError`].
    fn registered(&mut self) -> Result<Vec<Atom>>;

    /// `is_process_alive(Pid)` — whether the internal pid `pid` names a live
    /// process (`is_process_alive_1`, `erl_bif_info.c:4069`).
    ///
    /// # Errors
    ///
    /// Reports a table failure as a [`BeamError`].
    fn is_process_alive(&mut self, pid: ProcessId) -> Result<bool>;

    /// `processes/0` — every local process, in the table's own order
    /// (`processes_0`, `bif.c:4101`).
    ///
    /// The order is not observable (the oracle gives no guarantee); the BIF layer
    /// preserves whatever the hook supplies.
    ///
    /// # Errors
    ///
    /// Reports a table failure as a [`BeamError`].
    fn processes(&mut self) -> Result<Vec<ProcessId>>;

    /// `ports/0` — every local port, in the table's own order (`ports_0`,
    /// `bif.c:4129`).
    ///
    /// The order is not observable; the BIF layer preserves whatever the hook
    /// supplies.
    ///
    /// # Errors
    ///
    /// Reports a table failure as a [`BeamError`].
    fn ports(&mut self) -> Result<Vec<PortId>>;

    /// `process_info/1` — the item names this runtime reports for `pid`, in the
    /// order the oracle reports them (the C's `pi_1_keys`, `erl_bif_info.c:1008`),
    /// or `Ok(None)` when `pid` names no live process (the BIF renders the atom
    /// `undefined`).
    ///
    /// The set is the runtime's own: the oracle's `pi_1_keys` **omits**
    /// `registered_name` for a process that has no registered name
    /// (`erl_bif_info.c:1606-1633` documents this inconsistency), so a fixed item
    /// list built by the BIF layer would be wrong. Handing the list back from the
    /// hook makes a fabricated item list impossible; the hook need not (and must
    /// not) include an item it cannot resolve through
    /// [`ProcessHook::process_info_item`].
    ///
    /// # Errors
    ///
    /// Reports a table failure as a [`BeamError`].
    fn process_info_keys(&mut self, pid: ProcessId) -> Result<Option<Vec<Atom>>>;

    /// `process_info/2` — one item's value for `pid`, built on `heap`.
    ///
    /// `item` is validated **first** (`pi_arg2ix`): an item this runtime does not
    /// report is [`ProcessInfoItem::Unknown`] — the oracle's `badarg`, which is
    /// decided before the process is inspected (see [`ProcessInfoItem`]). Only when
    /// `item` is known does the hook look the process up, answering
    /// [`ProcessInfoItem::Dead`] for a pid that names no live process (the oracle's
    /// `undefined`).
    ///
    /// # Errors
    ///
    /// Reports a table/rendering failure as a [`BeamError`].
    fn process_info_item(
        &mut self,
        heap: &mut Heap,
        atoms: &AtomTable,
        pid: ProcessId,
        item: Atom,
    ) -> Result<ProcessInfoItem>;

    /// `process_flag(Flag, Value)` — sets `flag` to `value` and returns the
    /// **previous** value (`process_flag_2`, `bif.c:2028`), or `Ok(None)` for the
    /// oracle's `badarg`.
    ///
    /// `Ok(None)` covers both failure classes the hook owns: a flag this runtime
    /// does not implement (the C's dispatch chain falls through to
    /// `process_flag_aux`, whose `am_badarg` becomes `BIF_ERROR(…, BADARG)`,
    /// `bif.c:2024,2233`) and an illegal `value` for a known flag (e.g.
    /// `trap_exit` with a non-boolean). The BIF layer has already checked that
    /// `flag` is an atom; the undocumented `{monitor_nodes, N}` tuple form is a
    /// distribution feature and stays the argument-only `badarg`.
    ///
    /// # Errors
    ///
    /// Reports a process-state failure as a [`BeamError`].
    fn process_flag(
        &mut self,
        heap: &mut Heap,
        atoms: &AtomTable,
        flag: Atom,
        value: Eterm,
    ) -> Result<Option<Eterm>>;

    /// `group_leader/0` — the calling process's group leader (`group_leader_0`,
    /// `bif.c:4903`, which returns `BIF_P->group_leader`).
    ///
    /// The oracle renders it as a pid term; the BIF layer does the rendering.
    ///
    /// # Errors
    ///
    /// Reports a table failure as a [`BeamError`].
    fn group_leader(&mut self) -> Result<ProcessId>;
}

/// The node's memory accounting for `erlang:memory/0,1`.
///
/// `memory/0` reports the runtime's memory usage as `{Key, Bytes}` pairs and
/// `memory/1` reports one or more named keys. The key set and the byte figures are
/// pure runtime accounting (allocator carriers, the atom table, the code area, ETS)
/// that no BIF can derive, so they are injected through this hook; the runtime
/// supplies the concrete implementation. With no hook installed — or with the
/// defaulted method — both BIFs report the typed `unsupported_bif` capability gap,
/// never an invented key set or a fabricated byte total (workspace `CONTEXT.md` §3).
pub trait MemoryHook {
    /// Every `{Key, Bytes}` pair the runtime reports, in the runtime's own order
    /// (the order of `erlang:memory/0`), or the typed `unsupported_bif` gap.
    ///
    /// # Errors
    ///
    /// Defaults to the typed `unsupported_bif` capability gap.
    fn memory(&mut self) -> Result<Vec<(&'static str, u64)>> {
        Err(crate::error::unsupported_bif())
    }
}

/// The runtime-coupled BIF escape hatch (the Stage-5 machine).
///
/// A large part of the `bif.tab` surface can only be answered by the *running*
/// runtime: process control (`spawn/3`, `link/1`, `monitor/2`, ...), the timer wheel
/// (`start_timer/3`, `send_after/3`, ...), code loading (`function_exported/3`,
/// `module_loaded/1`, ...), node/system introspection (`system_info/1`, `nodes/0`,
/// ...), the resource-object stores (`atomics:*`, `counters_*`), the ETS C-interior
/// entries, the `erts_debug`/`erl_debugger` tools, the `re`/PCRE engine, the
/// `erl_ddll` driver loader and the distribution machinery (`dist_ctrl_*`, `dt_*`,
/// ...). None of that is reachable from a BIF, so every such BIF resolves through
/// this single defaulted method; the runtime supplies the concrete dispatcher.
///
/// With no hook installed — or with the defaulted method — every BIF behind it
/// reports the typed `unsupported_bif` capability gap, never a fabricated value and
/// never a plain `undef` (the function *is* a declared BIF; only its runtime provider
/// is missing). The `(module, function, arity)` identity is passed through verbatim
/// (arity is `args.len()`) so the runtime dispatches on exactly the `bif.tab` key.
///
/// This is deliberately **one** method rather than one trait per subsystem: the
/// signature is identical for every runtime-coupled BIF and the adopter's natural
/// implementation is a single `(module, function, arity)` dispatch — the same shape
/// ERTS itself uses. Per-family traits would only duplicate this signature.
pub trait RuntimeHooks {
    /// Executes the runtime-coupled BIF `module:function/args.len()` with `args`,
    /// building its result on `heap` (with `atoms` for interning).
    ///
    /// # Errors
    ///
    /// Defaults to the typed `unsupported_bif` capability gap; a provider propagates
    /// the BIF's own failure (`badarg`, a raise, ...).
    fn call(
        &mut self,
        _heap: &mut Heap,
        _atoms: &AtomTable,
        _module: &str,
        _function: &str,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        Err(crate::error::unsupported_bif())
    }
}

/// What a BIF may read and write during one call.
pub struct BifContext<'a> {
    heap: &'a mut Heap,
    atoms: &'a AtomTable,
    self_pid: ProcessId,
    current_module: Option<&'a str>,
    output: Option<&'a mut dyn OutputSink>,
    sender: Option<&'a mut dyn SendHook>,
    tables: Option<&'a mut TableRegistry>,
    interner: Option<&'a mut dyn AtomInterner>,
    fun_caller: Option<&'a mut dyn FunCaller>,
    port: Option<&'a mut dyn PortHook>,
    dict: Option<&'a mut dyn DictHook>,
    persistent_term: Option<&'a mut dyn PersistentTerm>,
    nif_loader: Option<&'a mut dyn NifLoader>,
    record_source: Option<&'a mut dyn RecordSource>,
    local_node: Option<&'a mut dyn LocalNode>,
    fun_identity: Option<&'a mut dyn LocalFunIdentity>,
    module_info: Option<&'a mut dyn ModuleInfo>,
    process: Option<&'a mut dyn ProcessHook>,
    memory: Option<&'a mut dyn MemoryHook>,
    runtime: Option<&'a mut dyn RuntimeHooks>,
}

impl<'a> BifContext<'a> {
    /// A context over `heap`, `atoms` and the calling process `self_pid`, with no
    /// optional seams installed.
    pub fn new(heap: &'a mut Heap, atoms: &'a AtomTable, self_pid: ProcessId) -> Self {
        BifContext {
            heap,
            atoms,
            self_pid,
            current_module: None,
            output: None,
            sender: None,
            tables: None,
            interner: None,
            fun_caller: None,
            port: None,
            dict: None,
            persistent_term: None,
            nif_loader: None,
            record_source: None,
            local_node: None,
            fun_identity: None,
            module_info: None,
            process: None,
            memory: None,
            runtime: None,
        }
    }

    /// Installs the `io:format` output sink.
    pub fn with_output(mut self, sink: &'a mut dyn OutputSink) -> Self {
        self.output = Some(sink);
        self
    }

    /// Installs the `send/2` delivery hook.
    pub fn with_sender(mut self, hook: &'a mut dyn SendHook) -> Self {
        self.sender = Some(hook);
        self
    }

    /// Installs the `ets:*` table registry.
    pub fn with_tables(mut self, reg: &'a mut TableRegistry) -> Self {
        self.tables = Some(reg);
        self
    }

    /// Installs the atom-interning hook used by `list_to_atom`/`binary_to_atom`.
    pub fn with_atom_interner(mut self, interner: &'a mut dyn AtomInterner) -> Self {
        self.interner = Some(interner);
        self
    }

    /// Installs the fun-calling hook used by the `fun`-taking `lists`/`maps` BIFs.
    pub fn with_fun_caller(mut self, caller: &'a mut dyn FunCaller) -> Self {
        self.fun_caller = Some(caller);
        self
    }

    /// Installs the port hook used by `open_port/2` and the port BIFs.
    pub fn with_port_hook(mut self, hook: &'a mut dyn PortHook) -> Self {
        self.port = Some(hook);
        self
    }

    /// Installs the process-dictionary hook used by the `put/2`/`get/1`/`erase/1`
    /// /`get_keys/*` BIFs.
    pub fn with_dict_hook(mut self, hook: &'a mut dyn DictHook) -> Self {
        self.dict = Some(hook);
        self
    }

    /// Installs the node-global persistent-term store used by the
    /// `persistent_term:get/0,1,2`/`put/2`/`put_new/2`/`erase/1`/`info/0` BIFs.
    pub fn with_persistent_term(mut self, hook: &'a mut dyn PersistentTerm) -> Self {
        self.persistent_term = Some(hook);
        self
    }

    /// Installs the NIF-loading hook used by `erlang:load_nif/2`.
    pub fn with_nif_loader(mut self, loader: &'a mut dyn NifLoader) -> Self {
        self.nif_loader = Some(loader);
        self
    }

    /// Installs the record-source hook used by `records:get_definition/2`.
    pub fn with_record_source(mut self, source: &'a mut dyn RecordSource) -> Self {
        self.record_source = Some(source);
        self
    }

    /// Installs the local-node-identity hook used when encoding internal pid/port ids.
    pub fn with_local_node(mut self, node: &'a mut dyn LocalNode) -> Self {
        self.local_node = Some(node);
        self
    }

    /// Installs the local-fun-identity hook used when encoding a local
    /// (`make_fun3`) fun with `term_to_binary`.
    pub fn with_local_fun_identity(mut self, hook: &'a mut dyn LocalFunIdentity) -> Self {
        self.fun_identity = Some(hook);
        self
    }

    /// Installs the loaded-module-information hook used by
    /// `erlang:get_module_info/1,2`.
    pub fn with_module_info(mut self, source: &'a mut dyn ModuleInfo) -> Self {
        self.module_info = Some(source);
        self
    }

    /// Installs the process/name/global-state hook used by `erlang:whereis/1`,
    /// `register/2`, `unregister/1`, `registered/0`, `is_process_alive/1`,
    /// `processes/0` and `ports/0`.
    pub fn with_process_hook(mut self, hook: &'a mut dyn ProcessHook) -> Self {
        self.process = Some(hook);
        self
    }

    /// Installs the memory-accounting hook used by `erlang:memory/0,1`.
    pub fn with_memory_hook(mut self, hook: &'a mut dyn MemoryHook) -> Self {
        self.memory = Some(hook);
        self
    }

    /// Installs the runtime-coupled BIF dispatcher used by the Stage-5 BIF families
    /// (process control, timers, code loading, system/introspection,
    /// atomics/counters, the ETS interior, `erts_debug`/`erl_debugger`, `re`,
    /// `erl_ddll` and distribution).
    pub fn with_runtime_hooks(mut self, hooks: &'a mut dyn RuntimeHooks) -> Self {
        self.runtime = Some(hooks);
        self
    }

    /// Sets the name of the module whose code is executing — the call site of a BIF
    /// (the oracle derives `erlang:load_nif/2`'s module from its call instruction).
    pub fn with_current_module(mut self, module: &'a str) -> Self {
        self.current_module = Some(module);
        self
    }

    /// The destination heap.
    pub fn heap(&mut self) -> &mut Heap {
        &mut *self.heap
    }

    /// The atom table.
    pub fn atoms(&self) -> &AtomTable {
        self.atoms
    }

    /// The calling process's pid.
    pub fn self_pid(&self) -> ProcessId {
        self.self_pid
    }

    /// The name of the module whose code is executing, when the runtime knows it.
    ///
    /// The oracle derives `erlang:load_nif/2`'s owning module from the call site;
    /// with `None` the BIF reports `unsupported_bif` rather than guessing.
    pub fn current_module(&self) -> Option<&str> {
        self.current_module
    }

    /// Writes `s` to the configured sink, or to stdout when none is installed.
    ///
    /// # Errors
    ///
    /// Propagates the sink's failure.
    pub fn write_output(&mut self, s: &str) -> Result<()> {
        match self.output.as_deref_mut() {
            Some(sink) => sink.write_str(s),
            None => {
                let mut sink = StdoutSink;
                sink.write_str(s)
            }
        }
    }

    /// The installed delivery hook, if any.
    ///
    /// The trait-object lifetime is spelled out (`dyn SendHook + 'a`) because a
    /// `&mut` reference is invariant over its referent: the hook handed back must
    /// stay valid for the context's own lifetime, not merely the accessor's borrow.
    pub fn sender(&mut self) -> Option<&mut (dyn SendHook + 'a)> {
        self.sender.as_deref_mut()
    }

    /// The installed NIF-loading hook, if any.
    ///
    /// The trait-object lifetime is spelled out (`dyn NifLoader + 'a`) for the same
    /// invariance reason as [`BifContext::sender`].
    pub fn nif_loader(&mut self) -> Option<&mut (dyn NifLoader + 'a)> {
        self.nif_loader.as_deref_mut()
    }

    /// The `Recs`-chunk definition named `name` in the loaded module `module`.
    ///
    /// This is the seam behind `records:get_definition/2`: the runtime's code server
    /// resolves the definition (name-resolved) from its loaded modules.
    ///
    /// # Errors
    ///
    /// Propagates the hook's own failure. Returns `Ok(None)` when no
    /// [`RecordSource`] hook is installed.
    pub fn record_definition(
        &mut self,
        module: &str,
        name: &str,
    ) -> Result<Option<RecordDefinitionView>> {
        match self.record_source.as_deref_mut() {
            Some(source) => source.record_definition(module, name),
            None => Ok(None),
        }
    }

    /// The local node identity `(node_atom, creation)`.
    ///
    /// With a [`LocalNode`] hook installed the hook answers. With none, this
    /// falls back to the M1 runtime's fixed, non-distributed identity — the
    /// `nonode@nohost` atom (exactly what `crate::convert`'s `node/0` answers)
    /// with creation 0 — when that atom is interned in this context's atom
    /// table, and `None` otherwise (internal ids then stay unencodable).
    pub fn local_node(&mut self) -> Option<(Atom, u32)> {
        if let Some(hook) = self.local_node.as_deref_mut() {
            return hook.local_node();
        }
        self.atoms().lookup("nonode@nohost").map(|node| (node, 0))
    }

    /// The `NEW_FUN_EXT` identity of the local (`make_fun3`) fun `term`.
    ///
    /// With a [`LocalFunIdentity`] hook installed the hook answers; with none (or
    /// when the hook cannot resolve the identity) this is `Ok(None)`, which leaves
    /// the fun unencodable (`unsupported_local_fun`) rather than inventing the wire
    /// fields.
    ///
    /// # Errors
    ///
    /// Propagates the hook's own failure.
    pub fn local_fun_identity(&mut self, term: Eterm) -> Result<Option<beam_codec::FunIdentity>> {
        match self.fun_identity.as_deref_mut() {
            Some(hook) => hook.local_fun_identity(term),
            None => Ok(None),
        }
    }

    /// The loaded module `module`'s introspection item `item`, built on the
    /// context heap — the seam behind `erlang:get_module_info/1,2`.
    ///
    /// `Ok(None)` means the module or the item is unknown (the BIF renders the
    /// oracle's `badarg`).
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no
    /// [`ModuleInfo`] hook; otherwise propagates the hook's own failure.
    pub fn module_info(&mut self, module: &str, item: &str) -> Result<Option<Eterm>> {
        match self.module_info.as_deref_mut() {
            Some(hook) => hook.module_info(self.heap, self.atoms, module, item),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// The id registered under `name` through the installed [`ProcessHook`], or
    /// `None` when the name is unregistered (the BIF renders `undefined`) — the
    /// seam behind `erlang:whereis/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn proc_whereis(&mut self, name: Atom) -> Result<Option<Eterm>> {
        match self.process.as_deref_mut() {
            Some(hook) => hook.whereis(name),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Registers `id` under `name` through the installed [`ProcessHook`],
    /// returning whether the registration succeeded (`false` is the oracle's
    /// `badarg`) — the seam behind `erlang:register/2`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn proc_register(&mut self, name: Atom, id: Eterm) -> Result<bool> {
        match self.process.as_deref_mut() {
            Some(hook) => hook.register(name, id),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Removes `name` through the installed [`ProcessHook`], returning whether it
    /// was registered (`false` is the oracle's `badarg`) — the seam behind
    /// `erlang:unregister/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn proc_unregister(&mut self, name: Atom) -> Result<bool> {
        match self.process.as_deref_mut() {
            Some(hook) => hook.unregister(name),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Every registered name through the installed [`ProcessHook`], in the hook's
    /// order — the seam behind `erlang:registered/0`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn proc_registered(&mut self) -> Result<Vec<Atom>> {
        match self.process.as_deref_mut() {
            Some(hook) => hook.registered(),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Whether `pid` names a live process, through the installed [`ProcessHook`]
    /// — the seam behind `erlang:is_process_alive/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    /// Whether a [`ProcessHook`] is installed.
    ///
    /// The liveness/registry seam. `ets:give_away/3` and `ets:setopts/2` must ask
    /// whether a pid is *alive* (the C's `erts_pid2proc`/`erts_proc_lookup` in
    /// `erl_db.c`), and with no hook that question cannot be answered at all — so
    /// a caller can tell "the hook answered `false`" from "nothing can answer".
    pub fn has_process_hook(&self) -> bool {
        self.process.is_some()
    }

    pub fn proc_is_alive(&mut self, pid: ProcessId) -> Result<bool> {
        match self.process.as_deref_mut() {
            Some(hook) => hook.is_process_alive(pid),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Every local process through the installed [`ProcessHook`], in the hook's
    /// order — the seam behind `erlang:processes/0`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn proc_processes(&mut self) -> Result<Vec<ProcessId>> {
        match self.process.as_deref_mut() {
            Some(hook) => hook.processes(),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Every local port through the installed [`ProcessHook`], in the hook's order
    /// — the seam behind `erlang:ports/0`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn proc_ports(&mut self) -> Result<Vec<PortId>> {
        match self.process.as_deref_mut() {
            Some(hook) => hook.ports(),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// The item names `erlang:process_info/1` reports for `pid`, in the hook's
    /// order, or `None` when `pid` names no live process (the BIF renders
    /// `undefined`) — the seam behind `erlang:process_info/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn proc_process_info_keys(&mut self, pid: ProcessId) -> Result<Option<Vec<Atom>>> {
        match self.process.as_deref_mut() {
            Some(hook) => hook.process_info_keys(pid),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// One `erlang:process_info/2` item's outcome for `pid` — the seam behind
    /// `erlang:process_info/2`.
    ///
    /// The item is validated before the process is inspected, so an unknown item is
    /// [`ProcessInfoItem::Unknown`] even for a dead pid (see that type).
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn proc_process_info_item(&mut self, pid: ProcessId, item: Atom) -> Result<ProcessInfoItem> {
        match self.process.as_deref_mut() {
            Some(hook) => hook.process_info_item(self.heap, self.atoms, pid, item),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Sets the process flag `flag` to `value` through the installed
    /// [`ProcessHook`], returning the **previous** value, or `None` for the
    /// oracle's `badarg` (an unknown flag or an illegal value) — the seam behind
    /// `erlang:process_flag/2`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn proc_process_flag(&mut self, flag: Atom, value: Eterm) -> Result<Option<Eterm>> {
        match self.process.as_deref_mut() {
            Some(hook) => hook.process_flag(self.heap, self.atoms, flag, value),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// The calling process's group leader through the installed [`ProcessHook`] —
    /// the seam behind `erlang:group_leader/0`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn proc_group_leader(&mut self) -> Result<ProcessId> {
        match self.process.as_deref_mut() {
            Some(hook) => hook.group_leader(),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// The installed table registry, if any.
    pub fn tables(&mut self) -> Option<&mut TableRegistry> {
        self.tables.as_deref_mut()
    }

    /// Calls `fun` with `args` through the installed [`FunCaller`].
    ///
    /// This is the seam the `fun`-taking `lists`/`maps` BIFs use to execute an
    /// Erlang fun (`lists:map/2`, `lists:foldl/3`, `maps:map/2`, …).
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no caller;
    /// otherwise propagates the callee's own failure.
    pub fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        match self.fun_caller.as_deref_mut() {
            Some(caller) => caller.call_fun(fun, args),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Creates a port through the installed [`PortHook`] — the seam behind
    /// `erlang:open_port/2`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure (`badarg` for an unusable Name/Options).
    pub fn open_port(&mut self, name: Eterm, options: Eterm) -> Result<PortId> {
        match self.port.as_deref_mut() {
            Some(hook) => hook.open_port(name, options),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Closes `port` through the installed [`PortHook`] — the seam behind
    /// `erlang:port_close/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn close_port(&mut self, port: PortId) -> Result<()> {
        match self.port.as_deref_mut() {
            Some(hook) => hook.close_port(port),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// The `{Item, Value}` pairs of `port` through the installed [`PortHook`] —
    /// the seam behind `erlang:port_info/1,2`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn port_info(&mut self, port: PortId) -> Result<Vec<(Eterm, Eterm)>> {
        match self.port.as_deref_mut() {
            Some(hook) => hook.port_info(port),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Stores `value` under `key` in the process dictionary through the installed
    /// [`DictHook`], returning the **replaced** value (`None` when the key was
    /// absent) — the seam behind `erlang:put/2`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn dict_put(&mut self, key: Eterm, value: Eterm) -> Result<Option<Eterm>> {
        match self.dict.as_deref_mut() {
            Some(hook) => hook.put(key, value),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// The value stored under `key` in the process dictionary through the
    /// installed [`DictHook`], or `None` when the key is absent — the seam behind
    /// `erlang:get/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn dict_get(&mut self, key: Eterm) -> Result<Option<Eterm>> {
        match self.dict.as_deref_mut() {
            Some(hook) => hook.get(key),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Removes `key` from the process dictionary through the installed
    /// [`DictHook`], returning the value that was stored under it (`None` when the
    /// key was absent) — the seam behind `erlang:erase/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn dict_erase(&mut self, key: Eterm) -> Result<Option<Eterm>> {
        match self.dict.as_deref_mut() {
            Some(hook) => hook.erase(key),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Every key of the process dictionary through the installed [`DictHook`] —
    /// the seam behind `erlang:get_keys/0`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn dict_get_keys(&mut self) -> Result<Vec<Eterm>> {
        match self.dict.as_deref_mut() {
            Some(hook) => hook.get_keys(),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Every key whose value raw-equals `value` through the installed
    /// [`DictHook`] — the seam behind `erlang:get_keys/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn dict_get_keys_for_value(&mut self, value: Eterm) -> Result<Vec<Eterm>> {
        match self.dict.as_deref_mut() {
            Some(hook) => hook.get_keys_for_value(value),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Every `{Key, Value}` pair of the process dictionary through the installed
    /// [`DictHook`] — the seam behind `erlang:get/0`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook (or its
    /// hook leaves [`DictHook::get_all`] at its default); otherwise propagates
    /// the hook's own failure.
    pub fn dict_get_all(&mut self) -> Result<Vec<(Eterm, Eterm)>> {
        match self.dict.as_deref_mut() {
            Some(hook) => hook.get_all(),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Empties the process dictionary through the installed [`DictHook`],
    /// returning every `{Key, Value}` pair it held — the seam behind
    /// `erlang:erase/0`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook (or its
    /// hook leaves [`DictHook::erase_all`] at its default); otherwise propagates
    /// the hook's own failure.
    pub fn dict_erase_all(&mut self) -> Result<Vec<(Eterm, Eterm)>> {
        match self.dict.as_deref_mut() {
            Some(hook) => hook.erase_all(),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// The value stored under `key` in the node-global persistent-term store
    /// through the installed [`PersistentTerm`] hook, or `None` when the key is
    /// absent — the seam behind `persistent_term:get/1,2`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn persistent_term_get(&mut self, key: Eterm) -> Result<Option<Eterm>> {
        match self.persistent_term.as_deref_mut() {
            Some(hook) => hook.get(key),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Stores `value` under `key` in the node-global persistent-term store through
    /// the installed [`PersistentTerm`] hook, returning whether the store accepted
    /// the update (`false` is the oracle's `badarg` for a `put_new` collision) —
    /// the seam behind `persistent_term:put/2` and `put_new/2`.
    ///
    /// `only_if_new` is `false` for `put/2` and `true` for `put_new/2`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn persistent_term_put(
        &mut self,
        key: Eterm,
        value: Eterm,
        only_if_new: bool,
    ) -> Result<bool> {
        match self.persistent_term.as_deref_mut() {
            Some(hook) => hook.put(key, value, only_if_new),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Erases `key` from the node-global persistent-term store through the
    /// installed [`PersistentTerm`] hook, returning whether it was present — the
    /// seam behind `persistent_term:erase/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn persistent_term_erase(&mut self, key: Eterm) -> Result<bool> {
        match self.persistent_term.as_deref_mut() {
            Some(hook) => hook.erase(key),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Every `{Key, Value}` pair of the node-global persistent-term store through
    /// the installed [`PersistentTerm`] hook, in the hook's order — the seam behind
    /// `persistent_term:get/0`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn persistent_term_get_all(&mut self) -> Result<Vec<(Eterm, Eterm)>> {
        match self.persistent_term.as_deref_mut() {
            Some(hook) => hook.get_all(),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// The node-global persistent-term store's `(count, memory)` bookkeeping
    /// through the installed [`PersistentTerm`] hook — the seam behind
    /// `persistent_term:info/0`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn persistent_term_info(&mut self) -> Result<(u64, u64)> {
        match self.persistent_term.as_deref_mut() {
            Some(hook) => hook.info(),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// The runtime's memory accounting, in the runtime's own key order — the seam
    /// behind `erlang:memory/0,1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no [`MemoryHook`];
    /// otherwise propagates the hook's own failure.
    pub fn memory(&mut self) -> Result<Vec<(&'static str, u64)>> {
        match self.memory.as_deref_mut() {
            Some(hook) => hook.memory(),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Executes the runtime-coupled BIF `module:function/args.len()` through the
    /// installed [`RuntimeHooks`] dispatcher.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no [`RuntimeHooks`];
    /// otherwise propagates the dispatcher's own failure.
    pub fn call_runtime_bif(
        &mut self,
        module: &str,
        function: &str,
        args: &[Eterm],
    ) -> Result<Eterm> {
        match self.runtime.as_deref_mut() {
            Some(hooks) => hooks.call(self.heap, self.atoms, module, function, args),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// A 60-bit small integer.
    ///
    /// # Errors
    ///
    /// [`BeamError::SystemLimit`] when `v` does not fit a small integer (the
    /// rewrite has no `f64`-free bignum arithmetic in Milestone-1; real Erlang
    /// promotes to a bignum).
    pub fn int(&self, v: i64) -> Result<Eterm> {
        try_make_small(v).map_err(|_| BeamError::SystemLimit)
    }

    /// The atom `name`.
    ///
    /// # Errors
    ///
    /// [`BeamError::TermNotFound`] when `name` is not interned in the atom table.
    pub fn atom(&self, name: &str) -> Result<Eterm> {
        self.atoms
            .lookup(name)
            .map(make_atom)
            .ok_or(BeamError::TermNotFound)
    }

    /// The atom `name`, interning it through the [`AtomInterner`] hook when it is
    /// not already present.
    ///
    /// An already-interned name never needs the hook; a new name requires it.
    ///
    /// # Errors
    ///
    /// [`BeamError::TermNotFound`] when `name` is unknown and no interner is
    /// installed (the runtime has not wired one up).
    pub fn intern_atom(&mut self, name: &str) -> Result<Eterm> {
        if let Some(atom) = self.atoms.lookup(name) {
            return Ok(make_atom(atom));
        }
        match self.interner.as_deref_mut() {
            Some(interner) => Ok(make_atom(interner.intern(name))),
            None => Err(BeamError::TermNotFound),
        }
    }

    /// The atom `true` or `false`.
    pub fn boolean(&self, b: bool) -> Result<Eterm> {
        self.atom(if b { "true" } else { "false" })
    }

    /// The atom `ok`.
    pub fn ok(&self) -> Result<Eterm> {
        self.atom("ok")
    }

    /// A boxed float built on the context heap.
    ///
    /// # Errors
    ///
    /// Propagates the heap's allocation failure.
    pub fn make_float(&mut self, v: f64) -> Result<Eterm> {
        access::make_float(self.heap, v)
    }

    /// A tuple of `elems` built on the context heap.
    ///
    /// # Errors
    ///
    /// Propagates the heap's allocation failure.
    pub fn tuple(&mut self, elems: &[Eterm]) -> Result<Eterm> {
        self.heap.tuple(elems)
    }

    /// A cons cell `[car | cdr]` built on the context heap.
    ///
    /// # Errors
    ///
    /// Propagates the heap's allocation failure.
    pub fn cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm> {
        self.heap.cons(car, cdr)
    }

    /// A proper list of `elems` built on the context heap (`NIL` when empty).
    ///
    /// # Errors
    ///
    /// Propagates the heap's allocation failure.
    pub fn list(&mut self, elems: &[Eterm]) -> Result<Eterm> {
        let mut acc = NIL;
        for &elem in elems.iter().rev() {
            acc = self.heap.cons(elem, acc)?;
        }
        Ok(acc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_term::{atom_val, is_list, make_small};

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok"]);
        atoms
    }

    #[test]
    fn writes_to_the_configured_sink() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut sink = BytesSink::new();
        {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
            ctx.write_output("hello ").expect("captured");
            ctx.write_output("world").expect("captured");
        }
        assert_eq!(sink.contents(), b"hello world");
        assert_eq!(sink.as_str(), "hello world");
        sink.clear();
        assert!(sink.contents().is_empty());
    }

    #[test]
    fn null_sink_and_default_stdout_sink_discard_and_write() {
        let mut sink = NullSink;
        assert!(sink.write_str("dropped").is_ok());
        // `StdoutSink` writes to the process stdout; a fresh handle is enough to
        // exercise the code path without asserting on the terminal.
        let mut stdout = StdoutSink;
        assert!(stdout.write_str("").is_ok());
    }

    #[test]
    fn identity_and_atoms_accessors() {
        let mut heap = Heap::new(8).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(7, 3));
        assert_eq!(ctx.self_pid(), ProcessId::new(7, 3));
        assert!(ctx.atoms().lookup("ok").is_some());
        assert!(ctx.heap().available() > 0);
    }

    #[test]
    fn writers_build_the_expected_terms() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        assert_eq!(ctx.int(7).expect("a small int"), make_small(7));
        let ok = ctx.ok().expect("ok is seeded");
        assert_eq!(ctx.atoms().name(atom_val(ok)), Some("ok"));
        assert_eq!(
            ctx.atoms()
                .name(atom_val(ctx.boolean(true).expect("seeded"))),
            Some("true")
        );
        assert_eq!(
            ctx.atoms()
                .name(atom_val(ctx.boolean(false).expect("seeded"))),
            Some("false")
        );

        assert!(matches!(ctx.atom("nope"), Err(BeamError::TermNotFound)));
        assert!(matches!(
            ctx.int(beam_term::MAX_SMALL + 1),
            Err(BeamError::SystemLimit)
        ));
    }

    #[test]
    fn structural_writers_build_terms() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let tuple = ctx.tuple(&[make_small(1), make_small(2)]).expect("fits");
        assert!(crate::access::is_tuple(tuple));

        let list = ctx.list(&[make_small(1), make_small(2)]).expect("fits");
        assert!(is_list(list));
        assert_eq!(ctx.list(&[]).expect("empty is NIL"), NIL);
        assert!(is_list(ctx.cons(make_small(1), NIL).expect("fits")));

        let float = ctx.make_float(1.5).expect("fits");
        assert!(crate::access::is_float(float));
        assert_eq!(crate::access::float_value(float), 1.5);
    }

    /// A minimal `FunCaller` test double: it recognises a *sentinel* fun term (the
    /// small integer `1`) and answers with `Args ++ Marker` on its own heap.
    ///
    /// Milestone-1 cannot build a real `FUN_SUBTAG` term from safe Rust without an
    /// evaluator, so the tests identify "the fun" structurally — exactly what a
    /// `beam_rt` implementation does with a real closure reference.
    struct RecordingCaller {
        heap: Heap,
        calls: Vec<Vec<Eterm>>,
    }

    impl RecordingCaller {
        fn new() -> RecordingCaller {
            RecordingCaller {
                heap: Heap::new(64).expect("test heap"),
                calls: Vec::new(),
            }
        }
    }

    impl FunCaller for RecordingCaller {
        fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
            assert_eq!(fun, make_small(1), "only the sentinel fun is recognised");
            self.calls.push(args.to_vec());
            self.heap.tuple(args)
        }
    }

    #[test]
    fn call_fun_without_a_caller_is_a_typed_unsupported() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        match ctx.call_fun(make_small(1), &[make_small(2)]) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected an unsupported_bif raise, got {other:?}"),
        }
    }

    #[test]
    fn call_fun_with_a_caller_delegates_with_the_arguments() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let fun = make_small(1);
        let mut caller = RecordingCaller::new();
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_fun_caller(&mut caller);
            let result = ctx
                .call_fun(fun, &[make_small(7), make_small(8)])
                .expect("the caller answers");
            assert!(crate::access::is_tuple(result));
            assert_eq!(crate::access::tuple_size(result), 2);
            assert_eq!(crate::access::tuple_element(result, 1), make_small(8));
        }
        assert_eq!(caller.calls, vec![vec![make_small(7), make_small(8)]]);
    }

    /// A minimal `PortHook` test double: it allocates `PortId::new(11)` for every
    /// open and answers a fixed pair list from `port_info`.
    struct StubPorts;

    impl PortHook for StubPorts {
        fn open_port(&mut self, _name: Eterm, _options: Eterm) -> Result<PortId> {
            Ok(PortId::new(11))
        }

        fn close_port(&mut self, _port: PortId) -> Result<()> {
            Ok(())
        }

        fn port_info(&mut self, _port: PortId) -> Result<Vec<(Eterm, Eterm)>> {
            Ok(vec![(make_small(1), make_small(2))])
        }
    }

    #[test]
    fn port_bifs_without_a_hook_are_a_typed_unsupported() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let port = PortId::new(3);

        for result in [
            ctx.open_port(make_small(1), NIL).map(|_| ()),
            ctx.close_port(port),
            ctx.port_info(port).map(|_| ()),
        ] {
            match result {
                Err(BeamError::Raise(raise)) => {
                    assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                    assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
                }
                other => panic!("expected an unsupported_bif raise, got {other:?}"),
            }
        }
    }

    #[test]
    fn port_bifs_with_a_hook_delegate() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut hook = StubPorts;
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_port_hook(&mut hook);

        assert_eq!(ctx.open_port(make_small(1), NIL).expect("the hook answers"), PortId::new(11));
        ctx.close_port(PortId::new(11)).expect("the hook closes");
        assert_eq!(
            ctx.port_info(PortId::new(11)).expect("the hook answers"),
            vec![(make_small(1), make_small(2))]
        );
    }

    /// A minimal `DictHook` test double over an in-memory pair list: it stores
    /// `(Key, Value)` pairs by raw `Eterm` equality (the `beam_process::Dict`
    /// contract).
    #[derive(Default)]
    struct StubDict {
        entries: Vec<(Eterm, Eterm)>,
    }

    impl DictHook for StubDict {
        fn put(&mut self, key: Eterm, value: Eterm) -> Result<Option<Eterm>> {
            match self.entries.iter().position(|&(stored, _)| stored == key) {
                Some(index) => Ok(Some(core::mem::replace(&mut self.entries[index].1, value))),
                None => {
                    self.entries.push((key, value));
                    Ok(None)
                }
            }
        }

        fn get(&mut self, key: Eterm) -> Result<Option<Eterm>> {
            Ok(self
                .entries
                .iter()
                .find(|&&(stored, _)| stored == key)
                .map(|&(_, value)| value))
        }

        fn erase(&mut self, key: Eterm) -> Result<Option<Eterm>> {
            match self.entries.iter().position(|&(stored, _)| stored == key) {
                Some(index) => Ok(Some(self.entries.remove(index).1)),
                None => Ok(None),
            }
        }

        fn get_keys(&mut self) -> Result<Vec<Eterm>> {
            Ok(self.entries.iter().map(|&(key, _)| key).collect())
        }

        fn get_keys_for_value(&mut self, value: Eterm) -> Result<Vec<Eterm>> {
            Ok(self
                .entries
                .iter()
                .filter(|&&(_, stored)| stored == value)
                .map(|&(key, _)| key)
                .collect())
        }
    }

    #[test]
    fn dict_accessors_without_a_hook_are_a_typed_unsupported() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        for result in [
            ctx.dict_put(make_small(1), make_small(2)).map(|_| ()),
            ctx.dict_get(make_small(1)).map(|_| ()),
            ctx.dict_erase(make_small(1)).map(|_| ()),
            ctx.dict_get_keys().map(|_| ()),
            ctx.dict_get_keys_for_value(make_small(2)).map(|_| ()),
        ] {
            match result {
                Err(BeamError::Raise(raise)) => {
                    assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                    assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
                }
                other => panic!("expected an unsupported_bif raise, got {other:?}"),
            }
        }
    }

    #[test]
    fn dict_accessors_with_a_hook_delegate() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut hook = StubDict::default();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);

        assert_eq!(ctx.dict_put(make_small(1), make_small(10)).expect("the hook answers"), None);
        assert_eq!(
            ctx.dict_put(make_small(1), make_small(11)).expect("the hook answers"),
            Some(make_small(10))
        );
        assert_eq!(ctx.dict_get(make_small(1)).expect("the hook answers"), Some(make_small(11)));
        assert_eq!(ctx.dict_get(make_small(2)).expect("the hook answers"), None);
        assert_eq!(ctx.dict_get_keys().expect("the hook answers"), vec![make_small(1)]);

        ctx.dict_put(make_small(2), make_small(11)).expect("the hook stores");
        assert_eq!(
            ctx.dict_get_keys_for_value(make_small(11)).expect("the hook answers"),
            vec![make_small(1), make_small(2)]
        );
        assert_eq!(ctx.dict_erase(make_small(1)).expect("the hook answers"), Some(make_small(11)));
        assert_eq!(ctx.dict_erase(make_small(1)).expect("the hook answers"), None);
    }

    /// A minimal `RecordSource` test double: it answers one fixed definition for a
    /// recognised `(module, name)` and `Ok(None)` for anything else.
    struct StubRecords;

    impl RecordSource for StubRecords {
        fn record_definition(
            &mut self,
            module: &str,
            name: &str,
        ) -> Result<Option<RecordDefinitionView>> {
            if module == "recs" && name == "person" {
                Ok(Some(RecordDefinitionView {
                    is_exported: true,
                    fields: vec![
                        RecordFieldView { name: "name".to_string(), default: Some(make_small(1)) },
                        RecordFieldView { name: "age".to_string(), default: None },
                    ],
                }))
            } else {
                Ok(None)
            }
        }
    }

    #[test]
    fn record_definition_without_a_hook_is_none() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        assert_eq!(ctx.record_definition("recs", "person").expect("no hook is Ok"), None);
    }

    #[test]
    fn record_definition_with_a_hook_delegates() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut source = StubRecords;
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_record_source(&mut source);

        let definition = ctx
            .record_definition("recs", "person")
            .expect("the hook answers")
            .expect("the definition exists");
        assert!(definition.is_exported);
        assert_eq!(
            definition.fields,
            vec![
                RecordFieldView { name: "name".to_string(), default: Some(make_small(1)) },
                RecordFieldView { name: "age".to_string(), default: None },
            ]
        );
        assert_eq!(ctx.record_definition("recs", "unknown").expect("the hook answers"), None);
    }

    #[test]
    fn local_node_without_a_hook_falls_back_to_nonode_at_nohost_when_seeded() {
        let mut heap = Heap::new(16).expect("test heap");
        let mut atoms = seeded_atoms();
        atoms.seed(["nonode@nohost"]);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let expected = atoms.lookup("nonode@nohost").expect("seeded");
        assert_eq!(ctx.local_node(), Some((expected, 0)));
    }

    #[test]
    fn local_node_without_a_hook_is_none_when_the_atom_is_absent() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        assert_eq!(ctx.local_node(), None);
    }

    /// A minimal `LocalNode` test double answering a fixed identity (the node atom
    /// `ok` and creation `7`).
    struct StubNode {
        node: Atom,
    }

    impl LocalNode for StubNode {
        fn local_node(&mut self) -> Option<(Atom, u32)> {
            Some((self.node, 7))
        }
    }

    #[test]
    fn local_node_with_a_hook_delegates() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let node = atoms.lookup("ok").expect("seeded");
        let mut hook = StubNode { node };
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_local_node(&mut hook);

        assert_eq!(ctx.local_node(), Some((node, 7)));
    }

    /// A minimal `LocalFunIdentity` test double answering a fixed identity.
    struct StubFunIdentity {
        identity: beam_codec::FunIdentity,
    }

    impl LocalFunIdentity for StubFunIdentity {
        fn local_fun_identity(&mut self, _term: Eterm) -> Result<Option<beam_codec::FunIdentity>> {
            Ok(Some(self.identity))
        }
    }

    #[test]
    fn local_fun_identity_without_a_hook_is_none() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        assert_eq!(ctx.local_fun_identity(NIL).expect("no hook is Ok"), None);
    }

    #[test]
    fn local_fun_identity_with_a_hook_delegates() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let module = atoms.lookup("ok").expect("seeded");
        let creator = beam_codec::FunPid {
            node: module,
            number: 10,
            serial: 0,
            creation: 0,
        };
        let expected = beam_codec::FunIdentity::new(module, [7u8; 16], 3, 9, creator);
        let mut hook = StubFunIdentity { identity: expected };
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_local_fun_identity(&mut hook);

        assert_eq!(
            ctx.local_fun_identity(NIL).expect("the hook answers"),
            Some(expected)
        );
    }
}
