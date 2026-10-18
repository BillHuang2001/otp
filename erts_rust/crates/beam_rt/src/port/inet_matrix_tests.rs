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
//! End-to-end acceptance of the numeric→named **`inet` control-opcode bridge** (contract A.4) as it
//! is reached from Erlang: `erlang:port_control/3` → the runtime-native floor
//! ([`crate::native::port`]'s `port_control`) → [`RuntimePorts::port_control`] →
//! [`crate::PortRegistry::port_control`] → `beam_io`'s driver `control` callback.
//!
//! The C's socket drivers decode one wire: `port_control(Port, Op, Data)` where `Op` is
//! `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER + request` (`erts/emulator/beam/sys.h:1466`, decoded by
//! `inet_ctl`/`packet_inet_ctl` in `erts/emulator/drivers/common/inet_drv.c`), and `Data` is the
//! request's packed operand. Three things therefore have to hold *through this crate*, none of
//! which `beam_io`'s own driver-seam suite can see:
//!
//! 1. the **capability gate** admits a `{spawn_driver, Name}` port whose driver declares a
//!    `control` callback (`udp_inet`, `tcp_inet`) and refuses one that does not
//!    (`ram_file_drv` — pinned by the sibling `control_capability_tests`);
//! 2. the **decimal `c_uint` rendering** of the operation reaches the driver unchanged, so the
//!    magic-offset subtraction inside it lands on the request the Erlang side meant;
//! 3. the reply is rendered the C's way — `buf_to_intlist` — as an Erlang list of byte integers,
//!    because neither `udp_inet` nor `tcp_inet` ever sets `PORT_CONTROL_FLAG_BINARY` for
//!    `erlang:port_control/3`.
//!
//! # What is compared, and how
//!
//! Every family below is one **port state sequence**: the same `(opcode, operand)` list is driven
//! through this runtime's `erlang:port_control/3` and through the system `erl`'s, in one
//! `begin … end` per family, and the two sessions are compared **line for line** — each line is
//! one probe's *whole* rendered reply (`~w`, i.e. `[1,4,0,0,0,0]`), or the bare atom `badarg`
//! where the oracle raises. A mismatch therefore names the shape that broke. Rows whose reply
//! carries an **ephemeral port** (`bind`'s `[1,hi,lo]`, `name`/`getladdrs`'s packed address) are
//! compared by shape — two sockets never get the same port — and the address octets are compared
//! exactly (the [`Rule`] discipline `beam_io`'s own `udp` matrix established).
//!
//! The families are the `udp/*` corpus surface `beam_io` reports (its
//! `drivers/inet/opcode_tests_udp.rs::oracle_matrix` plus the option store): the bare
//! (socket-less) refusals and fake addresses, the datagram lifecycle, the asynchronous receive
//! slot, the connected/peer half, the opcode-scope rules, the option store, the shared `inet_ctl`
//! arms (`subscribe`, a pre-adoption `fdopen`, the `getservby*` name service, the `getopts`
//! truncate-and-continue rule), the interface requests (`getiflist`/`ifget`/`ifset`/`getifaddrs`),
//! and the `tcp_inet` half of the same bridge: its bare refusals and the listening stream whose
//! accept is still queued.
//!
//! # The residual table
//!
//! What this driver floor still answers differently from C ERTS is a **capability gap or a
//! documented substitution**, never a wrong result:
//!
//! [`the_documented_datagram_residuals_stay_typed`] — the rows reached on a `udp_inet` port,
//! asserted on **both** sides (the exact oracle output, so the record cannot rot, plus this
//! runtime's honest answer): `open` of a non-`AF_INET` family (this floor's own `[0]eafnosupport`
//! where the C creates the socket) and `INET_REQ_FDOPEN` on a port that already holds a socket
//! (this floor's `[0]einval` where the C adopts the operand's descriptor and leaks the live
//! socket).
//!
//! The asynchronous `PACKET_REQ_RECV` op-id used to be the table's third row — served here as a
//! synchronous read-now substitution, answering the bare `[1]` — until `beam_io` grew the async-op
//! queue and the driver→owner completion channel. Both sides now answer
//! `[1] ++ <<Id:16/big>>`, so the shape is an ordinary agreement row
//! ([`the_datagram_async_receive_slot_matches_the_oracle`]). Its stream sibling
//! ([`the_queued_accept_stream_matches_the_oracle`]) is pinned the same way: a queued
//! `INET_REQ_ACCEPT` keeps the port a listener, and the rows read in that state — the
//! `GETSTATUS` bit set and the two stream-only requests — agree with the oracle.
//!
//! # Layout
//!
//! The file is deliberately one **matrix**: a builder per port-state sequence, the shared
//! [`Probe`]/[`Rule`]/[`Session`] comparison machinery, and the residual table, because every
//! sequence is read against the same vocabulary and against each other. It is a table, not a
//! subsystem, so it is kept whole rather than split into sibling files; the capability-gate matrix
//! over the same bridge is the sibling `control_capability_tests`.
//!
//! # The oracle
//!
//! The system `erl` on `PATH` (OTP 29.1.1 / erts-17.1) is the oracle, driven exactly as the rest of
//! this crate's suites drive it ([`erl_eval`]). A missing toolchain is a **loud skip**, never a
//! silent pass; [`the_headline_numeric_shapes_answer_without_an_oracle`] pins the headline
//! answers unconditionally, so the runtime half is asserted even with no `erl` present.
use super::tests::{erl_eval, fixture, skip_oracle_check, Fixture};
use super::*;
use crate::config::RuntimeConfig;
use beam_engine::terms;
use beam_term::make_small;

/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`erts/emulator/beam/sys.h:1466`) — the offset the
/// Erlang side adds to every `inet` control request and both `*_inet_ctl` entries subtract again.
const INET_OPCODE_MAGIC: u64 = 0x03f1_a300;

/// How a reply is compared, chosen by whether it can be deterministic at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rule {
    /// Byte-for-byte identical on both sides.
    Exact,
    /// `[1] ++ port_hi ++ port_lo`: the kernel assigns the ephemeral port, so only the shape (tag,
    /// length, a non-zero port) is comparable.
    BoundPort,
    /// `[1, family, port_hi, port_lo, a, b, c, d]`: two independent sockets bind two different
    /// ephemeral ports, so the tag, the family and the address octets are compared.
    PackedAddress,
    /// `[0] ++ errno-name`: the C answers with the *descriptor's own* `errno`, which depends on the
    /// process' file-descriptor table, so only the error tag and the errno-name shape are compared.
    ErrorReply,
    /// A **host-dependent** reply (the machine's interface table): both sides must answer the same
    /// stable prefix — never a literal host list, which would rot on any other machine.
    Prefix(&'static str),
}
/// One probe of the matrix: the line label both renderings print, the integer `port_control/3`
/// receives, the operand bytes and the comparison rule.
#[derive(Clone)]
struct Probe {
    /// The line label, spelled identically on both sides so the sessions are comparable.
    label: &'static str,
    /// The `Operation` integer — `request + INET_OPCODE_MAGIC`, or a raw value for a wrapped one.
    op: u64,
    /// The `Data` operand (`port_control/3` reads it as iodata, so these are the payload bytes).
    arg: Vec<u8>,
    /// How the two sides' replies are compared.
    rule: Rule,
}

impl Probe {
    /// The C request `code`, sent the way the Erlang side sends it (`code + magic`).
    fn req(label: &'static str, code: u32, arg: impl Into<Vec<u8>>, rule: Rule) -> Probe {
        Probe {
            label,
            op: u64::from(code) + INET_OPCODE_MAGIC,
            arg: arg.into(),
            rule,
        }
    }

    /// The C request `code` with the exact-compare rule — by far the common case.
    fn exact(label: &'static str, code: u32, arg: impl Into<Vec<u8>>) -> Probe {
        Probe::req(label, code, arg, Rule::Exact)
    }

    /// The C request `code` whose reply carries the descriptor's own `errno` (the C's
    /// `ctl_error(sock_errno())`), so only the error tag and the errno-name shape are compared.
    fn error_reply(label: &'static str, code: u32, arg: impl Into<Vec<u8>>) -> Probe {
        Probe::req(label, code, arg, Rule::ErrorReply)
    }

    /// The C request `code` whose reply lists this machine's interfaces, so only `prefix` — the
    /// stable part of the answer — is compared.
    fn prefix(
        label: &'static str,
        code: u32,
        arg: impl Into<Vec<u8>>,
        prefix: &'static str,
    ) -> Probe {
        Probe::req(label, code, arg, Rule::Prefix(prefix))
    }    /// A **raw** integer: an opcode below the magic number (whose unsigned subtraction wraps past
    /// every `case`) or a retired request number.
    fn raw(label: &'static str, value: u64, arg: impl Into<Vec<u8>>) -> Probe {
        Probe {
            label,
            op: value,
            arg: arg.into(),
            rule: Rule::Exact,
        }
    }
}

/// What this runtime answered for one probe.
enum Reply {
    /// The rendered control reply's bytes (the C's `ctl_reply` payload).
    Bytes(Vec<u8>),
    /// The `badarg` the C raises through `erts_internal_port_control_3` for an unknown port, an
    /// out-of-range operation, non-iodata data or a driver with no `control` slot.
    BadArg,
    /// A typed capability gap (the `unsupported` family) — never a fabricated reply.
    Gap(String),
}

impl Reply {
    /// The reply rendered the way `~w` prints it, so it can be compared with an oracle line.
    fn render(&self) -> String {
        match self {
            Reply::Bytes(bytes) => render_bytes(bytes),
            Reply::BadArg => "badarg".to_string(),
            Reply::Gap(message) => format!("gap({message})"),
        }
    }

    /// The reply bytes, panicking with `label` when this is not a byte reply.
    fn bytes(&self, label: &str) -> Vec<u8> {
        match self {
            Reply::Bytes(bytes) => bytes.clone(),
            other => panic!("{label}: expected a byte reply, got {}", other.render()),
        }
    }
}

/// `bytes` rendered the way `~w` prints a list of integers: `[1,4,0,0,0,0]`.
fn render_bytes(bytes: &[u8]) -> String {
    let rendered: Vec<String> = bytes.iter().map(u8::to_string).collect();
    format!("[{}]", rendered.join(","))
}

/// An Erlang binary literal for `bytes` (`<<1,2,0>>`), as both the oracle expression and a
/// `port_control/3` operand spell it.
fn binary_literal(bytes: &[u8]) -> String {
    let rendered: Vec<String> = bytes.iter().map(u8::to_string).collect();
    format!("<<{}>>", rendered.join(","))
}

/// The integers of a rendered reply (`[1,4,0]` → `vec![1, 4, 0]`); panics on `badarg`.
fn ints_of(rendered: &str) -> Vec<u8> {
    let inner = rendered
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or_else(|| panic!("{rendered:?} is not an integer list"));
    if inner.is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|number| {
            number
                .trim()
                .parse::<u8>()
                .unwrap_or_else(|_| panic!("{rendered:?} is not an integer list"))
        })
        .collect()
}

/// The process every port the fixture's hook opens claims as its owner.
///
/// A live runtime plus the port provider built over the runtime's **own** registry, so a port
/// opened here is the port the runtime's `port_control` path sees.
struct Session {
    /// The runtime whose registry every port is opened on (and whose atom table the floor reads).
    runtime: Runtime,
    /// The term/heap fixture (the `open_port/2` `Name`/`Options` vocabulary).
    fx: Fixture,
    /// The provider under test.
    hook: RuntimePorts,
}

impl Session {
    /// A fresh runtime with no port open.
    fn new() -> Session {
        let fx = fixture();
        let runtime = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let owner = runtime.self_pid();
        let hook = fx.hook_over(Rc::clone(&runtime.ports), owner);
        Session { runtime, fx, hook }
    }

    /// Opens `{spawn_driver, driver}` with the `[binary]` stream option — the shape every `inet`
    /// control port is created with (`prim_inet:open/4`).
    fn open(&mut self, driver: &str) -> PortId {
        let name = self.fx.string(driver);
        let name = self.fx.name("spawn_driver", name);
        let binary = self.fx.atom("binary");
        let options = self.fx.list(&[binary]);
        self.hook
            .open_port(name, options)
            .unwrap_or_else(|error| panic!("open_port({{spawn_driver,{driver:?}}}) failed: {error}"))
    }

    /// Drives one probe through `erlang:port_control/3` on the **native floor** — the same entry a
    /// compiled `prim_inet` call reaches — so the `int_list` rendering and the `badarg` path are
    /// exercised exactly as an Erlang caller sees them.
    fn control(&mut self, port: PortId, probe: &Probe) -> Reply {
        let data = terms::make_heap_binary(
            &mut crate::context::RuntimeContext::new(&mut self.runtime),
            &probe.arg,
        )
        .expect("the operand binary");
        let reply = crate::native::test_support::call(
            &mut self.runtime,
            "erlang",
            "port_control",
            &[port.to_eterm(), make_small(probe.op as i64), data],
        );
        match reply {
            Ok(term) => Reply::Bytes(reply_bytes(term)),
            Err(BeamError::BadArg) => Reply::BadArg,
            // A capability gap is no verdict of this matrix: it is rendered loudly so a comparison
            // fails with the real cause in the message.
            Err(other) => Reply::Gap(other.to_string()),
        }
    }
}

/// The bytes of a `port_control/3` reply term: an int list (`buf_to_intlist`), or a binary when a
/// driver asked for `PORT_CONTROL_FLAG_BINARY`.
fn reply_bytes(term: Eterm) -> Vec<u8> {
    if let Ok(bytes) = terms::binary_bytes(term) {
        return bytes.to_vec();
    }
    terms::list_elements(term)
        .expect("a control reply is a byte list")
        .into_iter()
        .map(|element| u8::try_from(signed_value(element)).expect("a reply byte"))
        .collect()
}

/// The oracle's labelled verdicts for `probes`, in order, or `None` when no usable `erl` exists.
///
/// One `io:format/2` per probe prints the label and the `~w` rendering of the reply, so a line can
/// only come from the shape it names. The `try … catch error:badarg -> badarg` wrapper is what
/// makes a raise print as the bare atom, i.e. the same text [`Reply::render`] produces.
fn oracle_lines(driver: &str, probes: &[Probe]) -> Option<Vec<String>> {
    let mut body = String::new();
    for probe in probes {
        body.push_str(&format!(
            "io:format(\"{} ~w~n\",[F(P,{},{})]),",
            probe.label,
            probe.op,
            binary_literal(&probe.arg)
        ));
    }
    let expr = format!(
        "begin F = fun(P,V,D) -> try erlang:port_control(P,V,D) of R -> R \
         catch error:badarg -> badarg end end, \
         P = open_port({{spawn_driver,\"{driver}\"}},[binary]), {body} erlang:port_close(P) end"
    );
    let dump = erl_eval(&expr)?;
    Some(
        dump.lines()
            .map(|line| {
                let (_, verdict) = line
                    .split_once(' ')
                    .unwrap_or_else(|| panic!("an oracle line without a verdict: {line:?}"));
                verdict.to_string()
            })
            .collect(),
    )
}

/// Asserts the oracle's reply and this runtime's agree under `probe.rule`, naming the shape.
fn assert_agrees(what: &str, probe: &Probe, ours: &Reply, oracle: &str) {
    let label = format!(
        "{what}: probe {:?} (op {}, operand {:?})",
        probe.label, probe.op, probe.arg
    );
    match probe.rule {
        Rule::Exact => assert_eq!(
            ours.render(),
            oracle,
            "{label}: the whole rendered reply must match the oracle"
        ),
        Rule::BoundPort => {
            let (ours, theirs) = (ours.bytes(&label), ints_of(oracle));
            assert_eq!(theirs.len(), 3, "{label}: the oracle's bind reply is [1,hi,lo]");
            assert_eq!(ours.len(), 3, "{label}: this runtime's bind reply is [1,hi,lo]");
            assert_eq!(ours[0], theirs[0], "{label}: the result tag");
            assert_eq!(ours[0], 1, "{label}: a successful bind answers the OK tag");
            assert_ne!(
                u16::from_be_bytes([theirs[1], theirs[2]]),
                0,
                "{label}: the oracle's socket has a port"
            );
            assert_ne!(
                u16::from_be_bytes([ours[1], ours[2]]),
                0,
                "{label}: the kernel assigned this runtime a port"
            );
        }
        Rule::PackedAddress => {
            let (ours, theirs) = (ours.bytes(&label), ints_of(oracle));
            assert_eq!(
                theirs.len(),
                8,
                "{label}: the oracle's [1,family,hi,lo,a,b,c,d] address"
            );
            assert_eq!(
                ours.len(),
                8,
                "{label}: this runtime's [1,family,hi,lo,a,b,c,d] address"
            );
            assert_eq!(ours[0], theirs[0], "{label}: the result tag");
            assert_eq!(ours[1], theirs[1], "{label}: the address family");
            assert_ne!(
                u16::from_be_bytes([ours[2], ours[3]]),
                0,
                "{label}: a bound socket has a non-zero port"
            );
            assert_eq!(&ours[4..8], &theirs[4..8], "{label}: the address octets");
        }
        Rule::ErrorReply => {
            let (ours, theirs) = (ours.bytes(&label), ints_of(oracle));
            assert_eq!(
                theirs.first(),
                Some(&0),
                "{label}: the oracle's ctl_error reply carries the INET_REP_ERROR tag"
            );
            assert_eq!(
                ours.first(),
                Some(&0),
                "{label}: this runtime's ctl_error reply carries the INET_REP_ERROR tag"
            );
            for (side, reply) in [("the oracle's", &theirs), ("this runtime's", &ours)] {
                let name = reply.get(1..).unwrap_or_default();
                assert!(
                    !name.is_empty(),
                    "{label}: {side} error reply names an errno"
                );
                assert_eq!(
                    name[0],
                    b'e',
                    "{label}: {side} errno name ({name:?}) starts with `e`"
                );
                assert!(
                    name.iter().all(|byte| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || *byte == b'_'),
                    "{label}: {side} errno name ({name:?}) is the C's lowercase spelling"
                );
            }
            // The name itself is the *descriptor's* errno — `ebadf` where the number is free,
            // `enotsock` where it is some other open file — so it is deliberately not compared.
        }
        Rule::Prefix(prefix) => {
            assert!(
                ours.render().starts_with(prefix),
                "{label}: this runtime's reply must start with {prefix:?}, got {:?}",
                ours.render()
            );
            assert!(
                oracle.starts_with(prefix),
                "{label}: the oracle's reply must start with {prefix:?}, got {oracle:?}"
            );
        }    }
}

/// Runs one family's whole `probes` sequence on both sides and asserts every reply agrees.
///
/// A missing `erl` skips **loudly** (the runtime half still runs, so the sequence itself — and
/// every typed refusal in it — is exercised without the oracle).
fn run_family(what: &str, driver: &str, probes: &[Probe]) {
    let mut session = Session::new();
    let port = session.open(driver);
    let ours: Vec<Reply> = probes.iter().map(|probe| session.control(port, probe)).collect();
    match oracle_lines(driver, probes) {
        Some(oracle) => {
            assert_eq!(
                oracle.len(),
                probes.len(),
                "{what}: the oracle answered every probe"
            );
            for ((probe, ours), oracle) in probes.iter().zip(&ours).zip(&oracle) {
                assert_agrees(what, probe, ours, oracle);
            }
        }
        None => {
            skip_oracle_check(what);
            for probe in probes {
                assert!(
                    !probe.label.is_empty(),
                    "the runtime half of {what} ran every probe"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The `udp_inet` families
// ---------------------------------------------------------------------------

/// The C's error reply for a request the driver's own `case` does not match: `[0] ++ "exbadport"`.
const EXBADPORT: &[u8] = &[0, 101, 120, 98, 97, 100, 112, 111, 114, 116];

/// The bare (socket-less) `udp_inet` port: the shared queries, the C's own state/length refusals,
/// the fake addresses `SETNAME`/`SETPEER` install, then the wrapped opcodes. Nothing here creates a
/// socket, so every answer is a pure function of the operand and the driver's state.
fn bare_datagram() -> Vec<Probe> {
    let fake = vec![1u8, 4, 210, 1, 2, 3, 4];
    vec![
        Probe::exact("gettype_bare", 15, vec![]),
        Probe::exact("getfd_bare", 14, vec![]),
        Probe::exact("getstatus_bare", 16, vec![]),
        Probe::exact("name_bare", 5, vec![]),
        Probe::exact("peer_bare", 4, vec![]),
        Probe::exact("recv8_bare", 60, vec![0; 8]),
        Probe::exact("recv0_bare", 60, vec![]),
        Probe::exact("tcp_recv42_bare", 42, vec![]),
        Probe::exact("accept26_bare", 26, vec![]),
        Probe::exact("listen27_bare", 27, vec![]),
        Probe::exact("listen27_05_bare", 27, vec![0, 5]),
        Probe::exact("bind0_bare", 6, vec![]),
        Probe::exact("bind_any_bare", 6, vec![3, 0, 0]),
        Probe::exact("connect_short_bare", 3, vec![0, 0]),
        Probe::exact("connect_empty_bare", 3, vec![]),
        Probe::exact("open_short_bare", 1, vec![1, 2]),
        Probe::exact("fdopen0_bare", 13, vec![]),
        Probe::exact("fdopen_afnosupport_bare", 13, vec![9, 2, 0, 0, 0, 1]),
        Probe::exact("fdopen_type_bare", 13, vec![1, 9, 0, 0, 0, 1]),
        Probe::exact("gethostname_bare", 12, vec![]),
        Probe::exact("gethostname_1_bare", 12, vec![1]),
        Probe::exact("getservbyname_bare", 17, vec![]),
        Probe::exact("getservbyport_bare", 18, vec![]),
        Probe::exact("setname_clear_bare", 19, vec![]),
        Probe::exact("setpeer_clear_bare", 20, vec![]),
        Probe::exact("setname_1_bare", 19, vec![1]),
        Probe::exact("setpeer_1_bare", 20, vec![1]),
        Probe::exact("setname_eafnosupport_bare", 19, vec![9, 1, 2]),
        Probe::exact("getiflist_bare", 21, vec![]),
        Probe::exact("ifget_bare", 22, vec![1]),
        Probe::exact("ifset_bare", 23, vec![1]),
        Probe::exact("getifaddrs_bare", 25, vec![]),
        Probe::exact("ignorefd_bare", 28, vec![1]),
        Probe::exact("getladdrs_bare", 29, vec![0, 0, 0, 0]),
        Probe::exact("getladdrs_0_bare", 29, vec![]),
        Probe::exact("getpaddrs_bare", 30, vec![0, 0, 0, 0]),
        Probe::exact("getpaddrs_0_bare", 30, vec![]),
        Probe::exact("setname_fake_bare", 19, fake),
        Probe::exact("name_fake_bare", 5, vec![]),
        Probe::exact("setname_any_bare", 19, vec![3, 4, 210]),
        Probe::exact("setname_loopback_bare", 19, vec![4, 4, 210]),
        Probe::exact("setname_clear2_bare", 19, vec![]),
        Probe::exact("name_after_clear_bare", 5, vec![]),
    ]
}

/// The datagram lifecycle in one port: `open`, the fake local address, the real wildcard/bound
/// address, the queries that need a socket, the option store on a bound socket, and the refusals
/// whose guard order the C fixes (`bind` checks the operand length before the socket state,
/// `connect` and `PACKET_REQ_RECV` the other way round).
fn datagram_lifecycle() -> Vec<Probe> {
    vec![
        Probe::exact("open", 1, vec![1, 2, 0]),
        Probe::exact("setname_any", 19, vec![3, 4, 210]),
        Probe::exact("name_any", 5, vec![]),
        Probe::exact("setname_loopback", 19, vec![4, 4, 210]),
        Probe::exact("name_loopback", 5, vec![]),
        Probe::exact("setname_clear", 19, vec![]),
        Probe::exact("name_wildcard", 5, vec![]),
        Probe::exact("gettype_open", 15, vec![]),
        Probe::exact("getstatus_open", 16, vec![]),
        Probe::exact("listen_open", 27, vec![]),
        Probe::exact("listen_open_05", 27, vec![0, 5]),
        Probe::exact("getladdrs_open", 29, vec![0, 0, 0, 0]),
        Probe::exact("getpaddrs_open", 30, vec![0, 0, 0, 0]),
        Probe::exact("ignorefd_open", 28, vec![1]),
        Probe::exact("recv_2_open", 60, vec![0, 0]),
        Probe::exact("connect_2_open", 3, vec![0, 0]),
        Probe::exact("open_short_open", 1, vec![1, 2]),
        Probe::req("bind", 6, vec![4, 0, 0], Rule::BoundPort),
        Probe::req("name_bound", 5, vec![], Rule::PackedAddress),
        Probe::req("getladdrs_bound", 29, vec![0, 0, 0, 0], Rule::PackedAddress),
        Probe::exact("getpaddrs_bound", 30, vec![0, 0, 0, 0]),
        Probe::exact("setopts_broadcast", 7, vec![4, 0, 0, 0, 1]),
        Probe::exact("getopts_broadcast", 8, vec![4]),
        Probe::exact("getstat_bound", 11, vec![]),
        Probe::exact("peer_bound", 4, vec![]),
        Probe::exact("recv_4_bound", 60, vec![0, 0, 0, 0]),
    ]
}

/// The **asynchronous receive slot** on a fresh datagram socket: `PACKET_REQ_RECV`(60) with the
/// C's full eight-byte `Timeout(4), Length(4)` operand joins the driver's async queue and answers
/// `[1] ++ <<Id:16/big>>` (`inet_drv.c:15001`-`:15010`), the first operation on a fresh port taking
/// id `0`; the datagram itself is delivered later as an `{inet_async, …}` message to the port's
/// owner, which is `port_control/3`'s business only through that reply.
///
/// This row used to be the table's first residual — this floor read the socket *now* and answered
/// the bare `[1]`, because no driver→owner path existed to carry the operation's result. The
/// async queue and the completion channel removed that limitation, so the row is pinned against
/// the oracle like every other agreement row.
///
/// Only the **first** operation is probed. A second one in the same session is not comparable: the
/// oracle frees the slot as soon as the zero-timeout operation completes with `{error, timeout}`
/// (between two `port_control/3` calls), while this runtime's driver advances at its pump points,
/// so the two sides would be reading different queue states rather than different op ids.
fn datagram_async_receive() -> Vec<Probe> {
    vec![
        Probe::exact("open_r", 1, vec![1, 2, 0]),
        Probe::exact("recv_wellformed", 60, vec![0; 8]),
    ]
}

/// The connected half: a real datagram `connect(2)` installs a default peer (no listener needed and
/// nothing is sent), `SETPEER` overlays a *fake* peer that only `PEER` sees while `GETPADDRS` keeps
/// reporting the real one, and the close leaves the driver's cached socket **kind** behind.
fn connected_datagram() -> Vec<Probe> {
    let fake = vec![1u8, 4, 210, 1, 2, 3, 4];
    vec![
        Probe::exact("open_c", 1, vec![1, 2, 0]),
        Probe::req("bind_c", 6, vec![4, 0, 0], Rule::BoundPort),
        Probe::exact("connect_c", 3, vec![0, 0, 0, 0, 1, 0, 9, 127, 0, 0, 1]),
        Probe::exact("getstatus_c", 16, vec![]),
        Probe::exact("peer_c", 4, vec![]),
        Probe::exact("getpaddrs_c", 30, vec![0, 0, 0, 0]),
        Probe::exact("ignorefd_c", 28, vec![1]),
        Probe::exact("setpeer_fake_c", 20, fake),
        Probe::exact("peer_fake_c", 4, vec![]),
        Probe::exact("getpaddrs_fake_c", 30, vec![0, 0, 0, 0]),
        Probe::exact("setpeer_clear_c", 20, vec![]),
        Probe::exact("peer_real_c", 4, vec![]),
        Probe::exact("connect_dissolve_c", 3, vec![]),
        Probe::exact("getstatus_dissolved_c", 16, vec![]),
        Probe::exact("close_c", 2, vec![]),
        Probe::exact("gettype_closed_c", 15, vec![]),
        Probe::exact("getstatus_closed_c", 16, vec![]),
        Probe::exact("getfd_closed_c", 14, vec![]),
        Probe::exact("name_closed_c", 5, vec![]),
        Probe::exact("close_again_c", 2, vec![]),
    ]
}

/// The **scope** rules on a live datagram socket: the stream-only switches belong to
/// `tcp_inet_ctl` alone, so on a datagram port the C's `inet_ctl` default arm answers
/// `[0]exbadport` — and so does a command below the magic number (whose `unsigned` subtraction
/// wraps past every `case`) and an unassigned request number.
fn opcode_scope() -> Vec<Probe> {
    vec![
        Probe::exact("open_s", 1, vec![1, 2, 0]),
        Probe::exact("tcp_recv42", 42, vec![]),
        Probe::exact("tcp_unrecv43", 43, vec![]),
        Probe::exact("tcp_shutdown44", 44, vec![]),
        Probe::exact("tcp_sendfile45", 45, vec![]),
        Probe::exact("accept26", 26, vec![]),
        Probe::raw("wrapped_cmd0", 0, vec![1, 2, 0]),
        Probe::raw("wrapped_cmd99", 99, vec![]),
        Probe::exact("unassigned_9", 9, vec![]),
    ]
}

/// The option store of an open datagram socket: the driver-state read-backs, the syscall-backed
/// codes, the aggregate/write-only codes, a multi-code request, an unserved code, and the
/// `setopts`/`getopts` round trip for every served code.
///
/// `nodelay` (code `10`) is pinned in [`datagram_shared_arms`] instead, where the C's
/// truncate-and-continue rule for a `getsockopt` the socket refuses is the shape under test.
fn datagram_options() -> Vec<Probe> {
    let mut probes = vec![Probe::exact("open_o", 1, vec![1, 2, 0])];
    for code in [0u8, 1, 4, 6, 7, 22, 23, 24, 25, 30, 32, 33, 35, 40, 46, 200] {
        probes.push(Probe::exact("getopts", 8, vec![code]));
    }
    probes.push(Probe::exact("getopts_multi", 8, vec![0, 4]));
    probes.push(Probe::exact("getopts_40_4", 8, vec![40, 4]));
    for (code, value) in [
        (0u8, 1i32),
        (4, 1),
        (22, 0),
        (23, 2),
        (32, 8),
        (24, 0),
        (25, 1),
        (33, 5),
    ] {
        let mut arg = vec![code];
        arg.extend_from_slice(&value.to_be_bytes());
        probes.push(Probe::exact("setopts", 7, arg));
        probes.push(Probe::exact("getopts_after", 8, vec![code]));
    }
    probes.push(Probe::exact("setopts_unknown", 7, vec![200, 0, 0, 0, 1, 0]));
    probes
}

/// The **shared `inet_ctl` arms** on an open datagram socket: the `INET_REQ_SUBSCRIBE`
/// registration reply, the `INET_REQ_FDOPEN` ladder's pre-adoption check, the name-service lookups,
/// and `getopts`'s truncate-and-continue rule for a `getsockopt` a datagram socket refuses.
///
/// `subscribe`'s reply is `[1] ++ (op ++ be32(queued output bytes))*`. What this driver floor cannot
/// honour is the *subscription* itself — no driver→owner channel exists, so no later
/// `{empty_out_q, Port}` message is ever sent — but the **reply** is exact, so all three rows are
/// ordinary agreement rows. `subscribe_2`'s `2` operand is the C's `return -1`, which the oracle
/// surfaces as the bare atom `badarg`.
///
/// `fdopen_badfd` hands an out-of-range descriptor number to the pre-adoption `sock_name(2)` check,
/// which is `EBADF` in every process — so it is exact. `fdopen_wellformed` names a descriptor both
/// sides hold and neither can socket-ise — standard output, which [`erl_eval`] pipes for the oracle
/// and the test harness captures for this runtime — and is compared by shape because the C answers
/// with *its* `sock_errno()`. (The **adoption** form of the same request, on a port that already
/// holds a socket, is a documented residual: see
/// [`the_documented_datagram_residuals_stay_typed`].)
///
/// The name-service rows are the same shared arms over the host's services database: the two
/// `*_wellformed` operands fail the C's operand arithmetic (`einval`) on both sides, and the
/// `domain`/`udp` pair exercises a real lookup in both directions (port → name and name → port).
///
/// `nodelay` (code `10`) is an option a datagram socket refuses: the C drops **that record** and
/// keeps the records it already produced, so `getopts_nodelay` answers the bare OK tag and the
/// `reuseaddr`-then-`nodelay` row keeps the `reuseaddr` record (`inet_drv.c:9734-9741`).
fn datagram_shared_arms() -> Vec<Probe> {
    let mut domain_name = vec![6u8];
    domain_name.extend_from_slice(b"domain");
    domain_name.push(3);
    domain_name.extend_from_slice(b"udp");
    let mut domain_port = vec![0u8, 53, 3];
    domain_port.extend_from_slice(b"udp");
    vec![
        Probe::exact("open_a", 1, vec![1, 2, 0]),
        Probe::exact("subscribe_empty", 24, vec![]),
        Probe::exact("subscribe_1", 24, vec![1]),
        Probe::exact("subscribe_2", 24, vec![2]),
        Probe::error_reply("fdopen_wellformed", 13, vec![1, 2, 0, 0, 0, 1]),
        Probe::exact("fdopen_badfd", 13, vec![1, 2, 0x7f, 0xff, 0xff, 0xff]),
        Probe::exact("getservbyname_wellformed", 17, vec![3, 100]),
        Probe::exact("getservbyport_wellformed", 18, vec![0, 53, 3]),
        Probe::exact("getservbyname_domain", 17, domain_name),
        Probe::exact("getservbyport_domain", 18, domain_port),
        Probe::exact("getopts_nodelay", 8, vec![10]),
        Probe::exact("getopts_reuseaddr_nodelay", 8, vec![0, 10]),
    ]
}
/// The **interface requests** on an open datagram socket — `GETIFLIST`(21), `IFGET`(22),
/// `IFSET`(23) and `GETIFADDRS`(25), the four shared `inet_ctl` arms that walk the host's
/// interface table.
///
/// `getiflist`/`getifaddrs` report **this machine's** interfaces, so they are compared by their
/// stable prefix (`[1,` — the OK tag followed by at least one interface) rather than by a literal
/// host list; the two sides still run against the same kernel, so a driver that answered a refusal
/// here would fail. The `IFGET`/`IFSET` rows use degenerate or **non-existent** interface operands,
/// whose answers are host-independent: `ifget`'s one-byte operand is the C's name-length guard
/// (`einval`), `ifget_nonexist` walks a name no host has (the `FLAGS` ioctl fails ⇒ the record
/// reports `0`, `inet_drv.c:5955`), `ifset`'s one-byte operand parses to no option at all, and
/// `ifset_lo` parses one `MTU` option and answers the bare OK tag — an `IFSET` only *parses* (the
/// writes are `(void)`-discarded in the C and not performed here).
fn datagram_interfaces() -> Vec<Probe> {
    let mut ifget_nonexist = vec![9u8];
    ifget_nonexist.extend_from_slice(b"nonexist0");
    ifget_nonexist.push(6); // INET_IFOPT_FLAGS
    vec![
        Probe::exact("open_i", 1, vec![1, 2, 0]),
        Probe::prefix("getiflist", 21, vec![], "[1,"),
        Probe::exact("ifget", 22, vec![1]),
        Probe::exact("ifset", 23, vec![1]),
        Probe::exact("ifget_nonexist", 22, ifget_nonexist),
        Probe::exact("ifset_lo", 23, vec![2, b'l', b'o', 4, 0, 0, 0, 0]),
        Probe::prefix("getifaddrs", 25, vec![], "[1,"),
    ]
}
/// The `tcp_inet` side of the same bridge, on the bare (socket-less) port: the shared queries, the
/// stream-only requests that need a connection, the datagram-only codes, and the retired numbers.
fn bare_stream() -> Vec<Probe> {
    vec![
        Probe::exact("tcp_bare_42", 42, vec![1, 2]),
        Probe::exact("tcp_bare_42_empty", 42, vec![]),
        Probe::exact("tcp_bare_15", 15, vec![]),
        Probe::exact("tcp_bare_16", 16, vec![]),
        Probe::exact("tcp_bare_60", 60, vec![0; 8]),
        Probe::exact("tcp_bare_27", 27, vec![0, 5]),
        Probe::exact("tcp_bare_99", 99, vec![]),
    ]
}

/// The **listening stream with its accept still queued** — the state the stream-only requests are
/// reached in, and the replacement for the pre-async `INET_STATE_CONNECTED` row.
///
/// `INET_REQ_ACCEPT`(26) joins the driver's multi-operation accept list (`enq_multi_op`,
/// `inet_drv.c:12194`) and answers `[1] ++ <<Id:16/big>>`; the connection it eventually takes is
/// delivered to the port's **owner** as `{inet_async, Port, Id, {ok, Descriptor}}`, never installed
/// as this port's own descriptor. The port therefore stays a listener: `INET_REQ_GETSTATUS`(16)
/// reports the C's `INET_STATE_ACCEPTING` — `INET_STATE_LISTENING`(`9`) `|` `INET_F_ACC`(`0x20`),
/// `inet_drv.c:1031`, read out at `:11160` — `[1,0,0,0,41]` — and the two stream-only requests
/// probed on it (`INET_REQ_IGNOREFD`(28), `TCP_REQ_SENDFILE`(45)) answer the C's `[0]enotconn`,
/// their `IS_CONNECTED` guard (`:11461`, `:12386`).
///
/// The accept's operand is a **two-second** timeout (`<<0,0,7,208>>`) rather than the C's "do not
/// wait" zero: a zero makes the oracle's operation expire between two `port_control/3` calls and
/// report the state *after* the queue drained (`[1,0,0,0,9]`), which is a different row. Two
/// seconds cannot expire inside this session, so both sides read the queue while it is non-empty.
///
/// This sequence used to be pinned on this runtime's side alone, as
/// `INET_STATE_CONNECTED`(`[1,0,0,0,5]`) plus two typed capability gaps: the pre-async floor
/// *substituted* a synchronous accept that took the connection here, so `ignorefd`/`sendfile`
/// reached their "connected stream" arms. No `port_control/3`-only session can reach a connected
/// stream on either runtime — a peer pending when the driver's round runs completes the C's accept
/// but still leaves the port listening (`[1,0,0,0,9]`, measured) — so the connected-stream
/// capability gaps of those two requests belong to `beam_io`, whose driver-seam suite pins them
/// (`drivers/inet/opcode_tests.rs`, the `not_served` rows).
fn queued_accept_stream() -> Vec<Probe> {
    let mut sendfile = vec![0x7fu8, 0xff, 0xff, 0xff]; // an out-of-range file descriptor
    sendfile.extend_from_slice(&0u64.to_be_bytes()); // offset
    sendfile.extend_from_slice(&0u64.to_be_bytes()); // count
    vec![
        Probe::exact("open_l", 1, vec![1, 1, 0]),
        Probe::req("bind_l", 6, vec![4, 0, 0], Rule::BoundPort),
        Probe::exact("listen_l", 27, vec![0, 5]),
        Probe::exact("accept_queued", 26, vec![0, 0, 7, 208]),
        Probe::exact("getstatus_queued", 16, vec![]),
        Probe::exact("ignorefd_queued", 28, vec![1]),
        Probe::exact("tcp_sendfile_queued", 45, sendfile),
    ]
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// The **headline** shapes, pinned with no oracle: this is the acceptance of the bridge's three
/// links (the capability gate, the decimal rendering, the `int_list` reply) in one place, so a
/// regression that needs no `erl` to see still fails.
#[test]
fn the_headline_numeric_shapes_answer_without_an_oracle() {
    let mut session = Session::new();
    let udp = session.open("udp_inet");

    // `Open` on a socket-less datagram port — the request the whole bridge exists for.
    assert_eq!(
        session.control(udp, &Probe::exact("open", 1, vec![1, 2, 0])).render(),
        "[1]",
        "INET_REQ_OPEN creates the socket"
    );
    // `Setopts` with no records, then `Getopts` of `broadcast` and `Gettype`.
    assert_eq!(
        session.control(udp, &Probe::exact("setopts", 7, vec![])).render(),
        "[1]"
    );
    assert_eq!(
        session.control(udp, &Probe::exact("setopts_broadcast", 7, vec![4, 0, 0, 0, 1])).render(),
        "[1]"
    );
    assert_eq!(
        session.control(udp, &Probe::exact("getopts_broadcast", 8, vec![4])).render(),
        "[1,4,0,0,0,1]",
        "GETOPTS answers [1] ++ [opt, val_be32]"
    );
    assert_eq!(
        session.control(udp, &Probe::exact("gettype", 15, vec![])).render(),
        "[1,0,0,0,1,0,0,0,2]",
        "GETTYPE reports SOCK_DGRAM (1) and IPPROTO_UDP (2)"
    );
    // A retired/SCTP-only request is the driver's own refusal, never a fabricated reply.
    assert_eq!(
        session.control(udp, &Probe::exact("listen", 27, vec![])).render(),
        render_bytes(EXBADPORT)
    );
    // `PACKET_REQ_RECV` allocates the queued operation's id — the row this suite used to record as
    // the synchronous read-now substitution (the bare `[1]`, with no async queue to answer for).
    assert_eq!(
        session.control(udp, &Probe::exact("recv", 60, vec![0; 8])).render(),
        "[1,0,0]",
        "PACKET_REQ_RECV answers `[1] ++ <<Id:16/big>>`"
    );

    // A bare (magic-less) operation never reaches `INET_REQ_OPEN`: the C's unsigned subtraction
    // wraps past every `case`, so the answer is the driver's `exbadport`.
    for code in [1u64, 8, 15, 24, 99] {
        assert_eq!(
            session.control(udp, &Probe::raw("bare", code, vec![])).render(),
            render_bytes(EXBADPORT),
            "a bare operation ({code}) is not an inet request"
        );
    }

    let tcp = session.open("tcp_inet");
    assert_eq!(
        session.control(tcp, &Probe::exact("recv42", 42, vec![1, 2])).render(),
        render_bytes(&[0, 101, 110, 111, 116, 99, 111, 110, 110]),
        "TCP_REQ_RECV on a socket-less stream port is enotconn"
    );
    assert_eq!(
        session.control(tcp, &Probe::exact("getstatus", 16, vec![])).render(),
        "[1,0,0,0,0]"
    );
    // A queued `INET_REQ_ACCEPT` on a listening stream: the operation gets an id and the port stays
    // a **listener** (`INET_STATE_ACCEPTING`) — the state this suite used to read as
    // `INET_STATE_CONNECTED` through the synchronous-accept substitution.
    assert_eq!(
        session.control(tcp, &Probe::exact("open", 1, vec![1, 1, 0])).render(),
        "[1]"
    );
    let bound = session.control(tcp, &Probe::req("bind", 6, vec![4, 0, 0], Rule::BoundPort));
    assert_eq!(
        bound.bytes("bind").len(),
        3,
        "a bound stream reports `[1] ++ port_hi ++ port_lo`"
    );
    assert_eq!(
        session.control(tcp, &Probe::exact("listen", 27, vec![0, 5])).render(),
        "[1]"
    );
    assert_eq!(
        session.control(tcp, &Probe::exact("accept", 26, vec![0, 0, 7, 208])).render(),
        "[1,0,0]",
        "INET_REQ_ACCEPT answers its async op id instead of taking the connection"
    );
    assert_eq!(
        session.control(tcp, &Probe::exact("getstatus", 16, vec![])).render(),
        "[1,0,0,0,41]",
        "a listener with one queued accept reports INET_STATE_ACCEPTING, never INET_STATE_CONNECTED"
    );
}

/// The bare (socket-less) datagram port, whole-session against the oracle.
#[test]
fn the_bare_datagram_port_refusals_match_the_oracle() {
    run_family("the bare `udp_inet` port", "udp_inet", &bare_datagram());
}

/// The datagram lifecycle, including the ephemeral-port rows compared by shape.
#[test]
fn the_datagram_lifecycle_and_address_shapes_match_the_oracle() {
    run_family("the `udp_inet` lifecycle", "udp_inet", &datagram_lifecycle());
}

/// The asynchronous receive slot: `PACKET_REQ_RECV`'s op id, which both runtimes allocate now.
#[test]
fn the_datagram_async_receive_slot_matches_the_oracle() {
    run_family(
        "the `udp_inet` async receive slot",
        "udp_inet",
        &datagram_async_receive(),
    );
}
/// The connected/peer half and the post-close cached-kind residual.
#[test]
fn the_connected_datagram_half_matches_the_oracle() {
    run_family("the connected `udp_inet` half", "udp_inet", &connected_datagram());
}

/// The opcode-scope rules on a live datagram socket.
#[test]
fn the_opcode_scope_rules_match_the_oracle() {
    run_family("the `udp_inet` opcode scope", "udp_inet", &opcode_scope());
}

/// The datagram option store.
#[test]
fn the_datagram_option_store_matches_the_oracle() {
    run_family("the `udp_inet` option store", "udp_inet", &datagram_options());
}

/// The shared `inet_ctl` arms: `subscribe`, `fdopen`'s pre-adoption check, the name service and
/// `getopts`'s truncate-and-continue rule.
#[test]
fn the_shared_inet_ctl_arms_match_the_oracle() {
    run_family(
        "the shared `udp_inet` control arms",
        "udp_inet",
        &datagram_shared_arms(),
    );
}

/// The four interface requests, with the host-dependent address lists compared by prefix.
#[test]
fn the_interface_requests_match_the_oracle() {
    run_family(
        "the `udp_inet` interface requests",
        "udp_inet",
        &datagram_interfaces(),
    );
}
/// The `tcp_inet` bare-port refusals of the same bridge.
#[test]
fn the_bare_stream_port_refusals_match_the_oracle() {
    run_family("the bare `tcp_inet` port", "tcp_inet", &bare_stream());
}

/// The listening stream with a queued accept: the op id the accept allocates, the state bits the
/// queue reports, and the two stream-only requests probed in that state.
#[test]
fn the_queued_accept_stream_matches_the_oracle() {
    run_family(
        "the `tcp_inet` listening stream with a queued accept",
        "tcp_inet",
        &queued_accept_stream(),
    );
}
// ---------------------------------------------------------------------------
// The documented residuals
// ---------------------------------------------------------------------------

/// One documented divergence: the probe, the oracle's answer and this runtime's honest one.
struct Residual {
    /// The probe itself.
    probe: Probe,
    /// The exact `~w` rendering of the reply OTP 29.1.1/erts-17.1 returns.
    oracle: &'static str,
    /// The exact reply bytes this runtime returns — the documented substitution for that shape.
    ours: &'static [u8],
    /// The residual this row records.
    residual: &'static str,
}
/// One residual **session**: the driver, the setup probes that put the port into the state its rows
/// need, and the rows probed on it.
///
/// Only the rows are divergences. The setup must **agree** on both sides — it is compared exactly
/// the way an agreement family's probes are ([`assert_agrees`]), so a setup `bind`'s ephemeral port
/// is still compared by shape.
struct ResidualSession {
    /// The session, as it appears in a failure message.
    what: &'static str,
    /// The driver the port is opened as `{spawn_driver, Driver}`.
    driver: &'static str,
    /// The probes that establish the state the rows are probed in.
    setup: Vec<Probe>,
    /// The documented divergences.
    rows: Vec<Residual>,
}

/// The `udp` shapes this runtime answers differently, each a **capability gap or a documented
/// substitution** on the driver side (`beam_io`), never a wrong result.
///
/// Both rows are the floor's own, and neither is a `beam_io` request that is merely unimplemented:
///
/// * `open` of a **non-`AF_INET`** family: this floor's sockets are `AF_INET` only, so an
///   `AF_INET6` open is the driver's own `[0]eafnosupport` refusal, where the C creates the socket
///   and answers `[1]`. It is probed on a **bare** port: on the C the successful open leaves a
///   socket behind, so a following `open` there is `exbadseq`, not a second `[1]`.
/// * `INET_REQ_FDOPEN`(`13`) on a port that **already holds a socket**: the C overwrites the
///   descriptor it is using with the operand's (`inet_drv.c:5251`, then `desc->prebound = 1`), so
///   the request answers `[1]`, the caller's number becomes the port's and the live socket is
///   leaked; this floor keeps the live socket and refuses with `[0]einval`. The operand's `bound`
///   word is **zero** — the ten-byte form's un-bound half — because that is what makes the row
///   pinnable: with `bound` set the C runs its pre-adoption `sock_name(2)` check first, whose answer
///   is the *probe process'* descriptor table (`[0]ebadf`/`[0]enotsock`/`[1]` depending on whatever
///   that number happens to name), which no oracle column can pin. The descriptor word is `65535`,
///   the largest number the emulator admits (so the C's own `driver_select` stays silent on it) and
///   one no process running this test holds, so the adoption is side-effect free on both sides.
///
/// `PACKET_REQ_RECV`'s op id was the table's third row until the async-op queue landed; it is an
/// agreement row now ([`datagram_async_receive`]).
fn residual_sessions() -> Vec<ResidualSession> {
    vec![
        ResidualSession {
            what: "the bare `udp_inet` port",
            driver: "udp_inet",
            setup: vec![],
            rows: vec![Residual {
                probe: Probe::exact("open_inet6", 1, vec![2, 2, 0]),
                oracle: "[1]",
                ours: b"\x00eafnosupport",
                residual: "`INET_REQ_OPEN` of an `AF_INET6` socket is this floor's `eafnosupport` \
                           refusal; the C creates it and answers `[1]`",
            }],
        },
        ResidualSession {
            what: "the `udp_inet` port that already holds a socket",
            driver: "udp_inet",
            // The precondition itself: `INET_REQ_OPEN` really gives the port a descriptor (two
            // sockets never get the same number, so only its shape is comparable).
            setup: vec![
                Probe::exact("open", 1, vec![1, 2, 0]),
                Probe::prefix("getfd_open", 14, vec![], "[1,0,0,0,"),
            ],
            rows: vec![Residual {
                probe: Probe::exact(
                    "fdopen_already_open",
                    13,
                    vec![1, 2, 0, 0, 255, 255, 0, 0, 0, 0],
                ),
                oracle: "[1]",
                ours: b"\x00einval",
                residual: "`INET_REQ_FDOPEN` on a port that already holds a socket: the C adopts \
                           the operand's descriptor and leaks the live socket, this floor keeps the \
                           live socket and refuses with `einval`",
            }],
        },
    ]
}
/// Every documented `udp` residual, asserted on **both** sides.
///
/// The runtime side holds with no toolchain; the oracle side pins the exact output of OTP
/// 29.1.1/erts-17.1 so the record cannot rot. Nothing here is a relaxed assertion: a row is the
/// **exact bytes** of a documented substitution, and the setup sequence around it must agree with
/// the oracle probe for probe.
#[test]
fn the_documented_datagram_residuals_stay_typed() {
    for session in residual_sessions() {
        run_residual_session(&session);
    }
}
/// Runs one residual session: the setup probes are compared with the oracle as an agreement family
/// is, then every row is asserted to be what the table says — on both sides.
fn run_residual_session(session: &ResidualSession) {
    let mut runtime_session = Session::new();
    let port = runtime_session.open(session.driver);
    let probes: Vec<Probe> = session
        .setup
        .iter()
        .chain(session.rows.iter().map(|row| &row.probe))
        .cloned()
        .collect();
    let ours: Vec<Reply> = probes
        .iter()
        .map(|probe| runtime_session.control(port, probe))
        .collect();
    let setup = session.setup.len();

    for (row, reply) in session.rows.iter().zip(&ours[setup..]) {
        let label = format!(
            "{}: the documented residual {:?} ({})",
            session.what, row.probe.label, row.residual
        );
        match reply {
            Reply::Bytes(bytes) => assert_eq!(&bytes[..], row.ours, "{label}"),
            other => panic!(
                "{label}: expected the documented substitution {}, got {} — a residual must stay \
                 what the table records, never a fabricated reply",
                render_bytes(row.ours),
                other.render()
            ),
        }
    }
    // The oracle's own answers for the very same probes, so the recorded divergence is measured —
    // and so the setup sequence is proven to agree before it is relied on.
    let Some(oracle) = oracle_lines(session.driver, &probes) else {
        skip_oracle_check(session.what);
        return;
    };
    assert_eq!(
        oracle.len(),
        probes.len(),
        "{}: the oracle answered every probe",
        session.what
    );
    for ((probe, ours), line) in probes[..setup].iter().zip(&ours[..setup]).zip(&oracle[..setup]) {
        assert_agrees(session.what, probe, ours, line);
    }
    for (row, line) in session.rows.iter().zip(&oracle[setup..]) {
        assert_eq!(
            *line, row.oracle,
            "{}: the documented residual {:?} ({})",
            session.what, row.probe.label, row.residual
        );
    }
}

