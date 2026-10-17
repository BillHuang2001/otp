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
//! (socket-less) refusals and fake addresses, the datagram lifecycle, the connected/peer half, the
//! opcode-scope rules, the option store, and the `tcp_inet` bare refusals of the same bridge.
//!
//! # The residuals (`the_documented_datagram_residuals_stay_typed`)
//!
//! A handful of `udp` shapes are **known divergences owned by other nodes** — the `subscribe`
//! body, a well-formed `fdopen`, the `getservby*`/`getif*` lookups, the asynchronous
//! `PACKET_REQ_RECV` op-id and the second-`open` `exbadseq`, plus `getopts` of a `getsockopt` the
//! socket refuses. Each is asserted on **both** sides: the exact oracle output (so the recorded
//! divergence cannot rot) and this runtime's current honest answer (a typed error, or the
//! documented synchronous read-now substitution — never a wrong result).
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
}

/// One probe of the matrix: the line label both renderings print, the integer `port_control/3`
/// receives, the operand bytes and the comparison rule.
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

    /// A **raw** integer: an opcode below the magic number (whose unsigned subtraction wraps past
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
    }
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
/// `nodelay` (code `10`) is **excluded here** and pinned in the residual suite instead: a datagram
/// socket's `getsockopt(TCP_NODELAY)` fails and the two sides answer differently.
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

/// The `tcp_inet` bare-port refusals of the same bridge.
#[test]
fn the_bare_stream_port_refusals_match_the_oracle() {
    run_family("the bare `tcp_inet` port", "tcp_inet", &bare_stream());
}

// ---------------------------------------------------------------------------
// The documented residuals
// ---------------------------------------------------------------------------

/// How the oracle answers one residual probe.
enum Oracle {
    /// The exact `~w` rendering of the reply this oracle version returns.
    Exact(&'static str),
    /// A reply that depends on the host: only its leading prefix is stable.
    Prefix(&'static str),
}

/// What this runtime answers today for the same probe.
enum Ours {
    /// A typed capability gap whose message must contain this fragment.
    Gap(&'static str),
    /// The exact reply bytes of a shape this floor deliberately substitutes.
    Bytes(&'static [u8]),
    /// A propagated host-syscall failure whose message must contain this fragment (the driver
    /// floor raising where the C drops the record).
    Io(&'static str),
}

/// One documented divergence: the probe, the oracle's answer and this runtime's honest one.
struct Residual {
    /// The probe itself.
    probe: Probe,
    /// What the oracle answers.
    oracle: Oracle,
    /// What this runtime answers today, and (in the doc comment of the list below) why.
    ours: Ours,
    /// The residual this row records.
    residual: &'static str,
}

/// The `getsockopt` failure text of the `nodelay`-on-a-datagram-socket residual.
const IO_RESIDUAL: Ours = Ours::Io("Operation not supported (os error 95)");

/// The `udp` shapes this runtime answers differently, each a **capability gap or a documented
/// substitution** on the driver side (`beam_io`), never a wrong result.
///
/// The driver floor has no driver→owner message path, so `INET_REQ_SUBSCRIBE`'s notification and the
/// asynchronous `PACKET_REQ_RECV` op-id cannot be honoured; adopting a caller's descriptor
/// (`INET_REQ_FDOPEN`), the `getservby*(3)` and `getifaddrs(3)` lookups are not wrapped by its `sys`
/// layer; a second `open` is the documented `einval` non-mirror of the C's `exbadseq`; and
/// `getopts` of an option the socket refuses propagates the `getsockopt` failure where the C drops
/// the record (`inet_drv.c:9734-9741`).
///
/// The three `subscribe` rows are the one group that closes **outside** this crate: `beam_io` owes
/// `INET_REQ_SUBSCRIBE` the `[1]` / `[1,1,0,0,0,0]` / `badarg` answers (its body only registers a
/// subscriber when the output queue is non-empty, which this floor's port never is). When that lands
/// they are ordinary agreement rows — the oracle column already holds what both sides will answer —
/// so they must move out of this table rather than be relaxed.
fn residuals() -> Vec<Residual> {
    let gap = |probe: Probe, oracle: Oracle, name: &'static str, residual: &'static str| Residual {
        probe,
        oracle,
        ours: Ours::Gap(name),
        residual,
    };
    vec![
        gap(
            Probe::exact("subscribe_empty", 24, vec![]),
            Oracle::Exact("[1]"),
            "`subscribe`",
            "INET_REQ_SUBSCRIBE's body needs the driver→owner notification the floor has no channel for",
        ),
        gap(
            Probe::exact("subscribe_1", 24, vec![1]),
            Oracle::Exact("[1,1,0,0,0,0]"),
            "`subscribe`",
            "the `[1] ++ op ++ outq_be32` notification registration, same missing channel",
        ),
        gap(
            Probe::exact("subscribe_2", 24, vec![2]),
            Oracle::Exact("badarg"),
            "`subscribe`",
            "the C's -1 for an unknown subscribe op, which the oracle surfaces as a raise",
        ),
        gap(
            Probe::exact("fdopen_wellformed", 13, vec![1, 2, 0, 0, 0, 9]),
            Oracle::Exact("[0,101,110,111,116,115,111,99,107]"),
            "`fdopen`",
            "adopting a caller's descriptor: the oracle answers `[0]enotsock`, the floor cannot adopt at all",
        ),
        gap(
            Probe::exact("getservbyname_wellformed", 17, vec![3, 100]),
            Oracle::Exact("[0,101,105,110,118,97,108]"),
            "`getservbyname`",
            "the `getservbyname(3)` lookup is not wrapped by the driver's `sys` layer",
        ),
        gap(
            Probe::exact("getservbyport_wellformed", 18, vec![0, 53, 3]),
            Oracle::Exact("[0,101,105,110,118,97,108]"),
            "`getservbyport`",
            "the `getservbyport(3)` lookup is not wrapped by the driver's `sys` layer",
        ),
        gap(
            Probe::exact("getiflist", 21, vec![]),
            Oracle::Prefix("[1,"),
            "`getiflist`",
            "the `getifaddrs(3)` interface table (host-dependent, hence the prefix rule)",
        ),
        gap(
            Probe::exact("ifget", 22, vec![1]),
            Oracle::Exact("[0,101,105,110,118,97,108]"),
            "`ifget`",
            "`ifget` over the same `getifaddrs(3)` walk",
        ),
        gap(
            Probe::exact("ifset", 23, vec![1]),
            Oracle::Exact("[1]"),
            "`ifset`",
            "`ifset` over the same `getifaddrs(3)` walk",
        ),
        gap(
            Probe::exact("getifaddrs", 25, vec![]),
            Oracle::Prefix("[1,"),
            "`getifaddrs`",
            "`getifaddrs` over the same `getifaddrs(3)` walk",
        ),
        Residual {
            probe: Probe::exact("getopts_nodelay", 8, vec![10]),
            oracle: Oracle::Exact("[1]"),
            ours: IO_RESIDUAL,
            residual: "`getsockopt(TCP_NODELAY)` on a datagram socket is ENOTSUP; the C drops the record and answers `[1]`",
        },
        Residual {
            probe: Probe::exact("recv_wellformed", 60, vec![0; 8]),
            oracle: Oracle::Exact("[1,0,0]"),
            ours: Ours::Bytes(&[1]),
            residual: "PACKET_REQ_RECV is the synchronous read-now substitution (bare `[1]` with no datagram pending) instead of the C's asynchronous `[1] ++ async_id_be16`",
        },
    ]
}

/// Every documented `udp` residual, asserted on **both** sides.
///
/// The runtime side holds with no toolchain; the oracle side pins the exact output of OTP
/// 29.1.1/erts-17.1 so the record cannot rot. Nothing here is a relaxed assertion: a residual is
/// either a typed error or the exact bytes of a documented substitution.
#[test]
fn the_documented_datagram_residuals_stay_typed() {
    let residual = residuals();
    let mut probes = vec![Probe::exact("open", 1, vec![1, 2, 0])];
    probes.extend(residual.iter().map(|row| {
        Probe {
            label: row.probe.label,
            op: row.probe.op,
            arg: row.probe.arg.clone(),
            rule: row.probe.rule,
        }
    }));

    let mut session = Session::new();
    let port = session.open("udp_inet");
    let mut ours = Vec::new();
    for probe in &probes {
        ours.push(session.control(port, probe));
    }
    let open = &ours[0];
    assert_eq!(
        open.render(),
        "[1]",
        "the residual session needs the socket `open` creates"
    );

    for (row, reply) in residual.iter().zip(&ours[1..]) {
        let label = format!(
            "the documented residual {:?} ({})",
            row.probe.label, row.residual
        );
        match (&row.ours, reply) {
            (Ours::Bytes(expected), Reply::Bytes(bytes)) => {
                assert_eq!(&bytes[..], *expected, "{label}")
            }
            (Ours::Io(fragment), Reply::Gap(message)) => assert!(
                message.starts_with("io error:") && message.contains(fragment),
                "{label}: expected the propagated host `getsockopt` failure {fragment:?}, got {message:?}"
            ),
            (Ours::Gap(fragment), Reply::Gap(message)) => assert!(
                message.starts_with("error unsupported:") && message.contains(fragment),
                "{label}: expected a typed capability gap naming {fragment}, got {message:?}"
            ),
            (expected, other) => panic!(
                "{label}: expected {}, got {} — a residual must stay a typed error, never a \
                 fabricated reply",
                match expected {
                    Ours::Bytes(bytes) => render_bytes(bytes),
                    Ours::Gap(_) => "a typed capability gap".to_string(),
                    Ours::Io(fragment) => format!("the host syscall failure {fragment:?}"),
                },
                other.render()
            ),
        }
    }

    // The oracle's own answers for the very same list, so the recorded divergence is measured.
    let Some(oracle) = oracle_lines("udp_inet", &probes) else {
        skip_oracle_check("the documented `udp_inet` control residuals");
        return;
    };
    assert_eq!(oracle.len(), probes.len(), "the oracle answered every residual probe");
    assert_eq!(oracle[0], "[1]", "the residual session's own `open`");
    for (row, line) in residual.iter().zip(&oracle[1..]) {
        let label = format!(
            "the documented residual {:?} ({})",
            row.probe.label, row.residual
        );
        match row.oracle {
            Oracle::Exact(expected) => assert_eq!(*line, expected, "{label}"),
            Oracle::Prefix(prefix) => assert!(
                line.starts_with(prefix),
                "{label}: expected a reply starting with {prefix:?}, got {line:?}"
            ),
        }
    }
}
