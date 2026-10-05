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

use beam_common::{BeamError, Result};
use beam_term::prelude::{
    is_internal_pid, is_internal_port, make_internal_pid, make_internal_port, make_pid_data,
    pid_number, pid_serial, Eterm,
};

/// The largest process-table number (`PID_NUM_SIZE` = 28 bits worth).
pub const MAX_PROCESS_NUMBER: u64 = (1 << 28) - 1;

/// The largest process serial (`PID_SER_SIZE` = 32 bits worth, `2^32 - 1`).
pub const MAX_PROCESS_SERIAL: u64 = (1 << 32) - 1;

// NOTE: `const fn` bodies cannot use the formatting form of `unimplemented!` (its
// `format_args!` is not const-evaluable), so the message-less `unimplemented!()` form is
// used for the `const fn`s below.

/// A typed process identifier.
///
/// `number` indexes the process table, `serial` disambiguates a reused slot; together they
/// encode bit-exactly into an internal pid immediate via
/// `make_internal_pid(make_pid_data(serial, number))`.
//
// TODO(impl): implement `ProcessId` (all bodies are stubs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessId {
    number: u64,
    serial: u64,
}

impl ProcessId {
    /// Builds a pid handle from a process-table number and serial.
    pub const fn new(number: u64, serial: u64) -> ProcessId {
        unimplemented!()
    }

    /// The process-table number of this pid.
    pub const fn number(self) -> u64 {
        unimplemented!()
    }

    /// The serial of this pid.
    pub const fn serial(self) -> u64 {
        unimplemented!()
    }

    /// Encodes this pid as its internal pid immediate (`make_internal_pid`).
    pub const fn to_eterm(self) -> Eterm {
        unimplemented!()
    }

    /// Decodes an internal pid immediate, rejecting any other term with `badarg`.
    ///
    /// # Errors
    ///
    /// Returns [`BeamError::BadArg`] when `term` is not an internal pid.
    pub fn from_eterm(term: Eterm) -> Result<ProcessId> {
        unimplemented!("stub — to be implemented")
    }

    /// Whether `term` is an internal pid (`is_internal_pid`).
    pub const fn is_process(term: Eterm) -> bool {
        unimplemented!()
    }
}

/// A typed port identifier.
///
/// Ports spend their whole 60-bit payload on the number (`_PORT_DATA_SIZE`), so a `PortId`
/// is just that number; it encodes via `make_internal_port`.
//
// TODO(impl): implement `PortId` (all bodies are stubs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortId {
    number: u64,
}

impl PortId {
    /// Builds a port handle from a port-table number.
    pub const fn new(number: u64) -> PortId {
        unimplemented!()
    }

    /// The port-table number of this port.
    pub const fn number(self) -> u64 {
        unimplemented!()
    }

    /// Encodes this port as its internal port immediate (`make_internal_port`).
    pub const fn to_eterm(self) -> Eterm {
        unimplemented!()
    }

    /// Decodes an internal port immediate, rejecting any other term with `badarg`.
    ///
    /// # Errors
    ///
    /// Returns [`BeamError::BadArg`] when `term` is not an internal port.
    pub fn from_eterm(term: Eterm) -> Result<PortId> {
        unimplemented!("stub — to be implemented")
    }

    /// Whether `term` is an internal port (`is_internal_port`).
    pub const fn is_port(term: Eterm) -> bool {
        unimplemented!()
    }
}
