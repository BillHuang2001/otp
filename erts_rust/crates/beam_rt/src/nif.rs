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

//! The NIF subsystem: the runtime's **live NIF libraries** and the host they run against.
//!
//! This is the runtime half of contract **A.3** (the NIF C ABI). `beam_abi` freezes the ABI
//! *shape* and owns the `dlopen` loader ([`beam_abi::nif::loader`]); this module owns the
//! **state**: which libraries a `beam_rt` process has loaded, the [`NifHost`] the `enif_*`
//! functions draw their runtime services from, and the call path that lets interpreted Erlang
//! reach a C NIF.
//!
//! # Two host handles, one library
//!
//! [`beam_abi::nif::loader::load`] **erases the borrow** of the [`NifHost`] it is given into
//! a raw `*mut dyn NifHost`, because the library's `unload` callback (run from
//! [`beam_abi::nif::loader::NifLibrary`]'s `Drop`) needs a host again and cannot name the
//! borrow's lifetime. The host passed to `load` must therefore **outlive the library**.
//!
//! [`NifEntry`] is that guarantee: it stores the [`NifLibrary`] **and** the boxed
//! [`NifHostImpl`] it was loaded against, and — because Rust drops struct fields in
//! declaration order — the library (and hence `unload`) drops *before* the host. A NIF
//! **call**, by contrast, runs against a **fresh** per-call host ([`NifHostImpl`] built by
//! [`RuntimeContext::try_call_nif`](crate::context::RuntimeContext)) that owns the per-call
//! heap and the per-call [`PendingInterner`](crate::context::PendingInterner). Keeping the
//! two apart is what lets the unload host be long-lived while the call host is cheap and
//! correctly rooted at the current atom-table length.
//!
//! # Deferred atom interning
//!
//! A NIF interns atoms through [`NifHost::intern_atom`] (an `enif_make_atom`), which must add
//! names to the runtime's **one** shared [`AtomTable`] — a table the BIF layer borrows
//! immutably. The host therefore uses the same deferred-commit
//! [`PendingInterner`](crate::context::PendingInterner) the BIF layer uses, and it shares the
//! **same** interner as the BIF call that triggered it: a `load` callback and the
//! `erlang:load_nif/2` BIF itself may interleave new-atom creations, and two independent
//! predicted-index lists would corrupt each other. One shared list, committed once by the
//! caller, keeps every predicted index exact.
//!
//! # The host's runtime services
//!
//! [`NifHostImpl`] serves a NIF call the runtime state it can reach: the atom services
//! (`intern_atom`/`atom_name`/`existing_atom`), the **ETF codec**
//! (`term_to_binary`/`binary_to_term`, over the same `beam_codec` encoder/decoder and the same
//! deferred-interner discipline the `erlang:term_to_binary/1` BIF uses), a **unique-integer
//! source** (`unique_integer`, mirroring the C emulator's four counter sequences), the
//! emulator **system-information** block (`system_info`, whose `scheduler_threads` is the
//! runtime's real normal-scheduler count) and the **reference/registry** hooks
//! (`make_ref`/`whereis_pid`, over the borrowed [`NifHostServices`] below). It also implements the
//! whole extra-environment family — `alloc_env`/`free_env` (a live message environment whose atoms
//! are the runtime's own, see [`NifHost::alloc_env`]) and `make_copy` (the message-env idiom's
//! term movement, see [`NifHost::make_copy`]).
//!
//! # Borrowed runtime services
//!
//! Two hooks need runtime state the host does not own: a fresh reference (`enif_make_ref`) and a
//! name lookup (`enif_whereis_pid`). The host reaches both through an optional [`NifHostServices`]
//! provider threaded in at construction. The **per-call** host (built by
//! [`RuntimeContext::try_call_nif`](crate::context::RuntimeContext)) carries a
//! [`RuntimeNifServices`] over the runtime's reference-id allocator, its reference registry and its
//! process table, so both hooks are live there. The **load/unload** host (built by
//! [`NifBridge::load_nif`](crate::context::bridges::NifBridge)) is *moved into* the runtime's NIF
//! registry, so it cannot borrow the runtime and is built with `services = None`: both hooks then
//! stay the trait default (the typed `None`), so a `load`/`unload` callback can never fabricate a
//! reference or a pid. `erlang:load_nif/2` on an already-loaded module is a `reload` (the oracle's
//! rule since OTP 20).

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicI64, Ordering};

use beam_abi::nif::loader::{NifCallError, NifLibrary};
use beam_abi::nif::types::ErlNifEnv;
use beam_abi::{ErlDrvSysInfo, NifEnv, NifHost};
use beam_alloc::Heap;
use beam_bifs::AtomInterner;
use beam_process::{MonitorRef, ProcessTable, RefAllocator};
use beam_term::prelude::{atom_val, is_atom, is_header, make_atom, make_small, Eterm};
use beam_term::AtomTable;
use libc::c_char;

use crate::context::PendingInterner;

/// One loaded NIF library plus the host it was loaded against.
///
/// The field order is load-bearing: `lib` is declared **first** so it is dropped first, and
/// its `Drop` (which runs the library's `unload` callback against the erased host pointer)
/// therefore still sees a live `host`.
pub(crate) struct NifEntry {
    /// The `dlopen`ed library (its `Drop` runs `unload` + `dlclose`).
    lib: NifLibrary,
    /// The host the library was loaded against — kept alive for the library's lifetime.
    ///
    /// Read only indirectly (through the erased pointer inside `lib`), which is why the field
    /// carries an explicit `allow`: it is the *ownership* that matters, not any Rust access.
    #[allow(dead_code)]
    host: Box<NifHostImpl<'static>>,
}

impl NifEntry {
    /// A library entry pairing `lib` with the `host` it was loaded against.
    pub(crate) fn new(lib: NifLibrary, host: Box<NifHostImpl<'static>>) -> NifEntry {
        NifEntry { lib, host }
    }

    /// The `{name, arity}` pairs the library exports.
    fn funcs(&self) -> &[(String, u32)] {
        self.lib.funcs()
    }
}

/// The runtime's live NIF libraries, keyed by the Erlang module each implements.
///
/// **Not `Send`**: a [`NifLibrary`] owns raw `dlopen` handles and an erased host pointer, so a
/// registry is confined to the thread that loaded it — which is fine, because the whole
/// runtime is single-threaded (`beam_rt` has no `unsafe` and no cross-thread sharing).
pub(crate) struct NifRegistry {
    /// The loaded libraries, by module name.
    libs: HashMap<String, NifEntry>,
}

impl NifRegistry {
    /// An empty registry.
    ///
    /// The [`enif_api_table`](beam_abi::nif::enif::enif_api_table) reference is **load-bearing**:
    /// it drags the `enif_*`-carrying codegen unit into the link so the symbols a loaded `.so`
    /// imports are actually present (see `build.rs`'s `--export-dynamic`).
    pub(crate) fn new() -> NifRegistry {
        let _ = std::hint::black_box(beam_abi::nif::enif::enif_api_table());
        NifRegistry {
            libs: HashMap::new(),
        }
    }

    /// Whether `module` already has a loaded NIF library (the oracle's `reload` condition).
    pub(crate) fn is_loaded(&self, module: &str) -> bool {
        self.libs.contains_key(module)
    }

    /// Records `entry` as the library implementing `module`.
    pub(crate) fn insert(&mut self, module: String, entry: NifEntry) {
        self.libs.insert(module, entry);
    }

    /// Whether `module`'s library exports `function/arity`.
    ///
    /// This is the NIF-dispatch predicate: `false` (a miss) is a perfect no-op, so a call to an
    /// ordinary Erlang function of a module that also has a NIF library is never intercepted.
    pub(crate) fn lookup(&self, module: &str, function: &str, arity: u32) -> bool {
        self.libs
            .get(module)
            .is_some_and(|entry| entry.funcs().iter().any(|(n, a)| n == function && *a == arity))
    }

    /// Calls `module`'s NIF `function/arity` with `args` over `env`.
    ///
    /// A module with no library, or one that does not export `function/arity`, is the typed
    /// [`NifCallError::UnknownFunction`] — never a panic and never a fabricated value.
    pub(crate) fn call(
        &self,
        module: &str,
        function: &str,
        arity: u32,
        env: &mut NifEnv,
        args: &[Eterm],
    ) -> std::result::Result<Eterm, NifCallError> {
        match self.libs.get(module) {
            Some(entry) => entry.lib.call(env, function, arity, args),
            None => Err(NifCallError::UnknownFunction {
                name: function.to_string(),
                arity,
            }),
        }
    }
}

/// The runtime services a NIF host calls back into.
///
/// The two `enif_*` hooks [`NifHost::make_ref`] and [`NifHost::whereis_pid`] need state the host
/// does not own — a fresh reference and a name lookup — so the host borrows it through this trait
/// instead of duplicating the runtime's minting/registry logic. The concrete provider is
/// [`RuntimeNifServices`], built by
/// [`RuntimeContext::try_call_nif`](crate::context::RuntimeContext); the load/unload host carries
/// no provider, so both hooks stay the typed `None` there.
pub(crate) trait NifHostServices {
    /// Mint a fresh internal reference on `heap` and register it in the runtime's reference
    /// registry (a **plain** reference, so `demonitor/1` recognises it), returning its term.
    ///
    /// `None` only if the reference thing cannot be built on `heap` — never a fabricated value.
    fn make_ref(&mut self, heap: &mut Heap) -> Option<Eterm>;

    /// Resolve the registered name `name` (an atom) to an internal pid term; `None` for a
    /// non-atom argument or an unregistered name.
    fn whereis_pid(&mut self, name: Eterm) -> Option<Eterm>;
}

/// The concrete [`NifHostServices`] over a runtime's own reference id space, reference registry
/// and process table.
///
/// The three fields are **disjoint borrows** of the runtime ([`Runtime::refs`](crate::Runtime) /
/// `references` / `processes`), so this provider co-exists with the rest of the NIF call's runtime
/// uses. `processes` is the caller-supplied pass table when the call runs inside a scheduler slice,
/// else the runtime's own — the same resolution [`RuntimeContext::process_table`] makes.
pub(crate) struct RuntimeNifServices<'a> {
    /// The one reference id space (`erlang:make_ref/0` and a monitor ref share it).
    pub(crate) refs: &'a mut RefAllocator,
    /// The runtime's reference registry; a minted plain reference is recorded here.
    pub(crate) references: &'a mut HashMap<Eterm, Option<MonitorRef>>,
    /// The process table a registered name is resolved against.
    pub(crate) processes: &'a ProcessTable,
}

impl NifHostServices for RuntimeNifServices<'_> {
    /// The same two operations `erlang:make_ref/0`
    /// ([`crate::native::process::RuntimeContext::make_ref_0`]) performs: draw the next id from the
    /// shared allocator, build its term on `heap`, and register it as a plain reference.
    fn make_ref(&mut self, heap: &mut Heap) -> Option<Eterm> {
        let reference = self.refs.next();
        let term = reference.to_eterm(heap).ok()?;
        self.references.insert(term, None);
        Some(term)
    }

    /// Look `name` up in the process table's local name registry. A non-atom argument never names
    /// a process.
    fn whereis_pid(&mut self, name: Eterm) -> Option<Eterm> {
        if !is_atom(name) {
            return None;
        }
        Some(self.processes.whereis(atom_val(name))?.to_eterm())
    }
}

/// The runtime services one NIF call (or one library `load`/`unload`) draws on.
///
/// It **owns** its [`Heap`] so it has a stable address when boxed (the library's erased host
/// pointer must stay valid), and carries a handle to the runtime's one shared [`AtomTable`]
/// plus the in-flight [`PendingInterner`] a new atom is recorded on.
///
/// The lifetime parameter is the borrow of the optional [`NifHostServices`] provider: the per-call
/// host borrows a [`RuntimeNifServices`] for the duration of the call, while the load/unload host
/// (which is moved into the runtime's NIF registry) carries `None` and is therefore `'static`.
pub(crate) struct NifHostImpl<'a> {
    /// The term-allocation arena for the NIF's lifetime.
    heap: Heap,
    /// A handle to the runtime's one shared atom table (index resolution).
    atoms: Rc<AtomTable>,
    /// The deferred-commit interner new atoms are recorded on.
    ///
    /// Shared (via its `Rc`) with the BIF call that triggered this host, so the interleaved
    /// names of one `load_nif/2` call land in one ordered, exactly-predicted list.
    interner: PendingInterner,
    /// `erlang:self()` as seen by the NIF.
    self_pid: Eterm,
    /// The message environments this host allocated for `enif_alloc_env` and still owns.
    ///
    /// Each is a live [`NifEnv`] (its raw host pointer erased, so the field is lifetime-free) held
    /// here so `enif_free_env` can find it by pointer; dropping the host drops any env a NIF forgot
    /// to free. See [`NifHost::alloc_env`].
    envs: Vec<Box<NifEnv>>,
    /// The runtime's real normal-scheduler count (`+S`), reported by `system_info`.
    ///
    /// `i32` because that is [`ErlDrvSysInfo::scheduler_threads`]'s C type (`c_int`).
    schedulers: i32,
    /// The runtime services this host may call back into; `None` for the load/unload host.
    services: Option<&'a mut dyn NifHostServices>,
}

impl<'a> NifHostImpl<'a> {
    /// A host over `heap`, resolving atoms against `atoms`/`interner`, running as `self_pid`,
    /// reporting `schedulers` normal schedulers and reaching the runtime through `services`.
    pub(crate) fn new(
        heap: Heap,
        atoms: Rc<AtomTable>,
        interner: PendingInterner,
        self_pid: Eterm,
        schedulers: i32,
        services: Option<&'a mut dyn NifHostServices>,
    ) -> NifHostImpl<'a> {
        NifHostImpl {
            heap,
            atoms,
            interner,
            self_pid,
            envs: Vec::new(),
            schedulers,
            services,
        }
    }

    /// Takes the heap back out (the per-call host is consumed once the call returns).
    pub(crate) fn into_heap(self) -> Heap {
        self.heap
    }
}

/// The four independent unique-integer counters (one per property combination), exactly as the C
/// emulator keeps four sequences.
static UNIQUE_COUNTERS: [AtomicI64; 4] = [
    AtomicI64::new(0),
    AtomicI64::new(0),
    AtomicI64::new(0),
    AtomicI64::new(0),
];

/// The monotonic-counter base — `erlang:unique_integer([monotonic])`'s first value.
const UNIQUE_MONOTONIC_BASE: i64 = -576_460_752_303_423_488;
/// The non-monotonic (negative) counter base — the oracle's first `erlang:unique_integer()`.
const UNIQUE_NEGATIVE_BASE: i64 = -576_460_752_303_421_235;
/// The first `erlang:unique_integer([positive])`.
const UNIQUE_POSITIVE_BASE: i64 = 2_307;
/// The first `erlang:unique_integer([positive, monotonic])`.
const UNIQUE_POSITIVE_MONOTONIC_BASE: i64 = 2;

/// `ERLANG_VERSION` — the emulator's ERTS version string, NUL-terminated ([`beam_common::ERTS_VERSION`]).
///
/// A `static` so the pointer handed out by `system_info` (`ErlDrvSysInfo::erts_version`) stays valid
/// for as long as the caller may read it, which is what the C's emulator-owned string guarantees.
static ERTS_VERSION_C: &[u8] = b"17.1\0";

/// `ERLANG_OTP_RELEASE` — the OTP release string, NUL-terminated (the major of
/// [`beam_common::OTP_VERSION`]).
static OTP_RELEASE_C: &[u8] = b"30\0";

impl NifHost for NifHostImpl<'_> {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// Interns `name`: an existing atom keeps its index, a new one is recorded on the shared
    /// interner at its predicted index (committed by the caller after the call returns).
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        if let Some(atom) = self.atoms.lookup(name) {
            return Some(make_atom(atom));
        }
        Some(make_atom(self.interner.intern(name)))
    }

    /// The text of `atom`, or `None` for a non-atom term or an index this table does not hold.
    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        self.atoms.name(atom_val(atom)).map(str::to_string)
    }

    /// `erlang:self()` inside the NIF — the pid the calling process runs as.
    fn self_pid(&mut self) -> Option<Eterm> {
        Some(self.self_pid)
    }

    /// `enif_term_to_binary` — encode `term` as an External Term Format image.
    ///
    /// This is the **same** image `erlang:term_to_binary/1` produces: [`beam_codec::Encoder::new`]
    /// writes the leading `VERSION_MAGIC` (`131`) and defaults to [`MinorVersion::V2`] with no
    /// compression, which is precisely `term_to_binary/1`'s `TERM_TO_BINARY_DFLAGS`-with-`level=0`
    /// option set. The runtime's local node identity is installed first (mirroring the BIF's
    /// `encode_body`), so an internal pid/port id encodes as the oracle's `NEW_PID_EXT`/`V4_PORT_EXT`
    /// rather than being unencodable.
    ///
    /// A tag the codec cannot write — most importantly a local (`make_fun3`) fun, whose identity is
    /// the *compiled module's*, unreachable from this host — yields the typed `None`: `enif_term_to_binary`
    /// then answers the C's failure code `0` and writes nothing, never a wrong image.
    ///
    /// [`MinorVersion::V2`]: beam_codec::MinorVersion::V2
    fn term_to_binary(&mut self, term: Eterm) -> Option<Vec<u8>> {
        // The local node identity is the fixed `nonode@nohost` sysname the runtime seeds into the
        // shared table at boot (the same identity the node bridge hands `beam_codec`). Creation is
        // not part of the term layout this runtime models, so it is the C's default `0`.
        let node = self.atoms.lookup(crate::dist::NONODE_NAME);
        let mut encoder = beam_codec::Encoder::new(&self.atoms);
        if let Some(node) = node {
            encoder = encoder.with_local_node(node, 0);
        }
        encoder.encode(term).ok()?;
        Some(encoder.finish())
    }

    /// `enif_binary_to_term` — decode an External Term Format image onto this host's heap.
    ///
    /// The decoder owns the version-byte check and the `COMPRESSED` wrapper, and is strict about
    /// trailing bytes in the plain form, so a malformed, truncated or wrong-version image is the
    /// typed `None` (the C's `0`). `None` is also what an undecodable *tag* yields — never a partial
    /// term.
    ///
    /// A name the image introduces is not yet in the runtime's shared table (this host may only
    /// borrow it), so the shared table is **cloned** and the decoder interns into the clone; every
    /// name the clone appended is then recorded on the shared [`PendingInterner`] at exactly the
    /// index the clone assigned it, for the caller to materialise once its own borrow is released.
    /// `intern` — not `intern_atom` — is load-bearing: the interner's `base` was fixed when this host
    /// was built, so appending mid-sequence would shift the prediction of every later name.
    fn binary_to_term(&mut self, bytes: &[u8]) -> Option<Eterm> {
        let mut local = (*self.atoms).clone();
        let known = local.len();
        let term = beam_codec::decode_term(bytes, &mut self.heap, &mut local).ok()?;
        for index in known..local.len() {
            let Some(name) = local.name(beam_term::Atom::from_raw(index as u32)) else {
                continue;
            };
            self.interner.intern(name);
        }
        Some(term)
    }

    /// `enif_make_existing_atom` — look `name` up in the **committed** table, never creating it.
    ///
    /// `None` is the C's "no such atom" (`0`, `*atom` untouched). An atom interned earlier in the
    /// *same* call is not yet committed, so it is not visible here — but `beam_abi`'s per-`NifEnv`
    /// atom record ([`NifEnv::recorded_atom`](beam_abi::nif::host::NifEnv)) covers exactly that case
    /// and is consulted before this hook, so a same-call atom still resolves.
    fn existing_atom(&mut self, name: &str) -> Option<Eterm> {
        self.atoms.lookup(name).map(make_atom)
    }

    /// `enif_make_ref()` — a fresh reference, drawn from the runtime's one reference id space.
    ///
    /// Delegates to the borrowed [`NifHostServices`]: the provider allocates the next id, builds
    /// the reference thing on **this host's** heap and registers it as a plain reference (so
    /// `erlang:demonitor/1` recognises it and answers `true`, exactly as for `make_ref/0`). With no
    /// provider (the load/unload host) this is the trait default `None` — `enif_make_ref` then
    /// raises `badarg`, never a fabricated reference.
    fn make_ref(&mut self) -> Option<Eterm> {
        // Disjoint field borrows: `heap` and `services` are distinct fields of `self`.
        let heap = &mut self.heap;
        self.services.as_mut()?.make_ref(heap)
    }

    /// `enif_whereis_pid(name)` — resolve a registered name to a pid.
    ///
    /// Delegates to the borrowed [`NifHostServices`], which looks the name up in the process
    /// table's local name registry. A non-atom argument, an unregistered name, or no provider (the
    /// load/unload host) is the typed `None` — `enif_whereis_pid` then answers the C's `0` and
    /// writes nothing, never a fabricated pid.
    fn whereis_pid(&mut self, name: Eterm) -> Option<Eterm> {
        self.services.as_mut()?.whereis_pid(name)
    }

    /// `enif_make_unique_integer` — draw from the runtime's four NIF-facing unique sequences.
    ///
    /// One monotonic counter per property combination, so a value under `[positive, monotonic]` is
    /// strictly increasing among its own draws. The bases mirror the C emulator's sequences (see
    /// [`beam_abi`](beam_abi::nif::enif::time)'s built-in fallback), so a host-supplied value has the
    /// same shape a library sees from any other runtime. `wrapping_add` keeps a counter that runs for
    /// the lifetime of the process from ever overflowing into a panic.
    ///
    /// This runtime serves no `erlang:unique_integer/1`, so these sequences are the **only**
    /// NIF-facing unique source — nothing else in the runtime draws from them.
    fn unique_integer(&mut self, positive: bool, monotonic: bool) -> Option<Eterm> {
        let index = (positive as usize) | ((monotonic as usize) << 1);
        let base = match (positive, monotonic) {
            (false, false) => UNIQUE_NEGATIVE_BASE,
            (false, true) => UNIQUE_MONOTONIC_BASE,
            (true, false) => UNIQUE_POSITIVE_BASE,
            (true, true) => UNIQUE_POSITIVE_MONOTONIC_BASE,
        };
        let step = UNIQUE_COUNTERS[index].fetch_add(1, Ordering::Relaxed);
        Some(make_small(base.wrapping_add(step)))
    }

    /// `enif_system_info` — the emulator's information block.
    ///
    /// The two version strings point at this file's `static`s, so they are NUL-terminated and outlive
    /// the call. `thread_support`/`smp_support` are the build capabilities the C hard-codes to `1`
    /// (`io.c`'s `driver_system_info`); `async_threads` is `0` because this runtime has no async
    /// pool. `scheduler_threads` is the runtime's **real** normal-scheduler count, the value the
    /// host was built with (from [`crate::info::schedulers_count`] — `+S N` or the machine's
    /// parallelism). `dirty_scheduler_support` is `0`: this runtime has **no** dirty schedulers and
    /// its `enif_schedule_nif` hook is unsupported, so reporting them as available would be a
    /// capability lie the C only avoids because OTP always builds them in. The four ABI-version
    /// fields are `beam_abi`'s compile-time constants (the C hard-codes them in `driver_system_info`
    /// too), and `enif_system_info` overwrites them anyway.
    fn system_info(&mut self) -> Option<ErlDrvSysInfo> {
        Some(ErlDrvSysInfo {
            driver_major_version: beam_abi::ERL_DRV_EXTENDED_MAJOR_VERSION,
            driver_minor_version: beam_abi::ERL_DRV_EXTENDED_MINOR_VERSION,
            erts_version: ERTS_VERSION_C.as_ptr() as *mut c_char,
            otp_release: OTP_RELEASE_C.as_ptr() as *mut c_char,
            thread_support: 1,
            smp_support: 1,
            async_threads: 0,
            scheduler_threads: self.schedulers,
            nif_major_version: beam_abi::ERL_NIF_MAJOR_VERSION,
            nif_minor_version: beam_abi::ERL_NIF_MINOR_VERSION,
            dirty_scheduler_support: 0,
        })
    }

    /// `enif_alloc_env()` — a fresh message environment owned by this host.
    ///
    /// The C allocates a **process-independent** environment whose heap *and* atom table are the
    /// runtime's (`erl_nif.c:669-675`); the message env idiom (`enif_alloc_env` → build a term →
    /// `enif_send` → `enif_free_env`) needs both, so the env here is a live [`NifEnv`] over **this
    /// same host**: its `intern_atom`/`heap` are the running runtime's, so an atom built on it is
    /// the same index the call env builds — never a wrong term.
    ///
    /// `NifEnv::new` erases the host borrow into a raw pointer, so the env is lifetime-free and can
    /// be owned by this host (`envs`) until [`NifHost::free_env`] — or this host's drop — releases
    /// it. It is reached only via `enif_alloc_env`'s current-NIF-call host (`with_current_host`),
    /// which is this host, so the re-publication `NifEnv::new` performs is a no-op for reachability
    /// (the innermost host is unchanged) and its token is withdrawn when the env drops.
    ///
    /// `None` is never returned here while the host is live: the C never answers `NULL` (an
    /// allocation failure aborts the emulator). The `None` case belongs to a host that does not
    /// support the hook (the trait default) or a thread not inside a NIF call (no current host).
    fn alloc_env(&mut self) -> Option<*mut ErlNifEnv> {
        let mut env = Box::new(NifEnv::new(self));
        let ptr = env.as_ptr();
        self.envs.push(env);
        Some(ptr)
    }

    /// `enif_free_env(env)` — release an environment [`NifHost::alloc_env`] handed out.
    ///
    /// `true` when `env` is one of this host's message envs (which is then dropped, withdrawing its
    /// current-host token); `false` for a per-call env or an unknown pointer — memory this host does
    /// not own is never freed (`beam_abi`'s `enif_free_env` already ignores the `false`).
    fn free_env(&mut self, env: *mut ErlNifEnv) -> bool {
        if env.is_null() {
            return false;
        }
        let Some(index) = self
            .envs
            .iter_mut()
            .position(|owned| (**owned).as_ptr() == env)
        else {
            return false;
        };
        self.envs.swap_remove(index);
        true
    }

    /// `enif_make_copy(dst_env, src_term)` — the message-env idiom's term movement.
    ///
    /// In the C the destination env's heap is a *different* heap from the source's, so the term
    /// must be `size_object`d and `copy_struct`ed onto it (`erl_nif.c:1194-1197`, `:843-858`). Here
    /// [`NifHost::alloc_env`] builds every message env over **this same host** — one heap, one atom
    /// table — so a term materialized on the message env already lives on the very heap
    /// `enif_make_copy` would copy it to, and the copy is *exactly* the identity. The host's heap is
    /// the runtime's **pointer-stable** pooled BIF heap (never reset, no GC in Milestone 1), so
    /// reusing the source term is observationally identical to the C's copy and never hands a NIF a
    /// wrong term (the atoms are the same indices either way).
    ///
    /// The trait default would answer `None`, and `enif_make_copy` would then return the ABI's
    /// `THE_NON_VALUE` — a bare header word that is **not** a term, which a NIF returning it hands
    /// straight to the engine. This hook is therefore what makes the whole
    /// `enif_alloc_env` → `enif_make_copy` → `enif_free_env` idiom work; `beam_abi` already refuses
    /// a non-term *source* before this hook is consulted.
    fn make_copy(&mut self, term: Eterm) -> Option<Eterm> {
        Some(term)
    }
}

/// Whether `term` is a term word at all (the C `is_value`/`is_term_word` test).
///
/// `false` for the ABI's `THE_NON_VALUE` (`0`) and any other bare header word — a word the engine
/// must never be handed (its primary tag is the one the boxed-term predicates accept, so a bare
/// header would be dereferenced as a boxed pointer and abort). Used by
/// [`RuntimeContext::try_call_nif`](crate::context::RuntimeContext) to reject a NIF result that is
/// not a term, and mirrors `beam_abi`'s own `is_term_word` (which is crate-private there).
pub(crate) fn is_nif_value(term: Eterm) -> bool {
    !is_header(term)
}

/// Maps `beam_abi`'s loader error onto `beam_bifs`'s identically-shaped one.
///
/// The two enums exist because `beam_abi` must not depend on `beam_bifs` (the dependency runs
/// downward); the mapping is variant-for-variant, so the `{error, {Reason, Text}}` term the BIF
/// renders is the same regardless of which layer produced it.
pub(crate) fn map_load_error(
    error: beam_abi::nif::loader::NifLoadError,
) -> beam_bifs::NifLoadError {
    use beam_abi::nif::loader::NifLoadError as Abi;
    use beam_bifs::NifLoadError as Bifs;
    match error {
        Abi::LoadFailed(text) => Bifs::LoadFailed(text),
        Abi::BadLib(text) => Bifs::BadLib(text),
        Abi::Reload(text) => Bifs::Reload(text),
        Abi::Load(text) => Bifs::Load(text),
        Abi::Upgrade(text) => Bifs::Upgrade(text),
        Abi::OldCode(text) => Bifs::OldCode(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::prelude::{is_small, make_small, signed_value, Atom, NIL};

    /// A host over a fresh table that already holds every name in `seed`, plus the table handle and
    /// the interner, so a test can observe exactly what the host committed (and what it did not).
    fn host_with(seed: &[&str]) -> (NifHostImpl<'static>, Rc<AtomTable>, PendingInterner) {
        let mut table = AtomTable::new();
        for name in seed {
            table.intern(name);
        }
        let atoms = Rc::new(table);
        let interner = PendingInterner::new(atoms.len());
        let host = NifHostImpl::new(
            Heap::new(256).expect("a test heap"),
            Rc::clone(&atoms),
            interner.clone(),
            make_small(7),
            1,
            None,
        );
        (host, atoms, interner)
    }

    /// [`NifHost::make_copy`] is the **identity**: `alloc_env` builds the message env over this
    /// same host, so the message env and the call env share one heap and there is nothing to move.
    ///
    /// This is the hook behind `enif_make_copy`. Without it `beam_abi` answers the ABI's
    /// `THE_NON_VALUE` (a bare header word), which a NIF returning it hands to the engine — the
    /// gate's `nif_res/alloc_env_roundtrip` then aborted on the term comparison. The end-to-end
    /// round trip through the real binary is pinned by
    /// `tests/nif.rs::enif_make_copy_copies_the_message_env_term_into_the_call_env`.
    #[test]
    fn make_copy_is_the_identity_over_the_shared_host_heap() {
        let atoms = Rc::new(AtomTable::new());
        let mut host = NifHostImpl::new(
            Heap::new(256).expect("a test heap"),
            atoms,
            PendingInterner::new(0),
            make_small(7),
            1,
            None,
        );
        let mut env = NifEnv::new(&mut host);

        let atom = env
            .host()
            .intern_atom("ok")
            .expect("interns unconditionally");
        assert_eq!(
            env.host().make_copy(atom),
            Some(atom),
            "a value copies to itself"
        );
        assert_eq!(
            env.host().make_copy(make_small(42)),
            Some(make_small(42)),
            "an immediate copies to itself"
        );
    }

    /// A bare header word (the ABI's `THE_NON_VALUE`, and any other non-term) is rejected by
    /// [`is_nif_value`] — the guard `try_call_nif` uses to keep it out of the engine.
    #[test]
    fn a_non_term_result_is_recognised() {
        assert!(
            !is_nif_value(Eterm::from_raw(0)),
            "THE_NON_VALUE is not a term"
        );
        assert!(!is_nif_value(beam_term::prelude::make_header(
            0,
            beam_term::prelude::FLOAT_SUBTAG
        )));
        assert!(is_nif_value(make_small(7)));
        assert!(is_nif_value(beam_term::prelude::NIL));
    }
    #[test]
    fn a_fresh_registry_knows_no_library() {
        let registry = NifRegistry::new();
        assert!(!registry.is_loaded("test_nif"));
        assert!(!registry.lookup("test_nif", "add", 2));

        // The dispatch predicate is a no-op miss: `call` reports the typed unknown function.
        let atoms = Rc::new(AtomTable::new());
        let mut host = NifHostImpl::new(
            Heap::new(256).expect("a test heap"),
            atoms,
            PendingInterner::new(0),
            make_small(7),
            1,
            None,
        );
        let mut env = NifEnv::new(&mut host);
        match registry.call("test_nif", "add", 2, &mut env, &[]) {
            Err(NifCallError::UnknownFunction { name, arity }) => {
                assert_eq!(name, "add");
                assert_eq!(arity, 2);
            }
            other => panic!("expected UnknownFunction, got {other:?}"),
        }
    }

    #[test]
    fn the_host_resolves_existing_atoms_and_records_new_ones() {
        let mut table = AtomTable::new();
        let known = table.intern("known");
        let atoms = Rc::new(table);
        let interner = PendingInterner::new(atoms.len());
        let mut host = NifHostImpl::new(
            Heap::new(256).expect("a test heap"),
            Rc::clone(&atoms),
            interner.clone(),
            make_small(7),
            1,
            None,
        );

        // An existing atom keeps its index.
        assert_eq!(host.intern_atom("known"), Some(make_atom(known)));
        // A new atom is predicted at `base + 0` (nothing else interning in between).
        let fresh = host.intern_atom("fresh").expect("always interns");
        assert!(is_atom(fresh));
        assert_eq!(atom_val(fresh), Atom::from_raw(atoms.len() as u32));
        // The name is not in the live table yet — it is committed by the caller.
        assert_eq!(host.atom_name(fresh), None);
        // A non-atom term never names an atom.
        assert_eq!(host.atom_name(make_small(9)), None);
        // `self_pid` is reported verbatim.
        assert_eq!(host.self_pid(), Some(make_small(7)));

        // The committed names land exactly at the predicted index.
        let mut committed = (*atoms).clone();
        interner.commit(&mut committed);
        assert_eq!(committed.name(atom_val(fresh)), Some("fresh"));

        // A second call to intern the same name deduplicates within the call.
        assert_eq!(host.intern_atom("fresh"), Some(fresh));
    }

    #[test]
    fn the_host_hands_its_heap_back() {
        let atoms = Rc::new(AtomTable::new());
        let host = NifHostImpl::new(
            Heap::new(64).expect("a test heap"),
            atoms,
            PendingInterner::new(0),
            make_small(7),
            1,
            None,
        );
        let heap = host.into_heap();
        assert!(heap.available() > 0, "the heap survives the host");
    }

    #[test]
    fn load_errors_map_variant_for_variant() {
        use beam_abi::nif::loader::NifLoadError as Abi;
        use beam_bifs::NifLoadError as Bifs;
        assert_eq!(
            map_load_error(Abi::LoadFailed("a".into())),
            Bifs::LoadFailed("a".into())
        );
        assert_eq!(
            map_load_error(Abi::BadLib("b".into())),
            Bifs::BadLib("b".into())
        );
        assert_eq!(
            map_load_error(Abi::Reload("c".into())),
            Bifs::Reload("c".into())
        );
        assert_eq!(map_load_error(Abi::Load("d".into())), Bifs::Load("d".into()));
        assert_eq!(
            map_load_error(Abi::Upgrade("e".into())),
            Bifs::Upgrade("e".into())
        );
        assert_eq!(
            map_load_error(Abi::OldCode("f".into())),
            Bifs::OldCode("f".into())
        );
    }

    // The oracle byte vectors: `binary_to_list(term_to_binary(T))` from the system `erl`, pinned
    // literally so the host's ETF encoder is checked against the real image, not a guess.
    /// `42` → `[131, 97, 42]`.
    const ORACLE_SMALL: &[u8] = &[131, 97, 42];
    /// `[1,2,3]` → `[131, 107, 0, 3, 1, 2, 3]`.
    const ORACLE_LIST: &[u8] = &[131, 107, 0, 3, 1, 2, 3];
    /// `hello` → `[131, 119, 5, 104, 101, 108, 108, 111]`.
    const ORACLE_ATOM: &[u8] = &[131, 119, 5, 104, 101, 108, 108, 111];
    /// `#{a => 1}` → `[131, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1]`.
    const ORACLE_MAP: &[u8] = &[131, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1];
    /// `{a,1,<<"hi">>}` → `[131, 104, 3, 119, 1, 97, 97, 1, 109, 0, 0, 0, 2, 104, 105]`.
    const ORACLE_TUPLE: &[u8] = &[131, 104, 3, 119, 1, 97, 97, 1, 109, 0, 0, 0, 2, 104, 105];

    /// `term_to_binary` emits the very bytes `erlang:term_to_binary/1` emits — the version byte,
    /// the `MinorVersion::V2` tag set, no compression.
    #[test]
    fn term_to_binary_matches_the_oracle_bytes() {
        let (mut host, _atoms, _interner) = host_with(&["hello", "a"]);

        assert_eq!(
            host.term_to_binary(make_small(42)).as_deref(),
            Some(ORACLE_SMALL)
        );

        // `hello` is committed, so the encoder can name it (a predicted-only atom could not).
        let hello = host.intern_atom("hello").expect("seeded");
        assert_eq!(host.term_to_binary(hello).as_deref(), Some(ORACLE_ATOM));

        // Build `[1,2,3]` on the host's own heap; the returned pointers stay valid because the
        // bump heap never moves its storage.
        let heap = host.heap();
        let l3 = heap.cons(make_small(3), NIL).expect("a test heap");
        let l2 = heap.cons(make_small(2), l3).expect("a test heap");
        let list = heap.cons(make_small(1), l2).expect("a test heap");
        assert_eq!(host.term_to_binary(list).as_deref(), Some(ORACLE_LIST));
    }

    /// Every oracle image decodes and then re-encodes to exactly the bytes it came from.
    #[test]
    fn binary_to_term_round_trips_the_oracle_images() {
        let (mut host, _atoms, _interner) = host_with(&["hello", "a"]);

        for image in [ORACLE_SMALL, ORACLE_LIST, ORACLE_ATOM, ORACLE_MAP, ORACLE_TUPLE] {
            let term = host.binary_to_term(image).expect("a decodable oracle image");
            assert_eq!(
                host.term_to_binary(term).as_deref(),
                Some(image),
                "the decoded term re-encodes to the image it came from"
            );
        }

        assert_eq!(host.binary_to_term(ORACLE_SMALL), Some(make_small(42)));
    }

    /// A malformed, truncated or wrong-version image is the typed `None` — never a partial term.
    #[test]
    fn binary_to_term_rejects_malformed_and_truncated_images() {
        let (mut host, _atoms, _interner) = host_with(&["a"]);
        for bad in [
            &[][..],
            &[131][..],                 // no term after the version byte
            &[131, 97][..],             // a small int with its byte missing
            &[131, 107, 0, 3, 1, 2][..], // a list claiming 3 elements but holding 2
            &[131, 97, 42, 0][..],      // a trailing byte after a complete plain term
            &[130, 97, 42][..],         // the wrong version byte
        ] {
            assert_eq!(host.binary_to_term(bad), None, "rejected: {bad:?}");
        }
    }

    /// A committed name decodes to its existing index; a new name is predicted at the index the
    /// cloned table assigned it and materialises there once the caller commits.
    #[test]
    fn binary_to_term_resolves_and_predicts_atoms() {
        let (mut host, atoms, interner) = host_with(&["known"]);
        let known = atoms.lookup("known").expect("seeded");

        // `known` — an atom already in the table, so the interner is untouched.
        let existing = &[131u8, 119, 5, b'k', b'n', b'o', b'w', b'n'];
        assert_eq!(host.binary_to_term(existing), Some(make_atom(known)));
        assert!(interner.is_empty(), "an existing atom interns nothing");

        // `fresh` — a name the table does not hold yet: predicted at the clone's first free index.
        let fresh = &[131u8, 119, 5, b'f', b'r', b'e', b's', b'h'];
        let term = host.binary_to_term(fresh).expect("decodes");
        assert_eq!(term, make_atom(Atom::from_raw(atoms.len() as u32)));

        // The commit lands the name at exactly that predicted index.
        let mut committed = (*atoms).clone();
        interner.commit(&mut committed);
        assert_eq!(
            committed.name(Atom::from_raw(atoms.len() as u32)),
            Some("fresh")
        );
    }

    /// `existing_atom` is a pure lookup: a miss interns nothing and records nothing.
    #[test]
    fn existing_atom_never_interns() {
        let (mut host, atoms, interner) = host_with(&["known"]);
        let before = atoms.len();

        assert_eq!(
            host.existing_atom("known"),
            Some(make_atom(atoms.lookup("known").expect("seeded")))
        );
        assert_eq!(host.existing_atom("nope"), None, "an unknown name is a miss");
        assert_eq!(atoms.len(), before, "a miss interns nothing");
        assert!(interner.is_empty(), "a miss records nothing on the interner");
    }

    /// Each property combination draws from its own sequence: successive values are strictly
    /// increasing, and `positive = true` yields a positive value.
    #[test]
    fn unique_integers_are_strictly_increasing_and_positive_when_asked() {
        let (mut host, _atoms, _interner) = host_with(&[]);
        for (positive, monotonic) in [(false, false), (false, true), (true, false), (true, true)] {
            let first = host
                .unique_integer(positive, monotonic)
                .expect("always mints");
            let second = host
                .unique_integer(positive, monotonic)
                .expect("always mints");
            assert!(is_small(first) && is_small(second), "both are small integers");
            assert!(
                signed_value(second) > signed_value(first),
                "strictly increasing for ({positive}, {monotonic})"
            );
            if positive {
                assert!(signed_value(first) > 0, "positive = true yields > 0");
            }
        }
    }

    /// The emulator information block: NUL-terminated version strings equal to the workspace
    /// constants, the build capabilities, and `beam_abi`'s ABI versions.
    #[test]
    fn system_info_reports_the_emulators_real_shape() {
        // The statics are NUL-terminated and carry exactly the workspace's version strings.
        assert_eq!(ERTS_VERSION_C.last(), Some(&0), "ERTS version is NUL-terminated");
        assert_eq!(
            &ERTS_VERSION_C[..ERTS_VERSION_C.len() - 1],
            beam_common::ERTS_VERSION.as_bytes()
        );
        let otp_major = beam_common::OTP_VERSION
            .split('.')
            .next()
            .expect("OTP_VERSION has a major");
        assert_eq!(OTP_RELEASE_C.last(), Some(&0), "OTP release is NUL-terminated");
        assert_eq!(
            &OTP_RELEASE_C[..OTP_RELEASE_C.len() - 1],
            otp_major.as_bytes()
        );

        let (mut host, _atoms, _interner) = host_with(&[]);
        let info = host.system_info().expect("always supplies the block");
        assert_eq!(info.erts_version, ERTS_VERSION_C.as_ptr() as *mut c_char);
        assert_eq!(info.otp_release, OTP_RELEASE_C.as_ptr() as *mut c_char);
        assert_eq!(info.thread_support, 1);
        assert_eq!(info.smp_support, 1);
        assert_eq!(info.async_threads, 0);
        assert_eq!(info.scheduler_threads, 1);
        assert_eq!(info.dirty_scheduler_support, 0);
        assert_eq!(
            info.driver_major_version,
            beam_abi::ERL_DRV_EXTENDED_MAJOR_VERSION
        );
        assert_eq!(
            info.driver_minor_version,
            beam_abi::ERL_DRV_EXTENDED_MINOR_VERSION
        );
        assert_eq!(info.nif_major_version, beam_abi::ERL_NIF_MAJOR_VERSION);
        assert_eq!(info.nif_minor_version, beam_abi::ERL_NIF_MINOR_VERSION);
    }

    /// The [`NifHostServices`] provider a host delegates `make_ref`/`whereis_pid` to, recording
    /// that `make_ref` was handed the host's own heap.
    struct MockServices {
        reference: Eterm,
        name: Eterm,
        pid: Eterm,
        saw_heap: bool,
    }

    impl NifHostServices for MockServices {
        fn make_ref(&mut self, heap: &mut Heap) -> Option<Eterm> {
            self.saw_heap = heap.available() > 0;
            Some(self.reference)
        }

        fn whereis_pid(&mut self, name: Eterm) -> Option<Eterm> {
            (name == self.name).then_some(self.pid)
        }
    }

    /// `make_ref`/`whereis_pid` delegate to the borrowed provider (the reference on the host's own
    /// heap), and stay the typed `None` when the host carries no provider (the load/unload host) —
    /// never a fabricated value.
    #[test]
    fn the_reference_and_registry_hooks_delegate_to_the_provider() {
        use beam_term::Atom;

        let atoms = Rc::new(AtomTable::new());
        let registered = make_atom(Atom::from_raw(3));
        let mut services = MockServices {
            reference: make_small(11),
            name: registered,
            pid: make_small(22),
            saw_heap: false,
        };
        let mut host = NifHostImpl::new(
            Heap::new(256).expect("a test heap"),
            atoms,
            PendingInterner::new(0),
            make_small(7),
            1,
            Some(&mut services),
        );
        assert_eq!(
            host.make_ref(),
            Some(make_small(11)),
            "delegates to the provider"
        );
        assert_eq!(
            host.whereis_pid(registered),
            Some(make_small(22)),
            "a registered name resolves through the provider"
        );
        assert_eq!(
            host.whereis_pid(make_atom(Atom::from_raw(4))),
            None,
            "an unregistered name is the provider's miss"
        );
        drop(host);
        assert!(services.saw_heap, "make_ref is handed the host's own heap");

        // No provider (the load/unload host): the typed default, never a fabricated value.
        let (mut bare, _atoms, _interner) = host_with(&[]);
        assert_eq!(bare.make_ref(), None);
        assert_eq!(bare.whereis_pid(registered), None);
    }

    /// `system_info` reports the scheduler count the host was built with — the runtime's real
    /// normal-scheduler count (`+S`), not a hard-coded `1`.
    #[test]
    fn system_info_reports_the_built_scheduler_count() {
        let atoms = Rc::new(AtomTable::new());
        let mut host = NifHostImpl::new(
            Heap::new(256).expect("a test heap"),
            atoms,
            PendingInterner::new(0),
            make_small(7),
            4,
            None,
        );
        assert_eq!(
            host.system_info()
                .expect("always supplies the block")
                .scheduler_threads,
            4
        );
    }

    /// [`RuntimeNifServices`] mints a reference from the runtime's **own** id space and registers
    /// it, and resolves a name through the runtime's process table — the same two operations
    /// `erlang:make_ref/0` and `erlang:whereis/1` perform.
    #[test]
    fn runtime_services_mint_a_registered_reference_and_resolve_a_name() {
        let mut runtime = crate::Runtime::with_defaults().expect("the default runtime");
        let name_atom = runtime.code_server_atoms_mut().intern("a_registered_name");
        let absent = runtime.code_server_atoms_mut().intern("surely_not_registered");
        let name = make_atom(name_atom);
        let absent = make_atom(absent);
        let pid = runtime.self_pid();
        runtime
            .processes
            .register(name_atom, pid)
            .expect("registers an unregistered name");

        let mut heap = Heap::new(256).expect("a test heap");
        let mut services = RuntimeNifServices {
            refs: &mut runtime.refs,
            references: &mut runtime.references,
            processes: &runtime.processes,
        };
        let reference = services.make_ref(&mut heap).expect("mints a reference");
        assert_eq!(
            services.whereis_pid(name),
            Some(pid.to_eterm()),
            "a registered name resolves to its pid"
        );
        assert_eq!(
            services.whereis_pid(absent),
            None,
            "an unregistered name is a miss"
        );
        assert_eq!(
            services.whereis_pid(make_small(1)),
            None,
            "a non-atom never names a process"
        );
        drop(services);
        assert!(
            runtime.references.contains_key(&reference),
            "the minted reference is registered in the runtime ({reference:?})"
        );
    }
}
