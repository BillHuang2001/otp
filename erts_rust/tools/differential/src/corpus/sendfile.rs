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
//! Contract **A.4** — the port `sendfile` / `advise` surface above the driver.
//!
//! A `file:sendfile/5` on an inet socket does not reach the driver by itself. It walks
//! `file:sendfile/5` → `file:sendfile/8` (`lib/kernel/src/file.erl:2723-2743`) →
//! `prim_file:sendfile/8` (`erts/preloaded/src/prim_file.erl:420-439`), whose body
//! **advises** the descriptor (`advise(Fd, Offset, Bytes, sequential)`, a local call into the
//! non-exported NIF stub `advise_nif/4`) and then asks the inet driver to move the bytes
//! (`prim_inet:sendfile/4` → `TCP_REQ_SENDFILE`, opcode 45). This runtime's `beam_io` answers
//! `{error,enotsup}` for opcode 45, so `file.erl`'s `case` falls through to OTP's own Erlang
//! `sendfile_fallback` — a userspace read/write loop over the **port command seam**, whose
//! caller tag is zero-length. Both sides therefore deliver the requested extent, by different
//! mechanisms; that equivalence is what this group pins. The same seam is what serves
//! `prim_file:advise/4` as a *hint* (`posix_fadvise(2)` is never called, so a live descriptor
//! answers `ok` whatever the advice is).
//!
//! No corpus case id contained `sendfile` or `advise` before this group: the runtime-side
//! matrix in `crates/beam_rt/src/native/prim_file/{sendfile,advise}.rs` was unit-tested and
//! never held against the oracle by the gate.
//!
//! ## What each row pins
//!
//! * **`file_sendfile_over_an_inet_socket_delivers_the_requested_extent`** — the whole chain
//!   end to end: a 36-byte file written to a run-unique path (from `os:getpid()`, so no temp
//!   path can enter a golden), a `[raw, read, binary]` descriptor, a loopback
//!   `gen_tcp:listen/2` + `connect/3` + `accept/1` pair, and `file:sendfile(Fd, Cl, 10, 5, [])`
//!   for the **sub-extent** at offset 10. The row reduces to `{Answer, ByteCount, Bytes}`:
//!   `{ok,5}` from the call, and the five bytes `<<"abcde">>` read off the peer's
//!   `{active, true}` queue by a bounded accumulate-until-N loop (never `gen_tcp:recv/2,3`).
//!   So the answer *and* the bytes really on the wire are pinned, in order and byte-exact.
//! * **`prim_file_advise_badarg_wrapper_shape`** — the `{error,badarg}` the `prim_file:advise/4`
//!   wrapper surfaces. Its body is `try #{handle := FRef} = get_fd_data(Fd),
//!   advise_nif(FRef, Offset, Length, Advise) catch error:badarg -> {error, badarg} end`
//!   (`prim_file.erl:274-280`), so the wrapper answer exists only where the NIF **raises**
//!   `badarg`: a non-*number* `Offset` or `Length` on a live descriptor. The row pins both
//!   positions, and — together with them — the identical `{error,badarg}` that
//!   `file:sendfile/8`'s own catch-all clause answers for a non-descriptor file argument
//!   (`file.erl:2818`), which is a plain `file.erl` value rather than the stub's
//!   `erlang:nif_error(undef)`.
//! * **`prim_file_advise_on_a_live_descriptor_answers_ok`** — the hint itself: all six advice
//!   atoms (`normal | random | sequential | will_need | dont_need | no_reuse`) answer `ok`, an
//!   `int64`-fitting bignum extent (`9223372036854775807`, past the 60-bit small-int range) and a
//!   zero-length extent answer `ok`, and only a negative extent / an out-of-set advice atom are
//!   `{error,einval}`. That is the C `advise_nif_impl` classification (`prim_file_nif.c:859-887`)
//!   observed from Erlang, and it is why the sendfile chain can advise before it sends.
//!
//! ## What is deliberately NOT pinned
//!
//! * **A non-descriptor argument to `prim_file:advise/4`.** Measured on the host oracle
//!   (OTP 29.1.2 / erts-17.1.1): `prim_file:advise(not_a_descriptor, 0, 10, normal)` **raises**
//!   `{'EXIT',{function_clause,[{prim_file,get_fd_data,1,[…]}, …]}}` — `get_fd_data/1` has one
//!   clause, and `function_clause` is not the `error:badarg` the wrapper catches. This runtime
//!   answers the value `{error,badarg}` there (the documented uniform malformed-descriptor
//!   divergence of the whole `prim_file` floor, `crates/beam_rt/src/native/prim_file/advise.rs`
//!   §"Documented divergences"), so a row pinning it would be a `mismatch`. Guarded by the
//!   group's `no_case_pins_the_malformed_descriptor_divergence` test: every `prim_file:advise(`
//!   in this group is handed the descriptor `file:open/2` returned.
//! * **The `{error,enotsup}` header/trailer row** (`prim_file.erl:439`, the non-empty
//!   `Headers`/`Trailers` clause). `file:sendfile/5`'s option list cannot express them — the
//!   `chunk_size` branch is the only one that keeps them empty (`file.erl:2729-2743`) — so the
//!   only way to reach that clause is to call the preloaded `prim_file:sendfile/8` directly,
//!   which is not the surface a caller drives.
//! * **The 16 MiB extent cap** (`crates/beam_rt/src/native/prim_file/sendfile.rs`'s
//!   `MAX_SENDFILE_BYTES`): past it this runtime answers a **typed** `unsupported` capability
//!   gap where the oracle keeps reading. That is a gap, not an agreement, so no row pins it.
//! * **A descriptor opened by another process** (`not_on_controlling_process`) and a **closed**
//!   descriptor (`{error,einval}`): both need state a single-expression row would have to build
//!   out of a second process or a closed handle, and neither is what this group is about — the
//!   sendfile chain always advises the descriptor it was handed. They stay unpinned here rather
//!   than being claimed untested.
//!
//! Every expression is a single line, binds nothing named `V`/`C`/`R` (the probe wrapper binds
//! them), keeps its file path run-unique, deletes that file before it answers, and bounds every
//! `receive` with `after` — so no golden carries a temp path, a pid or a timeout.
use super::{Case, CaseKind, Contract, Golden, Probe};
/// One `sendfile/*` eval case.
///
/// [`Probe::Value`] prints the expression's own answer as a readable line, and the golden is
/// the **oracle's** exact stdout line — measured with `differential run <id>`, never
/// hand-written. Every case exits `0`: each refusal is a plain `{error, posix() | badarg}`
/// value, so nothing is left to escape.
fn case(
    id: &'static str,
    description: &'static str,
    contracts: &'static [Contract],
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts,
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}
/// The A.4 port `sendfile` / `advise` group: `sendfile/*`.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- the whole file:sendfile/5 chain, answer + the bytes on the wire ---------------
        case(
            "sendfile/file_sendfile_over_an_inet_socket_delivers_the_requested_extent",
            "file:sendfile/5 for the sub-extent {offset 10, length 5} of a 36-byte file over a \
             loopback gen_tcp connection answers {ok,5} and puts exactly the five bytes \
             <<\"abcde\">> on the peer's {active,true} queue — the chain \
             file:sendfile/8 -> prim_file:sendfile/8 -> (advise, then the driver's enotsup) -> \
             OTP's own Erlang sendfile_fallback delivers the requested extent byte-exactly",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin Pid = os:getpid(), \
             Path = \"/tmp/differential_sendfile_\" ++ Pid ++ \".bin\", \
             Content = <<\"0123456789abcdefghijklmnopqrstuvwxyz\">>, \
             ok = file:write_file(Path, Content), \
             {ok, Fd} = file:open(Path, [raw, read, binary]), \
             {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, {active, false}]), \
             {ok, Port} = inet:port(L), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, [binary, {active, false}]), \
             {ok, A} = gen_tcp:accept(L), \
             ok = inet:setopts(A, [{active, true}]), \
             Answer = file:sendfile(Fd, Cl, 10, 5, []), \
             Drain = fun Loop(Acc) when byte_size(Acc) >= 5 -> Acc; \
                         Loop(Acc) -> receive {tcp, A, Db} -> \
                                          Loop(<<Acc/binary, Db/binary>>) \
                                      after 2000 -> Acc end end, \
             Got = Drain(<<>>), \
             ok = file:close(Fd), \
             gen_tcp:close(Cl), \
             gen_tcp:close(A), \
             gen_tcp:close(L), \
             ok = file:delete(Path), \
             {Answer, byte_size(Got), Got} end",
            "ok {{ok,5},5,<<97,98,99,100,101>>}",
        ),
        // --- the prim_file:advise/4 wrapper's {error,badarg} shape --------------------------
        case(
            "sendfile/prim_file_advise_badarg_wrapper_shape",
            "prim_file:advise/4 on a live [raw,read,binary] descriptor answers the wrapper value \
             {error,badarg} for a non-number Offset (foo) and for a non-number Length (bar) — the \
             catch the wrapper's try body adds around the NIF's badarg raise — and \
             file:sendfile/8's own catch-all answers that same {error,badarg} for a \
             non-descriptor file argument (a plain file.erl value, not the stub's undef raise)",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin Pid = os:getpid(), \
             Path = \"/tmp/differential_advise_\" ++ Pid ++ \".bin\", \
             ok = file:write_file(Path, <<\"0123456789a\">>), \
             {ok, Fd} = file:open(Path, [raw, read, binary]), \
             R1 = prim_file:advise(Fd, foo, 10, normal), \
             R2 = prim_file:advise(Fd, 0, bar, normal), \
             R3 = file:sendfile(foo, self(), 0, 10, []), \
             ok = file:close(Fd), \
             ok = file:delete(Path), \
             {R1, R2, R3} end",
            "ok {{error,badarg},{error,badarg},{error,badarg}}",
        ),
        // --- the hint: a live descriptor answers ok ----------------------------------------
        case(
            "sendfile/prim_file_advise_on_a_live_descriptor_answers_ok",
            "prim_file:advise/4's hint behaviour on a live descriptor: all six advice atoms \
             (normal/random/sequential/will_need/dont_need/no_reuse) answer ok, an int64-fitting \
             bignum extent and a zero-length extent answer ok, and only a negative extent or an \
             out-of-set advice atom answer {error,einval} — the C advise_nif_impl \
             classification, which is what lets prim_file:sendfile/8 advise before it sends",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin Pid = os:getpid(), \
             Path = \"/tmp/differential_adviseok_\" ++ Pid ++ \".bin\", \
             ok = file:write_file(Path, <<\"0123456789a\">>), \
             {ok, Fd} = file:open(Path, [raw, read, binary]), \
             Six = [prim_file:advise(Fd, 0, 10, Adv) || \
                    Adv <- [normal, random, sequential, will_need, dont_need, no_reuse]], \
             Big = prim_file:advise(Fd, 9223372036854775807, 9223372036854775807, normal), \
             Zero = prim_file:advise(Fd, 0, 0, normal), \
             Other = {prim_file:advise(Fd, -1, 10, normal), \
                      prim_file:advise(Fd, 0, 10, foo)}, \
             ok = file:close(Fd), \
             ok = file:delete(Path), \
             {Six, Big, Zero, Other} end",
            "ok {[ok,ok,ok,ok,ok,ok],ok,ok,{{error,einval},{error,einval}}}",
        ),
    ]
}
#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{group, Contract};
    /// The group is a real A.4 group: ids are `sendfile/`-prefixed, unique and described, and
    /// every case is a self-terminating `eval` case whose golden is a non-empty line at exit `0`
    /// (each refusal is a plain `{error, posix() | badarg}` value, never an exception left to
    /// escape).
    #[test]
    fn every_case_is_a_described_exit_zero_sendfile_eval_case() {
        let cases = group();
        assert_eq!(cases.len(), 3, "the group's size is pinned by this test");
        let mut ids = Vec::new();
        for case in &cases {
            assert!(
                case.id.starts_with("sendfile/"),
                "case {} is not in the sendfile/ group",
                case.id
            );
            assert!(
                !ids.contains(&case.id),
                "duplicate case id {}",
                case.id
            );
            ids.push(case.id);
            assert!(
                case.contracts.contains(&Contract::DriverAbi),
                "case {} must cover contract A.4",
                case.id
            );
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
            assert_eq!(case.golden.exit_code, 0, "case {} must exit 0", case.id);
            let golden = case.golden.stdout.expect("every case carries a golden line");
            assert!(!golden.is_empty(), "case {} has an empty golden", case.id);
            assert!(
                golden.starts_with("ok "),
                "case {}: the golden is the probe's own ok line, got {golden:?}",
                case.id
            );
            assert!(!golden.ends_with('\n'), "case {}: a golden is a line", case.id);
        }
        let unique: HashSet<_> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "duplicate case id in the sendfile/ group"
        );
    }
    /// A row must be a single-line expression that keeps its file path run-unique and cleans it
    /// up, so no golden can ever carry a temp path, and one that bounds every `receive` — an
    /// unbounded receive would hang the gate instead of answering.
    #[test]
    fn every_case_uses_a_run_unique_path_a_bounded_receive_and_cleans_up() {
        for case in &group() {
            let expr = match &case.kind {
                super::CaseKind::Eval { expr, .. } => *expr,
                other => panic!("case {} is not an eval case: {other:?}", case.id),
            };
            assert!(!expr.contains('\n'), "case {} spans lines", case.id);
            assert!(
                expr.contains("os:getpid()"),
                "case {}: the path is not nonce-derived, so it is not run-unique",
                case.id
            );
            assert!(
                expr.contains("file:delete(") && expr.contains("file:write_file("),
                "case {}: the row must create and delete its own file",
                case.id
            );
            if expr.contains("receive") {
                assert!(
                    expr.contains("after "),
                    "case {}: every receive must be bounded by `after`",
                    case.id
                );
            }
            // The passive read is the shape that still diverges (see `inet_control`): the peer
            // of this group drains its `{active, true}` queue instead.
            assert!(
                !expr.contains("gen_tcp:recv("),
                "case {}: pins a passive read; drain the active-mode queue instead",
                case.id
            );
        }
    }
    /// `prim_file:advise/4` handed a term that is not a `#file_descriptor{}` **raises**
    /// `function_clause` on the oracle (`get_fd_data/1` has one clause) while this runtime
    /// answers the value `{error,badarg}` — the documented malformed-descriptor divergence of
    /// the `prim_file` floor (`crates/beam_rt/src/native/prim_file/advise.rs`). A row pinning it
    /// would be a `mismatch`, so every `prim_file:advise(` here takes the descriptor
    /// `file:open/2` returned.
    #[test]
    fn no_case_pins_the_malformed_descriptor_divergence() {
        for case in &group() {
            let expr = match &case.kind {
                super::CaseKind::Eval { expr, .. } => *expr,
                other => panic!("case {} is not an eval case: {other:?}", case.id),
            };
            assert!(
                expr.contains("{ok, Fd} = file:open("),
                "case {}: the descriptor must come from file:open/2",
                case.id
            );
            let mut rest = expr;
            while let Some(at) = rest.find("prim_file:advise(") {
                rest = &rest[at + "prim_file:advise(".len()..];
                assert!(
                    rest.starts_with("Fd,"),
                    "case {}: prim_file:advise/4 is handed a non-descriptor ({rest:?})",
                    case.id
                );
            }
        }
    }
}
