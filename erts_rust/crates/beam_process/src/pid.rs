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

//! Process and port identity — bit-exact internal pid/port immediates.
//!
//! A pid or port is an [`Eterm`] immediate: the 60-bit payload splits into a
//! process-table **number** and a **serial** (`erl_term.h`: `_PID_NUM_SIZE` = 28,
//! `_PID_DATA_SIZE` = 60), so a reused table slot is distinguishable from the stale pid
//! that used to name it. The tagging half lives in `beam_term::immediate`
//! (`make_internal_pid`/`pid_number`/`pid_serial`); this module owns the typed handles the
//! rest of the runtime passes around, and delegates every bit operation to it — the layout
//! is frozen (workspace `../CONTEXT.md` §2) and is never re-derived here.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_term.h` (`make_pid`,
//! `pid_number`, `pid_serial`) and `erl_ptab.h` (the number space limits).

use beam_common::Result;
use beam_term::prelude::{
    is_internal_pid, is_internal_port, make_internal_pid, make_internal_port, make_pid_data,
    make_port_data, pid_number, pid_serial, port_number, Eterm,
};

use crate::error;

/// The largest process-table number (`PID_NUM_SIZE` = 28 bits worth).
pub const MAX_PROCESS_NUMBER: u64 = (1 << 28) - 1;

/// The largest process serial (`PID_SER_SIZE` = 32 bits worth, `2^32 - 1`).
pub const MAX_PROCESS_SERIAL: u64 = (1 << 32) - 1;

/// A typed process identifier.
///
/// `number` indexes the process table, `serial` disambiguates a reused slot; together they
/// encode bit-exactly into an internal pid immediate via
/// `make_internal_pid(make_pid_data(serial, number))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessId {
    number: u64,
    serial: u64,
}

impl ProcessId {
    /// Builds a pid handle from a process-table number and serial.
    pub const fn new(number: u64, serial: u64) -> ProcessId {
        ProcessId { number, serial }
    }

    /// The process-table number of this pid.
    pub const fn number(self) -> u64 {
        self.number
    }

    /// The serial of this pid.
    pub const fn serial(self) -> u64 {
        self.serial
    }

    /// Encodes this pid as its internal pid immediate (`make_internal_pid`).
    pub const fn to_eterm(self) -> Eterm {
        make_internal_pid(make_pid_data(self.serial, self.number))
    }

    /// Decodes an internal pid immediate, rejecting any other term with `badarg`.
    ///
    /// # Errors
    ///
    /// Returns [`BeamError::BadArg`] when `term` is not an internal pid.
    pub fn from_eterm(term: Eterm) -> Result<ProcessId> {
        if is_internal_pid(term) {
            Ok(ProcessId::new(pid_number(term), pid_serial(term)))
        } else {
            Err(error::bad_pid())
        }
    }

    /// Whether `term` is an internal pid (`is_internal_pid`).
    pub const fn is_process(term: Eterm) -> bool {
        is_internal_pid(term)
    }
}

/// A typed port identifier.
///
/// Ports spend their whole 60-bit payload on the number (`_PORT_DATA_SIZE`), so a `PortId`
/// is just that number; it encodes via `make_internal_port`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortId {
    number: u64,
}

impl PortId {
    /// Builds a port handle from a port-table number.
    pub const fn new(number: u64) -> PortId {
        PortId { number }
    }

    /// The port-table number of this port.
    pub const fn number(self) -> u64 {
        self.number
    }

    /// Encodes this port as its internal port immediate (`make_internal_port`).
    pub const fn to_eterm(self) -> Eterm {
        make_internal_port(make_port_data(self.number))
    }

    /// Decodes an internal port immediate, rejecting any other term with `badarg`.
    ///
    /// # Errors
    ///
    /// Returns [`BeamError::BadArg`] when `term` is not an internal port.
    pub fn from_eterm(term: Eterm) -> Result<PortId> {
        if is_internal_port(term) {
            Ok(PortId::new(port_number(term)))
        } else {
            Err(error::bad_pid())
        }
    }

    /// Whether `term` is an internal port (`is_internal_port`).
    pub const fn is_port(term: Eterm) -> bool {
        is_internal_port(term)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pid/port handles: encode/decode round-trips against
    //! `beam_term`'s own accessors, boundary fields, and the rejection of a term that is
    //! not an internal pid/port.

    use beam_common::BeamError;
    use beam_term::prelude::{make_atom, make_small, AtomTable};

    use super::*;

    #[test]
    fn pid_round_trips_through_its_immediate() {
        let pid = ProcessId::new(1234, 7);
        let term = pid.to_eterm();
        assert!(ProcessId::is_process(term));
        assert_eq!(ProcessId::from_eterm(term).expect("a valid pid decodes"), pid);
        // The number/serial halves land in the fields `beam_term` reads.
        assert_eq!(pid_number(term), 1234);
        assert_eq!(pid_serial(term), 7);
    }

    #[test]
    fn pid_boundary_fields_round_trip() {
        for (number, serial) in [
            (0u64, 0u64),
            (1, 1),
            (MAX_PROCESS_NUMBER, MAX_PROCESS_SERIAL),
        ] {
            let pid = ProcessId::new(number, serial);
            assert_eq!(
                ProcessId::from_eterm(pid.to_eterm()).expect("in range"),
                pid,
                "number {number}, serial {serial}"
            );
        }
    }

    #[test]
    fn pid_from_eterm_rejects_a_non_pid() {
        assert!(matches!(
            ProcessId::from_eterm(make_small(1)),
            Err(BeamError::BadArg)
        ));
        let mut atoms = AtomTable::new();
        let atom = make_atom(atoms.intern("not_a_pid"));
        assert!(matches!(ProcessId::from_eterm(atom), Err(BeamError::BadArg)));
        // A port is not a pid.
        assert!(matches!(
            ProcessId::from_eterm(PortId::new(5).to_eterm()),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn port_round_trips_through_its_immediate() {
        let port = PortId::new(0xDEAD_BEEF);
        let term = port.to_eterm();
        assert!(PortId::is_port(term));
        assert_eq!(PortId::from_eterm(term).expect("a valid port decodes"), port);
        assert_eq!(port_number(term), 0xDEAD_BEEF);
        assert!(!ProcessId::is_process(term));
    }

    #[test]
    fn port_from_eterm_rejects_a_non_port() {
        assert!(matches!(
            PortId::from_eterm(make_small(1)),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            PortId::from_eterm(ProcessId::new(1, 0).to_eterm()),
            Err(BeamError::BadArg)
        ));
    }
}
