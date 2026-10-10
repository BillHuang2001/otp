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

//! The runtime-native `erts_internal:*` floor (contract **A.7**).
//!
//! `erts_internal` is the *internal* face of the emulator: the BIFs the Erlang
//! side (`erts_internal.erl`, `erts_debug.erl`, `code.erl`, …) calls when it needs
//! something only the runtime can answer. `beam_bifs` owns exactly **one** of them
//! (`erts_internal:cmp_term/2`, the A.2 term-comparison primitive) and adds no
//! others; the rest are either preloaded `.beam` code or C BIFs registered by
//! `erts_internal.erl`'s `-nifs()` table.
//!
//! This module serves the subset that is a genuine *runtime* operation (it reads
//! the runtime's own tables, atom space or process table) and needs no machinery
//! this crate lacks. Everything it does not claim returns [`None`] from
//! [`RuntimeContext::erts_internal_call`], so the call falls through to
//! `beam_bifs`' registry and then to a real preloaded-module load — never a wrong
//! value, never a fabricated success.
//!
//! # BIFs served (each verified against the `erl` oracle, OTP 29.1.1 / erts-17.1)
//!
//! | BIF | Ground truth | Oracle probe |
//! |---|---|---|
//! | `erts_internal:term_type/1` | `erl_map.c:3336-3417` | `1`→`fixnum`, `a`→`atom`, `{x}`→`tuple`, `#{a=>1}`→`flatmap`, `self()`→`pid`, `1.0`→`hfloat`, `[]`→`nil`, `[1,2]`→`list`, `fun erts_internal:term_type/1`→`export`, `1 bsl 100`→`bignum`, `<a wire external pid/port/reference>`→`external_pid`/`external_port`/`external_reference` |
//! | `erts_internal:get_dflags/0` | `dist.c:5566-5592` | `{erts_dflags,468283523004,17230663572,468283523004,8396866,8192}` — the same fixed record on a non-distributed and on a `-sname` node |
//! | `erts_internal:beamfile_chunk/2` | `beam_bif_load.c:165-201` | `byte_size(beamfile_chunk(element(2,code:get_object_code(lists)),"Code"))`→`30110`; `"Nope"`→`undefined`; `"code"`→`undefined`; `5`/`"Code"`→`error:badarg` |
//! | `erts_internal:beamfile_module_md5/1` | `beam_bif_load.c:203-229` | `beamfile_module_md5(element(2,code:get_object_code(slice)))`→`<<2,133,170,13,173,204,31,130,161,1,140,211,209,13,116,36>>`; `<<"garbage">>`→`undefined`; `5`→`error:badarg` |
//! | `erts_internal:map_to_tuple_keys/1` | `erl_map.c:3318-3328` | `#{a=>1,b=>2,c=>3}`→a keys tuple; `#{}`→`{}`; a hashmap→`badarg`; `[1,2]`→`error:{badmap,[1,2]}` |
//! | `erts_internal:binary_to_integer/2` | `big.c:4186-4232` (`chars_to_integer`) | `<<"ff">>,16`→`255`; `<<"-ff">>,16`→`-255`; `<<>>,10`→`badarg`; `<<"10">>,37`→`badarg`; 19 digits→`big` |
//! | `erts_internal:list_to_integer/2` | `big.c:4234-4355` | `"10",10`→`{10,[]}`; `"12ab",10`→`{12,"ab"}`; `[]`→`no_integer`; `foo`→`not_a_list`; 18 nines→`big` |
//! | `erts_internal:time_unit/0` | `erl_time_sup.c:2539` | `1000000000` |
//! | `erts_internal:perf_counter_unit/0` | `erl_time_sup.c:2537` | `1000000000` |
//! | `erts_internal:get_creation/0` | `dist.c:5594-5604` | `undefined` on a non-distributed node |
//! | `erts_internal:is_process_alive/1` | `erl_bif_info.c:4069` | `self()`→`true`; after a process exits→`false`; `foo`→`error:badarg` |
//! | `erts_internal:is_process_alive/2` | `erl_bif_info.c:4058-4067` | `(self(), make_ref())`→`ok` for a live **and** a dead pid; `(foo, Ref)`/`(self(), foo)`→`error:badarg` |
//! | `erts_internal:process_flag/3` | `bif.c:2262-2297` (`process_flag_aux:1896-2022`) | `(self(), save_calls, N)`→the previous length (`0` first, then `7`, `4`, …); `(self(), trap_exit, true)`/`(self(), save_calls, true)`/`(self(), save_calls, 10001)`→`error:badarg`; `(foo\|123, save_calls, 1)`→the **value** `badtype` |
//! | `erts_internal:no_aux_work_threads/0` | `erl_process.c:9234-9237` (`:6219-6223`) | `erl`→`17` (16 schedulers + 1 + no dirty-alloc instances); `erl +S 1`→`2` |
//! | `erts_internal:processes_next/1` | `bif.c:4111-4120` | `processes_next(1000000000000)`→`badarg`; `foo`→`badarg` |
//! | `erts_internal:term_to_string/1,2` | `bif.c:4160-4189` | `{a,1}`→`"{a,1}"` (one line, `~w`); a limit `1`→`error:badarg` |
//! | `erts_internal:garbage_collect/1` | `bif.c:4081-4094` | `minor`/`major`→`true` (a request is recorded); `foo`/`1`→`error:badarg` |
//! | `erlang:garbage_collect/0,1` | `erlang.erl:3051-3066` (both reduce to that `erts_internal` BIF) | `()`→`true`; `(self())`→`true`; `(LivePid)`→`true`; `(DeadPid)`→`false`; `(foo\|123\|[Pid])`→`error:badarg`; `/2` is a typed gap |
//!
//! # Deliberate Milestone-1 divergences (documented, deterministic)
//!
//! * `term_type/1` reports every byte-aligned binary as `sub_binary`. The C splits the
//!   binary family into `heap_binary` / `sub_binary` / `refc_binary` by the *boxed
//!   header subtag* (`erl_map.c:3368-3376`), and the only predicates that read it
//!   (`beam_term::boxed::is_internal_ref` and its siblings) are `unsafe` — which
//!   `beam_rt` is `#![deny(unsafe_code)]` against. The oracle answers `heap_binary` for
//!   `binary:part(<<"abcd">>,1,2)` and `sub_binary` for `<<1,2>>`; this runtime answers
//!   `sub_binary` for both.
//! * `no_aux_work_threads/0` answers *this* runtime's own scheduler count `+ 1`, so its
//!   **default** is as many threads short as this runtime's `system_info(schedulers)`
//!   is short of the oracle's (the count is `available_parallelism()` — affinity- and
//!   cgroup-aware — while ERTS reads the online processor count). With an explicit
//!   `+S N` the two agree exactly (`+S 1` ⇒ `2`). Both figures are self-consistent and
//!   derived, never hardcoded — the same deliberate basis as `erlang:processes/0`.
//! * `processes_next/1` is index-addressed over *this* runtime's process table
//!   (one pid per call, in table order) instead of the C ptab's paginated index
//!   space; `erlang:processes/0` is already a documented divergence for the same
//!   reason.
//! * `term_to_string/2`'s **numeric** depth limit is a typed
//!   [`unsupported`](crate::error::unsupported) gap: the C implements it with the
//!   `%.*T` `~P` depth directive, which the single workspace printer
//!   ([`crate::format`]) does not expose. The `undefined`-limit form (`/1`) is
//!   served exactly.
//!
//! # Not served (falls through, then a real module load ⇒ an honest failure)
//!
//! Every other `erts_internal` BIF. See `src/native/CONTEXT.md` for the itemised
//! list and the machinery each one needs (distribution, the application controller,
//! microstate accounting, code purge, tracing, atomics/counters resources, the
//! system-task protocol, …).
//!
//! `erts_internal:beamfile_module_md5/1` (`beam_bif_load.c:203-229`) returns the module
//! checksum as a **bare 16-byte binary** (the C's `erts_new_binary_from_data(sizeof
//! beam.checksum, beam.checksum)`), or the atom `undefined` for an image `beamfile_read`
//! cannot parse — *not* `beam_lib:md5/1`'s `{ok, {Module, Md5}}` wrapper, which is the
//! Erlang-side shape. The digest is the one [`crate::context::module_md5`] already computes
//! for the `NEW_FUN_EXT` identity (the `FunT` 24-byte record walk with `OldUniq` forced to
//! zero, `beam_file.c:1751-1829`), so this BIF reuses it rather than re-deriving the
//! chunk-layout parse. The image is read from the argument **binary**, never the loaded
//! module.

// The `erts_internal:trace/3` + `trace_pattern/3` half of this floor — the two BIFs the
// `erlang:trace/2,3` / `erlang:trace_pattern/2,3` wrappers call — is a distinct
// responsibility and lives in its own submodule (see `trace.rs`); the dispatch arm below
// is its only entry point from here.
mod trace;

use beam_common::{BeamError, ExitReason, LiveReason, Raise, Result};
use beam_dist::flags::{
    DFLAG_DIST_ADDABLE, DFLAG_DIST_DEFAULT, DFLAG_DIST_MANDATORY, DFLAG_DIST_REJECTABLE,
    DFLAG_DIST_STRICT_ORDER,
};
use beam_engine::terms;
use beam_loader::container::{ChunkId, Container};
use beam_process::ProcessId;
use beam_term::{
    is_atom, is_byte, is_catch, is_internal_pid, is_internal_port, is_list, is_nil, is_small,
    make_small, signed_value, Eterm,
};

use super::RuntimeContext;
use crate::error;

/// The number of base-`b` digits that always fit a small integer —
/// `digits_per_small_lookup` (`big.c:4115-4122`), `b = 2..=36`, the 64-bit table.
///
/// Index `0` is base `2`. Copied verbatim from the C comment's `Wo.Alpha`
/// formula `Trunc[59 / log[2,n]] - 1`.
const DIGITS_PER_SMALL: [usize; 35] = [
    59, 37, 29, 25, 22, 21, 19, 18, 17, 17, 16, 15, 15, 15, 14, 14, 14, 13, 13, 13, 13, 13, 12, 12,
    12, 12, 12, 12, 12, 11, 11, 11, 11, 11, 11,
];

/// The outcome of [`chars_to_integer`] — the C's three return values.
enum CharInt {
    /// `make_small(i)` — a parsed small integer.
    Small(i64),
    /// `am_big` — the magnitude needs a bignum, which M1 cannot build here.
    Big,
    /// `am_badarg` — an empty/invalid input (returned as a **value**, like the C).
    BadArg,
}

/// `get_digits_per_small(base)` (`big.c:434-436`).
fn digits_per_small(base: usize) -> usize {
    DIGITS_PER_SMALL[base - 2]
}

/// `c2int_is_valid_char(ch, base)` (`big.c:4060-4072`).
fn is_valid_char(ch: u8, base: usize) -> bool {
    if base <= 10 {
        return ch >= b'0' && (ch as usize) < b'0' as usize + base;
    }
    (ch >= b'0' && (ch as usize) < b'0' as usize + base)
        || (ch >= b'A' && (ch as usize) < b'A' as usize + base - 10)
        || (ch >= b'a' && (ch as usize) < b'a' as usize + base - 10)
}

/// `c2int_is_invalid_char(ch, base)` (`big.c:4074-4076`).
fn is_invalid_char(ch: u8, base: usize) -> bool {
    !is_valid_char(ch, base)
}

/// `c2int_digit_from_base(ch)` (`big.c:4078-4081`).
fn digit_from_base(ch: u8) -> u8 {
    if ch <= b'9' {
        ch - b'0'
    } else if ch <= b'Z' {
        10 + (ch - b'A')
    } else {
        10 + (ch - b'a')
    }
}

/// `chars_to_integer(bytes, size, base)` (`big.c:4162-4232`) — the shared
/// base-`N` parser behind both `binary_to_integer/2` and `list_to_integer/2`.
///
/// Faithful port, including the C's *value* returns: leading `-`/`+` handling,
/// leading-zero trimming (an all-zero input is `0`, sign discarded), the
/// `digits_per_small` magnitude shortcut, the `base <= 10` fast path, and the
/// `c2int_*` digit tables.
fn chars_to_integer(mut bytes: &[u8], base: usize) -> CharInt {
    if bytes.is_empty() {
        return CharInt::BadArg;
    }
    let mut neg = false;
    if bytes[0] == b'-' {
        neg = true;
        bytes = &bytes[1..];
    } else if bytes[0] == b'+' {
        bytes = &bytes[1..];
    }
    if bytes.is_empty() {
        return CharInt::BadArg;
    }
    // Trim leading zeroes; an all-zero input is `make_small(0)` with no sign.
    while bytes[0] == b'0' {
        bytes = &bytes[1..];
        if bytes.is_empty() {
            return CharInt::Small(0);
        }
    }
    if bytes.len() > digits_per_small(base) {
        return CharInt::Big;
    }
    let mut i: i64 = 0;
    if base <= 10 {
        for &b in bytes {
            let digit = i64::from(b) - i64::from(b'0');
            if digit < 0 || digit as usize >= base {
                return CharInt::BadArg;
            }
            i = i * base as i64 + digit;
        }
    } else {
        for &b in bytes {
            if is_invalid_char(b, base) {
                return CharInt::BadArg;
            }
            i = i * base as i64 + i64::from(digit_from_base(b));
        }
    }
    if neg {
        i = -i;
    }
    CharInt::Small(i)
}

impl RuntimeContext<'_> {
    /// The `erts_internal:*` half of the native floor.
    ///
    /// Returns `None` when `(function, arity)` is not one this module serves — the
    /// caller then falls through to the `beam_bifs` registry (which owns
    /// `erts_internal:cmp_term/2`) and then to a preloaded-module load.
    pub(super) fn erts_internal_call(
        &mut self,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        match (function, arity) {
            ("term_type", 1) => Some(self.erts_term_type(args)),
            ("get_dflags", 0) => Some(self.erts_get_dflags()),
            ("beamfile_chunk", 2) => Some(self.erts_beamfile_chunk(args)),
            ("beamfile_module_md5", 1) => Some(self.erts_beamfile_module_md5(args)),
            ("map_to_tuple_keys", 1) => Some(self.erts_map_to_tuple_keys(args)),
            ("binary_to_integer", 2) => Some(self.erts_binary_to_integer(args)),
            ("list_to_integer", 2) => Some(self.erts_list_to_integer(args)),
            ("time_unit", 0) => Some(self.erts_time_unit()),
            ("perf_counter_unit", 0) => Some(self.erts_perf_counter_unit()),
            ("get_creation", 0) => Some(self.erts_get_creation()),
            ("is_process_alive", 1) => Some(self.erts_is_process_alive(args)),
            ("is_process_alive", 2) => Some(self.erts_is_process_alive_2(args)),
            ("process_flag", 3) => Some(self.erts_process_flag(args)),
            // The per-process group leader (`bif.tab:79-80`, `bif.c:4949-5021`): the two
            // counterparts of `erlang:group_leader/2` (which `erlang.erl:3328-3341` wraps).
            // The floor runs before the registry, so it preempts the defaulted
            // `RuntimeHooks` entries `beam_bifs` registers for these two names. See
            // [`super::group_leader`].
            ("group_leader", 2) => Some(self.erts_group_leader_2(args)),
            ("group_leader", 3) => Some(self.erts_group_leader_3(args)),
            ("no_aux_work_threads", 0) => Some(self.erts_no_aux_work_threads()),
            ("processes_next", 1) => Some(self.erts_processes_next(args)),
            ("term_to_string", 1) => Some(self.erts_term_to_string(args, false)),
            ("term_to_string", 2) => Some(self.erts_term_to_string(args, true)),
            ("trace", 3) => Some(self.erts_trace(args)),
            ("trace_pattern", 3) => Some(self.erts_trace_pattern(args)),
            ("garbage_collect", 1) => Some(self.erts_garbage_collect(args)),
            _ => None,
        }
    }

    // -- erlang:garbage_collect/0,1 and erts_internal:garbage_collect/1 --------

    /// `erlang:garbage_collect/0` — collect the **calling** process
    /// (`erts/preloaded/src/erlang.erl:3051-3053`, whose body is
    /// `erts_internal:garbage_collect(major)`).
    ///
    /// The oracle's answer is `true`. `erlang` is a floor namespace
    /// ([`super::FLOOR_MODULES`]), so its `.beam` body is never entered and the floor owns
    /// this name.
    ///
    /// # Errors
    ///
    /// Only the BIF heap's own exhaustion; the oracle's `/0` never raises.
    pub(super) fn garbage_collect_0(&mut self) -> Result<Eterm> {
        self.request_own_gc();
        self.atom("true")
    }

    /// `erlang:garbage_collect/1` — collect `Pid`
    /// (`erts/preloaded/src/erlang.erl:3055-3066`, the wrapper `erlang:garbage_collect(Pid, [])`).
    ///
    /// # Ground truth (oracle probes, OTP 29.1.1 / erts-17.1)
    ///
    /// `garbage_collect(Pid, [])` (`erlang.erl:3121-3147`) is **synchronous**: for `Pid`
    /// equal to `self()` it calls `erts_internal:garbage_collect(major)` and returns its
    /// `true`; for any other pid it sends the system task
    /// `{garbage_collect, ReqId, major}` (`erts/emulator/beam/erl_process.c:11309-11419`,
    /// `request_system_task/4`) and blocks on `{garbage_collect, ReqId, GCResult}`. Probing
    /// the live oracle:
    ///
    /// | call | oracle |
    /// |---|---|
    /// | `garbage_collect(self())` | `true` |
    /// | `garbage_collect(LivePid)` | `true` |
    /// | `garbage_collect(DeadPid)` | `false` |
    /// | `garbage_collect(foo \| 123 \| [Pid])` | `error:badarg` |
    /// | `garbage_collect(ExternalPid)` (`node = n1`) | `error:badarg` |
    ///
    /// The `badarg` cases are the C's own argument test: the target must be an **internal**
    /// (node-local) pid, or an external pid whose distribution entry is *this* node's
    /// (`erl_process.c:11319-11324`) — any other term, and any pid naming another node, is
    /// `am_badarg`. The `false` is the reply a target that terminated before the request
    /// could be satisfied produces (`erlang.erl:3097-3103`).
    ///
    /// # This runtime
    ///
    /// An internal pid the process table holds is a live local process: the oracle's `true`
    /// is returned and a **request** is recorded for it through the one latch
    /// ([`Runtime::request_gc`](crate::runtime::Runtime), see [`Self::request_own_gc`]) —
    /// never a fabricated collection. The request is served at the target's next
    /// heap-admission point or park, which is exactly the BEAM's own
    /// "handled when appropriate" wording; this runtime cannot *block* the caller on a
    /// cross-process system task, so the answer is the accepted request rather than the
    /// target's completion report (the timing divergence is documented, the value is not
    /// invented).
    ///
    /// A pid the table does not hold is the oracle's `false`: the request cannot be honoured,
    /// so none is recorded and nothing is pretended.
    ///
    /// Any term that is not an internal pid — including an **external** (wire) pid, which
    /// this runtime cannot resolve to a live local process and cannot send a request to
    /// (it has no distribution links) — is the oracle's `badarg`, the C's `am_badarg` for a
    /// non-local target. A `true` is never fabricated for a node this runtime cannot reach.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a non-pid (or non-internal-pid) argument; the BIF heap's own
    /// exhaustion otherwise.
    pub(super) fn garbage_collect_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        let pid = ProcessId::from_eterm(term)?;
        if !self.process_table_mut().contains(pid) {
            // The request cannot be honoured: the oracle's `false`, and nothing recorded.
            return self.atom("false");
        }
        // A live local process: accept the request exactly as `erts_internal:garbage_collect/1`
        // does for the calling process (the same latch, no second mechanism).
        self.runtime.request_gc(pid);
        self.atom("true")
    }

    /// Records a GC request for the **calling** process — the body `erlang:garbage_collect/0`
    /// and `erts_internal:garbage_collect/1` share.
    ///
    /// A collection's root set is the **executing machine's** `x[0..live)` plus its Erlang
    /// stack, and a BIF running inside [`ExecutionContext`](beam_engine::ExecutionContext)
    /// owns neither — so the BIF records a *request* on the runtime
    /// ([`Runtime::request_gc`](crate::runtime::Runtime)) and returns the oracle's `true`.
    /// The request is served at the engine's heap-admission point
    /// ([`crate::context::gc::ensure_heap`]) or, if the process parks first, at the runner's
    /// park arm (`Runtime::serve_gc_request`) — both of which own the machine's root set.
    ///
    /// Only a **scheduled process** — the one whose young heap the collector owns — can be
    /// collected. Every other context (the `-eval` evaluator, a single-pass `run_*`)
    /// allocates on the pointer-stable runtime arena, which no collection ever moves, so a
    /// request for it would be a no-op pretending to be work; the call is the oracle's `true`
    /// and nothing more.
    ///
    /// **No collection is ever pretended.** `Runtime::gc_stats()`'s `collections` moves only
    /// when a collection really ran. A request from a process whose collection is pinned,
    /// halted or globally frozen, from a process the host cannot relocate (a
    /// **relocation-exempt** one), or from one whose *module* code may hold a live
    /// float-register term, is refused — honestly, with the oracle's `true` return value and
    /// the counters untouched. A request that no collection point is ever reached for (a
    /// process that asks and then returns without allocating or parking) is pruned when its
    /// heap dies; there is nothing left to collect.
    fn request_own_gc(&mut self) {
        if !self.owns_process_table() {
            let pid = self.pid();
            self.runtime.request_gc(pid);
        }
    }

    /// `erts_internal:garbage_collect/1` — the runtime's **on-demand collection**, which
    /// both `erlang:garbage_collect/0` and `/1`'s `self()` clause reduce to.
    ///
    /// # Ground truth
    ///
    /// `erts/emulator/beam/bif.c:4081-4094` (`erts_internal_garbage_collect_1`): the atom
    /// `minor` requests a relaxed collection, `major` a fullsweep and anything else is
    /// `badarg` (`am_badarg`); the C then runs `erts_garbage_collect` and returns `am_true`.
    ///
    /// The value is the oracle's `true` for `minor` **and** `major` alike: the runtime
    /// records *that* a collection was asked for, and the two modes differ only in the
    /// collector's own escalation policy (`beam_gc::Collector::collect` picks a minor or a
    /// fullsweep from the live set and the growth table), which this BIF does not
    /// second-guess. The latch, the serving points and the honest refusals are
    /// [`Self::request_own_gc`]'s.
    ///
    /// # Errors
    ///
    /// `badarg` for anything but the atoms `minor`/`major` — the C's `am_badarg` — and, from
    /// the caller, the BIF heap's own exhaustion.
    fn erts_garbage_collect(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let mode = *args.first().ok_or(BeamError::BadArg)?;
        match self.atom_name(mode).as_deref() {
            Some("minor") | Some("major") => {}
            _ => return Err(BeamError::BadArg),
        }
        self.request_own_gc();
        self.atom("true")
    }

    // -- erts_internal:term_type/1 -------------------------------------------

    /// `erts_internal:term_type/1` — the internal type name of a term
    /// (`erl_map.c:3336-3417`).
    ///
    /// # Errors
    ///
    /// A boxed family this crate's safe classifier cannot name is a typed
    /// [`unsupported`](crate::error::unsupported) gap — never a substituted name: an
    /// internal `reference` (`beam_term::boxed::is_internal_ref` is `unsafe`) and a
    /// non-byte-aligned bitstring. As every valid term has a type, the C never raises
    /// here.
    fn erts_term_type(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        // `TAG_PRIMARY_IMMED1` (the C's inner switches) and `TAG_PRIMARY_LIST`.
        if is_small(term) {
            return self.atom("fixnum");
        }
        if is_atom(term) {
            return self.atom("atom");
        }
        if is_nil(term) {
            return self.atom("nil");
        }
        if is_catch(term) {
            return self.atom("catch");
        }
        if is_internal_pid(term) {
            return self.atom("pid");
        }
        if is_internal_port(term) {
            return self.atom("port");
        }
        if is_list(term) {
            return self.atom("list");
        }
        // `TAG_PRIMARY_BOXED`: the C switches on the header subtag, which is reachable
        // here only through the safe classifiers of `beam_engine::terms`,
        // `beam_codec::term` and `beam_term::map`. Each probe below matches its own
        // subtag, so none can fire for a family already classified above.
        if terms::tuple_arity(term).is_ok() {
            return self.atom("tuple");
        }
        if terms::float_value(term).is_ok() {
            return self.atom("hfloat");
        }
        if terms::map_entries(term).is_ok() {
            return if terms::is_hashmap(term) {
                self.atom("hashmap")
            } else {
                self.atom("flatmap")
            };
        }
        if terms::is_fun(term) {
            // `is_local_fun(hdr)` = the fun header is neither the C external kind
            // nor the wire-imported one (`erl_fun.h:87-90`).
            return if terms::fun_uniq(term).is_ok() {
                self.atom("fun")
            } else {
                self.atom("export")
            };
        }
        if terms::binary_bytes(term).is_ok() {
            // Every byte-aligned binary — heap, sub or refc — reports `sub_binary`
            // (see the module docs).
            return self.atom("sub_binary");
        }
        // `POS_BIG_SUBTAG` / `NEG_BIG_SUBTAG` (`erl_map.c:3374-3376`).
        if beam_codec::term::big_parts(term).is_ok() {
            return self.atom("bignum");
        }
        // The three `EXTERNAL_*_SUBTAG` families (`erl_map.c:3379-3386`).
        if beam_codec::term::external_pid_parts(term).is_ok() {
            return self.atom("external_pid");
        }
        if beam_codec::term::external_port_parts(term).is_ok() {
            return self.atom("external_port");
        }
        if beam_codec::term::external_ref_parts(term).is_ok() {
            return self.atom("external_reference");
        }
        Err(error::unsupported(
            "erts_internal:term_type/1: boxed family outside the safe M1 classifier (an \
             internal reference, a non-byte-aligned bitstring)",
        ))
    }

    // -- erts_internal:get_dflags/0 ------------------------------------------

    /// `erts_internal:get_dflags/0` — the node's distribution-flag record
    /// (`dist.c:5566-5592`).
    ///
    /// The C returns `ERTS_GLOBAL_LIT_DFLAGS_RECORD`, a build-time literal, whenever the
    /// internal `erts_dflags_test_remove` mask is clear — so the answer is the same fixed
    /// 6-tuple on a non-distributed node and on a `-sname` node alike. The five flag words
    /// are the emitter's own `beam_dist::flags` constants (the single source of truth,
    /// each defined in terms of the individual `DFLAG_*` bits), narrowed to `i64` for
    /// `make_small`: `DFLAG_DIST_DEFAULT` (the whole preferred set), `DFLAG_DIST_MANDATORY`
    /// (what every peer must offer), `DFLAG_DIST_ADDABLE` (`DFLAG_DIST_DEFAULT` under its
    /// other name), `DFLAG_DIST_REJECTABLE` and `DFLAG_DIST_STRICT_ORDER`. The two values
    /// above 2^32 are 60-bit `fixnum`s in the oracle (they are *slots* in a literal
    /// record, not heap-built bignums), so `term_to_binary/1` renders them through the
    /// codec's `SMALL_BIG_EXT` path for a wide fixnum; the unit test pins the whole
    /// image byte-for-byte against the oracle.
    ///
    /// # Errors
    ///
    /// The heap/Term writers' own errors (a full heap). The C never raises.
    fn erts_get_dflags(&mut self) -> Result<Eterm> {
        let tag = self.atom("erts_dflags")?;
        terms::make_tuple(
            self,
            &[
                tag,
                make_small(DFLAG_DIST_DEFAULT as i64),
                make_small(DFLAG_DIST_MANDATORY as i64),
                make_small(DFLAG_DIST_ADDABLE as i64),
                make_small(DFLAG_DIST_REJECTABLE as i64),
                make_small(DFLAG_DIST_STRICT_ORDER as i64),
            ],
        )
    }

    // -- erts_internal:beamfile_chunk/2 --------------------------------------

    /// `erts_internal:beamfile_chunk/2` — a named chunk's payload of a `.beam` image
    /// (`beam_bif_load.c:165-201`).
    ///
    /// The chunk name is a four-element byte list (`read_iff_list`, `:139-163`) matched
    /// against the container's four-byte ASCII tags **case-sensitively**. A chunk that
    /// is absent — or present with a zero-length payload, which the C's
    /// `chunk.size > 0` guard also rejects — answers the atom `undefined`; an image the
    /// container cannot parse (`iff_init`'s `FOR1` assert, or `read_beam_chunks`'s
    /// `BEAM` form-type assert) answers `undefined` too.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when the first argument is not a byte-aligned binary, or
    /// the second is not exactly a four-element byte list — the C's `BADARG` for both.
    fn erts_beamfile_chunk(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let binary = *args.first().ok_or(BeamError::BadArg)?;
        let name = *args.get(1).ok_or(BeamError::BadArg)?;
        let Some(tag) = iff_list_tag(name) else {
            return Err(BeamError::BadArg);
        };
        let bytes = terms::binary_bytes(binary).map_err(|_| BeamError::BadArg)?;
        let payload = match beam_loader::container::parse(bytes) {
            Ok(container) => chunk_payload(&container, ChunkId(tag))
                .filter(|data| !data.is_empty())
                .map(<[u8]>::to_vec),
            Err(_) => None,
        };
        match payload {
            // The payload is built on the runtime's pointer-stable `-eval` heap — the
            // same `beam_codec::term::make_binary` writer `prim_file:read_file/1` uses,
            // which has no 64-byte on-heap limit.
            Some(data) => beam_codec::term::make_binary(self.eval_heap(), &data),
            None => self.atom("undefined"),
        }
    }

    // -- erts_internal:beamfile_module_md5/1 --------------------------------

    /// `erts_internal:beamfile_module_md5/1` — the module checksum of a `.beam` image
    /// (`beam_bif_load.c:203-229`).
    ///
    /// The C reads the whole image with `beamfile_read` and, on success, returns
    /// `erts_new_binary_from_data(sizeof beam.checksum, beam.checksum)` — a **bare**
    /// 16-byte binary; on any read failure it returns the atom `undefined`. This is *not*
    /// `beam_lib:md5/1`'s `{ok, {Module, Md5}}` wrapper (that is Erlang-side sugar over
    /// this BIF plus `module_info`), and the oracle confirms the bare-binary shape:
    /// `erts_internal:beamfile_module_md5(element(2, code:get_object_code(slice)))` ⇒
    /// `<<2,133,170,13,173,204,31,130,161,1,140,211,209,13,116,36>>`.
    ///
    /// The checksum itself is [`crate::context::module_md5`] — the same chunk-layout digest
    /// the `NEW_FUN_EXT` identity uses (`beam_file.c:1751-1829`), *not* a stored value.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when the first argument is not a byte-aligned binary — the C's
    /// `erts_get_aligned_binary_bytes` `BADARG`. An image the digest cannot parse (a
    /// malformed container, or a `FunT` chunk that is not `4 + 24n` bytes) is the C's
    /// `beamfile_read != SUCCESS` case and answers the atom `undefined`, never an error.
    fn erts_beamfile_module_md5(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let binary = *args.first().ok_or(BeamError::BadArg)?;
        let bytes = terms::binary_bytes(binary).map_err(|_| BeamError::BadArg)?;
        match crate::context::module_md5(bytes) {
            // The digest on the runtime's pointer-stable `-eval` heap: the same
            // `beam_codec::term::make_binary` writer `beamfile_chunk/2` uses.
            Ok(md5) => beam_codec::term::make_binary(self.eval_heap(), &md5),
            Err(_) => self.atom("undefined"),
        }
    }

    // -- erts_internal:no_aux_work_threads/0 ---------------------------------
    /// `erts_internal:no_aux_work_threads/0` — the auxiliary work-thread count
    /// (`erl_process.c:9234-9237`, set at `:6219-6223`).
    ///
    /// The C computes `normal_schedulers + 1 + erts_no_dirty_alloc_instances`; the
    /// dirty-alloc term comes from `+dai`, which this runtime does not model, so it is
    /// `0` — exactly the oracle's own default. The scheduler count is this runtime's
    /// (`+S N`, else the processor count); see the module docs for the default-count
    /// divergence.
    ///
    /// # Errors
    ///
    /// None: the answer is always a small integer.
    fn erts_no_aux_work_threads(&mut self) -> Result<Eterm> {
        Ok(make_small(crate::info::schedulers_count(self.runtime) + 1))
    }

    // -- erts_internal:map_to_tuple_keys/1 -----------------------------------

    /// `erts_internal:map_to_tuple_keys/1` — a **flat** map's key tuple
    /// (`erl_map.c:3318-3328`).
    ///
    /// `flatmap_val(mp)->keys` is the map's own keys tuple, i.e. the pairs in
    /// `erts_cmp_flatmap_keys` order (`beam_term::compare::cmp_flatmap_keys`, the
    /// order [`beam_term::map::write_map`] stores them in).
    ///
    /// # Errors
    ///
    /// * a hash map (any map past the flat-map limit of 32 pairs) — the C's bare
    ///   `BADARG`;
    /// * a non-map — the C's structured `error:{badmap, Term}`, carrying the live
    ///   reason term.
    fn erts_map_to_tuple_keys(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        // The C tests `is_flatmap` first, then `is_hashmap` (bare `badarg`), then
        // falls through to `{badmap, Term}`.
        if terms::map_entries(term).is_err() {
            return Err(self.badmap(term));
        }
        if terms::is_hashmap(term) {
            return Err(BeamError::BadArg);
        }
        let keys: Vec<Eterm> = {
            let atoms = self.runtime.code_server.atoms();
            let mut pairs = match beam_term::map::map_pairs(term, atoms) {
                Ok(pairs) => pairs,
                Err(_) => return Err(self.badmap(term)),
            };
            pairs.sort_by(|a, b| beam_term::cmp_flatmap_keys(a.0, b.0, atoms));
            pairs.into_iter().map(|(key, _)| key).collect()
        };
        terms::make_tuple(self, &keys)
    }

    /// The C's `BADMAP` structured raise: `error:{badmap, Term}` with the live
    /// reason term attached (`bif.c`'s `BIF_ERROR(BIF_P, terms, am_badmap, Term)`).
    fn badmap(&mut self, term: Eterm) -> BeamError {
        let term = match self.build_badmap_reason(term) {
            Ok(term) => term,
            Err(error) => return error,
        };
        BeamError::Raise(
            Raise::error(ExitReason::Structured).with_live_reason(LiveReason::new(term)),
        )
    }

    /// The `{badmap, Term}` reason term, built on the context's own heap.
    fn build_badmap_reason(&mut self, term: Eterm) -> Result<Eterm> {
        let tag = self.atom("badmap")?;
        terms::make_tuple(self, &[tag, term])
    }

    // -- erts_internal:binary_to_integer/2 -----------------------------------

    /// `erts_internal:binary_to_integer/2` — `big.c:4231-4256` over
    /// [`chars_to_integer`].
    ///
    /// The base must be a small integer in `2..=36` and the input a byte-aligned
    /// binary; either failure is the C's `am_badarg` **returned as a value**, not a
    /// raise. So is an invalid digit and `am_big` for a magnitude past
    /// `digits_per_small`.
    fn erts_binary_to_integer(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        let base_term = *args.get(1).ok_or(BeamError::BadArg)?;
        if !is_small(base_term) {
            return self.atom("badarg");
        }
        let base = signed_value(base_term);
        if !(2..=36).contains(&base) {
            return self.atom("badarg");
        }
        // `erts_get_aligned_binary_bytes` answers NULL for a non-binary and for a
        // bitstring that is not a whole number of bytes.
        let bytes = match terms::binary_bytes(term) {
            Ok(bytes) => bytes,
            Err(_) => return self.atom("badarg"),
        };
        match chars_to_integer(bytes, base as usize) {
            CharInt::Small(value) => Ok(make_small(value)),
            CharInt::Big => self.atom("big"),
            CharInt::BadArg => self.atom("badarg"),
        }
    }

    // -- erts_internal:list_to_integer/2 -------------------------------------

    /// `erts_internal:list_to_integer/2` — `big.c:4258-4355`.
    ///
    /// Returns the C's `{Value, Rest}` tuple with `Rest` the unconsumed tail, or the
    /// *value* atoms `no_integer`, `not_a_list`, `badarg` or `big`.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] only if the arguments themselves are missing (an
    /// arity violation the engine cannot produce).
    fn erts_list_to_integer(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let list = *args.first().ok_or(BeamError::BadArg)?;
        let base_term = *args.get(1).ok_or(BeamError::BadArg)?;
        if is_nil(list) {
            return self.atom("no_integer");
        }
        if !is_list(list) {
            return self.atom("not_a_list");
        }
        if !is_small(base_term) {
            return self.atom("badarg");
        }
        let base = signed_value(base_term);
        if !(2..=36).contains(&base) {
            return self.atom("badarg");
        }
        let base = base as usize;

        let mut neg = false;
        let mut rest = list;
        match cons_car(rest) {
            Some(car) if is_small(car) && signed_value(car) == i64::from(b'-') => {
                neg = true;
                rest = cons_cdr(rest)?;
            }
            Some(car) if is_small(car) && signed_value(car) == i64::from(b'+') => {
                rest = cons_cdr(rest)?;
            }
            _ => {}
        }

        // Skip zero digits (`while (is_list(list))`).
        let mut n: usize = 0;
        while let Some(car) = cons_car(rest) {
            if !is_small(car) {
                break;
            }
            let c = signed_value(car) as u8;
            if c != b'0' {
                if is_invalid_char(c, base) {
                    if n == 0 {
                        return self.atom("no_integer");
                    }
                    return self.small_and_rest(0, rest);
                }
                break;
            }
            n += 1;
            rest = cons_cdr(rest)?;
        }
        if !is_list(rest) {
            if n == 0 {
                return self.atom("no_integer");
            }
            return self.small_and_rest(0, rest);
        }

        // The digit loop, bounded by the small-integer digit count.
        let digits_per_small = digits_per_small(base);
        let mut ui: i64 = 0;
        n = 0;
        while n <= digits_per_small {
            let Some(car) = cons_car(rest) else { break };
            if !is_small(car) {
                break;
            }
            let c = signed_value(car) as u8;
            if is_invalid_char(c, base) {
                break;
            }
            ui = ui * base as i64 + i64::from(digit_from_base(c));
            n += 1;
            rest = cons_cdr(rest)?;
        }
        if n == 0 {
            return self.atom("no_integer");
        }
        if n > digits_per_small {
            return self.atom("big");
        }
        let value = if neg { -ui } else { ui };
        self.small_and_rest(value, rest)
    }

    /// The C's `TUPLE2(hp, make_small(value), rest)`.
    fn small_and_rest(&mut self, value: i64, rest: Eterm) -> Result<Eterm> {
        terms::make_tuple(self, &[make_small(value), rest])
    }

    // -- erts_internal:time_unit/0, perf_counter_unit/0 ----------------------

    /// `erts_internal:time_unit/0` — `1_000_000_000`
    /// (`ERTS_MONOTONIC_TIME_UNIT`, `erl_time_sup.c:2539`).
    fn erts_time_unit(&mut self) -> Result<Eterm> {
        Ok(make_small(1_000_000_000))
    }

    /// `erts_internal:perf_counter_unit/0` — `1_000_000_000`
    /// (`erl_time_sup.c:2537`).
    fn erts_perf_counter_unit(&mut self) -> Result<Eterm> {
        Ok(make_small(1_000_000_000))
    }

    // -- erts_internal:get_creation/0 ----------------------------------------

    /// `erts_internal:get_creation/0` — `dist.c:5594-5604`.
    ///
    /// The C returns the atom `undefined` when the node's creation is `0` (a
    /// non-distributed node), else the positive creation integer.
    fn erts_get_creation(&mut self) -> Result<Eterm> {
        let creation = self.runtime.dist_node().map_or(0, |node| node.creation());
        if creation == 0 {
            self.atom("undefined")
        } else {
            Ok(make_small(i64::from(creation)))
        }
    }

    // -- erts_internal:is_process_alive/1 ------------------------------------

    /// `erts_internal:is_process_alive/1` — `is_process_alive_1`
    /// (`erl_bif_info.c:4069-4075`).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for anything that is not an internal pid — the C's
    /// `BIF_ERROR(BIF_P, BADARG)`.
    ///
    /// The `/2` form is served separately by [`Self::erts_is_process_alive_2`]
    /// (`erl_bif_info.c:4058`), which is the reference-carrying variant Erlang's `/1`
    /// wrapper is built on.
    fn erts_is_process_alive(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        let pid = ProcessId::from_eterm(term).map_err(|_| BeamError::BadArg)?;
        if self.process_table_mut().contains(pid) {
            self.atom("true")
        } else {
            self.atom("false")
        }
    }

    // -- erts_internal:is_process_alive/2 ------------------------------------

    /// `erts_internal:is_process_alive/2` — `erl_bif_info.c:4058-4067`.
    ///
    /// The C's contract is an **argument test plus a signal send**: it rejects
    /// `!is_internal_pid(Arg1) || !is_internal_ordinary_ref(Arg2)` with `badarg` and then
    /// queues an is-alive request; the BIF's own return value is the atom `ok` in exactly
    /// those cases. A pid the process table does **not** hold still answers `ok` — the C
    /// never consults the table here, it just sends the request, and the reply (if any)
    /// arrives as a message; probing OTP 29.1.1 / erts-17.1 confirms that for a live and a
    /// dead pid alike, while a non-pid or a non-reference first/second argument is
    /// `error:badarg`.
    ///
    /// The request itself is signal/distribution machinery this runtime has no peer for;
    /// `erlang:is_process_alive/1` — the table lookup above, which is the form every caller
    /// uses — is the answer this runtime serves for "is it alive".
    ///
    /// # Why the reference test is the runtime's own registry
    ///
    /// Reading a reference thing's payload to classify it is `unsafe`
    /// (`beam_term::boxed_val`) and this crate is `#![deny(unsafe_code)]`. The safe
    /// substitute is the same one `erlang:cancel_timer/1` and `erts_internal:group_leader/3`
    /// share: [`RuntimeContext::is_runtime_reference`], i.e. "a reference **this runtime
    /// minted**" (`Runtime::references`, plus the timer reference space). Every internal
    /// ordinary reference in this runtime comes from that one id space, so the test is exact
    /// for the terms a program can hold; any other term is the C's `badarg`.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] unless `Arg1` is an internal pid **and** `Arg2` is a reference
    /// this runtime minted.
    fn erts_is_process_alive_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let pid = *args.first().ok_or(BeamError::BadArg)?;
        let reference = *args.get(1).ok_or(BeamError::BadArg)?;
        if !is_internal_pid(pid) || !self.is_runtime_reference(reference) {
            return Err(BeamError::BadArg);
        }
        self.atom("ok")
    }

    // -- erts_internal:process_flag/3 ----------------------------------------

    /// `erts_internal:process_flag/3` — `bif.c:2262-2297`.
    ///
    /// The C has a three-way split, in this order:
    ///
    /// 1. `Arg1` is the **calling** process (`BIF_P->common.id == BIF_ARG_1`): the body is
    ///    `process_flag_aux(BIF_P, NULL, Flag, Value)` (`bif.c:1896-2022`), and that helper
    ///    implements exactly **one** flag — `save_calls`. `process_flag/2` is *not* the same
    ///    function: it has its own chain for `trap_exit`/`priority`/`sensitive`/`error_handler`
    ///    (…), so the direct form rejects every one of them with `badarg` (probed: the oracle
    ///    answers `error:badarg` for `process_flag(self(), trap_exit, true)` while
    ///    `erlang:process_flag(trap_exit, true)` succeeds). This arm must therefore **not**
    ///    delegate to `erlang:process_flag/2`.
    /// 2. `Arg1` is not an internal pid: the C returns the **value** atom `badtype`
    ///    (`BIF_RET(am_badtype)`, *not* `badarg`) — probed for an atom and for an integer.
    /// 3. any other internal pid: `erts_proc_sig_send_rpc_request(…)` answers an internal
    ///    **reference** and later delivers `{Ref, Result}` to the caller.
    ///
    /// `process_flag_aux`'s `save_calls` semantics, faithfully: a **non-small** value and one
    /// outside `0..=10000` are `badarg`; otherwise the buffer length is set and the *previous*
    /// length is returned (`0` when there was none, so `7` then `4` then `0` answers `0`, `7`,
    /// `4`).
    ///
    /// # The one remaining gap
    ///
    /// Case 3 is the typed capability gap: this runtime has no cross-process RPC with a
    /// reference-keyed reply, so a reference minted here would never be answered — a
    /// fabricated success the workspace error strategy forbids (`../CONTEXT.md` §3).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for the calling process and any flag other than `save_calls`, or
    /// a `save_calls` value that is not a small integer in `0..=10000`; the typed
    /// [`unsupported`](crate::error::unsupported) gap for another process.
    fn erts_process_flag(&mut self, args: &[Eterm]) -> Result<Eterm> {
        /// The async flag RPC (the C's case 3).
        const RPC_GAP: &str = "erts_internal:process_flag/3 targeting another process (the async \
                               flag RPC answers a reference this runtime could never reply to)";
        let pid_term = *args.first().ok_or(BeamError::BadArg)?;
        let flag = *args.get(1).ok_or(BeamError::BadArg)?;
        let value = *args.get(2).ok_or(BeamError::BadArg)?;
        if pid_term == self.pid().to_eterm() {
            // `process_flag_aux`: the direct form implements `save_calls` only.
            if self.atom_name(flag).as_deref() != Some("save_calls") {
                return Err(BeamError::BadArg);
            }
            if !is_small(value) {
                return Err(BeamError::BadArg);
            }
            let next = signed_value(value);
            if !(0..=10_000).contains(&next) {
                return Err(BeamError::BadArg);
            }
            let pid = self.pid();
            let Some(process) = self.process_table_mut().get_mut(pid) else {
                // The calling process has no PCB, so there is nowhere to store the flag. An
                // internal invariant of every dispatch path (the `-eval` root process and
                // every scheduled slice are both in the table) — reported honestly rather
                // than panicking.
                return Err(error::unsupported(
                    "erts_internal:process_flag/3: the calling process has no process-table entry",
                ));
            };
            let previous = process.save_calls();
            process.set_save_calls(next as u16);
            return Ok(make_small(i64::from(previous)));
        }
        if !is_internal_pid(pid_term) {
            // The C's `badtype`: a **value** atom, not a raise.
            return self.atom("badtype");
        }
        Err(error::unsupported(RPC_GAP))
    }

    // -- erts_internal:processes_next/1 --------------------------------------

    /// `erts_internal:processes_next/1` — `bif.c:4111-4120` over
    /// `erts_ptab_processes_next` (`erl_ptab.c:1473`).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a non-small-integer argument, or an index past the
    /// table limit — the C's `badarg` (it has an index space and answers exactly
    /// this way for `first > limit`).
    ///
    /// Milestone-1 divergence: the C's ptab is paginated (it returns a *batch* of
    /// pids per index and `none` past the end); this runtime's process table is a
    /// plain `ProcessId` set with no index space, so the "index" is the position in
    /// table order, one pid per call, and the past-the-end answer is the C's
    /// `{Limit, []}`. `erlang:processes/0` carries the same documented divergence.
    fn erts_processes_next(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        if !is_small(term) {
            return Err(BeamError::BadArg);
        }
        let first = signed_value(term);
        if first < 0 {
            return Err(BeamError::BadArg);
        }
        let pids: Vec<ProcessId> = self.process_table_mut().pids().collect();
        let limit = pids.len() as i64;
        if first > limit {
            return Err(BeamError::BadArg);
        }
        if first == limit {
            // `{ptab->max, []}` — the C's "at the limit, an empty batch".
            let batch = terms::make_list(self, &[])?;
            return terms::make_tuple(self, &[make_small(limit), batch]);
        }
        let pid = pids[first as usize];
        let batch = terms::make_list(self, &[pid.to_eterm()])?;
        terms::make_tuple(self, &[make_small(first + 1), batch])
    }

    // -- erts_internal:term_to_string/1,2 ------------------------------------

    /// `erts_internal:term_to_string/1,2` — `bif.c:4160-4189`.
    ///
    /// The C formats with `"%.*T"`, i.e. the `~w` control with **no** line breaking
    /// and no printable-string collapsing, on a single line and without a trailing
    /// newline. The whole workspace has one printer, so this drives
    /// [`crate::format`]'s `~w` control rather than re-implementing it.
    ///
    /// # Errors
    ///
    /// * [`BeamError::BadArg`] when the limit is a small integer `<= 1`, or neither a
    ///   small integer nor the atom `undefined` (the C's argument check);
    /// * a typed [`unsupported`](crate::error::unsupported) for a **numeric** limit:
    ///   the C's `.*` precision is the `~P` depth directive, which the registered
    ///   printer does not expose (`src/native/CONTEXT.md` records the seam request);
    /// * whatever the printer reports for a term outside its printable set.
    fn erts_term_to_string(&mut self, args: &[Eterm], with_limit: bool) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        if with_limit {
            let limit = *args.get(1).ok_or(BeamError::BadArg)?;
            if is_small(limit) {
                let limit = signed_value(limit);
                if limit <= 1 {
                    return Err(BeamError::BadArg);
                }
                return Err(error::unsupported(
                    "erts_internal:term_to_string/2 with a numeric depth limit (the ~P directive)",
                ));
            }
            match self.atom_name(limit) {
                Some(name) if name == "undefined" => {}
                _ => return Err(BeamError::BadArg),
            }
        }
        let text = self.render(term, b"~w")?;
        let bytes: Vec<Eterm> = text
            .bytes()
            .map(|byte| make_small(i64::from(byte)))
            .collect();
        terms::make_list(self, &bytes)
    }
}

/// The `CAR` of a cons cell, or `None` when `term` is not a cons cell.
fn cons_car(term: Eterm) -> Option<Eterm> {
    terms::cons_head(term).ok()
}

/// The `CDR` of a cons cell; the C's `CDR(list_val(list))`, which the caller has
/// already established is a cons cell (so a non-list is an engine-level invariant
/// violation, reported as `badarg` rather than a panic).
fn cons_cdr(term: Eterm) -> Result<Eterm> {
    terms::cons_tail(term)
}

/// The C's `read_iff_list` (`beam_bif_load.c:139-163`): the four bytes of a
/// well-formed four-element byte list (big-endian, as the C shifts them in), or `None`
/// for the C's `badarg`.
///
/// Exactly four cells, each `CAR` a byte (a small integer in `0..=255`), terminated by
/// `NIL` — so a too-short, too-long, improper, non-byte or non-list name is rejected.
fn iff_list_tag(term: Eterm) -> Option<[u8; 4]> {
    let mut tag = [0u8; 4];
    let mut rest = term;
    for slot in &mut tag {
        if !is_list(rest) {
            return None;
        }
        let octet = terms::cons_head(rest).ok()?;
        if !is_byte(octet) {
            return None;
        }
        *slot = signed_value(octet) as u8;
        rest = terms::cons_tail(rest).ok()?;
    }
    is_nil(rest).then_some(tag)
}

/// The payload of the **last** chunk tagged `id`, or `None` when there is none.
///
/// `read_beam_chunks` (`beam_file.c:1556-1581`) walks the whole directory and keeps
/// overwriting its answer slot, so a duplicate tag resolves to the *last* occurrence —
/// not the first, which is what [`Container::find`] would give. The caller applies the
/// C's `chunk.size > 0` guard (`beam_bif_load.c:190`).
fn chunk_payload<'b>(container: &Container<'b>, id: ChunkId) -> Option<&'b [u8]> {
    container
        .chunks
        .iter()
        .rev()
        .find(|chunk| chunk.id == id)
        .map(|chunk| chunk.data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::RuntimeContext;
    use crate::native::test_support::{atom, call, chars, runtime, string_of, system_runtime};
    use crate::runtime::Runtime;
    use beam_bifs::NullSink;
    use beam_term::{make_internal_pid, make_pid_data, NIL};

    // -- fixtures -------------------------------------------------------------
    //
    // Every test drives the floor through the runtime's own dispatch ([`RuntimeContext::call_bif`]),
    // exactly as the sibling native test modules do, so the tests pin the *served* surface
    // rather than an internal function.

    /// `erts_internal:function(args)` through the runtime's dispatch.
    fn erts(runtime: &mut Runtime, function: &str, args: &[Eterm]) -> Result<Eterm> {
        call(runtime, "erts_internal", function, args)
    }

    /// Builds a term on the runtime's pointer-stable arena through a temporary
    /// context, so the term stays valid for the [`erts`] call that reads it.
    fn heap_term(
        runtime: &mut Runtime,
        build: impl for<'a> FnOnce(&mut RuntimeContext<'a>) -> Result<Eterm>,
    ) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        build(&mut context).expect("a heap term")
    }

    /// Builds a term on the runtime's long-lived `-eval` heap — the heap
    /// `beam_codec::term`'s materializers write through.
    fn eval_heap_term(
        runtime: &mut Runtime,
        build: impl FnOnce(&mut beam_alloc::Heap) -> Result<Eterm>,
    ) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        build(context.eval_heap()).expect("an eval-heap term")
    }

    /// The atom name of an `Ok` answer.
    fn ok_name(runtime: &mut Runtime, function: &str, args: &[Eterm]) -> String {
        let answer = erts(runtime, function, args).expect("an answer");
        runtime
            .atom_name_of(answer)
            .unwrap_or_else(|| panic!("{answer:?} is not an atom"))
            .to_string()
    }

    /// `term_type/1`, as the oracle's atom name.
    fn type_of(runtime: &mut Runtime, term: Eterm) -> String {
        ok_name(runtime, "term_type", &[term])
    }

    /// A map built from `pairs` by the runtime's own layout choice (flat ≤ 32 keys,
    /// else a hashmap).
    fn map(runtime: &mut Runtime, pairs: &[(Eterm, Eterm)]) -> Eterm {
        let pairs = pairs.to_vec();
        heap_term(runtime, |ctx| {
            let atoms = ctx.runtime.code_server.atoms_handle();
            terms::make_map(ctx, &pairs, &atoms)
        })
    }

    /// The names of a tuple's atom elements.
    fn tuple_atom_names(runtime: &Runtime, term: Eterm) -> Vec<String> {
        let arity = terms::tuple_arity(term).expect("a tuple");
        (0..arity)
            .map(|index| {
                let element = terms::tuple_element(term, index).expect("an element");
                runtime
                    .atom_name_of(element)
                    .expect("an atom element")
                    .to_string()
            })
            .collect()
    }

    /// The `{Value, Rest}` halves of a `list_to_integer/2` answer.
    fn value_and_rest(term: Eterm) -> (i64, Eterm) {
        assert_eq!(terms::tuple_arity(term).expect("a 2-tuple"), 2);
        let value = terms::tuple_element(term, 0).expect("the value");
        let rest = terms::tuple_element(term, 1).expect("the rest");
        (signed_value(value), rest)
    }

    /// A map of `n` distinct small-integer keys (`0..n`) — a hashmap for `n > 32`.
    fn numbered_map(runtime: &mut Runtime, n: i64) -> Eterm {
        let pairs: Vec<(Eterm, Eterm)> = (0..n).map(|i| (make_small(i), make_small(i))).collect();
        map(runtime, &pairs)
    }

    /// `bytes` as a binary on the runtime's long-lived `-eval` heap.
    ///
    /// Built by the same `beam_codec::term::make_binary` the `beamfile_chunk/2`
    /// implementation uses, so a whole `.beam` image — far past
    /// `beam_engine::terms::make_heap_binary`'s 64-byte on-heap limit — is
    /// representable.
    fn big_binary(runtime: &mut Runtime, bytes: &[u8]) -> Eterm {
        eval_heap_term(runtime, |heap| beam_codec::term::make_binary(heap, bytes))
    }

    /// A minimal `FOR1`/`BEAM` container holding `(tag, payload)` chunks, padded to the
    /// 4-byte boundary exactly as the wire format requires.
    ///
    /// This only *builds* bytes; the production path still parses them through
    /// `beam_loader::container` (no second parser exists).
    fn iff_image(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (tag, payload) in chunks {
            body.extend_from_slice(*tag);
            body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            body.extend_from_slice(payload);
            body.extend(std::iter::repeat(0u8).take((4 - (payload.len() % 4)) % 4));
        }
        let mut image = Vec::new();
        image.extend_from_slice(b"FOR1");
        image.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
        image.extend_from_slice(b"BEAM");
        image.extend_from_slice(&body);
        image
    }

    /// The system `lists.beam` image — what the oracle's
    /// `element(2, code:get_object_code(lists))` returns — or `None`, in which case the
    /// caller skips **loudly**.
    fn lists_beam(runtime: &mut Runtime) -> Option<Vec<u8>> {
        let Some(path) = runtime.find_module_file("lists") else {
            eprintln!("SKIP: the system `lists.beam` is not on the code path");
            return None;
        };
        match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                eprintln!("SKIP: cannot read {}: {error}", path.display());
                None
            }
        }
    }
    // -- erts_internal:term_type/1 (oracle: OTP 29.1.1 / erts-17.1) ------------

    /// Oracle: `erts_internal:term_type(T)` for `1`/`a`/`{x}`/`#{a=>1}`/`self()`/`1.0`/`[]`/
    /// `<<1,2>>`/`[1,2]` ⇒ `fixnum`/`atom`/`tuple`/`flatmap`/`pid`/`hfloat`/`nil`/`sub_binary`/`list`.
    #[test]
    fn term_type_names_the_immediate_and_common_boxed_families() {
        let mut runtime = runtime();
        assert_eq!(type_of(&mut runtime, make_small(1)), "fixnum");
        assert_eq!(type_of(&mut runtime, NIL), "nil");

        let a = atom(&mut runtime, "a");
        assert_eq!(type_of(&mut runtime, a), "atom");

        let x = atom(&mut runtime, "x");
        let tuple = heap_term(&mut runtime, |ctx| terms::make_tuple(ctx, &[x]));
        assert_eq!(type_of(&mut runtime, tuple), "tuple");

        let flat = map(&mut runtime, &[(a, make_small(1))]);
        assert_eq!(type_of(&mut runtime, flat), "flatmap");

        let me = call(&mut runtime, "erlang", "self", &[]).expect("self/0");
        assert_eq!(type_of(&mut runtime, me), "pid");

        let float = heap_term(&mut runtime, |ctx| terms::make_float(ctx, 1.0));
        assert_eq!(type_of(&mut runtime, float), "hfloat");

        let list = heap_term(&mut runtime, |ctx| {
            terms::make_list(ctx, &[make_small(1), make_small(2)])
        });
        assert_eq!(type_of(&mut runtime, list), "list");

        let binary = heap_term(&mut runtime, |ctx| terms::make_heap_binary(ctx, &[1, 2]));
        assert_eq!(type_of(&mut runtime, binary), "sub_binary");
    }

    /// Oracle: a map past the 32-key flat-map limit is a `hashmap`,
    /// `#{a=>1}` (below it) a `flatmap`.
    #[test]
    fn term_type_distinguishes_a_flat_map_from_a_hashmap() {
        let mut runtime = runtime();
        let flat = numbered_map(&mut runtime, 32);
        assert_eq!(type_of(&mut runtime, flat), "flatmap");
        let hash = numbered_map(&mut runtime, 40);
        assert_eq!(type_of(&mut runtime, hash), "hashmap");
    }

    /// Oracle: a `Local` closure is `fun`, an `EXPORT_EXT` fun is `export`.
    #[test]
    fn term_type_distinguishes_a_closure_from_an_export() {
        let mut runtime = runtime();
        let closure = heap_term(&mut runtime, |ctx| terms::make_fun(ctx, 0, 7, 1, &[]));
        assert_eq!(type_of(&mut runtime, closure), "fun");

        // `fun erts_internal:term_type/1` — an `EXPORT_EXT` block (no uniq).
        let module = runtime.code_server_atoms_mut().intern("erts_internal");
        let function = runtime.code_server_atoms_mut().intern("term_type");
        let export = eval_heap_term(&mut runtime, |heap| {
            beam_codec::term::make_fun_external(heap, 1, module, function)
        });
        assert_eq!(type_of(&mut runtime, export), "export");
    }

    /// Oracle: `1 bsl 100` ⇒ `bignum`; `binary_to_term(<<131,88,118,0,2,110,49,0,0,0,1,
    /// 0,0,0,2,0,0,0,0>>)` ⇒ `external_pid`; `binary_to_term(<<131,89,118,0,2,110,49,0,
    /// 0,0,1,0,0,0,1>>)` ⇒ `external_port`; `binary_to_term(<<131,90,0,1,118,0,2,110,49,
    /// 0,0,0,1,0,0,0,2>>)` ⇒ `external_reference`.
    #[test]
    fn term_type_names_the_boxed_families_the_safe_api_reaches() {
        let mut runtime = runtime();

        // `1 bsl 100` = 2^36 · 2^64 — a two-limb bignum (`make_big` takes the limbs
        // least-significant first).
        let big = eval_heap_term(&mut runtime, |heap| {
            beam_codec::term::make_big(heap, false, &[0, 1u64 << 36])
        });
        assert_eq!(type_of(&mut runtime, big), "bignum");

        // The three wire `EXTERNAL_*` families, decoded from the oracle's own images.
        for (wire, expected) in [
            (
                &[
                    131u8, 88, 118, 0, 2, 110, 49, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 0,
                ][..],
                "external_pid",
            ),
            (
                &[131, 89, 118, 0, 2, 110, 49, 0, 0, 0, 1, 0, 0, 0, 1][..],
                "external_port",
            ),
            (
                &[131, 90, 0, 1, 118, 0, 2, 110, 49, 0, 0, 0, 1, 0, 0, 0, 2][..],
                "external_reference",
            ),
        ] {
            let image = binary(&mut runtime, wire);
            let term = call(&mut runtime, "erlang", "binary_to_term", &[image])
                .unwrap_or_else(|e| panic!("binary_to_term of {expected}: {e}"));
            assert_eq!(type_of(&mut runtime, term), expected);
        }
    }

    /// The boxed families `beam_term`'s **safe** API cannot name are a typed capability
    /// gap here — never a substituted name (the module docs' rule). Two remain: an
    /// internal `reference` (the oracle answers `reference`) and a non-byte-aligned
    /// bitstring (the oracle answers `sub_binary`).
    #[test]
    fn term_type_reports_a_typed_gap_for_families_the_safe_classifier_cannot_name() {
        let mut runtime = runtime();

        // documented divergence: the oracle answers `reference`.
        let reference = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");
        let error =
            erts(&mut runtime, "term_type", &[reference]).expect_err("a reference is a typed gap");
        assert!(error::unsupported_reason(&error).is_some(), "{error}");

        // documented divergence: the oracle answers `sub_binary` (a non-byte-aligned
        // bitstring is still a binary family member to the C).
        let bits = eval_heap_term(&mut runtime, |heap| {
            beam_codec::term::make_bitstring(heap, &[0b1000_0000], 1)
        });
        let error = erts(&mut runtime, "term_type", &[bits])
            .expect_err("a non-byte-aligned bitstring is a typed gap");
        assert_eq!(
            error::unsupported_reason(&error).as_deref(),
            Some(
                "erts_internal:term_type/1: boxed family outside the safe M1 classifier (an \
                 internal reference, a non-byte-aligned bitstring)"
            )
        );
    }

    // -- erts_internal:map_to_tuple_keys/1 ------------------------------------

    /// Oracle: `map_to_tuple_keys(#{a=>1,b=>2,c=>3})` ⇒ the flat map's own keys tuple,
    /// stored in `erts_cmp_flatmap_keys` order — for atoms that is the **atom index**
    /// order (`erl_utils.h:267-272` compares the raw terms), i.e. the order the atoms
    /// were interned in, not their name order. Interning `c`, `b`, `a` in that order
    /// therefore stores `{c,b,a}` — the corpus case's value (verified against
    /// `erl -noshell -eval '[list_to_atom(X) || X <- ["c","b","a"]], ...'`, which
    /// reproduces it; a bare `#{a=>1,b=>2,c=>3}` literal gets whatever index order the
    /// `-eval` compiler's atom interning happened to produce). `#{}` ⇒ `{}`.
    #[test]
    fn map_to_tuple_keys_answers_a_flat_maps_own_key_tuple() {
        let mut runtime = runtime();
        // Interned in this order, so the indices — and hence the key order — are c < b < a.
        let c = atom(&mut runtime, "c");
        let b = atom(&mut runtime, "b");
        let a = atom(&mut runtime, "a");
        let flat = map(
            &mut runtime,
            &[(a, make_small(1)), (b, make_small(2)), (c, make_small(3))],
        );
        let keys = erts(&mut runtime, "map_to_tuple_keys", &[flat]).expect("a keys tuple");
        assert_eq!(tuple_atom_names(&runtime, keys), ["c", "b", "a"]);

        // Non-atom keys are ordered by term order (oracle: `#{1=>1,2=>2,3=>3}` ⇒ `{1,2,3}`).
        let numbers = map(
            &mut runtime,
            &[
                (make_small(3), make_small(3)),
                (make_small(1), make_small(1)),
                (make_small(2), make_small(2)),
            ],
        );
        let keys = erts(&mut runtime, "map_to_tuple_keys", &[numbers]).expect("a keys tuple");
        assert_eq!(terms::tuple_arity(keys).expect("a tuple"), 3);
        let values: Vec<i64> = (0..3)
            .map(|i| signed_value(terms::tuple_element(keys, i).expect("an element")))
            .collect();
        assert_eq!(values, [1, 2, 3]);

        let empty = map(&mut runtime, &[]);
        let keys = erts(&mut runtime, "map_to_tuple_keys", &[empty]).expect("an empty tuple");
        assert_eq!(terms::tuple_arity(keys).expect("a tuple"), 0);
    }

    /// Oracle: `map_to_tuple_keys(<a >32-key map>)` ⇒ `error:badarg` — the C only
    /// serves the flat layout (`erl_map.c:3318-3328`).
    #[test]
    fn map_to_tuple_keys_rejects_a_hashmap_with_badarg() {
        let mut runtime = runtime();
        let hashmap = numbered_map(&mut runtime, 40);
        assert_eq!(type_of(&mut runtime, hashmap), "hashmap");
        assert!(matches!(
            erts(&mut runtime, "map_to_tuple_keys", &[hashmap]),
            Err(BeamError::BadArg)
        ));
    }

    /// Oracle: `map_to_tuple_keys([1,2])` ⇒ `error:{badmap,[1,2]}` — the structured
    /// reason, carried as the raise's live reason term.
    #[test]
    fn map_to_tuple_keys_raises_the_structured_badmap_reason() {
        let mut runtime = runtime();
        let one = make_small(1);
        let two = make_small(2);
        let list = heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[one, two]));
        let error = erts(&mut runtime, "map_to_tuple_keys", &[list]).expect_err("a non-map");
        assert!(matches!(error, BeamError::Raise(_)), "{error}");

        let reason = error::live_reason_term(&error).expect("the structured reason term");
        assert_eq!(terms::tuple_arity(reason).expect("a 2-tuple"), 2);
        let tag = terms::tuple_element(reason, 0).expect("the tag");
        assert_eq!(runtime.atom_name_of(tag), Some("badmap"));
        let payload = terms::tuple_element(reason, 1).expect("the offending term");
        assert_eq!(
            terms::list_elements(payload).expect("a list"),
            vec![one, two]
        );
    }

    // -- erts_internal:binary_to_integer/2 ------------------------------------

    /// A binary of `bytes`, built on the runtime's arena.
    fn binary(runtime: &mut Runtime, bytes: &[u8]) -> Eterm {
        heap_term(runtime, |ctx| terms::make_heap_binary(ctx, bytes))
    }

    /// Oracle: `<<"ff">>,16` ⇒ `255`; `<<"-ff">>,16` ⇒ `-255`; `<<"+10">>,10` ⇒ `10`.
    #[test]
    fn binary_to_integer_parses_the_oracles_accepted_inputs() {
        let mut runtime = runtime();
        for (bytes, base, expected) in [
            (&b"ff"[..], 16, 255),
            (b"-ff", 16, -255),
            (b"+10", 10, 10),
            (b"z", 36, 35),
            (b"000", 10, 0),
        ] {
            let input = binary(&mut runtime, bytes);
            let answer = erts(
                &mut runtime,
                "binary_to_integer",
                &[input, make_small(base)],
            )
            .expect("a small integer");
            assert_eq!(signed_value(answer), expected, "{bytes:?} base {base}");
        }
    }

    /// Oracle: every rejection is the **value** atom `badarg`, never a raise —
    /// `<<>>,10`; `<<"10">>,37`; `<<"10">>,1`; `<<"10">>,16.0`; `<<"10">>,[10]`;
    /// `<<" 10">>,10`; and a non-binary first argument.
    #[test]
    fn binary_to_integer_answers_the_badarg_value_for_the_oracles_rejections() {
        let mut runtime = runtime();
        let ten = binary(&mut runtime, b"10");
        let empty = binary(&mut runtime, b"");
        let spaced = binary(&mut runtime, b" 10");
        let float_base = heap_term(&mut runtime, |ctx| terms::make_float(ctx, 16.0));
        let list_base = heap_term(&mut runtime, |ctx| terms::make_list(ctx, &[make_small(10)]));

        assert_eq!(
            ok_name(&mut runtime, "binary_to_integer", &[empty, make_small(10)]),
            "badarg"
        );
        assert_eq!(
            ok_name(&mut runtime, "binary_to_integer", &[ten, make_small(37)]),
            "badarg"
        );
        assert_eq!(
            ok_name(&mut runtime, "binary_to_integer", &[ten, make_small(1)]),
            "badarg"
        );
        assert_eq!(
            ok_name(&mut runtime, "binary_to_integer", &[ten, float_base]),
            "badarg"
        );
        assert_eq!(
            ok_name(&mut runtime, "binary_to_integer", &[ten, list_base]),
            "badarg"
        );
        assert_eq!(
            ok_name(&mut runtime, "binary_to_integer", &[spaced, make_small(10)]),
            "badarg"
        );
        assert_eq!(
            ok_name(
                &mut runtime,
                "binary_to_integer",
                &[make_small(1), make_small(10)]
            ),
            "badarg"
        );
        // An invalid digit for the base is the same value atom (`<<"z">>,10`).
        let z = binary(&mut runtime, b"z");
        assert_eq!(
            ok_name(&mut runtime, "binary_to_integer", &[z, make_small(10)]),
            "badarg"
        );
    }

    /// Oracle: a magnitude past the small-integer digit count is the **value** atom
    /// `big`. `digits_per_small(10)` is `17` (`big.c:4115-4122`), so 18 decimal digits
    /// already answer `big` — as does 19.
    #[test]
    fn binary_to_integer_reports_big_past_the_small_digit_count() {
        let mut runtime = runtime();
        for digits in [&b"999999999999999999"[..], b"1234567890123456789"] {
            let input = binary(&mut runtime, digits);
            assert_eq!(
                ok_name(&mut runtime, "binary_to_integer", &[input, make_small(10)]),
                "big",
                "{digits:?}"
            );
        }
        // 17 digits still fit the small-integer path.
        let seventeen = binary(&mut runtime, b"99999999999999999");
        let answer = erts(
            &mut runtime,
            "binary_to_integer",
            &[seventeen, make_small(10)],
        )
        .expect("a small integer");
        assert_eq!(signed_value(answer), 99_999_999_999_999_999);
    }

    // -- erts_internal:list_to_integer/2 --------------------------------------

    /// Oracle: the accepted inputs answer `{Value, Rest}` — `"10",10` ⇒ `{10,[]}`,
    /// `"+10"` ⇒ `{10,[]}`, `"-10"` ⇒ `{-10,[]}`, `"0012"` ⇒ `{12,[]}`,
    /// `"12ab",10` ⇒ `{12,"ab"}`, `"ffzz",16` ⇒ `{255,"zz"}`, `"Z",36` ⇒ `{35,[]}`.
    #[test]
    fn list_to_integer_parses_and_returns_the_unconsumed_tail() {
        let mut runtime = runtime();
        for (text, base, expected, rest) in [
            ("10", 10, 10, ""),
            ("+10", 10, 10, ""),
            ("-10", 10, -10, ""),
            ("0012", 10, 12, ""),
            ("-0", 10, 0, ""),
            ("12ab", 10, 12, "ab"),
            ("ffzz", 16, 255, "zz"),
            ("Z", 36, 35, ""),
        ] {
            let input = chars(&mut runtime, text);
            let answer = erts(&mut runtime, "list_to_integer", &[input, make_small(base)])
                .expect("a value and a rest");
            let (value, tail) = value_and_rest(answer);
            assert_eq!(value, expected, "{text:?} base {base}");
            assert_eq!(
                string_of(tail),
                rest,
                "{text:?} base {base}: the unconsumed tail"
            );
        }
    }

    /// Oracle: `[]` ⇒ `no_integer`; `"-",10` ⇒ `no_integer`; a first element that is
    /// not a character ⇒ `no_integer`; a non-list ⇒ `not_a_list`; 18 nines ⇒ `big`;
    /// a base outside `2..=36` ⇒ `badarg`.
    #[test]
    fn list_to_integer_answers_no_integer_not_a_list_big_and_badarg() {
        let mut runtime = runtime();

        let empty = chars(&mut runtime, "");
        assert_eq!(
            ok_name(&mut runtime, "list_to_integer", &[empty, make_small(10)]),
            "no_integer"
        );

        let dash = chars(&mut runtime, "-");
        assert_eq!(
            ok_name(&mut runtime, "list_to_integer", &[dash, make_small(10)]),
            "no_integer"
        );

        let foo = atom(&mut runtime, "foo");
        let not_a_char = heap_term(&mut runtime, |ctx| {
            terms::make_list(ctx, &[foo, make_small(1)])
        });
        assert_eq!(
            ok_name(
                &mut runtime,
                "list_to_integer",
                &[not_a_char, make_small(10)]
            ),
            "no_integer"
        );

        assert_eq!(
            ok_name(&mut runtime, "list_to_integer", &[foo, make_small(10)]),
            "not_a_list"
        );

        let eighteen = chars(&mut runtime, "999999999999999999");
        assert_eq!(
            ok_name(&mut runtime, "list_to_integer", &[eighteen, make_small(10)]),
            "big"
        );

        let ten = chars(&mut runtime, "10");
        assert_eq!(
            ok_name(&mut runtime, "list_to_integer", &[ten, make_small(37)]),
            "badarg"
        );
        assert_eq!(
            ok_name(&mut runtime, "list_to_integer", &[ten, make_small(1)]),
            "badarg"
        );
    }

    // -- erts_internal:time_unit/0, perf_counter_unit/0, get_creation/0 -------

    /// Oracle: `time_unit()` and `perf_counter_unit()` both ⇒ `1000000000`
    /// (nanoseconds).
    #[test]
    fn time_units_are_nanoseconds() {
        let mut runtime = runtime();
        let unit = erts(&mut runtime, "time_unit", &[]).expect("time_unit/0");
        assert_eq!(signed_value(unit), 1_000_000_000);
        let perf = erts(&mut runtime, "perf_counter_unit", &[]).expect("perf_counter_unit/0");
        assert_eq!(signed_value(perf), 1_000_000_000);
    }

    /// Oracle: a node that is not distributed answers `undefined`
    /// (`dist.c:5594-5604`; creation `0`).
    #[test]
    fn get_creation_is_undefined_on_a_non_distributed_node() {
        let mut runtime = runtime();
        assert_eq!(ok_name(&mut runtime, "get_creation", &[]), "undefined");
    }

    // -- erts_internal:get_dflags/0 -------------------------------------------

    /// `term_to_binary(erts_internal:get_dflags())`, from
    /// `erl -noshell -eval 'io:format("~p~n",[term_to_binary(erts_internal:get_dflags())]),halt().'`.
    ///
    /// It pins two things at once: the record's exact shape/values and the *encoding* of
    /// the two wide slots (`468283523004` and `17230663572` are 60-bit fixnums in the
    /// oracle, and a fixnum past signed-32-bit goes out as `SMALL_BIG_EXT` with its
    /// magnitude as base-256 bytes, least-significant first — `110,5,0,188,127,223,7,109`
    /// for the first, `110,5,0,148,15,7,3,4` for the second). The two small slots take the
    /// plain `INTEGER_EXT` form (`98,0,128,32,66` = `8396866`, `98,0,0,32,0` = `8192`).
    const ORACLE_DFLAGS_IMAGE: &[u8] = &[
        131, 104, 6, 119, 11, 101, 114, 116, 115, 95, 100, 102, 108, 97, 103, 115, 110, 5, 0, 188,
        127, 223, 7, 109, 110, 5, 0, 148, 15, 7, 3, 4, 110, 5, 0, 188, 127, 223, 7, 109, 98, 0,
        128, 32, 66, 98, 0, 0, 32, 0,
    ];

    /// Oracle: `erts_internal:get_dflags()` ⇒
    /// `{erts_dflags,468283523004,17230663572,468283523004,8396866,8192}` — the identical
    /// record on a non-distributed and on a `-sname` node (`ERTS_GLOBAL_LIT_DFLAGS_RECORD`
    /// is a build-time literal, so the answer does not depend on the node's state).
    #[test]
    fn get_dflags_is_the_oracles_fixed_flag_record() {
        let mut runtime = runtime();
        let dflags = erts(&mut runtime, "get_dflags", &[]).expect("get_dflags/0");
        assert_eq!(terms::tuple_arity(dflags).expect("a 6-tuple"), 6);

        let element = |index: usize| terms::tuple_element(dflags, index).expect("an element");
        assert_eq!(runtime.atom_name_of(element(0)), Some("erts_dflags"));
        let values: Vec<i64> = (1..6).map(|index| signed_value(element(index))).collect();
        assert_eq!(
            values,
            [
                468_283_523_004,
                17_230_663_572,
                468_283_523_004,
                8_396_866,
                8_192
            ]
        );

        // The wire image must be byte-identical to the oracle's (this is what pins the
        // wide slots' encoding).
        let image =
            call(&mut runtime, "erlang", "term_to_binary", &[dflags]).expect("term_to_binary/1");
        assert_eq!(
            terms::binary_bytes(image).expect("a binary"),
            ORACLE_DFLAGS_IMAGE
        );
    }

    // -- erts_internal:beamfile_chunk/2 ---------------------------------------

    /// Oracle: `byte_size(erts_internal:beamfile_chunk(element(2,
    /// code:get_object_code(lists)),"Code"))` ⇒ `30110`; the same call with `"Nope"` ⇒
    /// `undefined` and with the lowercase `"code"` ⇒ `undefined` (the tags match
    /// case-sensitively).
    #[test]
    fn beamfile_chunk_answers_a_named_chunk_of_a_real_beam() {
        let Some(mut runtime) = system_runtime() else {
            return;
        };
        let Some(bytes) = lists_beam(&mut runtime) else {
            return;
        };
        let beam = big_binary(&mut runtime, &bytes);

        let code = chars(&mut runtime, "Code");
        let chunk = erts(&mut runtime, "beamfile_chunk", &[beam, code]).expect("the Code chunk");
        assert_eq!(terms::binary_bytes(chunk).expect("a binary").len(), 30110);

        let nope = chars(&mut runtime, "Nope");
        assert_eq!(
            ok_name(&mut runtime, "beamfile_chunk", &[beam, nope]),
            "undefined"
        );

        let lowercase = chars(&mut runtime, "code");
        assert_eq!(
            ok_name(&mut runtime, "beamfile_chunk", &[beam, lowercase]),
            "undefined"
        );
    }

    /// Oracle: a non-binary first argument, and a name that is not exactly a
    /// four-element byte list (too short, too long, improper, a binary, an out-of-range
    /// element, an atom, `[]`), each raise `error:badarg` — `read_iff_list`
    /// (`beam_bif_load.c:139-163`) / `erts_get_aligned_binary_bytes`.
    #[test]
    fn beamfile_chunk_rejects_a_non_binary_and_a_malformed_name() {
        let mut runtime = runtime();
        let bytes = iff_image(&[(b"Test", b"xy")]);
        let beam = big_binary(&mut runtime, &bytes);

        let five = make_small(5);
        let code = chars(&mut runtime, "Code");
        assert!(matches!(
            erts(&mut runtime, "beamfile_chunk", &[five, code]),
            Err(BeamError::BadArg)
        ));

        for name in ["Cod", "Codee", ""] {
            let name = chars(&mut runtime, name);
            assert!(
                matches!(
                    erts(&mut runtime, "beamfile_chunk", &[beam, name]),
                    Err(BeamError::BadArg)
                ),
                "{name:?}"
            );
        }

        let name_binary = binary(&mut runtime, b"Code");
        assert!(matches!(
            erts(&mut runtime, "beamfile_chunk", &[beam, name_binary]),
            Err(BeamError::BadArg)
        ));

        let name_atom = atom(&mut runtime, "code");
        assert!(matches!(
            erts(&mut runtime, "beamfile_chunk", &[beam, name_atom]),
            Err(BeamError::BadArg)
        ));

        // A four-element list whose first element is not a byte.
        let wide = heap_term(&mut runtime, |ctx| {
            terms::make_list(ctx, &[make_small(256), make_small(111), make_small(100)])
        });
        assert!(matches!(
            erts(&mut runtime, "beamfile_chunk", &[beam, wide]),
            Err(BeamError::BadArg)
        ));

        // A proper four-element list terminated by a non-`NIL` tail.
        let improper = heap_term(&mut runtime, |ctx| {
            let mut list = make_small(65);
            for car in [101, 100, 111, 67] {
                list = terms::make_cons(ctx, make_small(car), list)?;
            }
            Ok(list)
        });
        assert!(matches!(
            erts(&mut runtime, "beamfile_chunk", &[beam, improper]),
            Err(BeamError::BadArg)
        ));
    }

    /// Oracle: an image the container cannot parse — `<<"garbage">>` and
    /// `<<"FOR1",4:32,"NOPE">>` (a non-`BEAM` form type) — answers `undefined`; a chunk
    /// that is present but empty answers `undefined` (the C's `chunk.size > 0` guard,
    /// `beam_bif_load.c:190`); and a **duplicated** tag resolves to the *last*
    /// occurrence (`read_beam_chunks`, `beam_file.c:1556-1581`, keeps overwriting its
    /// slot — the oracle answers `<<"\t\t">>` for a `"Test"` chunk written twice as
    /// `<<1,2,3>>` then `<<9,9>>`).
    #[test]
    fn beamfile_chunk_answers_undefined_for_absent_empty_and_unparseable_chunks() {
        let mut runtime = runtime();

        let garbage = big_binary(&mut runtime, b"garbage");
        let code = chars(&mut runtime, "Code");
        assert_eq!(
            ok_name(&mut runtime, "beamfile_chunk", &[garbage, code]),
            "undefined"
        );

        // A well-formed `FOR1` whose form type is not `BEAM`.
        let non_beam = big_binary(&mut runtime, b"FOR1\x00\x00\x00\x04NOPE");
        assert_eq!(
            ok_name(&mut runtime, "beamfile_chunk", &[non_beam, code]),
            "undefined"
        );

        let empty_chunk = big_binary(&mut runtime, &iff_image(&[(b"Zero", b"")]));
        let zero = chars(&mut runtime, "Zero");
        assert_eq!(
            ok_name(&mut runtime, "beamfile_chunk", &[empty_chunk, zero]),
            "undefined"
        );

        let duplicated = big_binary(
            &mut runtime,
            &iff_image(&[(b"Test", &[1, 2, 3]), (b"Test", &[9, 9])]),
        );
        let test = chars(&mut runtime, "Test");
        let last = erts(&mut runtime, "beamfile_chunk", &[duplicated, test])
            .expect("the duplicated chunk");
        assert_eq!(terms::binary_bytes(last).expect("a binary"), [9, 9]);
    }

    // -- erts_internal:beamfile_module_md5/1 ----------------------------------

    /// Oracle: `erts_internal:beamfile_module_md5(element(2,
    /// code:get_object_code(slice)))` ⇒ `<<2,133,170,13,173,204,31,130,161,1,140,211,209,
    /// 13,116,36>>` — the module checksum as a **bare** 16-byte binary (the C's
    /// `erts_new_binary_from_data`; `beam_lib:md5/1`'s `{ok,{Module,Md5}}` wrapper is the
    /// Erlang-side shape, not this BIF's).
    #[test]
    fn beamfile_module_md5_answers_the_bare_checksum_binary_of_a_real_beam() {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_loader/tests/fixtures/slice.beam"
        ))
        .expect("the slice fixture");
        let mut runtime = runtime();
        let beam = big_binary(&mut runtime, &bytes);

        let md5 = erts(&mut runtime, "beamfile_module_md5", &[beam]).expect("the checksum");
        assert_eq!(
            terms::binary_bytes(md5).expect("a binary"),
            [2, 133, 170, 13, 173, 204, 31, 130, 161, 1, 140, 211, 209, 13, 116, 36],
        );
    }

    /// Oracle: an image `beamfile_read` cannot parse answers `undefined` — `<<"garbage">>`,
    /// a well-formed `FOR1` whose form type is not `BEAM`, and a `FunT` chunk that is not
    /// `4 + 24n` bytes (`BEAMFILE_READ_CORRUPT_LAMBDA_TABLE`). A non-binary first argument
    /// raises `error:badarg` (`erts_get_aligned_binary_bytes`).
    #[test]
    fn beamfile_module_md5_answers_undefined_for_unreadable_images_and_rejects_non_binaries() {
        let mut runtime = runtime();

        let garbage = big_binary(&mut runtime, b"garbage");
        assert_eq!(
            ok_name(&mut runtime, "beamfile_module_md5", &[garbage]),
            "undefined"
        );

        let non_beam = big_binary(&mut runtime, b"FOR1\x00\x00\x00\x04NOPE");
        assert_eq!(
            ok_name(&mut runtime, "beamfile_module_md5", &[non_beam]),
            "undefined"
        );

        // A short `FunT` payload: a *parseable* container whose digest is the corrupt-lambda
        // read failure — the C's `beamfile_read != SUCCESS` ⇒ `undefined`, **not** a raise.
        let corrupt = big_binary(&mut runtime, &iff_image(&[(b"FunT", &[0, 0, 0, 1, 1, 2, 3])]));
        assert_eq!(
            ok_name(&mut runtime, "beamfile_module_md5", &[corrupt]),
            "undefined"
        );

        for bad in [make_small(5), atom(&mut runtime, "foo")] {
            assert!(
                matches!(
                    erts(&mut runtime, "beamfile_module_md5", &[bad]),
                    Err(BeamError::BadArg)
                ),
                "{bad:?}"
            );
        }
    }

    // -- erts_internal:no_aux_work_threads/0 ----------------------------------

    /// Oracle: `erl -noshell -eval 'io:format("~p~n",[erts_internal:no_aux_work_threads()]),
    /// halt().'` ⇒ `17` on this 16-scheduler machine; `erl +S 1 …` ⇒ `2`; `erl +S 3 …`
    /// ⇒ `4` (`erl_process.c:6219-6223`: `normal_schedulers + 1 +
    /// erts_no_dirty_alloc_instances`, the last being `0` without `+dai`).
    #[test]
    fn no_aux_work_threads_is_the_scheduler_count_plus_one() {
        for (configured, expected) in [(1u32, 2i64), (3, 4), (5, 6)] {
            let mut runtime =
                Runtime::new(crate::config::RuntimeConfig::default().with_schedulers(configured))
                    .unwrap_or_else(|e| panic!("+S {configured}: {e}"));
            let answer =
                erts(&mut runtime, "no_aux_work_threads", &[]).expect("no_aux_work_threads/0");
            assert_eq!(signed_value(answer), expected, "+S {configured}");
        }

        // The default follows *this* runtime's own scheduler count — a documented
        // divergence only in that count's basis (`available_parallelism()`, which is
        // affinity/cgroup-aware, vs ERTS's online processor count).
        let mut default = runtime();
        let expected = crate::info::schedulers_count(&default) + 1;
        let answer = erts(&mut default, "no_aux_work_threads", &[]).expect("the default count");
        assert_eq!(signed_value(answer), expected);
    }
    // -- erts_internal:is_process_alive/1 -------------------------------------

    /// Oracle: `is_process_alive(self())` ⇒ `true`; an atom or a binary ⇒
    /// `error:badarg` (only an internal pid is accepted).
    #[test]
    fn is_process_alive_answers_self_and_rejects_non_pids() {
        let mut runtime = runtime();
        let me = call(&mut runtime, "erlang", "self", &[]).expect("self/0");
        assert_eq!(ok_name(&mut runtime, "is_process_alive", &[me]), "true");

        let foo = atom(&mut runtime, "foo");
        assert!(matches!(
            erts(&mut runtime, "is_process_alive", &[foo]),
            Err(BeamError::BadArg)
        ));
        let binary = binary(&mut runtime, &[1, 2]);
        assert!(matches!(
            erts(&mut runtime, "is_process_alive", &[binary]),
            Err(BeamError::BadArg)
        ));
    }

    // -- erts_internal:is_process_alive/2 -------------------------------------

    /// Oracle: `erts_internal:is_process_alive(self(), make_ref())` ⇒ `ok`. The C's
    /// contract is an argument test plus a *queued* is-alive request
    /// (`erl_bif_info.c:4058-4067`), so the BIF's own return value is the atom `ok`
    /// whenever the arguments pass.
    #[test]
    fn is_process_alive_2_answers_ok_for_a_pid_and_a_runtime_reference() {
        let mut runtime = runtime();
        let me = call(&mut runtime, "erlang", "self", &[]).expect("self/0");
        let reference = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");

        let answer = erts(&mut runtime, "is_process_alive", &[me, reference])
            .expect("a pid and a runtime reference");
        assert_eq!(runtime.atom_name_of(answer), Some("ok"));
    }

    /// Oracle: `(foo, Ref)`, `(self(), foo)`, `(self(), <<1>>)`, `(self(), [])` and
    /// `(self(), binary_to_term(<<131,90,…>>))` all ⇒ `error:badarg` — the C tests
    /// `is_internal_pid(Arg1) && is_internal_ordinary_ref(Arg2)`, and a wire
    /// `EXTERNAL_REFERENCE` is *not* an ordinary ref thing either, so this arm's
    /// runtime-reference test agrees with the oracle there (agreement, not a divergence).
    #[test]
    fn is_process_alive_2_rejects_a_non_pid_and_a_non_runtime_reference() {
        let mut runtime = runtime();
        let me = call(&mut runtime, "erlang", "self", &[]).expect("self/0");
        let reference = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");

        let foo = atom(&mut runtime, "foo");
        let not_a_reference = binary(&mut runtime, &[1, 2]);
        assert!(matches!(
            erts(&mut runtime, "is_process_alive", &[foo, reference]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            erts(&mut runtime, "is_process_alive", &[me, foo]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            erts(&mut runtime, "is_process_alive", &[me, not_a_reference]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            erts(&mut runtime, "is_process_alive", &[me, NIL]),
            Err(BeamError::BadArg)
        ));

        // The wire `EXTERNAL_REFERENCE` image the `term_type/1` test also decodes.
        let image = binary(
            &mut runtime,
            &[131, 90, 0, 1, 118, 0, 2, 110, 49, 0, 0, 0, 1, 0, 0, 0, 2],
        );
        let external =
            call(&mut runtime, "erlang", "binary_to_term", &[image]).expect("binary_to_term/1");
        assert!(matches!(
            erts(&mut runtime, "is_process_alive", &[me, external]),
            Err(BeamError::BadArg)
        ));
    }

    /// The discriminating pair, over a pid the process table does **not** hold: `/1`
    /// consults the table and answers `false`, while `/2` never consults it — it is an
    /// argument test plus a queued request, so it answers `ok` for a live and an absent
    /// pid alike (oracle-verified).
    #[test]
    fn is_process_alive_2_answers_ok_where_is_process_alive_1_answers_false() {
        let mut runtime = runtime();
        let reference = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");
        // `MAX_PROCESS_NUMBER` with serial 1 — a well-formed internal pid no process
        // table in this test can hold.
        let absent = make_internal_pid(make_pid_data(1, 0x0FFF_FFFF));

        assert_eq!(
            ok_name(&mut runtime, "is_process_alive", &[absent]),
            "false"
        );
        let answer = erts(&mut runtime, "is_process_alive", &[absent, reference])
            .expect("the /2 form answers ok");
        assert_eq!(runtime.atom_name_of(answer), Some("ok"));
    }

    // -- erts_internal:process_flag/3 -----------------------------------------

    /// Oracle: `erts_internal:process_flag(self(), save_calls, 7)` ⇒ `0`;
    /// `… save_calls, 4` ⇒ `7`; `… save_calls, 0` ⇒ `4` — `process_flag_aux`'s
    /// `save_calls` arm (`bif.c:1896-2022`) installs the buffer length and returns the
    /// **previous** one (`0` when there was none).
    #[test]
    fn process_flag_3_save_calls_answers_the_previous_length() {
        let mut runtime = runtime();
        let me = call(&mut runtime, "erlang", "self", &[]).expect("self/0");
        let save_calls = atom(&mut runtime, "save_calls");

        for (value, previous) in [(7i64, 0i64), (4, 7), (0, 4)] {
            let answer = erts(
                &mut runtime,
                "process_flag",
                &[me, save_calls, make_small(value)],
            )
            .expect("the calling process");
            assert_eq!(signed_value(answer), previous, "save_calls {value}");
        }
    }

    /// The direct form implements **only** `save_calls` — `process_flag/2`'s own flag
    /// chain is a *different* function (`bif.c:2262-2297` vs `:1896-2022`), so the oracle
    /// rejects `trap_exit` here even though `erlang:process_flag(trap_exit, true)`
    /// succeeds. A `save_calls` value that is not a small integer (`true`) or is outside
    /// `0..=10000` (`-1`, `10001`) is `error:badarg`; the top of the range is accepted.
    #[test]
    fn process_flag_3_is_the_bare_process_flag_aux_surface() {
        let mut runtime = runtime();
        let me = call(&mut runtime, "erlang", "self", &[]).expect("self/0");
        let save_calls = atom(&mut runtime, "save_calls");
        let trap_exit = atom(&mut runtime, "trap_exit");
        let true_ = atom(&mut runtime, "true");

        assert!(matches!(
            erts(&mut runtime, "process_flag", &[me, trap_exit, true_]),
            Err(BeamError::BadArg)
        ));
        for value in [true_, make_small(-1), make_small(10_001)] {
            assert!(
                matches!(
                    erts(&mut runtime, "process_flag", &[me, save_calls, value]),
                    Err(BeamError::BadArg)
                ),
                "save_calls {value:?}"
            );
        }

        // Rejections above are side-effect free, so the buffer is still empty.
        let answer = erts(
            &mut runtime,
            "process_flag",
            &[me, save_calls, make_small(10_000)],
        )
        .expect("10000 is in range");
        assert_eq!(signed_value(answer), 0);
    }

    /// Oracle: a **non-pid** first argument is the *value* atom `badtype`
    /// (`BIF_RET(am_badtype)`, `bif.c:2290-2293`) — not a raise, and not `badarg` — for
    /// an atom and for an integer alike.
    #[test]
    fn process_flag_3_answers_badtype_for_a_non_pid() {
        let mut runtime = runtime();
        let save_calls = atom(&mut runtime, "save_calls");
        let one = make_small(1);

        for not_a_pid in [atom(&mut runtime, "foo"), make_small(123)] {
            let answer = erts(&mut runtime, "process_flag", &[not_a_pid, save_calls, one])
                .expect("badtype is a value, not a raise");
            assert_eq!(runtime.atom_name_of(answer), Some("badtype"));
        }
    }

    /// documented divergence: another **internal** pid is the C's async flag RPC
    /// (`erts_proc_sig_send_rpc_request`, `bif.c:2284-2289`), whose oracle answer is an
    /// internal reference (`#Ref<…>`, `erts_internal:process_flag(Child, save_calls, 3)`
    /// ⇒ e.g. `#Ref<0.2788243652.3908304899.166571>`) with `{Ref, Result}` delivered to
    /// the caller afterwards. This runtime has no cross-process RPC with a
    /// reference-keyed reply, so a reference minted here could never be answered; it
    /// reports the typed capability gap instead of fabricating one.
    #[test]
    fn process_flag_3_reports_the_async_rpc_gap_for_another_process() {
        let mut runtime = runtime();
        let save_calls = atom(&mut runtime, "save_calls");
        // A well-formed internal pid this process table does not hold (and never `self()`).
        let other = make_internal_pid(make_pid_data(1, 0x0FFF_FFFF));

        let error = erts(
            &mut runtime,
            "process_flag",
            &[other, save_calls, make_small(1)],
        )
        .expect_err("another process is the async-RPC gap");
        assert!(error::unsupported_reason(&error).is_some(), "{error}");
    }

    // -- erts_internal:processes_next/1 ---------------------------------------

    /// Oracle: `processes_next(1000000000000)` ⇒ `error:badarg` (past the table limit)
    /// and `processes_next(foo)` ⇒ `error:badarg` (not an index).
    #[test]
    fn processes_next_rejects_a_huge_index_and_a_non_integer() {
        let mut runtime = runtime();
        assert!(matches!(
            erts(
                &mut runtime,
                "processes_next",
                &[make_small(1_000_000_000_000)]
            ),
            Err(BeamError::BadArg)
        ));
        let foo = atom(&mut runtime, "foo");
        assert!(matches!(
            erts(&mut runtime, "processes_next", &[foo]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            erts(&mut runtime, "processes_next", &[make_small(-1)]),
            Err(BeamError::BadArg)
        ));
    }

    /// `processes_next(0)` walks **this** runtime's process table (documented
    /// divergence: one pid per call instead of the C ptab's paginated batch), and an
    /// index at the limit is the C's `{Limit, []}`.
    #[test]
    fn processes_next_walks_this_runtimes_process_table() {
        let mut runtime = runtime();
        let answer =
            erts(&mut runtime, "processes_next", &[make_small(0)]).expect("the first batch");
        assert_eq!(terms::tuple_arity(answer).expect("a 2-tuple"), 2);
        let index = terms::tuple_element(answer, 0).expect("the index");
        assert_eq!(signed_value(index), 1, "the next index");
        let batch = terms::list_elements(terms::tuple_element(answer, 1).expect("the batch"))
            .expect("a list");
        assert_eq!(batch.len(), 1, "one pid per call (documented divergence)");
        assert_eq!(batch[0], runtime.self_pid().to_eterm());

        let all = call(&mut runtime, "erlang", "processes", &[]).expect("processes/0");
        let limit = terms::list_elements(all).expect("a list").len() as i64;
        let answer =
            erts(&mut runtime, "processes_next", &[make_small(limit)]).expect("at the limit");
        assert_eq!(
            terms::tuple_element(answer, 0)
                .map(signed_value)
                .expect("the index"),
            limit
        );
        assert!(
            terms::list_elements(terms::tuple_element(answer, 1).expect("the batch"))
                .expect("a list")
                .is_empty(),
            "the C answers an empty batch at the limit"
        );
    }

    // -- erts_internal:term_to_string/1,2 -------------------------------------

    /// Oracle: `term_to_string({a,1})` ⇒ `"{a,1}"` (a string, one line, no newline),
    /// and the `undefined`-limit `/2` form answers the same.
    #[test]
    fn term_to_string_renders_one_line_with_the_w_control() {
        let mut runtime = runtime();
        let a = atom(&mut runtime, "a");
        let tuple = heap_term(&mut runtime, |ctx| {
            terms::make_tuple(ctx, &[a, make_small(1)])
        });

        let text = erts(&mut runtime, "term_to_string", &[tuple]).expect("term_to_string/1");
        assert_eq!(string_of(text), "{a,1}");

        let undefined = atom(&mut runtime, "undefined");
        let text = erts(&mut runtime, "term_to_string", &[tuple, undefined]).expect("the /2 form");
        assert_eq!(string_of(text), "{a,1}");
    }

    /// Oracle: a limit of `1` ⇒ `error:badarg`; a limit that is neither a small integer
    /// nor `undefined` ⇒ `error:badarg`.
    #[test]
    fn term_to_string_rejects_a_limit_of_one_and_a_non_numeric_limit() {
        let mut runtime = runtime();
        let a = atom(&mut runtime, "a");
        let tuple = heap_term(&mut runtime, |ctx| {
            terms::make_tuple(ctx, &[a, make_small(1)])
        });

        assert!(matches!(
            erts(&mut runtime, "term_to_string", &[tuple, make_small(1)]),
            Err(BeamError::BadArg)
        ));
        let foo = atom(&mut runtime, "foo");
        assert!(matches!(
            erts(&mut runtime, "term_to_string", &[tuple, foo]),
            Err(BeamError::BadArg)
        ));
    }

    /// documented divergence: the oracle truncates to the numeric depth limit
    /// (`term_to_string({a,1},4)` ⇒ `"{a,1}... "`); this runtime reports a typed
    /// capability gap, because the `~P` depth directive is not exposed by the
    /// workspace's one printer (`src/native/CONTEXT.md` records the seam request).
    #[test]
    fn term_to_string_reports_a_typed_gap_for_a_numeric_depth_limit() {
        let mut runtime = runtime();
        let a = atom(&mut runtime, "a");
        let tuple = heap_term(&mut runtime, |ctx| {
            terms::make_tuple(ctx, &[a, make_small(1)])
        });
        let error = erts(&mut runtime, "term_to_string", &[tuple, make_small(4)])
            .expect_err("the numeric limit is a typed gap");
        assert_eq!(
            error::unsupported_reason(&error).as_deref(),
            Some("erts_internal:term_to_string/2 with a numeric depth limit (the ~P directive)")
        );
    }

    // -- the dispatch contract -----------------------------------------------

    /// The floor must not claim the `erts_internal` BIF `beam_bifs` owns: `cmp_term/2`
    /// answers `None` here and the registry serves it (exactly the oracle's `-1 | 0 | 1`).
    #[test]
    fn cmp_term_falls_through_to_the_registry() {
        let mut runtime = runtime();
        let less =
            erts(&mut runtime, "cmp_term", &[make_small(1), make_small(2)]).expect("cmp_term/2");
        assert_eq!(signed_value(less), -1);
        let greater =
            erts(&mut runtime, "cmp_term", &[make_small(2), make_small(1)]).expect("cmp_term/2");
        assert_eq!(signed_value(greater), 1);
        let equal =
            erts(&mut runtime, "cmp_term", &[make_small(1), make_small(1)]).expect("cmp_term/2");
        assert_eq!(signed_value(equal), 0);
    }

    /// The floor must not claim the names owned **elsewhere** — a claimed arm here would
    /// shadow the `beam_bifs` registry (or, for `is_system_process/1` and
    /// `garbage_collect/1`, the `RuntimeHooks` bridge behind it), which is exactly why the
    /// dispatch order is floor → registry → module load. The assertion is on the floor's
    /// own [`RuntimeContext::erts_internal_call`], so it stays exact even if the registry's
    /// coverage grows or shrinks.
    #[test]
    fn the_floor_does_not_claim_names_served_elsewhere() {
        let mut runtime = runtime();
        for (function, arity) in [
            ("is_system_process", 1),
            ("map_hashmap_children", 1),
            ("map_next", 3),
            ("counters_new", 1),
            ("counters_get", 2),
            ("counters_add", 3),
            ("counters_put", 3),
            ("counters_info", 1),
        ] {
            let mut sink = NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            assert!(
                context.erts_internal_call(function, arity, &[]).is_none(),
                "erts_internal:{function}/{arity} must fall through to the registry"
            );
        }
    }

    /// The fall-through above is not merely "not owned": the registry serves the
    /// `counters` family for real, so the same call succeeds through it with the oracle's
    /// shape.
    ///
    /// Oracle: `C = counters:new(2, [])`, `counters:get(C,1)` ⇒ `0`,
    /// `counters:put(C,1,5)` ⇒ `ok`, `counters:add(C,1,3)` ⇒ `ok`,
    /// `counters:info(C)` ⇒ `#{max => …, memory => …, min => …, size => 2}` (the
    /// `max`/`min` pair belongs to `atomics:info/1`). The `memory` figure is this
    /// runtime's own ERTS-model approximation, so only its presence is pinned.
    #[test]
    fn counters_calls_fall_through_to_the_registry() {
        let mut runtime = runtime();
        let counters =
            erts(&mut runtime, "counters_new", &[make_small(2)]).expect("counters_new/1");
        let one = make_small(1);

        let answer = erts(&mut runtime, "counters_get", &[counters, one]).expect("counters_get/2");
        assert_eq!(signed_value(answer), 0, "every counter starts at 0");

        let answer = erts(
            &mut runtime,
            "counters_put",
            &[counters, one, make_small(5)],
        )
        .expect("put/3");
        assert_eq!(runtime.atom_name_of(answer), Some("ok"));
        let answer = erts(
            &mut runtime,
            "counters_add",
            &[counters, one, make_small(3)],
        )
        .expect("add/3");
        assert_eq!(runtime.atom_name_of(answer), Some("ok"));
        let answer = erts(&mut runtime, "counters_get", &[counters, one]).expect("counters_get/2");
        assert_eq!(signed_value(answer), 8, "5 + 3");

        let info = erts(&mut runtime, "counters_info", &[counters]).expect("counters_info/1");
        let entries = terms::map_entries(info).expect("a flat map");
        let mut size = None;
        let mut has_memory = false;
        for (key, value) in &entries {
            match runtime.atom_name_of(*key) {
                Some("size") => size = Some(signed_value(*value)),
                Some("memory") => has_memory = true,
                _ => {}
            }
        }
        assert_eq!(size, Some(2));
        assert!(has_memory, "the memory key is present");
    }

    /// `erts_internal:garbage_collect/1` answers the oracle's `true` for both modes and
    /// raises `badarg` for anything else — and, precisely because it is **not** a scheduled
    /// slice, it records no collection request (there is no movable heap for this context).
    ///
    /// Oracle (`erl -noshell -eval '…'`): `erts_internal:garbage_collect(minor)` ⇒ `true`,
    /// `(major)` ⇒ `true`, `(foo)` ⇒ `error:badarg`, `(1)` ⇒ `error:badarg`.
    #[test]
    fn garbage_collect_answers_the_oracle_and_only_records_a_request_for_a_scheduled_process() {
        let mut runtime = runtime();
        let minor = atom(&mut runtime, "minor");
        let major = atom(&mut runtime, "major");
        let foo = atom(&mut runtime, "foo");

        let answer = erts(&mut runtime, "garbage_collect", &[minor]).expect("minor is served");
        assert_eq!(runtime.atom_name_of(answer), Some("true"), "the oracle's `true`");
        let answer = erts(&mut runtime, "garbage_collect", &[major]).expect("major is served");
        assert_eq!(runtime.atom_name_of(answer), Some("true"), "the oracle's `true`");

        for bad in [foo, make_small(1)] {
            let error = erts(&mut runtime, "garbage_collect", &[bad])
                .expect_err("only the two mode atoms are accepted");
            assert!(
                matches!(error, BeamError::BadArg),
                "the C's `am_badarg`; got {error:?}"
            );
        }

        // Not a scheduled slice (no process table, no movable heap): nothing was recorded
        // and nothing was — or could honestly be — collected.
        assert_eq!(
            runtime.gc_stats().collections,
            0,
            "a non-scheduled context owns no movable heap to collect"
        );
    }

    /// `erlang:garbage_collect/0` answers the oracle's `true` and records **no** request —
    /// this context is not a scheduled slice, so it owns no movable heap to collect.
    ///
    /// Oracle (`erl -noshell -eval 'erlang:garbage_collect()'`): `true`.
    #[test]
    fn erlang_garbage_collect_0_answers_the_oracles_true() {
        let mut runtime = runtime();
        let answer =
            call(&mut runtime, "erlang", "garbage_collect", &[]).expect("the oracle's `true`");
        assert_eq!(runtime.atom_name_of(answer), Some("true"));
        assert_eq!(
            runtime.gc_stats().collections,
            0,
            "a non-scheduled context owns no movable heap to collect"
        );
    }

    /// `erlang:garbage_collect/1` mirrors the oracle: a **live local pid** (the caller's own
    /// `self()`) ⇒ `true` and a request recorded for that pid; a well-formed internal pid the
    /// table does not hold ⇒ the oracle's `false` with nothing recorded; anything that is not
    /// an internal pid ⇒ the C's `badarg`.
    ///
    /// Oracle: `garbage_collect(self())` ⇒ `true`, `(foo | 123 | [Pid])` ⇒ `error:badarg`,
    /// and a pid that has already died ⇒ `false`.
    #[test]
    fn erlang_garbage_collect_1_matches_the_oracle() {
        let mut runtime = runtime();
        let me = runtime.self_pid();

        // A live local pid: the oracle's `true`, and the request lands on the **target**.
        let answer = call(&mut runtime, "erlang", "garbage_collect", &[me.to_eterm()])
            .expect("a live local pid is served");
        assert_eq!(runtime.atom_name_of(answer), Some("true"));
        assert!(
            runtime.gc_request_pending(me),
            "the request is recorded for the target pid"
        );

        // A pid the table does not hold: the oracle's `false`, and nothing is pretended.
        let absent = make_internal_pid(make_pid_data(1, 0x0FFF_FFFF));
        let answer = call(&mut runtime, "erlang", "garbage_collect", &[absent]).expect("served");
        assert_eq!(runtime.atom_name_of(answer), Some("false"));
        let absent_pid = ProcessId::from_eterm(absent).expect("a well-formed internal pid");
        assert!(
            !runtime.gc_request_pending(absent_pid),
            "a request that cannot be honoured is never recorded"
        );

        // Anything that is not an internal pid is the C's `am_badarg`.
        let foo = atom(&mut runtime, "foo");
        let not_a_pid = heap_term(&mut runtime, |context| {
            terms::make_list(context, &[me.to_eterm()])
        });
        for bad in [foo, make_small(123), not_a_pid] {
            let error = call(&mut runtime, "erlang", "garbage_collect", &[bad])
                .expect_err("only an internal pid is accepted");
            assert!(matches!(error, BeamError::BadArg), "got {error:?}");
        }
    }

    /// A BIF this module does not serve is `None` from `erts_internal_call`, so the call
    /// falls through — never a fabricated value. On a code-path-less runtime the module
    /// load then fails honestly.
    #[test]
    fn unserved_erts_internal_calls_fall_through_to_a_module_load() {
        let mut runtime = runtime();
        let error = erts(&mut runtime, "not_a_bif", &[make_small(1)]).expect_err("not served");
        assert!(
            error::unsupported_reason(&error).is_none(),
            "a fall-through is not a capability gap: {error}"
        );
    }
}
