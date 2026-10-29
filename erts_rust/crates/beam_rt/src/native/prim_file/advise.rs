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

//! The `prim_file:advise/4` family of the runtime-native `prim_file:*` floor (contract A.7).
//!
//! Serves both the **exported** wrapper's key (`advise/4`, reached by `call_ext`) and the
//! **non-exported** NIF stub `advise_nif/4` that `prim_file.erl`'s own `advise/4` body chains
//! into locally — the second is what `file:sendfile/8` calls before it sends
//! (`prim_file.erl:432-434`).
//!
//! # Ground truth
//!
//! * the C NIF — `advise_nif_impl` (`../../erts/emulator/nifs/common/prim_file_nif.c:854-890`)
//!   behind `WRAP_FILE_HANDLE_EXPORT` (`:319-357`, which consumes `argv[0]`, so the impl sees
//!   `(Offset, Length, Advise)`) and `efile_advise` (`unix_prim_file.c:570-598`);
//! * the Erlang wrapper — `advise/4` (`erts/preloaded/src/prim_file.erl:274-280`), whose
//!   `try … catch error:badarg -> {error, badarg}` is why a `badarg` **raise** from the NIF is
//!   the *value* `{error,badarg}` at the wrapper's caller;
//! * the stub — `advise_nif(_FileRef,_Offset,_Length,_Advise) -> erlang:nif_error(undef)`
//!   (`prim_file.erl:547-548`), listed in `-nifs([…])` (`:70-75`) and therefore **not
//!   exported** (`:28`), so on the oracle it can only be reached from *inside* the module.
//!
//! # The C's answer matrix, and what it means for the two keys
//!
//! `advise_nif_impl` classifies in this order (`:859-887`):
//!
//! 1. `!enif_is_number(Offset) || !enif_is_number(Length)` ⇒ `enif_make_badarg` (a **raise**);
//! 2. `!enif_get_int64(Offset) || !enif_get_int64(Length) || Offset < 0 || Length < 0`
//!    ⇒ `posix_error_to_tuple(env, EINVAL)` — so a **float**, a **bignum** outside the `int64`
//!    range (both numbers to `enif_is_number`, neither convertible by `enif_get_int64`) and a
//!    **negative** integer are `{error,einval}`. A bignum that *does* fit an `int64` is a plain
//!    value to `enif_get_int64` (measured: `advise(Fd,9223372036854775807,
//!    9223372036854775807,normal)` ⇒ `ok`, and that literal is a bignum term because it is past
//!    the 60-bit small-int range) — see [`advise_number`];
//! 3. `Advise` must be one of the six atoms `normal | random | sequential | will_need |
//!    dont_need | no_reuse`, else `{error,einval}` (the C's own comment: "The tests check for
//!    EINVAL instead of badarg. Sigh." — so an unknown advise is **not** a `badarg`);
//! 4. `efile_advise` failure ⇒ `{error,<errno>}`, else the atom `ok`.
//!
//! **`posix_fadvise(2)` is never called.** It is a *hint*: a live descriptor answers `ok` on
//! Linux whatever the advice is, and `efile_advise` cannot fail for a descriptor this runtime
//! hands out (`posix_fadvise` reports by *returning* the error number, which the C's `< 0` test
//! does not even see — `unix_prim_file.c:587`). Serving `ok` for a valid descriptor is
//! therefore the oracle's own observable behaviour, not an approximation, and it keeps this
//! module `unsafe`-free (`posix_fadvise` is not reachable from `std`).
//!
//! # The descriptor argument: the oracle's decomposition, reproduced
//!
//! `advise/4` is `try #{handle := FRef} = get_fd_data(Fd), advise_nif(FRef, Offset, Length,
//! Advise) catch error:badarg -> {error, badarg} end` (`prim_file.erl:274-280`) over
//! `get_fd_data(#file_descriptor{data = Data}) -> #{owner := Owner} = Data, case self() of
//! Owner -> Data; _ -> error(not_on_controlling_process) end` (`:499-504`), so the malformed
//! shapes are raises the wrapper's `catch error:badarg` does **not** take. Measured on the
//! host oracle (OTP 29.1.2 / erts-17.1.1), `prim_file:advise(Fd, 0, 10, normal)` with:
//!
//! * `42`, `foo`, `[]`, `#{}`, `{a,b}`, a 4-tuple, `{nope,prim_file,#{}}` ⇒
//!   `error:function_clause` — the **clause head** of `get_fd_data/1` fails before any body
//!   runs, so nothing is caught;
//! * `{file_descriptor,prim_file,nope}`, `{file_descriptor,prim_file,#{}}`, and a live
//!   descriptor's data map with `owner` (or `handle`) removed ⇒ `error:{badmatch, Data}`
//!   (the `#{owner := Owner} = Data` match, then the wrapper's own `#{handle := FRef} = …`);
//! * `{file_descriptor,prim_file,#{owner => self(), handle => make_ref()}}` ⇒ the value
//!   `{error,badarg}`: `enif_get_resource` fails on the forged handle and the wrapper catches
//!   that `badarg`.
//!
//! [`RuntimeContext::prim_advise`] reproduces that order exactly ([`RuntimeContext::
//! advise_descriptor`]), so a program's `try … catch C:R:S` observes the oracle's own
//! `(class, reason)` on every one of them. The **other** `prim_file` entries still answer the
//! uniform `{error,badarg}` value for the first two families — that divergence is theirs
//! (`../prim_file.rs` § "Documented divergences"), not this module's.
//!
//! # Documented divergences
//!
//! * **A closed descriptor is `{error,einval}`, NOT `{error,badarg}`.** `file_handle_wrapper`
//!   tests the `efile` resource's state *before* it calls the operation: a resource in
//!   `EFILE_STATE_CLOSED` answers `posix_error_to_tuple(env, EINVAL)` without reaching
//!   `advise_nif_impl` at all (`prim_file_nif.c:343-347`). Measured on the oracle: after
//!   `prim_file:close(Fd)`, `advise(Fd,0,10,normal)` ⇒ `{error,einval}` — and so does
//!   `advise(Fd,foo,10,normal)`, which is the same state check firing before the argument
//!   checks. This is the oracle's answer for **every** `prim_file` entry and this module
//!   reproduces it; it is *not* the `{error,badarg}` a resource-lookup failure would give.
//!
//! # The local-call stub (`advise_nif/4`)
//!
//! `prim_file.erl` never mentions a `fun` at all (`grep -c 'fun ' erts/preloaded/src/
//! prim_file.erl` ⇒ `0`, so also `grep -c 'fun .*_nif'` ⇒ `0`): the module holds no stub in a
//! record and applies none as a closure, so the record-held `make_fun3` seam
//! (`crate::context::apply::served_local_key`) needs **no** `prim_file` entry. The one key
//! [`LOCAL_KEYS`] serves is [`advise/4`]'s own chain.
//!
//! Exactly **one** `prim_file` stub is served. It is served because it is on
//! `file:sendfile/8`'s path: `prim_file.erl`'s `sendfile/8` body calls
//! `advise(Fd, Offset, Bytes, sequential)` (`:433`) and then `prim_inet:sendfile/4`; that
//! `advise/4` is a **local** call inside `prim_file.beam`, so `call_ext` never sees it and the
//! floor's external `advise/4` arm cannot serve it — the stub is the only seam. Every other
//! `prim_file` `*_nif` stub (`read_nif`, `pwrite_nif`, `allocate_nif`, …) stays **unserved**:
//! their wrappers are served directly by the floor's exported-key arms, so claiming them would
//! add a dispatch path nothing reaches.
//!
//! # Oracle measurements (OTP 29.1.2 / erts-17.1.1, `erl -noshell -eval …`)
//!
//! File `x.txt` = `"0123456789a"`, `{ok,Fd} = prim_file:open(F,[read])`:
//!
//! | expression | oracle |
//! |---|---|
//! | `advise(Fd,0,10,normal)` | `ok` |
//! | `advise(Fd,0,0,normal)` / `advise(Fd,9223372036854775807,9223372036854775807,normal)` | `ok` |
//! | `advise(Fd,0,10,A)` for `A` ∈ the six atoms | `ok` (all six accepted) |
//! | `advise(Fd,-1,10,normal)` / `advise(Fd,0,-1,normal)` | `{error,einval}` |
//! | `advise(Fd,1.0,10,normal)` / `advise(Fd,0,1.5,normal)` | `{error,einval}` |
//! | `advise(Fd,1 bsl 80,10,normal)` / `advise(Fd,0,1 bsl 80,normal)` | `{error,einval}` |
//! | `advise(Fd,foo,10,normal)` / `advise(Fd,0,bar,normal)` | `{error,badarg}` |
//! | `advise(Fd,0,10,foo)` / `advise(Fd,0,10,{normal})` / `advise(Fd,0,10,1)` / `advise(Fd,0,10,[])` | `{error,einval}` |
//! | a `[read,directory]` fd / a `[read,write]` fd | `ok` |
//! | after `close(Fd)`: `advise(Fd,0,10,normal)` / `advise(Fd,foo,10,normal)` / `advise(Fd,0,10,foo)` | `{error,einval}` |
//! | `advise(42,…)` / `advise(foo,…)` / `advise([],…)` / `advise(#{},…)` / `advise({a,b},…)` | `error:function_clause` (the `get_fd_data/1` clause head) |
//! | `advise({file_descriptor,prim_file,nope},…)` / an empty data map / a live data map minus `owner` (or `handle`) | `error:{badmatch, Data}` |
//! | `advise({file_descriptor,prim_file,#{owner=>self(),handle=>make_ref()}},…)` | `{error,badarg}` (the forged handle's `enif_get_resource` failure) |
//! | an fd opened by **another** process | that process dies `{not_on_controlling_process,[{prim_file,get_fd_data,1,[]},{prim_file,advise,4,[]},…]}` |
//!
//! The stub itself is unobservable on the oracle (`prim_file:advise_nif/4` from outside is
//! `{'EXIT',{undef,…}}` — it is not exported), so its matrix is the C's (`:859-887`) plus the
//! wrapper's catch, which is what the two keys below implement.

use beam_common::{BeamError, ExitReason, LiveReason, Raise, Result};
use beam_engine::terms;
use beam_term::Eterm;

use super::{number_arg, Descriptor, NumArg};
use crate::context::RuntimeContext;
use crate::error;
use crate::native::prim::function_clause;

/// The `#file_descriptor{}` record's tag atom (`prim_file.erl`'s
/// `-record(file_descriptor, {module = ?MODULE, data})`), which the clause head of
/// `get_fd_data/1` matches before its body runs.
const FILE_DESCRIPTOR_TAG: &str = "file_descriptor";

/// Every `{function, arity}` of `prim_file`'s **non-exported** NIF stubs this floor serves —
/// the ONE source of truth behind [`serves_local_call`].
///
/// One key only: `advise_nif/4`, the **non-exported** stub that `prim_file.erl`'s own `advise/4`
/// wrapper calls (`:277`, inside the wrapper at `:274-280`) — and therefore the one the chain
/// site `file:sendfile/8` needs, `advise(Fd, Offset, Bytes, sequential)` at
/// `erts/preloaded/src/prim_file.erl:433`. The module's other stubs are reached through the
/// floor's exported-key arms, so serving them locally would add a dispatch path nothing takes —
/// see the module docs.
pub(crate) const LOCAL_KEYS: [(&str, u32); 1] = [("advise_nif", 4)];

/// Whether `(module, function, arity)` is a `prim_file` NIF stub this floor serves — the
/// predicate behind [`beam_engine::ExecutionContext::resolve_local_call`], consulted for
/// **every** local call in every module, so it must answer `false` for everything else.
///
/// Allocation-free by construction: a module-name equality first (the fast path the engine's
/// leading guard mirrors), then a membership test over a one-row constant table.
pub(crate) fn serves_local_call(module: &str, function: &str, arity: u32) -> bool {
    module == "prim_file" && LOCAL_KEYS.contains(&(function, arity))
}

/// The C's three-way classification of `(Offset, Length, Advise)` (`advise_nif_impl`,
/// `prim_file_nif.c:859-887`), shared by both keys so the argument matrix cannot drift between
/// the wrapper route and the stub route.
enum AdviseOutcome {
    /// A live descriptor and a well-formed argument triple ⇒ the C's `am_ok`.
    Advised,
    /// `posix_error_to_tuple(env, EINVAL)` — a number that is not a non-negative `int64`, or an
    /// advise atom outside the six.
    EInval,
    /// `enif_make_badarg(env)` — `Offset`/`Length` is not a number at all.
    BadArg,
}

/// An `(Offset, Length)` operand, classified as the C's `enif_is_number` + `enif_get_int64`
/// pair does — [`super::number_arg`] plus the one refinement that pair needs here.
///
/// The sibling classifier collapses **every** bignum to [`NumArg::EInval`] (`prim_file.rs`'s
/// own note: "a float or a bignum"). `enif_get_int64` does not: it converts a bignum whose
/// value fits an `int64`, which is measurable on the oracle — `advise(Fd,
/// 9223372036854775807, 9223372036854775807, normal)` ⇒ `ok`, and `9223372036854775807` is
/// past the 60-bit small-int range, i.e. a bignum on both runtimes. So a fitting bignum is
/// [`NumArg::Value`] here and only an out-of-range one (or a float) is `EInval`.
///
/// The refinement lives in this module because `prim_file.rs`'s classifier is shared with
/// `pread`/`pwrite` and changing it is a separate change (reported as a follow-up); the
/// behaviour below is the C's, not a divergence.
fn advise_number(term: Eterm) -> NumArg {
    match bignum_value(term) {
        Some(value) => NumArg::Value(value),
        // Not a bignum at all (`big_parts` rejects every other term), so the small-int,
        // float and not-a-number cases are exactly the sibling's.
        None => number_arg(term),
    }
}

/// The `int64` value of a **bignum** term, or `None` when `term` is not one or its value does
/// not fit (`enif_get_int64`'s refusal, which the C maps to `EINVAL`).
///
/// `digits` are `ErtsDigit`s (`u64`) least significant first (`big.h:59-73`), so a value that
/// fits `int64` has at most one non-zero limb; trailing zero limbs are stripped first, since
/// a term built by hand can carry them (`external.c`'s decoder never emits one).
fn bignum_value(term: Eterm) -> Option<i64> {
    let (negative, digits) = beam_codec::term::big_parts(term).ok()?;
    let mut digits = digits;
    while digits.last() == Some(&0) {
        digits = &digits[..digits.len() - 1];
    }
    let [limb] = digits else {
        // Zero limbs (the value `0`), or a second limb — and any second limb is ≥ 2^64.
        return digits.is_empty().then_some(0);
    };
    if negative {
        // `-2^63` is the one negative value whose magnitude does not fit a positive `int64`.
        i64::try_from(i128::from(*limb).checked_neg()?).ok()
    } else {
        i64::try_from(*limb).ok()
    }
}

impl RuntimeContext<'_> {
    /// `prim_file:advise/4` — the exported wrapper's key, served directly by the floor
    /// (`prim_file.erl:274-280`), answering what the wrapper's `try … catch` answers.
    ///
    /// The wrapper is `try #{handle := FRef} = get_fd_data(Fd), advise_nif(FRef, Offset,
    /// Length, Advise) catch error:badarg -> {error, badarg} end` over
    /// `get_fd_data(#file_descriptor{data = Data}) -> #{owner := Owner} = Data, case self() of
    /// Owner -> Data; _ -> error(not_on_controlling_process) end` (`:499-504`), and the
    /// argument is decomposed in exactly the order that Erlang evaluates it
    /// ([`Self::advise_descriptor`]), so:
    ///
    /// 1. a term that is not a `#file_descriptor{}` fails the **clause head** of
    ///    `get_fd_data/1` ⇒ the uncaught raise `error:function_clause` (the wrapper's
    ///    `catch error:badarg` does not take it) — measured on the host oracle for `42`, `foo`,
    ///    `[]`, `#{}`, `{a,b}`, a 4-tuple and `{nope,prim_file,#{}}`;
    /// 2. a `#file_descriptor{}` whose `data` is not a map, or carries no `owner` ⇒ the
    ///    `#{owner := Owner} = Data` **badmatch** ⇒ `error:{badmatch, Data}`, with the data
    ///    term as the live reason;
    /// 3. the recorded `owner` must be the caller, else the **raise**
    ///    `error:not_on_controlling_process` — `get_fd_data/1`'s own `error/1`
    ///    (`prim_file.erl:499-504`), which the wrapper's `catch error:badarg` does **not**
    ///    catch either, so it must stay a raise here too;
    /// 4. a `data` map carrying no `handle` ⇒ the wrapper's own `#{handle := FRef} = …`
    ///    badmatch ⇒ `error:{badmatch, Data}`;
    /// 5. a well-shaped descriptor whose handle is **not** one of this runtime's — a forged
    ///    `#{owner => self(), handle => make_ref()}` — is the value `{error,badarg}`: the C's
    ///    `enif_get_resource` fails on it and the wrapper catches that `badarg` (measured: the
    ///    oracle answers exactly that value);
    /// 6. a descriptor whose slot left the open-file table (a **closed** one) is
    ///    `{error,einval}`: the resource-state check precedes the operation
    ///    (`prim_file_nif.c:343-347`), measured on the oracle. This is the one entry where a
    ///    closed answer is worth stating explicitly, because the *next* check is the one that
    ///    would otherwise make it a `badarg`;
    /// 7. then the C's matrix: a non-number `Offset`/`Length` ⇒ the value `{error,badarg}`
    ///    (the wrapper's catch of the NIF's `badarg` raise), a float / a bignum outside the
    ///    `int64` range / a negative ⇒ `{error,einval}`, an advise atom outside the six
    ///    ⇒ `{error,einval}` (a **fitting** bignum is a value to the C's `enif_get_int64`, so it
    ///    is `ok` — measured, and handled by [`advise_number`]);
    /// 8. a valid triple ⇒ the atom `ok`.
    ///
    /// # Errors
    ///
    /// `error:function_clause` for a non-`#file_descriptor{}` argument,
    /// `error:{badmatch, Data}` for a descriptor whose data map lacks `owner`/`handle`,
    /// `error:not_on_controlling_process` for a descriptor another process opened, and
    /// [`BeamError::BadArg`] for a missing operand (unreachable through the dispatch, which
    /// passes `arity` arguments).
    pub(crate) fn prim_advise(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let fd = *args.first().ok_or(BeamError::BadArg)?;
        let offset = *args.get(1).ok_or(BeamError::BadArg)?;
        let length = *args.get(2).ok_or(BeamError::BadArg)?;
        let advise = *args.get(3).ok_or(BeamError::BadArg)?;
        let Some(descriptor) = self.advise_descriptor(fd)? else {
            // The handle names no descriptor of this runtime: the C's `enif_get_resource`
            // failure, which the wrapper catches into the value.
            return self.error_tuple("badarg");
        };
        // The C's `file_handle_wrapper` state test, which runs **before** the operation and so
        // also precedes every argument check (`prim_file_nif.c:343-347`).
        if !self.runtime.open_files_mut().contains_key(&descriptor.id) {
            return self.error_tuple("einval");
        }
        match self.advise_outcome(offset, length, advise) {
            AdviseOutcome::Advised => self.ok_atom(),
            AdviseOutcome::EInval => self.error_tuple("einval"),
            // The NIF's `enif_make_badarg`, caught by the wrapper.
            AdviseOutcome::BadArg => self.error_tuple("badarg"),
        }
    }

    /// The descriptor argument decomposed the way `prim_file:advise/4` decomposes it — the
    /// ordered checks of [`Self::prim_advise`]'s doc — with the oracle's own raises.
    ///
    /// `Ok(Some(_))` is a descriptor this runtime can act on; `Ok(None)` is a well-shaped
    /// `#file_descriptor{}` whose handle is not one of ours — the C's `enif_get_resource`
    /// failure, which the caller answers as the value `{error,badarg}`.
    ///
    /// The order is observable and therefore load-bearing: the owner test lives in
    /// `get_fd_data/1`'s **body** while the `handle` match is the wrapper's own
    /// (`prim_file.erl:276`), so a descriptor that is both another process's and handle-less
    /// raises `not_on_controlling_process`, not `{badmatch, Data}`.
    fn advise_descriptor(&mut self, fd: Eterm) -> Result<Option<Descriptor>> {
        // 1. `get_fd_data(#file_descriptor{data = Data})`'s **clause head**: a 3-tuple whose
        //    first element is the record's tag atom. Anything else never runs a body, so the
        //    raise is the bare `error:function_clause` (with no reason term to carry).
        let data = match (
            terms::tuple_arity(fd),
            terms::tuple_element(fd, 0),
            terms::tuple_element(fd, 2),
        ) {
            (Ok(3), Ok(tag), Ok(data))
                if self.atom_name(tag).as_deref() == Some(FILE_DESCRIPTOR_TAG) =>
            {
                data
            }
            _ => return Err(function_clause()),
        };
        // 2. `#{owner := Owner} = Data` (`prim_file.erl:500`): a non-map, or a map without
        //    `owner`, is that match's failure.
        let entries = match terms::map_entries(data) {
            Ok(entries) => entries,
            Err(_) => return Err(self.badmatch(data)),
        };
        let mut owner = None;
        let mut has_handle = false;
        for (key, value) in entries {
            match self.atom_name(key).as_deref() {
                Some("owner") => owner = Some(value),
                Some("handle") => has_handle = true,
                _ => {}
            }
        }
        let Some(owner) = owner else {
            return Err(self.badmatch(data));
        };
        // 3. `case self() of Owner -> Data; _ -> error(not_on_controlling_process) end` — the
        //    comparison `super::RuntimeContext::check_owner` makes, taken from the map's own
        //    `owner` because that helper wants a decoded descriptor this ordered decomposition
        //    does not have yet.
        if owner != self.pid().to_eterm() {
            return Err(error::message("not_on_controlling_process"));
        }
        // 4. the wrapper's `#{handle := FRef} = get_fd_data(Fd)` (`prim_file.erl:276`).
        if !has_handle {
            return Err(self.badmatch(data));
        }
        // 5. the C's `enif_get_resource`: only a descriptor this runtime built carries its
        //    private registry key, so anything else is a handle that names no open file.
        Ok(self.decode_descriptor(fd))
    }

    /// The `error:{badmatch, Data}` raise `get_fd_data/1`'s `#{owner := Owner} = Data`
    /// (`prim_file.erl:500`) and the wrapper's `#{handle := FRef} = …` (`:276`) produce, with
    /// the data term attached to the live-reason slot — the same shape [`crate::native`]'s
    /// `erts_internal` `{badmap, Term}` uses.
    fn badmatch(&mut self, data: Eterm) -> BeamError {
        let reason = match self.badmatch_reason(data) {
            Ok(reason) => reason,
            // A heap that cannot hold the two-word reason is a typed error, never a bare
            // `badmatch` with the wrong (atom-only) reason.
            Err(error) => return error,
        };
        BeamError::Raise(
            Raise::error(ExitReason::Structured).with_live_reason(LiveReason::new(reason)),
        )
    }

    /// The `{badmatch, Data}` reason term, built on the context's own heap.
    fn badmatch_reason(&mut self, data: Eterm) -> Result<Eterm> {
        let tag = self.atom("badmatch")?;
        terms::make_tuple(self, &[tag, data])
    }

    /// `prim_file:advise_nif/4` — the **non-exported** NIF stub (`prim_file.erl:547-548`),
    /// served for the module's own local call.
    ///
    /// Here `args[0]` is the bare handle **reference** (`get_fd_data/1`'s
    /// `#{ handle := FRef }`), not a descriptor: the C's `WRAP_FILE_HANDLE_EXPORT` resolves
    /// `argv[0]` to the `efile` resource, and this runtime's substitution is that the
    /// open-file table is keyed by that reference's **id** (`prim_open`'s
    /// `let id = reference.id(); … open_files_mut().insert(id, file)`, `../prim_file.rs:223-226`,
    /// so `beam_codec::internal_ref_id` recovers the key). That invariant is pinned by
    /// [`tests::the_open_file_table_is_keyed_by_the_handle_reference_id`].
    ///
    /// The answers follow the C plus the fact that this arm is reached **from Erlang code**
    /// (the wrapper, on the `sendfile` path), never from the CLI:
    ///
    /// * a non-reference `args[0]` ⇒ the **raise** [`BeamError::BadArg`] — the C's
    ///   `enif_get_resource` failure (`prim_file_nif.c:322-324`), which the wrapper turns into
    ///   the value `{error,badarg}`;
    /// * a reference whose id is not in the open-file table (a **closed** handle) ⇒ the value
    ///   `{error,einval}`: the resource still exists on the oracle but is in
    ///   `EFILE_STATE_CLOSED`, and `file_handle_wrapper` answers `EINVAL` there *without*
    ///   consulting the operation (`:343-347`). The one unreachable divergence: a reference
    ///   that never named a file would fail `enif_get_resource` on the oracle (`badarg`), and
    ///   this runtime cannot tell it from a closed one — no module can call this stub except
    ///   `prim_file` itself, which only ever passes a handle it got from `open/2`;
    /// * otherwise the C's matrix, with a **non-number** `Offset`/`Length` raising
    ///   [`BeamError::BadArg`] (the NIF's `enif_make_badarg`) rather than answering the value.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a non-reference handle or a non-number `Offset`/`Length`, and
    /// for a missing operand (unreachable through the dispatch).
    pub(crate) fn prim_advise_nif(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let handle = *args.first().ok_or(BeamError::BadArg)?;
        let offset = *args.get(1).ok_or(BeamError::BadArg)?;
        let length = *args.get(2).ok_or(BeamError::BadArg)?;
        let advise = *args.get(3).ok_or(BeamError::BadArg)?;
        // The sanctioned safe reader: it validates the reference block before reading the id
        // word, so it is sound for any `Eterm`, and a non-reference is `BeamError::BadArg`.
        let id = beam_codec::internal_ref_id(handle)?;
        if !self.runtime.open_files_mut().contains_key(&id) {
            return self.error_tuple("einval");
        }
        match self.advise_outcome(offset, length, advise) {
            AdviseOutcome::Advised => self.ok_atom(),
            AdviseOutcome::EInval => self.error_tuple("einval"),
            AdviseOutcome::BadArg => Err(BeamError::BadArg),
        }
    }

    /// The C's `(Offset, Length, Advise)` classification (`advise_nif_impl`,
    /// `prim_file_nif.c:859-887`) for a **live** handle — the one matrix both keys share.
    ///
    /// The order is the C's and it is observable: `enif_is_number` runs on both operands
    /// *before* `enif_get_int64`, so a float `Offset` beside an atom `Length` is the `badarg`
    /// of the first test, not the `einval` of the second. The advise atom is tested **last**,
    /// and outside the six (`normal | random | sequential | will_need | dont_need | no_reuse`)
    /// it is `EINVAL` — never `badarg` (`:876-881`).
    fn advise_outcome(&self, offset: Eterm, length: Eterm, advise: Eterm) -> AdviseOutcome {
        let (offset, length) = match (advise_number(offset), advise_number(length)) {
            (NumArg::Value(offset), NumArg::Value(length)) => (offset, length),
            // `!enif_is_number(argv[0]) || !enif_is_number(argv[1])` — the C's *first* test.
            (NumArg::BadArg, _) | (_, NumArg::BadArg) => return AdviseOutcome::BadArg,
            // A float, or a bignum outside the `int64` range: a number `enif_get_int64` refuses.
            _ => return AdviseOutcome::EInval,
        };
        if offset < 0 || length < 0 {
            return AdviseOutcome::EInval;
        }
        match self.atom_name(advise).as_deref() {
            Some("normal" | "random" | "sequential" | "will_need" | "dont_need" | "no_reuse") => {
                AdviseOutcome::Advised
            }
            _ => AdviseOutcome::EInval,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for both keys of the `advise/4` family: the exported wrapper's `catch`ed
    //! value matrix, the local stub's raise/value split, and the two invariants the stub
    //! depends on (the open-file table's key is the handle reference's id; exactly one
    //! `prim_file` stub is served).
    //!
    //! Every case carries its oracle measurement; the checks that need a *live* oracle answer
    //! that cannot be reproduced here (the `function_clause` raise for a non-descriptor, the
    //! child process's `not_on_controlling_process`) state the measured oracle shape and pin
    //! this runtime's documented side of it.

    use std::path::Path;

    use beam_bifs::NullSink;
    use beam_engine::terms;

    use crate::context::test_support::scratch_dir;
    use crate::native::test_support::{atom, call, runtime};
    use crate::Runtime;

    use super::*;

    /// `prim_file:function(args)` through the runtime's dispatch, panicking on a raise.
    fn prim_file(runtime: &mut Runtime, function: &str, args: &[Eterm]) -> Eterm {
        call(runtime, "prim_file", function, args).expect("a prim_file answer")
    }

    /// Asserts `prim_file:function(args)` answers the bare atom `ok`.
    fn assert_ok(runtime: &mut Runtime, function: &str, args: &[Eterm]) {
        let answer = prim_file(runtime, function, args);
        assert_eq!(atom_name(runtime, answer), "ok", "{function}");
    }

    /// `prim_file:advise/4` — the exported wrapper's post-`catch` value.
    ///
    /// Driven straight into [`RuntimeContext::prim_advise`] rather than through the floor's
    /// `call_bif`: `src/native/prim.rs:216-218`'s `("advise", 4)` arm still answers the typed
    /// `unsupported` capability gap until the follow-up wiring commit replaces it with a call
    /// to this arm, and a test routed through `call_bif` today would exercise that gap instead
    /// of the matrix. The `open`/`close` halves of the fixtures do go through the floor — they
    /// are what builds the real open-file table the arm reads.
    fn advise(runtime: &mut Runtime, args: &[Eterm]) -> Eterm {
        call_advise(runtime, args).expect("an advise answer")
    }

    /// [`advise`] without the panicking unwrap: the wrapper's `catch error:badarg` turns a
    /// NIF `badarg` into a value, but `get_fd_data/1`'s `not_on_controlling_process` still
    /// raises, so the error shape is part of the contract.
    fn call_advise(runtime: &mut Runtime, args: &[Eterm]) -> Result<Eterm> {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        context.prim_advise(args)
    }

    /// Asserts `advise(args)` answers the bare atom `ok`.
    fn assert_advise_ok(runtime: &mut Runtime, args: &[Eterm]) {
        let answer = advise(runtime, args);
        assert_eq!(atom_name(runtime, answer), "ok", "{args:?}");
    }

    /// The reason atom of an `{error, Reason}` answer from `advise(args)`.
    fn advise_error(runtime: &mut Runtime, args: &[Eterm]) -> String {
        let answer = advise(runtime, args);
        error_reason(runtime, answer)
    }

    /// The atom `name` of a term, resolved through the shared table.
    fn atom_name(runtime: &Runtime, term: Eterm) -> String {
        runtime
            .atom_name_of(term)
            .unwrap_or_else(|| panic!("{term:?} is not an atom"))
            .to_string()
    }

    /// The reason atom of an `{error, Reason}` answer.
    fn error_reason(runtime: &Runtime, answer: Eterm) -> String {
        assert_eq!(terms::tuple_arity(answer).expect("a tuple"), 2);
        assert_eq!(atom_name(runtime, element(answer, 0)), "error");
        atom_name(runtime, element(answer, 1))
    }

    /// The `index`-th element of a tuple.
    fn element(answer: Eterm, index: usize) -> Eterm {
        terms::tuple_element(answer, index).expect("an element")
    }

    /// `elements` as an Erlang list built on the runtime's own heap seam.
    fn list(runtime: &mut Runtime, elements: &[Eterm]) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        terms::make_list(&mut context, elements).expect("a list")
    }

    /// Builds a term through a temporary context, so it stays valid for the call that reads it.
    fn heap_term(
        runtime: &mut Runtime,
        build: impl for<'a> FnOnce(&mut RuntimeContext<'a>) -> Result<Eterm>,
    ) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        build(&mut context).expect("a heap term")
    }

    /// A char-list path argument for `path`.
    fn path_arg(runtime: &mut Runtime, path: &Path) -> Eterm {
        crate::native::test_support::chars(
            runtime,
            path.to_str().expect("a UTF-8 scratch path"),
        )
    }

    /// Opens `name` in `dir` `[read]` and answers its `#file_descriptor{}`.
    fn open_read(runtime: &mut Runtime, dir: &Path, name: &str) -> Eterm {
        let path = path_arg(runtime, &dir.join(name));
        let read = atom(runtime, "read");
        let modes = list(runtime, &[read]);
        let answer = prim_file(runtime, "open", &[path, modes]);
        assert_eq!(terms::tuple_arity(answer).expect("a tuple"), 2);
        assert_eq!(atom_name(runtime, element(answer, 0)), "ok");
        element(answer, 1)
    }

    /// A scratch file with the eleven bytes the oracle probes above used (`"0123456789a"`).
    ///
    /// The directory removes itself on drop, so the caller keeps the value alive for as long
    /// as it opens the file from it.
    fn advise_file(label: &str) -> crate::context::test_support::FixtureDir {
        let dir = scratch_dir(label);
        std::fs::write(dir.path().join("x.txt"), b"0123456789a").expect("the fixture is writable");
        dir
    }

    /// The `handle` entry of a `#file_descriptor{}`'s `Data` map — the bare NIF reference the
    /// stub takes as `args[0]`.
    fn handle_of(runtime: &Runtime, fd: Eterm) -> Eterm {
        let data = super::super::descriptor_data(fd).expect("a well-shaped descriptor");
        for (key, value) in terms::map_entries(data).expect("a map") {
            if runtime.atom_name_of(key) == Some("handle") {
                return value;
            }
        }
        panic!("the descriptor carries no handle");
    }

    /// The six advice atoms, in the C's own order (`prim_file_nif.c:871-882`).
    fn advise_atoms(runtime: &mut Runtime) -> [Eterm; 6] {
        [
            atom(runtime, "normal"),
            atom(runtime, "random"),
            atom(runtime, "sequential"),
            atom(runtime, "will_need"),
            atom(runtime, "dont_need"),
            atom(runtime, "no_reuse"),
        ]
    }

    /// Oracle: `advise(Fd,0,10,normal)`⇒`ok`; `advise(Fd,0,0,normal)`⇒`ok`;
    /// `advise(Fd,9223372036854775807,9223372036854775807,normal)`⇒`ok`; all six advice atoms
    /// ⇒`ok`; a `[read,write]` fd⇒`ok`; a `[read,directory]` fd⇒`ok`.
    #[test]
    fn a_live_descriptor_answers_ok_for_every_documented_advice() {
        let mut runtime = runtime();
        let dir = advise_file("advise_ok");
        let fd = open_read(&mut runtime, dir.path(), "x.txt");

        let zero = beam_term::make_small(0);
        let ten = beam_term::make_small(10);
        let normal = atom(&mut runtime, "normal");
        assert_advise_ok(&mut runtime, &[fd, zero, ten, normal]);
        assert_advise_ok(&mut runtime, &[fd, zero, zero, normal]);
        let max = heap_term(&mut runtime, |context| {
            // `i64::MAX` is past the 60-bit small-int range, so it is a bignum — and
            // `enif_get_int64` still accepts it (the oracle answers `ok`).
            beam_codec::term::make_big(context.eval_heap(), false, &[i64::MAX as u64])
        });
        assert_advise_ok(&mut runtime, &[fd, max, max, normal]);
        for advice in advise_atoms(&mut runtime) {
            assert_advise_ok(&mut runtime, &[fd, zero, ten, advice]);
        }

        // A write-flagged descriptor and a directory descriptor agree on every platform this
        // runtime targets: the advice is a hint, so `efile_advise` never fails.
        let path = path_arg(&mut runtime, &dir.path().join("x.txt"));
        let read = atom(&mut runtime, "read");
        let write = atom(&mut runtime, "write");
        let rw = list(&mut runtime, &[read, write]);
        let answer = prim_file(&mut runtime, "open", &[path, rw]);
        assert_eq!(atom_name(&runtime, element(answer, 0)), "ok");
        let rw_fd = element(answer, 1);
        let will_need = atom(&mut runtime, "will_need");
        assert_advise_ok(&mut runtime, &[rw_fd, zero, ten, will_need]);

        let directory = atom(&mut runtime, "directory");
        let dir_modes = list(&mut runtime, &[read, directory]);
        let dir_path = path_arg(&mut runtime, dir.path());
        let answer = prim_file(&mut runtime, "open", &[dir_path, dir_modes]);
        assert_eq!(atom_name(&runtime, element(answer, 0)), "ok");
        let dir_fd = element(answer, 1);
        assert_advise_ok(&mut runtime, &[dir_fd, zero, ten, normal]);
    }

    /// Oracle: `advise(Fd,-1,10,normal)`/`(Fd,0,-1,normal)`⇒`{error,einval}`;
    /// `advise(Fd,1.0,10,normal)`/`(Fd,0,1.5,normal)`⇒`{error,einval}`;
    /// `advise(Fd,1 bsl 80,10,normal)`/`(Fd,0,1 bsl 80,normal)`⇒`{error,einval}`;
    /// `advise(Fd,foo,10,normal)`/`(Fd,0,bar,normal)`⇒`{error,badarg}`.
    #[test]
    fn offset_and_length_split_badarg_from_einval_like_the_c() {
        let mut runtime = runtime();
        let dir = advise_file("advise_numbers");
        let fd = open_read(&mut runtime, dir.path(), "x.txt");
        let zero = beam_term::make_small(0);
        let ten = beam_term::make_small(10);
        let normal = atom(&mut runtime, "normal");

        // A negative integer is a number `enif_get_int64` accepts, so it is EINVAL, not badarg.
        let negative = beam_term::make_small(-1);
        assert_eq!(
            advise_error(&mut runtime, &[fd, negative, ten, normal]),
            "einval"
        );
        assert_eq!(
            advise_error(&mut runtime, &[fd, zero, negative, normal]),
            "einval"
        );

        // A float is a number to `enif_is_number` and not an `int64` to `enif_get_int64`.
        let float = heap_term(&mut runtime, |context| {
            beam_codec::term::make_float(context.eval_heap(), 1.0)
        });
        assert_eq!(
            advise_error(&mut runtime, &[fd, float, ten, normal]),
            "einval"
        );
        assert_eq!(
            advise_error(&mut runtime, &[fd, zero, float, normal]),
            "einval"
        );

        // A bignum past the `int64` range is the same split: `1 bsl 80` is a number
        // `enif_is_number` accepts and `enif_get_int64` refuses (`{error,einval}` measured).
        let big = heap_term(&mut runtime, |context| {
            let heap = context.eval_heap();
            beam_codec::term::make_big(heap, false, &[0, 1 << 16])
        });
        assert_eq!(
            advise_error(&mut runtime, &[fd, big, ten, normal]),
            "einval"
        );
        assert_eq!(
            advise_error(&mut runtime, &[fd, zero, big, normal]),
            "einval"
        );

        // A non-number is the C's *first* test, i.e. the wrapper's caught `badarg`.
        let foo = atom(&mut runtime, "foo");
        let bar = atom(&mut runtime, "bar");
        assert_eq!(
            advise_error(&mut runtime, &[fd, foo, ten, normal]),
            "badarg"
        );
        assert_eq!(
            advise_error(&mut runtime, &[fd, zero, bar, normal]),
            "badarg"
        );
        let nil = list(&mut runtime, &[]);
        assert_eq!(
            advise_error(&mut runtime, &[fd, nil, ten, normal]),
            "badarg"
        );
        // The `is_number` test precedes `get_int64`, so a float beside an atom is the atom's
        // `badarg`, not the float's `einval`.
        assert_eq!(
            advise_error(&mut runtime, &[fd, float, bar, normal]),
            "badarg"
        );
    }

    /// Oracle: `advise(Fd,0,10,foo)`/`(Fd,0,10,{normal})`/`(Fd,0,10,1)`/`(Fd,0,10,[])`
    /// ⇒`{error,einval}` — never `badarg` ("The tests check for EINVAL instead of badarg",
    /// `prim_file_nif.c:877-878`).
    #[test]
    fn an_unknown_advice_atom_is_einval_not_badarg() {
        let mut runtime = runtime();
        let dir = advise_file("advise_atom");
        let fd = open_read(&mut runtime, dir.path(), "x.txt");
        let zero = beam_term::make_small(0);
        let ten = beam_term::make_small(10);

        let unknown = atom(&mut runtime, "foo");
        assert_eq!(
            advise_error(&mut runtime, &[fd, zero, ten, unknown]),
            "einval"
        );
        // A non-atom advice term goes through the same fall-through.
        let one = beam_term::make_small(1);
        assert_eq!(
            advise_error(&mut runtime, &[fd, zero, ten, one]),
            "einval"
        );
        let nil = list(&mut runtime, &[]);
        assert_eq!(
            advise_error(&mut runtime, &[fd, zero, ten, nil]),
            "einval"
        );
        let tuple = heap_term(&mut runtime, |context| {
            terms::make_tuple(context, &[one])
        });
        assert_eq!(
            advise_error(&mut runtime, &[fd, zero, ten, tuple]),
            "einval"
        );
    }

    /// Oracle: `advise(42,…)` / `advise(foo,…)` / `advise([],…)` / `advise(#{},…)` /
    /// `advise({a,b},…)` / a 4-tuple / `advise({nope,prim_file,#{}},…)` all **raise**
    /// `error:function_clause` — the clause head of `get_fd_data/1` fails before any body runs,
    /// so the wrapper's `catch error:badarg` never sees it. Measured on OTP 29.1.2.
    #[test]
    fn a_term_that_is_not_a_descriptor_raises_function_clause() {
        let mut runtime = runtime();
        let zero = beam_term::make_small(0);
        let ten = beam_term::make_small(10);
        let normal = atom(&mut runtime, "normal");

        let empty_map = heap_term(&mut runtime, |context| {
            let atoms = context.runtime.code_server.atoms_handle();
            terms::make_map(context, &[], &atoms)
        });
        let tag = atom(&mut runtime, "file_descriptor");
        let module = atom(&mut runtime, "prim_file");
        let wrong_tag = atom(&mut runtime, "nope");
        let pair = heap_term(&mut runtime, |context| {
            terms::make_tuple(context, &[tag, module])
        });
        let five = heap_term(&mut runtime, |context| {
            terms::make_tuple(context, &[tag, module, empty_map, empty_map, empty_map])
        });
        let wrong_tagged = heap_term(&mut runtime, |context| {
            terms::make_tuple(context, &[wrong_tag, module, empty_map])
        });

        for term in [
            beam_term::make_small(42),
            atom(&mut runtime, "foo"),
            list(&mut runtime, &[]),
            empty_map,
            pair,
            five,
            wrong_tagged,
        ] {
            let raised = call_advise(&mut runtime, &[term, zero, ten, normal])
                .expect_err("not a descriptor is the clause head's failure");
            assert_function_clause(&raised);
        }
    }

    /// Oracle: `advise({file_descriptor,prim_file,nope},…)`, `advise({file_descriptor,
    /// prim_file,#{}},…)`, `advise({file_descriptor,prim_file,#{owner=>self()}},…)` and a
    /// **live** descriptor's data map with `owner` (or `handle`) removed each raise
    /// `error:{badmatch, Data}` — `get_fd_data/1`'s `#{owner := Owner} = Data`
    /// (`prim_file.erl:500`), then the wrapper's own `#{handle := FRef} = …` (`:276`) — with
    /// the data term itself as the reason's second element. Measured on OTP 29.1.2.
    #[test]
    fn a_descriptor_whose_data_map_lacks_owner_or_handle_raises_badmatch() {
        let mut runtime = runtime();
        let zero = beam_term::make_small(0);
        let ten = beam_term::make_small(10);
        let normal = atom(&mut runtime, "normal");
        let dir = advise_file("advise_badmatch");
        let fd = open_read(&mut runtime, dir.path(), "x.txt");
        let tag = element(fd, 0);
        let module = element(fd, 1);
        let nope = atom(&mut runtime, "nope");
        let empty_map = heap_term(&mut runtime, |context| {
            let atoms = context.runtime.code_server.atoms_handle();
            terms::make_map(context, &[], &atoms)
        });

        // `data` is not a map at all: the `#{owner := Owner} = Data` match.
        let not_a_map = heap_term(&mut runtime, |context| {
            terms::make_tuple(context, &[tag, module, nope])
        });
        let raised = call_advise(&mut runtime, &[not_a_map, zero, ten, normal])
            .expect_err("a non-map data is the owner match's failure");
        assert_eq!(assert_badmatch(&runtime, &raised), nope);

        // A map with no `owner` entry.
        let empty_descriptor = heap_term(&mut runtime, |context| {
            terms::make_tuple(context, &[tag, module, empty_map])
        });
        let raised = call_advise(&mut runtime, &[empty_descriptor, zero, ten, normal])
            .expect_err("a data map without owner is the owner match's failure");
        assert_eq!(assert_badmatch(&runtime, &raised), empty_map);

        // The owner match passes (`owner => self()`), the wrapper's handle match does not.
        let owner = owner_of(&runtime, fd);
        let owner_only = descriptor_data_map(&mut runtime, &[(owner, "owner")]);
        let raised = call_advise(&mut runtime, &[owner_only, zero, ten, normal])
            .expect_err("a data map without handle is the wrapper's match failure");
        let payload = assert_badmatch(&runtime, &raised);
        assert!(has_entry(&runtime, payload, "owner"));
        assert!(!has_entry(&runtime, payload, "handle"));

        // A **live** descriptor's own data map with one required entry removed: the raise reads
        // the map the caller passed, the runtime's registry key included.
        let data = super::super::descriptor_data(fd).expect("a well-shaped descriptor");
        let data_len = terms::map_entries(data).expect("a map").len();
        for dropped in ["owner", "handle"] {
            let entries: Vec<(Eterm, Eterm)> = terms::map_entries(data)
                .expect("a map")
                .into_iter()
                .filter(|(key, _)| runtime.atom_name_of(*key) != Some(dropped))
                .collect();
            let descriptor = heap_term(&mut runtime, |context| {
                let atoms = context.runtime.code_server.atoms_handle();
                let map = terms::make_map(context, &entries, &atoms)?;
                terms::make_tuple(context, &[tag, module, map])
            });
            let raised = call_advise(&mut runtime, &[descriptor, zero, ten, normal])
                .expect_err("a live data map minus a required entry raises");
            let payload = assert_badmatch(&runtime, &raised);
            assert!(!has_entry(&runtime, payload, dropped), "{dropped}");
            assert_eq!(
                terms::map_entries(payload).expect("a map").len(),
                data_len - 1
            );
        }
    }

    /// Oracle: a well-shaped `#file_descriptor{}` whose `handle` is not one of this runtime's —
    /// the forged `{file_descriptor,prim_file,#{owner => self(), handle => make_ref()}}` — is
    /// the **value** `{error,badarg}`: `enif_get_resource` fails on that handle and the
    /// wrapper's `catch error:badarg` (`prim_file.erl:274-280`) takes it. Measured on
    /// OTP 29.1.2 (`make_ref()` stands in for the forged handle; any non-resource is the same
    /// `enif_get_resource` failure).
    #[test]
    fn a_handle_that_names_no_open_file_is_the_badarg_value() {
        let mut runtime = runtime();
        let zero = beam_term::make_small(0);
        let ten = beam_term::make_small(10);
        let normal = atom(&mut runtime, "normal");
        let dir = advise_file("advise_forged");
        let fd = open_read(&mut runtime, dir.path(), "x.txt");
        let owner = owner_of(&runtime, fd);
        let forged_handle = beam_term::make_small(7);

        let forged = descriptor_data_map(
            &mut runtime,
            &[(owner, "owner"), (forged_handle, "handle")],
        );
        assert_eq!(
            advise_error(&mut runtime, &[forged, zero, ten, normal]),
            "badarg"
        );
    }

    /// The bare `error:function_clause` raise `get_fd_data/1`'s clause head produces: the reason
    /// atom, and **no** live reason term (nothing ran, so no term was built).
    fn assert_function_clause(raised: &BeamError) {
        match raised {
            BeamError::Raise(raise) => assert_eq!(
                raise.reason.as_atom(),
                Some("function_clause"),
                "the oracle's clause-head reason"
            ),
            other => panic!("expected the function_clause raise, got {other}"),
        }
        assert!(
            error::live_reason_term(raised).is_none(),
            "a clause head builds no reason term"
        );
    }

    /// The `Data` payload of an `error:{badmatch, Data}` raise, asserted to be that shape (a
    /// structured reason whose term is the two-element `{badmatch, Data}`).
    fn assert_badmatch(runtime: &Runtime, raised: &BeamError) -> Eterm {
        match raised {
            BeamError::Raise(raise) => assert!(
                raise.reason.as_atom().is_none(),
                "a structured reason carries no reason atom: {raised}"
            ),
            other => panic!("expected a structured raise, got {other}"),
        }
        let reason = error::live_reason_term(raised).expect("the {badmatch, Data} reason term");
        assert_eq!(terms::tuple_arity(reason).expect("a 2-tuple"), 2);
        let tag = element(reason, 0);
        assert_eq!(runtime.atom_name_of(tag), Some("badmatch"));
        element(reason, 1)
    }

    /// Whether the map `map` carries an entry named `name`.
    fn has_entry(runtime: &Runtime, map: Eterm, name: &str) -> bool {
        terms::map_entries(map)
            .expect("a map")
            .iter()
            .any(|(key, _)| runtime.atom_name_of(*key) == Some(name))
    }

    /// The `owner` entry of a live descriptor's data map — the calling process's own pid.
    fn owner_of(runtime: &Runtime, fd: Eterm) -> Eterm {
        let data = super::super::descriptor_data(fd).expect("a well-shaped descriptor");
        for (key, value) in terms::map_entries(data).expect("a map") {
            if runtime.atom_name_of(key) == Some("owner") {
                return value;
            }
        }
        panic!("the descriptor carries no owner");
    }

    /// A `{file_descriptor, prim_file, Map}` whose data map holds exactly `entries`
    /// (`(value, key name)` pairs, in the given order).
    fn descriptor_data_map(runtime: &mut Runtime, entries: &[(Eterm, &str)]) -> Eterm {
        let mut pairs = Vec::with_capacity(entries.len());
        for (value, name) in entries {
            let key = atom(runtime, name);
            pairs.push((key, *value));
        }
        let tag = atom(runtime, "file_descriptor");
        let module = atom(runtime, "prim_file");
        heap_term(runtime, |context| {
            let atoms = context.runtime.code_server.atoms_handle();
            let map = terms::make_map(context, &pairs, &atoms)?;
            terms::make_tuple(context, &[tag, module, map])
        })
    }

    /// Oracle: after `close(Fd)`, `advise(Fd,0,10,normal)`⇒`{error,einval}` — and so does
    /// `advise(Fd,foo,10,normal)`, i.e. the resource-state check precedes the argument checks
    /// (`file_handle_wrapper`, `prim_file_nif.c:343-347`).
    #[test]
    fn a_closed_descriptor_is_einval_before_any_argument_is_classified() {
        let mut runtime = runtime();
        let dir = advise_file("advise_closed");
        let fd = open_read(&mut runtime, dir.path(), "x.txt");
        assert_ok(&mut runtime, "close", &[fd]);

        let zero = beam_term::make_small(0);
        let ten = beam_term::make_small(10);
        let normal = atom(&mut runtime, "normal");
        assert_eq!(
            advise_error(&mut runtime, &[fd, zero, ten, normal]),
            "einval"
        );
        // The state check wins over every argument classification.
        let foo = atom(&mut runtime, "foo");
        assert_eq!(
            advise_error(&mut runtime, &[fd, foo, ten, normal]),
            "einval"
        );
        assert_eq!(
            advise_error(&mut runtime, &[fd, zero, ten, foo]),
            "einval"
        );
    }

    /// Oracle: an fd opened by **another** process answers that process's
    /// `{not_on_controlling_process,[{prim_file,get_fd_data,1,[]},{prim_file,advise,4,[]},…]}`
    /// — `get_fd_data/1`'s own `error/1` (`prim_file.erl:499-504`), which the wrapper's
    /// `catch error:badarg` does not catch, so it is a raise here too and it precedes every
    /// other check (the oracle's child never reaches the NIF).
    #[test]
    fn a_descriptor_owned_by_another_process_raises() {
        let mut runtime = runtime();
        let dir = advise_file("advise_owner");
        let fd = open_read(&mut runtime, dir.path(), "x.txt");

        // Rebuild the same descriptor with a **foreign** owner pid, which is exactly what a
        // descriptor handed to another process carries: the registry slot is the same live
        // file, so only the owner test can answer.
        let foreign = beam_process::ProcessId::new(0x1234, 0).to_eterm();
        let data = super::super::descriptor_data(fd).expect("a well-shaped descriptor");
        let mut entries: Vec<(Eterm, Eterm)> = Vec::new();
        for (key, value) in terms::map_entries(data).expect("a map") {
            let value = if runtime.atom_name_of(key) == Some("owner") {
                foreign
            } else {
                value
            };
            entries.push((key, value));
        }
        let tag = element(fd, 0);
        let module = element(fd, 1);
        let borrowed = heap_term(&mut runtime, |context| {
            let atoms = context.runtime.code_server.atoms_handle();
            let map = terms::make_map(context, &entries, &atoms)?;
            terms::make_tuple(context, &[tag, module, map])
        });

        let zero = beam_term::make_small(0);
        let ten = beam_term::make_small(10);
        let normal = atom(&mut runtime, "normal");
        let error = call_advise(&mut runtime, &[borrowed, zero, ten, normal])
            .expect_err("another process's descriptor raises");
        match error {
            BeamError::Raise(raise) => assert_eq!(
                raise.reason.as_atom(),
                Some("not_on_controlling_process"),
                "the oracle's get_fd_data/1 reason"
            ),
            other => panic!("expected the owner raise, got {other}"),
        }
    }

    /// The stub depends on one invariant: `prim_open` keys the open-file table by the
    /// **handle reference's id** (`../prim_file.rs:223-226`), so
    /// `beam_codec::internal_ref_id(handle)` is the table key. Pinned so a future change to
    /// either half cannot silently desynchronize the two.
    #[test]
    fn the_open_file_table_is_keyed_by_the_handle_reference_id() {
        let mut runtime = runtime();
        let dir = advise_file("advise_key");
        let fd = open_read(&mut runtime, dir.path(), "x.txt");
        let handle = handle_of(&runtime, fd);

        let id = beam_codec::internal_ref_id(handle).expect("the handle is a reference");
        assert!(
            runtime.open_files_mut().contains_key(&id),
            "the open-file table's key is the handle reference's id"
        );
        // And the descriptor's own `fd_registry_id` entry names the same slot.
        let data = super::super::descriptor_data(fd).expect("a well-shaped descriptor");
        let registry = terms::map_entries(data)
            .expect("a map")
            .into_iter()
            .find(|(key, _)| runtime.atom_name_of(*key) == Some(super::super::REGISTRY_ID_KEY))
            .map(|(_, value)| beam_term::signed_value(value))
            .expect("the descriptor carries the registry key");
        assert_eq!(u64::try_from(registry).ok(), Some(id));

        // Closing removes exactly that slot, which is what makes the stub's absence test the
        // C's CLOSED state.
        assert_ok(&mut runtime, "close", &[fd]);
        assert!(!runtime.open_files_mut().contains_key(&id));
    }

    /// The local stub's matrix: a live handle answers `ok`, the argument split matches the
    /// C's, a closed handle is the `{error,einval}` value (see [`RuntimeContext::prim_advise_nif`])
    /// and a non-reference raises — the C's `enif_get_resource` failure, which the wrapper
    /// catches into `{error,badarg}`.
    #[test]
    fn the_local_stub_serves_a_live_handle_and_raises_on_a_non_reference() {
        let mut runtime = runtime();
        let dir = advise_file("advise_nif");
        let fd = open_read(&mut runtime, dir.path(), "x.txt");
        let handle = handle_of(&runtime, fd);
        let zero = beam_term::make_small(0);
        let ten = beam_term::make_small(10);
        let normal = atom(&mut runtime, "normal");

        // A live handle: `ok`, and the argument matrix is the C's.
        let answer = stub(&mut runtime, &[handle, zero, ten, normal]);
        assert_eq!(atom_name(&runtime, answer), "ok");
        for advice in advise_atoms(&mut runtime) {
            let answer = stub(&mut runtime, &[handle, zero, ten, advice]);
            assert_eq!(atom_name(&runtime, answer), "ok");
        }
        let negative = beam_term::make_small(-1);
        let answer = stub(&mut runtime, &[handle, negative, ten, normal]);
        assert_eq!(error_reason(&runtime, answer), "einval");
        let unknown = atom(&mut runtime, "foo");
        let answer = stub(&mut runtime, &[handle, zero, ten, unknown]);
        assert_eq!(error_reason(&runtime, answer), "einval");

        // A non-number operand is the NIF's `enif_make_badarg`: a **raise** on this arm (the
        // wrapper above it is what turns it into the value `{error,badarg}`).
        let error = call_stub(&mut runtime, &[handle, unknown, ten, normal])
            .expect_err("a non-number Offset is the NIF's badarg raise");
        assert!(
            matches!(error, BeamError::BadArg),
            "expected badarg, got {error}"
        );

        // A handle whose slot is gone (a closed descriptor) is the value `{error,einval}`:
        // the resource still exists on the oracle but is in `EFILE_STATE_CLOSED`.
        assert_ok(&mut runtime, "close", &[fd]);
        let answer = stub(&mut runtime, &[handle, zero, ten, normal]);
        assert_eq!(error_reason(&runtime, answer), "einval");

        // A term that is not an M1 reference at all is `enif_get_resource`'s failure.
        let not_a_reference = beam_term::make_small(42);
        let error = call_stub(&mut runtime, &[not_a_reference, zero, ten, normal])
            .expect_err("a non-reference handle raises badarg");
        assert!(matches!(error, BeamError::BadArg), "got {error}");
        let foreign_pid = beam_process::ProcessId::new(0x1234, 0).to_eterm();
        let error = call_stub(&mut runtime, &[foreign_pid, zero, ten, normal])
            .expect_err("a pid is not a reference either");
        assert!(matches!(error, BeamError::BadArg), "got {error}");
    }

    /// Exactly one `prim_file` stub is served, reachable as
    /// `crate::native::prim_file::serves_local_call` (the re-export the follow-up wiring arm
    /// consults) — and the predicate is a pure `(module, function, arity)` membership test, so
    /// no other module name and no other `prim_file` stub can slip through.
    #[test]
    fn serves_local_call_is_exactly_the_advise_nif_key() {
        let serves = crate::native::prim_file::serves_local_call;
        assert!(serves("prim_file", "advise_nif", 4));
        assert_eq!(LOCAL_KEYS, [("advise_nif", 4)]);
        // The module-name equality is the engine's fast path, not a narrower filter: any other
        // module answers `false`.
        assert!(!serves("other", "advise_nif", 4));
        assert!(!serves("prim_tty", "advise_nif", 4));
        // Arity and name are part of the key.
        assert!(!serves("prim_file", "advise_nif", 3));
        assert!(!serves("prim_file", "advise_nif", 5));
        // The other `prim_file` stubs stay unserved: their wrappers are the floor's
        // exported-key arms.
        for (function, arity) in [
            ("read_nif", 2),
            ("pwrite_nif", 3),
            ("allocate_nif", 3),
            ("advise", 4),
            ("read_handle_info_nif", 1),
        ] {
            assert!(
                !serves("prim_file", function, arity),
                "prim_file:{function}/{arity} must stay unserved"
            );
        }
    }

    /// Drives [`RuntimeContext::prim_advise_nif`] directly (the stub is not an exported key,
    /// so the floor's `call_bif` never reaches it — `resolve_local_call` will, once the
    /// follow-up wiring lands).
    fn stub(runtime: &mut Runtime, args: &[Eterm]) -> Eterm {
        call_stub(runtime, args).expect("a stub answer")
    }

    /// [`stub`] without the panicking unwrap.
    fn call_stub(runtime: &mut Runtime, args: &[Eterm]) -> Result<Eterm> {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        context.prim_advise_nif(args)
    }
}
