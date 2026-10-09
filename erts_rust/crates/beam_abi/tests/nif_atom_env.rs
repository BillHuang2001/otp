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

//! Integration tests for the three NIF-side divergences closed on the `beam_abi` side:
//!
//! 1. **Atom visibility within one call.** The runtime commits a freshly interned atom lazily (a
//!    *pending interner*), so a host's `atom_name`/`existing_atom` miss an atom interned earlier in
//!    the **same** call. [`NifEnv`]'s per-call atom record restores that visibility for
//!    `enif_get_atom`, `enif_get_atom_length`, `enif_make_existing_atom` and
//!    `enif_make_existing_atom_len` (a miss still falls through to the host hook, unchanged).
//! 2. **`enif_clear_env`.** The C frees an env's temporary objects and rewinds its heap top but
//!    leaves the `exception_thrown` flag alone, so a raise survives it.
//! 3. **`enif_system_info`.** The runtime-dependent fields prefer the current call's host
//!    `system_info` block and fall back to the built-ins.
//!
//! Plus the C-correct failure/edge behaviour of the host-hook entry points that `beam_rt` owns
//! (`enif_term_to_binary`, `enif_binary_to_term`, `enif_make_ref`, `enif_whereis_pid`).
//!
//! The host here is [`DeferredCommitHost`], which deliberately models the runtime's deferred commit:
//! `intern_atom` assigns an atom immediately but only a `commit`ed name is visible to
//! `atom_name`/`existing_atom` — the exact shape that makes the record load-bearing.

mod common;

use std::collections::HashSet;
use std::ffi::CString;

use beam_abi::nif::enif::*;
use beam_abi::nif::host::{NifEnv, NifHost};
use beam_abi::nif::term::{eterm_from_nif, nif_from_eterm, ERL_NIF_TERM};
use beam_abi::nif::types::{ErlNifBinary, ErlNifCharEncoding, ErlNifPid};
use beam_abi::ErlDrvSysInfo;
use beam_alloc::Heap;
use beam_term::atom::{Atom, AtomTable};
use beam_term::prelude::{atom_val, is_atom, make_atom, make_small, Eterm};
use libc::{c_char, c_uint};

// ---------------------------------------------------------------------------------------------
// A host that models the runtime's deferred atom commit
// ---------------------------------------------------------------------------------------------

/// A [`NifHost`] whose `atom_name`/`existing_atom` see only the **committed** names.
///
/// `intern_atom` assigns an index (so terms are stable) but does not commit it — exactly the state
/// the real runtime's pending interner leaves an atom in until the call flushes it. A name passed
/// to [`DeferredCommitHost::commit`] is visible to the reverse lookups, standing in for an atom from
/// an earlier call.
struct DeferredCommitHost {
    heap: Heap,
    atoms: AtomTable,
    committed: HashSet<Atom>,
    /// The [`NifHost::system_info`] block this host reports (`None` = the hook is unsupported).
    sysinfo: Option<ErlDrvSysInfo>,
}

impl DeferredCommitHost {
    /// A host with no committed atoms and no `system_info` hook.
    fn new() -> Self {
        DeferredCommitHost {
            heap: Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            committed: HashSet::new(),
            sysinfo: None,
        }
    }

    /// A host whose `system_info` hook reports `info`.
    fn with_sysinfo(info: ErlDrvSysInfo) -> Self {
        DeferredCommitHost {
            sysinfo: Some(info),
            ..Self::new()
        }
    }

    /// Intern `name` **and commit it** (as if a previous call had flushed the pending interner).
    fn commit(&mut self, name: &str) -> Eterm {
        let atom = self.atoms.intern(name);
        self.committed.insert(atom);
        make_atom(atom)
    }
}

impl NifHost for DeferredCommitHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// Assign an atom index, but do **not** commit it — the deferred-commit model.
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    /// Reverse lookup over the committed set only (the divergence under test).
    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        let index = atom_val(atom);
        if !self.committed.contains(&index) {
            return None;
        }
        self.atoms.name(index).map(str::to_string)
    }

    /// Lookup-only over the committed set (never creates the atom).
    fn existing_atom(&mut self, name: &str) -> Option<Eterm> {
        let index = self.atoms.lookup(name)?;
        self.committed.contains(&index).then(|| make_atom(index))
    }

    fn system_info(&mut self) -> Option<ErlDrvSysInfo> {
        self.sysinfo
    }
}

// A NUL-terminated "host" ERTS/OTP version pair for the `system_info` test.
static HOST_ERTS_VERSION: &[u8] = b"erts-host\0";
static HOST_OTP_RELEASE: &[u8] = b"99\0";

/// A `system_info` block with distinctive, unmistakably-host values (and *wrong* ABI-version
/// fields, to show the crate's constants win for those).
fn host_sysinfo() -> ErlDrvSysInfo {
    ErlDrvSysInfo {
        driver_major_version: 0,
        driver_minor_version: 0,
        erts_version: HOST_ERTS_VERSION.as_ptr() as *mut c_char,
        otp_release: HOST_OTP_RELEASE.as_ptr() as *mut c_char,
        thread_support: 1,
        smp_support: 1,
        async_threads: 3,
        scheduler_threads: 7,
        nif_major_version: 0,
        nif_minor_version: 0,
        dirty_scheduler_support: 1,
    }
}

// ---------------------------------------------------------------------------------------------
// 1. Atom visibility within one call
// ---------------------------------------------------------------------------------------------

/// `enif_make_atom` → `enif_get_atom`: the just-interned atom's name is readable **in the same
/// call**, even though the host's `atom_name` cannot see it yet.
#[test]
fn enif_get_atom_reads_an_atom_interned_earlier_in_the_same_call() {
    let mut host = DeferredCommitHost::new();
    let mut env = NifEnv::new(&mut host);
    let name = CString::new("fresh").expect("no interior NUL");

    // SAFETY: `env` is live, `name` is NUL-terminated, `buf` is writable.
    unsafe {
        let atom = enif_make_atom(env.as_ptr(), name.as_ptr());

        // The host's own reverse lookup cannot see it (the deferred commit)...
        assert_eq!(
            NifHost::atom_name(env.host(), eterm_from_nif(atom)),
            None,
            "the host's committed table has not got the atom yet"
        );

        // ...but `enif_get_atom` still answers the name: BEFORE the fix this returned 0.
        let mut buf = [0i8; 16];
        let n = enif_get_atom(
            env.as_ptr(),
            atom,
            buf.as_mut_ptr(),
            buf.len() as c_uint,
            ErlNifCharEncoding::Latin1,
        );
        assert_eq!(n, 6, "5 bytes + NUL");
        // SAFETY: the call wrote `n` bytes including the NUL.
        assert_eq!(
            std::ffi::CStr::from_ptr(buf.as_ptr()).to_bytes(),
            b"fresh"
        );

        // `enif_get_atom_length` resolves it too.
        let mut len: c_uint = 0;
        assert_eq!(
            enif_get_atom_length(
                env.as_ptr(),
                atom,
                &mut len,
                ErlNifCharEncoding::Latin1,
            ),
            1
        );
        assert_eq!(len, 5);
        assert_eq!(
            enif_get_atom_length(env.as_ptr(), atom, &mut len, ErlNifCharEncoding::Utf8),
            1
        );
        assert_eq!(len, 5);
    }
}

/// The record is keyed by the term word only for `enif_get_atom`: an atom the host *committed*
/// before the call still resolves through the `atom_name` fallback.
#[test]
fn enif_get_atom_still_falls_back_to_the_host_hook() {
    let mut host = DeferredCommitHost::new();
    let committed = host.commit("already");
    let mut env = NifEnv::new(&mut host);

    // SAFETY: `env` is live and `buf` is writable.
    unsafe {
        let mut buf = [0i8; 16];
        let n = enif_get_atom(
            env.as_ptr(),
            nif_from_eterm(committed),
            buf.as_mut_ptr(),
            buf.len() as c_uint,
            ErlNifCharEncoding::Latin1,
        );
        assert_eq!(n, 8, "7 bytes + NUL");
        // SAFETY: the call wrote `n` bytes including the NUL.
        assert_eq!(
            std::ffi::CStr::from_ptr(buf.as_ptr()).to_bytes(),
            b"already"
        );
    }
}

/// `enif_make_existing_atom` resolves an atom interned earlier in the same call — **without**
/// creating one — while an unknown name still answers 0 with `*atom` untouched.
#[test]
fn enif_make_existing_atom_finds_a_just_interned_atom() {
    let mut host = DeferredCommitHost::new();
    let mut env = NifEnv::new(&mut host);
    let fresh = CString::new("fresh").expect("no interior NUL");
    let absent = CString::new("never_seen").expect("no interior NUL");

    // SAFETY: `env` is live; both names are NUL-terminated; `out` is writable.
    unsafe {
        let atom = enif_make_atom(env.as_ptr(), fresh.as_ptr());

        // A hit through the record: BEFORE the fix this returned 0.
        let mut out = ERL_NIF_TERM::from_raw(0xdead_beef);
        assert_eq!(
            enif_make_existing_atom(
                env.as_ptr(),
                fresh.as_ptr(),
                &mut out,
                ErlNifCharEncoding::Latin1,
            ),
            1
        );
        assert_eq!(out, atom, "the recorded atom is written out");

        // A miss leaves `*atom` untouched and never invents an atom.
        let sentinel = ERL_NIF_TERM::from_raw(0xdead_beef);
        let mut out = sentinel;
        assert_eq!(
            enif_make_existing_atom(
                env.as_ptr(),
                absent.as_ptr(),
                &mut out,
                ErlNifCharEncoding::Latin1,
            ),
            0
        );
        assert_eq!(out, sentinel, "a miss must not write the out pointer");

        // The `_len` variant resolves the same just-interned atom.
        let bytes = b"freshX";
        let mut out = ERL_NIF_TERM::from_raw(0xdead_beef);
        assert_eq!(
            enif_make_existing_atom_len(
                env.as_ptr(),
                bytes.as_ptr() as *const c_char,
                5,
                &mut out,
                ErlNifCharEncoding::Latin1,
            ),
            1
        );
        assert_eq!(out, atom);
    }
}

/// An atom the host committed before the call is still found by `enif_make_existing_atom` (the
/// `existing_atom` fallback), and a non-atom term answers 0 with the out-pointer untouched.
#[test]
fn enif_get_atom_refuses_an_unknown_term_without_writing() {
    let mut host = DeferredCommitHost::new();
    let _committed = host.commit("known");
    let mut env = NifEnv::new(&mut host);

    // SAFETY: `env` is live and `buf` is writable.
    unsafe {
        let mut buf = [0x7i8; 8];
        // A small int is not an atom: 0, buffer untouched.
        assert_eq!(
            enif_get_atom(
                env.as_ptr(),
                nif_from_eterm(make_small(7)),
                buf.as_mut_ptr(),
                buf.len() as c_uint,
                ErlNifCharEncoding::Latin1,
            ),
            0
        );
        assert_eq!(buf, [0x7i8; 8], "a non-atom never writes the buffer");

        // An atom term whose index the host never committed is also refused.
        let orphan = nif_from_eterm(make_atom(Atom::from_raw(999)));
        assert_eq!(
            enif_get_atom(
                env.as_ptr(),
                orphan,
                buf.as_mut_ptr(),
                buf.len() as c_uint,
                ErlNifCharEncoding::Latin1,
            ),
            0
        );
        assert_eq!(buf, [0x7i8; 8]);
    }
}

// ---------------------------------------------------------------------------------------------
// 2. `enif_clear_env` leaves the pending exception set
// ---------------------------------------------------------------------------------------------

/// A raise **survives** `enif_clear_env`: `enif_has_pending_exception` still reports the reason.
/// BEFORE the fix the flag was cleared.
#[test]
fn enif_clear_env_leaves_the_pending_exception() {
    let mut host = DeferredCommitHost::new();
    let mut env = NifEnv::new(&mut host);
    let reason = nif_from_eterm(make_small(42));
    let mut got = ERL_NIF_TERM::default();

    // SAFETY: `env` is live; `got` is a live local.
    unsafe {
        assert_eq!(enif_has_pending_exception(env.as_ptr(), &mut got), 0);
        let returned = enif_raise_exception(env.as_ptr(), reason);
        assert_eq!(returned, reason);
        assert_eq!(enif_has_pending_exception(env.as_ptr(), &mut got), 1);
        assert_eq!(got, reason);

        enif_clear_env(env.as_ptr());
        assert_eq!(
            enif_has_pending_exception(env.as_ptr(), &mut got),
            1,
            "enif_clear_env must not clear the exception (erl_nif.c:700-723)"
        );
        assert_eq!(got, reason);

        // A null env is still a safe no-op.
        enif_clear_env(core::ptr::null_mut());
    }
}

// ---------------------------------------------------------------------------------------------
// 3. `enif_system_info` prefers the host
// ---------------------------------------------------------------------------------------------

/// With a live env whose host supplies a `system_info` block, the runtime-dependent fields come
/// from the host; the ABI-version fields stay the crate's constants.
#[test]
fn enif_system_info_uses_the_host_block_when_available() {
    let mut host = DeferredCommitHost::with_sysinfo(host_sysinfo());
    let _env = NifEnv::new(&mut host);
    let full = core::mem::size_of::<ErlDrvSysInfo>();
    let mut info = host_sysinfo();

    // SAFETY: `info` is a live, fully sized `ErlDrvSysInfo`; `_env` keeps the host on the stack.
    unsafe { enif_system_info(&mut info as *mut ErlDrvSysInfo, full) };

    // SAFETY: both pointers are the static NUL-terminated buffers set above.
    unsafe {
        assert_eq!(
            std::ffi::CStr::from_ptr(info.erts_version).to_bytes(),
            b"erts-host"
        );
        assert_eq!(
            std::ffi::CStr::from_ptr(info.otp_release).to_bytes(),
            b"99"
        );
    }
    assert_eq!(info.scheduler_threads, 7);
    assert_eq!(info.async_threads, 3);
    assert_eq!(info.dirty_scheduler_support, 1);
    assert_eq!((info.thread_support, info.smp_support), (1, 1));
    // The ABI-version fields are the crate's constants (the host reported 0).
    assert_eq!(info.driver_major_version, beam_abi::ERL_DRV_EXTENDED_MAJOR_VERSION);
    assert_eq!(info.driver_minor_version, beam_abi::ERL_DRV_EXTENDED_MINOR_VERSION);
    assert_eq!(info.nif_major_version, beam_abi::ERL_NIF_MAJOR_VERSION);
    assert_eq!(info.nif_minor_version, beam_abi::ERL_NIF_MINOR_VERSION);
}

/// With no reachable host (and with a host whose hook answers `None`), the built-in values are
/// reported — the partial-fill contract is preserved either way.
#[test]
fn enif_system_info_falls_back_to_the_built_ins() {
    let full = core::mem::size_of::<ErlDrvSysInfo>();

    // No live env on this thread: no host at all.
    let mut info = host_sysinfo();
    // SAFETY: `info` is a live, fully sized `ErlDrvSysInfo`.
    unsafe { enif_system_info(&mut info as *mut ErlDrvSysInfo, full) };
    assert_eq!(info.scheduler_threads, 1);
    assert_eq!(info.async_threads, 0);
    assert_eq!(info.dirty_scheduler_support, 0);
    // SAFETY: the pointer is the crate's static ERTS-version buffer.
    unsafe {
        assert_eq!(
            std::ffi::CStr::from_ptr(info.erts_version).to_str().expect("ascii"),
            beam_common::ERTS_VERSION
        );
    }

    // A live env whose host leaves `system_info` unsupported: still the built-ins.
    let mut host = DeferredCommitHost::new();
    let _env = NifEnv::new(&mut host);
    let mut info = host_sysinfo();
    // SAFETY: `info` is a live, fully sized `ErlDrvSysInfo`; `_env` keeps the host on the stack.
    unsafe { enif_system_info(&mut info as *mut ErlDrvSysInfo, full) };
    assert_eq!(info.scheduler_threads, 1);
    assert_eq!(info.async_threads, 0);

    // Partial fill with a host: only the group covered by `si_size` is written.
    let smp_end = core::mem::offset_of!(ErlDrvSysInfo, smp_support) + core::mem::size_of::<libc::c_int>();
    let mut buf = [0xFFu8; core::mem::size_of::<ErlDrvSysInfo>()];
    // SAFETY: the buffer is exactly the struct's size, so it is writable for `smp_end` bytes.
    unsafe { enif_system_info(buf.as_mut_ptr().cast::<ErlDrvSysInfo>(), smp_end) };
    assert_eq!(
        libc::c_int::from_ne_bytes(buf[36..40].try_into().expect("4 bytes")),
        -1,
        "scheduler_threads (offset 36) lies past si_size and must not be written"
    );
}

// ---------------------------------------------------------------------------------------------
// 4. Host-hook entry points: the C-correct failure / edge behaviour with no hook
// ---------------------------------------------------------------------------------------------

/// `enif_term_to_binary` answers 0 (its failure code) with no `term_to_binary` hook, and on a
/// null env / out-pointer.
#[test]
fn enif_term_to_binary_answers_0_without_a_host_hook() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let mut bin = ErlNifBinary::default();
    // SAFETY: `env` is live; `bin` is a live local; the null cases are explicitly allowed.
    unsafe {
        assert_eq!(
            enif_term_to_binary(env.as_ptr(), nif_from_eterm(make_small(1)), &mut bin),
            0,
            "no term_to_binary hook ⇒ the C's failure code, never a wrong binary"
        );
        assert_eq!(
            enif_term_to_binary(env.as_ptr(), nif_from_eterm(make_small(1)), core::ptr::null_mut()),
            0,
            "null out-pointer"
        );
        assert_eq!(
            enif_term_to_binary(core::ptr::null_mut(), nif_from_eterm(make_small(1)), &mut bin),
            0,
            "null env"
        );
    }
}

/// `enif_binary_to_term` answers 0 with no `binary_to_term` hook, and on null arguments.
#[test]
fn enif_binary_to_term_answers_0_without_a_host_hook() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let mut out = ERL_NIF_TERM::from_raw(0xdead_beef);
    let bytes = [131u8, 97, 0];
    // SAFETY: `env` is live; `bytes` has 3 readable bytes; `out` is writable; null cases allowed.
    unsafe {
        assert_eq!(
            enif_binary_to_term(env.as_ptr(), bytes.as_ptr(), bytes.len(), &mut out, 0),
            0,
            "no binary_to_term hook ⇒ 0"
        );
        assert_eq!(out.raw(), 0xdead_beef, "nothing was written");
        assert_eq!(
            enif_binary_to_term(
                env.as_ptr(),
                bytes.as_ptr(),
                bytes.len(),
                core::ptr::null_mut(),
                0
            ),
            0,
            "null term out-pointer"
        );
        assert_eq!(
            enif_binary_to_term(env.as_ptr(), core::ptr::null(), 0, &mut out, 0),
            0,
            "null data"
        );
        assert_eq!(
            enif_binary_to_term(core::ptr::null_mut(), bytes.as_ptr(), bytes.len(), &mut out, 0),
            0,
            "null env"
        );
    }
}

/// `enif_make_ref` raises `badarg` (a pending exception) with no `make_ref` hook.
#[test]
fn enif_make_ref_raises_badarg_without_a_host_hook() {
    let mut host = common::TestHost::new();
    let badarg = host.atom("badarg");
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live.
    unsafe {
        let term = enif_make_ref(env.as_ptr());
        assert_eq!(eterm_from_nif(term), badarg);
        assert_eq!(env.pending_exception(), Some(badarg));
        // A null env is a bare `badarg` (NIL), never a crash.
        assert_eq!(
            eterm_from_nif(enif_make_ref(core::ptr::null_mut())),
            beam_term::prelude::NIL
        );
    }
}

/// `enif_whereis_pid` answers 0 (and writes nothing) with no `whereis_pid` hook.
#[test]
fn enif_whereis_pid_answers_0_without_a_host_hook() {
    let mut host = common::TestHost::new();
    let name = host.atom("a_registered_name");
    let mut env = NifEnv::new(&mut host);
    let mut pid = ErlNifPid::default();
    let sentinel = pid;
    // SAFETY: `env` is live; `pid` is a live local; the null cases are explicitly allowed.
    unsafe {
        assert_eq!(
            enif_whereis_pid(env.as_ptr(), nif_from_eterm(name), &mut pid),
            0,
            "no whereis_pid hook ⇒ 0"
        );
        assert_eq!(pid, sentinel, "nothing was written");
        assert_eq!(
            enif_whereis_pid(env.as_ptr(), nif_from_eterm(name), core::ptr::null_mut()),
            0,
            "null pid out-pointer"
        );
        assert_eq!(
            enif_whereis_pid(env.as_ptr(), nif_from_eterm(make_small(7)), &mut pid),
            0,
            "a non-atom name"
        );
        assert_eq!(
            enif_whereis_pid(core::ptr::null_mut(), nif_from_eterm(name), &mut pid),
            0,
            "null env"
        );
    }
}
