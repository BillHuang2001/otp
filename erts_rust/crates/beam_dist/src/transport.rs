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

//! The abstract transport seam: byte-oriented `Read`/`Write` traits plus an
//! in-memory full-duplex test double.
//!
//! This crate is **pure protocol/value code** and never opens a socket; actual
//! I/O is `beam_io`'s job and `beam_rt` drives node startup. Everything here is
//! generic over these two tiny traits, so the whole distribution stack is
//! exercised hermetically against [`MemoryTransport`].
//!
//! Errors: a transport failure maps to `BeamError::Io`; a short read / end of the
//! input buffer maps to `BeamError::BadArg` (never a panic).

use beam_common::{BeamError, Result};

/// The read half of a transport: whole-buffer reads and the big-endian readers
/// the distribution wire needs (every multi-byte field is big-endian).
pub trait Read {
    /// Reads exactly `buf.len()` bytes, filling `buf`.
    ///
    /// A short read (end of stream before `buf` is full) yields
    /// `BeamError::BadArg`; an underlying I/O failure yields `BeamError::Io`.
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()>;

    /// Reads a single byte.
    fn read_u8(&mut self) -> Result<u8> {
        let mut buf = [0u8; 1];
        self.read_exact(&mut buf)?;
        Ok(buf[0])
    }

    /// Reads a big-endian `u16`.
    fn read_be_u16(&mut self) -> Result<u16> {
        let mut buf = [0u8; 2];
        self.read_exact(&mut buf)?;
        Ok(u16::from_be_bytes(buf))
    }

    /// Reads a big-endian `u32`.
    fn read_be_u32(&mut self) -> Result<u32> {
        let mut buf = [0u8; 4];
        self.read_exact(&mut buf)?;
        Ok(u32::from_be_bytes(buf))
    }

    /// Reads a big-endian `u64`.
    fn read_be_u64(&mut self) -> Result<u64> {
        let mut buf = [0u8; 8];
        self.read_exact(&mut buf)?;
        Ok(u64::from_be_bytes(buf))
    }
}

/// The write half of a transport: whole-buffer writes and the big-endian writers
/// the distribution wire needs.
pub trait Write {
    /// Writes all of `buf`.
    fn write_all(&mut self, buf: &[u8]) -> Result<()>;

    /// Flushes any buffered output.
    fn flush(&mut self) -> Result<()>;

    /// Writes a single byte.
    fn write_u8(&mut self, value: u8) -> Result<()> {
        self.write_all(&[value])
    }

    /// Writes a big-endian `u16`.
    fn write_u16be(&mut self, value: u16) -> Result<()> {
        self.write_all(&value.to_be_bytes())
    }

    /// Writes a big-endian `u32`.
    fn write_u32be(&mut self, value: u32) -> Result<()> {
        self.write_all(&value.to_be_bytes())
    }

    /// Writes a big-endian `u64`.
    fn write_u64be(&mut self, value: u64) -> Result<()> {
        self.write_all(&value.to_be_bytes())
    }
}

/// A transport: something that can both read and write bytes.
pub trait Transport: Read + Write {}

impl<T: Read + Write + ?Sized> Transport for T {}

/// A full-duplex in-memory transport used by every hermetic test.
///
/// Reads are served from a caller-supplied input buffer; writes are appended to
/// an output buffer the test inspects with [`written`](MemoryTransport::written).
#[derive(Debug, Default, Clone)]
pub struct MemoryTransport {
    /// Bytes available to be read.
    input: Vec<u8>,
    /// The read cursor into `input`.
    pos: usize,
    /// Bytes that have been written.
    output: Vec<u8>,
}

impl MemoryTransport {
    /// A transport with no input and an empty output buffer.
    pub fn new() -> MemoryTransport {
        MemoryTransport { input: Vec::new(), pos: 0, output: Vec::new() }
    }

    /// A transport that will serve `input` on the read side.
    pub fn with_input(input: Vec<u8>) -> MemoryTransport {
        MemoryTransport { input, pos: 0, output: Vec::new() }
    }

    /// The bytes written so far.
    pub fn written(&self) -> &[u8] {
        &self.output
    }

    /// Consumes the transport, returning the bytes written.
    pub fn into_written(self) -> Vec<u8> {
        self.output
    }

    /// The number of input bytes not yet read.
    pub fn input_remaining(&self) -> usize {
        self.input.len().saturating_sub(self.pos)
    }
}

impl Read for MemoryTransport {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        // A short read (end of buffer before `buf` is full) is a `BadArg` and
        // must not consume any input: compute the end offset first and only
        // advance `pos` once the whole read can succeed.
        let end = self.pos.checked_add(buf.len()).ok_or(BeamError::BadArg)?;
        if end > self.input.len() {
            return Err(BeamError::BadArg);
        }
        buf.copy_from_slice(&self.input[self.pos..end]);
        self.pos = end;
        Ok(())
    }
}

impl Write for MemoryTransport {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.output.extend_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        // An in-memory buffer never needs flushing.
        Ok(())
    }
}
