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

//! [`crate::port::select`]'s unit suite: the `enif_select`/`prim_tty:tty_select/2` registration
//! store ([`SelectWatch`]'s arm/stop/cancel/take_ready/prune_dead semantics over the runtime's one
//! poller) and the readiness classification [`events_of`] performs.
//!
//! A separate file so the suite does not grow the store past its production size; the sibling
//! `port/*_tests.rs` modules (`port/driver/*_tests.rs` in particular) are split the same way. It is
//! a child module of `select`, so `use super::*;` reaches every private item of the store.
use super::*;
use beam_io::Ready;
use beam_term::make_small;
use std::collections::HashSet;
use std::os::fd::AsRawFd;
/// A poller and a watch with one `prim_tty:tty_select/2`-shaped registration (a read interest
/// on [`TTY_IFD`], a plain payload, no resource object) armed, plus the terms it carries.
fn armed() -> (Poller, SelectWatch, ProcessId, Eterm, Eterm) {
    let mut poller = Poller::new().expect("a poller");
    let mut watch = SelectWatch::new();
    let owner = ProcessId::new(7, 0);
    let handle = make_small(11);
    let reference = make_small(22);
    watch
        .arm(
            &mut poller,
            TTY_IFD,
            Interest::READABLE,
            owner,
            SelectPayload::Plain { reference },
            handle,
            core::ptr::null_mut(),
        )
        .expect("fd 0 is registrable in a test process");
    (poller, watch, owner, handle, reference)
}

/// The poll round's event for `key`, reporting **no** observed direction — what an EOF-only
/// readiness looks like (see [`SelectEvent::of`]'s fallback).
///
/// The token is read out of the watch's own registration: a poller token is opaque, so the
/// only way a test can name a real one is to take the one the registration was minted with
/// (the store keys by `key`, so any minted token will do for a foreign key).
fn polled(watch: &SelectWatch, key: u64) -> Event {
    let token = watch
        .registrations
        .get(&key)
        .or_else(|| watch.registrations.values().next())
        .map(|registration| registration.token)
        .expect("a poll round needs a token, so at least one registration must be armed");
    Event::new(token, key, Ready::default())
}

#[test]
fn a_fresh_watch_is_not_armed_and_owns_nothing() {
    let watch = SelectWatch::new();
    assert!(!watch.is_armed());
    assert!(!watch.owns(0));
    assert!(!watch.owns(TTY_KEY_TOP));
}

#[test]
fn arming_reserves_a_key_and_owns_it() {
    let (_poller, watch, ..) = armed();
    assert!(watch.is_armed());
    assert!(watch.owns(TTY_KEY_TOP), "the first key is the top of the range");
    // The reservation stays outside the port-number space and below the signal sentinel.
    assert!(TTY_KEY_TOP < super::super::watch::SIGNAL_KEY);
    assert!(TTY_KEY_FLOOR > 1 << 59, "keys never wrap into the port-number space");
}

/// The full `prim_tty` shape: a read interest on fd 0 delivers one plain `ready_input` report
/// carrying the handle and the caller's reference, and disarms itself.
#[test]
fn a_readiness_report_is_delivered_exactly_once() {
    let (mut poller, mut watch, owner, handle, reference) = armed();
    let events = [polled(&watch, TTY_KEY_TOP)];
    let delivered = watch.take_ready(&mut poller, &events).expect("a delivery");
    assert_eq!(delivered.len(), 1);
    let delivery = &delivered[0];
    assert_eq!(delivery.owner, owner);
    assert_eq!(delivery.handle, handle);
    assert_eq!(delivery.event, SelectEvent::ReadyInput);
    match delivery.payload {
        SelectPayload::Plain { reference: delivered } => assert_eq!(delivered, reference),
        SelectPayload::Custom { .. } => panic!("a plain arming must not deliver a custom message"),
    }
    assert!(delivery.obj.is_null(), "a `tty_select/2` arming carries no resource object");
    // One-shot: the interest is disarmed, so a second round reports nothing.
    assert!(!watch.is_armed());
    assert!(watch
        .take_ready(&mut poller, &events)
        .expect("nothing armed")
        .is_empty());
}

#[test]
fn a_key_the_watch_does_not_own_is_ignored() {
    let (mut poller, mut watch, ..) = armed();
    let foreign = [polled(&watch, 42)];
    assert!(watch
        .take_ready(&mut poller, &foreign)
        .expect("a foreign key is skipped")
        .is_empty());
    assert!(watch.is_armed(), "the armed registration is untouched");
}

/// A second arming on one descriptor **widens** the interest mask and replaces the payload,
/// owner, handle and object — `mio` allows one membership per descriptor, so there is no second
/// registration to create.
#[test]
fn a_second_arming_widens_the_interest_and_replaces_the_payload() {
    let (mut poller, mut watch, owner, _handle, _reference) = armed();
    let handle = make_small(33);
    let message = make_small(44);
    watch
        .arm(
            &mut poller,
            TTY_IFD,
            Interest::WRITABLE,
            owner,
            SelectPayload::Custom { message },
            handle,
            core::ptr::null_mut(),
        )
        .expect("a re-registration");
    assert_eq!(watch.registrations.len(), 1, "still one registration");
    assert_eq!(watch.by_fd.len(), 1, "still one descriptor index entry");
    assert_eq!(watch.by_fd.get(&TTY_IFD).copied(), Some(TTY_KEY_TOP), "the key is kept");
    let registration = &watch.registrations[&TTY_KEY_TOP];
    assert_eq!(registration.interest, Interest::BOTH, "the mask is widened");
    assert_eq!(registration.handle, handle, "the handle is replaced");

    let events = [polled(&watch, TTY_KEY_TOP)];
    let delivered = watch.take_ready(&mut poller, &events).expect("a delivery");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].handle, handle);
    match delivered[0].payload {
        SelectPayload::Custom { message: delivered } => assert_eq!(delivered, message),
        SelectPayload::Plain { .. } => panic!("the widened arming's payload must replace the old one"),
    }
}

/// `ERL_NIF_SELECT_STOP` answers the directions the stopped registration had armed, and
/// de-registers both the descriptor and the `(fd, handle)` index — a handle that names no
/// registration on an armed descriptor still stops it (the C addresses a registration by its
/// descriptor).
#[test]
fn stop_de_registers_the_descriptor() {
    let (mut poller, mut watch, _owner, handle, _reference) = armed();
    let stale = make_small(99);
    let stopped = watch.stop(&mut poller, TTY_IFD, handle).expect("a stop");
    assert!(
        stopped.readable && !stopped.writable && !stopped.error,
        "the armed read interest is the one the stop reports: {stopped:?}"
    );
    assert!(!watch.is_armed());
    assert_eq!(watch.by_fd.get(&TTY_IFD), None);
    assert_eq!(watch.by_fd_handle.get(&(TTY_IFD, handle_key(handle))), None);
    assert!(
        watch
            .stop(&mut poller, TTY_IFD, handle)
            .expect("nothing armed")
            .is_empty(),
        "a second STOP reports that there was nothing to stop"
    );

    // A handle the store does not know stops the descriptor all the same.
    let (mut poller, mut watch, ..) = armed();
    assert!(
        !watch
            .stop(&mut poller, TTY_IFD, stale)
            .expect("a stop")
            .is_empty()
    );
    assert!(!watch.is_armed());
}

/// `ERL_NIF_SELECT_CANCEL` reports the directions really armed and clears exactly those — the
/// descriptor is de-registered once no direction is left.
#[test]
fn cancel_reports_the_directions_really_armed() {
    let (mut poller, mut watch, owner, handle, reference) = armed();
    // A read interest is all that is armed, so a write cancel clears nothing.
    assert!(watch
        .cancel(&mut poller, TTY_IFD, handle, Interest::WRITABLE)
        .expect("a cancel")
        .is_empty());
    assert!(watch.is_armed(), "nothing was cancelled, so nothing was disarmed");

    // Widen to both directions, then clear the read half: the write half stays armed.
    watch
        .arm(
            &mut poller,
            TTY_IFD,
            Interest::WRITABLE,
            owner,
            SelectPayload::Plain { reference },
            handle,
            core::ptr::null_mut(),
        )
        .expect("a re-registration");
    let cancelled = watch
        .cancel(&mut poller, TTY_IFD, handle, Interest::READABLE)
        .expect("a cancel");
    assert!(cancelled.readable && !cancelled.writable && !cancelled.error);
    assert!(watch.is_armed());
    assert_eq!(watch.registrations[&TTY_KEY_TOP].interest, Interest::WRITABLE);

    // Clearing the remaining direction de-registers the descriptor.
    let cancelled = watch
        .cancel(&mut poller, TTY_IFD, handle, Interest::WRITABLE)
        .expect("a cancel");
    assert!(!cancelled.readable && cancelled.writable && !cancelled.error);
    assert!(!watch.is_armed());
    assert!(
        watch
            .cancel(&mut poller, TTY_IFD, handle, Interest::READABLE)
            .expect("nothing armed")
            .is_empty(),
        "a cancel on an unarmed descriptor cleared nothing"
    );
}

#[test]
fn a_dead_owner_is_pruned_and_a_live_one_is_kept() {
    let (mut poller, mut watch, owner, ..) = armed();
    let live: HashSet<ProcessId> = HashSet::new();
    watch
        .prune_dead(&mut poller, |pid| live.contains(&pid))
        .expect("a prune");
    assert!(!watch.is_armed(), "{owner:?} is gone, so the select is disarmed");
    assert_eq!(watch.by_fd.get(&TTY_IFD), None);

    let (mut poller, mut watch, owner, ..) = armed();
    let mut live = HashSet::new();
    live.insert(owner);
    watch
        .prune_dead(&mut poller, |pid| live.contains(&pid))
        .expect("a prune");
    assert!(watch.is_armed(), "a live owner keeps its registration");
}

/// Every observed direction the registration asked for is reported, in the C's `input` →
/// `output` → `error` order, and a direction it did **not** ask for is not
/// (`erl_check_io.c:2126-2175`).
#[test]
fn each_observed_and_requested_direction_is_reported_in_the_c_order() {
    let readable: Observed = Observed {
        readable: true,
        writable: false,
        error: false,
    };
    let writable = Observed {
        readable: false,
        writable: true,
        error: false,
    };
    let errored = Observed {
        readable: false,
        writable: false,
        error: true,
    };
    let combinations: [(Observed, Vec<SelectEvent>); 7] = [
        (readable, vec![SelectEvent::ReadyInput]),
        (writable, vec![SelectEvent::ReadyOutput]),
        (errored, vec![SelectEvent::ReadyError]),
        (
            Observed {
                readable: true,
                writable: true,
                error: false,
            },
            vec![SelectEvent::ReadyInput, SelectEvent::ReadyOutput],
        ),
        // The measured `READ | ERROR` shape: a reset socket is both readable and errored.
        (
            Observed {
                readable: true,
                writable: false,
                error: true,
            },
            vec![SelectEvent::ReadyInput, SelectEvent::ReadyError],
        ),
        (
            Observed {
                readable: false,
                writable: true,
                error: true,
            },
            vec![SelectEvent::ReadyOutput, SelectEvent::ReadyError],
        ),
        (
            Observed {
                readable: true,
                writable: true,
                error: true,
            },
            vec![
                SelectEvent::ReadyInput,
                SelectEvent::ReadyOutput,
                SelectEvent::ReadyError,
            ],
        ),
    ];
    for (observed, expected) in combinations {
        assert_eq!(
            events_of(observed, Interest::BOTH.add(Interest::ERROR)),
            expected,
            "a registration asking for everything reports every observed direction, in order"
        );
        // Each direction on its own reports only itself.
        assert_eq!(events_of(observed, Interest::READABLE), {
            let mut filtered = expected.clone();
            filtered.retain(|event| *event == SelectEvent::ReadyInput);
            // A readiness that observed *something* but nothing the mask asked for reports
            // nothing at all (`:2144-2151` leaves that direction's slot alone).
            filtered
        });
        assert_eq!(
            events_of(observed, Interest::ERROR),
            expected
                .iter()
                .copied()
                .filter(|event| *event == SelectEvent::ReadyError)
                .collect::<Vec<_>>()
        );
    }
}

/// A readiness that observed no direction at all — a bare hang-up — still reports the direction
/// the registration asked for, which is what wakes a reader whose peer closed.
#[test]
fn a_bare_hang_up_falls_back_to_the_requested_direction() {
    assert_eq!(
        events_of(Observed::default(), Interest::READABLE),
        vec![SelectEvent::ReadyInput]
    );
    assert_eq!(
        events_of(Observed::default(), Interest::WRITABLE),
        vec![SelectEvent::ReadyOutput]
    );
    assert_eq!(
        events_of(Observed::default(), Interest::ERROR),
        vec![SelectEvent::ReadyError]
    );
    assert_eq!(
        events_of(Observed::default(), Interest::BOTH),
        vec![SelectEvent::ReadyInput],
        "a combined mask reports the readable direction first"
    );
}

/// One registration that observed two requested directions delivers **two** notifications,
/// both carrying its own payload — the C's one message per direction.
#[test]
fn one_readiness_delivers_one_notification_per_direction() {
    let (_poller, watch, owner, handle, reference) = armed();
    let registration = SelectRegistration {
        token: watch.registrations[&TTY_KEY_TOP].token,
        fd: TTY_IFD,
        interest: Interest::READABLE.add(Interest::ERROR),
        owner,
        payload: SelectPayload::Plain { reference },
        handle,
        obj: core::ptr::null_mut(),
        hold: None,
    };
    let delivered = deliveries_of(
        registration,
        Observed {
            readable: true,
            writable: false,
            error: true,
        },
    );
    assert_eq!(delivered.len(), 2, "the readable and the errored direction");
    assert_eq!(delivered[0].event, SelectEvent::ReadyInput);
    assert_eq!(delivered[1].event, SelectEvent::ReadyError);
    for delivery in &delivered {
        assert_eq!(delivery.owner, owner);
        assert_eq!(delivery.handle, handle);
        match delivery.payload {
            SelectPayload::Plain { reference: carried } => assert_eq!(carried, reference),
            SelectPayload::Custom { .. } => panic!("a plain arming must not deliver a custom message"),
        }
    }
}

/// A dangling address this runtime never allocated: the "an object that is not ours" operand the
/// keep refusal is about.
fn dangling_obj() -> *mut c_void {
    core::ptr::NonNull::<u8>::dangling().as_ptr().cast()
}

/// An arming with **no** resource object (`prim_tty:tty_select/2`'s shape) holds no reference at
/// all — there is nothing to keep, and the registration must therefore carry no [`SelectHold`].
#[test]
fn an_arm_with_no_resource_object_takes_no_hold() {
    let (_poller, watch, ..) = armed();
    let registration = &watch.registrations[&TTY_KEY_TOP];
    assert!(
        registration.obj.is_null(),
        "the tty arming carries no resource object"
    );
    assert!(
        registration.hold.is_none(),
        "so there is no reference to hold (or to release)"
    );
}

/// A non-null object this runtime cannot keep alive is **refused**, and a refusal arms nothing: no
/// registration, no descriptor index entries, and the key counter untouched — so the poller was
/// never asked to register the descriptor and a later, keepable arming of the same descriptor still
/// succeeds.
///
/// The descriptor is a socketpair end rather than an inherited one: the sandbox refuses `fcntl` on
/// inherited descriptors (see `port/tests.rs`'s `armed_select`), while `UnixStream::pair` always
/// accepts.
#[test]
fn an_object_this_runtime_cannot_keep_is_refused_and_arms_nothing() {
    let (local, _peer) = std::os::unix::net::UnixStream::pair().expect("a socketpair");
    let fd = local.as_raw_fd();
    let mut poller = Poller::new().expect("a poller");
    let mut watch = SelectWatch::new();
    let refused = watch.arm(
        &mut poller,
        fd,
        Interest::READABLE,
        ProcessId::new(7, 0),
        SelectPayload::Plain {
            reference: make_small(22),
        },
        make_small(11),
        dangling_obj(),
    );
    assert!(refused.is_err(), "an unkeepable object must not arm an interest");
    assert!(!watch.is_armed(), "nothing was armed");
    assert!(watch.by_fd.is_empty(), "no descriptor index entry");
    assert!(watch.by_fd_handle.is_empty(), "no (fd, handle) index entry");
    assert_eq!(
        watch.next_key, TTY_KEY_TOP,
        "a refused arming consumes no poller key"
    );

    // The poller was never touched by the refusal: the same descriptor is still registrable.
    watch
        .arm(
            &mut poller,
            fd,
            Interest::READABLE,
            ProcessId::new(7, 0),
            SelectPayload::Plain {
                reference: make_small(22),
            },
            make_small(11),
            core::ptr::null_mut(),
        )
        .expect("the refused arming left the poller untouched");
    assert!(watch.is_armed());
    assert_eq!(watch.by_fd.get(&fd).copied(), Some(TTY_KEY_TOP));
}

/// The re-registration path refuses the same way and leaves the **existing** registration
/// completely undisturbed: same key, same interest, same handle, same object, same (absent) hold —
/// and the widened mask is never applied to the poll set.
#[test]
fn a_refused_re_registration_leaves_the_existing_registration_undisturbed() {
    let (mut poller, mut watch, owner, handle, reference) = armed();
    let before = &watch.registrations[&TTY_KEY_TOP];
    let (token, interest, obj) = (before.token, before.interest, before.obj);
    assert_eq!(watch.by_fd.len(), 1);

    let refused = watch.arm(
        &mut poller,
        TTY_IFD,
        Interest::WRITABLE,
        owner,
        SelectPayload::Custom {
            message: make_small(44),
        },
        make_small(33),
        dangling_obj(),
    );
    assert!(
        refused.is_err(),
        "an unkeepable object must not re-arm an interest"
    );
    assert!(watch.is_armed(), "the existing registration survives");
    assert_eq!(watch.registrations.len(), 1);
    assert_eq!(watch.by_fd.get(&TTY_IFD).copied(), Some(TTY_KEY_TOP));
    let after = &watch.registrations[&TTY_KEY_TOP];
    assert_eq!(after.token, token, "the poller token is unchanged");
    assert_eq!(after.interest, interest, "the widened mask was not applied");
    assert_eq!(after.handle, handle, "the handle is unchanged");
    assert_eq!(after.obj, obj, "the object is unchanged");
    assert!(after.obj.is_null(), "the tty arming carried no object");
    assert!(
        after.hold.is_none(),
        "so the existing registration still holds no reference"
    );
    match after.payload {
        SelectPayload::Plain { reference: carried } => assert_eq!(
            carried, reference,
            "the payload was not replaced by the refused call's"
        ),
        SelectPayload::Custom { .. } => panic!("the refused call must not replace the payload"),
    }
}

/// One reference per **registration**, so it travels with the first delivery of a readiness and the
/// later directions of the same readiness carry none. (The hold here is already released —
/// `held: false` — so no real object is needed to exercise the travel.)
#[test]
fn the_interest_reference_travels_with_the_first_delivery_only() {
    let (_poller, watch, owner, handle, reference) = armed();
    let registration = SelectRegistration {
        token: watch.registrations[&TTY_KEY_TOP].token,
        fd: TTY_IFD,
        interest: Interest::READABLE.add(Interest::ERROR),
        owner,
        payload: SelectPayload::Plain { reference },
        handle,
        obj: dangling_obj(),
        hold: Some(SelectHold {
            obj: dangling_obj(),
            held: false,
        }),
    };
    let delivered = deliveries_of(
        registration,
        Observed {
            readable: true,
            writable: false,
            error: true,
        },
    );
    assert_eq!(delivered.len(), 2, "the readable and the errored direction");
    assert!(
        delivered[0].hold.is_some(),
        "the interest's reference travels with the first delivery"
    );
    assert!(
        delivered[1].hold.is_none(),
        "the second direction of the same readiness carries no second reference"
    );
}
