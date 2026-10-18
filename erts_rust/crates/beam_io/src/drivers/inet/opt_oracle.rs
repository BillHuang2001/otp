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


//! Shared oracle machinery for the option-family suites and the `accept_opts` replay: spawn a
//! real system `tcp_inet`/`udp_inet` port and run the same `port_control` sequence on both sides.
#![allow(dead_code)]

//! The shared oracle machinery for the socket-option suites.
//!
//! Four test modules need to drive the **real** system `tcp_inet`/`udp_inet` driver through
//! `open_port({spawn_driver, Name}, [binary])` + `erlang:port_control/3` and byte-compare its
//! replies against ours: [`opt_tests`](super::opt_tests), the two option-family suites
//! (`opt_state_tests.rs`, `opt_sys_tests.rs`) and the `prim_inet:accept_opts/3` replay
//! (`accept_opts_tests.rs`). The helpers live here — one copy — and every one of them is
//! `pub(super)` so the sibling modules reach them as `super::oracle::*`.
//!
//! Both sides run the **same** `(request code, payload)` sequence: the oracle from Erlang, ours
//! through a real [`PortRegistry`] port over the reactor, so a difference is a genuine
//! behavioural divergence rather than a harness artefact. Every `erl`-dependent helper **skips
//! loudly** (a `SKIP:`-style `eprintln!` plus `None`) when `erl` is absent or exceeds its
//! budget — no oracle case ever fails for want of a tool, and none asserts a wall-clock value.

use crate::driver::DriverRegistry;
use crate::drivers::inet::{INET_AF_INET, INET_PROTO_DEFAULT, INET_TYPE_DGRAM, INET_TYPE_STREAM};
use crate::poller::Poller;
use crate::port::PortRegistry;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// The C request numbers of the verbs the cross-checks drive (`inet_drv.c:805`-`:847`) and
/// the magic offset the Erlang side adds (`prim_inet.erl:2969`,
/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`).
pub(super) const INET_REQ_OPEN: u32 = 1;
pub(super) const INET_REQ_CLOSE: u32 = 2;
pub(super) const INET_REQ_SETOPTS: u32 = 7;
pub(super) const INET_REQ_GETOPTS: u32 = 8;
pub(super) const INET_REQ_BIND: u32 = 6;
pub(super) const INET_REQ_LISTEN: u32 = 27;
pub(super) const INET_REQ_ACCEPT: u32 = 26;
pub(super) const CONTROL_MAGIC: u32 = 0x03f1_a300;

/// Wall-clock budget for the `erl` oracle before the affected check is skipped.
pub(super) const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// The packed `open` argument for a stream socket: `[AF_INET, SOCK_STREAM, default protocol]`.
pub(super) fn open_arg() -> Vec<u8> {
    vec![INET_AF_INET, INET_TYPE_STREAM, INET_PROTO_DEFAULT]
}

/// The packed `open` argument for a datagram socket.
pub(super) fn open_arg_dgram() -> Vec<u8> {
    vec![INET_AF_INET, INET_TYPE_DGRAM, INET_PROTO_DEFAULT]
}

/// The verb name a driver's `control` entry answers for a C request code.
pub(super) fn verb_name(code: u32) -> &'static str {
    match code {
        INET_REQ_OPEN => "open",
        INET_REQ_CLOSE => "close",
        INET_REQ_SETOPTS => "setopts",
        INET_REQ_GETOPTS => "getopts",
        INET_REQ_BIND => "bind",
        INET_REQ_LISTEN => "listen",
        INET_REQ_ACCEPT => "accept",
        other => panic!("no control verb for request code {other}"),
    }
}

// ---------------------------------------------------------------------------
// The oracle side
// ---------------------------------------------------------------------------

/// The Erlang side of the cross-check: it opens a **real** port on `driver` with
/// `open_port({spawn_driver, …})` (exactly as `prim_inet.erl:103` does) and runs the same
/// sequence, printing each reply as comma-separated bytes, one reply per `|`-separated field.
pub(super) fn oracle_expression(driver: &str, sequence: &[(u32, Vec<u8>)]) -> String {
    let calls: Vec<String> = sequence
        .iter()
        .map(|(code, arg)| {
            let bytes: Vec<String> = arg.iter().map(u8::to_string).collect();
            format!("F({code}, <<{}>>)", bytes.join(","))
        })
        .collect();
    format!(
        "M = {CONTROL_MAGIC}, \
         P = open_port({{spawn_driver, \"{driver}\"}}, [binary]), \
         F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
         R = [{}], \
         S = fun(L) -> lists:flatten(lists:join(\",\", [integer_to_list(B) || B <- L])) end, \
         io:format(\"~s~n\", [lists:flatten(lists:join(\"|\", [S(L) || L <- R]))])",
        calls.join(", ")
    )
}

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or `None` — after a
/// loud skip message — when `erl` cannot be spawned or does not answer in time.
pub(super) fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(
            Command::new("erl")
                .arg("-noshell")
                .arg("-eval")
                .arg(&code)
                .output(),
        );
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(err)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({err}); is Erlang on PATH?");
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "the oracle expression failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// Parses the oracle's `a,b,c|d,e|…` reply dump into one byte vector per reply.
pub(super) fn parse_reply_dump(stdout: &str) -> Option<Vec<Vec<u8>>> {
    let line = stdout.lines().find(|line| line.contains('|'))?;
    let mut replies = Vec::new();
    for field in line.trim().split('|') {
        let mut bytes = Vec::new();
        for part in field.split(',') {
            if part.is_empty() {
                continue;
            }
            bytes.push(part.trim().parse::<u8>().ok()?);
        }
        replies.push(bytes);
    }
    Some(replies)
}

// ---------------------------------------------------------------------------
// The Rust side
// ---------------------------------------------------------------------------

/// The Rust side: the identical sequence driven through a real port over the reactor, with
/// `driver` naming the registered built-in.
pub(super) fn rust_replies(driver: &str, sequence: &[(u32, Vec<u8>)]) -> Vec<Vec<u8>> {
    let mut poller = Poller::new().expect("a poll set");
    let registry = DriverRegistry::with_builtins();
    let created = registry
        .create(driver)
        .expect("the built-in driver is registered");
    let mut ports = PortRegistry::new();
    let id = ports.open(&mut poller, created).expect("the port opens");

    let replies = sequence
        .iter()
        .map(|(code, arg)| {
            let verb = verb_name(*code);
            ports
                .get_mut(id)
                .expect("the port is live")
                .control(&mut poller, verb, arg)
                .unwrap_or_else(|err| panic!("the `{verb}` verb failed: {err:?}"))
        })
        .collect();

    ports.close(&mut poller, id).expect("closes the port");
    replies
}
/// Compares `rust` against the oracle reply for reply, or **skips loudly** when `erl` is absent.
pub(super) fn assert_agrees_with_oracle(
    driver: &str,
    sequence: &[(u32, Vec<u8>)],
    rust: &[Vec<u8>],
) {
    let Some(stdout) = erl_stdout(&oracle_expression(driver, sequence)) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let Some(oracle) = parse_reply_dump(&stdout) else {
        panic!("could not parse the oracle reply dump: {stdout:?}");
    };
    assert_eq!(oracle.len(), sequence.len(), "one reply per step: {oracle:?}");

    eprintln!("oracle option replies: {oracle:?}");
    eprintln!("rust option replies:   {rust:?}");

    for (index, ((code, arg), (rust_reply, oracle_reply))) in
        sequence.iter().zip(rust.iter().zip(oracle.iter())).enumerate()
    {
        assert_eq!(
            rust_reply, oracle_reply,
            "reply {index} (`{}` of {arg:?}, request code {code}) disagrees",
            verb_name(*code)
        );
    }
}

/// A zeroed `[opt, val_be32]` record for `code`, with `value` in the four payload bytes.
pub(super) fn record(code: u8, value: i32) -> Vec<u8> {
    let mut record = vec![code];
    record.extend_from_slice(&value.to_be_bytes());
    record
}
