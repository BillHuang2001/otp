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

//! The **`{active, …}` state seam** on the two built-in socket drivers: `Driver::active_mode`
//! (the C's `desc->inet.active` / `desc->inet.active_count`) and `Driver::take_active_spends` (the
//! grants a **delivery** spent), reached through the runtime's own
//! [`PortRegistry::active_mode`](crate::PortRegistry::active_mode) /
//! [`PortRegistry::take_active_spends`](crate::PortRegistry::take_active_spends).
//!
//! Two facts the owner-facing messages cannot carry are pinned here:
//!
//! * the `{active, N}` **counter** ([`ActiveMode::Multi`]) — no message reports how many credits
//!   are left, so without the seam a runtime could only mirror the `setopts` walk it sent itself;
//! * the `{active, once}` **flip**, which is silent in the C (`INET_CHECK_ACTIVE_TO_PASSIVE`,
//!   `inet_drv.c:1552`-`:1560`): the grant is spent and the socket goes passive with **no**
//!   `{tcp_passive, S}` / `{udp_passive, S}`, so a spend with no accompanying `Passive` event is
//!   the once-grant's only announcement.
//!
//! The spend is recorded in exactly one place per driver — the per-delivery spend site
//! ([`TcpInetDriver::count_delivery`](super::active) and its datagram twin
//! [`UdpInetDriver::after_delivery`](super::udp::active)) — and **never** by the `setopts` walk
//! that drives an `{active, N}` counter to zero (`inet_drv.c:6993`-`:6998`), which delivers no
//! data and therefore spends nothing. That distinction is a case of its own below, because
//! "fixing" it would be an easy mistake.
//!
//! Every case drives the real runtime path over a real loopback socket (the stream half through
//! [`link`], the datagram half through the `udp` suite's rig), and every wait is a **condition**
//! under a hang guard, never a fixed sleep. The `erl` rows at the bottom cross-check the
//! observable counterpart on the system driver and skip **loudly** when `erl` is absent.

use std::time::{Duration, Instant};

use super::active_tests::{
    active_false, active_multi, active_once, active_true, link, link_with, packet, passive_count,
    payloads, records, send, Rig,
};
use super::opcode_tests::erl_stdout;
use super::udp::active_tests::{data_of, passive_count as udp_passive_count, Rig as UdpRig};
use super::wire::ctl_ok;
use crate::port_task::{ActiveInputEvent, ActiveMode, ActiveSpend};

/// `INET_ONCE` (`inet_drv.c:781`) — the `{active, once}` wire value.
const INET_ONCE: i32 = 2;
/// The `{active, N}` credit the counter cases arm with.
const CREDIT: i16 = 5;
/// How long a condition wait may take before the case is declared hung.
const GUARD: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// `{active, once}` — one delivery, one silent spend
// ---------------------------------------------------------------------------

/// `{active, once}` on a connected stream port: arming is not spending, **one** delivery spends the
/// grant, the spend is reported **once** as [`ActiveSpend::Once`], and the port is passive
/// afterwards — with no `Passive` event, which is exactly what makes the spend the only
/// announcement (`INET_CHECK_ACTIVE_TO_PASSIVE`, `inet_drv.c:1552`-`:1560`).
#[test]
fn a_once_grant_on_a_stream_port_spends_itself_silently() {
    let super::active_tests::Link {
        mut port,
        mut peer,
    } = link();

    assert_eq!(port.active_mode(), ActiveMode::False, "a fresh port is passive");
    assert_eq!(port.setopts(&[active_once()]), ctl_ok());
    let id = port.id();
    assert_eq!(port.active_mode(), ActiveMode::Once, "the grant is armed");
    assert!(
        port.take_active_spends().is_empty(),
        "arming a grant is not spending it"
    );

    send(&mut peer, b"abc");
    let delivered = port.until(1);
    assert_eq!(delivered.len(), 1, "one message: {delivered:?}");
    assert_eq!(payloads(&delivered)[0].body.as_deref(), Some(&b"abc"[..]));
    assert_eq!(
        passive_count(&delivered),
        0,
        "`{{active, once}}` never sends a passive event"
    );

    assert_eq!(
        port.take_active_spends(),
        vec![(id, ActiveSpend::Once)],
        "the delivered bytes spent the grant"
    );
    assert_eq!(
        port.active_mode(),
        ActiveMode::False,
        "the spent grant left the port passive"
    );
    assert!(
        port.take_active_spends().is_empty(),
        "a spend is reported exactly once"
    );

    // The grant really is gone: a further write is not delivered.
    send(&mut peer, b"def");
    assert!(
        port.quiet(3).is_empty(),
        "a spent grant delivers nothing more"
    );
}

/// The datagram twin: `{active, once}` on a bound `udp_inet` port delivers **one** datagram, then
/// goes passive silently (`packet_reply_binary_data` → `INET_CHECK_ACTIVE_TO_PASSIVE`,
/// `inet_drv.c:4352`/`:1552`-`:1560`) — reported only through the spend.
#[test]
fn a_once_grant_on_a_datagram_port_spends_itself_silently() {
    let mut rig = UdpRig::new();
    assert_eq!(rig.active_mode(), ActiveMode::False, "a fresh port is passive");
    rig.arm(INET_ONCE);
    let id = rig.id();
    assert_eq!(rig.active_mode(), ActiveMode::Once);

    rig.inject(b"abc");
    let drained = rig.drain_until_quiet();
    assert_eq!(data_of(&drained.inputs).len(), 1, "one datagram delivered");
    assert_eq!(
        udp_passive_count(&drained.inputs),
        0,
        "`{{active, once}}` never reports a passive message"
    );

    assert_eq!(rig.take_active_spends(), vec![(id, ActiveSpend::Once)]);
    assert_eq!(rig.active_mode(), ActiveMode::False);
    assert!(rig.take_active_spends().is_empty(), "taking consumes");
}

// ---------------------------------------------------------------------------
// `{active, N}` — the counter seam, and one spend at zero
// ---------------------------------------------------------------------------

/// `{active, 5}`: the seam exposes the **running** counter (which no message carries), a delivery
/// below the ceiling spends nothing, and the delivery that reaches zero spends the grant **once**
/// while its `Passive` event announces the flip (`packet_passive_message`, `inet_drv.c:4215`,
/// reached from `:1552`).
#[test]
fn a_counter_grant_counts_down_and_spends_once_at_zero() {
    let super::active_tests::Link {
        mut port,
        mut peer,
    } = link();
    assert_eq!(port.setopts(&[packet(2), active_multi(CREDIT)]), ctl_ok());
    let id = port.id();
    assert_eq!(
        port.active_mode(),
        ActiveMode::Multi { remaining: CREDIT },
        "the grant is the full credit"
    );

    // Three of the five credits: each delivered frame decrements the counter, all silently.
    for (spent, byte) in [b'a', b'b', b'c'].into_iter().enumerate() {
        send(&mut peer, &[0, 1, byte]);
        let delivered = port.until(1);
        assert_eq!(delivered.len(), 1, "one frame, one message: {delivered:?}");
        assert_eq!(
            port.active_mode(),
            ActiveMode::Multi {
                remaining: CREDIT - (spent as i16) - 1
            },
            "the counter is decremented per delivered frame"
        );
        assert!(
            port.take_active_spends().is_empty(),
            "a credit is spent only when the counter reaches zero"
        );
    }

    // The fourth: one credit left, still nothing spent.
    send(&mut peer, &[0, 1, b'd']);
    assert_eq!(port.until(1).len(), 1);
    assert_eq!(port.active_mode(), ActiveMode::Multi { remaining: 1 });
    assert!(port.take_active_spends().is_empty());

    // The fifth reaches zero: one spend, one passive event, a passive port afterwards.
    send(&mut peer, &[0, 1, b'e']);
    let last = port.until(2);
    assert_eq!(last.len(), 2, "the last delivery and its passive event");
    assert_eq!(payloads(&last[..1])[0].body.as_deref(), Some(&b"e"[..]));
    assert_eq!(passive_count(&last), 1, "the flip is announced exactly once");
    assert_eq!(
        port.take_active_spends(),
        vec![(id, ActiveSpend::Multi)],
        "the exhausted counter spent the grant"
    );
    assert_eq!(port.active_mode(), ActiveMode::False);
    assert!(port.take_active_spends().is_empty(), "taking consumes");
}

/// The datagram twin of the counter: the remaining credit is readable at every step, and only the
/// datagram that reaches zero spends.
#[test]
fn a_datagram_counter_counts_down_and_spends_once_at_zero() {
    let mut rig = UdpRig::new();
    rig.arm_multi(3);
    let id = rig.id();
    assert_eq!(rig.active_mode(), ActiveMode::Multi { remaining: 3 });

    for payload in [b"a", b"b"] {
        rig.inject(payload);
        let drained = rig.drain_until_quiet();
        assert_eq!(data_of(&drained.inputs).len(), 1, "one datagram");
        assert_eq!(udp_passive_count(&drained.inputs), 0, "a credit is left");
        assert!(
            rig.take_active_spends().is_empty(),
            "no spend before the counter reaches zero"
        );
    }
    assert_eq!(rig.active_mode(), ActiveMode::Multi { remaining: 1 });

    rig.inject(b"c");
    let last = rig.drain_until_quiet();
    assert_eq!(data_of(&last.inputs).len(), 1, "the last datagram");
    assert_eq!(
        udp_passive_count(&last.inputs),
        1,
        "the flip is announced once"
    );
    assert_eq!(rig.take_active_spends(), vec![(id, ActiveSpend::Multi)]);
    assert_eq!(rig.active_mode(), ActiveMode::False);
}

// ---------------------------------------------------------------------------
// `{active, true}` / `{active, false}` — the two arms that never spend
// ---------------------------------------------------------------------------

/// `{active, true}` arms once and never spends — the C's `default`/no-op arm has no counter and no
/// flip (`INET_CHECK_ACTIVE_TO_PASSIVE` only touches `INET_ONCE` and `INET_MULTI`).
#[test]
fn an_active_true_port_stays_armed_and_never_spends() {
    let super::active_tests::Link {
        mut port,
        mut peer,
    } = link();
    assert_eq!(port.setopts(&[packet(2), active_true()]), ctl_ok());
    assert_eq!(port.active_mode(), ActiveMode::True);

    for byte in [b'a', b'b', b'c'] {
        send(&mut peer, &[0, 1, byte]);
        assert_eq!(port.until(1).len(), 1, "every frame is delivered");
        assert_eq!(
            port.active_mode(),
            ActiveMode::True,
            "`{{active, true}}` never flips"
        );
        assert!(
            port.take_active_spends().is_empty(),
            "`{{active, true}}` never spends a grant"
        );
    }
}

/// `{active, false}`: the mode reads passive and nothing is ever spent — the bytes take the
/// passive path into the port input queue, so the active channel stays silent.
#[test]
fn a_passive_port_has_no_mode_and_never_spends() {
    // A bare port, before any socket exists: the same state.
    let mut bare = Rig::bare();
    assert_eq!(bare.active_mode(), ActiveMode::False);
    assert!(bare.take_active_spends().is_empty());

    let super::active_tests::Link {
        mut port,
        mut peer,
    } = link();
    assert_eq!(port.setopts(&[active_false()]), ctl_ok());
    assert_eq!(port.active_mode(), ActiveMode::False);

    send(&mut peer, b"abc");
    assert!(
        port.quiet(3).is_empty(),
        "a passive port delivers nothing on the active channel"
    );
    assert_eq!(
        port.active_mode(),
        ActiveMode::False,
        "a passive port has no mode to spend"
    );
    assert!(
        port.take_active_spends().is_empty(),
        "`{{active, false}}` never spends"
    );
    // ... and the bytes are on the passive path, where the existing `recv` route reads them.
    until_input(&mut port, 3);
}

// ---------------------------------------------------------------------------
// The distinction: the `setopts` walk that lands on zero spends nothing
// ---------------------------------------------------------------------------

/// An `{active, N}` counter driven to zero **inside the `setopts` walk** still emits its
/// `{tcp_passive, S}` (`inet_drv.c:6993`-`:6998`) but records **no** spend: the C spends a grant
/// only per delivered packet (`INET_CHECK_ACTIVE_TO_PASSIVE` runs from the delivery path,
/// `:4302`/`:4329`/`:4352`), and the option walk delivers nothing.
///
/// This is the case that would look "fixed" if the spend were recorded wherever the counter
/// reaches zero — the passive event and the spend are **not** the same event.
#[test]
fn the_setopts_walk_that_lands_on_zero_reports_a_passive_event_but_spends_nothing() {
    let super::active_tests::Link { mut port, .. } = link();

    // `{active, 1}` arms a one-message grant; `{active, -1}` then drives the counter to zero
    // inside the walk. No round runs between the two calls.
    assert_eq!(port.setopts(&[active_multi(1)]), ctl_ok());
    assert_eq!(port.active_mode(), ActiveMode::Multi { remaining: 1 });
    assert_eq!(port.setopts(&[active_multi(-1)]), ctl_ok());

    let staged = port.drain_staged();
    assert_eq!(staged.len(), 1, "one passive event from the walk: {staged:?}");
    assert!(
        matches!(staged[0].event, ActiveInputEvent::Passive),
        "the option walk's own `{{tcp_passive, S}}`"
    );
    assert_eq!(port.active_mode(), ActiveMode::False, "the walk zeroed it");
    assert!(
        port.take_active_spends().is_empty(),
        "no data was delivered, so no grant was spent"
    );
}

// ---------------------------------------------------------------------------
// The runtime's route: the built-in registry's `"tcp_inet"` port
// ---------------------------------------------------------------------------

/// The seam over a port whose driver came from the **built-in registry**
/// (`DriverRegistry::with_builtins().create("tcp_inet")` — `beam_rt`'s
/// `open_port({spawn_driver, "tcp_inet"}, …)` route, boxed as a `Box<dyn Driver>`), with
/// `{active, once}` armed through the **named** `setopts` verb the way `prim_inet` arms it.
///
/// This is the exact path the runtime takes: one round drains the delivery from
/// `take_active_inputs`, and the two seam accessors then report the flip and the spend.
#[test]
fn the_registry_reports_the_mode_and_the_spend_of_a_factory_built_port() {
    let super::active_tests::Link {
        mut port,
        mut peer,
    } = link_with(Rig::from_factory("tcp_inet"));
    let id = port.id();

    assert_eq!(
        port.active_mode(),
        ActiveMode::False,
        "a factory-built port starts passive"
    );
    assert_eq!(
        port.named("setopts", &records(&[active_once()])),
        ctl_ok(),
        "the named `setopts` verb arms the grant"
    );
    assert_eq!(port.active_mode(), ActiveMode::Once);
    assert!(port.take_active_spends().is_empty());

    send(&mut peer, b"abc");
    assert_eq!(port.until(1).len(), 1, "the granted message");

    assert_eq!(
        port.active_mode(),
        ActiveMode::False,
        "the delivery spent the grant"
    );
    assert_eq!(
        port.take_active_spends(),
        vec![(id, ActiveSpend::Once)],
        "the registry stamps the spend with the port's own id"
    );
    assert!(
        port.take_active_spends().is_empty(),
        "draining is what removes it"
    );
}

// ---------------------------------------------------------------------------
// The system-`erl` oracle (OTP 29.1.1 / erts-17.1)
//
// One row per mode, phrased as the counts the spend notification stands for: a `{active, once}`
// grant delivers one message with **no** passive one (one spend, no announcement), a `{active, 2}`
// grant delivers two messages and then exactly one `{tcp_passive, S}` (one spend with an
// announcement), and `{active, true}` delivers every message with neither (no spend at all).
// ---------------------------------------------------------------------------

/// The oracle's observable counterpart of what [`ActiveSpend`] reports, for all three modes — the
/// count of `{tcp, S, Data}` messages and of `{tcp_passive, S}` messages each mode produces.
#[test]
fn the_oracle_agrees_with_what_the_spend_notification_reports() {
    /// Runs `body` (a `Send`/`Count` preamble) on the system driver and returns its dump line.
    fn counts(setopts: &str, frames: &str) -> Option<String> {
        let expr = concat!(
            "{ok,L} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true},",
            "{ip,{127,0,0,1}}]), ",
            "{ok,P} = inet:port(L), ",
            "{ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{nodelay,true}]), ",
            "{ok,S} = gen_tcp:accept(L), ",
            "ok = inet:setopts(S,[{packet,2},SETOPTS]), ",
            "Send = fun(B) -> ok = gen_tcp:send(C, <<0,1,B>>) end, ",
            // Collect whatever arrives within the budget: `{tcp,_,_}`, `{tcp_passive,_}`, nothing.
            "Count = fun Count(Acc) -> receive {tcp,_,_} -> Count([data|Acc]); ",
            "{tcp_passive,_} -> Count([passive|Acc]) after 500 -> lists:reverse(Acc) end end, ",
            "[Send(B) || B <- FRAMES], ",
            "io:format(\"~p~n\",[Count([])])"
        )
        .replace("SETOPTS", setopts)
        .replace("FRAMES", frames);
        erl_stdout(&expr)
    }

    // `{active, once}`: one message, no passive one — the spend is silent.
    let Some(out) = counts("{active,once}", "[97,98]") else {
        return;
    };
    assert_eq!(
        out.trim(),
        "[data]",
        "`{{active,once}}` delivers one message and never a passive one"
    );

    // `{active, 2}`: two messages, then exactly one `{tcp_passive, S}`.
    let Some(out) = counts("{active,2}", "[97,98,99]") else {
        return;
    };
    assert_eq!(
        out.trim(),
        "[data,data,passive]",
        "two deliveries, one passive message, then silence"
    );

    // `{active, true}`: every message, never a passive one.
    let Some(out) = counts("{active,true}", "[97,98,99]") else {
        return;
    };
    assert_eq!(
        out.trim(),
        "[data,data,data]",
        "`{{active,true}}` delivers them all and never reports a passive one"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rounds until the port's **passive** input queue holds `want` bytes — the non-vacuity witness
/// that a passive port really took the passive path — under [`GUARD`].
fn until_input(port: &mut Rig, want: usize) {
    let deadline = Instant::now() + GUARD;
    while port.input_len() < want {
        assert!(
            Instant::now() < deadline,
            "the passive bytes never reached the input queue ({} of {want})",
            port.input_len()
        );
        let _ = port.round();
    }
}
