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

//! End-to-end coverage for a **failing `start`** observed through
//! [`PortRegistry::open`](beam_io::PortRegistry::open), over the real, compiled C driver fixture
//! `tests/fixtures/host_driver.c` (workspace contract **A.4**, the frozen driver C ABI).
//!
//! The C's `ErlDrvData start(ErlDrvPort, char*)` has three failure **sentinels**
//! (`ERL_DRV_ERROR_GENERAL` / `_ERRNO` / `_BADARG`, `erl_driver.h:176-191`); a real driver returns
//! one to refuse an `open_port/2`. `DriverPort::start` maps all three onto the single typed
//! `BeamError::BadArg` (the C distinguishes them only by *rendering* the reason, which belongs to
//! the runtime — see the adapter's docs). That mapping is unit-tested
//! (`src/abi/port.rs::only_the_three_start_sentinels_are_failures`); this file is the missing
//! **end-to-end** half: a real `.so` returns each sentinel and the failure is observed through the
//! public open path.
//!
//! The fixture's `fail-start=<kind>` command token makes `host_start` return the matching sentinel
//! **before** it allocates its per-port state (a state built before the failure would leak: the host
//! runs the driver's `stop` with a null `ErlDrvData`). So the test can assert all three properties
//! end to end:
//!
//! 1. `PortRegistry::open` fails with the adapter's typed `BeamError::BadArg` (all three sentinels
//!    collapse to it);
//! 2. no port is left registered (the registry is still empty); and
//! 3. the id the failed open took is handed **back** to the id space — the next successful open gets
//!    that same number, which a leaked allocation could not do.
//!
//! # Skip-vs-fail policy
//!
//! Compiling the fixture needs a C compiler and the system OTP include directory (`erl_driver.h`);
//! when either is missing every test prints a **loud** `SKIP:` banner and returns (the
//! [`cdriver`](crate) helpers). A load failure is **never** a skip — it means the `driver_*` host
//! symbols are not in this test binary's dynamic symbol table, which is a wiring bug.

mod cdriver;

use beam_common::BeamError;
use beam_io::prelude::*;

use cdriver::*;

/// Each of the C's three `start` sentinels fails the open with the adapter's typed `badarg`, leaves
/// no port registered and releases the id it took.
///
/// The id-release assertion is the observable proof that a failed start does not leak: a fresh
/// registry hands out number `0`, so a leaked allocation would make the next successful open number
/// `1`.
#[test]
fn the_three_start_sentinels_fail_the_open_and_release_the_id() {
    let Some(so) = fixture_so() else {
        return;
    };

    for kind in ["general", "errno", "badarg"] {
        // The command carries the fixture's `fail-start=<kind>` token; the log path is only there to
        // prove that **no** callback of the failed start ran (see the assertion below).
        let probe = logged_port_command(&format!("fail-start-{kind}"));
        let command = format!("{} fail-start={kind}", probe.command);

        let host = TestPortHost::new().0;
        let driver = load_driver(&so, &command, Box::new(host));
        let mut poller = Poller::new().expect("a poll set");
        let mut registry = PortRegistry::new();

        let err = registry
            .open(&mut poller, Box::new(driver))
            .expect_err("a start sentinel must fail the open");

        // All three C sentinels collapse to the adapter's single typed `badarg` (the C renders
        // `{einval,…}`/`{Errno,…}`/`{badarg,…}`, a runtime concern this crate documents it drops).
        assert!(
            matches!(err, BeamError::BadArg),
            "`{kind}` must be the typed badarg, got {err:?}"
        );
        assert_eq!(err.as_atom(), Some("badarg"));

        // No port is left registered.
        assert!(
            registry.is_empty(),
            "a failed start must not leave a port registered (`{kind}`)"
        );
        assert_eq!(registry.len(), 0);

        // The id the failed open took came back: a fresh registry allocates `0`, so the next
        // successful open gets `0` again exactly when the failed open's number was released.
        let host = TestPortHost::new().0;
        let ok = load_driver(&so, "", Box::new(host));
        let id = registry
            .open(&mut poller, Box::new(ok))
            .expect("a command without a `fail-start` token opens normally");
        assert_eq!(
            id.number(),
            0,
            "the failed open's id was released back to the id space, not leaked (`{kind}`)"
        );
        registry.close(&mut poller, id).expect("the port closes");

        // The sentinel is returned before the fixture allocates its state, so neither `start`'s log
        // line nor a `stop` for a state that was never built exists: the log file was never created.
        assert!(
            log_lines(&probe.log).is_empty(),
            "a failed start runs no driver callback and leaves no state behind (`{kind}`), \
             but the log holds {:?}",
            log_lines(&probe.log)
        );
    }
}

/// `fail-start=<kind>` with an unknown spelling is **not** a failure: only the exact three spellings
/// the C's sentinels cover fail an open, so the port starts normally (a boundary case that pins the
/// token's vocabulary rather than a prefix match).
#[test]
fn an_unknown_fail_start_kind_does_not_fail_the_start() {
    let Some(so) = fixture_so() else {
        return;
    };
    let probe = logged_port_command("fail-start-unknown");
    let command = format!("{} fail-start=bogus", probe.command);

    let host = TestPortHost::new().0;
    let driver = load_driver(&so, &command, Box::new(host));
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();

    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("an unknown `fail-start` kind is ignored, so the port opens");
    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string()],
        "the driver's start callback ran normally"
    );

    registry.close(&mut poller, id).expect("the port closes");
}
