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

//! The **in-process ABI assertions** for the three A.3 (NIF C ABI) paths whose oracle behaviour the
//! fixtures `tests/fixtures/a3_{schedule,thread_env,resource_stop}.{c,erl}` measure.
//!
//! `beam_abi`'s `enif_*` entry points are `#[no_mangle] pub unsafe extern "C" fn`s — plain Rust
//! functions — so the contract the fixtures observe through a loaded `.so` under the live system
//! `erl` can be asserted here directly: **no** `dlopen`, no shared object, no oracle, and no external
//! toolchain. The ground-truth artifacts (the C fixtures, their Erlang drivers and the recorded
//! transcripts) stay in the sibling `nif_a3_oracle_replay.rs`; the specification behind both files is
//! `../HANDOFF_A3_NIF_HOST.md` §[1]/§[2]/§[3], a real, readable file in this crate's root.
//!
//! # What each test asserts, and the honest boundary
//!
//! * [`schedule::enif_schedule_nif_runs_its_continuation_to_completion`] — §1: the continuation
//!   trampoline. The continuation runs (exactly once, with the caller's own uncopied `argv`), its value
//!   is the call's result, a nested schedule answers the C's `THE_NON_VALUE` while the outermost call
//!   still yields the chain's final value, the host gets one yield point per trampoline iteration, a
//!   dirty flag shows through `enif_thread_type`, and the C's refusals still run nothing.
//!   **Boundary:** the C's raw `enif_schedule_nif` return word is `THE_NON_VALUE` on *every* path,
//!   because the emulator re-runs the trap and `execute_nif` supplies the value later. This crate
//!   folds that re-run into the outermost call — the one substitution `beam_abi`'s `schedule` module
//!   documents — so the outermost call answers the chain's value here (and, for a raise, the pending
//!   reason term; see the raise section). The Erlang-visible result is identical; only a probe that
//!   exposes the raw word can tell. Nothing about attribution (the stacktrace head) is asserted: that
//!   is the engine's to build and this layer deliberately fabricates no frame.
//! * [`thread_env::enif_alloc_env_off_the_call_path_needs_a_factory_and_then_works`] — §2:
//!   `enif_alloc_env` off the NIF-call path. Both halves are the contract, so both are asserted: with
//!   **no** factory a thread outside a call still gets `NULL` (an honest refusal — a crate-owned env
//!   would intern atoms into a table the runtime does not use and hand a NIF a *wrong atom index*),
//!   and with a factory installed the env is real, atoms round-trip through it,
//!   `enif_send(NULL, &pid, msg_env, msg)` delivers with the C's `undefined` sender, and
//!   `enif_free_env` hands the env back to the host that allocated it.
//!   **Boundary:** ABI level only. There is no live `AtomTable`, no real `beam_process` mailbox and no
//!   wake-up assertion here — a host's `NifHost::send` is where that lives (`beam_rt`'s runtime path),
//!   and this file asserts the *seam* the host is called through.
//! * [`select_stop::enif_select_stop_verdicts_run_the_right_callback`] — §3: the `enif_select` STOP
//!   verdicts. The oracle's verdict matrix (`STOP_CALLED` / `STOP_CALLED|READ_CANCELLED` synchronous
//!   and direct; `STOP_SCHEDULED|READ_CANCELLED` deferred to the drain with `is_direct_call = false`;
//!   every other code calling nothing) is driven through a host that answers each verdict in turn,
//!   with the reference accounting checked per case (the parked stop's `enif_keep_resource` is taken
//!   and the drain gives it back).
//!   **Boundary:** the host owns the poller, so the *code* is the host's; what is asserted is this
//!   crate's verdict dispatch and its reference accounting. A resource type opened **without** a
//!   `stop` callback is a deliberate typed no-op here (the oracle instead calls a NULL function
//!   pointer and dies: SIGSEGV, 3/3 measured runs — `probe_stop_nostop_type/0` is never run), and
//!   `enif_select` is never issued against such a type in this file.
//!
//! # File layout (one test binary, three modules)
//!
//! Cargo runs `tests/nif_a3_abi_assertions.rs` as **one** test binary, so the three process-wide
//! states it touches (the thread-env factory, the pending-stop table) stay inside one process and each
//! serialises itself over its own lock. The bodies live in three sibling modules under
//! `tests/nif_a3_abi_assertions/` — one per A.3 path, in the order above — so that no file outgrows
//! this workspace's ~1000-line rule. The module path is spelled out with `#[path]` because this crate
//! already does that for its own side-by-side test files.
//!
//! # Test isolation
//!
//! Two pieces of state are process-wide while the harness runs tests in parallel, so each has one
//! lock and one RAII teardown:
//!
//! * the **thread-env factory** (`thread_env::FactoryGuard`, which holds its lock and clears the
//!   factory on drop *and* on panic), and
//! * the **pending-stop table** (`select_stop::STOP_LOCK`); the assertions then read this test's own
//!   objects' counters, never `pending_stop_count()` globally.
//!
//! `enif_schedule_nif`'s trampoline state is thread-local, so the schedule test needs no lock.

#[path = "nif_a3_abi_assertions/schedule.rs"]
mod schedule;

#[path = "nif_a3_abi_assertions/thread_env.rs"]
mod thread_env;

#[path = "nif_a3_abi_assertions/select_stop.rs"]
mod select_stop;
