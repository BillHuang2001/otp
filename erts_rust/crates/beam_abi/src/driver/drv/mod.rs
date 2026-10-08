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

//! The `driver_*` / `erl_drv_*` host functions — the Rust side of the driver C ABI (workspace
//! contract **A.4**).
//!
//! Every function here is a real `#[no_mangle] pub unsafe extern "C" fn` whose signature matches the
//! corresponding field of the frozen [`ErlDrvApi`] table **exactly** (see [`crate::driver::api`]).
//!
//! [`driver_api_table`] exposes the `#[used] static DRIVER_API` — the runtime's `driver_*` surface as
//! a populated table. The `#[used]` static is what keeps the `#[no_mangle]` symbols in the linkage of
//! a binary that pulls this crate in (a driver `.so` resolves them through `-Wl,--export-dynamic`),
//! exactly as [`crate::nif::enif::enif_api_table`] does for the NIF half.
//!
//! # Layout
//!
//! This is a directory module split along the ABI's own seam, so no file grows past the workspace's
//! ~1000-line target. The boundary is **part 1 (host-free) vs part 2 (port/host-dependent)**,
//! described below:
//!
//! * this file — the module docs, the shared `env_for` helper, the exported `driver_term_nil` data
//!   symbol, and the `#[used]` [`DRIVER_API`] table assembled from the per-file `populate` chain
//!   (so the signature check against the one frozen table lives here);
//! * `mem` — **part 1, host-free**: the functions that need no port, no scheduler and no runtime
//!   service, plus their `populate` step;
//! * `port` — **part 2, port/host-dependent**: the functions whose `ErlDrvPort` operand **is** the
//!   pointer [`DriverEnv::as_port`] hands out ([`DriverEnv::from_port`] recovers it), plus their
//!   `populate` step;
//! * `lifecycle`, `queues`, `threads` — **implemented**: each owns one group of the C surface (port
//!   lifecycle/attributes, the port queue, the `erl_drv_*` thread family) and contributes its own
//!   `populate` step;
//! * `pdl` — **implemented**: the port data lock, the driver monitors and the deprecated
//!   `driver_get_now` wall clock — see that file's header for the semantics and the documented
//!   deviations. (The binary reference-count trio is **not** here: it shares the driver-binary
//!   block layout and is therefore owned by `mem`, alongside `driver_alloc_binary`.)
//! * `registry` — **implemented**: the four slots that reach structure outside one port —
//!   `add_driver_entry`/`remove_driver_entry` (which reach their host through the thread-local
//!   [`with_current_host`](crate::driver::current_host::with_current_host) publication, since they
//!   carry no port operand) and `driver_lock_driver`/`driver_create_port` — see that file's header.
//!
//! # Table assembly
//!
//! [`DRIVER_API`] starts from the all-`None` [`EMPTY_DRIVER_API`] and is threaded through the
//! `const fn populate` of every file above, innermost-first
//! (`port → mem → threads → queues → pdl → lifecycle → registry`). Each file is therefore the single
//! owner of the `Some(…)` list for the slots it implements; the whole chain is `const`, so it folds
//! at compile time and the frozen table's *shape* is unchanged.
//!
//! # Scope: part 1 (host-free) and part 2 (port/host-dependent)
//!
//! **Part 1 — host-free.** The functions that need **no port, no scheduler and no runtime service**:
//! `driver_alloc`/`driver_free`/`driver_realloc`, `driver_alloc_binary`/`driver_realloc_binary`/
//! `driver_free_binary`, the binary reference-count trio
//! (`driver_binary_get_refc`/`inc_refc`/`dec_refc`), `driver_system_info`, `null_func`,
//! `erl_errno_id`, `erl_drv_consume_timeslice`, the time trio
//! (`erl_drv_monotonic_time`/`erl_drv_time_offset`/`erl_drv_convert_time_unit`), the
//! `driver_dl_open`/`driver_dl_sym`/`driver_dl_close`/`driver_dl_error` quartet,
//! `erl_drv_putenv`/`erl_drv_getenv` and `driver_mk_term_nil`.
//!
//! **Part 2 — port/host-dependent.** The functions whose `ErlDrvPort` operand **is** the pointer
//! [`DriverEnv::as_port`] hands out ([`DriverEnv::from_port`] recovers it) and which therefore
//! delegate to the [`DriverHost`](crate::driver::host::DriverHost) seam:
//!
//! * **output** — [`driver_output`], [`driver_output2`], [`driver_output_binary`],
//!   [`driver_outputv`]: they gather the header/body bytes and call [`DriverHost::output`](crate::driver::host::DriverHost::output).
//! * **term data** — [`driver_output_term`], [`driver_send_term`], [`erl_drv_output_term`],
//!   [`erl_drv_send_term`]: they decode the `ErlDrvTermData[]` spec with
//!   [`decode_term_spec`](crate::driver::host::decode_term_spec) and
//!   call [`DriverHost::send_term`](crate::driver::host::DriverHost::send_term).
//! * **port queries** — [`driver_connected`], [`driver_caller`], [`driver_mk_port`],
//!   [`driver_mk_atom`].
//! * **select / timers / async** — [`driver_select`], [`driver_set_timer`], [`driver_cancel_timer`],
//!   [`driver_read_timer`], [`driver_async`].
//!
//! Each of them returns the C failure code (`-1`) for a null port, a malformed spec or a
//! host-reported failure — never a crash, never a wrong result.
//!
//! Everything else is populated by the family that owns it:
//!
//! * **port queues** — `driver_vec_to_buf` and the `driver_enq`/`driver_pushq`/`driver_deq`/
//!   `driver_sizeq`/`driver_peekq*`/`driver_enqv`/`driver_pushqv` family (the `queues` file);
//! * **port lifecycle & attributes** — `add_driver_entry`, `remove_driver_entry`,
//!   `driver_lock_driver`, `driver_create_port` (the `registry` file), `erl_drv_init_ack`,
//!   `erl_drv_set_os_pid`, `erl_drv_busy_msgq_limits`, the `driver_failure*`/`driver_exit` family,
//!   `set_busy_port`, `set_port_control_flags`, `get_port_flags` (the `lifecycle` file);
//! * **async thread pool** — `driver_async_port_key` (the `lifecycle` file; the "fair" async key
//!   comes from the host, which owns the port's id term);
//! * **threads** — the whole `erl_drv_mutex_*`/`erl_drv_cond_*`/`erl_drv_rwlock_*`/`erl_drv_tsd_*`/
//!   `erl_drv_thread_*`/`erl_drv_*_name` family (the `threads` file).
//!
//! The **binary reference-count trio** (`driver_binary_get_refc`/`inc_refc`/`dec_refc`) is part of
//! the driver-binary family and lives in `mem`: the count needs a header of its own, allocated
//! *before* the frozen public
//! [`ErlDrvBinary`](crate::driver::types::ErlDrvBinary) (`{ orig_size, orig_bytes }`, 16 bytes,
//! asserted in [`crate::driver::types`]), exactly mirroring the emulator's private `Binary` header.
//! The public struct's layout is untouched — see `mem`'s *Driver binaries* section.
//!
//! # Documented deviations from the oracle
//!
//! * [`erl_errno_id`] returns `strerror`'s **descriptive message**; the oracle returns the lowercase
//!   POSIX errno *name* (`"enoent"`, via `strerrorname_np`). Same input, different text.
//! * [`erl_drv_time_offset`] returns `0` by construction: this runtime anchors Erlang monotonic time
//!   on the system clock's epoch, so there is no separate offset to measure.
//! * [`driver_system_info`] **returns without writing** for a null `sip` or an `si_size` shorter than
//!   the first field group, where the C calls `erts_exit(ERTS_ERROR_EXIT, …)`: a library function
//!   must not abort its host process (workspace `CONTEXT.md` §3, never panic-as-control-flow).
//!
//! Part 2 (port/host-dependent) adds one deviation per port-carrying operand, because the C's port
//! *id term* has no counterpart in this seam:
//!
//! * [`driver_mk_port`] returns the **port handle pointer**, pointer-encoded, where the C returns the
//!   port's id term (`prt->common.id`); [`driver_mk_atom`] returns the **name pointer** where the C
//!   returns a pre-interned atom handle. Both are exactly the operands [`erl_drv_output_term`]/
//!   [`erl_drv_send_term`] and [`decode_term_spec`](crate::driver::host::decode_term_spec)'s
//!   `ERL_DRV_ATOM` expect, so each pair round-trips
//!   (`driver_mk_port` → `erl_drv_output_term`, `driver_mk_atom` → `ERL_DRV_ATOM`).
//! * [`driver_output_term`]/[`driver_send_term`]/[`erl_drv_output_term`]/[`erl_drv_send_term`] return
//!   the host's code (`0` = delivered) where the C returns `1` for a delivered term and `0` for an
//!   undeliverable one; a driver following the documented idiom of testing `< 0` sees identical
//!   behaviour. The header/body split of `driver_output*` is likewise joined into one byte slice,
//!   since [`DriverHost::output`](crate::driver::host::DriverHost::output) takes a single slice (the C can keep the header out as a list
//!   prefix).
//! * A malformed or out-of-range request is **refused** (`-1`) where the C would read past the end of
//!   a caller buffer: `driver_output_binary`'s `offset + len > orig_size`, a `driver_outputv` `skip`
//!   past the vector's declared lengths, and a null `time_left`/`data`/`ErlIOVec`.
//! * `driver_select`/`driver_set_timer`/`driver_cancel_timer`/`driver_read_timer`/`driver_async`
//!   forward to [`DriverHost`](crate::driver::host::DriverHost), whose defaulted methods report
//!   "unsupported" as `-1` (the C's
//!   failure code for a driver without the matching callback): a driver told `-1` falls back exactly
//!   as it would against an ERTS whose port has no such callback.
//!
//! # `unsafe`
//!
//! Every function here is FFI. Each `unsafe fn` carries a `# Safety` section for its caller contract
//! and every `unsafe` block a preceding `// SAFETY:` rationale (workspace `CONTEXT.md` §3). No
//! `unwrap()` happens on runtime data and no failure path panics. Part 2 adds the port-pointer
//! recovery ([`DriverEnv::from_port`]) and the raw operand reads of the term-data/vector/binary
//! arguments; each is reached only through one documented helper.

mod lifecycle;
mod mem;
mod pdl;
mod port;
mod queues;
mod registry;
mod threads;

use beam_term::prelude::NIL;

use crate::driver::api::ErlDrvApi;
use crate::driver::host::DriverEnv;
use crate::driver::types::{ErlDrvPort, ErlDrvTermData};

// The glob re-exports below pull the moved functions into the table below and keep them Rust-side
// reachable at `drv::<name>` exactly as they were before the split (their C linkage comes from
// `#[no_mangle]` and is unaffected either way).
pub use self::mem::*;
pub use self::port::*;

#[cfg(test)]
use crate::driver::types::ErlDrvBinary;
#[cfg(test)]
use core::mem::offset_of;

// ---------------------------------------------------------------------------------------------
// Static data exported to drivers
// ---------------------------------------------------------------------------------------------

/// `driver_term_nil` — the `ErlDrvTermData` for the empty list: the C is
/// `const ErlDrvTermData driver_term_nil = (ErlDrvTermData) NIL;` (`erts/emulator/beam/io.c:78`).
///
/// It is a **data** symbol, not a function, so it is not part of the function-pointer [`ErlDrvApi`]
/// table (see that module's note on `driver_mk_term_nil`).
#[no_mangle]
pub static driver_term_nil: ErlDrvTermData = NIL.raw() as ErlDrvTermData;

// ---------------------------------------------------------------------------------------------
// Shared helper (used by the part-2 port functions)
// ---------------------------------------------------------------------------------------------

/// The driver environment behind a C-facing port pointer, or `None` for a null pointer.
///
/// The whole of part 2 goes through this one recovery point, so the port-pointer safety contract is
/// stated once: **[`DriverEnv::as_port`] + [`DriverEnv::from_port`] are the port ABI** (see
/// [`crate::driver::host`]).
///
/// # Safety
///
/// `port` must be null or a pointer previously returned by [`DriverEnv::as_port`] for an environment
/// (and host) that is still alive — exactly [`DriverEnv::from_port`]'s own contract.
unsafe fn env_for<'a>(port: ErlDrvPort) -> Option<&'a mut DriverEnv> {
    // SAFETY: the caller's contract is `DriverEnv::from_port`'s contract, restated above.
    unsafe { DriverEnv::from_port(port) }
}

// ---------------------------------------------------------------------------------------------
// The `#[used]` table
// ---------------------------------------------------------------------------------------------

/// The all-`None` base table.
///
/// `crate::driver::api::ErlDrvApi::empty()` is a plain `fn` (not `const`), so it cannot initialise a
/// `static`; this `const` is the `const`-evaluable equivalent. Every field of [`ErlDrvApi`] is an
/// `Option<unsafe extern "C" fn …>`, for which `None` is the null pointer (the null-pointer
/// optimization), so an all-zero struct **is** the all-`None` table.
/// `empty_base_matches_the_all_none_table_by_bytes` pins that equality against `ErlDrvApi::empty()`,
/// so a layout change cannot slip through unnoticed.
const EMPTY_DRIVER_API: ErlDrvApi = unsafe { core::mem::zeroed() };

/// The runtime's populated `driver_*` / `erl_drv_*` table.
///
/// `#[used]` keeps this static (and hence the `#[no_mangle]` functions it names) in the linkage, so a
/// driver `.so` loaded into the process can resolve them. Slots left `None` are listed in the module
/// header.
///
/// The table is assembled by a `const fn` `populate` chain: **each file owns the `Some(…)` list for
/// the slots it implements** (`mem` the host-free group, `port` the port/host-dependent group, and
/// the `lifecycle`/`pdl`/`queues`/`threads`/`registry` family files the group each one owns), so a
/// later workstream can wire a family without ever editing this file. The chain starts from the
/// all-`None` [`EMPTY_DRIVER_API`], and every step is `const`, so it folds entirely at compile time.
#[used]
static DRIVER_API: ErlDrvApi = registry::populate(
    lifecycle::populate(pdl::populate(queues::populate(threads::populate(mem::populate(
        port::populate(EMPTY_DRIVER_API),
    ))))),
);

/// The populated `driver_*` / `erl_drv_*` table (see [`DRIVER_API`]).
pub fn driver_api_table() -> &'static ErlDrvApi {
    &DRIVER_API
}

/// A payload view of a driver binary, reached through the block base rather than the declared
/// one-element `orig_bytes` array (the C's flexible-array-member convention).
///
/// Shared by the part-1 ([`mem`]) and part-2 ([`port`]) binary tests, so it lives with the table.
#[cfg(test)]
fn binary_payload<'a>(bin: *mut ErlDrvBinary, len: usize) -> &'a mut [u8] {
    // SAFETY: `bin` heads a `driver_binary_block_size(len)`-byte block, so the payload at
    // `offset_of!(orig_bytes)` is valid for `len` bytes.
    unsafe {
        core::slice::from_raw_parts_mut(
            (bin as *mut u8).add(offset_of!(ErlDrvBinary, orig_bytes)),
            len,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{size_of, size_of_val};

    #[test]
    fn empty_base_matches_the_all_none_table_by_bytes() {
        let ours = EMPTY_DRIVER_API;
        let theirs = ErlDrvApi::empty();
        let n = size_of::<ErlDrvApi>();
        // SAFETY: both are live values of exactly `n` bytes (the table is a plain pointer mix).
        let a = unsafe { core::slice::from_raw_parts(&ours as *const _ as *const u8, n) };
        // SAFETY: as above.
        let b = unsafe { core::slice::from_raw_parts(&theirs as *const _ as *const u8, n) };
        assert_eq!(a, b, "the zeroed base must equal the all-None table");
    }

    #[test]
    fn driver_api_table_populates_the_implemented_slots_and_leaves_the_rest_none() {
        let t = driver_api_table();
        let populated = [
            // Part 1: host-free.
            t.driver_alloc.is_some(),
            t.driver_free.is_some(),
            t.driver_realloc.is_some(),
            t.driver_alloc_binary.is_some(),
            t.driver_realloc_binary.is_some(),
            t.driver_free_binary.is_some(),
            t.driver_binary_get_refc.is_some(),
            t.driver_binary_inc_refc.is_some(),
            t.driver_binary_dec_refc.is_some(),
            t.driver_system_info.is_some(),
            t.null_func.is_some(),
            t.erl_errno_id.is_some(),
            t.erl_drv_consume_timeslice.is_some(),
            t.erl_drv_monotonic_time.is_some(),
            t.erl_drv_time_offset.is_some(),
            t.erl_drv_convert_time_unit.is_some(),
            t.driver_dl_open.is_some(),
            t.driver_dl_sym.is_some(),
            t.driver_dl_close.is_some(),
            t.driver_dl_error.is_some(),
            t.erl_drv_putenv.is_some(),
            t.erl_drv_getenv.is_some(),
            t.driver_mk_term_nil.is_some(),
            // Part 2: port/host-dependent.
            t.driver_output.is_some(),
            t.driver_output2.is_some(),
            t.driver_output_binary.is_some(),
            t.driver_outputv.is_some(),
            t.driver_output_term.is_some(),
            t.driver_send_term.is_some(),
            t.erl_drv_output_term.is_some(),
            t.erl_drv_send_term.is_some(),
            t.driver_mk_atom.is_some(),
            t.driver_mk_port.is_some(),
            t.driver_connected.is_some(),
            t.driver_caller.is_some(),
            t.driver_select.is_some(),
            t.driver_set_timer.is_some(),
            t.driver_cancel_timer.is_some(),
            t.driver_read_timer.is_some(),
            t.driver_async.is_some(),
            // Part 3: the port-queue family (`queues`).
            t.driver_vec_to_buf.is_some(),
            t.driver_enq.is_some(),
            t.driver_pushq.is_some(),
            t.driver_deq.is_some(),
            t.driver_sizeq.is_some(),
            t.driver_enq_bin.is_some(),
            t.driver_pushq_bin.is_some(),
            t.driver_peekqv.is_some(),
            t.driver_peekq.is_some(),
            t.driver_enqv.is_some(),
            t.driver_pushqv.is_some(),
            // Part 4: the driver registry / port creation family (`registry`).
            t.add_driver_entry.is_some(),
            t.remove_driver_entry.is_some(),
            t.driver_lock_driver.is_some(),
            t.driver_create_port.is_some(),
        ];
        assert!(
            populated.iter().all(|&p| p),
            "every implemented slot must be populated: {populated:?}"
        );

        // The binary-refcount trio belongs to `mem` (it shares the driver-binary block layout).
        // Which of the *other* slots are still `None` is deliberately NOT asserted here — it
        // changes as families land, and each family's own test pins its own slots. See `CONTEXT.md`
        // for the current status.
        assert!(
            t.driver_binary_get_refc.is_some()
                && t.driver_binary_inc_refc.is_some()
                && t.driver_binary_dec_refc.is_some()
        );

        assert_eq!(size_of_val(t), size_of::<ErlDrvApi>());
    }
}
