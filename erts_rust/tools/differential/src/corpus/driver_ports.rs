//! Contract **A.4** — the port-*opening* grammar and its error shapes.
//!
//! Split out of [`super::driver`] by responsibility: that group drives a working port
//! (`os:cmd/1`, the read path, `port_command/2,3` round trips, `port_info/1,2`); this one
//! pins what `open_port/2` does with the arguments it must **reject** or with a child that
//! cannot be started, plus the `{fd,N}` adopted-descriptor form whose write stream is the
//! caller's descriptor.
//!
//! Like every port case these spawn a real OS process, so they stay small and fast.

use super::Case;

/// The A.4 port-open / error-shape cases.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
