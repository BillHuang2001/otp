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

//! The dynamic-library primitive — a safe wrapper over `dlopen(3)`/`dlsym(3)`/`dlclose(3)`.
//!
//! This is the reusable, **safe** loading facility `beam_rt`'s `erl_ddll` floor builds on: open a
//! shared object, resolve a symbol, close it — with the `dlopen` mode, the `dlerror` idiom and the
//! error-atom vocabulary the oracle uses reproduced **exactly**, and every failure returned as a
//! typed [`DllError`] (never a panic, never a null dereference, never a swallowed error).
//!
//! The work is task **P1** (`erl_ddll`): `erl_ddll:load_driver/2` and friends are
//! registered-but-seam-backed in `beam_bifs` (they answer the typed `unsupported_bif`), and
//! `beam_rt`'s `erl_ddll` floor had no safe primitive to stand on. The two existing loaders
//! ([`beam_abi::driver::loader`], [`beam_abi::nif::loader`]) each call `libc::dlopen`/`dlsym`/
//! `dlclose` directly and are self-contained around their own C-ABI contract.
//!
//! # Why this lives in `beam_io` — and who may use it
//!
//! `beam_io` is the **lowest** crate of the workspace that owns a raw-pointer/OS facility a
//! `#![deny(unsafe_code)]` consumer cannot write itself (the same role [`crate::signal`] plays for
//! `os:set_signal/2`). `beam_io` already depends on `beam_abi`, so `beam_abi`'s own loaders
//! **cannot** use this module: a `beam_io → beam_abi` edge in the other direction would be a
//! dependency **cycle** (the workspace `../CONTEXT.md` §"Layering" DAG is acyclic by design). The
//! intended consumer is therefore `beam_rt`'s `erl_ddll`/driver-loading floor, which depends on
//! *both* crates. A future consolidation could instead move this wrapper **down** into the lower
//! crate `beam_abi` (which has no `beam_io` dependency) and re-export it here — see the crate
//! `CONTEXT.md` handoff note.
//!
//! # The `dlopen` mode (matched to the emulator, not guessed)
//!
//! The emulator opens a driver/NIF object with **`RTLD_NOW | RTLD_LOCAL`**:
//!
//! ```c
//! /* erts/emulator/sys/unix/erl_unix_sys_ddll.c:133 (erts_sys_ddll_open_noext) */
//! if ((*handle = dlopen(dlname, RTLD_NOW | RTLD_LOCAL)) == NULL) { ... }
//! ```
//!
//! Both in-tree Rust loaders hardcode the same pair — `beam_abi/src/driver/loader.rs` (which
//! cites `do_load_driver_entry`, `erl_bif_ddll.c:1474`) and `beam_abi/src/nif/loader.rs`
//! (`RTLD_NOW | RTLD_LOCAL`). This module **matches that exactly**: [`DynLibFlags::EMULATOR`] is
//! its default, so a caller that does not pass flags gets the oracle's behaviour. `RTLD_GLOBAL`
//! (adding the object's symbols to the global scope) is *available* via [`DynLibFlags::NOW_GLOBAL`]
//! but is deliberately **not** the default — the emulator uses `RTLD_LOCAL`, and a global scope
//! lets a later unrelated load accidentally bind to this object's symbols.
//!
//! `RTLD_NOW` also means every *undefined* symbol in the object is resolved eagerly at load time:
//! a `.so` that references, say, `driver_mk_port` fails `dlopen` outright unless the loading
//! executable exports that symbol (see the next section). That is why the emulator's driver/NIF
//! path sees "load failed" rather than a crash on first call.
//!
//! **`.so` suffix.** The emulator's `erts_sys_ddll_open` (`erl_unix_sys_ddll.c:107-125`) appends
//! `FILE_EXT` (`".so"`) to the name before calling `erts_sys_ddll_open_noext`. This primitive does
//! **not**: it is the raw `dlopen` wrapper, and the spelling policy (`<name>` vs `<name>.so`, a
//! candidate list, a code-path search) belongs to the caller — exactly as
//! [`beam_abi::driver::loader`] keeps the "append the suffix only when absent" rule to itself.
//!
//! # `-Wl,--export-dynamic` (a hard requirement, stated here once)
//!
//! A shared object loaded with `RTLD_NOW` leaves the `enif_*` (contract A.3) and `driver_*`/
//! `erl_drv_*` (contract A.4) symbols **undefined** and resolves them out of the *loading*
//! process's dynamic symbol table (`.dynsym`). Rust links test/binary targets with
//! `-Wl,--no-export-dynamic` by default, so those symbols are absent and
//! `dlopen(RTLD_NOW)` fails with `<path>: undefined symbol: driver_mk_port`. The loading
//! executable must therefore be linked with **`-Wl,--export-dynamic`** (the linker spelling of
//! `-rdynamic`) — this crate's `build.rs` emits exactly that for its test targets, and `beam_rt`
//! must do the same for the runtime binary. A load failure of this shape is reported as
//! [`DllErrorKind::Open`] with an `undefined symbol` text; [`DllError::is_undefined_symbol`]
//! recognises it.
//!
//! # `dlerror` handling (the C idiom, exactly)
//!
//! `dlerror(3)` returns the *first* error since the last call to `dlerror` (it also clears that
//! state). It is a single per-thread slot, so a **stale** error from an earlier, unrelated loader
//! call would be reported for a later success if the slot is not cleared first. Every fallible
//! entry point here therefore:
//!
//! 1. calls `dlerror()` to **clear** the slot,
//! 2. performs the loader call, and
//! 3. calls `dlerror()` again — a non-NULL result *is* the failure (and on `dlsym` it is the only
//!    reliable failure signal, because a symbol may legitimately resolve to address `0`).
//!
//! This mirrors `erts_sys_ddll_open_noext` and `erts_sys_ddll_sym2`
//! (`erl_unix_sys_ddll.c:127-183`).
//!
//! # Error taxonomy (the `erl_ddll` vocabulary, extracted from the C)
//!
//! A dynamic-library failure is classified as a [`DllError`], whose [`DllError::reason_atom`] is the
//! atom the oracle renders in `{error, Reason}`. The vocabulary is the emulator's `errcode_tab`
//! (`erl_bif_ddll.c:1709-1720`) plus the **dynamic** `open_error` case (`build_load_error_hp`,
//! `erl_bif_ddll.c:1770-1780`) and the `badarg` raise for a malformed argument (`pick_list_or_atom`,
//! `erl_bif_ddll.c:1792-1822`):
//!
//! | situation | reason | C citation |
//! |---|---|---|
//! | `dlopen` failed (missing file, not an ELF object, unresolved symbol) | `{open_error, Code}` | `erl_unix_sys_ddll.c:152` |
//! | `dlsym` failed | `symbol_not_found` (the table atom for `ERL_DE_LOOKUP_ERROR_NOT_FOUND`) | `erl_bif_ddll.c:1718` |
//! | object has no `driver_init` | `no_driver_init` | `erl_bif_ddll.c:1480` |
//! | `driver_init()` returned `NULL`, or the entry's `init` refused | `driver_init_failed` | `erl_bif_ddll.c:1486,1525` |
//! | `driver_name` mismatch | `bad_driver_name` | `erl_bif_ddll.c:1508` |
//! | name longer than the platform limit (checked on Windows only, `erl_win32_sys_ddll.c:88`) | `driver_name_to_long` | `erl_bif_ddll.c:1714` |
//! | version outside the accepted range | `driver_incorrect_version` | `erl_bif_ddll.c:1497,1503` |
//! | no `dlopen` on the platform | `no_ddll_available` | `erl_unix_sys_ddll.c:123,156` |
//! | any other dynamic error | `unspecified_error` | `erl_unix_sys_ddll.c:271` |
//! | malformed path/name argument | `badarg` (a raise) | `erl_bif_ddll.c:1806` |
//!
//! The dynamic `{open_error, Code}` code is an index into a **process-local** table of raw
//! `dlerror` strings (`find_errcode`/`ERL_DE_DYNAMIC_ERROR_OFFSET = -10`,
//! `erl_unix_sys_ddll.c:65-88,152`), so it cannot be computed out of process. This primitive
//! therefore hands up the **raw textual** `dlerror` output (see [`DllError::text`]); the runtime
//! floor owns the dynamic table (its index assignment, and the `{open_error, Code}` term) exactly
//! as `find_errcode` does. Classification is **text-based for the same reason the emulator's is**:
//! `dlopen` sets no `errno` for every failure shape (glibc leaves `errno` untouched for a truncated
//! file, and it is a *stale* value from any earlier call), so `errno` is deliberately not consulted
//! — the loader's own message, which the C also stores verbatim, is the only reliable signal.
//!
//! **Atoms that are *not* part of this vocabulary.** `badfile`, `not_a_driver`,
//! `undefined_symbol` and `not_supported` are **not** `erl_ddll` error atoms: they appear nowhere
//! in `errcode_tab` or `format_error_int`, and the oracle's own
//! `erl_ddll:format_error(badfile)` / `(undefined_symbol)` / `(not_a_driver)` / `(not_supported)`
//! each raise `badarg` instead of returning a string (verified against OTP 29.1.1 / erts-17.1,
//! `lib/kernel/src/erl_ddll.erl:1004-1012` → `erl_ddll_format_error_int_1`,
//! `erl_bif_ddll.c:813-876`). The three *situations* those names describe are still classifiable
//! here — a missing file, a format error and an unresolved symbol — through
//! [`DllError::is_missing_file`], [`DllError::is_format_error`] and
//! [`DllError::is_undefined_symbol`]; only the **atoms** differ.
//!
//! # Thread model
//!
//! [`DynLib`] holds its raw `dlopen` handle as a plain `*mut c_void`, so it is neither [`Send`] nor
//! [`Sync`] — the loader handle is used from the one OS thread that opened it, matching the
//! single-thread invariant the rest of this crate carries ([`crate::PortRegistry`]). This is the
//! conservative choice for the workspace's runtime model; POSIX does permit a handle's
//! `dlopen`/`dlsym`/`dlclose` calls to come from different threads.

use std::ffi::{CStr, CString, c_void};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// `ERL_DE_NO_ERROR` (`erts/emulator/beam/global.h:172`) — success.
pub const ERL_DE_NO_ERROR: i32 = 0;
/// `ERL_DE_LOAD_ERROR_NO_INIT` (`global.h:173`) — the object has no `driver_init`.
pub const ERL_DE_LOAD_ERROR_NO_INIT: i32 = -1;
/// `ERL_DE_LOAD_ERROR_FAILED_INIT` (`global.h:174`) — `driver_init` returned `NULL`.
pub const ERL_DE_LOAD_ERROR_FAILED_INIT: i32 = -2;
/// `ERL_DE_LOAD_ERROR_BAD_NAME` (`global.h:175`) — `driver_name` mismatch.
pub const ERL_DE_LOAD_ERROR_BAD_NAME: i32 = -3;
/// `ERL_DE_LOAD_ERROR_NAME_TO_LONG` (`global.h:176`) — the name exceeds the limit.
pub const ERL_DE_LOAD_ERROR_NAME_TO_LONG: i32 = -4;
/// `ERL_DE_LOAD_ERROR_INCORRECT_VERSION` (`global.h:177`) — `erl_driver.h` version mismatch.
pub const ERL_DE_LOAD_ERROR_INCORRECT_VERSION: i32 = -5;
/// `ERL_DE_ERROR_NO_DDLL_FUNCTIONALITY` (`global.h:178`) — no `dlopen` on this platform.
pub const ERL_DE_ERROR_NO_DDLL_FUNCTIONALITY: i32 = -6;
/// `ERL_DE_ERROR_UNSPECIFIED` (`global.h:179`) — an unspecified dynamic error.
pub const ERL_DE_ERROR_UNSPECIFIED: i32 = -7;
/// `ERL_DE_LOOKUP_ERROR_NOT_FOUND` (`global.h:180`) — a symbol lookup found nothing.
pub const ERL_DE_LOOKUP_ERROR_NOT_FOUND: i32 = -8;
/// `ERL_DE_DYNAMIC_ERROR_OFFSET` (`global.h:181`) — the base of the process-local
/// `{open_error, Code}` codes (`code = ERL_DE_DYNAMIC_ERROR_OFFSET - index`).
pub const ERL_DE_DYNAMIC_ERROR_OFFSET: i32 = -10;

/// The emulator's `errcode_tab` — `erl_bif_ddll.c:1709-1720`, byte-for-byte (atom, code).
///
/// This is the table `build_load_error_hp` (`erl_bif_ddll.c:1770-1780`) searches to render a
/// numeric code as an atom; a code absent from it becomes the tuple `{open_error, Code}` instead.
/// It is exposed so the runtime's `erl_ddll` floor renders `{error, Atom}` from the same data the
/// emulator does, rather than re-typing it.
pub const ERRCODE_TAB: &[(&str, i32)] = &[
    ("no_error", ERL_DE_NO_ERROR),
    ("no_driver_init", ERL_DE_LOAD_ERROR_NO_INIT),
    ("driver_init_failed", ERL_DE_LOAD_ERROR_FAILED_INIT),
    ("bad_driver_name", ERL_DE_LOAD_ERROR_BAD_NAME),
    ("driver_name_to_long", ERL_DE_LOAD_ERROR_NAME_TO_LONG),
    ("driver_incorrect_version", ERL_DE_LOAD_ERROR_INCORRECT_VERSION),
    ("no_ddll_available", ERL_DE_ERROR_NO_DDLL_FUNCTIONALITY),
    ("unspecified_error", ERL_DE_ERROR_UNSPECIFIED),
    ("symbol_not_found", ERL_DE_LOOKUP_ERROR_NOT_FOUND),
];

/// The atom for a numeric code — `build_load_error_hp`, `erl_bif_ddll.c:1770-1780`.
///
/// Returns `Some(atom)` for a code in [`ERRCODE_TAB`], `None` for anything else — the C's signal
/// to build the tuple `{open_error, Code}` instead of a bare atom.
#[must_use]
pub fn errcode_to_atom(code: i32) -> Option<&'static str> {
    ERRCODE_TAB
        .iter()
        .find_map(|&(atom, c)| (c == code).then_some(atom))
}

/// The numeric code for an atom — `errdesc_to_code`, `erl_bif_ddll.c:1722-1745`.
///
/// Returns `Some(code)` for an atom in [`ERRCODE_TAB`], `None` otherwise (the C then rejects the
/// whole `erl_ddll:format_error/1` call with `badarg`). The C compares the atom's **bytes** with
/// each table name byte-for-byte, so this comparison is exact (no case folding, no trimming).
#[must_use]
pub fn atom_to_errcode(atom: &str) -> Option<i32> {
    ERRCODE_TAB
        .iter()
        .find_map(|&(name, code)| (name == atom).then_some(code))
}

/// The human-readable text for a table atom — `erts_ddll_error`, `erl_bif_ddll.c:1201-1224`.
///
/// Returns `None` for an atom outside [`ERRCODE_TAB`] *and* for the `{open_error, Code}` case,
/// whose text is a raw `dlerror` string held in the loader's process-local table (see the module
/// docs) — so a caller never mistakes a missing translation for a translation.
#[must_use]
pub fn format_error_text(atom: &str) -> Option<&'static str> {
    match atom_to_errcode(atom)? {
        ERL_DE_NO_ERROR => Some("No error"),
        ERL_DE_LOAD_ERROR_NO_INIT => Some("No driver init in dynamic library"),
        ERL_DE_LOAD_ERROR_FAILED_INIT => Some("Driver init failed"),
        ERL_DE_LOAD_ERROR_BAD_NAME => Some("Bad driver name"),
        ERL_DE_LOAD_ERROR_NAME_TO_LONG => Some("Driver name to long"),
        ERL_DE_LOAD_ERROR_INCORRECT_VERSION => {
            Some("Driver compiled with incorrect version of erl_driver.h")
        }
        ERL_DE_ERROR_NO_DDLL_FUNCTIONALITY => {
            Some("DDLL functionality not available on this platform")
        }
        ERL_DE_ERROR_UNSPECIFIED => Some("Unspecified dynamic library error"),
        ERL_DE_LOOKUP_ERROR_NOT_FOUND => Some("Symbol not found in dynamic library"),
        _ => None,
    }
}

/// The `dlopen(3)` mode: a binding mode (`RTLD_NOW`/`RTLD_LAZY`) plus a symbol-visibility scope
/// (`RTLD_LOCAL`/`RTLD_GLOBAL`).
///
/// A `#[repr(transparent)]` newtype over the raw `c_int` bits, so a mode outside the two axes can
/// still be built with [`DynLibFlags::from_bits`] when a caller needs one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct DynLibFlags(i32);

impl DynLibFlags {
    /// `RTLD_NOW | RTLD_LOCAL` — the emulator's mode for driver/NIF loading
    /// (`erl_unix_sys_ddll.c:133`), and this module's default.
    pub const EMULATOR: DynLibFlags = DynLibFlags(libc::RTLD_NOW | libc::RTLD_LOCAL);

    /// `RTLD_NOW | RTLD_GLOBAL` — eager binding, symbols added to the global scope.
    pub const NOW_GLOBAL: DynLibFlags = DynLibFlags(libc::RTLD_NOW | libc::RTLD_GLOBAL);

    /// `RTLD_LAZY | RTLD_LOCAL` — function symbols resolved on first use.
    pub const LAZY_LOCAL: DynLibFlags = DynLibFlags(libc::RTLD_LAZY | libc::RTLD_LOCAL);

    /// `RTLD_LAZY | RTLD_GLOBAL` — lazy binding, symbols added to the global scope.
    pub const LAZY_GLOBAL: DynLibFlags = DynLibFlags(libc::RTLD_LAZY | libc::RTLD_GLOBAL);

    /// Wrap raw `dlopen(3)` mode bits (e.g. a platform-specific flag this newtype does not name).
    #[must_use]
    pub const fn from_bits(bits: i32) -> DynLibFlags {
        DynLibFlags(bits)
    }

    /// The raw mode bits handed to `dlopen(3)`.
    #[must_use]
    pub const fn bits(self) -> i32 {
        self.0
    }
}

impl Default for DynLibFlags {
    /// [`DynLibFlags::EMULATOR`] — the oracle's mode (`RTLD_NOW | RTLD_LOCAL`).
    fn default() -> Self {
        DynLibFlags::EMULATOR
    }
}

/// A handle to a loaded shared object.
///
/// [`DynLib::drop`] calls `dlclose(3)`; the handle is never used after that and never moved into
/// another owner (there is deliberately no `into_raw`/`from_raw` escape hatch — the runtime needs
/// an *owned* handle whose lifetime it can rely on, which is the whole point of the facility).
///
/// Not [`Send`]/[`Sync`] (it holds a raw pointer): open and use it from one OS thread — see the
/// module docs.
pub struct DynLib {
    /// The live `dlopen(3)` handle. Never null (a null result is turned into a [`DllError`] before
    /// a `DynLib` is constructed).
    handle: *mut c_void,
}

impl DynLib {
    /// Open the shared object `path` with the emulator's mode ([`DynLibFlags::EMULATOR`],
    /// `RTLD_NOW | RTLD_LOCAL`).
    ///
    /// `path` is used **verbatim** — no `.so` suffix is appended and no search path is implied
    /// beyond `dlopen`'s own (see the module docs).
    ///
    /// # Errors
    ///
    /// [`DllErrorKind::BadArg`] when `path` is not a valid C string (an interior NUL);
    /// [`DllErrorKind::Open`] when `dlopen` fails (a missing file, a non-ELF file, an unresolved
    /// symbol under `RTLD_NOW`, a permission error, …) — carrying the raw `dlerror` text and the
    /// `open_error` reason atom.
    pub fn open(path: &Path) -> Result<DynLib, DllError> {
        DynLib::open_with(path, DynLibFlags::EMULATOR)
    }

    /// Open the shared object `path` with an explicit `dlopen` mode.
    ///
    /// See [`DynLib::open`] for the errors; the `Unspecified` note there applies to the raw bits
    /// too (a mode `dlopen` rejects is reported by the loader, not pre-validated here).
    pub fn open_with(path: &Path, flags: DynLibFlags) -> Result<DynLib, DllError> {
        let name = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            DllError::badarg(format!("path contains an interior NUL byte: {}", path.display()))
        })?;
        DynLib::open_cstr(&name, flags)
    }

    /// Open the shared object named by `name` (an already-NUL-terminated C string) with `flags`.
    ///
    /// This is the form `dlopen` itself takes: `name` may be an absolute or relative path, or a
    /// bare soname (`libc.so.6`) for the loader to search.
    ///
    /// # Errors
    ///
    /// [`DllErrorKind::Open`] when `dlopen` fails, with the raw `dlerror` text.
    pub fn open_cstr(name: &CStr, flags: DynLibFlags) -> Result<DynLib, DllError> {
        clear_dl_error();
        // SAFETY: `name` is a valid NUL-terminated C string for the whole call (it outlives it)
        // and `flags.bits()` is a plain integer; `dlopen` only reads the string and returns either
        // NULL (reported through `dlerror`) or an owned handle this struct closes once in `Drop`.
        let handle = unsafe { libc::dlopen(name.as_ptr(), flags.bits()) };
        if handle.is_null() {
            return Err(DllError::open(take_dl_error()));
        }
        Ok(DynLib { handle })
    }

    /// Resolve the symbol `name` in this object.
    ///
    /// A missing symbol is a typed [`DllErrorKind::SymbolNotFound`] error — **never** a null
    /// pointer handed to the caller. The `dlsym` contract is that a *successful* resolution may
    /// still yield address `0` (an absolute/weak symbol), so the failure signal is `dlerror`, not
    /// the pointer value: this reads `dlerror` after the call exactly as
    /// `erts_sys_ddll_sym2` does (`erl_unix_sys_ddll.c:163-181`).
    ///
    /// # Errors
    ///
    /// [`DllErrorKind::SymbolNotFound`] when the object has no such symbol (the reason atom is
    /// `symbol_not_found`, [`format_error_text`] → "Symbol not found in dynamic library").
    pub fn symbol(&self, name: &CStr) -> Result<*mut c_void, DllError> {
        clear_dl_error();
        // SAFETY: `self.handle` is a live `dlopen` handle (owned by `self`, closed only in `Drop`)
        // and `name` is a valid NUL-terminated C string for the whole call; `dlsym` only reads the
        // string and returns a symbol address (or NULL, distinguished by `dlerror`).
        let sym = unsafe { libc::dlsym(self.handle, name.as_ptr()) };
        let error = take_dl_error();
        if error.is_empty() {
            // `dlerror` reported nothing, so the resolution succeeded — even if the address is 0.
            Ok(sym)
        } else {
            Err(DllError::symbol_not_found(name.to_string_lossy().into_owned(), error))
        }
    }

    /// Resolve `name`, returning `None` when the object has no such symbol.
    ///
    /// The `Option`-shaped convenience over [`DynLib::symbol`]. A successful resolution to address
    /// `0` is still `Some(null)` (the symbol exists); only a real lookup failure is `None`.
    #[must_use]
    pub fn try_symbol(&self, name: &CStr) -> Option<*mut c_void> {
        self.symbol(name).ok()
    }

    /// The underlying `dlopen(3)` handle.
    ///
    /// Exposed for a caller that must hand the raw handle to a C API (`dlvsym`, say). The handle
    /// is owned by `self`: it must not be `dlclose`d by the caller, and it is invalid once this
    /// `DynLib` is dropped.
    #[must_use]
    pub fn handle(&self) -> *mut c_void {
        self.handle
    }
}

impl Drop for DynLib {
    fn drop(&mut self) {
        // SAFETY: `self.handle` came from a successful `dlopen` and, by construction, is closed
        // exactly once here (there is no `Clone`, no `into_raw`, and no other `dlclose` call in
        // this module). `dlclose`'s return value reports whether the object's destructors ran
        // cleanly; there is no way to report it from `Drop`, and the handle must be released
        // regardless, so it is intentionally ignored.
        unsafe { libc::dlclose(self.handle) };
    }
}

impl fmt::Debug for DynLib {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DynLib").field("handle", &self.handle).finish()
    }
}

/// The classification of a dynamic-library failure — the emulator's `erl_ddll` reason vocabulary.
///
/// Each variant's atom is [`DllErrorKind::reason_atom`]; see the module docs for the C citation of
/// every case. [`DllErrorKind::Open`] is the only one a `dlopen` failure produces; the driver- and
/// NIF-level variants are the rest of the vocabulary a load path (the runtime's `erl_ddll` floor)
/// renders `{error, Atom}` from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DllErrorKind {
    /// `dlopen` failed — the dynamic `{open_error, Code}` case
    /// (`erl_unix_sys_ddll.c:152`). The `Code` is a process-local index this primitive cannot
    /// compute, so the raw `dlerror` text is carried instead (see the module docs).
    Open,
    /// `dlsym` failed — `symbol_not_found` (`erl_bif_ddll.c:1718`).
    SymbolNotFound,
    /// The object has no `driver_init` — `no_driver_init` (`erl_bif_ddll.c:1480`).
    NoDriverInit,
    /// `driver_init()` returned `NULL`, or the entry's `init` callback refused — `driver_init_failed`
    /// (`erl_bif_ddll.c:1486,1525`).
    DriverInitFailed,
    /// The requested name does not match the entry's `driver_name` — `bad_driver_name`
    /// (`erl_bif_ddll.c:1508`).
    BadDriverName,
    /// The name is longer than the platform's limit — `driver_name_to_long`
    /// (`erl_bif_ddll.c:1714`; the check itself is Windows-only, `erl_win32_sys_ddll.c:88`).
    DriverNameTooLong,
    /// The entry was compiled against an incompatible `erl_driver.h` — `driver_incorrect_version`
    /// (`erl_bif_ddll.c:1497,1503`).
    DriverIncorrectVersion,
    /// The platform has no dynamic loader — `no_ddll_available` (`erl_unix_sys_ddll.c:123,156`).
    NoDdllAvailable,
    /// Any other dynamic error — `unspecified_error` (`erl_unix_sys_ddll.c:271`).
    Unspecified,
    /// A malformed path/name argument — the `badarg` raise (`erl_bif_ddll.c:1806`), not an
    /// `{error, Atom}` return.
    BadArg,
}

impl DllErrorKind {
    /// The `erl_ddll` reason atom the oracle renders for this failure.
    ///
    /// [`DllErrorKind::Open`] yields `open_error` — the atom member of the `{open_error, Code}`
    /// tuple (the code itself is the loader's process-local index; see the module docs).
    /// [`DllErrorKind::BadArg`] yields `badarg`, which the oracle **raises** rather than returns.
    #[must_use]
    pub const fn reason_atom(self) -> &'static str {
        match self {
            DllErrorKind::Open => "open_error",
            DllErrorKind::SymbolNotFound => "symbol_not_found",
            DllErrorKind::NoDriverInit => "no_driver_init",
            DllErrorKind::DriverInitFailed => "driver_init_failed",
            DllErrorKind::BadDriverName => "bad_driver_name",
            DllErrorKind::DriverNameTooLong => "driver_name_to_long",
            DllErrorKind::DriverIncorrectVersion => "driver_incorrect_version",
            DllErrorKind::NoDdllAvailable => "no_ddll_available",
            DllErrorKind::Unspecified => "unspecified_error",
            DllErrorKind::BadArg => "badarg",
        }
    }

    /// The numeric code [`ERRCODE_TAB`] assigns to this kind, or `None` for a kind outside the
    /// table ([`DllErrorKind::Open`]'s `{open_error, Code}` and [`DllErrorKind::BadArg`]'s raise).
    #[must_use]
    pub const fn errcode(self) -> Option<i32> {
        match self {
            DllErrorKind::NoDriverInit => Some(ERL_DE_LOAD_ERROR_NO_INIT),
            DllErrorKind::DriverInitFailed => Some(ERL_DE_LOAD_ERROR_FAILED_INIT),
            DllErrorKind::BadDriverName => Some(ERL_DE_LOAD_ERROR_BAD_NAME),
            DllErrorKind::DriverNameTooLong => Some(ERL_DE_LOAD_ERROR_NAME_TO_LONG),
            DllErrorKind::DriverIncorrectVersion => Some(ERL_DE_LOAD_ERROR_INCORRECT_VERSION),
            DllErrorKind::NoDdllAvailable => Some(ERL_DE_ERROR_NO_DDLL_FUNCTIONALITY),
            DllErrorKind::Unspecified => Some(ERL_DE_ERROR_UNSPECIFIED),
            DllErrorKind::SymbolNotFound => Some(ERL_DE_LOOKUP_ERROR_NOT_FOUND),
            DllErrorKind::Open | DllErrorKind::BadArg => None,
        }
    }
}

/// A dynamic-library failure: a [`DllErrorKind`] plus the loader's own diagnostic text.
///
/// This is deliberately a **crate-local** error type rather than a [`BeamError`](beam_common::BeamError),
/// matching [`beam_abi::driver::loader::DriverLoadError`]/[`beam_abi::nif::loader::NifLoadError`]:
/// the failure is a *reason atom + text* pair, and the runtime's BIF floor must build the
/// `{error, Reason}` term itself (it needs the atom table), so an early conversion into a
/// `BeamError::Raise` would discard both the text and the distinction between a raise (`badarg`)
/// and an `{error, Atom}` return.
///
/// Use [`DllError::reason_atom`] for the `{error, Atom}` render, [`DllError::text`] for the
/// message, and the `is_*` predicates to recognise the three loader-failure shapes a caller
/// usually branches on.
#[derive(Clone, PartialEq, Eq)]
pub struct DllError {
    kind: DllErrorKind,
    text: String,
}

impl DllError {
    /// A `dlopen` failure ([`DllErrorKind::Open`]) with the raw `dlerror` text.
    fn open(text: String) -> DllError {
        DllError { kind: DllErrorKind::Open, text }
    }

    /// A `dlsym` failure ([`DllErrorKind::SymbolNotFound`]).
    fn symbol_not_found(name: String, text: String) -> DllError {
        DllError {
            kind: DllErrorKind::SymbolNotFound,
            text: format!("{name}: {text}"),
        }
    }

    /// A malformed-argument failure ([`DllErrorKind::BadArg`]).
    fn badarg(text: String) -> DllError {
        DllError { kind: DllErrorKind::BadArg, text }
    }

    /// The classification of this failure.
    #[must_use]
    pub fn kind(&self) -> DllErrorKind {
        self.kind
    }

    /// The `erl_ddll` reason atom the oracle renders (`{error, Atom}`, or the `badarg` raise).
    #[must_use]
    pub fn reason_atom(&self) -> &'static str {
        self.kind.reason_atom()
    }

    /// The loader's diagnostic text — the raw `dlerror(3)` string for an [`DllErrorKind::Open`]
    /// failure (which is what the oracle's `{open_error, Code}` code indexes), and a
    /// `<name>: <dlerror>` form for a symbol miss.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the failure is **a missing file** — the situation the object could not be found.
    ///
    /// Recognised from the loader's own message (`No such file or directory`), which is what the
    /// emulator stores verbatim in its dynamic error table. Note that `erl_ddll` has no `badfile`
    /// atom (see the module docs): the runtime floor renders this as `{open_error, Code}`.
    ///
    /// Classification is text-based (never `errno`) because `dlopen` does not set `errno` for every
    /// failure shape and a *stale* value from an earlier unrelated syscall would otherwise be
    /// mistaken for this one — the loader's message is the only reliable signal, and it is the same
    /// signal the C keeps.
    #[must_use]
    pub fn is_missing_file(&self) -> bool {
        self.kind == DllErrorKind::Open && self.text.contains("No such file or directory")
    }

    /// Whether the failure is **an unresolved symbol** — a symbol the object (or a `dlsym` lookup)
    /// could not bind.
    ///
    /// True for an [`DllErrorKind::Open`] failure whose text is the loader's `undefined symbol: …`
    /// (an `RTLD_NOW` load of an object with an unresolved reference — see the
    /// `-Wl,--export-dynamic` note in the module docs) **and** for a [`DllErrorKind::SymbolNotFound`]
    /// lookup miss, whose reason atom is `symbol_not_found`.
    #[must_use]
    pub fn is_undefined_symbol(&self) -> bool {
        self.text.contains("undefined symbol")
    }

    /// Whether the failure is **a format error** — the file exists but is not a loadable object.
    ///
    /// Recognised from the loader's own message: glibc's format complaints (`file too short`,
    /// `invalid ELF header`, `wrong ELF class`, `cannot read file data`). `dlopen` sets no portable
    /// error code for this case, which is exactly why the emulator keeps the raw `dlerror` string
    /// in a process-local table.
    #[must_use]
    pub fn is_format_error(&self) -> bool {
        self.kind == DllErrorKind::Open
            && (self.text.contains("invalid ELF header")
                || self.text.contains("file too short")
                || self.text.contains("cannot read file data")
                || self.text.contains("wrong ELF class"))
    }
}

impl fmt::Display for DllError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason_atom(), self.text)
    }
}

impl fmt::Debug for DllError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DllError")
            .field("kind", &self.kind)
            .field("reason_atom", &self.reason_atom())
            .field("text", &self.text)
            .finish()
    }
}

impl std::error::Error for DllError {}

/// Clear the loader's per-thread error slot (step 1 of the `dlerror` idiom — see the module docs).
fn clear_dl_error() {
    // SAFETY: `dlerror` takes no arguments and has no preconditions; its only effect here is to
    // clear the per-thread error slot, which is exactly what the C idiom needs before a call.
    unsafe { libc::dlerror() };
}

/// Read (and thereby clear) the loader's per-thread error slot; `""` when it holds nothing.
///
/// `dlerror` returns a pointer into loader-owned storage that is valid only until the next loader
/// call, so the bytes are copied into an owned `String` before returning.
fn take_dl_error() -> String {
    // SAFETY: `dlerror` takes no arguments and has no preconditions. Its result is either NULL (no
    // pending error) or a valid NUL-terminated string owned by the loader, which is copied out
    // immediately and never retained as a pointer.
    let ptr = unsafe { libc::dlerror() };
    if ptr.is_null() {
        String::new()
    } else {
        // SAFETY: non-null result of `dlerror` is a valid NUL-terminated string per its contract.
        unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
    }
}

/// Convert a Rust `str` to a [`CString`] for a symbol name, mapping an interior NUL to a [`DllError`].
///
/// A convenience for the (very common) case of resolving a literal symbol name; the fallible entry
/// points themselves take a `&CStr` so no allocation is imposed on the caller.
///
/// # Errors
///
/// [`DllErrorKind::BadArg`] when `name` contains an interior NUL byte.
pub fn symbol_name(name: &str) -> Result<CString, DllError> {
    CString::new(name)
        .map_err(|_| DllError::badarg(format!("symbol name contains an interior NUL byte: {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A candidate list of shared objects present on essentially every Linux/macOS host, in
    /// preference order. The tests skip **loudly** when none of them is `dlopen`-able (a musl or
    /// otherwise unusual layout) instead of asserting on an unrelated failure.
    const SYSTEM_LIBS: &[&str] = &["libc.so.6", "libm.so.6", "libdl.so.2", "libSystem.B.dylib"];

    /// Open the first loadable system library; `None` (with a loud banner) when none loads.
    fn open_system_lib() -> Option<DynLib> {
        for name in SYSTEM_LIBS {
            let c = CString::new(*name).expect("no NUL in a literal");
            match DynLib::open_cstr(&c, DynLibFlags::EMULATOR) {
                Ok(lib) => return Some(lib),
                Err(_) => continue,
            }
        }
        eprintln!("SKIP: none of {SYSTEM_LIBS:?} could be dlopen-ed on this host");
        None
    }

    #[test]
    fn the_emulator_flags_are_rtld_now_local() {
        assert_eq!(
            DynLibFlags::EMULATOR.bits(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
            "the default mode must be the emulator's (erl_unix_sys_ddll.c:133)"
        );
        assert_eq!(DynLibFlags::default(), DynLibFlags::EMULATOR);
        assert_eq!(DynLibFlags::NOW_GLOBAL.bits(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
        assert_eq!(DynLibFlags::LAZY_LOCAL.bits(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        assert_eq!(DynLibFlags::from_bits(7).bits(), 7);
    }

    #[test]
    fn the_errcode_table_is_the_emulators_exactly() {
        // The whole table, in the order of `erl_bif_ddll.c:1709-1720`.
        assert_eq!(
            ERRCODE_TAB,
            &[
                ("no_error", 0),
                ("no_driver_init", -1),
                ("driver_init_failed", -2),
                ("bad_driver_name", -3),
                ("driver_name_to_long", -4),
                ("driver_incorrect_version", -5),
                ("no_ddll_available", -6),
                ("unspecified_error", -7),
                ("symbol_not_found", -8),
            ]
        );
        assert_eq!(errcode_to_atom(-1), Some("no_driver_init"));
        assert_eq!(errcode_to_atom(-8), Some("symbol_not_found"));
        assert_eq!(errcode_to_atom(-10), None, "a dynamic code has no table atom");
        assert_eq!(atom_to_errcode("bad_driver_name"), Some(-3));
        assert_eq!(atom_to_errcode("badfile"), None);
        assert_eq!(atom_to_errcode("undefined_symbol"), None);
        assert_eq!(atom_to_errcode("not_a_driver"), None);
        assert_eq!(atom_to_errcode("not_supported"), None);
    }

    #[test]
    fn format_error_text_reproduces_erts_ddll_error() {
        assert_eq!(format_error_text("no_error"), Some("No error"));
        assert_eq!(format_error_text("no_driver_init"), Some("No driver init in dynamic library"));
        assert_eq!(format_error_text("driver_init_failed"), Some("Driver init failed"));
        assert_eq!(format_error_text("bad_driver_name"), Some("Bad driver name"));
        assert_eq!(format_error_text("driver_name_to_long"), Some("Driver name to long"));
        assert_eq!(
            format_error_text("driver_incorrect_version"),
            Some("Driver compiled with incorrect version of erl_driver.h")
        );
        assert_eq!(
            format_error_text("no_ddll_available"),
            Some("DDLL functionality not available on this platform")
        );
        assert_eq!(
            format_error_text("unspecified_error"),
            Some("Unspecified dynamic library error")
        );
        assert_eq!(
            format_error_text("symbol_not_found"),
            Some("Symbol not found in dynamic library")
        );
        assert_eq!(format_error_text("open_error"), None, "open_error carries raw dlerror text");
        assert_eq!(format_error_text("badarg"), None);
    }

    #[test]
    fn kinds_map_to_their_atoms_and_codes() {
        let cases: &[(DllErrorKind, &str, Option<i32>)] = &[
            (DllErrorKind::Open, "open_error", None),
            (DllErrorKind::SymbolNotFound, "symbol_not_found", Some(-8)),
            (DllErrorKind::NoDriverInit, "no_driver_init", Some(-1)),
            (DllErrorKind::DriverInitFailed, "driver_init_failed", Some(-2)),
            (DllErrorKind::BadDriverName, "bad_driver_name", Some(-3)),
            (DllErrorKind::DriverNameTooLong, "driver_name_to_long", Some(-4)),
            (DllErrorKind::DriverIncorrectVersion, "driver_incorrect_version", Some(-5)),
            (DllErrorKind::NoDdllAvailable, "no_ddll_available", Some(-6)),
            (DllErrorKind::Unspecified, "unspecified_error", Some(-7)),
            (DllErrorKind::BadArg, "badarg", None),
        ];
        for &(kind, atom, code) in cases {
            assert_eq!(kind.reason_atom(), atom);
            assert_eq!(kind.errcode(), code, "{atom}: code");
            // Every table code round-trips through the table as well.
            if let Some(code) = code {
                assert_eq!(errcode_to_atom(code), Some(atom));
            }
        }
    }

    #[test]
    fn a_known_symbol_resolves_and_a_missing_one_is_a_typed_error() {
        let Some(lib) = open_system_lib() else { return };
        let name = CString::new("malloc").unwrap();
        let sym = lib.symbol(&name).expect("libc/libm export malloc");
        assert!(!sym.is_null(), "malloc is a real symbol");

        let missing = CString::new("beam_io_ddll_no_such_symbol_probe").unwrap();
        let err = lib.symbol(&missing).expect_err("a bogus symbol must not resolve");
        assert_eq!(err.kind(), DllErrorKind::SymbolNotFound);
        assert_eq!(err.reason_atom(), "symbol_not_found");
        assert_eq!(
            format_error_text(err.reason_atom()),
            Some("Symbol not found in dynamic library")
        );
        assert!(err.text().contains("beam_io_ddll_no_such_symbol_probe"), "{}", err.text());
        assert!(err.is_undefined_symbol(), "the loader text says `undefined symbol`: {}", err);
        assert!(lib.try_symbol(&missing).is_none());
        assert!(lib.try_symbol(&name).is_some());
    }

    #[test]
    fn a_missing_file_is_the_open_error_atom() {
        let path = Path::new("/nonexistent/beam_io-ddll-does-not-exist.so");
        let err = DynLib::open(path).expect_err("a nonexistent object must not open");
        assert_eq!(err.kind(), DllErrorKind::Open);
        // The emulator renders a `dlopen` failure as `{open_error, Code}` — there is no `badfile`
        // in its vocabulary (see the module docs).
        assert_eq!(err.reason_atom(), "open_error");
        assert!(err.is_missing_file(), "ENOENT or the loader text must be recognised: {err}");
        assert!(!err.is_format_error());
        assert!(
            err.text().contains("does-not-exist") || err.text().contains("No such file"),
            "the loader text should name the failure: {}",
            err.text()
        );
    }

    #[test]
    fn a_path_with_an_interior_nul_is_badarg() {
        let path = Path::new("/tmp/beam\0io.so");
        let err = DynLib::open(path).expect_err("an interior NUL cannot be a path");
        assert_eq!(err.kind(), DllErrorKind::BadArg);
        assert_eq!(err.reason_atom(), "badarg");
    }

    #[test]
    fn a_second_open_of_the_same_library_works_and_dropping_closes_cleanly() {
        let Some(first) = open_system_lib() else { return };
        let name = CString::new("getpid").unwrap();
        assert!(first.symbol(&name).is_ok());
        drop(first);
        // A fresh open after the first handle was closed must still succeed (no leaked loader
        // state, no double-close).
        let second = open_system_lib().expect("reopening after a drop must work");
        assert!(second.symbol(&name).is_ok());
    }

    #[test]
    fn an_empty_symbol_name_reports_the_dlerror_text() {
        let Some(lib) = open_system_lib() else { return };
        let empty = CString::new("").unwrap();
        // An empty name is not a valid symbol; the loader reports it and the error carries a text.
        match lib.symbol(&empty) {
            Ok(_) => {} // some loaders treat it permissively; not a failure of this wrapper
            Err(err) => {
                assert_eq!(err.kind(), DllErrorKind::SymbolNotFound);
                assert!(!err.text().is_empty());
            }
        }
    }

    #[test]
    fn symbol_name_rejects_an_interior_nul_and_accepts_a_plain_name() {
        assert_eq!(symbol_name("driver_init").unwrap().to_str().unwrap(), "driver_init");
        let err = symbol_name("bad\0name").expect_err("interior NUL");
        assert_eq!(err.kind(), DllErrorKind::BadArg);
    }

    #[test]
    fn the_size_predicates_do_not_over_fire() {
        let symbol = DllError::symbol_not_found("x".into(), "…: undefined symbol: x".into());
        assert!(symbol.is_undefined_symbol());
        assert!(!symbol.is_missing_file());
        assert!(!symbol.is_format_error());

        let other = DllError::open("something else entirely".into());
        assert!(!other.is_missing_file());
        assert!(!other.is_undefined_symbol());
        assert!(!other.is_format_error());
    }
}
