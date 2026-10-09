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

//! The byte-source seam: the device's `read`.
//!
//! `file_io_server` reads through `?CALL_FD(Handle, read, [Size])`, which answers
//! `{ok, Bin} | eof | {error, Reason}`. [`ByteSource`] is that operation term-free:
//! `Ok(Some(bytes))`, `Ok(None)` and `Err`.
//!
//! The read size the loop asks for is [`read_size`](super::read_size) — `8 KiB` for a
//! `binary`-mode device and `128` bytes for a `list`-mode one
//! (`file_io_server.erl:39-40,815-818`). A source need not fill the request: a short read
//! is normal, and the loop simply reads again (which is what makes the incremental
//! continuation protocol work).

use beam_common::Result;

/// A device's byte source — the `read` half of an io device's handle.
///
/// Implementations must be *streaming*: successive calls return successive bytes.
/// `Ok(None)` reports end of file, and it must be **sticky** — the continuation protocol can
/// ask for more data after an `eof` (a `{more, Cont}` handed the atom `eof` makes the loop
/// read again), so a source that has answered `Ok(None)` has to keep answering `Ok(None)`
/// rather than restarting or failing.
///
/// # Errors
///
/// A failing read is returned as the workspace [`BeamError::Io`](beam_common::BeamError::Io),
/// which the loop propagates — the oracle's `{error, Reason}` branch, which stops the
/// request rather than fabricating data.
pub trait ByteSource {
    /// Reads **up to** `size` bytes.
    ///
    /// `Ok(None)` reports end of file; `Ok(Some(bytes))` reports `bytes` (possibly fewer
    /// than `size`, possibly empty).
    fn read_chunk(&mut self, size: usize) -> Result<Option<Vec<u8>>>;
}

impl ByteSource for std::fs::File {
    fn read_chunk(&mut self, size: usize) -> Result<Option<Vec<u8>>> {
        if size == 0 {
            return Ok(Some(Vec::new()));
        }
        let mut buf = vec![0u8; size];
        loop {
            match std::io::Read::read(self, &mut buf) {
                // A zero-length read on a file is the only end-of-file signal `read(2)`
                // gives.
                Ok(0) => return Ok(None),
                Ok(read) => {
                    buf.truncate(read);
                    return Ok(Some(buf));
                }
                // `read(2)` is not restarted after a signal; retry here rather than
                // surfacing a spurious failure (the same rule `Poller::poll` follows).
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(crate::error::io(err)),
            }
        }
    }
}

/// A [`ByteSource`] over an in-memory slice.
///
/// Useful for tests and for a device whose contents are already resident; it is the
/// term-free analogue of reading a binary with a cursor.
#[derive(Clone, Debug)]
pub struct SliceSource<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> SliceSource<'a> {
    /// Builds a source over `data`, positioned at its first byte.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    /// The number of bytes not yet handed out.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.position
    }

    /// `true` once every byte has been handed out.
    pub fn is_empty(&self) -> bool {
        self.position >= self.data.len()
    }
}

impl ByteSource for SliceSource<'_> {
    fn read_chunk(&mut self, size: usize) -> Result<Option<Vec<u8>>> {
        if self.position >= self.data.len() {
            return Ok(None);
        }
        let end = self.position.saturating_add(size).min(self.data.len());
        let chunk = self.data[self.position..end].to_vec();
        self.position = end;
        Ok(Some(chunk))
    }
}

/// A [`ByteSource`] that is always empty — the `eof`-immediately device the loop's
/// end-of-file path is tested against.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptySource;

impl ByteSource for EmptySource {
    fn read_chunk(&mut self, _size: usize) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slice_source_hands_out_successive_chunks() {
        let mut source = SliceSource::new(b"abcdef");
        assert_eq!(source.remaining(), 6);
        assert_eq!(source.read_chunk(4).expect("read"), Some(b"abcd".to_vec()));
        assert_eq!(source.remaining(), 2);
        assert_eq!(source.read_chunk(4).expect("read"), Some(b"ef".to_vec()));
        assert!(source.is_empty());
        assert_eq!(source.read_chunk(4).expect("read"), None);
        // The end of file is sticky.
        assert_eq!(source.read_chunk(4).expect("read"), None);
    }

    #[test]
    fn an_empty_slice_source_reports_eof_immediately() {
        let mut source = SliceSource::new(b"");
        assert!(source.is_empty());
        assert_eq!(source.read_chunk(128).expect("read"), None);
    }

    #[test]
    fn the_empty_source_is_eof() {
        assert_eq!(EmptySource.read_chunk(8).expect("read"), None);
    }

    #[test]
    fn a_real_file_is_a_byte_source() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("beam_io_io_device_source");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("src.txt");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(b"hello world").expect("write");
        drop(file);

        let mut file = std::fs::File::open(&path).expect("open");
        assert_eq!(file.read_chunk(5).expect("read"), Some(b"hello".to_vec()));
        assert_eq!(file.read_chunk(64).expect("read"), Some(b" world".to_vec()));
        assert_eq!(file.read_chunk(64).expect("read"), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_read_error_is_a_typed_io_failure() {
        // A directory cannot be read as a byte stream.
        let mut dir = std::fs::File::open(std::env::temp_dir()).expect("open dir");
        let error = dir.read_chunk(8).expect_err("a directory is not readable");
        assert!(matches!(error, beam_common::BeamError::Io(_)), "{error:?}");
    }
}
