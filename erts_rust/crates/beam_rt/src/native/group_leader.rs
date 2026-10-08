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

//! The `erlang.group_leader` floor: `erlang:group_leader/2` and its two
//! `erts_internal:group_leader/2,3` counterparts.
//!
//! A process's **group leader** is the process `io:format/2` and friends send their output
//! to. It is *per-process runtime state*: the C keeps it in the process struct
//! (`Process::group_leader`, `erl_process.h`) and every reader (`group_leader_0`,
//! `process_info(Pid, group_leader)`) reads that field. `beam_process`'s
//! [`Process`](beam_process::Process) has no such field, so this crate keeps the one map
//! ([`Runtime::group_leaders`](crate::Runtime)) and *every* reader goes through
//! [`Runtime::group_leader_of`](crate::Runtime) — the runtime-native floor, the `ProcessHook`
//! `group_leader/0` bridge and the `process_info` snapshot — so they can never disagree.
//!
//! `application_master:init/3` — reached by the boot script's instruction 18
//! (`application:start_boot(kernel, permanent)`) — is the reason this exists: it sets its
//! application master's group leader and dies with `error:undef` without it.
//!
//! # Ground truth (probed against the system oracle, OTP 29.1.1 / erts-17.1)
//!
//! ``erlang:group_leader/2` is **not** a BIF: it is an `erlang.erl` wrapper
//! (`erts/preloaded/src/erlang.erl:3328-3341`) over `erts_internal:group_leader/2`, falling
//! back to the `/3` (signal) form and a `receive` when `/2` answers `false`:
//!
//! ```text
//! erlang:group_leader(GL, Pid) ->
//!     case case erts_internal:group_leader(GL, Pid) of
//!              false -> Ref = erlang:make_ref(), erts_internal:group_leader(GL, Pid, Ref),
//!                       receive {Ref, MsgRes} -> MsgRes end;
//!              Res -> Res
//!          end of
//!         true -> true;
//!         Error -> error_with_info(Error, [GL, Pid])
//!     end.
//! ```
//!
//! | Call | Oracle | C |
//! |---|---|---|
//! | `erlang:group_leader(GL, Pid)` | `true`; `badarg` (raised) for a non-pid `GL`, a non-pid `Pid`, or a **dead/unknown** `Pid`; a **dead `GL` is accepted** | `bif.c:4965-5021` + `erlang.erl:3328-3341` |
//! | `erts_internal:group_leader(GL, Pid)` | `true` on success; the **atom** `badarg` (returned, never raised) for a bad `GL` or a non-pid/dead `Pid` | `bif.c:4965-5021` |
//! | `erts_internal:group_leader(GL, Pid, Ref)` | the atom `ok`; `badarg` (raised) for a non-pid `GL`, a non-pid `Pid`, or a non-reference `Ref` — the `Pid` need not be live | `bif.c:4949-4963` |
//!
//! `erlang:group_leader/3` does not exist (the oracle raises `undef`), so no arm claims it.
//! A child inherits its parent's leader (`erl_process.c:12654-12663`; a spawn with an invalid
//! pid uses `self`), which [`Runtime::inherit_group_leader`](crate::Runtime) implements at
//! the three spawn sites.
//!
//! # Documented Milestone-1 divergences (honoured, never hidden)
//!
//! * **The `/3` signal is applied immediately.** The C sends an *asynchronous* signal
//!   (`erts_proc_sig_send_group_leader`), so its effect is only observed at the target's next
//!   signal check; this single-threaded runtime holds no signal queue, so it records the new
//!   leader right away and answers `ok`. That is never *older* than the oracle's eventual
//!   state — a probe of the oracle shows the leader really does change once the target
//!   receives (`{gl2, <0.83.0>, true}`).
//! * **Only internal pids are modelled.** `bif.tab:78-80` declares `erlang:group_leader/0` and
//!   `erts_internal:group_leader/2,3`, and the C accepts a *wire* external pid as the leader
//!   (it stores it) or as the target (it returns `true` after a remote signal). This runtime's
//!   process state is a local pid map, so an external pid is the typed `badarg` — an honest
//!   rejection, never a fabricated `true` or a leader it cannot report.
//! * **The reference test is the runtime's own registry.** The C checks the *shape*
//!   (`is_not_internal_ref`) because it can read the thing; reading a reference's payload needs
//!   `beam_term::boxed_val` (which is `unsafe`, and this crate is `#![deny(unsafe_code)]`), so
//!   the recognisable references are the ones this runtime minted — [`Runtime::references`]
//!   (`make_ref/0`, `monitor/2`) and the timer heap. That is the same predicate
//!   `erlang:cancel_timer/1` uses.
//!
//! [`Runtime::references`]: crate::Runtime::references

use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::Eterm;

use crate::context::RuntimeContext;

impl RuntimeContext<'_> {
    /// `erlang:group_leader(GL, Pid)` — the `erlang.erl` wrapper (`erlang.erl:3328-3341`)
    /// over `erts_internal:group_leader/2`.
    ///
    /// The wrapper's observable result for every **local** case is exactly
    /// `erts_internal:group_leader/2`'s, with its `badarg` answer turning into the
    /// `error:badarg` raise (`error_with_info`), so this BIF reproduces that directly: the `/2`
    /// form can never answer `false` here (nothing ever locks the target process in a
    /// single-threaded runtime), so the wrapper's `/3` + `receive` fallback is unreachable.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a non-pid `GL`, a non-pid `Pid`, or a `Pid` that is not live
    /// — the oracle's `badarg` in each of those cases. A dead **`GL`** is accepted.
    pub(super) fn group_leader_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let leader = *args.first().ok_or(BeamError::BadArg)?;
        let target = *args.get(1).ok_or(BeamError::BadArg)?;
        // The C's `is_not_pid` (`bif.c:4967`): a group leader must be a pid but need not be
        // alive — `erlang:group_leader(<dead pid>, self())` ⇒ `true` in the oracle.
        let leader = ProcessId::from_eterm(leader).map_err(|_| BeamError::BadArg)?;
        // The C's `erts_try_lock_sig_free_proc` (`bif.c:4977`): a target that names no live
        // process is `badarg` (never a silent success on a process that does not exist).
        let target = ProcessId::from_eterm(target).map_err(|_| BeamError::BadArg)?;
        if !self.process_table_mut().contains(target) {
            return Err(BeamError::BadArg);
        }
        self.runtime.set_group_leader(target, leader);
        self.atom("true")
    }

    /// `erts_internal:group_leader/2` — `erts_internal_group_leader_2`
    /// (`bif.c:4965-5021`).
    ///
    /// The classification cases answer the **atom** `badarg` (a *returned* value the
    /// `erlang.erl` wrapper inspects), not a raise.
    ///
    /// # Errors
    ///
    /// Nothing on this path is raised: the C returns `am_badarg` for a non-pid group leader,
    /// a non-pid target and a target that names no live process. Only the term builders can
    /// fail.
    pub(super) fn erts_group_leader_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let leader = *args.first().ok_or(BeamError::BadArg)?;
        let target = *args.get(1).ok_or(BeamError::BadArg)?;
        // The C checks the group leader first (`bif.c:4967-4968`) and *returns* `badarg`.
        let Ok(leader) = ProcessId::from_eterm(leader) else {
            return self.atom("badarg");
        };
        // `is_internal_pid(BIF_ARG_2)` (`bif.c:4970`): a non-pid target answers `badarg`.
        let Ok(target) = ProcessId::from_eterm(target) else {
            return self.atom("badarg");
        };
        // A target with no live process — the failed `erts_try_lock_sig_free_proc`
        // (`bif.c:4977-4984`), the `EXITING` state (`erts_set_group_leader` returns 0,
        // `bif.c:4921-4922`) and a target this table simply does not hold all answer `badarg`.
        if !self.process_table_mut().contains(target) {
            return self.atom("badarg");
        }
        self.runtime.set_group_leader(target, leader);
        self.atom("true")
    }

    /// `erts_internal:group_leader/3` — `erts_internal_group_leader_3`
    /// (`bif.c:4949-4963`).
    ///
    /// Validation only (all three argument shape checks **raise** `badarg`), then the
    /// group-leader signal — which this single-threaded runtime applies immediately (see the
    /// module docs) — and the atom `ok`. The target pid need **not** be live: the C only sends
    /// a signal, and the oracle answers `ok` even for a dead one.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a non-pid group leader, a non-pid target, or an argument that
    /// is not a reference this runtime minted.
    pub(super) fn erts_group_leader_3(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let leader = *args.first().ok_or(BeamError::BadArg)?;
        let target = *args.get(1).ok_or(BeamError::BadArg)?;
        let reference = *args.get(2).ok_or(BeamError::BadArg)?;
        // `if (is_not_pid(BIF_ARG_1)) BIF_ERROR(BADARG)` (`bif.c:4951-4952`).
        let leader = ProcessId::from_eterm(leader).map_err(|_| BeamError::BadArg)?;
        // `if (is_not_internal_pid(BIF_ARG_2)) BIF_ERROR(BADARG)` (`bif.c:4953-4954`).
        let target = ProcessId::from_eterm(target).map_err(|_| BeamError::BadArg)?;
        // `if (is_not_internal_ref(BIF_ARG_3)) BIF_ERROR(BADARG)` (`bif.c:4955-4956`),
        // approximated by the runtime's own reference registry (see the module docs).
        if !self.is_runtime_reference(reference) {
            return Err(BeamError::BadArg);
        }
        self.runtime.set_group_leader(target, leader);
        self.atom("ok")
    }
}

#[cfg(test)]
mod tests {
    //! The BIF layer against the runtime's own store. Every case is the oracle's
    //! (OTP 29.1.1 / erts-17.1), quoted at the assertion.

    use super::*;
    use crate::native::test_support::{atom, call, runtime};
    use beam_term::make_small;

    /// The term of `name` as an atom in the runtime's shared table.
    fn is_atom_named(runtime: &crate::Runtime, term: Eterm, name: &str) -> bool {
        runtime.atom_name_of(term) == Some(name)
    }

    // -- erlang:group_leader/2 ------------------------------------------------

    /// Oracle: `erlang:group_leader(self(), self())` ⇒ `true`, and the store really changed —
    /// `erlang:group_leader/0` (through the `ProcessHook` bridge) then reports the new leader.
    #[test]
    fn group_leader_2_sets_the_leader_and_answers_true() {
        let mut runtime = runtime();
        let me = runtime.self_pid().to_eterm();
        let answer = call(&mut runtime, "erlang", "group_leader", &[me, me]).expect("group_leader/2");
        assert!(is_atom_named(&runtime, answer, "true"));
        let leader = call(&mut runtime, "erlang", "group_leader", &[]).expect("group_leader/0");
        assert_eq!(leader, me, "the leader is the pid just set");
    }

    /// Oracle: a non-pid group leader ⇒ `error:badarg`.
    #[test]
    fn group_leader_2_rejects_a_non_pid_leader() {
        let mut runtime = runtime();
        let me = runtime.self_pid().to_eterm();
        let bad = atom(&mut runtime, "nope");
        assert!(matches!(
            call(&mut runtime, "erlang", "group_leader", &[bad, me]),
            Err(BeamError::BadArg)
        ));
    }

    /// Oracle: a non-pid target and a dead target both ⇒ `error:badarg`.
    #[test]
    fn group_leader_2_rejects_a_non_pid_and_a_dead_target() {
        let mut runtime = runtime();
        let me = runtime.self_pid().to_eterm();
        let bad = make_small(7);
        assert!(matches!(
            call(&mut runtime, "erlang", "group_leader", &[me, bad]),
            Err(BeamError::BadArg)
        ));
        // A pid naming no live process (`is_process_alive` ⇒ `false`).
        let dead = ProcessId::new(9_999, 0).to_eterm();
        assert!(matches!(
            call(&mut runtime, "erlang", "group_leader", &[me, dead]),
            Err(BeamError::BadArg)
        ));
    }

    /// Oracle: a **dead** group leader is accepted (`erlang:group_leader(<dead pid>, self())`
    /// ⇒ `true`).
    #[test]
    fn group_leader_2_accepts_a_dead_leader() {
        let mut runtime = runtime();
        let me = runtime.self_pid().to_eterm();
        let dead = ProcessId::new(9_999, 0).to_eterm();
        let answer =
            call(&mut runtime, "erlang", "group_leader", &[dead, me]).expect("a dead leader is ok");
        assert!(is_atom_named(&runtime, answer, "true"));
        let leader = call(&mut runtime, "erlang", "group_leader", &[]).expect("group_leader/0");
        assert_eq!(leader, dead, "the dead leader is recorded as given");
    }

    // -- erts_internal:group_leader/2 ----------------------------------------

    /// Oracle: `erts_internal:group_leader(self(), self())` ⇒ `true`.
    #[test]
    fn erts_group_leader_2_answers_true_on_success() {
        let mut runtime = runtime();
        let me = runtime.self_pid().to_eterm();
        let answer = call(&mut runtime, "erts_internal", "group_leader", &[me, me])
            .expect("erts_internal:group_leader/2");
        assert!(is_atom_named(&runtime, answer, "true"));
    }

    /// Oracle: a bad group leader, a non-pid target and a dead target all **return** the atom
    /// `badarg` (they are not raised).
    #[test]
    fn erts_group_leader_2_returns_the_atom_badarg() {
        let mut runtime = runtime();
        let me = runtime.self_pid().to_eterm();
        let bad = atom(&mut runtime, "nope");
        for args in [
            vec![bad, me],
            vec![me, make_small(7)],
            vec![me, ProcessId::new(9_999, 0).to_eterm()],
        ] {
            let answer = call(&mut runtime, "erts_internal", "group_leader", &args)
                .unwrap_or_else(|e| panic!("{args:?} returns, never raises: {e}"));
            assert!(
                is_atom_named(&runtime, answer, "badarg"),
                "{args:?} answers the atom badarg"
            );
        }
    }

    // -- erts_internal:group_leader/3 ----------------------------------------

    /// Oracle: `erts_internal:group_leader(self(), self(), make_ref())` ⇒ `ok`, and the leader
    /// (applied immediately here) really changes.
    #[test]
    fn erts_group_leader_3_answers_ok_and_applies_the_leader() {
        let mut runtime = runtime();
        let me = runtime.self_pid().to_eterm();
        let reference = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");
        let answer = call(
            &mut runtime,
            "erts_internal",
            "group_leader",
            &[me, me, reference],
        )
        .expect("erts_internal:group_leader/3");
        assert!(is_atom_named(&runtime, answer, "ok"));
        let leader = call(&mut runtime, "erlang", "group_leader", &[]).expect("group_leader/0");
        assert_eq!(leader, me);
    }

    /// Oracle: a non-pid group leader, a non-pid target and a non-reference third argument all
    /// ⇒ `error:badarg` (raised). A dead **target** is accepted (`ok`).
    #[test]
    fn erts_group_leader_3_validates_its_three_arguments() {
        let mut runtime = runtime();
        let me = runtime.self_pid().to_eterm();
        let reference = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");
        let bad = atom(&mut runtime, "nope");
        for args in [
            vec![bad, me, reference],
            vec![me, make_small(7), reference],
            vec![me, me, make_small(7)],
        ] {
            assert!(
                matches!(
                    call(&mut runtime, "erts_internal", "group_leader", &args),
                    Err(BeamError::BadArg)
                ),
                "{args:?} raises badarg"
            );
        }
        // The target need not be live: the C only sends a signal (`bif.c:4949-4963`).
        let dead = ProcessId::new(9_999, 0).to_eterm();
        let answer = call(&mut runtime, "erts_internal", "group_leader", &[me, dead, reference])
            .expect("a dead target is ok");
        assert!(is_atom_named(&runtime, answer, "ok"));
    }

    // -- process_info(Pid, group_leader) --------------------------------------

    /// The store is the **one** source of truth: `process_info(Pid, group_leader)` reports the
    /// leader `group_leader/2` set, for the same process.
    #[test]
    fn process_info_reports_the_stored_leader() {
        let mut runtime = runtime();
        let me = runtime.self_pid().to_eterm();
        let leader_atom = atom(&mut runtime, "group_leader");
        let before = call(&mut runtime, "erlang", "process_info", &[me, leader_atom])
            .expect("process_info/2");
        let _ = before;
        call(&mut runtime, "erlang", "group_leader", &[me, me]).expect("group_leader/2");
        let after = call(&mut runtime, "erlang", "process_info", &[me, leader_atom])
            .expect("process_info/2");
        let value = beam_engine::terms::tuple_element(after, 1).expect("the {group_leader, GL} tuple");
        assert_eq!(value, me, "process_info reflects the explicit set");
    }
}
