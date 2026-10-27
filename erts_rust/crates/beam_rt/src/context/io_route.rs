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
//! **Which route an `io` output call takes**: this runtime's own output stream, or the OTP
//! `{io_request, …}` / `{io_reply, …}` protocol with the caller's **group leader**.
//!
//! # The two routes
//!
//! OTP's `io` module has no output of its own: every `io:format/1,2,3`, `io:put_chars/1,2`,
//! `io:nl/0,1`, `io:write/1,2` and `io:fwrite/1,2,3` is a thin wrapper around
//! `io:o_request/2`, which builds `{io_request, self(), Ref, Request}` for the **device** —
//! and for the omitted-device arity that device is `group_leader()`
//! (`lib/stdlib/src/io.erl`'s `default_output/0`). The device replies `{io_reply, Ref, Reply}`;
//! a `{'DOWN', Ref, …}` instead means `error:terminated`, and an `{error, Reason}` reply is
//! re-raised through `conv_reason/1` (non-`arguments`/`terminated`/`calling_self`/`no_translation`
//! reasons become `badarg`).
//!
//! That protocol is what makes `group_leader/2`-based **output capture** work — it is exactly
//! how `eunit` collects a test's `io:format` output (`lib/eunit/src/eunit_proc.erl`'s
//! `new_group_leader/1` + `group_leader_sync/1`) so a failing test's output can be reported
//! with the failure instead of leaking into the run's own stream.
//!
//! A plain process's leader is this runtime's **root** process ([`Runtime::group_leader_of`]'s
//! default), which is not an io device: posting a request to it would never be answered. So
//! there are two routes, and this module is the one predicate that chooses between them:
//!
//! * **The host fast path.** The leader is this runtime's own output — the root pid, or the
//!   `standard_error` device (`erl_compile`'s worker points its leader there and then prints
//!   through the omitted-device arity). The floor
//!   ([`crate::native::io`]) serves the call directly on the caller's `OutputSink`: no
//!   allocation beyond the rendering, no scheduler round trip, no message.
//! * **The protocol.** Any other leader — a capture process, a `user` device, a file's
//!   `file_io_server`, a **dead** pid (which the oracle answers with `error:terminated`,
//!   because a send to a dead pid is a no-op and its monitor fires immediately). The callee
//!   `lib/stdlib/src/io.erl` body runs **in-machine**
//!   ([`super::RuntimeContext::resolve_external_call`]), so its `receive` suspends the process
//!   through the engine's `RunOutcome::Wait` and the scheduler resumes it when the device
//!   replies — the same seam that already carries `io:getopts/1`/`io:scan_erl_form/4` for
//!   `epp` (`tests/scheduled_io_device.rs`).
//!
//! # Why the seam, and not the BIF
//!
//! [`RuntimeContext::call_bif`] runs **inside** a scheduler slice, where the nested pump is
//! disabled (`may_pump_scheduler() == false`), so a `receive` there folds into the typed
//! `blocked()` gap instead of suspending. Entering the module body through
//! `resolve_external_call` is therefore the only route that can complete the round trip for a
//! **scheduled** process, and it is the route `io`'s other device functions already take.
//!
//! The BIF route still sees an `io` output call in a few shapes — an `apply/3`, a `-eval`
//! expression, a process entry — and [`RuntimeContext::call_bif_routed`] consults the same
//! predicate, running the body through the **pumping** nested runner when the calling context
//! may pump. Inside a slice it answers the typed capability gap rather than writing the
//! request to the wrong stream.
//!
//! # What is deliberately *not* covered
//!
//! The **explicit-device** arity (`io:format(Dev, F, A)`) is decided here by the *leader*,
//! because [`beam_engine::ExecutionContext::resolve_external_call`] receives only
//! `(module, function, arity)` — never the arguments — so a device named there cannot be
//! classified at this seam. With a host-owned leader the floor answers it as before (and a
//! *foreign* pid device stays its documented typed gap); with a device leader the whole call
//! goes through the protocol, which is what the oracle does for every device.
//!
//! [`Runtime::group_leader_of`]: crate::runtime::Runtime::group_leader_of
use super::RuntimeContext;
/// The `io` **output** functions — the ones whose body is an `o_request/2` to a device.
///
/// `io:format/3`, `io:put_chars/2` and `io:nl/1` are the explicit-device arities; the rest
/// address `group_leader()` (`default_output/0`). `io_lib:format/2,3` is **not** here: it is
/// the renderer, it writes nothing.
pub(super) fn is_output_call(function: &str, arity: u32) -> bool {
    matches!(
        (function, arity),
        ("format", 1)
            | ("format", 2)
            | ("format", 3)
            | ("fwrite", 1)
            | ("fwrite", 2)
            | ("fwrite", 3)
            | ("put_chars", 1)
            | ("put_chars", 2)
            | ("write", 1)
            | ("write", 2)
            | ("nl", 0)
            | ("nl", 1)
    )
}
impl RuntimeContext<'_> {
    /// Whether an `io` output call must go through the `{io_request, …}`/`{io_reply, …}`
    /// protocol instead of this runtime's own output stream.
    ///
    /// True for every `io` output key whose **group leader** is not one of the two
    /// stream devices this runtime owns — the root pid (its `init` stand-in) and the
    /// registered `standard_error` process. A dead leader counts as protocol work: the
    /// oracle's answer for it is `error:terminated`, and only the protocol produces that.
    ///
    /// The key test comes first so a non-`io` call pays two bytes of string comparison and
    /// nothing else — this predicate is on the external-call seam, which every `call_ext*`
    /// consults.
    pub(super) fn io_routes_through_the_protocol(
        &self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> bool {
        if module != "io" || !is_output_call(function, arity) {
            return false;
        }
        let protocol = self.io_leader_needs_the_protocol();
        if protocol && crate::debug_trace::enabled() {
            crate::debug_trace::trace(format_args!(
                "io-route [{:?}] io:{function}/{arity} -> the {{io_request,_}} protocol \
                 (leader {:?})",
                self.mailbox_pid,
                self.runtime.group_leader_of(self.mailbox_pid),
            ));
        }
        protocol
    }
    /// Whether the calling process's group leader is a **device** rather than this runtime's
    /// own output stream.
    ///
    /// The stream leaders are the three pids the floor already treats as streams when they are
    /// named explicitly — the symmetry that matters, because a leader and an explicit device
    /// name the same device class and must classify alike:
    ///
    /// * the **root** pid ([`Runtime::root_pid`]) — the default leader, standing in for OTP's
    ///   `init`/`user` terminal. Note it is the *root*, not
    ///   [`Runtime::self_pid`](crate::runtime::Runtime::self_pid): during an action `self_pid`
    ///   is the action's own child, and comparing against that made every action process look
    ///   like it had a foreign leader (measured: `eunit`'s listener, whose leader is the root,
    ///   posted an `{io_request, …}` to it and blocked forever);
    /// * the registered **`standard_error`** process — what `erl_compile`'s worker and
    ///   `error_logger`'s fallback point at (`Stream::Stderr`);
    /// * the registered **`user`** process — the terminal device, which [`Self::io_device`]
    ///   treats as the output stream by name (`Stream::Output`).
    ///
    /// Everything else — a capture process, a file's io server, a dead pid — needs the
    /// protocol.
    ///
    /// [`Runtime::root_pid`]: crate::runtime::Runtime::root_pid
    pub(super) fn io_leader_needs_the_protocol(&self) -> bool {
        let leader = self.runtime.group_leader_of(self.mailbox_pid);
        if leader == self.runtime.root_pid() {
            return false;
        }
        // The **one** `standard_error` lookup in the workspace: the floor's own device
        // classifier reads the same name registry through the same helper, so the two can
        // never disagree about which process is the error stream. `user` is the same shape.
        Some(leader) != self.standard_error_pid() && Some(leader) != self.user_pid()
    }
}
#[cfg(test)]
mod tests {
    use super::is_output_call;
    /// The output keys are exactly OTP's `o_request/2` wrappers — and the renderer
    /// (`io_lib:format/2,3`) is deliberately not one of them.
    #[test]
    fn the_output_key_table_is_the_o_request_wrappers() {
        for (function, arity) in [
            ("format", 1),
            ("format", 2),
            ("format", 3),
            ("fwrite", 1),
            ("fwrite", 2),
            ("fwrite", 3),
            ("put_chars", 1),
            ("put_chars", 2),
            ("write", 1),
            ("write", 2),
            ("nl", 0),
            ("nl", 1),
        ] {
            assert!(is_output_call(function, arity), "io:{function}/{arity}");
        }
        for (function, arity) in [
            ("format", 4),
            ("getopts", 1),
            ("setopts", 2),
            ("request", 2),
            ("request", 3),
            ("request", 4),
            ("requests", 1),
            ("get_chars", 1),
            ("get_line", 1),
            ("read", 1),
            ("scan_erl_form", 4),
            ("columns", 0),
            ("put_chars", 3),
            ("nl", 2),
        ] {
            assert!(!is_output_call(function, arity), "io:{function}/{arity}");
        }
    }
}
