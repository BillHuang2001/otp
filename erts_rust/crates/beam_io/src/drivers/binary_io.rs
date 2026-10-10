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

//! The ERTS port **phase change** between text/list framing and raw binary framing.
//!
//! A port's input can be delivered to its owner in two "phases", mirroring the emulator's
//! `ERTS_PORT_SFLG_BINARY_IO` flag (`erts/emulator/beam/erl_port.h`; the `binary`/`line`
//! port commands flip it): either every chunk crosses the port boundary as an unbroken
//! blob of bytes (the *binary* phase) or the driver frames the stream into lines and each
//! line is delivered on its own (the *text*/*list* phase). The flag is what the `efile`,
//! `inet` and `erl_child_setup` drivers use to switch a port from message/list delivery to
//! raw binary delivery once a protocol header has been read.
//!
//! [`PhaseChange`] models that switch. It owns a [`LineBuffer`] (the `{line, N}` framing,
//! see [`crate::drivers::line_buffer`]) and applies it only in [`Phase::Text`]; in
//! [`Phase::Binary`] input passes through unchanged. The **default phase is
//! [`Phase::Binary`]**, so a driver that never touches the state machine behaves exactly
//! as a raw byte-stream driver.
//!
//! Switching phase does **not** flush or discard buffered input: a partial line held in
//! the text phase survives a switch to binary and is delivered by the next
//! [`PhaseChange::flush`] (or by a later switch back to text), which keeps the bytes a
//! driver has already read from disappearing.

use crate::drivers::line_buffer::{LineBuffer, DEFAULT_MAX_LINE_LEN};

/// The framing phase a port is currently in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Text/list phase: input is framed into lines (the `{line, N}` mode).
    Text,
    /// Raw binary phase: input passes through as unbroken bytes (the default).
    Binary,
}

impl Phase {
    /// Whether this is the text/list phase.
    pub fn is_text(self) -> bool {
        matches!(self, Phase::Text)
    }

    /// Whether this is the raw binary phase.
    pub fn is_binary(self) -> bool {
        matches!(self, Phase::Binary)
    }

    /// The phase as the token used in the [`PhaseChange`] options reply (`"text"` /
    /// `"binary"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Text => "text",
            Phase::Binary => "binary",
        }
    }
}

/// The port input-framing state machine: a [`Phase`] plus the [`LineBuffer`] used in the
/// text phase.
///
/// Created in [`Phase::Binary`] ([`PhaseChange::new`]); call [`PhaseChange::to_text`] (or
/// [`PhaseChange::set`]) to enable line framing. [`PhaseChange::handle`] is fed every
/// chunk the driver reads from its fd and returns the frames to deliver upward;
/// [`PhaseChange::flush`] is called at EOF/`stop` to release a trailing partial line.
pub struct PhaseChange {
    /// The current phase.
    phase: Phase,
    /// The line accumulator: filled in [`Phase::Text`], drained by [`PhaseChange::flush`]
    /// whatever the phase then is.
    line: LineBuffer,
}

impl PhaseChange {
    /// A phase machine in the default [`Phase::Binary`] phase, with the default maximum
    /// line length ([`DEFAULT_MAX_LINE_LEN`]) for a later switch to text.
    pub fn new() -> PhaseChange {
        PhaseChange {
            phase: Phase::Binary,
            line: LineBuffer::new(DEFAULT_MAX_LINE_LEN),
        }
    }

    /// A phase machine in the default [`Phase::Binary`] phase with a custom maximum line
    /// length (`0` = unlimited) for a later switch to text.
    pub fn with_max_line_len(max_len: usize) -> PhaseChange {
        PhaseChange {
            phase: Phase::Binary,
            line: LineBuffer::new(max_len),
        }
    }

    /// The current phase.
    pub fn current(&self) -> Phase {
        self.phase
    }

    /// Sets the current phase (without flushing buffered input).
    pub fn set(&mut self, phase: Phase) {
        self.phase = phase;
    }

    /// Switches to the raw binary phase.
    pub fn to_binary(&mut self) {
        self.phase = Phase::Binary;
    }

    /// Switches to the text/list (line-framed) phase.
    pub fn to_text(&mut self) {
        self.phase = Phase::Text;
    }

    /// The maximum line length used in the text phase (`0` = unlimited).
    pub fn max_line_len(&self) -> usize {
        self.line.max_len()
    }

    /// Sets the maximum line length (`0` = unlimited).
    pub fn set_max_line_len(&mut self, max_len: usize) {
        self.line.set_max_len(max_len);
    }

    /// The number of input bytes held back from delivery by text-phase framing.
    pub fn pending(&self) -> usize {
        self.line.pending()
    }

    /// The underlying line accumulator (used in the text phase).
    pub fn line_buffer(&self) -> &LineBuffer {
        &self.line
    }

    /// Feeds `data` through the current phase and returns the frames to deliver.
    ///
    /// In [`Phase::Binary`] the whole chunk is one frame (empty input yields no frames);
    /// in [`Phase::Text`] the chunk is accumulated and every complete line becomes a
    /// frame, with any trailing partial line held back for a later call or a
    /// [`PhaseChange::flush`].
    pub fn handle(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        match self.phase {
            Phase::Binary => {
                if data.is_empty() {
                    Vec::new()
                } else {
                    vec![data.to_vec()]
                }
            }
            Phase::Text => {
                self.line.push(data);
                self.line.take_lines()
            }
        }
    }

    /// Releases any input held back by text-phase framing at EOF/`stop`.
    ///
    /// This drains the line buffer **regardless of the current phase**: switching to
    /// [`Phase::Binary`] deliberately does not discard buffered input (see the module
    /// docs above), so bytes a driver already read in the text phase must still reach the
    /// port input queue. In [`Phase::Binary`] the line buffer is empty unless it was filled
    /// while the phase was [`Phase::Text`], so a driver that never used text framing gets
    /// no frames.
    pub fn flush(&mut self) -> Vec<Vec<u8>> {
        self.line.flush_all()
    }
}

impl Default for PhaseChange {
    fn default() -> Self {
        PhaseChange::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{Phase, PhaseChange};

    #[test]
    fn the_default_phase_is_raw_binary() {
        let mut pc = PhaseChange::new();
        assert_eq!(pc.current(), Phase::Binary);
        assert!(pc.current().is_binary());
        assert!(!pc.current().is_text());
        // Raw pass-through: the whole chunk is one frame, terminator or not.
        assert_eq!(pc.handle(b"a\nb\n"), vec![b"a\nb\n".to_vec()]);
        assert!(pc.flush().is_empty());
        assert_eq!(pc.pending(), 0);
    }

    #[test]
    fn binary_handle_of_empty_input_yields_no_frames() {
        let mut pc = PhaseChange::new();
        assert!(pc.handle(b"").is_empty());
    }

    #[test]
    fn switching_to_text_frames_input_into_lines() {
        let mut pc = PhaseChange::new();
        pc.to_text();
        assert_eq!(pc.current(), Phase::Text);
        assert_eq!(
            pc.handle(b"one\ntwo\n"),
            vec![b"one".to_vec(), b"two".to_vec()]
        );
    }

    #[test]
    fn text_holds_a_partial_line_until_flush() {
        let mut pc = PhaseChange::new();
        pc.to_text();
        assert!(pc.handle(b"partial").is_empty());
        assert_eq!(pc.pending(), 7);
        assert_eq!(pc.handle(b" more\n"), vec![b"partial more".to_vec()]);
        assert!(pc.handle(b"trailing").is_empty());
        assert_eq!(pc.flush(), vec![b"trailing".to_vec()]);
    }

    #[test]
    fn text_honours_the_max_line_length() {
        let mut pc = PhaseChange::with_max_line_len(3);
        pc.to_text();
        assert_eq!(pc.max_line_len(), 3);
        assert_eq!(
            pc.handle(b"abcdef\n"),
            vec![b"abc".to_vec(), b"def".to_vec()]
        );
        pc.set_max_line_len(2);
        assert_eq!(pc.handle(b"xy\n"), vec![b"xy".to_vec()]);
    }

    #[test]
    fn a_round_trip_through_binary_and_back_keeps_buffered_bytes() {
        let mut pc = PhaseChange::new();
        pc.to_text();
        assert!(pc.handle(b"abc").is_empty(), "held in the line buffer");
        pc.to_binary();
        // Binary pass-through leaves the held partial untouched…
        assert_eq!(pc.handle(b"RAW"), vec![b"RAW".to_vec()]);
        assert_eq!(pc.pending(), 3, "the text-phase partial survives");
        // …and it is released by a flush regardless of the current phase switch-back.
        pc.to_text();
        assert_eq!(pc.flush(), vec![b"abc".to_vec()]);
        assert_eq!(pc.pending(), 0);
    }

    #[test]
    fn flush_after_switching_to_binary_still_delivers_the_held_partial_line() {
        let mut pc = PhaseChange::new();
        pc.to_text();
        assert!(pc.handle(b"partial").is_empty(), "held in the line buffer");
        assert_eq!(pc.pending(), 7);
        pc.to_binary();
        assert_eq!(pc.current(), Phase::Binary);
        // The switch must not lose the bytes already read in the text phase: `flush`
        // drains the line buffer whatever the current phase is (the `FdDriver` EOF/`stop`
        // path, where a `binary`/`noline` control command may have flipped the phase in
        // between).
        assert_eq!(pc.flush(), vec![b"partial".to_vec()]);
        assert_eq!(pc.pending(), 0);
        // A second flush finds nothing.
        assert!(pc.flush().is_empty());
    }

    #[test]
    fn set_moves_between_phases() {
        let mut pc = PhaseChange::new();
        pc.set(Phase::Text);
        assert!(pc.current().is_text());
        pc.set(Phase::Binary);
        assert!(pc.current().is_binary());
    }

    #[test]
    fn phases_name_themselves_for_the_options_reply() {
        assert_eq!(Phase::Text.as_str(), "text");
        assert_eq!(Phase::Binary.as_str(), "binary");
    }
}
