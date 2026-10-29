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
//! The `prim_file` **read-ahead queue**: `read_line/1` and the two entries it couples to.
//!
//! `prim_file.erl` builds a `prim_buffer` queue per read-shaped descriptor (`build_fd_data/5`'s
//! `r_buffer => prim_buffer:new()`, `prim_file.erl:517`) and keeps it on the descriptor's data
//! map. Only `read_line/1` ever *fills* it — it is the one entry that reads more than it returns:
//! `read_line_1/4` (`:225-245`) reads `max(?MIN_READLINE_SIZE, r_ahead_size)` byte chunks into the
//! queue until a `\n` appears. Two sibling entries must therefore *respect* it, or they would
//! hand back bytes the queue has already consumed:
//!
//! * `read/2` — `read_1/5` (`:180-204`) answers from the queue while it holds enough, and
//!   concatenates a partial queue with a fresh host read otherwise;
//! * `position/2` — `position_1/3` (`:327-334`) wipes the queue, and `position/2`'s
//!   `{cur, Offset}` head (`:307-311`) first shifts the offset back by the queued byte count
//!   (the host cursor runs *ahead* of the logical position by exactly that much).
//!
//! Those two live next to their siblings in [`super`] (they are one entry each); this module owns
//! the queue itself, `read_line/1`, and the four queue operations the three entries share.
//!
//! # Why `read_line/1` is served here rather than by entering `prim_file.beam`
//!
//! `prim_file` is one of [`super::super::FLOOR_MODULES`]' wildcard namespaces: a cross-module call
//! to *any* `prim_file:*` is refused in-machine, so the preloaded module's own body can never run.
//! `read/2`, `write/2` and `position/2` are native for that reason too — they are the module's NIF
//! surface reimplemented on the host, not the Erlang bodies. `read_line/1` is the same case: the
//! body above is reproduced on the queue [`super::super::prim_buffer`] already owns.
//!
//! # Ground truth (measured against the oracle)
//!
//! `read_line/1` answers `{ok, Binary}` with the line **including** its `\n`, translating a
//! `\r\n` terminator to `\n` (`:239-245`); a drained descriptor answers `eof`; a final line with
//! no terminator still answers `{ok, Binary}` and the next call is `eof`; a **write**-shaped
//! descriptor is `{error,badarg}` (`get_fd_data_for_read/1` throws it, `:168-171`); a closed one is
//! `{error,einval}` (the C's `efile` state check fires before the wrapper's data lookup).
//!
//! # Documented divergences
//!
//! * **A term that is not a `#file_descriptor{}` is the value `{error,badarg}`** — the uniform
//!   malformed-descriptor divergence of the whole `prim_file` floor (`super`'s *Documented
//!   divergences*).
//! * **The queue is keyed by the descriptor's id, and the descriptor's `r_buffer` entry is that
//!   same reference** — the oracle mints a second reference for the queue (`prim_buffer:new/0`).
//!   One table key is what makes `close/1` unable to leak either half (`super::build_descriptor`).
//! * **`read_ahead` sizes the chunk, not the whole read.** This runtime never over-reads on
//!   `read/2`'s behalf (`prim_read` is a plain positioned host read), so `read_ahead` only changes
//!   how many bytes `read_line/1` asks for — which no answer or position can observe.
use beam_common::{BeamError, Result};
use beam_term::Eterm;
use super::RuntimeContext;
/// `?MIN_READLINE_SIZE` — the smallest chunk `read_line_1/4` reads (`prim_file.erl:62`).
const MIN_READLINE_SIZE: usize = 256;
impl RuntimeContext<'_> {
    // -- prim_file:read_line/1 -------------------------------------------------
    /// `prim_file:read_line/1` — one line as `{ok, Binary}`, or `eof`
    /// (`prim_file.erl:206-245`).
    ///
    /// `read_line_1/4`'s loop, against the descriptor's queue: search for `\n`, and on a miss read
    /// one `max(?MIN_READLINE_SIZE, r_ahead_size)` chunk off the descriptor's host file into the
    /// queue and search again. A `\r\n` terminator answers with just the `\n`; a drained host file
    /// answers whatever the queue holds, or `eof` when it holds nothing.
    ///
    /// Oracle: a 3-line file ⇒ `{ok,<<"alpha\n">>}`, `{ok,<<"beta\n">>}`, `{ok,<<"\n">>}`,
    /// `{ok,<<"gamma">>}`, then `eof`; `position(Fd,cur)` between two reads names the byte after
    /// the line just answered (the queue is invisible to it); a `\r\n` file ⇒ every line ends in a
    /// bare `\n`.
    pub(crate) fn prim_read_line(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let fd = *args.first().ok_or(BeamError::BadArg)?;
        let Some(descriptor) = self.decode_descriptor(fd) else {
            return self.error_tuple("badarg");
        };
        self.check_owner(&descriptor)?;
        // The C's `efile` state check fires before anything else, so a closed descriptor is
        // `{error,einval}` — `prim_close/1` drops the queue and the host file together.
        if !self.runtime.open_files_mut().contains_key(&descriptor.id) {
            return self.error_tuple("einval");
        }
        // `get_fd_data_for_read/1` (`prim_file.erl:526-535`): a descriptor without a read-ahead
        // queue (the **write** shape) throws `badarg`, which the wrapper turns into the value
        // `{error,badarg}`.
        if !self.has_readahead(descriptor.id) {
            return self.error_tuple("badarg");
        }
        let chunk = MIN_READLINE_SIZE.max(descriptor.read_ahead);
        loop {
            if let Some(index) = self.find_in_readahead(descriptor.id, b'\n') {
                let line = self.take_readahead(descriptor.id, index + 1);
                return self.line_answer(line);
            }
            // `read_nif(FRef, LineSize)` (`prim_file.erl:226`).
            let outcome = match self.runtime.open_files_mut().get_mut(&descriptor.id) {
                None => None,
                Some(file) => Some(super::read_extent(file, chunk)),
            };
            let Some(read) = outcome else {
                return self.error_tuple("einval");
            };
            match read {
                Ok(bytes) if bytes.is_empty() => {
                    // `eof` (`prim_file.erl:231-235`): what the queue holds, else `eof`.
                    let size = self.readahead_size(descriptor.id);
                    if size == 0 {
                        return self.atom("eof");
                    }
                    let line = self.take_readahead(descriptor.id, size);
                    return self.line_answer(line);
                }
                // `prim_buffer:write(RBuf, [Data])` then search again (`:227-230`). A non-empty
                // host read always advances the queue, so the loop terminates.
                Ok(bytes) => self.append_readahead(descriptor.id, &bytes),
                Err(error) => return self.error_tuple_for(&error),
            }
        }
    }
    /// The bytes `descriptor`'s read-ahead queue holds, `0` when it has none.
    pub(super) fn readahead_size(&mut self, id: u64) -> usize {
        self.runtime
            .buffers_mut()
            .get_mut(&id)
            .map_or(0, |buffer| buffer.size())
    }
    /// Whether `id` names a live read-ahead queue — i.e. whether the descriptor was opened with
    /// the **read** shape (`build_fd_data/5`'s `r_buffer` entry).
    fn has_readahead(&mut self, id: u64) -> bool {
        self.runtime.buffers_mut().contains_key(&id)
    }
    /// Drop `count` bytes from the front of the queue and hand them back — `read_1/5`'s
    /// `prim_buffer:read/2` (`prim_file.erl:181-183`), a `copying_read` in the C.
    ///
    /// Every caller checks the size first, so a `None` (a count past the end) is unreachable and
    /// degrades to an empty payload rather than a fabricated one.
    pub(super) fn take_readahead(&mut self, id: u64, count: usize) -> Vec<u8> {
        self.runtime
            .buffers_mut()
            .get_mut(&id)
            .and_then(|buffer| buffer.copying_read(count))
            .unwrap_or_default()
    }
    /// Append one host read's bytes to the queue (`prim_buffer:write/2`, `prim_file.erl:228`).
    fn append_readahead(&mut self, id: u64, bytes: &[u8]) {
        if let Some(buffer) = self.runtime.buffers_mut().get_mut(&id) {
            buffer.write_iovec(&[bytes]);
        }
    }
    /// The offset of the first `needle` in the queue (`prim_buffer:find_byte_index/2`,
    /// `prim_file.erl:211`), `None` when it holds no such byte.
    fn find_in_readahead(&mut self, id: u64, needle: u8) -> Option<usize> {
        self.runtime
            .buffers_mut()
            .get_mut(&id)
            .and_then(|buffer| buffer.find_byte(needle))
    }
    /// Empty the queue — `position_1/3`'s `prim_buffer:wipe/1` (`prim_file.erl:330`), which is
    /// `skip(RBuf, size(RBuf))` (`prim_buffer.erl`).
    pub(super) fn wipe_readahead(&mut self, id: u64) {
        if let Some(buffer) = self.runtime.buffers_mut().get_mut(&id) {
            let size = buffer.size();
            buffer.skip(size);
        }
    }
    /// `read_line_1/4`'s last clause: answer the dequeued line, translating a `\r\n` terminator
    /// into a bare `\n` (`prim_file.erl:239-245`).
    fn line_answer(&mut self, mut line: Vec<u8>) -> Result<Eterm> {
        let len = line.len();
        if len >= 2 && line[len - 2] == b'\r' && line[len - 1] == b'\n' {
            line.remove(len - 2);
        }
        // The same long-lived `-eval`-heap binary builder `read/2` uses, so a line longer than the
        // 64-byte on-heap limit is representable.
        let contents = beam_codec::term::make_binary(self.eval_heap(), &line)?;
        self.ok_tuple(contents)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::test_support::scratch_dir;
    use crate::native::test_support::runtime;
    use beam_bifs::NullSink;
    use beam_engine::terms;
    use std::path::Path;
    /// The descriptor id the queue-only tests register their queue under. A read-shaped
    /// `prim_file:open/2` picks the id from the reference it mints, so the queue operations below
    /// are exercised on a hand-picked key with the same shape (nothing about the queue is keyed by
    /// anything but the id).
    const ID: u64 = 7;
    /// Runs `body` against a context whose read-ahead table holds one empty queue under [`ID`].
    fn with_queue<T>(body: impl FnOnce(&mut RuntimeContext<'_>) -> T) -> T {
        let mut rt = runtime();
        rt.buffers_mut()
            .insert(ID, crate::native::prim_buffer::PrimBuffer::new());
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut rt, &mut sink);
        body(&mut context)
    }
    /// The bytes of the `{ok, Binary}` answer [`RuntimeContext::line_answer`] produces.
    fn line_bytes(context: &mut RuntimeContext<'_>, line: &[u8]) -> Vec<u8> {
        let answer = context.line_answer(line.to_vec()).expect("a line answer");
        assert_eq!(terms::tuple_arity(answer).expect("a tuple"), 2);
        let tag = terms::tuple_element(answer, 0).expect("an element");
        assert_eq!(context.atom_name(tag).as_deref(), Some("ok"));
        let contents = terms::tuple_element(answer, 1).expect("an element");
        terms::binary_bytes(contents)
            .expect("a binary")
            .to_vec()
    }
    /// Opens `path` with the read shape (`[raw, binary, read]`) and returns its descriptor term.
    fn open_read(context: &mut RuntimeContext<'_>, path: &Path) -> Eterm {
        let name = context
            .charlist(&path.to_string_lossy())
            .expect("the path as a char list");
        let modes: Vec<Eterm> = ["raw", "binary", "read"]
            .iter()
            .map(|mode| context.atom(mode).expect("a mode atom"))
            .collect();
        let modes = terms::make_list(context, &modes).expect("a mode list");
        let answer = context
            .prim_open(&[name, modes])
            .expect("prim_file:open/2");
        assert_eq!(terms::tuple_arity(answer).expect("a tuple"), 2);
        let tag = terms::tuple_element(answer, 0).expect("an element");
        assert_eq!(
            context.atom_name(tag).as_deref(),
            Some("ok"),
            "open/2 answered {answer:?}"
        );
        terms::tuple_element(answer, 1).expect("the descriptor")
    }
    #[test]
    fn the_queue_coalesces_appends_and_answers_a_copying_read() {
        with_queue(|context| {
            assert!(!context.has_readahead(ID + 1), "no queue ⇒ no read-ahead");
            assert!(context.has_readahead(ID), "the write shape registers one");
            assert_eq!(context.readahead_size(ID), 0);
            context.append_readahead(ID, b"alpha");
            context.append_readahead(ID, b"beta\n");
            assert_eq!(context.readahead_size(ID), 10);
            assert_eq!(context.find_in_readahead(ID, b'\n'), Some(9));
            assert_eq!(
                context.find_in_readahead(ID, b'z'),
                None,
                "a byte the queue does not hold"
            );
            // `copying_read(3)` drains the front only; the tail stays queued.
            assert_eq!(context.take_readahead(ID, 3), b"alp".to_vec());
            assert_eq!(context.readahead_size(ID), 7);
            assert_eq!(context.find_in_readahead(ID, b'\n'), Some(6));
            // A count past the end is unreachable from every caller and degrades to an empty
            // payload rather than a fabricated one.
            assert!(context.take_readahead(ID, 100).is_empty());
        });
    }
    #[test]
    fn wipe_readahead_empties_the_queue_without_touching_a_sibling() {
        with_queue(|context| {
            context.append_readahead(ID, b"alpha\nbeta\n");
            context.wipe_readahead(ID);
            assert_eq!(context.readahead_size(ID), 0);
            assert_eq!(context.find_in_readahead(ID, b'\n'), None);
            // Wiping an id with no queue is a no-op, never a panic or a fabricated entry.
            context.wipe_readahead(ID + 1);
            assert!(!context.has_readahead(ID + 1));
        });
    }
    #[test]
    fn a_crlf_terminator_answers_a_bare_newline() {
        with_queue(|context| {
            assert_eq!(line_bytes(context, b"alpha\r\n"), b"alpha\n".to_vec());
            // A lone `\r` before the end is data, not a terminator.
            assert_eq!(line_bytes(context, b"alpha\r"), b"alpha\r".to_vec());
            assert_eq!(line_bytes(context, b"\r\n"), b"\n".to_vec());
            // The final line of a file with no terminator is answered verbatim.
            assert_eq!(line_bytes(context, b"gamma"), b"gamma".to_vec());
        });
    }
    #[test]
    fn a_line_longer_than_the_on_heap_binary_limit_is_still_a_binary() {
        with_queue(|context| {
            // 200 bytes: past `ERL_ONHEAP_BINARY_LIMIT` (64), so the answer must be an off-heap
            // refc binary — the `make_binary` route, not a heap-binary writer.
            let mut line = vec![b'x'; 199];
            line.push(b'\n');
            assert_eq!(line_bytes(context, &line), line);
        });
    }
    #[test]
    fn read_line_walks_a_three_line_file_and_an_unterminated_tail() {
        let dir = scratch_dir("prim_file_readahead");
        let path = dir.path().join("lines.txt");
        std::fs::write(&path, b"alpha\nbeta\n\ngamma").expect("a scratch file");
        let mut rt = runtime();
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut rt, &mut sink);
        let fd = open_read(&mut context, &path);
        let mut lines = Vec::new();
        loop {
            let answer = context.prim_read_line(&[fd]).expect("read_line/1");
            if context.atom_name(answer).as_deref() == Some("eof") {
                break;
            }
            let contents = terms::tuple_element(answer, 1).expect("an element");
            lines.push(terms::binary_bytes(contents).expect("a binary").to_vec());
            assert!(lines.len() <= 8, "read_line/1 never terminated");
        }
        assert_eq!(
            lines,
            vec![
                b"alpha\n".to_vec(),
                b"beta\n".to_vec(),
                b"\n".to_vec(),
                b"gamma".to_vec()
            ],
            "the four lines, terminators included, the unterminated tail last"
        );
        // A drained descriptor keeps answering `eof`, never a fabricated empty line.
        let answer = context.prim_read_line(&[fd]).expect("read_line/1");
        assert_eq!(context.atom_name(answer).as_deref(), Some("eof"));
        // `position/2` wipes the queue, so a seek back to `bof` re-reads from the first line.
        let bof = context.atom("bof").expect("bof");
        let position = context
            .prim_position(&[fd, bof])
            .expect("prim_file:position/2");
        assert_eq!(
            terms::tuple_arity(position).expect("a tuple"),
            2,
            "position/2 answered {position:?}"
        );
        let pos_tag = terms::tuple_element(position, 0).expect("an element");
        assert_eq!(context.atom_name(pos_tag).as_deref(), Some("ok"));
        let answer = context.prim_read_line(&[fd]).expect("read_line/1");
        let first = terms::tuple_element(answer, 1).expect("an element");
        assert_eq!(
            terms::binary_bytes(first).expect("a binary"),
            b"alpha\n",
            "the queue was wiped, so the line comes from the host file again"
        );
        // The chunk read to answer that line left the rest of the file queued, which is exactly
        // the byte count `position(Fd, {cur, 0})` would shift back by.
        let id = context.decode_descriptor(fd).expect("a descriptor").id;
        assert_eq!(
            context.readahead_size(id),
            b"beta\n\ngamma".len(),
            "one chunk read ahead, the answered line taken off the front"
        );
    }
}