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

//! The dynamic NIF library loader — `dlopen`/`dlsym` + the `ErlNifEntry` load-time contract.
//!
//! This is the runtime half of contract **A.3** (the NIF C ABI): it reproduces what
//! `erts_load_nif` (`erts/emulator/beam/erl_nif.c:4729`) does between `erlang:load_nif/2` and a
//! callable NIF — open the shared object, resolve `nif_init`, validate the returned
//! [`ErlNifEntry`] in the oracle's **exact order** with its exact reason atoms and texts, run the
//! `load` callback, and expose the `{name, arity}` function table.
//!
//! # Error surface (matches the oracle's `{error,{Reason,Text}}`)
//!
//! Every failure is a [`NifLoadError`] carrying both the reason atom ([`NifLoadError::reason_atom`])
//! and the verbatim `Text` ([`NifLoadError::text`]). The mapping is the one tabulated in the crate
//! `CONTEXT.md` ("Oracle behavior: `erlang:load_nif/2` load-time contract"):
//!
//! | oracle check (`erl_nif.c`) | reason | variant |
//! |---|---|---|
//! | `dlopen` fails | `load_failed` | [`NifLoadError::LoadFailed`] |
//! | `dlsym nif_init`/`_nif_init` fails | `bad_lib` | [`NifLoadError::BadLib`] |
//! | `nif_init()` → `NULL` | `bad_lib` | [`NifLoadError::BadLib`] |
//! | version too new | `bad_lib` | [`NifLoadError::BadLib`] |
//! | version too old (or the `2.5` special case) | `bad_lib` | [`NifLoadError::BadLib`] |
//! | `vm_variant` mismatch | `bad_lib` | [`NifLoadError::BadLib`] |
//! | module-name mismatch | `bad_lib` | [`NifLoadError::BadLib`] |
//! | `load` callback veto | `load` | [`NifLoadError::Load`] |
//!
//! [`NifLoadError::Reload`], [`NifLoadError::Upgrade`] and [`NifLoadError::OldCode`] round out the
//! oracle's reason surface but are **never produced by [`load`]**: detecting a reload needs the
//! module instance's `nif` pointer, `upgrade` needs a previous module instance, and `old_code`
//! needs the caller's code area — all module/code-server state that lives outside this crate.
//!
//! # One documented deviation from the C
//!
//! **The `entry.funcs` table is also bound-checked here.** The C iterates
//! `entry->funcs[0..num_of_funcs]` and would fault on a `NULL` table; [`load`] returns a typed
//! `bad_lib` instead. The C's checks 9–12 (`Function not found` / `not declared as nif` /
//! `Illegal flags field value` / `Duplicate NIF entry`) compare the table against the *calling
//! module's* code, which is not reachable from this crate, so they are not performed here.
//!
//! The path rule is **not** a deviation: the shared-object suffix is appended unconditionally, so
//! a path already ending in `.so` opens `<path>.so`, exactly like `erts_sys_ddll_open`
//! (`erts/emulator/sys/unix/erl_unix_sys_ddll.c:104-118`, `FILE_EXT`), and both the `dlopen` call
//! and every message name the suffixed path.
//!
//! # Ownership
//!
//! [`NifLibrary`] owns the `dlopen` handle and the host the library was loaded against: its `Drop`
//! invokes the library's `unload` callback (with a live [`NifEnv`] built from that host) and then
//! `dlclose`s. As with [`NifEnv`], the caller keeps the host alive for as long as the library.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use libc::{c_char, c_int, c_void};

use beam_term::prelude::Eterm;

use crate::nif::host::{NifEnv, NifHost};
use crate::nif::term::{eterm_from_nif, nif_from_eterm, ERL_NIF_TERM};
use crate::nif::types::{ErlNifEntry, ErlNifEnv};
use crate::{
    ERL_NIF_MAJOR_VERSION, ERL_NIF_MINOR_VERSION, ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD,
    ERL_NIF_VM_VARIANT,
};

/// The shared-object suffix the oracle appends (`erl_unix_sys_ddll.c:49`, `FILE_EXT`).
const SHARED_OBJECT_SUFFIX: &str = ".so";

/// The `load_failed` message prefix (`erl_nif.c:4801`).
const LOAD_SLOGAN: &str = "Failed to load NIF library";

/// The signature of a [`ErlNifFunc::fptr`] NIF implementation.
type NifFptr = unsafe extern "C" fn(*mut ErlNifEnv, c_int, *const ERL_NIF_TERM) -> ERL_NIF_TERM;

// ---------------------------------------------------------------------------------------------
// The in-flight library's private data (the C's `env->mod_nif->priv_data`)
// ---------------------------------------------------------------------------------------------
//
// `enif_priv_data` (`erl_nif.c:609-612`) answers `env->mod_nif->priv_data` — the private pointer of
// the **library the call belongs to**, i.e. the pointer that library's `load` callback wrote into
// `lib->priv_data` (`erl_nif.c:4976` resets it, `:5003` stores what the callback produced). The C
// reaches that field through the environment, which points at the library; this crate's C-facing env
// ([`NifEnv`]) carries only the *host*, so the library is published here for the duration of
// [`NifLibrary::call`] and read back by `nif::enif::envs::enif_priv_data`.
//
// The store is **thread-local**, for the same reason the current-host stack is
// (`nif::host::CURRENT_HOSTS`): a NIF call runs on the thread that entered it — in the C the call env
// and its `mod_nif` live on that thread too — and [`NifEnv`], the only other handle on the call, is
// not `Send` (it holds a raw host pointer). A publication therefore reaches no other thread: a
// concurrent NIF call on another thread sees its own stack. Neither [`NifLibrary`] nor
// [`NifLibrary::call`] changes shape or gains a non-`Send` member — the publication is a local RAII
// guard holding only a token, so `beam_rt`'s use of a loaded library is unaffected.
//
// A **stack**, because a library call can nest (a NIF may call back into a second library), and each
// guard removes exactly its own entry: nesting restores the enclosing library's pointer, and a
// panic/unwind out of the call still restores it (`Drop` runs on unwind). Once the outermost guard is
// gone the stack is empty, which is what makes [`NifHost::priv_data`] the fallback source again.

thread_local! {
    /// The priv-data pointers published by the library calls live on this thread, innermost (current)
    /// last.
    static CURRENT_PRIV_DATA: RefCell<Vec<(u64, *mut c_void)>> = const { RefCell::new(Vec::new()) };
}

/// Identity of the next [`CurrentPrivData`] guard; drawn once per guard, never reused.
static NEXT_PRIV_DATA_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Publish `priv_data` as the private pointer of the library call whose guard carries `token`.
fn push_current_priv_data(token: u64, priv_data: *mut c_void) {
    CURRENT_PRIV_DATA.with(|stack| stack.borrow_mut().push((token, priv_data)));
}

/// Remove `token`'s entry, wherever it sits ([`CurrentPrivData`]'s `Drop`).
///
/// Removing a *position* rather than the top keeps the stack sound if guards ever unwind out of
/// nesting order: every guard removes its own entry, so the count reaches zero exactly when the
/// outermost call is gone.
fn pop_current_priv_data(token: u64) {
    CURRENT_PRIV_DATA.with(|stack| {
        let mut stack = stack.borrow_mut();
        if let Some(pos) = stack.iter().position(|&(t, _)| t == token) {
            stack.remove(pos);
        }
    });
}

/// The private pointer of the library whose NIF call this thread is inside, if any.
///
/// `None` means "no library call is in progress on this thread" — the reader
/// (`nif::enif::envs::enif_priv_data`) then falls back to its host. A published `NULL` is distinct
/// from `None` and is answered as such: the library is authoritative for its own private data, just
/// as `env->mod_nif->priv_data` is in the C.
pub(crate) fn current_priv_data() -> Option<*mut c_void> {
    CURRENT_PRIV_DATA.with(|stack| stack.borrow().last().map(|&(_, priv_data)| priv_data))
}

/// RAII publication of one library's private pointer for the duration of a [`NifLibrary::call`].
struct CurrentPrivData {
    /// Identity of this guard's [`CURRENT_PRIV_DATA`] entry, so `Drop` removes exactly its own.
    token: u64,
}

impl CurrentPrivData {
    /// Publish `priv_data` on this thread until the guard is dropped.
    fn publish(priv_data: *mut c_void) -> CurrentPrivData {
        let token = NEXT_PRIV_DATA_TOKEN.fetch_add(1, Ordering::Relaxed);
        push_current_priv_data(token, priv_data);
        CurrentPrivData { token }
    }
}

impl Drop for CurrentPrivData {
    /// Withdraw this guard's publication, so a reader sees the enclosing call's library again — or
    /// no library at all, once the outermost call is over.
    fn drop(&mut self) {
        pop_current_priv_data(self.token);
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// A failure of [`load`], mirroring the oracle's `{error,{Reason,Text}}`.
///
/// The payload is the oracle's `Text` verbatim; [`NifLoadError::reason_atom`] is the reason atom.
#[derive(Debug)]
pub enum NifLoadError {
    /// `load_failed` — the shared object could not be opened (`dlopen`).
    LoadFailed(String),
    /// `bad_lib` — the library is not a usable NIF: no init function, a `NULL` entry, a version /
    /// variant / module-name mismatch, or a malformed function table.
    BadLib(String),
    /// `reload` — the module already has a NIF library loaded (never produced by [`load`]).
    Reload(String),
    /// `load` — the library's `load` callback vetoed the load.
    Load(String),
    /// `upgrade` — the library's `upgrade` callback vetoed a reload (never produced by [`load`]).
    Upgrade(String),
    /// `old_code` — `load_nif` was called from an old version of the module (never by [`load`]).
    OldCode(String),
}

impl NifLoadError {
    /// The reason atom the oracle places in `{error,{Reason,Text}}`.
    pub fn reason_atom(&self) -> &'static str {
        match self {
            NifLoadError::LoadFailed(_) => "load_failed",
            NifLoadError::BadLib(_) => "bad_lib",
            NifLoadError::Reload(_) => "reload",
            NifLoadError::Load(_) => "load",
            NifLoadError::Upgrade(_) => "upgrade",
            NifLoadError::OldCode(_) => "old_code",
        }
    }

    /// The verbatim `Text` of `{error,{Reason,Text}}`.
    pub fn text(&self) -> &str {
        match self {
            NifLoadError::LoadFailed(text)
            | NifLoadError::BadLib(text)
            | NifLoadError::Reload(text)
            | NifLoadError::Load(text)
            | NifLoadError::Upgrade(text)
            | NifLoadError::OldCode(text) => text,
        }
    }
}

impl fmt::Display for NifLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason_atom(), self.text())
    }
}

impl std::error::Error for NifLoadError {}

/// A failure of [`NifLibrary::call`].
#[derive(Debug)]
pub enum NifCallError {
    /// No function of that `{name, arity}` is exported by the library.
    UnknownFunction {
        /// The requested Erlang function name.
        name: String,
        /// The requested arity.
        arity: u32,
    },
    /// The NIF recorded an exception on the environment (`enif_raise_exception`).
    Raised(Eterm),
}

impl fmt::Display for NifCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NifCallError::UnknownFunction { name, arity } => {
                write!(f, "unknown NIF function {name}/{arity}")
            }
            NifCallError::Raised(reason) => write!(f, "NIF raised {reason:?}"),
        }
    }
}

impl std::error::Error for NifCallError {}

// ---------------------------------------------------------------------------------------------
// The loaded library
// ---------------------------------------------------------------------------------------------

/// A loaded NIF library: an open `dlopen` handle, the library's [`ErlNifEntry`], the `load`
/// callback's private pointer, and the exported `{name, arity}` table.
#[derive(Debug)]
pub struct NifLibrary {
    /// The `dlopen` handle, closed by [`NifLibrary::drop`].
    handle: *mut c_void,
    /// The library's `nif_init` result (live static data inside the loaded object).
    entry: *mut ErlNifEntry,
    /// The host the library was loaded against, used to build the `unload` callback's env.
    host: *mut dyn NifHost,
    /// The private pointer the `load` callback produced (`ErlNifEntry`'s `priv_data`).
    priv_data: *mut c_void,
    /// The Erlang module the library declares it implements.
    module: String,
    /// The exported `{name, arity}` pairs, in the library's declared order.
    funcs: Vec<(String, u32)>,
}

impl NifLibrary {
    /// Call the NIF `name/arity` with `args`.
    ///
    /// `args` must hold exactly `arity` terms (the NIF reads `argv[0..arity]`). On return, a
    /// pending exception recorded on `env` turns the call into [`NifCallError::Raised`]; otherwise
    /// the NIF's return term is handed back.
    ///
    /// For the duration of the call this library's `priv_data` pointer is published on the calling
    /// thread, which is how `enif_priv_data` answers the C's `env->mod_nif->priv_data` (see the
    /// module section "The in-flight library's private data").
    pub fn call(
        &self,
        env: &mut NifEnv,
        name: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm, NifCallError> {
        let Some(fptr) = self.find(name, arity) else {
            return Err(NifCallError::UnknownFunction {
                name: name.to_string(),
                arity,
            });
        };
        // `Eterm` and `ERL_NIF_TERM` are both `#[repr(transparent)]` over `usize` (contract A.3),
        // so this pointer cast reinterprets nothing — it is the same array of machine words.
        let argv = args.as_ptr() as *const ERL_NIF_TERM;
        // Publish this library's private pointer for the duration of the call — the C's call env
        // carries it in `env->mod_nif` (`erl_nif.c:609-612`), and `enif_priv_data` reads it back
        // (`nif::enif::envs`). The guard is held in a named binding so it lives until the NIF has
        // returned, and restores the enclosing library's pointer even on unwind.
        let _current_priv_data = CurrentPrivData::publish(self.priv_data);
        // SAFETY: `fptr` is the library's own implementation for exactly this `{name, arity}`; the
        // code stays mapped for as long as `self` (dropped in `Drop` via `dlclose`). `env` is a live
        // `NifEnv` and `argv` points at `args` (at least `arity` elements by contract).
        let raw = unsafe { fptr(env.as_ptr(), arity as c_int, argv) };
        if let Some(reason) = env.pending_exception() {
            return Err(NifCallError::Raised(reason));
        }
        Ok(eterm_from_nif(raw))
    }

    /// The Erlang module the library declares it implements.
    pub fn module_name(&self) -> &str {
        &self.module
    }

    /// The exported `{name, arity}` pairs, in the library's declared order.
    pub fn funcs(&self) -> &[(String, u32)] {
        &self.funcs
    }

    /// The `priv_data` pointer the entry's `load` callback produced.
    pub fn priv_data(&self) -> *mut c_void {
        self.priv_data
    }

    /// Look `name/arity` up in the live `ErlNifEntry` function table.
    fn find(&self, name: &str, arity: u32) -> Option<NifFptr> {
        // SAFETY: `self.entry` is non-null and owned by the loaded library, alive for `&self`.
        let (num, funcs) = unsafe { ((*self.entry).num_of_funcs, (*self.entry).funcs) };
        let num = num.max(0) as usize;
        if num > 0 && funcs.is_null() {
            return None;
        }
        for i in 0..num {
            // SAFETY: the `nif_init` contract guarantees `funcs` holds `num` entries.
            let f = unsafe { &*funcs.add(i) };
            if f.arity == arity && read_cstring(f.name).as_deref() == Some(name) {
                return f.fptr;
            }
        }
        None
    }
}

impl Drop for NifLibrary {
    fn drop(&mut self) {
        // SAFETY: `self.entry` is live (the library is still open — `dlclose` is below).
        if let Some(unload) = unsafe { (*self.entry).unload } {
            if self.host.is_null() {
                // No host to build an env from: pass a null env rather than a dangling one.
                // SAFETY: `unload` is the library's own callback; a null env is the documented
                // best-effort shape when no host is available.
                unsafe { unload(core::ptr::null_mut(), self.priv_data) };
            } else {
                // SAFETY: `self.host` was produced from a live `&mut dyn NifHost` in `load`; the
                // caller keeps the host alive for as long as the library (documented contract).
                let mut env = NifEnv::new(unsafe { &mut *self.host });
                // SAFETY: `unload` is the library's own `unload` callback; `env` outlives the call.
                unsafe { unload(env.as_ptr(), self.priv_data) };
            }
        }
        if !self.handle.is_null() {
            // SAFETY: `self.handle` came from a successful `dlopen` and is closed exactly once.
            unsafe { libc::dlclose(self.handle) };
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------------------------

/// Load a NIF shared object and validate it in the oracle's exact order (see the crate
/// `CONTEXT.md` table and the module docs above).
///
/// `path` is as passed to `erlang:load_nif/2`; the shared-object suffix (`.so`) is appended
/// **unconditionally**, so a path that already names a shared object resolves `<path>.so` (the
/// oracle appends `FILE_EXT` the same way, `erts/emulator/sys/unix/erl_unix_sys_ddll.c:104-118`).
/// `module` is the calling module's name (the oracle derives it from the call site's code
/// area; here the caller supplies it). `load_info` is the second argument of `load_nif/2`, handed
/// verbatim to the library's `load` callback.
///
/// The returned [`NifLibrary`] borrows `host` for its lifetime: keep `host` alive until the library
/// is dropped (the `unload` callback runs against it).
pub fn load(
    path: &str,
    module: &str,
    host: &mut dyn NifHost,
    load_info: Eterm,
) -> Result<NifLibrary, NifLoadError> {
    // The host is needed again in `NifLibrary::drop` (the `unload` callback), where no borrow can be
    // named. Erase the borrow's lifetime into a raw trait-object pointer, exactly as `NifEnv::new`
    // does: `&mut dyn NifHost` and `*mut dyn NifHost` are layout-identical (data word + vtable
    // word) and the transmute is a bit-preserving cast, so the caller's borrow is reproduced by
    // `NifLibrary::host`.
    // SAFETY: only the erased trait-object lifetime is reinterpreted; the caller keeps the host
    // alive for as long as the returned library.
    let host_ptr: *mut dyn NifHost = unsafe { core::mem::transmute(host) };

    let so_path = shared_object_path(path);
    let Ok(c_so_path) = CString::new(so_path.as_str()) else {
        return Err(NifLoadError::LoadFailed(format!(
            "{LOAD_SLOGAN}: invalid path (interior NUL byte): '{so_path}'"
        )));
    };

    // --- 1. dlopen --------------------------------------------------------------
    // SAFETY: `c_so_path` is a NUL-terminated path; `dlopen` accepts `RTLD_NOW | RTLD_LOCAL`.
    let handle =
        OpenHandle(unsafe { libc::dlopen(c_so_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) });
    if handle.get().is_null() {
        // `dlopen` failed, so the loader's error slot holds the reason. Both branches name the
        // *suffixed* path, as the C's `lib_name` does (`erl_nif.c:4802-4807`).
        let errdesc = dl_last_error().unwrap_or_else(|| "unknown dynamic loader error".to_string());
        let text = if errdesc.contains(&so_path) {
            format!("{LOAD_SLOGAN}: '{errdesc}'")
        } else {
            format!("{LOAD_SLOGAN} {so_path}: '{errdesc}'")
        };
        return Err(NifLoadError::LoadFailed(text));
    }

    // --- 2. nif_init / _nif_init ------------------------------------------------
    // SAFETY: `handle.get()` is a live `dlopen` handle; both names are NUL-terminated.
    let init_fn = unsafe {
        match dl_sym(handle.get(), b"nif_init\0") {
            Ok(sym) => sym,
            Err(_) => match dl_sym(handle.get(), b"_nif_init\0") {
                Ok(sym) => sym,
                Err(errdesc) => {
                    return Err(NifLoadError::BadLib(format!(
                        "Failed to find library init function: '{errdesc}'"
                    )));
                }
            },
        }
    };
    if init_fn.is_null() {
        return Err(NifLoadError::BadLib(format!(
            "Failed to find library init function: '{so_path}: undefined symbol: _nif_init'"
        )));
    }

    // --- 3. call nif_init() -----------------------------------------------------
    // SAFETY: `init_fn` is the library's `nif_init`/`_nif_init` symbol, whose C signature is
    // `ErlNifEntry* (void)`.
    let init: unsafe extern "C" fn() -> *mut ErlNifEntry = unsafe { core::mem::transmute(init_fn) };
    // SAFETY: `init` is the resolved, signature-correct init function.
    let entry = unsafe { init() };
    if entry.is_null() {
        return Err(NifLoadError::BadLib(
            "Library init-call unsuccessful".to_string(),
        ));
    }

    // --- 4-7. version / variant / module-name validation ------------------------
    validate_entry(entry, module)?;

    // --- function table ---------------------------------------------------------
    // SAFETY: `entry` is non-null and live (the library stays open until the returned library is
    // dropped).
    let (num_of_funcs, funcs_ptr) = unsafe { ((*entry).num_of_funcs.max(0), (*entry).funcs) };
    if num_of_funcs > 0 && funcs_ptr.is_null() {
        return Err(NifLoadError::BadLib(format!(
            "NIF library '{module}' declares {num_of_funcs} functions but its function table is NULL"
        )));
    }
    let mut funcs = Vec::with_capacity(num_of_funcs as usize);
    for i in 0..num_of_funcs as usize {
        // SAFETY: the `nif_init` contract guarantees `funcs` holds `num_of_funcs` entries.
        let f = unsafe { &*funcs_ptr.add(i) };
        funcs.push((read_cstring(f.name).unwrap_or_default(), f.arity));
    }

    // --- 8. the `load` callback -------------------------------------------------
    let mut priv_data: *mut c_void = core::ptr::null_mut();
    // SAFETY: `entry` is non-null and live (see above).
    if let Some(load_fn) = unsafe { (*entry).load } {
        // SAFETY: `host_ptr` was produced from a live `&mut dyn NifHost`; the borrow is reproduced
        // here for the duration of the callback only.
        let mut env = NifEnv::new(unsafe { &mut *host_ptr });
        // SAFETY: `load_fn` is the library's own `load` callback (C signature
        // `int (ErlNifEnv*, void**, ERL_NIF_TERM)`); `env` outlives the call.
        let veto = unsafe { load_fn(env.as_ptr(), &mut priv_data, nif_from_eterm(load_info)) };
        if veto != 0 {
            return Err(NifLoadError::Load(format!(
                "Library load-call unsuccessful ({veto})."
            )));
        }
    }

    Ok(NifLibrary {
        handle: handle.release(),
        entry,
        host: host_ptr,
        priv_data,
        module: module.to_string(),
        funcs,
    })
}

/// Validate the entry's version, vm variant and module name — the oracle's checks 6–9 in order
/// (`erl_nif.c:4820-4844`). Every failure is a `bad_lib`.
fn validate_entry(entry: *const ErlNifEntry, module: &str) -> Result<(), NifLoadError> {
    // SAFETY: `entry` is non-null and points at a live `ErlNifEntry` inside the loaded library.
    let (major, minor) = unsafe { ((*entry).major, (*entry).minor) };

    // Check 6: `major > 2 || (major == 2 && minor > 18)`.
    if major > ERL_NIF_MAJOR_VERSION
        || (major == ERL_NIF_MAJOR_VERSION && minor > ERL_NIF_MINOR_VERSION)
    {
        // SAFETY: `entry` is non-null and live.
        let min_erts =
            unsafe { read_cstring((*entry).min_erts) }.unwrap_or_else(|| "erts-?".to_string());
        return Err(NifLoadError::BadLib(format!(
            "That '{module}' NIF library needs {min_erts} or newer. Either try to recompile the NIF lib or use a newer erts runtime."
        )));
    }

    // Check 7: `major < 2 || (major == 2 && minor == 5)` (the experimental-maps release).
    if major < ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD || (major == 2 && minor == 5) {
        return Err(NifLoadError::BadLib(format!(
            "That old NIF library ({major}.{minor}) is not compatible with this erts runtime ({ERL_NIF_MAJOR_VERSION}.{ERL_NIF_MINOR_VERSION}). Try recompile the NIF lib."
        )));
    }

    // Check 8: `AT_LEAST_VERSION(entry, 2, 1) && vm_variant != "beam.vanilla"`. A null
    // `vm_variant` is treated as "not present" and skips the check (the C would fault).
    if at_least_version(major, minor, 2, 1) {
        // SAFETY: `entry` is non-null and live.
        if let Some(variant) = unsafe { read_cstring((*entry).vm_variant) } {
            if variant != ERL_NIF_VM_VARIANT {
                return Err(NifLoadError::BadLib(format!(
                    "Library ({variant}) not compiled for this vm variant ({ERL_NIF_VM_VARIANT})."
                )));
            }
        }
    }

    // Check 9: `entry->name` must equal the calling module.
    // SAFETY: `entry` is non-null and live.
    let name = unsafe { read_cstring((*entry).name) }.unwrap_or_default();
    if name != module {
        return Err(NifLoadError::BadLib(format!(
            "Library module name '{name}' does not match calling module '{module}'"
        )));
    }

    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The oracle's `AT_LEAST_VERSION` (`erl_nif.c:4580`).
fn at_least_version(major: c_int, minor: c_int, req_major: c_int, req_minor: c_int) -> bool {
    major * 0x100 + minor >= req_major * 0x100 + req_minor
}

/// `path` with the shared-object suffix appended — unconditionally, as the oracle does
/// (`erts_sys_ddll_open` copies `FILE_EXT` after the given name,
/// `erts/emulator/sys/unix/erl_unix_sys_ddll.c:104-118`), so `"/x/foo.so"` opens `"/x/foo.so.so"`.
fn shared_object_path(path: &str) -> String {
    format!("{path}{SHARED_OBJECT_SUFFIX}")
}

/// Read a NUL-terminated C string, returning `None` for a null pointer.
///
/// Invalid UTF-8 is replaced (never an error, never a panic).
fn read_cstring(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the `ErlNifEntry` contract guarantees a NUL-terminated C string at `ptr` (or null,
    // handled above); the bytes are copied out immediately.
    let cstr = unsafe { CStr::from_ptr(ptr) };
    Some(cstr.to_string_lossy().into_owned())
}

/// The pending dynamic-loader error, if any (reads and clears the loader's error slot).
fn dl_last_error() -> Option<String> {
    // SAFETY: `dlerror` takes no arguments and returns either NULL or a NUL-terminated string that
    // stays valid until the next `dl*` call; the string is copied out immediately.
    unsafe {
        let ptr = libc::dlerror();
        if ptr.is_null() {
            return None;
        }
        Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
    }
}

/// `dlsym` with the loader's own error detection (`dlerror` is non-NULL on failure), mirroring
/// `erts_sys_ddll_sym2` (`erl_unix_sys_ddll.c:163`). A null symbol *value* is therefore a success
/// if the loader reports no error; the caller null-checks separately.
///
/// # Safety
///
/// `handle` must be a live `dlopen` handle, and `name` must be NUL-terminated.
unsafe fn dl_sym(handle: *mut c_void, name: &[u8]) -> Result<*mut c_void, String> {
    // SAFETY: the caller guarantees `handle` is a live handle and `name` is NUL-terminated.
    unsafe {
        libc::dlerror(); // clear the loader's error slot
        let sym = libc::dlsym(handle, name.as_ptr() as *const c_char);
        match dl_last_error() {
            None => Ok(sym),
            Some(text) => Err(text),
        }
    }
}

/// RAII owner of a `dlopen` handle: closed on drop unless released into a [`NifLibrary`].
struct OpenHandle(*mut c_void);

impl OpenHandle {
    fn get(&self) -> *mut c_void {
        self.0
    }

    /// Hand the raw handle to the caller, disarming the drop.
    fn release(self) -> *mut c_void {
        let handle = self.0;
        core::mem::forget(self);
        handle
    }
}

impl Drop for OpenHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` came from a successful `dlopen` and has not been released.
            unsafe { libc::dlclose(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A non-null pointer that only ever has to round-trip (nothing dereferences it here).
    fn marker(addr: usize) -> *mut c_void {
        addr as *mut c_void
    }

    #[test]
    fn the_priv_data_stack_nests_and_restores_the_outer_library() {
        assert!(current_priv_data().is_none());
        let outer = CurrentPrivData::publish(marker(0x11));
        assert_eq!(current_priv_data(), Some(marker(0x11)));
        {
            let _inner = CurrentPrivData::publish(marker(0x22));
            assert_eq!(current_priv_data(), Some(marker(0x22)));
        }
        assert_eq!(
            current_priv_data(),
            Some(marker(0x11)),
            "leaving the nested call restores the enclosing library"
        );
        drop(outer);
        assert!(current_priv_data().is_none(), "the outermost call withdraws");
    }

    /// A published `NULL` is an answer in its own right (a library whose `load` stored nothing),
    /// distinct from "no library call is in progress".
    #[test]
    fn a_published_null_is_distinct_from_no_call() {
        let null = CurrentPrivData::publish(core::ptr::null_mut());
        assert_eq!(current_priv_data(), Some(core::ptr::null_mut()));
        drop(null);
        assert!(current_priv_data().is_none());
    }

    /// An unwind out of the call still withdraws the publication (`Drop` runs on unwind).
    #[test]
    fn the_publication_is_restored_on_unwind() {
        let _outer = CurrentPrivData::publish(marker(0x11));
        let unwound = std::panic::catch_unwind(|| {
            let _inner = CurrentPrivData::publish(marker(0x22));
            panic!("unwind out of the nested library call");
        });
        assert!(unwound.is_err(), "the panic propagated");
        assert_eq!(current_priv_data(), Some(marker(0x11)));
    }

    /// The publication is thread-local: a concurrent NIF call on another thread sees only its own.
    #[test]
    fn the_publication_is_invisible_to_other_threads() {
        let _outer = CurrentPrivData::publish(marker(0x11));
        std::thread::spawn(|| assert!(current_priv_data().is_none()))
            .join()
            .expect("the sibling thread observes an empty stack");
        assert_eq!(current_priv_data(), Some(marker(0x11)));
    }

    /// The oracle appends `FILE_EXT` unconditionally (`erl_unix_sys_ddll.c:104-118`), so an
    /// already-suffixed path resolves `<path>.so.so` — the measured OTP behaviour of
    /// `erlang:load_nif("/nonexistent/foo.so", [])`.
    #[test]
    fn shared_object_path_appends_the_suffix_unconditionally() {
        assert_eq!(shared_object_path("/tmp/x"), "/tmp/x.so");
        assert_eq!(shared_object_path("/tmp/x.so"), "/tmp/x.so.so");
        assert_eq!(shared_object_path("x"), "x.so");
        assert_eq!(shared_object_path(""), ".so");
    }

    #[test]
    fn at_least_version_matches_the_oracle_macro() {
        assert!(at_least_version(2, 18, 2, 1));
        assert!(at_least_version(2, 1, 2, 1));
        assert!(!at_least_version(2, 0, 2, 1));
        assert!(!at_least_version(1, 255, 2, 1));
        assert!(at_least_version(3, 0, 2, 18));
    }

    #[test]
    fn error_reason_atoms_and_texts_are_the_oracle_strings() {
        assert_eq!(
            NifLoadError::LoadFailed("t".into()).reason_atom(),
            "load_failed"
        );
        assert_eq!(NifLoadError::BadLib("t".into()).reason_atom(), "bad_lib");
        assert_eq!(NifLoadError::Reload("t".into()).reason_atom(), "reload");
        assert_eq!(NifLoadError::Load("t".into()).reason_atom(), "load");
        assert_eq!(NifLoadError::Upgrade("t".into()).reason_atom(), "upgrade");
        assert_eq!(NifLoadError::OldCode("t".into()).reason_atom(), "old_code");
        assert_eq!(NifLoadError::BadLib("boom".into()).text(), "boom");
        assert_eq!(
            NifLoadError::BadLib("boom".into()).to_string(),
            "bad_lib: boom"
        );
    }

    #[test]
    fn read_cstring_handles_null_and_valid_strings() {
        assert_eq!(read_cstring(core::ptr::null()), None);
        let s = b"hello\0";
        assert_eq!(
            read_cstring(s.as_ptr() as *const c_char).as_deref(),
            Some("hello")
        );
    }
}
